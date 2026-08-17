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
//! changed. The row's actual claim survives that noise: a single window
//! put view's own local footprint with a remote engine, divided by its
//! footprint with a local one, at a +1.2% delta (see "One leg at a time"
//! below for that recording). Whether the delta holds across the regime
//! itself -- not just within one window -- is tracked separately in
//! `.claude/plans/2026-08-09-p5_5-remote.md:405-413`: the amendment there
//! records the +/-20% across-day host-regime envelope and a +1.23% paired
//! delta landing inside it. So [`run_paired`]
//! gates [`RATIO_METRIC`] -- the remote/local ratio -- and records both
//! absolutes for reference, record-only on a shared class the same way a
//! tail statistic already is (see `view_harness::baselines::gate_headroom`).
//!
//! # One leg at a time
//!
//! [`run`] is a pass-through to [`memory::run`] -- the same workload
//! ([`memory::workload_files`], [`memory::workload_content`]), the same
//! settle/sample loop, the same own-process reader. It exists for the
//! opt-in real-SSH connectivity test at the bottom of this module, which
//! needs to prove a reading is obtainable at all, not to compare it against
//! anything.
//!
//! The gated row's driver is [`run_paired`], which never has two `view`
//! processes alive at once. A settled process's PSS/`phys_footprint`
//! reading is proportional to how many processes map each of its pages:
//! co-resident instances of one binary split their shared file-backed
//! pages' PSS between them, so keeping both legs alive through one shared
//! sampling window -- this row's design before this revision -- deflates
//! both legs' readings below what either reads alone. Measured ~-8.0% on
//! this class for a co-resident sibling against a ~+0.8% no-sibling
//! control (the same process read twice, alone); reproducible by spawning
//! a second instance of any sufficiently large binary and re-reading
//! `/proc/<pid>/smaps_rollup` for the first with and without the second
//! alive. [`run_paired`] instead runs `protocol.trials` ABBA-alternating
//! trial pairs ([`abba_trials`], which rounds an odd count up to even --
//! see below): each
//! trial spawns one leg through [`memory::prepare_workload_session`] and
//! [`memory::sample_distribution`], tears it down synchronously
//! ([`run_one_leg`]), then does the same for the other leg, so a second
//! `view` process never exists to deflate the first's reading.
//!
//! The two legs never overlap, so there is no idle-aging hazard to cancel
//! -- the leg spawned second is not spawned until the first has been
//! killed and reaped. What alternation cancels instead is a residual
//! positional bias: whichever leg samples second in a trial does so later
//! in wall-clock time than the one sampled first, exposed to whatever
//! ambient regime drift or leftover page-cache state the first leg's run
//! and teardown left behind. A fixed order would let that later-position
//! exposure land on the same leg every trial; alternating -- trial 0
//! samples remote then local, trial 1 local then remote, and so on --
//! spreads it across both legs in opposite trials instead. Alternation
//! only fully cancels across an even number of trials (each leg samples
//! first in exactly half of them), so [`abba_trials`] rounds an odd
//! request up by one rather than running it uncancelled or silently
//! dropping a trial -- a caller-side precondition would let some future
//! caller forget it, so the rounding lives with the loop it protects
//! instead. Each trial's ratio comes from that trial's
//! own pair of readings ([`remote_local_ratio`]), and the three reported
//! statistics -- both absolutes and the ratio -- are each the median
//! across trials of that trial's own value, never a value pooled from
//! every trial's raw samples (the discipline
//! `view_bench::scenarios::echo`'s multi-trial outcome also uses); the
//! ratio statistic is a median of per-trial ratios, not the quotient of
//! the two reported absolute medians, so a reader dividing them back out
//! will not recover it exactly.
//!
//! This costs wall clock the withdrawn concurrent design did not: instead
//! of two spawns running at once, each trial pays two full
//! spawn/settle/workload/settle/sample/teardown cycles back to back, so a
//! run costs `2 x` trial count of that cycle -- 8 sequential cycles at the
//! CLI's default `trials = 3` (rounded up to 4), against the 2 concurrent
//! spawns the withdrawn design paid.
//!
//! The +1.2% figure above is drawn from `remote_memory.pss_mb`'s
//! `[headroom]` entry in `baselines/dev-linux.headroom.toml` (recorded
//! 2026-08-16): an 8-report-plus-1-record replicate campaign put
//! `remote_memory/minimal`'s 9-draw median PSS at 3.734 MB against
//! `memory/minimal`'s own paired-window median of 3.6885 MB, both solo,
//! non-co-resident spawns under the row's pass-through driver at the
//! time. That topology is exactly what [`run_paired`]'s sequential legs
//! now automate one pair at a time, so the figure is directly comparable
//! to what this driver measures rather than needing a co-residency
//! caveat.
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

