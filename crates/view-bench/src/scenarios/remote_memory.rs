//! The `remote_memory` row: view's own process footprint, read the exact
//! way [`memory::run`] reads it, against a session whose engine is spawned
//! over `--remote` instead of locally -- the resource-claim ladder's fourth
//! rung, spec 3.1's "Remote editing: view's own local footprint once the
//! engine runs remotely". The claim under test is narrow and falsifiable:
//! the engine moving to a remote host must not grow view's *own* process,
//! since view holds no more buffer state remotely than it does locally.
//!
//! # Absolutes are host-regime noise; the ratio is the claim
//!
//! A settled process's PSS/`phys_footprint` reading moves with the host's
//! ambient memory regime (page cache eviction pressure, other tenants'
//! working sets) by as much as +/-20% across days on a shared box, which a
//! within-window headroom sidecar cannot absorb -- a bar sized to one
//! day's regime breaches on the next day's, with nothing about view having
//! changed. The row's actual claim survives that noise: view's own local
//! footprint with a remote engine, divided by its footprint with a local
//! one, drawn from the same interleaved window, stayed inside +0.6-2%
//! across regimes that moved the absolutes +/-20% (2026-08-17 ruling). So
//! [`run_paired`] gates [`RATIO_METRIC`] -- the remote/local ratio -- and
//! records both absolutes for reference, record-only on a shared class the
//! same way a tail statistic already is (see
//! `view_harness::baselines::gate_headroom`).
//!
//! # Two legs, one driver
//!
//! [`run`] is a pass-through to [`memory::run`] -- the same workload
//! ([`memory::workload_files`], [`memory::workload_content`]), the same
//! settle/sample loop, the same own-process reader. It exists for the
//! opt-in real-SSH connectivity test at the bottom of this module, which
//! needs to prove a reading is obtainable at all, not to compare it against
//! anything. The gated row's driver is [`run_paired`], which prepares both
//! a local and a remote session and reads them alternately: the only thing
//! that can differ between either leg is which [`SpawnSpec`] the caller
//! built, which is exactly the property the falsifiable check needs -- two
//! drivers sampling the same workload the same way would drift the moment
//! one changed and the other did not, while one function measured through
//! two specs cannot.
//!
//! The gated CI leg arms that spec against the committed stand-in
//! `ssh` client ([`view_oracle::remote`]) via [`arm_stub_ssh_path`]: a
//! `PATH` entry whose `ssh` resolves to the stub instead of a real client,
//! the same trick view's own `RemoteSpec::ssh_bin` documents as the only
//! lever the `view` binary's `--remote` flag leaves for selecting a
//! double, since the CLI exposes no `--ssh-bin` of its own. Because the
//! stub's far side is this host (`view_oracle::remote`'s own module doc),
//! the pinned local `nvim` binary's absolute path resolves there exactly
//! as it does locally.
//!
//! The opt-in leg reads [`REMOTE_HOST_ENV`] -- the same env var
//! `crates/view-oracle/tests/remote_real_ssh.rs` reads, so one exported
//! name configures both legs' real-SSH coverage -- and is exercised
//! **only** by this module's own `#[ignore]`d test. The gated bench matrix
//! (`crates/view-harness/src/bin/bench/remote_rows.rs`) does not read
//! [`REMOTE_HOST_ENV`] at all, unconditionally: a `--record`/`--gate` run
//! that measured a real network hop whenever an operator's shell happened
//! to carry a leftover export from an oracle acceptance run would silently
//! ratchet this class's bar against a different transport, on no evidence
//! any log line names. A real target is local/acceptance infrastructure the
//! CI host cannot be assumed to have, exactly as `remote_real_ssh.rs`'s own
//! doc explains, and it stays that way by construction: the gated path has
//! no branch that could read the var, not a branch that happens not to
//! today.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::sampling::{interleave_schedule, Distribution, Side};
use crate::scenarios::memory::{self, MemoryOutcome};
use crate::scenarios::Protocol;
use crate::session::ViewSpec;
use crate::BenchError;

/// The destination `--remote` is given for the CI leg: parsed and dropped
/// by the stub exactly as `view_oracle::remote::stub_spec`'s own target is
/// (see that module for why it must still look like a hostname).
pub const STUB_TARGET: &str = "view-bench-stub-host";

/// Env var naming a real remote host for the opt-in leg, in `[user@]host`
/// syntax -- the same name `crates/view-oracle/tests/remote_real_ssh.rs`
/// reads.
pub const REMOTE_HOST_ENV: &str = "VIEW_REMOTE_TEST_HOST";

