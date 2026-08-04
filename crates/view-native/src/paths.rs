//! Where view reads and writes per-user files: the config file `[native]`
//! is loaded from, and the state directory the regenerable caches beside it
//! live in.
//!
//! Resolution lives here, in the crate that owns `view.toml`'s loader,
//! rather than in each consumer: the theme cache is keyed on the identity
//! of the config path, and a second copy of the fallback chain would let
//! the two disagree about which file that is, silently orphaning a cache on
//! whichever host has only one of `HOME` and `LOCALAPPDATA` set.
//!
//! Every function here answers `None` rather than guessing when no base
//! directory can be determined at all, which callers treat as "this
//! convenience is unavailable this run" instead of an error.

use std::path::{Path, PathBuf};

/// The config file view reads: `$XDG_CONFIG_HOME/view/view.toml`, falling
/// back to `~/.config/view/view.toml` (unix) or `%APPDATA%/view/view.toml`
/// (Windows).
///
/// Computes the path only. No file is read, opened, or required to exist,
/// so a caller that just needs the cache key the path *identity* provides
/// carries no dependency on the loader.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    env_dir("XDG_CONFIG_HOME")
        .or_else(|| env_dir("HOME").map(|h| h.join(".config")))
        .or_else(|| env_dir("APPDATA"))
        .map(|base| base.join("view").join("view.toml"))
}

/// The base directory for regenerable per-user state:
/// `$XDG_STATE_HOME`, falling back to the unix convention
/// (`~/.local/state`) or the Windows convention (`%LOCALAPPDATA%`, itself
/// already a per-user state-like directory).
#[must_use]
pub fn state_dir() -> Option<PathBuf> {
    env_dir("XDG_STATE_HOME")
        .or_else(|| env_dir("HOME").map(|h| h.join(".local").join("state")))
        .or_else(|| env_dir("LOCALAPPDATA"))
}

/// view's own directory inside `state_dir`: the one place every cache file
/// view writes lives, so the theme cache and the first-run record are
/// found, inspected, and deleted together.
///
/// Takes the base rather than resolving it, so path construction stays pure
/// and directly testable apart from the environment resolution
/// [`state_dir`] performs.
#[must_use]
pub fn cache_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("view")
}

/// The record of which first-run notices have already been shown, beside
/// the theme cache in [`cache_dir`].
///
/// One file covering every config path rather than one file per config
/// path: a user opens this record to find out why a notice stopped
/// appearing, and a hash-named file per config would leave them guessing
/// which of several is theirs.
#[must_use]
pub fn first_run_record(state_dir: &Path) -> PathBuf {
    cache_dir(state_dir).join("native-first-run.toml")
}

/// A directory path from environment variable `var`, treating unset and
/// empty identically: shells routinely export empty XDG vars, and an empty
/// base would silently anchor every path built on it at the filesystem
/// root.
#[must_use]
fn env_dir(var: &str) -> Option<PathBuf> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    // the env-mutation sites below are the ones ENV_MUTATION_LOCK exists to
    // bound; each holds the guard across its own restore
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]

    use super::*;

    /// Serializes every test here that mutates the process environment.
    /// These tests set and then restore the same variable names, so two of
    /// them overlapping would interleave one's restore with another's plant
    /// and leave the loser reading a value it never set. Held for the whole
    /// body, restore included, because releasing it between the mutation
    /// and the restore is what opens that window.
    static ENV_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// [`ENV_MUTATION_LOCK`], with poisoning ignored: it orders two
    /// operations and guards no data, so a test that panicked while holding
    /// it left nothing behind for the next one to find broken.
    fn env_mutation_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTATION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Restores `var` to `prev`, the tail every environment-planting test
    /// here runs before releasing the guard.
    fn restore(var: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn config_path_prefers_xdg_config_home_then_home_then_appdata() {
        let _guard = env_mutation_guard();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        let prev_appdata = std::env::var("APPDATA").ok();

        std::env::set_var("XDG_CONFIG_HOME", "/xdg-cfg");
        assert_eq!(
            config_path(),
            Some(PathBuf::from("/xdg-cfg/view/view.toml"))
        );

        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::set_var("HOME", "/home/x");
        assert_eq!(
            config_path(),
            Some(PathBuf::from("/home/x/.config/view/view.toml"))
        );

        std::env::remove_var("HOME");
        std::env::set_var("APPDATA", "/appdata");
        assert_eq!(
            config_path(),
            Some(PathBuf::from("/appdata/view/view.toml"))
        );

        std::env::remove_var("APPDATA");
        assert_eq!(config_path(), None);

        restore("XDG_CONFIG_HOME", prev_xdg);
        restore("HOME", prev_home);
        restore("APPDATA", prev_appdata);
    }

    #[test]
    fn state_dir_prefers_xdg_state_home_then_home_then_localappdata() {
        let _guard = env_mutation_guard();
        let prev_xdg = std::env::var("XDG_STATE_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        let prev_local = std::env::var("LOCALAPPDATA").ok();

        std::env::set_var("XDG_STATE_HOME", "/xdg-state");
        assert_eq!(state_dir(), Some(PathBuf::from("/xdg-state")));

        std::env::remove_var("XDG_STATE_HOME");
        std::env::set_var("HOME", "/home/x");
        assert_eq!(state_dir(), Some(PathBuf::from("/home/x/.local/state")));

        std::env::remove_var("HOME");
        std::env::set_var("LOCALAPPDATA", "/localappdata");
        assert_eq!(state_dir(), Some(PathBuf::from("/localappdata")));

        std::env::remove_var("LOCALAPPDATA");
        assert_eq!(state_dir(), None);

        restore("XDG_STATE_HOME", prev_xdg);
        restore("HOME", prev_home);
        restore("LOCALAPPDATA", prev_local);
    }

    #[test]
    fn an_empty_variable_falls_through_instead_of_anchoring_at_the_root() {
        let _guard = env_mutation_guard();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_home = std::env::var("HOME").ok();

        std::env::set_var("XDG_CONFIG_HOME", "");
        std::env::set_var("HOME", "/home/x");
        assert_eq!(
            config_path(),
            Some(PathBuf::from("/home/x/.config/view/view.toml"))
        );

        restore("XDG_CONFIG_HOME", prev_xdg);
        restore("HOME", prev_home);
    }

    #[test]
    fn the_first_run_record_sits_in_the_same_directory_every_cache_file_does() {
        let base = Path::new("/state");
        assert_eq!(
            first_run_record(base).parent(),
            Some(cache_dir(base).as_path()),
            "the record must be beside the caches, not above them"
        );
        assert_eq!(cache_dir(base), PathBuf::from("/state/view"));
    }
}
