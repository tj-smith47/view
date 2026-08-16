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

use super::{run_case, stub_available, stub_client, RemoteCase, RemoteReport};

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
