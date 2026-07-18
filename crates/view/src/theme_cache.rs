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

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use view_core::hl::{HlAttr, HlTable};
use view_core::theme::{ResolvedStyle, Theme};

/// The on-disk cache format version this build writes and the newest
/// version it knows how to read. Bumped whenever [`CachedTheme`]'s field
/// set changes in a way `#[serde(default)]` cannot absorb (a rename, a
/// meaning change) -- a plain field addition does not need a bump, since
/// `#[serde(default)]` already lets an older file omit it. A cache file
/// carrying a *higher* version than this constant is from a newer build
/// this one predates and is never guessed at (see [`load_from_path`]).
const CACHE_SCHEMA_VERSION: u32 = 1;

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
/// all) still parses as schema v0 with every named group defaulting to
/// [`CachedResolvedStyle::default`] -- exactly the "no named color was
/// ever cached" truth for that file, not a guess. `deny_unknown_fields`
/// pairs with that default-on-missing behavior to catch the opposite case:
/// a field this build does not recognize (a future rename or removal)
/// fails to parse loudly instead of `toml` silently dropping it and this
/// build unknowingly loading a theme missing data it thinks it has.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CachedTheme {
    schema_version: u32,
    fg: Option<u32>,
    bg: Option<u32>,
    status_line: CachedResolvedStyle,
    tab_line: CachedResolvedStyle,
    tab_line_sel: CachedResolvedStyle,
    tab_line_fill: CachedResolvedStyle,
    pmenu: CachedResolvedStyle,
    pmenu_sel: CachedResolvedStyle,
    msg_area: CachedResolvedStyle,
}

impl From<Theme> for CachedTheme {
    fn from(theme: Theme) -> Self {
        Self {
            schema_version: CACHE_SCHEMA_VERSION,
            fg: theme.fg,
            bg: theme.bg,
            status_line: theme.status_line.into(),
            tab_line: theme.tab_line.into(),
            tab_line_sel: theme.tab_line_sel.into(),
            tab_line_fill: theme.tab_line_fill.into(),
            pmenu: theme.pmenu.into(),
            pmenu_sel: theme.pmenu_sel.into(),
            msg_area: theme.msg_area.into(),
        }
    }
}

impl From<CachedTheme> for Theme {
    fn from(cached: CachedTheme) -> Self {
        Self {
            fg: cached.fg,
            bg: cached.bg,
            status_line: cached.status_line.into(),
            tab_line: cached.tab_line.into(),
            tab_line_sel: cached.tab_line_sel.into(),
            tab_line_fill: cached.tab_line_fill.into(),
            pmenu: cached.pmenu.into(),
            pmenu_sel: cached.pmenu_sel.into(),
            msg_area: cached.msg_area.into(),
        }
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

/// Resolves `$XDG_STATE_HOME`, falling back to the unix convention
/// (`~/.local/state`) or the Windows convention (`%LOCALAPPDATA%`, itself
/// already a state-like per-user directory) when unset. `None` when no
/// usable base directory can be determined at all (e.g. `HOME` also
/// unset), which callers treat as "caching is unavailable this run" rather
/// than an error.
#[must_use]
fn state_dir_from_env() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("XDG_STATE_HOME") {
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Some(PathBuf::from(home).join(".local").join("state"));
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        if !local.is_empty() {
            return Some(PathBuf::from(local));
        }
    }
    None
}

/// Resolves the config file path the cache is keyed on:
/// `$XDG_CONFIG_HOME/view/view.toml`, falling back to `~/.config/view/view.toml`
/// (unix) or `%APPDATA%/view/view.toml` (Windows), matching this project's
/// documented config location. This computes only the path *identity* the
/// cache key hashes over -- no config file is read or parsed here, so it
/// carries no dependency on whatever loads `view.toml`'s contents. `None`
/// when no usable base directory can be determined at all, the same
/// "caching unavailable this run" condition [`state_dir_from_env`] signals.
#[must_use]
pub fn resolved_config_path() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("XDG_CONFIG_HOME") {
        if !v.is_empty() {
            return Some(PathBuf::from(v).join("view").join("view.toml"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("view")
                    .join("view.toml"),
            );
        }
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.is_empty() {
            return Some(PathBuf::from(appdata).join("view").join("view.toml"));
        }
    }
    None
}

/// The cache file path for `config_path` under `state_dir`: pure and
/// env-free so path construction is directly testable, separate from the
/// env-resolution `state_dir_from_env` performs for the public API.
#[must_use]
fn cache_path(state_dir: &Path, config_path: &Path) -> PathBuf {
    let hash = fnv1a(config_path.as_os_str().as_encoded_bytes());
    state_dir
        .join("view")
        .join(format!("theme-{hash:016x}.toml"))
}

/// Loads the last-cached theme for `config_path`. Falls back to
/// `Theme::default()`, loudly logged to stderr, when the state directory
/// cannot be determined, the cache file is missing or unreadable, its
/// contents fail to parse, or its schema is newer than this build
/// understands -- every failure degrades instead of blocking startup.
#[must_use]
pub fn load(config_path: &Path) -> Theme {
    let Some(state_dir) = state_dir_from_env() else {
        eprintln!(
            "view: no XDG_STATE_HOME, HOME, or LOCALAPPDATA set; theme cache unavailable, using built-in defaults"
        );
        return Theme::default();
    };
    load_from_path(&cache_path(&state_dir, config_path))
}

/// [`load`]'s implementation given an already-resolved cache file path, so
/// tests can exercise missing/corrupt/version-mismatched-file behavior
/// without mutating process environment.
#[must_use]
fn load_from_path(path: &Path) -> Theme {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!(
                "view: no theme cache at {} yet, using built-in defaults",
                path.display()
            );
            return Theme::default();
        }
        Err(e) => {
            eprintln!(
                "view: failed to read theme cache {}: {e}, using built-in defaults",
                path.display()
            );
            return Theme::default();
        }
    };
    match toml::from_str::<CachedTheme>(&contents) {
        Ok(cached) if cached.schema_version > CACHE_SCHEMA_VERSION => {
            eprintln!(
                "view: theme cache {} is schema v{}, newer than this build's v{CACHE_SCHEMA_VERSION}; using built-in defaults",
                path.display(),
                cached.schema_version
            );
            Theme::default()
        }
        Ok(cached) => cached.into(),
        Err(e) => {
            eprintln!(
                "view: corrupt theme cache {}: {e}, using built-in defaults",
                path.display()
            );
            Theme::default()
        }
    }
}

