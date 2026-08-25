//! Scaffolding shared by every integration test binary in this crate that
//! spawns a real `view` process: locating the `view` binary, isolating a
//! spawned `view` process's `XDG_*_HOME` from the host's real nvim config,
//! and a scratch-file/isolated-home pair that cleans itself up on drop,
//! plus the wall-clock budget a timing test holds a startup sequence to
//! ([`startup_budget`], over the workspace-shared
//! [`view_test_support::HostBudget`]).
//!
//! Compiled separately into each of those binaries, so a helper only one of
//! them needs reads as dead code in the other: `dead_code` is allowed here
//! for that reason alone, never because an unused helper is acceptable.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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
/// already current, so paying for the invocation once is cheap insurance
/// against exactly that class of false result.
///
/// Built once per test process, not once per spawn. The source cannot
/// change under a running test binary, so every call after the first can
/// only re-confirm what the first proved -- and the cargo invocation that
/// re-confirms it takes the workspace build-directory lock, which on a
/// host running other cargo work blocks for as long as that work holds it.
/// A spawn helper that pays that inside a caller's timing window measures
/// the neighbours instead of view.
pub fn view_bin_path() -> PathBuf {
    static BUILT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILT.get_or_init(build_view_bin).clone()
}

fn build_view_bin() -> PathBuf {
    let profile_dir = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let path = view_oracle::target_root().join(profile_dir).join("view");
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

/// Points every `XDG_*_HOME` var `cmd` sees at a subdirectory of `home`
/// AND plants a `view.toml` there turning every native feature off,
/// isolating the spawned process both from the host's real nvim config and
/// from view's own default-on chrome: a dashboard plugin, a custom keymap
/// on a bare "i", or a statusline row view itself paints would otherwise
/// make a typed-text assertion nondeterministic.
///
/// The name carries the config half deliberately. Writing files is not what
/// "isolate XDG" says, and a caller reading only the name would take the
/// all-off `view.toml` for the absent-config default
/// (`NativeConfig::load`'s documented "full experience"), which is what
/// [`isolate_xdg_first_launch`] actually leaves in place.
///
/// Only the four directories: the environment variables that redirect an
/// editor's configuration from outside them are dropped by
/// `PtySession::spawn_configured` for every pty spawn in the tree, this
/// one included, so that no caller has to remember a list to stay
/// hermetic.
pub fn isolate_xdg_native_off(cmd: &mut portable_pty::CommandBuilder, home: &Path) {
    isolate_xdg_first_launch(cmd, home);
    disable_native_features(home);
}

/// [`isolate_xdg_native_off`] without the config file, for the one test
/// whose subject is what a first launch does: reading the config, taking
/// the superseded surfaces over, claiming the feature keys and introducing
/// all of it once.
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
/// derivation for both the environment `isolate_xdg_native_off` sets and
/// the files planted under those directories, so a session never reads a
/// config from a directory it was not pointed at.
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

/// view-tui's `PROBE_DEADLINE`: how long the capability probe's first
/// window -- the one that runs before the alternate screen goes up -- waits
/// for replies that a silent terminal never sends.
///
/// Re-declared rather than imported. This crate takes no dependency on
/// view-tui (see the crate's module doc, and the crossterm/ratatui reach
/// rows in `scripts/audit-deps.sh` that such an edge would trip), so the
/// copy is held against the definition by
/// `the_probe_constant_copies_still_match_view_tuis_own` in this crate's
/// `timing_bounds` suite instead of by the type system.
pub const PROBE_DEADLINE: Duration = Duration::from_millis(50);

/// view-tui's `PROBE_HARD_CAP`: the total the capability probe may wait for
/// the DA1 fence, measured from the query write, so it subsumes
/// [`PROBE_DEADLINE`] rather than following it.
///
/// A terminal that answers ends both windows early; a pty that answers
/// nothing spends all of this, the part past the first window behind the
/// engine attach. Copied for the same reason [`PROBE_DEADLINE`] is, and held
/// by the same pin.
pub const PROBE_HARD_CAP: Duration = Duration::from_millis(400);

/// The wall-clock bound a pty startup sequence gets on a host with nothing
/// else to do, and the bound this crate asserted before any load reading
/// existed.
///
/// Kept as the anchor rather than replaced: [`startup_budget`] widens only
/// what the host is being asked to do, so an idle host -- and a host that
/// publishes no load at all -- still asserts exactly this.
pub const FLAT_STARTUP_BOUND: Duration = Duration::from_secs(3);

/// The wall clock a pty startup sequence may take, given that the sequence's
/// own constants sum to `fixed`.
///
/// The sequence is what these tests prove -- the probe goes unanswered, the
/// tier falls back at [`PROBE_DEADLINE`], the engine attaches, a typed
/// character reaches the buffer -- and only the fixed half of that is a
/// claim about view. The rest is a process spawn, an nvim start and a file
/// write, all of which take as long as the host lets them, so
/// [`view_test_support::HostBudget`] scales that half by the contention
/// this run actually started under. The absolute "how fast does view start"
/// claim lives in the bench's `first_paint` rows, which carry load-aware
/// headroom of their own; a restatement of it here would be a second,
/// weaker copy of that gate.
pub fn startup_budget(fixed: Duration) -> view_test_support::HostBudget {
    view_test_support::HostBudget::new(fixed, FLAT_STARTUP_BOUND.saturating_sub(fixed))
}