/// Env var naming the far-side `nvim` for the opt-in leg, mirroring
/// `remote_real_ssh.rs`'s own `VIEW_REMOTE_TEST_NVIM`: a real remote
/// account's `PATH` is not this host's, so a value here is the only way to
/// point at an `nvim` that PATH does not carry.
pub const REMOTE_NVIM_ENV: &str = "VIEW_REMOTE_TEST_NVIM";

/// Env var naming the `view` binary the opt-in leg's own test spawns,
/// mirroring the other two: the test has no other way to locate a release
/// build, since it is a `#[cfg(test)]` unit test rather than a bench-matrix
/// row that already carries `--view-bin` on its own command line.
pub const REMOTE_VIEW_BIN_ENV: &str = "VIEW_REMOTE_TEST_VIEW_BIN";

/// Reads view's own process memory after the standard workload, the same
/// way [`memory::run`] does, against whatever [`crate::session::SpawnSpec`]
/// the caller built. See this module's doc for why a pass-through is the
/// whole driver rather than a second one.
///
/// # Errors
///
/// Same as [`memory::run`].
pub fn run(view_spec: ViewSpec<'_>, protocol: &Protocol) -> Result<MemoryOutcome, BenchError> {
    memory::run(view_spec, protocol)
}

/// The local leg's metric name: [`memory::METRIC`] prefixed, so a reader of
/// `[remote_memory.*]` in a baseline file can never mistake this row's
/// local-leg reading -- drawn from the same interleaved window as the
/// remote one, for the paired ratio below -- for `memory.minimal`'s own,
/// separately-spawned reading of the same platform quantity.
pub const LOCAL_METRIC: Option<&str> = if cfg!(target_os = "linux") {
    Some("local_pss_mb")
} else if cfg!(target_os = "macos") {
    Some("local_phys_footprint_mb")
} else {
    None
};

/// The gated metric: the remote leg's absolute divided by the local leg's,
/// both drawn from the same interleaved window -- the row's actual claim
/// (see this module's doc) and the only metric it gates.
///
/// Named with neither a `p99` component nor a `delta` substring on
/// purpose: `view_harness::baselines::gate_headroom` exempts a tail
/// statistic (a name with a `p99` component) from gating on a shared
/// class, which is exactly backwards for this metric -- the ruling this
/// row implements found the ratio regime-invariant where the absolutes are
/// not, so it must gate everywhere, not become exempt by an accident of
/// spelling. A `delta` substring would instead route it through
/// `Headroom::Signed`, whose floor (`SIGNED_DELTA_FLOOR_MS`) is in
/// milliseconds and wrong for a dimensionless ratio; `contains("ratio")`
/// is the correct branch and this name earns it honestly.
pub const RATIO_METRIC: &str = "remote_local_ratio";

/// One paired remote-vs-local reading: both legs' absolutes (record-only
/// on a shared class -- see this module's doc) and the ratio between them
/// (gated everywhere).
#[derive(Debug)]
pub struct RemoteLocalOutcome {
    pub remote_distribution: Distribution,
    pub remote_metric: &'static str,
    /// p99 of the remote leg's post-workload reads, in megabytes.
    pub remote_mb: f64,
    pub local_distribution: Distribution,
    pub local_metric: &'static str,
    /// p99 of the local leg's post-workload reads, in megabytes.
    pub local_mb: f64,
    /// The gated statistic: `remote.p50() / local.p50()`. A median, not a
    /// p99 ratio, for the same reason `ratio_p50` (not `ratio_p99`) is the
    /// echo row's regime-invariant statistic: a tail is set by whichever
    /// sample a scheduler preemption landed on, while the bulk of the
    /// distribution is not, and this ratio must stay meaningful on a
    /// shared class where p99 tails are exempt from gating.
    pub ratio: f64,
}

/// The gated ratio from two full-window distributions: `remote.p50() /
/// local.p50()`. Split out from [`run_paired`] so the falsifiable claim
/// under test -- that only a same-window pairing recovers the row's actual
/// relationship, and a stale one does not -- can be exercised in this
/// module's tests without a live pair of spawned sessions.
fn remote_local_ratio(remote: &Distribution, local: &Distribution) -> f64 {
    remote.p50() / local.p50()
}

