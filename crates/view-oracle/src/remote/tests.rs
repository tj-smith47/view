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

use std::ffi::{OsStr, OsString};

use view_engine::env::HERMETIC_HOME_VAR;
use view_engine::process::EngineConfig;

use super::{
    run_case, stub_available, stub_client, stub_config, RemoteCase, RemoteReport, COLS, ROWS,
};
use crate::EngineSession;

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

/// The rest of that same contract: every host variable the local route's
/// plan drops is dropped on this route too.
///
/// The stand-in execs a shell as a child of this process, so its far side
/// inherits this process's environment -- which a plan built from named
/// constants alone leaves almost entirely intact, while the local reference
/// it is compared against has been swept clean. A developer with
/// `LD_PRELOAD` or `LD_LIBRARY_PATH` exported would then be running two
/// differently-linked editors against each other and reading the result as
/// a transport fault.
///
/// The sweep is asserted name by name against the same call the local plan
/// is built from, so the two cannot drift: whatever this host exports is the
/// witness, and a host exporting nothing outside the allowlist would leave
/// nothing to check, which the emptiness guard refuses rather than passes.
#[test]
fn the_stand_in_route_sweeps_every_host_variable_the_local_route_sweeps() {
    let swept = view_engine::env::hermetic_sweep();
    assert!(
        swept.len() > 1,
        "this host exports nothing outside the hermetic allowlist, so there is \
         no variable here for either route to drop and this proves nothing"
    );
    let plan = stub_config()
        .expect("the stub client and a preparable hermetic home")
        .env_plan();
    for (name, host_value) in &swept {
        if view_engine::env::env_names_eq(name, OsStr::new(HERMETIC_HOME_VAR)) {
            continue;
        }
        // removed, or overridden with something that is not the host's: the
        // two search-path variables are swept by name locally and replaced
        // by a neutralizer rather than dropped, on both routes
        let neutralized = plan
            .iter()
            .find(|(planned, _)| view_engine::env::env_names_eq(planned, name))
            .is_some_and(|(_, value)| value.as_ref() != Some(host_value));
        assert!(
            neutralized,
            "{name:?} is swept out of a local hermetic child but reaches the \
             stand-in route's far side with this host's own value, and that \
             far side inherits this process's environment; plan {plan:?}"
        );
    }
}

/// The same claim read out of two started editors rather than off a plan:
/// a variable this host exports reaches neither leg of the comparison.
///
/// The plan assertion above is about the config; this is about what a shell
/// on the far side actually did with it, which is the layer the whole
/// stand-in exists to exercise. The witness is taken from the sweep rather
/// than planted, because planting means mutating this process's environment
/// while sibling tests read it.
#[test]
fn a_host_variable_reaches_neither_side_of_the_stand_in_comparison() {
    let witness = view_engine::env::hermetic_sweep()
        .into_iter()
        .filter_map(|(name, value)| name.into_string().ok().map(|name| (name, value)))
        .find(|(name, value)| {
            !value.is_empty()
                && !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !opens_with_a_digit(name)
        })
        .map(|(name, _)| name)
        .expect("this host exports at least one swept variable a shell can name");

    let probe = format!("string(getenv('{witness}'))");
    let mut remote = EngineSession::spawn_configured(
        stub_config().expect("the stub client and a preparable hermetic home"),
        COLS,
        ROWS,
    )
    .expect("a stand-in route session must start");
    let mut local = EngineSession::spawn_configured(EngineConfig::isolated(), COLS, ROWS)
        .expect("a local session must start");

    for (side, session) in [("stand-in route", &mut remote), ("local", &mut local)] {
        assert_eq!(
            session.eval_str(&probe).unwrap(),
            "v:null",
            "{witness} reached the {side} editor: the two sides of every \
             comparison must be swept by the same rule, or a host variable \
             decides one of them and the report line blames the transport"
        );
    }
}

/// Whether a name opens with a digit, which no environment variable a shell
/// can read back does.
fn opens_with_a_digit(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_digit())
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