use crate::sampling::{median_of_trials, Distribution};
use crate::scenarios::memory::{self, MemoryOutcome};
use crate::scenarios::Protocol;
use crate::session::{SpawnSpec, ViewSpec};
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
/// local-leg reading -- paired against the remote leg's for the ratio
/// below, though never co-resident with it (see this module's doc) -- for
/// `memory.minimal`'s own, separately-spawned reading of the same
/// platform quantity.
pub const LOCAL_METRIC: Option<&str> = if cfg!(target_os = "linux") {
    Some("local_pss_mb")
} else if cfg!(target_os = "macos") {
    Some("local_phys_footprint_mb")
} else {
    None
};

/// The gated metric: the remote leg's absolute divided by the local leg's,
/// both drawn from the same trial -- the row's actual claim (see this
/// module's doc) and the only metric it gates.
///
/// Named with neither a `p99` component nor a `delta` substring on
/// purpose: `view_harness::baselines::gate_headroom` exempts a tail
/// statistic (a name with a `p99` component) from gating on a shared
/// class, which is exactly backwards for this metric -- the historical
/// evidence this row relies on found the ratio regime-invariant where the
/// absolutes are not, so it must gate everywhere, not become exempt by an
/// accident of spelling. A `delta` substring would instead route it
/// through `Headroom::Signed`, whose floor (`SIGNED_DELTA_FLOOR_MS`) is in
/// milliseconds and wrong for a dimensionless ratio; `contains("ratio")`
/// is the correct branch and this name earns it honestly.
pub const RATIO_METRIC: &str = "remote_local_ratio";

/// The two legs of a paired remote-vs-local comparison, named rather than
/// positional. Two identically typed [`ViewSpec`] parameters at a call
/// site can be transposed with nothing at the type checker to catch it,
/// and a transposed pair here would report the local leg's reading under
/// the remote metric and vice versa with no error anywhere in the chain.
/// Naming the fields makes that mistake unrepresentable: the caller must
/// write `local:`/`remote:` at the construction site, so a transposition
/// is a visibly wrong field name instead of a silently swapped positional
/// argument.
pub struct RemoteLocalSpecs<'a> {
    pub local: ViewSpec<'a>,
    pub remote: ViewSpec<'a>,
}

/// One ABBA trial's pair of readings: the remote leg's distribution, the
/// local leg's, and the ratio between them ([`remote_local_ratio`]),
/// computed while both are still in scope so a later change cannot pair
/// one trial's remote reading against a different trial's local one.
#[derive(Debug)]
pub struct RemoteTrial {
    pub remote: Distribution,
    pub local: Distribution,
    /// `remote.p50() / local.p50()`, this trial's own contribution to
    /// [`RemoteLocalOutcome::gated_ratio`].
    pub ratio: f64,
}

/// A full paired remote-vs-local run: every trial's own pair
/// ([`RemoteTrial`]), plus the row's three reported statistics, each the
/// median across trials of that trial's own value (see this module's
/// doc).
#[derive(Debug)]
pub struct RemoteLocalOutcome {
    pub remote_metric: &'static str,
    pub local_metric: &'static str,
    pub trials: Vec<RemoteTrial>,
    /// Median across trials of each trial's remote leg p99, in megabytes.
    pub gated_remote_mb: f64,
    /// Median across trials of each trial's local leg p99, in megabytes.
    pub gated_local_mb: f64,
    /// Median across trials of each trial's ratio -- the row's only gated
    /// metric. A median of per-trial ratios, not the quotient of
    /// `gated_remote_mb` and `gated_local_mb`: dividing those two back out
    /// will not in general recover this value.
    pub gated_ratio: f64,
}

