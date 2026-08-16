//! The `oracle remote` runner: drives `view_oracle::remote`'s battery
//! against the committed stand-in ssh client and a real pinned engine, and
//! prints one report line each.
//!
//! A runner rather than a second set of assertions. The stand-in, the cases,
//! the comparison and the pass/fail reading of a report all live in
//! `view-oracle`; what is here is the selection and the loop, so a manual
//! reproduction reports the same verdict a gated test asserts.

use anyhow::{bail, Result};
use view_oracle::remote::{self, RemoteCase};

/// Resolves `name` to the cases to run, or every case when `None`.
///
/// # Errors
///
/// Returns an error naming what is available if `name` matches nothing: a
/// misspelled case must not read as a clean run of zero cases.
pub fn select(name: Option<&str>) -> Result<Vec<RemoteCase>> {
    let Some(name) = name else {
        return Ok(RemoteCase::all().to_vec());
    };
    let found: Vec<RemoteCase> = RemoteCase::all()
        .into_iter()
        .filter(|case| case.label() == name)
        .collect();
    if found.is_empty() {
        bail!(
            "unknown case {name:?}; expected one of {}",
            RemoteCase::all().map(RemoteCase::label).join(", ")
        );
    }
    Ok(found)
}

/// Runs `case` by name, or every case when `None`, printing one report line
/// each plus every divergence a failing case found. Returns whether every
/// one of them succeeded.
///
/// A host with no stand-in client to run (see
/// [`view_oracle::remote::stub_available`]) reports the skip on its own line
/// and returns success: the remote path is a POSIX-shell claim, and a host
/// that cannot make it must say so rather than fail a run for a leg it was
/// never able to drive.
///
/// # Errors
///
/// Returns an error only for an unknown case name. A case that fails to run
/// at all is reported on its own line and folded into the returned verdict,
/// so one broken case does not hide the others.
pub fn run(case: Option<&str>) -> Result<bool> {
    let selected = select(case)?;
    if !remote::stub_available() {
        println!(
            "oracle: remote ... SKIPPED (no POSIX stand-in client at {})",
            remote::stub_client().display()
        );
        return Ok(true);
    }
    let mut all_ok = true;
    for case in selected {
        match remote::run_case(case) {
            Ok(report) => {
                println!("{}", report.report_line());
                for divergence in &report.divergences {
                    println!("  {divergence:?}");
                }
                all_ok &= report.is_success();
            }
            Err(err) => {
                println!("oracle: remote {} ... ERROR: {err}", case.label());
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
    fn a_bare_run_selects_every_case_with_the_stubs_own_fidelity_first() {
        let selected = select(None).unwrap();
        assert_eq!(selected, RemoteCase::all().to_vec());
        assert_eq!(selected.first(), Some(&RemoteCase::StubFlattening));
    }

    #[test]
    fn a_named_case_selects_only_itself() {
        assert_eq!(
            select(Some("parentless-open")).unwrap(),
            vec![RemoteCase::ParentlessOpen]
        );
    }

    #[test]
    fn an_unknown_case_name_is_an_error_rather_than_an_empty_run() {
        let err = select(Some("no-such-case")).unwrap_err();
        assert!(
            err.to_string().contains("stub-flattening"),
            "the error must name what is available: {err}"
        );
    }
}
