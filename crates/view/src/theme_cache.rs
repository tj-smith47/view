//! Loads and stores the last-derived [`Theme`] to disk, keyed by the
//! resolved config path, so the first paint after startup can already be
//! themed correctly before nvim's `default_colors_set`/`hl_attr_define`/
//! `hl_group_set` events arrive over the wire. `view-core` stays free of
//! `serde`/`toml` (a hard dependency-direction rule), so the on-disk shape
//! and its (de)serialization live entirely in this bin-crate module;
//! [`CachedTheme`] is the only type that ever touches `serde`.
//!
//! Corrupt, missing, or schema-incompatible cache state is never fatal:
//! every failure path here logs to stderr and falls back to
//! `Theme::default()` (or, for `store`, simply gives up) rather than
//! propagating an error startup would have to handle specially.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use view_core::hl::{HlAttr, HlTable, ProbedDefaults};
use view_core::theme::{ChromeGroup, ResolvedStyle, Theme};

/// The on-disk cache format version this build writes and the newest
/// version it knows how to read. Bumped whenever [`CachedTheme`]'s field
/// set changes in a way `#[serde(default)]` cannot absorb (a rename, a
/// meaning change) -- a plain field addition does not need a bump, since
/// `#[serde(default)]` already lets an older file omit it. A cache file
/// carrying a *higher* version than this constant is from a newer build
/// this one predates and is never guessed at (see [`load_from_path`]).
///
/// v2 moved the chrome groups from one key per group to a single
/// `[chrome]` table keyed by nvim's own highlight-group name, so a group is
/// declared once in [`ChromeGroup`] instead of being mirrored here by hand.
/// v1's per-group keys are unknown fields under the current schema and
/// would otherwise be reported as corruption, so `load_from_path` tells an
/// older file apart from a broken one and says so.
const CACHE_SCHEMA_VERSION: u32 = 2;

/// The on-disk mirror of [`ResolvedStyle`]: identical fields, but
/// `Serialize`/`Deserialize` derive here instead of on `ResolvedStyle`
/// itself, since `view-core` must not depend on `serde`. Every field
/// defaults on a missing key (`#[serde(default)]` at the container level)
/// so a cache written before a given named group existed in the schema
/// still parses; `deny_unknown_fields` is deliberately left off this
/// nested type, since [`CachedTheme`]'s container-level `deny_unknown_fields`
/// already rejects a whole-file schema mismatch and the two would only
/// ever fire together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
struct CachedResolvedStyle {
    fg: Option<u32>,
    bg: Option<u32>,
    bold: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
}

impl From<ResolvedStyle> for CachedResolvedStyle {
    fn from(s: ResolvedStyle) -> Self {
        Self {
            fg: s.fg,
            bg: s.bg,
            bold: s.bold,
            italic: s.italic,
            underline: s.underline,
            reverse: s.reverse,
        }
    }
}

impl From<CachedResolvedStyle> for ResolvedStyle {
    fn from(c: CachedResolvedStyle) -> Self {
        Self {
            fg: c.fg,
            bg: c.bg,
            bold: c.bold,
            italic: c.italic,
            underline: c.underline,
            reverse: c.reverse,
        }
    }
}

/// The on-disk mirror of [`Theme`]. `schema_version` and every other field
/// default on a missing key (`#[serde(default)]`), so a cache written by
/// the pre-named-groups format (only `fg`/`bg`, no `schema_version` key at
/// all) still parses as schema v0 with an empty `chrome` table -- exactly
/// the "no named color was ever cached" truth for that file, not a guess.
/// `deny_unknown_fields` pairs with that default-on-missing behavior to
/// catch the opposite case: a field this build does not recognize (a future
/// rename or removal) fails to parse loudly instead of `toml` silently
/// dropping it and this build unknowingly loading a theme missing data it
/// thinks it has.
///
/// `chrome` is keyed by [`ChromeGroup::hl_name`] rather than carrying one
/// struct field per group: the field-per-group shape made every added group
/// a second edit here that nothing forced, and a missed one persisted
/// nothing while compiling clean. Keying by the group's own nvim name also
/// makes the file readable to anyone inspecting it, since the keys are the
/// highlight groups they would write in a colorscheme.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CachedTheme {
    schema_version: u32,
    fg: Option<u32>,
    bg: Option<u32>,
    chrome: BTreeMap<String, CachedResolvedStyle>,
}

