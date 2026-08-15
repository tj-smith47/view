//! The `oracle speculate` runner: drives `view_oracle::speculate`'s battery
//! against a real pinned engine and prints one report line each.
//!
//! A runner rather than a second set of assertions. The schedules, the
//! bounds, the diff against nvim's own screen and the pass/fail reading of a
//! report all live in `view-oracle`; what is here is the selection and the
//! loop, so a manual reproduction reports the same verdict a gated test
//! asserts instead of a wall of numbers to read by eye.

use anyhow::{bail, Result};
use view_oracle::speculate::{self, SpecCase};

/// Resolves `name` to the cases to run, or every case when `None`.
///
/// # Errors
///
/// Returns an error naming what is available if `name` matches nothing: a
/// misspelled case must not read as a clean run of zero cases.
pub fn select(name: Option<&str>) -> Result<Vec<SpecCase>> {
    let Some(name) = name else {
        return Ok(SpecCase::all().to_vec());
    };
    let found: Vec<SpecCase> = SpecCase::all()
        .into_iter()
        .filter(|case| case.label() == name)
        .collect();
    if found.is_empty() {
        bail!(
            "unknown case {name:?}; expected one of {}",
            SpecCase::all().map(SpecCase::label).join(", ")
        );
    }
    Ok(found)
}

/// Runs `case` by name, or every case when `None`, printing one report line
/// each plus every divergence a failing case found. Returns whether every one
/// of them succeeded.
///
/// # Errors
///
/// Returns an error only for an unknown case name. A case that fails to run
/// at all is reported on its own line and folded into the returned verdict,
/// so one broken case does not hide the others.
pub fn run(case: Option<&str>) -> Result<bool> {
    let mut all_ok = true;
    for case in select(case)? {
        match speculate::run_case(case) {
            Ok(report) => {
                println!("{}", report.report_line());
                for divergence in &report.divergences {
                    println!("  {divergence:?}");
                }
                all_ok &= report.is_success();
            }
            Err(err) => {
                println!("oracle: speculate {} ... ERROR: {err}", case.label());
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
    fn a_bare_run_selects_every_case_with_the_burst_first() {
        let selected = select(None).unwrap();
        assert_eq!(selected, SpecCase::all().to_vec());
        assert_eq!(selected.first(), Some(&SpecCase::BurstTail));
    }

    #[test]
    fn a_named_case_selects_only_itself() {
        assert_eq!(
            select(Some("mispredict")).unwrap(),
            vec![SpecCase::Mispredict]
        );
    }

    #[test]
    fn an_unknown_case_name_is_an_error_rather_than_an_empty_run() {
        let err = select(Some("no-such-case")).unwrap_err();
        assert!(
            err.to_string().contains("burst-tail"),
            "the error must name what is available: {err}"
        );
    }
}
