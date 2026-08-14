//! The `oracle hang` runner: drives `view_oracle::hang`'s reproduced hang
//! schedules against a real pinned engine and prints one report line each.
//!
//! A runner rather than a second set of assertions. The schedules, the
//! bounds, the fold and the pass/fail reading of a report all live in
//! `view-oracle`; what is here is the selection and the loop, so a manual
//! reproduction reports the same verdict a gated test asserts instead of a
//! wall of numbers to read by eye.

use anyhow::{bail, Result};
use view_oracle::hang::{self, HangRun, HangSchedule};

/// Every schedule a bare `oracle hang` runs, control first: a run whose
/// control has stopped holding cannot be read as evidence about the two
/// hangs that follow it.
const SCHEDULES: [HangSchedule; 3] = [
    HangSchedule::BlockedOnKey,
    HangSchedule::ReadSideWedge,
    HangSchedule::DeadConnection,
];

/// Resolves `name` to the schedules to run, or every schedule when `None`.
///
/// # Errors
///
/// Returns an error naming what is available if `name` matches nothing: a
/// misspelled schedule must not read as a clean run of zero schedules.
pub fn select(name: Option<&str>) -> Result<Vec<HangSchedule>> {
    let Some(name) = name else {
        return Ok(SCHEDULES.to_vec());
    };
    let found: Vec<HangSchedule> = SCHEDULES
        .into_iter()
        .filter(|schedule| schedule.label() == name)
        .collect();
    if found.is_empty() {
        bail!(
            "unknown schedule {name:?}; expected one of {}",
            SCHEDULES.map(HangSchedule::label).join(", ")
        );
    }
    Ok(found)
}

/// Runs `schedule` by name, or every schedule when `None`, printing one
/// report line each. Returns whether every one of them succeeded.
///
/// # Errors
///
/// Returns an error only for an unknown schedule name. A schedule that
/// fails to run at all is reported on its own line and folded into the
/// returned verdict, so one broken schedule does not hide the others.
pub fn run(schedule: Option<&str>) -> Result<bool> {
    let mut all_ok = true;
    for schedule in select(schedule)? {
        match hang::run_schedule(HangRun::new(schedule)) {
            Ok(report) => {
                println!("{}", report.report_line());
                all_ok &= report.is_success();
            }
            Err(err) => {
                println!("oracle: hang {} ... ERROR: {err}", schedule.label());
                all_ok = false;
            }
        }
    }
    Ok(all_ok)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_bare_run_selects_every_schedule_with_the_control_first() {
        let selected = select(None).unwrap();
        assert_eq!(selected, SCHEDULES.to_vec());
        assert_eq!(selected.first(), Some(&HangSchedule::BlockedOnKey));
    }

    #[test]
    fn a_named_schedule_selects_only_itself() {
        assert_eq!(
            select(Some("dead-connection")).unwrap(),
            vec![HangSchedule::DeadConnection]
        );
    }

    #[test]
    fn an_unknown_schedule_name_is_an_error_rather_than_an_empty_run() {
        let err = select(Some("no-such-schedule")).unwrap_err();
        assert!(
            err.to_string().contains("read-side-wedge"),
            "the error must name what is available: {err}"
        );
    }
}