/// Reads nothing but `schema_version`, from a file the current schema
/// refused. Deliberately without `deny_unknown_fields`, which is the whole
/// point: it answers "which build wrote this?" for a file whose remaining
/// keys this build cannot make sense of, so an upgrade reports an older
/// cache honestly instead of accusing it of corruption.
#[derive(Deserialize, Default)]
#[serde(default)]
struct SchemaProbe {
    /// Zero for a file that carries no version at all, which is a value
    /// [`CACHE_SCHEMA_VERSION`] never takes, so "unversioned" stays
    /// distinguishable from "written by schema 1".
    schema_version: u32,
}

impl From<Theme> for CachedTheme {
    fn from(theme: Theme) -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            fg: theme.fg,
            bg: theme.bg,
            chrome: ChromeGroup::ALL
                .into_iter()
                .map(|group| (group.hl_name().to_string(), theme.chrome(group).into()))
                .collect(),
        }
    }
}

impl From<CachedTheme> for Theme {
    fn from(cached: CachedTheme) -> Self {
        let mut theme = Theme::with_colors(cached.fg, cached.bg);
        for group in ChromeGroup::ALL {
            if let Some(style) = cached.chrome.get(group.hl_name()) {
                theme.set_chrome(group, (*style).into());
            }
        }
        theme
    }
}

/// FNV-1a's standard 64-bit offset basis, verbatim from the algorithm's
/// public-domain specification.
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a's standard 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a over `bytes`, implemented locally rather than via a hash crate or
/// `std::hash::DefaultHasher`: `DefaultHasher`'s algorithm and output are
/// unspecified and have changed across toolchain versions, which would
/// silently orphan every existing cache file on a compiler bump. FNV-1a is
/// a fixed, documented algorithm with no such risk, and a cache-filename
/// hash has no adversarial input to defend against, so its known
/// non-cryptographic weaknesses do not apply here.
#[must_use]
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The cache file path for `config_path` under `state_dir`: pure and
/// env-free so path construction is directly testable, separate from the
/// env-resolution [`view_native::paths::state_dir`] performs for the public
/// API.
///
/// The directory comes from [`view_native::paths::cache_dir`] rather than
/// being joined here, so every file view caches lands in one directory a
/// user can inspect or delete wholesale.
#[must_use]
fn cache_path(state_dir: &Path, config_path: &Path) -> PathBuf {
    let hash = fnv1a(config_path.as_os_str().as_encoded_bytes());
    view_native::paths::cache_dir(state_dir).join(format!("theme-{hash:016x}.toml"))
}

/// Loads the last-cached theme for `config_path`, or `None`, loudly logged
/// to stderr, when the state directory cannot be determined, the cache file
/// is missing or unreadable, its contents fail to parse, or its schema is
/// newer than this build understands.
///
/// `None` rather than `Theme::default()`: the caller only seeds the engine's
/// highlight table on `Some`, so a genuine cache miss leaves `hl.groups`
/// empty and [`Theme::from_hl`](view_core::theme::Theme::from_hl)'s own
/// per-group fallback (emphasis styling for `TabLineSel`/`PmenuSel`, so the
/// selected/current row stays visually distinct) applies for the frame
/// before nvim's own `hl_group_set` arrives. Returning a bare `Theme` here
/// would make a cache miss indistinguishable from a *cached* all-default
/// theme, and unconditionally seeding either one registers those two groups
/// with all-false attributes, permanently defeating that fallback.
#[must_use]
pub fn load(config_path: &Path) -> Option<Theme> {
    let Some(path) = cache_target(config_path) else {
        eprintln!(
            "view: no XDG_STATE_HOME, HOME, or LOCALAPPDATA set; theme cache unavailable, using built-in defaults"
        );
        return None;
    };
    load_from_path(&path)
}

