//! Scaffolding shared by this crate's integration test binaries: asking the
//! OS whether a pid still has a process-table entry, which is how every
//! "the child was reaped, not merely killed" assertion here is proved,
//! whether it is still executing ([`pid_running`]), which is the separate
//! question a crash simulation asks, and the wall clock a live engine gets
//! to answer ([`rpc_deadline`]).
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

/// Whether `pid` is still executing.
///
/// A different question from [`pid_in_process_table`], and the one a crash
/// simulation asks: a child that has been killed and not yet waited for is
/// already dead, with only its exit status left to collect, and waiting for
/// its entry to go is waiting on whoever owns that collection rather than
/// on the crash. Which of the two a killed process is depends on whose
/// child it turns out to be -- the stand-in ssh client collapses into the
/// editor it starts wherever `/bin/sh` runs a single command without
/// forking, and then the process a remote test kills is the test's own
/// child and nothing waits for it until the restart.
#[must_use]
pub fn pid_running(pid: u32) -> bool {
    view_test_support::pid_running(pid)
}

/// Whether the OS still holds a process-table entry for `pid`.
///
/// The shared probe under its name here, because this module's callers read
/// it beside [`pid_running`] and the pair is what those cases are about.
#[must_use]
pub fn pid_in_process_table(pid: u32) -> bool {
    view_test_support::pid_in_process_table(pid)
}
