//! One editor process under measurement: a [`PtySession`] at the protocol
//! grid size plus the observation helpers scenarios share (quiescence
//! settle, cell waits, bounded shutdown). The caller supplies a fully
//! resolved [`SpawnSpec`] (binary, args, environment overrides, cwd);
//! fixture semantics stay in `view-harness`, which owns what the
//! environment means.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;
use view_oracle::{PtySession, QueryPolicy};

use crate::boundaries;
use crate::BenchError;

/// Terminal grid every measurement runs at, per the measurement protocol's
/// hermetic-environment contract.
pub const GRID_COLS: u16 = 120;
/// See [`GRID_COLS`].
pub const GRID_ROWS: u16 = 40;

/// Everything needed to spawn one editor for measurement. Environment
/// entries are overrides on top of the inherited environment (`PATH` stays
/// usable for an engine spawned from it), matching how the compat driver
/// spawns its sessions.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
    /// The binary the spawn measures when `program` is a wrapper that
    /// execs it, and `None` when the two are the same. A wrapper is opaque
    /// to everything downstream: read off `program`, a shell shim is a
    /// shell, and a caller asking what is under measurement gets the
    /// wrapper's answer instead of the editor's. Recorded by whoever wraps
    /// the spawn, since only that code knows what it wrapped.
    pub measured_program: Option<PathBuf>,
}

/// The spawn spec for the side under measurement.
///
/// The two sides carry distinct types so that a call transposing them
/// cannot compile. A transposed pair spawns each side in the other's role,
/// so the run still settles, still samples and still reports -- with the
/// baseline recorded as the measured side and every ratio the perf gate
/// reads inverted, and the numbers stay plausible enough that nothing
/// downstream can detect the swap.
#[derive(Debug, Clone, Copy)]
pub struct ViewSpec<'a>(pub &'a SpawnSpec);

/// The spawn spec for the bare-editor baseline the measured side is paired
/// against. See [`ViewSpec`] for why the sides are separate types.
#[derive(Debug, Clone, Copy)]
pub struct NvimSpec<'a>(pub &'a SpawnSpec);

/// The two bounds a quiescence wait needs.
///
/// Both are durations, so as adjacent positional arguments they transpose
/// silently: a 30-second quiet span inside a 2-second deadline refuses
/// every startup, and a 2-second deadline read as the quiet span accepts a
/// screen that never settled. Named fields make the pair say which is
/// which at the call site.
#[derive(Debug, Clone, Copy)]
pub struct SettleBound {
    /// How long the screen must hold still to count as settled.
    pub quiet: Duration,
    /// How long to wait for that to happen before giving up.
    pub deadline: Duration,
}

/// A spawned editor under measurement.
pub struct BenchSession {
    pty: PtySession,
    swap_dir: Option<PathBuf>,
}

/// The environment variable the compat fixtures read their probe-channel
/// address from. Every bench row reuses those fixtures, so every bench
/// spawn inherits the `serverstart` call whether or not it wants one.
///
/// The fixtures bind that address as their first statement. A sample
/// killed at its first painted frame -- which is exactly what the
/// `first_paint` row does to every one of its spawns -- never runs an exit
/// path, so the bound socket outlives it, and the next spawn in the same
/// side directory dies on `EADDRINUSE` before painting anything. Observed:
/// a `first_paint` cell timing out at 30 seconds against nvim's "Press
/// ENTER" prompt, with `E5113: Failed to start server: address already in
/// use` behind it. No bench row ever connects to the channel, so the
/// address has only to be unique and writable.
const PROBE_SOCKET_VAR: &str = "VIEW_COMPAT_SOCK";

/// The environment variable deciding where the editor under measurement
/// keeps its state, the swap directory this session empties included.
///
/// Shared by every spawn of a side rather than made unique per spawn: view
/// keeps its own first-run record and theme cache under this same root, so
/// a fresh one per sample would make every view sample a first run while
/// the bare-nvim arm it is paired against pays nothing -- an asymmetry
/// straight into the ratio the row reports. What a killed sample actually
/// leaves here is the engine's swap file, and that is removed once the
/// child is reaped ([`BenchSession::drop`]).
const STATE_HOME_VAR: &str = "XDG_STATE_HOME";

