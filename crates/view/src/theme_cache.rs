//! Loads and stores the last-derived [`Theme`] to disk, keyed by the
//! resolved config path, so the first paint after startup can already be
//! themed correctly before nvim's `default_colors_set`/`hl_attr_define`
//! events arrive over the wire. `view-core` stays free of `serde`/`toml` (a
//! hard dependency-direction rule), so the on-disk shape and its
//! (de)serialization live entirely in this bin-crate module; [`CachedTheme`]
//! is the only type that ever touches `serde`.
//!
//! Corrupt or missing cache state is never fatal: every failure path here
//! logs to stderr and falls back to `Theme::default()` (or, for `store`,
//! simply gives up) rather than propagating an error startup would have to
//! handle specially.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use view_core::theme::Theme;

/// The on-disk mirror of [`Theme`]: identical fields, but `Serialize`/
/// `Deserialize` derive here instead of on `Theme` itself, since
/// `view-core` must not depend on `serde`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
struct CachedTheme {
    fg: Option<u32>,
    bg: Option<u32>,
}

impl From<Theme> for CachedTheme {
    fn from(theme: Theme) -> Self {
        Self {
            fg: theme.fg,
            bg: theme.bg,
        }
    }
}

impl From<CachedTheme> for Theme {
    fn from(cached: CachedTheme) -> Self {
        Self {
            fg: cached.fg,
            bg: cached.bg,
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
/// cannot be determined, the cache file is missing or unreadable, or its
/// contents fail to parse -- every failure degrades instead of blocking
/// startup.
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
/// tests can exercise missing/corrupt-file behavior without mutating
/// process environment.
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

    #[test]
    fn store_creates_missing_parent_directories() {
        let dir = tmp_dir("nested");
        let path = dir.join("nested").join("dirs").join("theme.toml");
        let theme = Theme {
            fg: Some(1),
            bg: Some(2),
        };
        store_to_path(theme, &path);
        assert!(path.exists());
        let loaded = load_from_path(&path);
        assert_eq!(loaded, theme);
        let _ = std::fs::remove_dir_all(&dir);
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
