//! The `remote_memory` bench row: view's own process footprint with the
//! engine reached over `--remote`. Split from bench.rs/rows.rs because the
//! stub-arming machinery this row needs
//! ([`view_bench::scenarios::remote_memory`]) is a unix mechanism -- the
//! same reason `taps_rows.rs` is split out -- and bench.rs sits at its own
//! god-file ceiling with no headroom for the extra code inline.

use super::*;
use view_bench::scenarios::remote_memory;

/// Runs the `remote_memory` row against `world`'s hermetic `view` side and
/// returns the metrics [`rows::run_cell`] records. Same workload-planting
/// and reporting shape as the `memory` row in bench.rs, because the two
/// differ only in which spawn spec they measure through.
pub(super) fn run_remote_memory_row(
    world: &CellWorld,
    fixture: &str,
    bins: &Bins,
    scenario: &str,
    protocol: &Protocol,
) -> Result<CellMetrics> {
    let side = world.side(fixture, "view")?;
    for (index, name) in memory::workload_files().iter().enumerate() {
        std::fs::write(side.cwd.join(name), memory::workload_content(index + 1))
            .with_context(|| format!("writing workload buffer {name}"))?;
    }
    let view_spec = remote_memory_spec_from(side, bins.view_bins())
        .with_context(|| format!("arming the remote_memory/{fixture} spawn"))?;
    let outcome = remote_memory::run(ViewSpec(&view_spec), protocol)
        .with_context(|| format!("remote_memory/{fixture} run failed"))?;
    println!(
        "{}",
        report::absolute_cell(
            scenario,
            fixture,
            outcome.metric,
            report::AbsoluteStats {
                p50: outcome.distribution.p50(),
                p99: outcome.gated_mb,
                max: outcome.distribution.max(),
                unit: "MB",
                samples: outcome.distribution.len(),
                warmup: protocol.warmup,
            }
        )
    );
    println!(
        "{}",
        report::aggregate_line(outcome.metric, outcome.gated_mb, 1)
    );
    let mut metrics = CellMetrics::new();
    metrics.insert(outcome.metric.to_string(), outcome.gated_mb);
    Ok(metrics)
}

/// Builds the view-side spawn spec for the row above: `--remote` armed
/// against the committed stub-ssh double, unconditionally. The real-SSH leg
/// lives exclusively in `remote_memory`'s own `#[ignore]`d test -- this
/// function has no branch that could read
/// [`remote_memory::REMOTE_HOST_ENV`] at all, by construction rather than
/// by an early-return that happens not to fire today. A leftover export
/// from an oracle real-SSH run reaching this row would measure a real
/// network hop and let it silently ratchet into the dev-linux bar under a
/// route no log line names, which is why the gated path is stub-only with
/// no env branch to leave in.
///
/// `--nvim-bin` points at the pinned local binary, because the stub's far
/// side is this host (see `view_oracle::remote`'s module doc): the same
/// absolute path the local `memory` row resolves works unchanged there.
///
/// The scratch-file positional is the same one [`super::view_spec_from`]
/// opens for the paired `memory` row -- deliberately, so the two rows are a
/// minimal pair differing only in transport: everything a paired comparison
/// (spec 3.1's headroom claim) could otherwise attribute to "one row opens
/// a file and the other does not" is closed off. It must come last: view's
/// CLI forwards every token after the first positional to nvim verbatim
/// (`trailing_var_arg`), so a flag placed after it would never reach view's
/// own parser, and `--nvim-bin`/`--remote` must each keep their value as
/// the immediately following token for the same reason.
fn remote_memory_spec_from(side: SideSetup, bins: EditorBins<'_>) -> Result<SpawnSpec> {
    let mut env = side.env;
    let stub_dir = side.cwd.join("stub-ssh-path");
    let path = remote_memory::arm_stub_ssh_path(&stub_dir, std::env::var_os("PATH").as_deref())
        .with_context(|| format!("arming the stub ssh double in {}", stub_dir.display()))?;
    env.push((OsString::from("PATH"), path));
    let args = vec![
        OsString::from("--nvim-bin"),
        bins.nvim.as_os_str().to_os_string(),
        OsString::from("--remote"),
        OsString::from(remote_memory::STUB_TARGET),
        side.scratch_file.clone().into_os_string(),
    ];
    Ok(SpawnSpec {
        program: bins.view.to_path_buf(),
        args,
        env,
        cwd: Some(side.cwd),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]
    use super::*;

    /// Serializes this module's `std::env::set_var`/`remove_var` calls
    /// against each other: `cargo test` runs a module's tests on multiple
    /// threads by default, and a second test added here that touches the
    /// same name could otherwise interleave its plant with this one's
    /// restore. One test uses it today; the guard exists so a second test
    /// does not have to discover the hazard by flaking.
    static ENV_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_mutation_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTATION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// An exported `VIEW_REMOTE_TEST_HOST` -- a plausible leftover from
    /// running the oracle's real-SSH leg on the same shell -- must never
    /// reach the gated spec's target. This test sets the var itself, so a
    /// regression that re-adds an env read to the gated path fails it
    /// directly rather than depending on a reviewer noticing the diff.
    #[test]
    fn the_gated_spec_stays_on_the_stub_target_even_with_the_real_host_var_exported() {
        if !view_oracle::remote::stub_available() {
            eprintln!("skipped: no stub ssh client on this host");
            return;
        }
        let _guard = env_mutation_guard();
        let scratch = view_test_support::ScratchDir::new("remote-memory-gated-path-ignores-env")
            .expect("creating the scratch dir");
        let prior = std::env::var_os(remote_memory::REMOTE_HOST_ENV);
        std::env::set_var(
            remote_memory::REMOTE_HOST_ENV,
            "a-leftover-export-from-t12-testing",
        );
        let side = SideSetup {
            env: Vec::new(),
            cwd: scratch.to_path_buf(),
            scratch_file: scratch.join("scratch.txt"),
        };
        let bins = EditorBins {
            view: Path::new("/bin/true"),
            nvim: Path::new("/bin/true"),
        };
        let result = remote_memory_spec_from(side, bins);
        match prior {
            Some(value) => std::env::set_var(remote_memory::REMOTE_HOST_ENV, value),
            None => std::env::remove_var(remote_memory::REMOTE_HOST_ENV),
        }
        let spec = result.expect("stub arming must still succeed with an unrelated var exported");
        // args end `["--remote", <target>, <scratch positional>]`
        assert_eq!(
            spec.args.get(spec.args.len() - 3).map(OsString::as_os_str),
            Some(std::ffi::OsStr::new("--remote")),
            "the flag two tokens before the trailing positional must still be --remote"
        );
        assert_eq!(
            spec.args.get(spec.args.len() - 2).map(OsString::as_os_str),
            Some(std::ffi::OsStr::new(remote_memory::STUB_TARGET)),
            "an exported VIEW_REMOTE_TEST_HOST must never reach the gated spec's target"
        );
    }
}