/// Alternately samples the remote and local legs `total` times each,
/// `block` at a time starting with the remote leg -- the same
/// per-sample-alternation discipline `view_bench::pairing` uses for the
/// view/nvim pairing (`crate::sampling::interleave_schedule`), reused here
/// for remote/local so a run-wide drift (host load moving between the
/// first and the last sample) lands on both legs' distributions instead of
/// accumulating on whichever leg happened to sample later.
///
/// [`Side::View`] is read as "the remote leg" and [`Side::Nvim`] as "the
/// local leg" here -- not a view/nvim pairing at all, since both legs
/// spawn `view` -- because the alternative was a third two-variant enum
/// whose only job would be to alternate, which `Side` already does; the
/// mapping is documented at every call site rather than left implicit.
///
/// Generic over the two readers so the interleaving itself -- the property
/// this module's tests break on purpose -- is exercised without a live
/// pair of spawned sessions.
///
/// # Errors
///
/// Whatever the readers return.
fn interleaved_readings(
    total: usize,
    block: usize,
    mut remote_reader: impl FnMut() -> Result<f64, BenchError>,
    mut local_reader: impl FnMut() -> Result<f64, BenchError>,
) -> Result<(Vec<f64>, Vec<f64>), BenchError> {
    let mut remote_raw = Vec::with_capacity(total);
    let mut local_raw = Vec::with_capacity(total);
    for block in interleave_schedule(total, block.max(1), Side::View) {
        for _ in 0..block.count {
            match block.side {
                Side::View => remote_raw.push(remote_reader()?),
                Side::Nvim => local_raw.push(local_reader()?),
            }
        }
    }
    Ok((remote_raw, local_raw))
}

/// The paired driver: prepares a local session and a remote one through
/// the identical spawn/workload/settle sequence
/// ([`memory::prepare_workload_session`]), then alternately samples both
/// (see [`interleaved_readings`]) `protocol.warmup + protocol.samples`
/// times each. See this module's doc for why the gated statistic is the
/// ratio rather than either absolute.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if the platform defines no memory
/// metric, either session never settles, either pid is unavailable, or
/// either leg's reading cannot be taken.
pub fn run_paired(
    local_spec: ViewSpec<'_>,
    remote_spec: ViewSpec<'_>,
    protocol: &Protocol,
) -> Result<RemoteLocalOutcome, BenchError> {
    let Some(remote_metric) = memory::METRIC else {
        return Err(BenchError::Desync {
            context: "no memory metric is defined for this platform".to_string(),
        });
    };
    let Some(local_metric) = LOCAL_METRIC else {
        return Err(BenchError::Desync {
            context: "no local-leg memory metric is defined for this platform".to_string(),
        });
    };

    let ViewSpec(local) = local_spec;
    let ViewSpec(remote) = remote_spec;
    let (mut remote_session, remote_pid) = memory::prepare_workload_session(remote)?;
    let (mut local_session, local_pid) = memory::prepare_workload_session(local)?;

    let total = protocol.warmup + protocol.samples;
    let pace = Duration::from_millis(2);
    let (remote_raw, local_raw) = interleaved_readings(
        total,
        protocol.block,
        || {
            let value =
                memory::sample_reading(&mut remote_session, remote_pid, memory::read_memory_mb);
            let next = Instant::now() + pace;
            while Instant::now() < next {
                std::thread::yield_now();
            }
            value
        },
        || {
            let value =
                memory::sample_reading(&mut local_session, local_pid, memory::read_memory_mb);
            let next = Instant::now() + pace;
            while Instant::now() < next {
                std::thread::yield_now();
            }
            value
        },
    )?;
    remote_session.shutdown();
    local_session.shutdown();

    let remote_distribution = Distribution::from_samples(&remote_raw, protocol.warmup)?;
    let local_distribution = Distribution::from_samples(&local_raw, protocol.warmup)?;
    let ratio = remote_local_ratio(&remote_distribution, &local_distribution);
    let remote_mb = remote_distribution.p99();
    let local_mb = local_distribution.p99();

    Ok(RemoteLocalOutcome {
        remote_distribution,
        remote_metric,
        remote_mb,
        local_distribution,
        local_metric,
        local_mb,
        ratio,
    })
}

