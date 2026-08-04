//! Scaffolding shared by this crate's integration test binaries
//! (`smoke.rs`, `driver_legs.rs`): locating the `view` binary, isolating a
//! spawned `view` process's `XDG_*_HOME` from the host's real nvim config,
//! and a scratch-file/isolated-home pair that cleans itself up on drop.
//!
//! Compiled separately into each of those binaries, so a helper only one of
//! them needs reads as dead code in the other: `dead_code` is allowed here
//! for that reason alone, never because an unused helper is acceptable.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Locates the `view` binary next to this crate's own target directory,
/// always invoking `cargo build -p view` first to guarantee it reflects
/// the current source tree.
///
/// `view` is a bin-only crate with no library target, so Cargo's
/// `CARGO_BIN_EXE_<name>` mechanism is unavailable: Cargo only sets that
/// variable for binaries reachable via a package's own dependency graph, and
/// it refuses to add a lib-less crate as a dependency at all (confirmed by
/// attempting exactly that: `cargo add view -p view-oracle --dev` succeeds
/// but emits "ignoring invalid dependency `view` which is missing a lib
/// target", and `env!("CARGO_BIN_EXE_view")` then fails to compile). Falls
/// back to locating the workspace `target/<profile>/view` executable
/// directly.
///
/// The build call is unconditional, not gated on `!path.exists()`: an
/// existence check only proves *some* binary was built once before, not
/// that it reflects the source this test process just compiled against.
/// A stale binary left over from an earlier build (e.g. one taken while
/// iterating on `crates/view` itself with `git stash`) previously produced
/// a false RED or a false GREEN under a direct `cargo test -p
/// view-oracle`, indistinguishable from a real pass/fail until someone
/// noticed the binary's mtime predated the source. `cargo build` is a
/// no-op (a fast up-to-date check, not a recompile) when the binary is
/// already current, so paying for the invocation on every run is cheap
/// insurance against exactly that class of false result.
pub fn view_bin_path() -> PathBuf {
    let profile_dir = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("target");
    path.push(profile_dir);
    path.push("view");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = std::process::Command::new(cargo)
        .args(["build", "-p", "view"])
        .status()
        .expect("failed to invoke cargo build -p view");
    assert!(status.success(), "cargo build -p view failed");
    path
}

/// Returns the workspace `target/view-oracle-scratch` directory, creating
/// it, as the one place this crate's tests write scratch state.
///
/// Never `std::env::temp_dir()`: the system temp dir is world-writable and
/// every scratch name here is predictable (a label plus this process's pid),
/// so an unrelated process can pre-create one of these paths as a symlink
/// and have a test write through it -- which matters most for the one
/// scratch file that gets marked executable and then run as the engine
/// binary. A checkout's own build tree is the only directory the test
/// already owns outright.
pub fn scratch_root() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    let root = root.join("target").join("view-oracle-scratch");
    std::fs::create_dir_all(&root).expect("failed to create the scratch root");
    root
}

/// Points every `XDG_*_HOME` var `cmd` sees at a subdirectory of `home`,
/// isolating the spawned process from the host's real nvim config: a
/// dashboard plugin or custom keymap on a bare "i" would otherwise make a
/// typed-text assertion nondeterministic.
///
/// Only the four directories: the environment variables that redirect an
/// editor's configuration from outside them are dropped by
/// `PtySession::spawn_configured` for every pty spawn in the tree, this
/// one included, so that no caller has to remember a list to stay
/// hermetic.
pub fn isolate_xdg(cmd: &mut portable_pty::CommandBuilder, home: &Path) {
    isolate_xdg_first_launch(cmd, home);
    disable_native_features(home);
}

/// [`isolate_xdg`] without the config file, for the one test whose subject
/// is what a first launch does: reading the config, taking the superseded
/// surfaces over, claiming the feature keys and introducing all of it once.
pub fn isolate_xdg_first_launch(cmd: &mut portable_pty::CommandBuilder, home: &Path) {
    for var in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        cmd.env(var, xdg_home(home, var));
    }
}

/// `home`'s subdirectory for the `XDG_*_HOME` variable named `var`. One
/// derivation for both the environment `isolate_xdg` sets and the files
/// planted under those directories, so a session never reads a config from
/// a directory it was not pointed at.
///
/// Public because a test that plants its own file under one of those
/// directories -- or reads back a file the spawned process wrote to one --
/// must derive the path the same single way, not spell it out a second
/// time beside a definition it would silently diverge from.
pub fn xdg_home(home: &Path, var: &str) -> PathBuf {
    home.join(var.to_lowercase())
}

/// Writes a `view.toml` under `home` that switches every native feature
/// off.
///
/// A native takeover stops nvim drawing a surface so view can draw it
/// itself, which is a deliberate divergence from what nvim alone paints. A
/// test that asserts screen content is asserting the nvim-owned surfaces,
/// so it would otherwise fail on the takeover -- for a reason it never set
/// out to measure -- the day any feature ships one. Generated from the
/// registry rather than written out here, so a feature added later is
/// switched off by the same call instead of quietly defaulting on.
pub fn disable_native_features(home: &Path) {
    let dir = xdg_home(home, "XDG_CONFIG_HOME").join("view");
    std::fs::create_dir_all(&dir).expect("the isolated config home must be creatable");
    let mut text = String::from("[native]\n");
    for feature in view_core::native::registry::features() {
        text.push_str(feature.id);
        text.push_str(" = false\n");
    }
    std::fs::write(dir.join("view.toml"), text).expect("the isolated view.toml must be writable");
}

static NEXT_SCRATCH_ID: AtomicU64 = AtomicU64::new(0);

/// A scratch file and an isolated `XDG_*_HOME` directory, both removed on
/// drop. Disambiguated by an atomic counter, not just the test process's
/// pid: multiple tests in the same integration-test binary can spawn a
/// session concurrently, so pid alone would collide.
pub struct ScratchPaths {
    pub scratch: PathBuf,
    pub isolated_home: PathBuf,
}

impl ScratchPaths {
    /// `label` names the calling test binary (e.g. `"smoke"`,
    /// `"driver-legs"`) so scratch paths from different binaries never
    /// collide on disk even when run concurrently.
    pub fn new(label: &str) -> Self {
        let pid = std::process::id();
        let id = NEXT_SCRATCH_ID.fetch_add(1, Ordering::Relaxed);
        let root = scratch_root();
        let scratch = root.join(format!("{label}-{pid}-{id}.txt"));
        let isolated_home = root.join(format!("{label}-home-{pid}-{id}"));
        std::fs::create_dir_all(&isolated_home).unwrap();
        Self {
            scratch,
            isolated_home,
        }
    }
}

impl Drop for ScratchPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.scratch);
        let _ = std::fs::remove_dir_all(&self.isolated_home);
    }
}
