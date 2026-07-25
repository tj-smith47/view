//! Host environment variables that redirect where a spawned Neovim looks
//! for configuration, and the hermetic values that neutralize them.
//!
//! Pointing the four `XDG_*_HOME` variables at private directories does not
//! by itself detach a child from the host's editor setup. A handful of
//! other variables re-point the same lookups from outside those
//! directories, run commands before any of them are consulted, or (for the
//! two search-path variables) fall back to system-wide defaults when unset,
//! so that clearing them selects a host path rather than no path at all.
//!
//! The lists are enumerated from the pinned engine's own documentation
//! (`:help starting`, `:help standard-path`, `:help remote-plugin-manifest`,
//! `:help 'ttyfast'`, `:help $NVIM`), extended with the two LuaJIT search
//! paths that sit outside that documentation but reach the same child, and
//! every entry was confirmed against the pinned binary. Deliberately
//! absent:
//!
//! - `HOME`: once the `XDG_*_HOME` variables are set, Neovim derives none of
//!   its own directories from it, and overriding it would break the
//!   unrelated host tooling (Cargo, git) a harness runs alongside the child.
//! - `XDG_RUNTIME_DIR`: names where the child writes its server socket, not
//!   where it reads configuration or code from, and a private replacement
//!   deep inside a scratch tree risks overflowing the 104-byte limit on a
//!   Unix socket path, turning a hygiene measure into a spawn failure.
//! - `LUA_INIT` and `LUA_PATH_5_1`: LuaJIT honours neither inside Neovim
//!   (confirmed against the pinned binary: an `LUA_INIT` print never runs,
//!   and a module reachable only through `LUA_PATH_5_1` fails to resolve),
//!   unlike `LUA_PATH`/`LUA_CPATH`, which are in the removal list below.
//! - `LANG`, `LANGUAGE`, `LC_ALL`, `LC_MESSAGES`: the locale reaches the
//!   child, but the pinned binary ships no message catalogs at all (no
//!   `lang/` directory under its `$VIMRUNTIME` on either supported host),
//!   so a non-English locale leaves every message the screen-scraping
//!   oracles match in English. Confirmed by running the pinned binary under
//!   `LC_ALL=de_DE.UTF-8`: `v:lang` reported the locale as active while
//!   `E149` and `-- INSERT --` stayed English. Pinning `LC_ALL=C` anyway
//!   would trade that non-effect for a real one, since the `ctype` rules it
//!   selects are what the non-ASCII screen assertions read through.
//! - `TERM`, `SHELL`, `TMPDIR`: terminal and subprocess behavior, no
//!   configuration lookup. A measurement harness that needs them pinned
//!   pins them itself, as the value it wants is a property of the
//!   measurement, not a fixed hermetic constant. `TMPDIR` in particular is
//!   inert here only because [`empty_search_path`] deliberately does not
//!   live under it: a hermetic path selected by a host variable would be a
//!   host path with extra steps.

use std::io;
use std::path::{Path, PathBuf};

/// Environment variables removed outright from a hermetic child's
/// environment: each one either injects startup commands or redirects where
/// the child finds configuration, runtime files, plugin manifests, or Lua
/// modules.
///
/// Removal is the correct neutralizer for every entry here, since Neovim or
/// LuaJIT derives its own value for each when it is unset. Contrast
/// [`HOST_SEARCH_PATH_VARS`], where unset means a system-wide default.
pub const HOST_REDIRECT_VARS: &[&str] = &[
    // Ex command lines the child runs at startup, ahead of any config file:
    // arbitrary host code inside an otherwise hermetic child
    "VIMINIT",
    "EXINIT",
    // Neovim resolves this itself only when the host has not already set
    // it, so a host value survives into the child and is what a plugin's
    // `:source $MYVIMRC` reaches
    "MYVIMRC",
    // the GUI counterpart of MYVIMRC, inert in a child with no GUI to
    // source it. Carried because it belongs to the same documented set as
    // MYVIMRC and costs one string: an enumeration built from a
    // documentation sweep that silently omits one of its entries reads as
    // one whose other omissions were considered too, and they were not
    "MYGVIMRC",
    // both locate the runtime files that seed 'runtimepath'
    "VIM",
    "VIMRUNTIME",
    // redirects every standard directory below the XDG homes, so a host
    // value voids the config directory an otherwise hermetic
    // XDG_CONFIG_HOME just established, silently leaving the child with no
    // configuration at all
    "NVIM_APPNAME",
    // names the remote-plugin manifest, which is sourced as vimscript
    "NVIM_RPLUGIN_MANIFEST",
    // both tell the child it is a nested child of a live Neovim, which
    // changes what a plugin does at startup
    "NVIM",
    "NVIM_LISTEN_ADDRESS",
    // diverts the child's own log writes to a host path
    "NVIM_LOG_FILE",
    // forces 'nottyfast', changing what the TUI does during startup
    "NVIM_NOTTYFAST",
    // LuaJIT's own module search paths, outside the set Neovim documents
    // and therefore outside what a `:help` sweep finds: a host value lands
    // ahead of every compiled-in default in `package.path`/`package.cpath`
    // (confirmed against the pinned binary), so a `require()` of a name
    // absent from 'runtimepath' resolves against the host instead of
    // failing. A plugin's `pcall(require, "optional-dep")` probe taking the
    // other branch inside a measured process is the shape of that. Removal
    // rather than an empty override because an unset value yields LuaJIT's
    // compiled-in defaults, which the child's own modules need, while an
    // empty one would delete them
    "LUA_PATH",
    "LUA_CPATH",
];