/// Prepares `dir` to stand in for a `PATH` entry whose `ssh` resolves to
/// the committed stand-in `ssh` client, and returns the `PATH` value a
/// spawn should carry: `dir` prepended to `existing`.
///
/// A symlink rather than a copy: the stub is a script this tree owns and
/// commits, so a symlink names it once and can never drift from a copy
/// taken at some earlier commit. Any stale link already at `dir/ssh` (an
/// earlier scratch world's leftover, or one pointing at a since-moved
/// checkout) is replaced rather than trusted, since an existence check
/// would hide exactly that drift.
///
/// # Errors
///
/// [`BenchError::Desync`] if this host has no stub client at all (a `PATH`
/// entry pointing at nothing is a spawn that fails opaquely at the client,
/// not here), or if `dir` cannot be created or the symlink cannot be
/// placed.
pub fn arm_stub_ssh_path(dir: &Path, existing: Option<&OsStr>) -> Result<OsString, BenchError> {
    if !view_oracle::remote::stub_available() {
        return Err(BenchError::Desync {
            context: format!(
                "no stand-in ssh client on this host ({} is not an executable POSIX script), \
                 so a PATH entry in {} would point ssh at nothing",
                view_oracle::remote::stub_client().display(),
                dir.display()
            ),
        });
    }
    std::fs::create_dir_all(dir).map_err(|source| BenchError::Desync {
        context: format!(
            "creating the stub-ssh PATH directory {}: {source}",
            dir.display()
        ),
    })?;
    let link = dir.join("ssh");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(view_oracle::remote::stub_client(), &link).map_err(|source| {
        BenchError::Desync {
            context: format!(
                "symlinking {} to the stub ssh client: {source}",
                link.display()
            ),
        }
    })?;
    let mut path = OsString::from(dir);
    if let Some(existing) = existing {
        path.push(":");
        path.push(existing);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::session::SpawnSpec;
    use std::path::PathBuf;
    use view_test_support::ScratchDir;

    /// The falsifiable claim under test: only a same-window pairing (both
    /// legs read at the same interleaved instant) recovers the row's
    /// actual remote-vs-local relationship under a host regime that drifts
    /// both legs' absolutes together, the way the ruling's own citation
    /// describes (+/-20% PSS swing across days, +0.6-2% paired ratio
    /// across the same regimes). A stale local draw -- compared against a
    /// remote reading from later in the drift instead of one taken
    /// alongside it -- must not recover it.
    #[test]
    fn interleaved_reads_recover_the_fixed_ratio_under_shared_drift_but_a_stale_pairing_does_not() {
        let total = 200;
        let local_base = 100.0;
        let remote_base = local_base * 1.012;
        // drifts 20% peak-to-trough across the run, centered on 1.0 --
        // the shared regime shift itself, applied identically to both legs
        let regime = |i: usize| 1.0 + 0.20 * (i as f64 / total as f64 - 0.5);

        let mut remote_i = 0usize;
        let mut local_i = 0usize;
        let (remote_raw, local_raw) = interleaved_readings(
            total,
            1,
            || {
                let value = remote_base * regime(remote_i);
                remote_i += 1;
                Ok(value)
            },
            || {
                let value = local_base * regime(local_i);
                local_i += 1;
                Ok(value)
            },
        )
        .expect("synthetic readers never fail");
        let remote_dist = Distribution::from_samples(&remote_raw, 0).unwrap();
        let local_dist = Distribution::from_samples(&local_raw, 0).unwrap();
        let paired_ratio = remote_local_ratio(&remote_dist, &local_dist);
        assert!(
            (paired_ratio - 1.012).abs() < 1e-9,
            "block=1 alternation pairs remote and local at the same regime index every step, \
             so the aggregate ratio must recover the fixed 1.012 relationship exactly \
             regardless of the shared drift, got {paired_ratio}"
        );

        // break the pairing: compare the same remote draws against a
        // single stale local reading (index 0, never refreshed) instead
        // of one taken alongside each remote sample -- the shape a bug
        // that stopped re-sampling the local leg every iteration would
        // produce
        let stale_local_raw = vec![local_base * regime(0); total];
        let stale_local_dist = Distribution::from_samples(&stale_local_raw, 0).unwrap();
        let stale_ratio = remote_local_ratio(&remote_dist, &stale_local_dist);
        assert!(
            (stale_ratio - 1.012).abs() > 0.05,
            "a stale local draw must not recover the fixed relationship once the regime has \
             drifted away from index 0, got {stale_ratio}"
        );
    }

    /// `interleave_schedule`'s own contract (strict alternation, balanced
    /// per-side totals) is proven in `crate::sampling`; this checks only
    /// that [`interleaved_readings`] routes [`Side::View`] to the remote
    /// reader and [`Side::Nvim`] to the local one, per this module's own
    /// documented mapping.
    #[test]
    fn interleaved_readings_routes_view_to_remote_and_nvim_to_local() {
        let (remote_raw, local_raw) = interleaved_readings(4, 1, || Ok(9.0), || Ok(1.0)).unwrap();
        assert_eq!(remote_raw, vec![9.0; 4]);
        assert_eq!(local_raw, vec![1.0; 4]);
    }

    /// `arm_stub_ssh_path`'s own claim: after arming, `dir/ssh` resolves
    /// (via a `PATH` built from its return value) to the same file
    /// [`view_oracle::remote::stub_client`] names.
    #[test]
    fn armed_path_resolves_ssh_to_the_committed_stub() {
        if !view_oracle::remote::stub_available() {
            eprintln!("skipped: no stub ssh client on this host");
            return;
        }
        let scratch = ScratchDir::new("remote-memory-test").expect("creating the scratch dir");
        let path = arm_stub_ssh_path(&scratch, std::env::var_os("PATH").as_deref())
            .expect("arming a fresh scratch directory must succeed");
        let resolved = std::env::split_paths(&path)
            .find_map(|entry| {
                let candidate = entry.join("ssh");
                candidate.is_file().then_some(candidate)
            })
            .expect("the returned PATH must resolve an executable ssh");
        let resolved = std::fs::canonicalize(&resolved).expect("canonicalizing the resolved ssh");
        let stub = std::fs::canonicalize(view_oracle::remote::stub_client())
            .expect("canonicalizing the committed stub client");
        assert_eq!(
            resolved, stub,
            "the first ssh the armed PATH resolves must be the committed stub, not some other \
             ssh already on PATH"
        );
    }

    /// Re-arming the same directory replaces a stale link rather than
    /// leaving it: the doc's own claim about what an existence check would
    /// hide.
    #[test]
    fn rearming_replaces_a_stale_link() {
        if !view_oracle::remote::stub_available() {
            eprintln!("skipped: no stub ssh client on this host");
            return;
        }
        let scratch = ScratchDir::new("remote-memory-restale").expect("creating the scratch dir");
        let stale_target = scratch.join("not-a-real-client");
        std::fs::write(&stale_target, "#!/bin/sh\nexit 1\n").expect("writing a stale target");
        std::os::unix::fs::symlink(&stale_target, scratch.join("ssh"))
            .expect("planting a stale link");

        arm_stub_ssh_path(&scratch, None).expect("re-arming an already-populated directory");
        let resolved =
            std::fs::read_link(scratch.join("ssh")).expect("reading the link back after re-arming");
        assert_eq!(
            resolved,
            view_oracle::remote::stub_client(),
            "re-arming must replace the stale link, not leave it pointing at the old target"
        );
    }

    /// [`REMOTE_HOST_ENV`]'s own module doc promises: a real remote target
    /// plus the view binary this crate's tests have no other way to
    /// locate, mirroring `remote_real_ssh.rs`'s loud-skip-when-unset
    /// convention exactly.
    ///
    /// ```sh
    /// export VIEW_REMOTE_TEST_HOST=a-dev-box
    /// export VIEW_REMOTE_TEST_VIEW_BIN=/path/to/release/view
    /// # optional, only when the far side's PATH does not carry nvim:
    /// export VIEW_REMOTE_TEST_NVIM=/opt/homebrew/bin/nvim
    /// cargo test -p view-bench remote_memory:: -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs VIEW_REMOTE_TEST_HOST, VIEW_REMOTE_TEST_VIEW_BIN, and a reachable ssh target"]
    fn remote_footprint_over_a_real_ssh_target_is_readable() {
        fn env_var(name: &str) -> Option<String> {
            match std::env::var(name) {
                Ok(value) if !value.is_empty() => Some(value),
                _ => None,
            }
        }

        let Some(host) = env_var(REMOTE_HOST_ENV) else {
            eprintln!("skipped: {REMOTE_HOST_ENV} is unset (see this module's doc)");
            return;
        };
        let Some(view_bin) = env_var(REMOTE_VIEW_BIN_ENV) else {
            eprintln!("skipped: {REMOTE_VIEW_BIN_ENV} is unset (see this module's doc)");
            return;
        };

        let mut args = vec![OsString::from("--remote"), OsString::from(host)];
        if let Some(nvim) = env_var(REMOTE_NVIM_ENV) {
            args.push(OsString::from("--nvim-bin"));
            args.push(OsString::from(nvim));
        }
        let spec = SpawnSpec {
            program: PathBuf::from(view_bin),
            args,
            env: Vec::new(),
            cwd: None,
        };
        let outcome = run(
            ViewSpec(&spec),
            &Protocol {
                samples: 20,
                warmup: 5,
                ..Protocol::default()
            },
        )
        .expect("a reachable remote target must produce a memory reading");
        assert!(
            outcome.gated_mb > 0.0,
            "memory::run's own positivity floor already guarantees this; restated here as the \
             acceptance leg's own claim about a real connection"
        );
    }
}