/// The ratio from one trial's two full distributions: `remote.p50() /
/// local.p50()`. A median, not a p99 ratio, for the same reason
/// `ratio_p50` (not `ratio_p99`) is the echo row's regime-invariant
/// statistic: a tail is set by whichever sample a scheduler preemption
/// landed on, while the bulk of the distribution is not, and this ratio
/// must stay meaningful on a shared class where p99 tails are exempt from
/// gating.
///
/// Split out from [`abba_trials`] so the falsifiable claim under test --
/// that a trial's ratio comes from that trial's own pair of readings, not
/// a neighboring trial's -- can be exercised in this module's tests
/// without a live pair of spawned sessions.
fn remote_local_ratio(remote: &Distribution, local: &Distribution) -> f64 {
    remote.p50() / local.p50()
}

/// Runs `trials` ABBA-alternating trial pairs, first rounded up to the
/// nearest even number via [`even_trial_count`] (see that function's doc
/// for why): trial 0 samples the remote leg then the local one, trial 1
/// samples local then remote, and so on. `remote_leg`/`local_leg` never
/// run concurrently -- each is called to completion before the other
/// starts -- so this function only decides their *order*, never their
/// overlap; see this module's doc for why that ordering matters and what
/// alternating it buys over a fixed order.
///
/// The rounding lives here rather than at a call site so every caller gets
/// a trial count alternation can fully cancel -- an odd `trials` is safe
/// to pass in, not a precondition a caller must enforce first.
///
/// Generic over the two legs' runners so the ordering discipline -- the
/// property this module's tests break on purpose -- is exercised without
/// a live pair of spawned sessions.
///
/// # Errors
///
/// Whatever `remote_leg`/`local_leg` return.
fn abba_trials(
    trials: usize,
    mut remote_leg: impl FnMut() -> Result<Distribution, BenchError>,
    mut local_leg: impl FnMut() -> Result<Distribution, BenchError>,
) -> Result<Vec<RemoteTrial>, BenchError> {
    let trials = even_trial_count(trials);
    let mut pairs = Vec::with_capacity(trials);
    for trial in 0..trials {
        let (remote, local) = if trial.is_multiple_of(2) {
            let remote = remote_leg()?;
            let local = local_leg()?;
            (remote, local)
        } else {
            let local = local_leg()?;
            let remote = remote_leg()?;
            (remote, local)
        };
        let ratio = remote_local_ratio(&remote, &local);
        pairs.push(RemoteTrial {
            remote,
            local,
            ratio,
        });
    }
    Ok(pairs)
}

/// One leg of one trial, start to finish: spawn `spec` through
/// [`memory::prepare_workload_session`], sample it through
/// [`memory::sample_distribution`], and tear it down -- with no other
/// `view` process alive while any of that happens.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if the session never settles, the pid is
/// unavailable, or the reading cannot be taken.
fn run_one_leg(spec: &SpawnSpec, protocol: &Protocol) -> Result<Distribution, BenchError> {
    let (session, pid) = memory::prepare_workload_session(spec)?;
    memory::sample_distribution(session, pid, protocol, memory::read_memory_mb)
}

/// Rounds `requested` up to the nearest even number, never down: an odd
/// request run exactly would leave alternation's positional-bias
/// cancellation one trial short (see this module's doc), and a caller
/// asking for `N` trials should never receive fewer.
fn even_trial_count(requested: usize) -> usize {
    if requested.is_multiple_of(2) {
        requested
    } else {
        requested + 1
    }
}

