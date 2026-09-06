//! Proves the property every cold-spawn row depends on: a sample killed at
//! its first painted frame leaves the engine's swap file behind, and the
//! next spawn into the same state home is never offered it to recover.
//!
//! Unix only: the kill a sample ends with is a process-group signal, and
//! the operand's swap is materialised here by the same pty-hosted editor a
//! row measures. A different platform fact is why the cleanup itself can
//! fail rather than why this file is fenced: `remove_file` on a still-open
//! handle succeeds on unix and is refused on Windows, so a child that
//! outlives `BenchSession::drop`'s 2s wait leaves its swap there --
//! `session.rs`'s own `#[cfg(windows)]` unit test exercises that case
//! directly, since `empty_swap_dir` is crate-private to this integration
//! test.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use view_bench::remote_ui::RemoteUiServer;
use view_bench::session::{BenchSession, SpawnSpec};
use view_test_support::ScratchDir;

/// How many consecutive killed spawns the claim covers. Three, because the
/// fault it pins needs two leftovers to reach its prompt: the second spawn
/// recovers the first's swap and writes one beside it, and the third is
/// the one that parks.
const SPAWNS: usize = 3;

/// How long a spawn is given to materialise its swap file, before the
/// host's own contention is accounted for. All of it is a pty spawning an
/// editor that then writes a byte.
const SWAP_BOUND: Duration = Duration::from_secs(20);

/// The step between two looks at the swap directory.
const RECHECK: Duration = Duration::from_millis(25);

/// Resolves the engine the rest of the harness measures, skipping rather
/// than failing when it is absent: this is a mechanism proof, not a
/// dependency check, and `task ci` already verifies the pin separately.
fn nvim_bin() -> Option<PathBuf> {
    let bin =
        std::env::var_os("VIEW_NVIM_BIN").map_or_else(|| PathBuf::from("nvim"), PathBuf::from);
    std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|_| bin)
}

/// The swap files a directory holds, sorted so a failure names them in a
/// stable order.
fn swap_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A spawn of `nvim` over an operand in `dir` that materialises its swap
/// file, keeping its state under `state_home`.
///
/// No `-n`: the swap file this pins the cleanup of is one the editor has to
/// be allowed to write, and the operand is modified so nvim materialises it
/// rather than only naming it.
fn swap_writing_spec(nvim: PathBuf, dir: &ScratchDir, state_home: &Path) -> SpawnSpec {
    let file = dir.join("scratch.txt");
    std::fs::write(&file, "SWAPHYGIENEFIXTURE\n").unwrap();
    SpawnSpec {
        program: nvim,
        args: vec![
            OsString::from("-u"),
            OsString::from("NONE"),
            OsString::from("-c"),
            OsString::from("normal! Ax"),
            OsString::from("-c"),
            OsString::from("preserve"),
            file.into_os_string(),
        ],
        env: vec![
            (OsString::from("TERM"), OsString::from("xterm-256color")),
            (
                OsString::from("XDG_STATE_HOME"),
                state_home.as_os_str().to_os_string(),
            ),
        ],
        cwd: Some(dir.to_path_buf()),
        measured_program: None,
    }
}

#[test]
fn three_consecutive_killed_spawns_are_never_offered_a_second_swap() {
    let Some(nvim) = nvim_bin() else {
        view_test_support::announce_skip(
            "three_consecutive_killed_spawns_are_never_offered_a_second_swap",
            "no nvim on PATH or at $VIEW_NVIM_BIN",
        );
        return;
    };
    let dir = ScratchDir::new("bench-swap-hygiene").unwrap();
    let state_home = dir.join("xdg_state_home");
    let swap = state_home
        .join(view_oracle::engine_state_dir_name())
        .join("swap");
    let spec = swap_writing_spec(nvim, &dir, &state_home);
    // what view keeps under the same root a sample's swap lands in: the
    // first-run record it writes during its claims stage, and the theme
    // cache it reads on the next launch. A side that loses these between
    // samples measures a first run on every one of them, against a bare
    // engine that has no such state to lose. The assertion below only cares
    // that two files beside the swap directory survive, so the names here
    // are today's real ones (`view-native/src/paths.rs`'s first-run record,
    // `view/src/theme_cache.rs`'s cache) written as plain literals rather
    // than reached through a dependency this crate's direction audit would
    // have to allow
    let warm = state_home.join("view");
    std::fs::create_dir_all(&warm).unwrap();
    let record = warm.join("native-first-run.toml");
    let theme = warm.join("theme-0123456789abcdef.toml");
    std::fs::write(&record, b"shown = []\n").unwrap();
    std::fs::write(&theme, b"# a theme this side already resolved\n").unwrap();

    for spawn in 0..SPAWNS {
        assert!(
            swap_files(&swap).is_empty(),
            "spawn {spawn} starts against {:?}, so it faces nvim's recover-which-one dialog \
             instead of the buffer the row measures",
            swap_files(&swap)
        );
        let mut session = BenchSession::spawn(&spec).expect("a pty-hosted editor");
        let deadline = Instant::now() + view_test_support::host_deadline(SWAP_BOUND);
        while swap_files(&swap).is_empty() && Instant::now() < deadline {
            std::thread::sleep(RECHECK);
        }
        assert_eq!(
            swap_files(&swap).len(),
            1,
            "spawn {spawn} never materialised a swap file, so the cleanup below is pinned \
             against nothing; screen:\n{}",
            session.screen_text()
        );
        assert!(
            !session.screen_text().contains("swap file"),
            "spawn {spawn} is parked on a swap dialog rather than holding the operand; \
             screen:\n{}",
            session.screen_text()
        );
        drop(session);
        assert!(
            swap_files(&swap).is_empty(),
            "the reaped spawn left {:?} for the next one",
            swap_files(&swap)
        );
        assert!(
            record.exists() && theme.exists(),
            "spawn {spawn} took view's own state with the swap, so the next sample pays a \
             first run the bare-engine side it is paired against never pays"
        );
    }
}

/// The one editor spawn in this crate that is not pty-hosted: the control
/// arm's headless server opens the row's operand and dies by group
/// `SIGKILL`, so it leaves the same swap file a sample does.
#[test]
fn the_control_servers_swap_leaves_with_the_server() {
    let Some(nvim) = nvim_bin() else {
        view_test_support::announce_skip(
            "the_control_servers_swap_leaves_with_the_server",
            "no nvim on PATH or at $VIEW_NVIM_BIN",
        );
        return;
    };
    let dir = ScratchDir::new("bench-swap-control").unwrap();
    let state_home = dir.join("xdg_state_home");
    let swap = state_home
        .join(view_oracle::engine_state_dir_name())
        .join("swap");
    let spec = swap_writing_spec(nvim, &dir, &state_home);

    let server = RemoteUiServer::start(&spec, dir.join("ui.sock")).expect("headless server");
    let deadline = Instant::now() + view_test_support::host_deadline(SWAP_BOUND);
    while swap_files(&swap).is_empty() && Instant::now() < deadline {
        std::thread::sleep(RECHECK);
    }
    assert_eq!(
        swap_files(&swap).len(),
        1,
        "the server never materialised a swap file, so the cleanup below is pinned against \
         nothing"
    );
    drop(server);
    assert!(
        swap_files(&swap).is_empty(),
        "the reaped server left {:?} in the side directory its own row spawns into next",
        swap_files(&swap)
    );
}