/// Environment variables that must be *overridden* with an empty directory
/// rather than removed: Neovim substitutes system-wide defaults (`/etc/xdg`
/// and `/usr/local/share:/usr/share`) when they are unset, so clearing them
/// selects a host path instead of no path.
///
/// Both feed 'runtimepath' with a directory whose `plugin/` scripts the
/// child sources at startup, and `--clean` does not exclude them: it drops
/// the *user* directories only. Confirmed against the pinned binary, which
/// sourced a plugin from each of them under `--clean` (the two layouts
/// differ: `$XDG_CONFIG_DIRS/nvim/plugin/` and
/// `$XDG_DATA_DIRS/nvim/site/plugin/`).
pub const HOST_SEARCH_PATH_VARS: &[&str] = &["XDG_CONFIG_DIRS", "XDG_DATA_DIRS"];

/// The directory [`HOST_SEARCH_PATH_VARS`] are pointed at, whose emptiness
/// [`prepare_empty_search_path`] establishes before a hermetic child is
/// spawned.
///
/// Under the build tree rather than the system temp dir, matching where the
/// harness puts every other scratch tree: the temp dir is world-writable
/// with a guessable name, so a directory merely *expected* to stay empty is
/// a directory anyone can plant a `nvim/plugin/` script in, and every
/// "hermetic" child would then source it. Its path is resolved from this
/// crate's own manifest dir because the crate that owns the same derivation
/// for the harness bins sits above this one in the dependency order.
///
/// A build-machine path baked into a released binary never resolves at run
/// time, and never has to: only [`crate::process::EngineConfig::isolated`]
/// consults this, and the shipped editor spawns through
/// `EngineConfig::default` (pinned by test in the `view` binary itself).
#[must_use]
pub fn empty_search_path() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    root.join("target").join("view-hermetic-empty")
}

/// Creates [`empty_search_path`] if absent, verifies it holds nothing, and
/// makes it unwritable, returning the path a hermetic spawn points
/// `XDG_CONFIG_DIRS`/`XDG_DATA_DIRS` at.
///
/// Called at the spawn funnels rather than trusted as an invariant of the
/// path itself: "empty because nothing creates it" is a claim about this
/// repository's code, not about a directory on the machine running it, and
/// a child sourcing a planted `plugin/` script produces no error, no
/// warning, and no visible difference from a hermetic one.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the directory cannot be
/// created, read, or have its permissions set, and an
/// [`io::Error::other`] naming the offending entry if it is not empty.
pub fn prepare_empty_search_path() -> io::Result<PathBuf> {
    let path = empty_search_path();
    prepare_empty_dir(&path)?;
    Ok(path)
}

/// The body of [`prepare_empty_search_path`], taking its path as an
/// argument so a test can exercise the refusal against a directory of its
/// own instead of planting an entry in the one every concurrent spawn in
/// the same test binary is reading.
fn prepare_empty_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    // checked before the permission change below, so a directory that turns
    // out to hold something is left exactly as it was found
    if let Some(entry) = std::fs::read_dir(path)?.next() {
        let name = entry?.file_name();
        return Err(io::Error::other(format!(
            "the hermetic search path {} holds {:?}: a child pointed at it would source \
             whatever is planted there, so the spawn is refused rather than silently \
             measured against it",
            path.display(),
            name
        )));
    }
    // read+execute only: the emptiness checked a moment ago is a fact about
    // one instant, and the window between it and the child's startup is
    // exactly what a plant would aim for
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// A scratch directory of this test's own, outside the shared hermetic
    /// path that live spawns in this same binary are preparing concurrently.
    fn scratch(name: &str) -> PathBuf {
        let dir = empty_search_path()
            .with_file_name("view-env-tests")
            .join(name);
        // permissions restored first: a previous run left the directory
        // unwritable on purpose, and remove_dir_all cannot unlink through it
        #[cfg(unix)]
        if dir.exists() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_hermetic_search_path_is_empty_once_prepared() {
        let path = prepare_empty_search_path().unwrap();
        assert!(path.is_dir(), "{} is not a directory", path.display());
        assert!(
            std::fs::read_dir(&path).unwrap().next().is_none(),
            "{} holds an entry, so a hermetic child can source it",
            path.display()
        );
    }

    #[test]
    fn a_search_path_holding_anything_is_refused() {
        let dir = scratch("planted");
        std::fs::create_dir_all(dir.join("nvim/plugin")).unwrap();
        std::fs::write(dir.join("nvim/plugin/host.lua"), "-- planted").unwrap();
        let refused = prepare_empty_dir(&dir).unwrap_err();
        assert!(
            refused.to_string().contains("nvim"),
            "the refusal does not name what it found: {refused}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_prepared_search_path_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("prepared");
        prepare_empty_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o500,
            "the prepared search path stays writable, so a plugin can still be planted in it"
        );
    }
}