/// The paired driver: runs `protocol.trials` ABBA-alternating trial pairs
/// ([`abba_trials`], which rounds an odd count up to even itself -- see
/// that function's doc), each spawning [`RemoteLocalSpecs::remote`] and
/// [`RemoteLocalSpecs::local`] one at a time ([`run_one_leg`]), and
/// aggregates every reported statistic as the median across trials. See
/// this module's doc for why the gated statistic is the ratio rather than
/// either absolute, and for why no two `view` processes from this driver
/// are ever alive at once.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if the platform defines no memory
/// metric, a session never settles, a pid is unavailable, or a leg's
/// reading cannot be taken, and [`BenchError::NoTrials`] if no trial was
/// asked for.
pub fn run_paired(
    specs: RemoteLocalSpecs<'_>,
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

    let RemoteLocalSpecs { local, remote } = specs;
    let ViewSpec(local_spec) = local;
    let ViewSpec(remote_spec) = remote;
    let trials = abba_trials(
        protocol.trials,
        || run_one_leg(remote_spec, protocol),
        || run_one_leg(local_spec, protocol),
    )?;

    let remote_p99s: Vec<f64> = trials.iter().map(|trial| trial.remote.p99()).collect();
    let local_p99s: Vec<f64> = trials.iter().map(|trial| trial.local.p99()).collect();
    let ratios: Vec<f64> = trials.iter().map(|trial| trial.ratio).collect();
    let gated_remote_mb = median_of_trials(&remote_p99s)?;
    let gated_local_mb = median_of_trials(&local_p99s)?;
    let gated_ratio = median_of_trials(&ratios)?;

    Ok(RemoteLocalOutcome {
        remote_metric,
        local_metric,
        trials,
        gated_remote_mb,
        gated_local_mb,
        gated_ratio,
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
    use std::path::PathBuf;
    use view_test_support::ScratchDir;

    /// A trivial fixed-value distribution, for tests that only care about
    /// the value [`Distribution::p50`]/[`Distribution::p99`] read back, not
    /// about a real sample spread.
    fn const_distribution(value: f64) -> Distribution {
        Distribution::from_samples(&[value, value], 0).unwrap()
    }

    /// [`even_trial_count`]'s own contract: an even request passes through
    /// unchanged, an odd one is rounded up by exactly one, and it never
    /// rounds down (a rounded-down 3 -> 2 would silently drop a trial the
    /// caller asked for).
    #[test]
    fn even_trial_count_rounds_odd_up_by_one_and_leaves_even_alone() {
        assert_eq!(even_trial_count(0), 0);
        assert_eq!(even_trial_count(1), 2);
        assert_eq!(even_trial_count(2), 2);
        assert_eq!(even_trial_count(3), 4);
        assert_eq!(even_trial_count(4), 4);
    }

    /// The falsifiable claim under test: alternating which leg is spawned
    /// first each trial narrows a fixed *positional* bias (whichever leg
    /// samples second in a trial does so later in wall-clock time than the
    /// one sampled first, exposed to whatever ambient drift or leftover
    /// state the first leg's run left behind, and so reads low relative to
    /// the other) compared to always spawning the same leg first, which
    /// lets the bias land on that leg in every trial with nothing to
    /// offset it.
    ///
    /// The two runs share one synthetic bias model (a leg read in the
    /// first position of its trial reads 3% low) and differ only in the
    /// order [`abba_trials`] is allowed to pick: fixed order every trial
    /// (the mutation this module's redesign eliminates) versus the real
    /// ABBA alternation.
    #[test]
    fn alternating_leg_order_narrows_a_fixed_position_bias_a_fixed_order_leaves_uncancelled() {
        let true_remote = 101.2;
        let true_local = 100.0;
        let true_ratio = true_remote / true_local;
        let position_penalty = 0.97; // the leg spawned first in its trial reads 3% low

        // fixed order: remote is always spawned first, so it always eats
        // the position penalty -- an uncancelled fixed-order position
        // bias, exactly what alternating leg order exists to prevent.
        let fixed_pairs = abba_trials(
            4,
            || Ok(const_distribution(true_remote * position_penalty)),
            || Ok(const_distribution(true_local)),
        )
        .unwrap();
        let fixed_ratios: Vec<f64> = fixed_pairs.iter().map(|trial| trial.ratio).collect();
        let fixed_median = median_of_trials(&fixed_ratios).unwrap();
        let fixed_error = (fixed_median - true_ratio).abs();

        // real ABBA alternation: whichever leg abba_trials calls first
        // each trial eats the position penalty, tracked by a shared call
        // counter rather than by which closure is which -- a call at an
        // even global index is always the first of its trial's pair,
        // since every trial contributes exactly two calls in sequence.
        let call_index = std::cell::Cell::new(0usize);
        let alternating_pairs = abba_trials(
            4,
            || {
                let first = call_index.get().is_multiple_of(2);
                call_index.set(call_index.get() + 1);
                let value = if first {
                    true_remote * position_penalty
                } else {
                    true_remote
                };
                Ok(const_distribution(value))
            },
            || {
                let first = call_index.get().is_multiple_of(2);
                call_index.set(call_index.get() + 1);
                let value = if first {
                    true_local * position_penalty
                } else {
                    true_local
                };
                Ok(const_distribution(value))
            },
        )
        .unwrap();
        let alternating_ratios: Vec<f64> =
            alternating_pairs.iter().map(|trial| trial.ratio).collect();
        let alternating_median = median_of_trials(&alternating_ratios).unwrap();
        let alternating_error = (alternating_median - true_ratio).abs();

        assert!(
            fixed_error > 0.02,
            "a fixed spawn order must leave a real, uncancelled bias in the gated ratio, \
             got median {fixed_median} against true ratio {true_ratio} (error {fixed_error})"
        );
        assert!(
            alternating_error < fixed_error / 2.0,
            "alternating leg order must land closer to the true ratio than a fixed order does, \
             got alternating median {alternating_median} (error {alternating_error}) against \
             fixed median {fixed_median} (error {fixed_error})"
        );
    }

    /// The falsifiable claim under test: each trial's ratio comes from
    /// that trial's own remote and local readings, never a neighboring
    /// trial's. `abba_trials`' leg-order alternation does not change which
    /// value each leg's closure returns (each simply pulls its own next
    /// value), so this test is independent of alternation and isolates
    /// the pairing itself -- a bug that reused a stale value from the
    /// previous trial (an off-by-one on either leg) would move at least
    /// one of these four ratios away from its expected value. Requests an
    /// already-even count so the pairing invariant stays isolated from the
    /// rounding [`abba_trials_rounds_an_odd_request_up_to_even`] covers.
    #[test]
    fn each_trials_ratio_pairs_its_own_remote_and_local_reading() {
        let remote_values = [10.0, 20.0, 30.0, 40.0];
        let local_values = [1.0, 4.0, 9.0, 16.0];
        let expected_ratios = [10.0, 5.0, 30.0 / 9.0, 2.5];

        let mut remote_iter = remote_values.into_iter();
        let mut local_iter = local_values.into_iter();
        let pairs = abba_trials(
            4,
            || Ok(const_distribution(remote_iter.next().unwrap())),
            || Ok(const_distribution(local_iter.next().unwrap())),
        )
        .unwrap();

        let ratios: Vec<f64> = pairs.iter().map(|trial| trial.ratio).collect();
        for (got, expected) in ratios.iter().zip(expected_ratios.iter()) {
            assert!(
                (got - expected).abs() < 1e-9,
                "each trial's ratio must be computed from that trial's own remote and local \
                 reading, got {ratios:?}, expected {expected_ratios:?}"
            );
        }
    }

    /// [`abba_trials`]' own rounding contract, now that it -- not
    /// `run_paired` -- owns the [`even_trial_count`] call: an odd request
    /// must still come back as an even number of trials, so a caller that
    /// forgets to round first (or a future caller that never knew to)
    /// still gets alternation's full positional-bias cancellation rather
    /// than a silently uncancelled odd count.
    #[test]
    fn abba_trials_rounds_an_odd_request_up_to_even() {
        let pairs = abba_trials(
            3,
            || Ok(const_distribution(1.0)),
            || Ok(const_distribution(1.0)),
        )
        .unwrap();
        assert_eq!(
            pairs.len(),
            4,
            "an odd request of 3 must come back as 4 trials, matching even_trial_count(3)"
        );
    }

    /// [`abba_trials`]' own contract: trial 0 spawns the remote leg first,
    /// trial 1 spawns the local leg first, and so on -- never the same
    /// order twice running.
    #[test]
    fn abba_trials_alternates_which_leg_is_spawned_first() {
        use std::cell::RefCell;
        let call_order: RefCell<Vec<&'static str>> = RefCell::new(Vec::new());
        abba_trials(
            4,
            || {
                call_order.borrow_mut().push("remote");
                Ok(const_distribution(1.0))
            },
            || {
                call_order.borrow_mut().push("local");
                Ok(const_distribution(1.0))
            },
        )
        .unwrap();
        assert_eq!(
            *call_order.borrow(),
            vec!["remote", "local", "local", "remote", "remote", "local", "local", "remote"],
            "trial order must alternate remote-first/local-first every trial, with each leg \
             fully sampled before the other starts"
        );
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
