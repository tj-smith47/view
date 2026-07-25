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
//! `:help 'ttyfast'`, `:help $NVIM`) rather than from the offenders someone
//! happened to hit, and every entry was confirmed against the pinned binary.
//! Deliberately absent:
//!
//! - `HOME`: once the `XDG_*_HOME` variables are set, Neovim derives none of
//!   its own directories from it, and overriding it would break the
//!   unrelated host tooling (Cargo, git) a harness runs alongside the child.
//! - `XDG_RUNTIME_DIR`: names where the child writes its server socket, not
//!   where it reads configuration or code from, and a private replacement
//!   deep inside a scratch tree risks overflowing the 104-byte limit on a
//!   Unix socket path, turning a hygiene measure into a spawn failure.
//! - `TERM`, `SHELL`, `TMPDIR`: terminal and subprocess behavior, no
//!   configuration lookup. A measurement harness that needs them pinned
//!   pins them itself, as the value it wants is a property of the
//!   measurement, not a fixed hermetic constant.

use std::path::PathBuf;

/// Environment variables removed outright from a hermetic child's
/// environment: each one either injects startup commands or redirects where
/// the child finds configuration, runtime files, or plugin manifests.
///
/// Removal is the correct neutralizer for every entry here, since Neovim
/// derives its own value for each when it is unset. Contrast
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
];

/// Environment variables that must be *overridden* with an empty directory
/// rather than removed: Neovim substitutes system-wide defaults (`/etc/xdg`
/// and `/usr/local/share:/usr/share`) when they are unset, so clearing them
/// selects a host path instead of no path.
///
/// Both feed 'runtimepath' with a directory whose `plugin/` scripts the
/// child sources at startup, and `--clean` does not exclude them: it drops
/// the *user* directories only. Confirmed against the pinned binary, which
/// sourced a plugin from each of them under `--clean`.
pub const HOST_SEARCH_PATH_VARS: &[&str] = &["XDG_CONFIG_DIRS", "XDG_DATA_DIRS"];

/// A directory to point [`HOST_SEARCH_PATH_VARS`] at, guaranteed to contain
/// nothing because nothing ever creates it.
///
/// Neovim accepts a nonexistent search path, adds it to 'runtimepath', and
/// finds nothing under it; it never creates the directory itself (confirmed
/// against the pinned binary). Never creating it is what makes the
/// guarantee hold without a cleanup path that a crashed process would skip.
/// The process id keeps two concurrent runs from being neutralized by one
/// stray directory somebody created by hand.
#[must_use]
pub fn empty_search_path() -> PathBuf {
    std::env::temp_dir().join(format!("view-hermetic-empty-{}", std::process::id()))
}
