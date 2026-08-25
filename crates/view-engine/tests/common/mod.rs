//! Scaffolding shared by this crate's integration test binaries: asking the
//! OS whether a pid still has a process-table entry, which is how every
//! "the child was reaped, not merely killed" assertion here is proved, and
//! the wall clock a live engine gets to answer ([`rpc_deadline`]).
//!
//! Compiled separately into each of those binaries, so a helper only one of
//! them needs reads as dead code in the other: `dead_code` is allowed here
//! for that reason alone, never because an unused helper is acceptable.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::time::{Duration, Instant};

use view_engine::process::EngineConfig;

/// How long a live engine in these tests gets to answer one RPC round trip.
///
/// Derived rather than picked: the base is the engine's own default
/// handshake timeout, which is production's bound on exactly this -- one
/// request written to a real nvim and its answer read back -- and
/// `view_test_support::HostBudget` widens it by the contention the run
/// started under. A hand-picked five seconds is the same number on an idle
/// laptop and on a machine running three other gates, and on the second one
/// it fails as `Timeout { method, timeout }` while saying nothing at all
/// about the engine.
#[must_use]
pub fn rpc_deadline() -> Duration {
    rpc_deadline_for(1)
}

/// [`rpc_deadline`] for a wait that covers `round_trips` of them: a poll
/// loop that has to see a restart land, a redraw batch arrive and the state
/// probe that reads it back is bounded by what those trips cost, not by a
/// separate guess.
#[must_use]
pub fn rpc_deadline_for(round_trips: u32) -> Duration {
    view_test_support::host_deadline(EngineConfig::default().handshake_timeout * round_trips)
}

/// The instant a poll loop waiting on a live engine must give up at.
#[must_use]
pub fn rpc_poll_deadline() -> Instant {
    Instant::now() + rpc_deadline()
}

/// [`rpc_poll_deadline`] for a wait covering `round_trips` round trips.
#[must_use]
pub fn rpc_poll_deadline_for(round_trips: u32) -> Instant {
    Instant::now() + rpc_deadline_for(round_trips)
}

/// Whether the OS still holds a process-table entry for `pid`.
///
/// The distinction a reaping assertion needs: a killed-and-reaped child
/// leaves no entry at all, while a killed-but-never-waited one lingers
/// (a zombie on unix, a handle nothing closed on Windows) until something
/// reaps it, so an entry still being there says `kill` ran without `wait`.
///
/// Deliberately not `kill -0`: that is unix-only, and gating a test on it
/// takes the reaping proof away from exactly the platform whose process
/// lifetime rules differ most.
#[must_use]
pub fn pid_in_process_table(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(target_os = "macos")]
    {
        // a `ps` that could not run must not report "no entry": that reads
        // as a successfully reaped child and turns a broken probe into a
        // silent pass. Only an empty listing from a `ps` that did run is
        // the real negative (`ps` also exits nonzero for an unknown pid, so
        // its status is not the signal)
        let listing = std::process::Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .expect("/bin/ps must run for the process table to be observable at all");
        !listing.stdout.is_empty()
    }
    #[cfg(windows)]
    {
        let listing = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .expect("tasklist must run for the process table to be observable at all");
        // the no-match case is an INFO line on stdout rather than a nonzero
        // exit, so the pid's own presence is the only usable signal
        String::from_utf8_lossy(&listing.stdout).contains(&format!("\"{pid}\""))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        // reports no entry where there is neither `/proc`, a POSIX `ps`, nor
        // `tasklist`, leaving the assertion inert rather than failing on the
        // absence of a way to look
        let _ = pid;
        false
    }
}