/// Persists `theme` for `config_path`. Any failure (no state directory, a
/// directory-creation error, a write error) is logged to stderr and
/// otherwise ignored: a cache write is a best-effort convenience for the
/// *next* startup, never something the current session should fail over.
pub fn store(theme: Theme, config_path: &Path) {
    let Some(state_dir) = state_dir_from_env() else {
        eprintln!(
            "view: no XDG_STATE_HOME, HOME, or LOCALAPPDATA set; cannot cache theme for next startup"
        );
        return;
    };
    store_to_path(theme, &cache_path(&state_dir, config_path));
}

/// [`store`]'s implementation given an already-resolved cache file path;
/// see [`load_from_path`] for why the path-taking split exists.
fn store_to_path(theme: Theme, path: &Path) {
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

/// Seeds `hl` from a previously cached `theme` so the very first paint --
/// before nvim has sent a single `default_colors_set`/`hl_attr_define`/
/// `hl_group_set` event -- already reflects last session's colors instead
/// of `Theme::default()`'s all-unset fallback. Reuses the exact live
/// derivation path ([`Theme::from_hl`]) rather than special-casing a
/// "startup theme" value: each named group gets a synthetic `hl_id`
/// inserted into `attrs` and mapped by name in `groups`, the same two
/// channels a real `hl_attr_define`/`hl_group_set` pair would populate, so
/// the first real batch of those events overwrites this seed with zero
/// extra branching once it arrives.
pub fn seed_hl_table(hl: &mut HlTable, theme: &Theme) {
    hl.default_fg = theme.fg;
    hl.default_bg = theme.bg;
    seed_named(hl, "StatusLine", theme.status_line, 0);
    seed_named(hl, "TabLine", theme.tab_line, 1);
    seed_named(hl, "TabLineSel", theme.tab_line_sel, 2);
    seed_named(hl, "TabLineFill", theme.tab_line_fill, 3);
    seed_named(hl, "Pmenu", theme.pmenu, 4);
    seed_named(hl, "PmenuSel", theme.pmenu_sel, 5);
    seed_named(hl, "MsgArea", theme.msg_area, 6);
}

/// One [`seed_hl_table`] entry: reserves `SEED_HL_ID_BASE - offset` as
/// `name`'s synthetic `hl_id`, distinct per named group so seeding one
/// group's colors can never bleed into another's.
fn seed_named(hl: &mut HlTable, name: &str, style: ResolvedStyle, offset: u64) {
    let hl_id = SEED_HL_ID_BASE - offset;
    hl.attrs.insert(
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
    hl.groups.insert(name.to_string(), hl_id);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
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

    fn empty_hl_table() -> HlTable {
        HlTable {
            default_fg: None,
            default_bg: None,
            attrs: std::collections::HashMap::new(),
            groups: std::collections::HashMap::new(),
        }
    }

    /// The only test touching `XDG_CONFIG_HOME`/`HOME`/`APPDATA`; see
    /// `load_and_store_round_trip_through_xdg_state_home`'s doc comment for
    /// why exactly one test owns each env var this module reads.
    #[test]
    fn resolved_config_path_prefers_xdg_config_home_then_home_then_appdata() {
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        let prev_appdata = std::env::var("APPDATA").ok();

        std::env::set_var("XDG_CONFIG_HOME", "/xdg-cfg");
        assert_eq!(
            resolved_config_path(),
            Some(PathBuf::from("/xdg-cfg/view/view.toml"))
        );

        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("HOME", "/home/x");
        assert_eq!(
            resolved_config_path(),
            Some(PathBuf::from("/home/x/.config/view/view.toml"))
        );

        std::env::remove_var("HOME");
        std::env::set_var("APPDATA", "/appdata");
        assert_eq!(
            resolved_config_path(),
            Some(PathBuf::from("/appdata/view/view.toml"))
        );

        std::env::remove_var("APPDATA");
        assert_eq!(resolved_config_path(), None);

        match prev_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_appdata {
            Some(v) => std::env::set_var("APPDATA", v),
            None => std::env::remove_var("APPDATA"),
        }
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
        let theme = Theme {
            fg: Some(0xABCDEF),
            bg: Some(0x010203),
            ..Theme::default()
        };
        store_to_path(theme, &path);
        let loaded = load_from_path(&path);
        assert_eq!(loaded, theme);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_preserves_unset_colors() {
        let dir = tmp_dir("roundtrip-unset");
        let path = dir.join("theme.toml");
        let theme = Theme::default();
        store_to_path(theme, &path);
        let loaded = load_from_path(&path);
        assert_eq!(loaded, theme);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The load-bearing property coordinator requirement 5 exists to prove:
    /// every named group's full `ResolvedStyle` (colors and attributes
    /// alike) survives a store/load round trip, not just the base
    /// `fg`/`bg` pair the pre-amendment format carried.
    #[test]
    fn round_trip_preserves_named_group_colors() {
        let dir = tmp_dir("roundtrip-named");
        let path = dir.join("theme.toml");
        let theme = Theme {
            fg: Some(0x111111),
            bg: Some(0x222222),
            status_line: ResolvedStyle {
                fg: Some(0x333333),
                bg: Some(0x444444),
                bold: true,
                italic: false,
                underline: true,
                reverse: false,
            },
            tab_line: ResolvedStyle::default(),
            tab_line_sel: ResolvedStyle {
                reverse: true,
                ..ResolvedStyle::default()
            },
            tab_line_fill: ResolvedStyle::default(),
            pmenu: ResolvedStyle::default(),
            pmenu_sel: ResolvedStyle {
                reverse: true,
                ..ResolvedStyle::default()
            },
            msg_area: ResolvedStyle {
                fg: Some(0x555555),
                ..ResolvedStyle::default()
            },
        };
        store_to_path(theme, &path);
        let loaded = load_from_path(&path);
        assert_eq!(loaded, theme);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_cache_file_falls_back_to_default_without_panicking() {
        let dir = tmp_dir("missing");
        let path = dir.join("does-not-exist.toml");
        let loaded = load_from_path(&path);
        assert_eq!(loaded, Theme::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_cache_file_falls_back_to_default_without_panicking() {
        let dir = tmp_dir("corrupt");
        let path = dir.join("theme.toml");
        std::fs::write(&path, "this is not valid { toml at all ]]]").unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded, Theme::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Coordinator requirement 5's "loads cleanly" branch: a cache written
    /// by eae8542 (only `fg`/`bg` keys, no `schema_version`, no named
    /// groups at all) must still parse successfully under the new schema,
    /// with every named group correctly defaulting to
    /// `ResolvedStyle::default()` -- that default is not a guess, it is the
    /// literal truth for a file that never had named-group data to lose.
    #[test]
    fn pre_amendment_cache_without_named_groups_loads_cleanly_with_defaults() {
        let dir = tmp_dir("legacy");
        let path = dir.join("theme.toml");
        std::fs::write(&path, "fg = 16777215\nbg = 0\n").unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.fg, Some(16_777_215));
        assert_eq!(loaded.bg, Some(0));
        assert_eq!(loaded.status_line, ResolvedStyle::default());
        assert_eq!(loaded.pmenu_sel, ResolvedStyle::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Coordinator requirement 5's "falls back loudly" branch: a cache
    /// schema newer than this build understands is never guessed at by
    /// partially trusting whatever fields happen to still parse -- it is
    /// rejected wholesale in favor of `Theme::default()`.
    #[test]
    fn newer_schema_version_falls_back_to_default_instead_of_guessing() {
        let dir = tmp_dir("future-schema");
        let path = dir.join("theme.toml");
        std::fs::write(&path, "schema_version = 999\nfg = 1\nbg = 2\n").unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded, Theme::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other silent-misparse guard `deny_unknown_fields` provides: a
    /// field this build does not recognize at all (renamed or removed in a
    /// hypothetical future schema) must fail to parse loudly rather than
    /// being dropped in silence while the recognized fields load as if
    /// nothing were missing.
    #[test]
    fn unrecognized_field_falls_back_to_default_instead_of_being_silently_dropped() {
        let dir = tmp_dir("unknown-field");
        let path = dir.join("theme.toml");
        std::fs::write(&path, "fg = 1\nbg = 2\nsome_future_field = true\n").unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded, Theme::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_creates_missing_parent_directories() {
        let dir = tmp_dir("nested");
        let path = dir.join("nested").join("dirs").join("theme.toml");
        let theme = Theme {
            fg: Some(1),
            bg: Some(2),
            ..Theme::default()
        };
        store_to_path(theme, &path);
        assert!(path.exists());
        let loaded = load_from_path(&path);
        assert_eq!(loaded, theme);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The load-bearing property this whole seeding mechanism exists to
    /// prove: seeding a fresh `HlTable` from a cached `Theme` and then
    /// re-deriving through the exact live path (`Theme::from_hl`) recovers
    /// the cached values, with zero special-casing in the derivation
    /// itself.
    #[test]
    fn seed_hl_table_makes_named_groups_resolve_through_the_live_derivation_path() {
        let cached_theme = Theme {
            fg: Some(0x111111),
            bg: Some(0x222222),
            status_line: ResolvedStyle {
                fg: Some(0xAAAAAA),
                bg: Some(0xBBBBBB),
                bold: true,
                italic: false,
                underline: false,
                reverse: false,
            },
            pmenu_sel: ResolvedStyle {
                fg: Some(0xCCCCCC),
                bg: Some(0xDDDDDD),
                bold: false,
                italic: false,
                underline: false,
                reverse: true,
            },
            ..Theme::default()
        };
        let mut hl = empty_hl_table();
        seed_hl_table(&mut hl, &cached_theme);
        let derived = Theme::from_hl(&hl);
        assert_eq!(derived.fg, Some(0x111111));
        assert_eq!(derived.bg, Some(0x222222));
        assert_eq!(derived.status_line, cached_theme.status_line);
        assert_eq!(derived.pmenu_sel, cached_theme.pmenu_sel);
    }

    #[test]
    fn seed_hl_table_gives_each_named_group_a_distinct_synthetic_hl_id() {
        let mut hl = empty_hl_table();
        seed_hl_table(&mut hl, &Theme::default());
        let ids: std::collections::HashSet<u64> = hl.groups.values().copied().collect();
        assert_eq!(
            ids.len(),
            hl.groups.len(),
            "each named group must get its own synthetic hl_id, not a shared one"
        );
    }

    /// The only test touching process environment; the state-dir env
    /// precedence and the public `load`/`store` wiring both need at least
    /// one end-to-end check that does not bypass `state_dir_from_env`, but
    /// mutating shared process env is inherently racy against any other
    /// test doing the same, so exactly one test owns that responsibility.
    #[test]
    fn load_and_store_round_trip_through_xdg_state_home() {
        let dir = tmp_dir("xdg-e2e");
        let prev = std::env::var("XDG_STATE_HOME").ok();
        std::env::set_var("XDG_STATE_HOME", &dir);

        let config_path = Path::new("/home/x/.config/view/view.toml");
        let theme = Theme {
            fg: Some(0x123456),
            bg: Some(0x654321),
            ..Theme::default()
        };
        store(theme, config_path);
        let loaded = load(config_path);

        match prev {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(loaded, theme);
    }
}
