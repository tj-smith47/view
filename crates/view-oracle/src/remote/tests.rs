//! The falsifiable half of the remote battery.
//!
//! Every case runs against the committed stand-in client and a real pinned
//! engine, and asserts on the report it produced rather than on output read
//! by eye: the pass/fail reading is [`RemoteReport::is_success`], the same
//! one a manual `oracle remote` run prints, so a case cannot pass here and
//! fail there.
//!
//! Unix only, for the reason [`stub_available`] states: the stand-in is a
//! POSIX shell script and there is no shell on the other hosts to re-parse
//! what a client joined.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsString;

use view_engine::env::HERMETIC_HOME_VAR;
use view_engine::process::EngineConfig;

use super::{run_case, stub_available, stub_client, stub_config, RemoteCase, RemoteReport};

/// Runs `case` and returns its report, failing with the report's own line
/// and every divergence it carries rather than a bare boolean.
fn report(case: RemoteCase) -> RemoteReport {
    let report = run_case(case).expect("the stub client and a pinned nvim must drive the case");
    assert!(
        report.is_success(),
        "{}\n  divergences: {:#?}",
        report.report_line(),
        report.divergences,
    );
    report
}

/// The stand-in has to exist and be runnable before anything asserts
/// through it. A missing or non-executable fixture would otherwise turn
/// every case below into a skipped one wearing the shape of a passing one.
#[test]
fn the_stand_in_client_is_a_committed_executable_on_this_host() {
    assert!(
        stub_available(),
        "{} is not an executable committed fixture; every remote case is \
         unrunnable without it",
        stub_client().display()
    );
}

/// The double's own fidelity, proven before anything is asserted through
/// it: a client that preserved its caller's argument boundaries would be
/// strictly more forgiving than the one it stands for, and would pass a
/// caller whose quoting a real remote shell would break.
#[test]
fn the_stand_in_client_flattens_its_trailing_arguments_as_the_real_one_does() {
    report(RemoteCase::StubFlattening);
}

/// The nonexistent-parent path, remote against local. The remote spawn path
/// resolves nothing locally and carries no error handling of its own, and
/// this is what says so: the same error text, the same state, the same
/// screen on both paths.
#[test]
fn a_nonexistent_parent_directory_fails_identically_on_both_paths() {
    report(RemoteCase::ParentlessOpen);
}

/// The stand-in route's own contract: a session on it differs from a local
/// one in its transport and in nothing else, which for `HOME` means the same
/// prepared hermetic directory on both.
///
/// Read off the plans rather than off a running child so the two are
/// compared by construction: an isolated remote config takes the exemption
/// `EngineConfig::env_plan` documents, and a route that silently stopped
/// undoing it would leave the engine side of every comparison under the
/// invoking account's real home while the reference side stayed hermetic --
/// visible only once some entry probed a home-shaped thing.
#[test]
fn the_stand_in_route_hands_its_far_side_the_same_home_the_local_route_gets() {
    let local = EngineConfig::isolated().env_plan();
    let expected = local
        .iter()
        .find(|(name, _)| name == OsString::from(HERMETIC_HOME_VAR).as_os_str())
        .map(|(_, value)| value.clone())
        .expect("a local hermetic plan points HOME at the prepared home");
    let remote = stub_config()
        .expect("the stub client and a preparable hermetic home")
        .env_plan();
    assert_eq!(
        remote
            .iter()
            .find(|(name, _)| name == OsString::from(HERMETIC_HOME_VAR).as_os_str())
            .map(|(_, value)| value.clone()),
        Some(expected),
        "the stand-in route's far side no longer gets the hermetic home; plan \
         {remote:?}"
    );
}

/// A case's label is its selector on the runner, and every case must be
/// reachable by one: a label collision would leave one of them unrunnable
/// by name with nothing reporting it.
#[test]
fn every_case_has_a_distinct_label() {
    let mut labels: Vec<&str> = RemoteCase::all().iter().map(|c| c.label()).collect();
    let before = labels.len();
    labels.sort_unstable();
    labels.dedup();
    assert_eq!(labels.len(), before, "two cases share a label: {labels:?}");
}