/// The cache file `config_path`'s theme is read from and written to, or
/// `None` when no state directory can be resolved at all.
///
/// Public to this crate so a caller that writes the cache more than once per
/// session resolves the path a single time and never reads the environment
/// again from inside a loop pass.
#[must_use]
pub(crate) fn cache_target(config_path: &Path) -> Option<PathBuf> {
    let state_dir = view_native::paths::state_dir()?;
    Some(cache_path(&state_dir, config_path))
}

/// [`load`]'s implementation given an already-resolved cache file path, so
/// tests can exercise missing/corrupt/version-mismatched-file behavior
/// without mutating process environment. Public to this crate for the same
/// reason as [`store_to_path`]: it is the read side of a slot a caller
/// already holds the path to.
#[must_use]
pub(crate) fn load_from_path(path: &Path) -> Option<Theme> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "view: no theme cache at {} yet, using built-in defaults",
                path.display()
            );
            return None;
        }
        Err(e) => {
            eprintln!(
                "view: failed to read theme cache {}: {e}, using built-in defaults",
                path.display()
            );
            return None;
        }
    };
    match toml::from_str::<CachedTheme>(&contents) {
        Ok(cached) if cached.schema_version > CACHE_SCHEMA_VERSION => {
            eprintln!(
                "view: theme cache {} is schema v{}, newer than this build's v{CACHE_SCHEMA_VERSION}; using built-in defaults",
                path.display(),
                cached.schema_version
            );
            None
        }
        Ok(cached) => Some(cached.into()),
        Err(e) => {
            match toml::from_str::<SchemaProbe>(&contents) {
                // a version this build has since superseded: the file is
                // intact, its shape simply predates the current schema, and
                // saying "corrupt" about a perfectly good older cache would
                // send a user deleting state that was never broken. Version
                // 0 is excluded deliberately: the pre-versioning format
                // carries no groups at all and parses cleanly above, so a
                // file with no version that still failed is malformed, not
                // old.
                Ok(probe)
                    if probe.schema_version > 0 && probe.schema_version < CACHE_SCHEMA_VERSION =>
                {
                    eprintln!(
                        "view: theme cache {} is schema v{}, written before this build's v{CACHE_SCHEMA_VERSION}; using built-in defaults until this session stores its own",
                        path.display(),
                        probe.schema_version
                    );
                }
                _ => eprintln!(
                    "view: corrupt theme cache {}: {e}, using built-in defaults",
                    path.display()
                ),
            }
            None
        }
    }
}

/// Persists `theme` for `config_path`. Any failure (no state directory, a
/// directory-creation error, a write error) is logged to stderr and
/// otherwise ignored: a cache write is a best-effort convenience for the
/// *next* startup, never something the current session should fail over.
pub fn store(theme: Theme, config_path: &Path) {
    let Some(path) = cache_target(config_path) else {
        eprintln!(
            "view: no XDG_STATE_HOME, HOME, or LOCALAPPDATA set; cannot cache theme for next startup"
        );
        return;
    };
    store_to_path(theme, &path);
}

/// [`store`]'s implementation given an already-resolved cache file path;
/// see [`load_from_path`] for why the path-taking split exists. Public to
/// this crate for the same reason as [`cache_target`]: a mid-session write
/// already holds its resolved path and must not re-read the environment.
pub(crate) fn store_to_path(theme: Theme, path: &Path) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "view: failed to create theme cache directory {}: {e}",
                parent.display()
            );
            return;
        }
    }
    let cached: CachedTheme = theme.into();
    let rendered = match toml::to_string_pretty(&cached) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("view: failed to serialize theme cache: {e}");
            return;
        }
    };
    if let Err(e) = std::fs::write(path, rendered) {
        eprintln!("view: failed to write theme cache {}: {e}", path.display());
    }
}

/// Highlight ids reserved for cache-seeded synthetic entries, chosen from
/// the very top of the `u64` range so they can never collide with a real
/// `hl_id` nvim assigns (nvim's own ids start at 0 and grow by small
/// increments per highlight group defined over one session, nowhere near
/// `u64::MAX`).
const SEED_HL_ID_BASE: u64 = u64::MAX;