/// The engine's swap directory under the state home `env` names, and
/// `None` for an environment that names none.
///
/// The name is the engine's own (`nvim` on unix, `nvim-data` on Windows),
/// read from the crate that owns the spawn rather than restated here.
pub(crate) fn engine_swap_dir(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    env.iter()
        .find(|(key, _)| key == STATE_HOME_VAR)
        .map(|(_, value)| {
            PathBuf::from(value)
                .join(view_oracle::engine_state_dir_name())
                .join("swap")
        })
}

/// Removes every file `dir` holds, best effort, leaving the directory
/// itself and everything beside it in place.
///
/// A sample killed at its first painted frame runs no exit path, so the
/// swap file its engine opened outlives it, and the next spawn into the
/// same state home finds it: one stale swap is answered by view's own
/// recovery autocommand, two or more put nvim's "Enter number of swap file
/// to use" on screen, where a harness with nothing to type parks until the
/// wait gives up. Observed on the `startup` row, whose fourth cold spawn
/// was still at that prompt half a minute later. Emptying rather than
/// removing the
/// root: the state home is also where view keeps the first-run record and
/// theme cache a warm side is measured with.
pub(crate) fn empty_swap_dir(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// The environment entries no spawn may inherit from the spawn before it,
/// each for the reason its own constant records.
const PER_SPAWN_VARS: [&str; 1] = [PROBE_SOCKET_VAR];

/// Distinguishes the per-spawn paths of each spawn from the last one's.
static SPAWN_SERIAL: AtomicU64 = AtomicU64::new(0);

/// Returns `env` with every [`PER_SPAWN_VARS`] path made unique to
/// `serial`, leaving every other entry untouched and adding nothing for a
/// variable that is absent.
///
/// Suffixing rather than unlinking what the last spawn left bound: an
/// address the next spawn cannot collide with does not depend on the
/// unlink having happened, and the address is the harness's to choose --
/// unlike the state home, which is the side's and carries state the
/// measurement wants kept.
fn per_spawn_env(env: &[(OsString, OsString)], serial: u64) -> Vec<(OsString, OsString)> {
    env.iter()
        .map(|(key, value)| {
            if PER_SPAWN_VARS.iter().any(|var| key == *var) {
                let mut unique = value.clone();
                unique.push(format!(".{serial}"));
                (key.clone(), unique)
            } else {
                (key.clone(), value.clone())
            }
        })
        .collect()
}

impl BenchSession {
    /// Spawns `spec` inside a [`GRID_COLS`]x[`GRID_ROWS`] pty.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Session`] if the pty cannot be opened or the
    /// command fails to spawn.
    pub fn spawn(spec: &SpawnSpec) -> Result<Self, BenchError> {
        let mut cmd = CommandBuilder::new(&spec.program);
        for arg in &spec.args {
            cmd.arg(arg);
        }
        let serial = SPAWN_SERIAL.fetch_add(1, Ordering::Relaxed);
        for (key, value) in per_spawn_env(&spec.env, serial) {
            cmd.env(key, value);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }
        // a measurement must face the terminal its budget row names. Answering
        // only the DA1 fence resolves neither synchronized output nor the
        // kitty keyboard protocol, so the child derives its most conservative
        // tier and every frame skips the sync bracket the stated tier pays
        // for -- less work than the bar was written against.
        let pty = PtySession::spawn_configured_with(
            cmd,
            GRID_COLS,
            GRID_ROWS,
            QueryPolicy::AnswerFullTier,
        )?;
        Ok(Self {
            pty,
            swap_dir: engine_swap_dir(&spec.env),
        })
    }

    /// Writes `bytes` to the pty as if typed.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Session`] if the write fails.
    pub fn send(&mut self, bytes: &[u8]) -> Result<(), BenchError> {
        self.pty.send(bytes).map_err(Into::into)
    }

    /// Blocks until the screen content has stayed unchanged for
    /// `bound.quiet` (checked by whole-screen cell hash), returning `false`
    /// if that never happens within `bound.deadline`. The settle gate
    /// before any sampling starts: startup traffic (plugin manager output,
    /// theme paints) must never be mistaken for a response to a sample
    /// input.
    pub fn settle(&mut self, bound: SettleBound) -> bool {
        let SettleBound { quiet, deadline } = bound;
        let overall = Instant::now() + deadline;
        let mut last_hash = self.pty.with_screen(boundaries::screen_hash);
        let mut quiet_since = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(25));
            let hash = self.pty.with_screen(boundaries::screen_hash);
            let now = Instant::now();
            if hash == last_hash {
                if now.duration_since(quiet_since) >= quiet {
                    return true;
                }
            } else {
                last_hash = hash;
                quiet_since = now;
            }
            if now >= overall {
                return false;
            }
        }
    }

    /// Tight-polls (yielding, not sleeping or spinning) until the cell at
    /// `(row, col)` holds exactly `expected`, returning whether it did
    /// within `timeout`. The sampling wait: sub-millisecond latencies sit
    /// far below the OS sleep granularity, so a sleeping poll would inject
    /// its own interval into every sample; a spinning poll on a busy host
    /// starves the measured child of scheduler time and biases ratios.
    #[must_use]
    pub fn wait_cell(
        &mut self,
        at: boundaries::CellPos,
        expected: &str,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let hit = self
                .pty
                .with_screen(|screen| boundaries::cell_holds(screen, at, expected));
            if hit {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
    }

    /// Number of cells currently holding `target`, draining pending output
    /// first.
    #[must_use]
    pub fn count_char_cells(&mut self, target: &str) -> usize {
        self.pty
            .with_screen(|screen| boundaries::count_char_cells(screen, target))
    }

    /// Runs `f` against the current screen after draining pending output.
    pub fn with_screen<R>(&mut self, f: impl FnOnce(&vt100::Screen) -> R) -> R {
        self.pty.with_screen(f)
    }

    /// The whole screen's text, for error context when a run desyncs.
    #[must_use]
    pub fn screen_text(&mut self) -> String {
        self.pty.screen()
    }

    /// The child's OS pid, if the platform exposes one.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pty.pid()
    }

    /// Best-effort shutdown: ask for a quit, then let the bounded exit
    /// wait kill and reap the child if it does not comply. A hung
    /// measurement target must never hang the harness.
    pub fn shutdown(&mut self) {
        let _ = self.pty.send(b"\x1b:silent qa!\r");
        if self.pty.wait_for_exit(Duration::from_secs(2)).is_none() {
            self.pty.kill();
            let _ = self.pty.wait_for_exit(Duration::from_secs(2));
        }
    }
}

