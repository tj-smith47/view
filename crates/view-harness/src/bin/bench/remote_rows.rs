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
/// against the committed stub-ssh double, unless
/// [`remote_memory::REMOTE_HOST_ENV`] names a real target for the opt-in
/// acceptance leg -- the same env var
/// `crates/view-oracle/tests/remote_real_ssh.rs` reads, so one export
/// configures both legs.
///
/// The stub leg points `--nvim-bin` at the pinned local binary, because the
/// stub's far side is this host (see `view_oracle::remote`'s module doc):
/// the same absolute path the local `memory` row resolves works unchanged
/// there. The real-host leg instead defers to the far side's own `PATH`
/// (or [`remote_memory::REMOTE_NVIM_ENV`] when that `PATH` does not carry
/// it), mirroring `remote_real_ssh.rs`'s own `spec()`: a real remote
/// account's `PATH` is not this host's, so a local absolute path would
/// name a binary nothing on the far side can run.
fn remote_memory_spec_from(side: SideSetup, bins: EditorBins<'_>) -> Result<SpawnSpec> {
    let mut env = side.env;
    // `--nvim-bin`'s value must directly follow it, and so must
    // `--remote`'s: view's CLI reads a flag's very next token as its
    // value (`allow_hyphen_values` is off for both), so a flag/value pair
    // can never be split apart by another flag landing between them.
    let mut args = Vec::new();
    let target = match std::env::var(remote_memory::REMOTE_HOST_ENV) {
        Ok(host) if !host.is_empty() => {
            if let Ok(nvim) = std::env::var(remote_memory::REMOTE_NVIM_ENV) {
                if !nvim.is_empty() {
                    args.push(OsString::from("--nvim-bin"));
                    args.push(OsString::from(nvim));
                }
            }
            OsString::from(host)
        }
        _ => {
            args.push(OsString::from("--nvim-bin"));
            args.push(bins.nvim.as_os_str().to_os_string());
            let stub_dir = side.cwd.join("stub-ssh-path");
            let path =
                remote_memory::arm_stub_ssh_path(&stub_dir, std::env::var_os("PATH").as_deref())
                    .with_context(|| {
                        format!("arming the stub ssh double in {}", stub_dir.display())
                    })?;
            env.push((OsString::from("PATH"), path));
            OsString::from(remote_memory::STUB_TARGET)
        }
    };
    args.push(OsString::from("--remote"));
    args.push(target);
    Ok(SpawnSpec {
        program: bins.view.to_path_buf(),
        args,
        env,
        cwd: Some(side.cwd),
    })
}