/// Builds a highlight table from a previously cached `theme` so the very
/// first paint -- before nvim has sent a single `default_colors_set`/
/// `hl_attr_define`/
/// `hl_group_set` event -- already reflects last session's colors instead
/// of `Theme::default()`'s all-unset fallback. Reuses the exact live
/// derivation path ([`Theme::from_hl`]) rather than special-casing a
/// "startup theme" value: each named group gets a synthetic `hl_id`
/// inserted into `attrs` and mapped by name in `groups`, the same two
/// channels a real `hl_attr_define`/`hl_group_set` pair would populate, so
/// the first real batch of those events overwrites this seed with zero
/// extra branching once it arrives.
///
/// Also seeds `hl.confirmed` at the table's current `probe_generation`, not
/// just the raw `default_fg`/`default_bg` pair: `Theme::from_hl` only trusts
/// a cached bg over a fresh, still-ambiguous wire zero when a confirmed
/// value already exists, so a cache hit that stopped at the raw pair would
/// still let attach's first `default_colors_set` flash black on a
/// transparent config every single startup -- the exact symptom this
/// seeding closes. The cache only ever stores an already-honest `Theme`
/// (never a wire-ambiguous raw value -- see `store`'s caller), so re-marking
/// it "confirmed" here is not fabricating certainty that was never earned.
///
/// Returns a fresh table rather than seeding one in place: the model's own
/// table is reachable only through `EngineModel`, whose accessors exist so
/// that installing a replacement records the damage the swap causes.
#[must_use]
pub fn seeded_hl_table(theme: &Theme) -> HlTable {
    let mut hl = HlTable::new();
    // the generation the write itself opened, so the seeded confirmation
    // answers these defaults rather than whatever generation preceded them
    let generation = hl.set_default_colors(theme.fg, theme.bg);
    hl.confirm_defaults(ProbedDefaults {
        generation,
        fg: theme.fg,
        bg: theme.bg,
    });
    // offsets come from enumeration, not hand-written literals, so two
    // groups can never collide on a synthetic hl_id
    for (offset, group) in ChromeGroup::ALL.into_iter().enumerate() {
        seed_named(&mut hl, group.hl_name(), theme.chrome(group), offset as u64);
    }
    hl
}

/// One [`seeded_hl_table`] entry: reserves `SEED_HL_ID_BASE - offset` as
/// `name`'s synthetic `hl_id`, distinct per named group so seeding one
/// group's colors can never bleed into another's.
fn seed_named(hl: &mut HlTable, name: &str, style: ResolvedStyle, offset: u64) {
    let hl_id = SEED_HL_ID_BASE - offset;
    hl.define_attr(
        hl_id,
        HlAttr {
            fg: style.fg,
            bg: style.bg,
            bold: style.bold,
            italic: style.italic,
            underline: style.underline,
            reverse: style.reverse,
        },
    );
    hl.set_group(name.to_string(), hl_id);
}