impl Drop for BenchSession {
    fn drop(&mut self) {
        self.pty.kill();
        let _ = self.pty.wait_for_exit(Duration::from_secs(2));
        // after the reap, so the file the child is still writing is not the
        // one removed
        if let Some(dir) = &self.swap_dir {
            empty_swap_dir(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(*k), OsString::from(*v)))
            .collect()
    }

    #[test]
    fn two_spawns_never_share_a_probe_socket_address() {
        let env = env_of(&[(PROBE_SOCKET_VAR, "/scratch/nvim/compat.sock")]);
        let first = per_spawn_env(&env, 0);
        let second = per_spawn_env(&env, 1);
        assert_ne!(
            first[0].1, second[0].1,
            "a killed sample leaves its socket bound, so a shared address is a spawn the next \
             sample cannot make"
        );
    }

    /// The claim [`PER_SPAWN_VARS`] itself makes, walked rather than
    /// spelled once per entry: an entry added to the set is pinned by this
    /// test the moment it joins, and a rewriter that stopped covering one
    /// fails naming it.
    #[test]
    fn every_per_spawn_entry_differs_between_two_spawns() {
        for var in PER_SPAWN_VARS {
            let env = env_of(&[(var, "/scratch/nvim/per-spawn")]);
            let first = per_spawn_env(&env, 0);
            let second = per_spawn_env(&env, 1);
            assert_ne!(
                first[0].1, second[0].1,
                "{var} survives a killed sample, so a value shared with the next spawn is a \
                 leftover that spawn cannot get past"
            );
        }
    }

    /// The state home is the side's, not the spawn's: view reads its
    /// first-run record and theme cache from it, so a value made unique
    /// per spawn measures a first run on every sample of the view arm and
    /// nothing at all on the nvim arm it is paired against.
    #[test]
    fn the_state_home_reaches_every_spawn_of_a_side_unchanged() {
        let env = env_of(&[(STATE_HOME_VAR, "/scratch/nvim/xdg_state_home")]);
        assert_eq!(per_spawn_env(&env, 0), env);
        assert_eq!(per_spawn_env(&env, 1), env);
    }

