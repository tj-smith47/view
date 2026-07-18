//! Scaffolding shared by this crate's integration test binaries
//! (`smoke.rs`, `driver_legs.rs`): locating the `view` binary, isolating a
//! spawned `view` process's `XDG_*_HOME` from the host's real nvim config,
//! and a scratch-file/isolated-home pair that cleans itself up on drop.
#![allow(clippy::unwrap_used, clippy::expect_used)]

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

/// Points every `XDG_*_HOME` var `cmd` sees at a subdirectory of `home`,
/// isolating the spawned process from the host's real nvim config: a
/// dashboard plugin or custom keymap on a bare "i" would otherwise make a
/// typed-text assertion nondeterministic.
pub fn isolate_xdg(cmd: &mut portable_pty::CommandBuilder, home: &Path) {
    for var in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        cmd.env(var, home.join(var.to_lowercase()));
    }
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
        let scratch = std::env::temp_dir().join(format!("view-oracle-{label}-{pid}-{id}.txt"));
        let isolated_home =
            std::env::temp_dir().join(format!("view-oracle-{label}-home-{pid}-{id}"));
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