#[cfg(test)]
mod tests {
    // the env-mutation sites below are the ones ENV_MUTATION_LOCK exists to
    // bound; each holds the guard across its own restore
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "view-theme-cache-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Serializes every test in this module that calls `std::env::set_var`/
    /// `remove_var`. `cargo test` runs a module's tests on multiple threads
    /// by default, and these tests set and then restore the *same* names, so
    /// two of them overlapping would interleave one's restore with another's
    /// plant and leave the loser reading a value it never set. The lock is
    /// held for the whole body, restore included, because releasing it
    /// between the mutation and the restore is what opens that window.
    ///
    /// The hazard is that overlap, not a torn read of libc's `environ`:
    /// std's own `ENV_LOCK` already excludes `set_var` against `vars_os` and
    /// against the environment copy inside `Command::spawn`, so a program
    /// mutating the environment only through `std` cannot observe a table
    /// mid-reallocation.
    static ENV_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// [`ENV_MUTATION_LOCK`], with poisoning ignored: it orders two
    /// operations and guards no data, so a test that panicked while holding
    /// it left nothing behind for the next one to find broken. Propagating
    /// the poison would turn one real failure into a screenful of unrelated
    /// ones, none of which names what actually broke.
    fn env_mutation_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTATION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn fnv1a_is_deterministic_and_differs_by_input() {
        let a = fnv1a(b"/home/x/.config/view/view.toml");
        let b = fnv1a(b"/home/x/.config/view/view.toml");
        let c = fnv1a(b"/home/y/.config/view/view.toml");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn cache_path_is_deterministic_and_namespaced_under_view() {
        let state_dir = Path::new("/state");
        let config_path = Path::new("/home/x/.config/view/view.toml");
        let p1 = cache_path(state_dir, config_path);
        let p2 = cache_path(state_dir, config_path);
        assert_eq!(p1, p2);
        assert_eq!(p1.parent(), Some(Path::new("/state/view")));
        assert!(p1
            .file_name()
            .expect("cache path must have a file name")
            .to_string_lossy()
            .starts_with("theme-"));
        assert!(p1.extension().is_some_and(|ext| ext == "toml"));
    }

    #[test]
    fn cache_path_differs_for_different_config_paths() {
        let state_dir = Path::new("/state");
        let p1 = cache_path(state_dir, Path::new("/a/view.toml"));
        let p2 = cache_path(state_dir, Path::new("/b/view.toml"));
        assert_ne!(p1, p2);
    }

    #[test]
    fn round_trip_preserves_theme_colors() {
        let dir = tmp_dir("roundtrip");
        let path = dir.join("theme.toml");
        let theme = Theme::with_colors(Some(0xABCDEF), Some(0x010203));
        store_to_path(theme, &path);
        let loaded = load_from_path(&path);
        assert_eq!(loaded, Some(theme));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_preserves_unset_colors() {
        let dir = tmp_dir("roundtrip-unset");
        let path = dir.join("theme.toml");
        let theme = Theme::default();
        store_to_path(theme, &path);
        let loaded = load_from_path(&path);
        assert_eq!(loaded, Some(theme));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every named group's full `ResolvedStyle` (colors and attributes
    /// alike) survives a store/load round trip, not just the base
    /// `fg`/`bg` pair the pre-amendment format carried.
    #[test]
    fn round_trip_preserves_named_group_colors() {
        let dir = tmp_dir("roundtrip-named");
        let path = dir.join("theme.toml");
        let mut theme = Theme::with_colors(Some(0x111111), Some(0x222222));
        theme.set_chrome(
            ChromeGroup::StatusLine,
            ResolvedStyle {
                fg: Some(0x333333),
                bg: Some(0x444444),
                bold: true,
                italic: false,
                underline: true,
                reverse: false,
            },
        );
        theme.set_chrome(
            ChromeGroup::TabLineSel,
            ResolvedStyle {
                reverse: true,
                ..ResolvedStyle::default()
            },
        );
        theme.set_chrome(
            ChromeGroup::PmenuSel,
            ResolvedStyle {
                reverse: true,
                ..ResolvedStyle::default()
            },
        );
        theme.set_chrome(
            ChromeGroup::MsgArea,
            ResolvedStyle {
                fg: Some(0x555555),
                ..ResolvedStyle::default()
            },
        );
        store_to_path(theme, &path);
        let loaded = load_from_path(&path);
        assert_eq!(loaded, Some(theme));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_cache_file_yields_none_without_panicking() {
        let dir = tmp_dir("missing");
        let path = dir.join("does-not-exist.toml");
        let loaded = load_from_path(&path);
        assert_eq!(loaded, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_cache_file_yields_none_without_panicking() {
        let dir = tmp_dir("corrupt");
        let path = dir.join("theme.toml");
        std::fs::write(&path, "this is not valid { toml at all ]]]").unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cache written under an earlier schema version (only `fg`/`bg`
    /// keys, no `schema_version`, no named groups at all) must still parse
    /// successfully under the current schema, with every named group
    /// correctly defaulting to `ResolvedStyle::default()` -- that default is
    /// not a guess, it is the literal truth for a file that never had
    /// named-group data to lose.
    #[test]
    fn pre_amendment_cache_without_named_groups_loads_cleanly_with_defaults() {
        let dir = tmp_dir("legacy");
        let path = dir.join("theme.toml");
        std::fs::write(&path, "fg = 16777215\nbg = 0\n").unwrap();
        let loaded = load_from_path(&path).expect("a successfully parsed legacy file is a hit");
        assert_eq!(loaded.fg, Some(16_777_215));
        assert_eq!(loaded.bg, Some(0));
        assert_eq!(
            loaded.chrome(ChromeGroup::StatusLine),
            ResolvedStyle::default()
        );
        assert_eq!(
            loaded.chrome(ChromeGroup::PmenuSel),
            ResolvedStyle::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The "falls back loudly" branch: a cache schema newer than this build
    /// understands is never guessed at by partially trusting whatever
    /// fields happen to still parse -- it is rejected wholesale, yielding
    /// `None` rather than a guessed `Theme`.
    #[test]
    fn newer_schema_version_yields_none_instead_of_guessing() {
        let dir = tmp_dir("future-schema");
        let path = dir.join("theme.toml");
        std::fs::write(&path, "schema_version = 999\nfg = 1\nbg = 2\n").unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every arm of [`ChromeGroup::ALL`] survives a store/load round trip
    /// carrying its own distinct style, so a group added to the enum but
    /// missing from the cache's map is a red test rather than a silently
    /// stale default painted at the next cold start. Distinct values per
    /// group, not one shared style: a map that wrote every group under the
    /// same key would round-trip a single shared value and pass a
    /// same-value check.
    #[test]
    fn every_chrome_group_round_trips_through_the_cache() {
        let dir = tmp_dir("roundtrip-every-group");
        let path = dir.join("theme.toml");
        let mut theme = Theme::with_colors(Some(0x111111), Some(0x222222));
        for (offset, group) in ChromeGroup::ALL.into_iter().enumerate() {
            let offset = offset as u32;
            theme.set_chrome(
                group,
                ResolvedStyle {
                    fg: Some(0x40_0000 + offset),
                    bg: Some(0x50_0000 + offset),
                    bold: offset.is_multiple_of(2),
                    italic: offset.is_multiple_of(3),
                    underline: offset.is_multiple_of(4),
                    reverse: offset.is_multiple_of(5),
                },
            );
        }
        store_to_path(theme, &path);
        let loaded = load_from_path(&path).expect("a freshly stored cache must load");
        for group in ChromeGroup::ALL {
            assert_eq!(
                loaded.chrome(group),
                theme.chrome(group),
                "{} lost its cached style across the round trip",
                group.hl_name()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cache written by an earlier schema (per-group keys, before the
    /// name-keyed `[chrome]` table) is rejected -- its keys are unknown to
    /// this build, and guessing at a partial parse is what
    /// `deny_unknown_fields` exists to prevent -- but it is rejected as an
    /// older file rather than as corruption, because it is intact and the
    /// distinction is the difference between a user waiting one startup for
    /// a fresh cache and a user deleting state that was never broken.
    #[test]
    fn a_cache_from_an_earlier_schema_is_rejected_as_older_not_as_corrupt() {
        let dir = tmp_dir("older-schema");
        let path = dir.join("theme.toml");
        std::fs::write(
            &path,
            "schema_version = 1\nfg = 1\nbg = 2\n\n[status_line]\nfg = 3\n",
        )
        .unwrap();
        assert_eq!(load_from_path(&path), None);
        let probe: SchemaProbe = toml::from_str(
            &std::fs::read_to_string(&path).expect("the planted file must be readable"),
        )
        .expect("a version probe must succeed on a file the strict schema refused");
        assert!(
            probe.schema_version > 0 && probe.schema_version < CACHE_SCHEMA_VERSION,
            "the planted file must land in the older-schema branch, got v{}",
            probe.schema_version
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The spec's "keyed by config path" requirement, asserted where a user
    /// would see it: two sessions started against two different config
    /// files leave two files in the cache directory, each holding its own
    /// session's colors. Listing the directory rather than only comparing
    /// two derived paths, so a keying that collapsed both configs onto one
    /// file (or wrote outside the cache directory entirely) fails here.
    #[test]
    fn two_config_paths_leave_two_entries_in_the_cache_directory() {
        let _guard = env_mutation_guard();
        let dir = tmp_dir("two-configs");
        let prev = std::env::var("XDG_STATE_HOME").ok();
        std::env::set_var("XDG_STATE_HOME", &dir);

        let first_config = Path::new("/home/first/.config/view/view.toml");
        let second_config = Path::new("/home/second/.config/view/view.toml");
        let first_theme = Theme::with_colors(Some(0x0A0A0A), Some(0x0B0B0B));
        let second_theme = Theme::with_colors(Some(0x0C0C0C), Some(0x0D0D0D));
        store(first_theme, first_config);
        store(second_theme, second_config);
        let first_loaded = load(first_config);
        let second_loaded = load(second_config);
        let cache_dir = view_native::paths::cache_dir(&dir);
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&cache_dir)
            .expect("the cache directory must exist after two stores")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("theme-"))
            })
            .collect();
        entries.sort();

        match prev {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            entries.len(),
            2,
            "two config paths must key two cache entries, found {entries:?}"
        );
        assert_eq!(first_loaded, Some(first_theme));
        assert_eq!(second_loaded, Some(second_theme));
    }

    /// The other silent-misparse guard `deny_unknown_fields` provides: a
    /// field this build does not recognize at all (renamed or removed in a
    /// hypothetical future schema) must fail to parse loudly rather than
    /// being dropped in silence while the recognized fields load as if
    /// nothing were missing.
    #[test]
    fn unrecognized_field_yields_none_instead_of_being_silently_dropped() {
        let dir = tmp_dir("unknown-field");
        let path = dir.join("theme.toml");
        std::fs::write(&path, "fg = 1\nbg = 2\nsome_future_field = true\n").unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_creates_missing_parent_directories() {
        let dir = tmp_dir("nested");
        let path = dir.join("nested").join("dirs").join("theme.toml");
        let theme = Theme::with_colors(Some(1), Some(2));
        store_to_path(theme, &path);
        assert!(path.exists());
        let loaded = load_from_path(&path);
        assert_eq!(loaded, Some(theme));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The load-bearing property this whole seeding mechanism exists to
    /// prove: seeding a fresh `HlTable` from a cached `Theme` and then
    /// re-deriving through the exact live path (`Theme::from_hl`) recovers
    /// the cached values, with zero special-casing in the derivation
    /// itself.
    #[test]
    fn seeded_hl_table_makes_named_groups_resolve_through_the_live_derivation_path() {
        let mut cached_theme = Theme::with_colors(Some(0x111111), Some(0x222222));
        cached_theme.set_chrome(
            ChromeGroup::StatusLine,
            ResolvedStyle {
                fg: Some(0xAAAAAA),
                bg: Some(0xBBBBBB),
                bold: true,
                italic: false,
                underline: false,
                reverse: false,
            },
        );
        cached_theme.set_chrome(
            ChromeGroup::PmenuSel,
            ResolvedStyle {
                fg: Some(0xCCCCCC),
                bg: Some(0xDDDDDD),
                bold: false,
                italic: false,
                underline: false,
                reverse: true,
            },
        );
        let hl = seeded_hl_table(&cached_theme);
        let derived = Theme::from_hl(&hl);
        assert_eq!(derived.fg, Some(0x111111));
        assert_eq!(derived.bg, Some(0x222222));
        assert_eq!(
            derived.chrome(ChromeGroup::StatusLine),
            cached_theme.chrome(ChromeGroup::StatusLine)
        );
        assert_eq!(
            derived.chrome(ChromeGroup::PmenuSel),
            cached_theme.chrome(ChromeGroup::PmenuSel)
        );
    }

    /// Seeding does not stop at the raw `default_fg`/`default_bg` pair, it
    /// also marks the cached theme `confirmed` at the table's current
    /// generation -- otherwise a cached, already-disambiguated bg would
    /// still lose to a fresh, wire-ambiguous `default_colors_set` on the
    /// very next attach, reintroducing the black flash this seeding exists
    /// to prevent.
    #[test]
    fn seeded_hl_table_marks_the_cached_theme_confirmed_at_the_current_generation() {
        let cached_theme = Theme::with_colors(Some(0xF8F8F2), None);
        let hl = seeded_hl_table(&cached_theme);
        let confirmed = hl
            .confirmed()
            .expect("seeding must record a confirmed value");
        assert_eq!(confirmed.generation, hl.probe_generation());
        assert_eq!(confirmed.fg, Some(0xF8F8F2));
        assert_eq!(confirmed.bg, None);
    }

    /// End-to-end warm-start property spanning cache seeding and live
    /// derivation together: a cached transparent theme, seeded pre-attach,
    /// must survive attach's own `default_colors_set` -- which resends the
    /// same wire-ambiguous zero every single startup -- without flashing
    /// black while that event's own probe reply is still in flight. Without
    /// the confirmed seed (reverting to a raw-pair-only seed), `Theme::from_hl`
    /// would read the fresh ambiguous zero directly and this would assert
    /// `Some(0)` instead of `None`.
    #[test]
    fn a_seeded_transparent_theme_survives_attachs_own_ambiguous_default_colors_set() {
        let cached_theme = Theme::with_colors(Some(0xF8F8F2), None);
        let hl = seeded_hl_table(&cached_theme);
        assert_eq!(
            Theme::from_hl(&hl).bg,
            None,
            "seeded state must paint pre-attach"
        );

        // attach's own default_colors_set resends the same wire-ambiguous
        // zero the real bug report always reproduces with
        let mut model = view_core::model::Model::new();
        model.engine.replace_hl(hl);
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::Redraw(vec![view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0xF8F8F2),
                bg: Some(0),
                sp: None,
            }]),
        );
        assert_eq!(
            Theme::from_hl(model.engine.hl()).bg,
            None,
            "the seeded confirmed value must hold while attach's probe is in flight"
        );
    }

    /// Black twin of the transparent case above: a cached theme that is
    /// genuinely black (confirmed bg `Some(0)`, not absent) must also
    /// survive attach's own ambiguous `default_colors_set` and keep
    /// deriving black -- pinning that seeding does not special-case a zero
    /// bg differently from any other confirmed value. Without the confirmed
    /// seed (reverting to a raw-pair-only seed), the very first assertion
    /// below would already read `None` instead of `Some(0)`, before attach
    /// ever runs.
    #[test]
    fn a_seeded_black_theme_survives_attachs_own_ambiguous_default_colors_set() {
        let cached_theme = Theme::with_colors(Some(0xF8F8F2), Some(0));
        let hl = seeded_hl_table(&cached_theme);
        assert_eq!(
            Theme::from_hl(&hl).bg,
            Some(0),
            "seeded black state must paint pre-attach"
        );

        // attach's own default_colors_set resends the same wire-ambiguous
        // zero every single startup, black theme or not
        let mut model = view_core::model::Model::new();
        model.engine.replace_hl(hl);
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::Redraw(vec![view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0xF8F8F2),
                bg: Some(0),
                sp: None,
            }]),
        );
        assert_eq!(
            Theme::from_hl(model.engine.hl()).bg,
            Some(0),
            "the seeded confirmed black value must hold while attach's probe is in flight"
        );
    }

    #[test]
    fn seeded_hl_table_gives_each_named_group_a_distinct_synthetic_hl_id() {
        let hl = seeded_hl_table(&Theme::default());
        let ids: std::collections::HashSet<u64> = ChromeGroup::ALL
            .into_iter()
            .map(|group| {
                hl.group(group.hl_name())
                    .expect("seeding must map every named group")
            })
            .collect();
        assert_eq!(
            ids.len(),
            ChromeGroup::COUNT,
            "each named group must get its own synthetic hl_id, not a shared one"
        );
    }

    /// The state-dir env precedence and the public `load`/`store` wiring
    /// both need at least one end-to-end check that does not bypass
    /// [`view_native::paths::state_dir`]; see `ENV_MUTATION_LOCK`'s doc
    /// comment for why every env-planting test here serializes against the
    /// shared lock rather than relying on owning disjoint var names.
    #[test]
    fn load_and_store_round_trip_through_xdg_state_home() {
        let _guard = env_mutation_guard();
        let dir = tmp_dir("xdg-e2e");
        let prev = std::env::var("XDG_STATE_HOME").ok();
        std::env::set_var("XDG_STATE_HOME", &dir);

        let config_path = Path::new("/home/x/.config/view/view.toml");
        let theme = Theme::with_colors(Some(0x123456), Some(0x654321));
        store(theme, config_path);
        let loaded = load(config_path);

        match prev {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(loaded, Some(theme));
    }
}