    #[test]
    fn the_swap_directory_is_the_engines_own_under_the_state_home() {
        let env = env_of(&[(STATE_HOME_VAR, "/scratch/nvim/xdg_state_home")]);
        assert_eq!(
            engine_swap_dir(&env),
            Some(
                PathBuf::from("/scratch/nvim/xdg_state_home")
                    .join(view_oracle::engine_state_dir_name())
                    .join("swap")
            )
        );
        assert_eq!(
            engine_swap_dir(&env_of(&[("TERM", "xterm-256color")])),
            None
        );
    }

    /// The half of the cleanup that keeps the two arms comparable: what a
    /// killed sample left goes, and the state a warm side is measured with
    /// stays.
    #[test]
    fn emptying_the_swap_directory_leaves_views_own_state_beside_it() {
        let dir = view_test_support::ScratchDir::new("bench-swap-empty").unwrap();
        let state_home = dir.join("xdg_state_home");
        let swap = state_home
            .join(view_oracle::engine_state_dir_name())
            .join("swap");
        std::fs::create_dir_all(&swap).unwrap();
        std::fs::write(swap.join("scratch.txt.swp"), b"stale").unwrap();
        let first_run = state_home.join("view").join("first-run.toml");
        std::fs::create_dir_all(state_home.join("view")).unwrap();
        std::fs::write(&first_run, b"seen").unwrap();

        let env = env_of(&[(STATE_HOME_VAR, &state_home.to_string_lossy())]);
        empty_swap_dir(&engine_swap_dir(&env).unwrap());

        assert!(
            swap.is_dir(),
            "the swap directory itself is the next spawn's to write into, and removing the \
             root takes view's own state with it"
        );
        assert_eq!(
            std::fs::read_dir(&swap).unwrap().count(),
            0,
            "a swap the next spawn is offered to recover is the prompt this cleanup exists for"
        );
        assert!(
            first_run.exists(),
            "view's own state lives under the same root; removing it makes every sample a \
             first run while the nvim arm pays nothing"
        );
    }

    #[test]
    fn every_other_environment_entry_survives_untouched() {
        let env = env_of(&[
            ("XDG_CONFIG_HOME", "/scratch/nvim/xdg_config_home"),
            (PROBE_SOCKET_VAR, "/scratch/nvim/compat.sock"),
            ("TERM", "xterm-256color"),
        ]);
        let rewritten = per_spawn_env(&env, 7);
        assert_eq!(rewritten.len(), env.len());
        assert_eq!(rewritten[0], env[0]);
        assert_eq!(rewritten[2], env[2]);
        assert_eq!(
            rewritten[1].1,
            OsString::from("/scratch/nvim/compat.sock.7")
        );
    }

    #[test]
    fn an_environment_without_a_probe_socket_is_unchanged() {
        let env = env_of(&[("TERM", "xterm-256color")]);
        assert_eq!(per_spawn_env(&env, 3), env);
    }

    /// The one platform where the best-effort cleanup can leave a swap:
    /// `remove_file` on a still-open handle succeeds on unix (`tests/
    /// swap_hygiene.rs` covers that leg through a live editor) and is
    /// refused on Windows, which is the leftover a child outliving
    /// `BenchSession::drop`'s 2s wait can still hand the next spawn.
    /// `empty_swap_dir` is crate-private, so a Windows pin against a real
    /// hold has to live in this module rather than in that integration
    /// test.
    #[cfg(windows)]
    #[test]
    fn a_held_open_swap_file_survives_the_best_effort_cleanup() {
        let dir = view_test_support::ScratchDir::new("bench-swap-windows-held").unwrap();
        let held = dir.join("scratch.txt.swp");
        let file = std::fs::File::create(&held).unwrap();
        empty_swap_dir(&dir);
        assert!(
            held.exists(),
            "an open file refuses Windows' best-effort remove_file, which is the leftover \
             this cleanup shrinks but cannot always prevent"
        );
        drop(file);
    }
}
