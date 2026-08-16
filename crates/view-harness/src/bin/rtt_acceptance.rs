//! `rtt-acceptance`: drives the `echo_speculated` scenario at four injected
//! SSH-RTT tiers (0/25/100/300 ms) through the delay-relay fixture in front
//! of the committed stub-ssh double, and asserts every tier's
//! `speculated_ratio_p50` stays under `budgets.toml`'s bound for that row.
//! This is the RTT-injection proof `scripts/acceptance/remote-rtt.sh`
//! drives; see that script and
//! `view_bench::scenarios::echo_speculated_rtt` for the delay-injection
//! design.
//!
//! ```text
//! cargo run --release -p view-harness --bin rtt-acceptance -- \
//!     --taps-view-bin target/taps/release/view --nvim-bin nvim
//! ```
//!
//! Deliberately outside `bench`'s own `--record`/`--gate` machinery: an
//! injected-latency run must never be able to reach the recorded/gated
//! `dev-linux` bar, the same caution `remote_memory`'s real-SSH leg already
//! documents for the identical reason (a leftover env var or flag
//! ratcheting the class's baseline against a transport no log line names).
//! This binary reads `budgets.toml` and the class baseline, but only ever
//! to print and compare against, never to record into.

use std::ffi::OsString;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use view_bench::scenarios::echo::DEFAULT_STARTUP_QUIET;
use view_bench::scenarios::echo_speculated_rtt::{self, RTT_TIERS_MS};
use view_bench::scenarios::taps::TapPipe;
use view_bench::scenarios::{echo_speculated, Protocol};
use view_bench::session::{NvimSpec, SpawnSpec, ViewSpec};
use view_harness::baselines::{self, CellId};
use view_harness::budgets;
use view_harness::fixture::{copy_dir_recursive, fixtures_root, scratch_root, workspace_root};

#[derive(Parser)]
struct Cli {
    /// Path to the bench-taps build of view. Defaults to the same
    /// `target/taps/release/view` `task bench` builds.
    #[arg(long)]
    taps_view_bin: Option<PathBuf>,
    /// Path to the nvim binary the bare reference side spawns.
    #[arg(long, default_value = "nvim")]
    nvim_bin: PathBuf,
    /// Fixture the scenario runs against.
    #[arg(long, default_value = "minimal")]
    fixture: String,
    /// Machine class the printed baseline/budget comparison reads;
    /// `speculated_ratio_p50`'s own bound is class-scoped the same way
    /// every other budget row is.
    #[arg(long, default_value = "dev-linux")]
    class: String,
    /// Measured samples per tier. Kept far below a recorded row's
    /// thousand-sample protocol: the falsifiable check here is a threshold
    /// crossing, not a tail statistic precise enough to record, and the
    /// 300ms tier's own injected delay already dominates this binary's
    /// wall time at any sample count.
    #[arg(long, default_value_t = 60)]
    samples: usize,
    /// Warmup samples per tier, excluded from the statistic.
    #[arg(long, default_value_t = 10)]
    warmup: usize,
    /// RTT tiers to run, in milliseconds, comma-separated (e.g.
    /// `--tiers 0,300`). Defaults to all four tiers the brief names; the
    /// 300ms tier alone dominates the leg's ~55s wall time, so
    /// reproducing one failing tier during debugging should not require
    /// editing `RTT_TIERS_MS` and rebuilding `view-bench`.
    #[arg(long, value_delimiter = ',')]
    tiers: Option<Vec<u64>>,
}

/// Owns the acceptance run's scratch root and removes it on every exit path
/// via [`Drop`] -- including the early `?`-propagated returns this binary's
/// `main` is full of, which the prior trailing `remove_dir_all` (reached
/// only after every tier succeeded) never ran for. Matches the
/// `ScratchDir` pattern in `view-test-support` (`Deref<Target = Path>` +
/// unconditional `Drop` cleanup) without adding that crate as a
/// non-dev dependency here: this binary's `main` is not test code, and the
/// scratch location/naming (`scratch_root("rtt-acceptance")`, not an
/// OS-temp-dir `view-<label>-<pid>` path) stays exactly what it was.
struct ScratchRoot(PathBuf);

impl Deref for ScratchRoot {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How many independent round trips [`median_probe`] takes through the
/// relay before reducing to a median. Sized from measured data, not
/// guessed: 150 raw single-sample trials at `DELAY_RELAY_MS=0` on this
/// host ranged 28.4-52.0ms (a 23.6ms single-sample spread); bucketing that
/// same pool into medians-of-N narrowed the spread to 11.1ms at N=7,
/// 9.0ms at N=9, 8.2ms at N=11.
const PROBE_TRIALS: usize = 11;

/// The probe payload every [`median_probe`] round trip must read back
/// unchanged.
const PROBE_LINE: &str = "hello from the jitter-tolerance test";

/// Median round-trip time through the delay relay at `rtt_ms`, over
/// [`PROBE_TRIALS`] independent trials wrapping `cat`.
///
/// # Errors
///
/// Whatever [`echo_speculated_rtt::round_trip_through_relay`] reports, or
/// [`BenchError::Desync`]'s anyhow-wrapped equivalent if any trial's probe
/// line comes back altered.
fn median_probe(cat: &str, rtt_ms: u64, trials: usize) -> Result<Duration> {
    let mut samples = Vec::with_capacity(trials);
    for _ in 0..trials {
        let (elapsed, line) = echo_speculated_rtt::round_trip_through_relay(rtt_ms, cat)
            .with_context(|| format!("probing the delay relay's round trip at RTT {rtt_ms}ms"))?;
        if line.trim_end() != PROBE_LINE {
            bail!(
                "RTT {rtt_ms}ms: the delay relay's probe echoed {line:?} instead of the probe \
                 line unchanged"
            );
        }
        samples.push(elapsed);
    }
    samples.sort_unstable();
    Ok(samples[trials / 2])
}

fn budgets_path() -> PathBuf {
    workspace_root()
        .join("crates")
        .join("view-bench")
        .join("budgets.toml")
}

fn baseline_path(class: &str) -> PathBuf {
    workspace_root()
        .join("crates")
        .join("view-bench")
        .join("baselines")
        .join(format!("{class}.toml"))
}

/// Reads one `[[budget]]` row's `max`, refusing to run a tier against a
/// bound this binary made up: every printed verdict below must trace back
/// to the one file the project's gate itself reads.
fn budget_max(
    file: &budgets::BudgetFile,
    scenario: &str,
    metric: &str,
    class: &str,
) -> Result<f64> {
    file.budget
        .iter()
        .find(|b| b.scenario == scenario && b.metric == metric && b.covers(class))
        .map(|b| b.max)
        .with_context(|| {
            format!(
                "no [[budget]] row for {scenario}/{metric} covers class {class} in {}",
                budgets_path().display()
            )
        })
}

/// One side's hermetic spawn inputs: a private copy of the fixture config
/// (so neither side's editor can see the other's, or a real user's own
/// `~/.config/nvim`), plus the scratch file both sides open.
///
/// A pared-down `CellWorld::side` (`bin/bench/cell_world.rs`): that type is
/// private to the `bench` binary crate and this is a separate `[[bin]]`
/// target, so it cannot be imported; the lockfile-keyed plugin cache branch
/// it carries for the `heavy` fixture is dropped here because this binary
/// only ever runs the `minimal` fixture's plain config, which has no
/// lockfile to key against.
struct Side {
    env: Vec<(OsString, OsString)>,
    cwd: PathBuf,
    scratch_file: PathBuf,
}

fn side_setup(fixture: &str, tag: &str, root: &std::path::Path) -> Result<Side> {
    let side_dir = root.join(tag);
    std::fs::create_dir_all(&side_dir)
        .with_context(|| format!("creating {}", side_dir.display()))?;
    let fixture_dir = fixtures_root().join(fixture);
    let xdg_config_home = side_dir.join("xdg_config_home");
    copy_dir_recursive(&fixture_dir, &xdg_config_home)
        .with_context(|| format!("copying fixture {fixture:?} for the {tag} side"))?;
    let env: Vec<(OsString, OsString)> = [
        ("XDG_CONFIG_HOME", xdg_config_home.as_os_str()),
        ("XDG_DATA_HOME", side_dir.join("xdg_data_home").as_os_str()),
        (
            "XDG_STATE_HOME",
            side_dir.join("xdg_state_home").as_os_str(),
        ),
        (
            "XDG_CACHE_HOME",
            side_dir.join("xdg_cache_home").as_os_str(),
        ),
        // every committed fixture's own init.lua calls
        // `vim.fn.serverstart(vim.env.VIEW_COMPAT_SOCK)` unconditionally
        // (compat/fixtures/*/nvim/init.lua), not only under the compat
        // harness, so a side missing this fails nvim's own startup with
        // `E474: Invalid argument` rather than measuring anything
        ("VIEW_COMPAT_SOCK", side_dir.join("compat.sock").as_os_str()),
        ("TERM", "xterm-256color".as_ref()),
        ("COLORTERM", "truecolor".as_ref()),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), v.to_os_string()))
    .collect();
    let scratch_file = side_dir.join("scratch.txt");
    Ok(Side {
        env,
        cwd: side_dir,
        scratch_file,
    })
}

/// The "unspeculated equivalent" arithmetic the pitch table already
/// stated: the honest round trip's own recorded p99 plus two hops of the
/// injected RTT, held against the `echo`/`view_p99_ms` bar. Two hops
/// because the honest path's own boundary is a full round trip -- a
/// keystroke leaving the terminal and a redraw coming back -- and an
/// injected one-way relay delay is paid once per direction.
fn unspeculated_equivalent_ms(honest_view_p99_ms: f64, rtt_ms: u64) -> f64 {
    honest_view_p99_ms + 2.0 * rtt_ms as f64
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let taps_view_bin = cli.taps_view_bin.unwrap_or_else(|| {
        workspace_root()
            .join("target")
            .join("taps")
            .join("release")
            .join("view")
    });
    if !taps_view_bin.is_file() {
        bail!(
            "taps view binary {} does not exist; build it first (`cargo build --release -p view \
             --features bench-taps --target-dir target/taps`) or pass --taps-view-bin",
            taps_view_bin.display()
        );
    }
    if let Some(reason) = echo_speculated_rtt::delay_relay_unavailable_reason() {
        bail!("{reason}; the RTT-injection proof cannot run without it");
    }

    let budget_file = budgets::load(&budgets_path())
        .with_context(|| format!("loading {}", budgets_path().display()))?;
    let speculated_max = budget_max(
        &budget_file,
        "echo_speculated",
        "speculated_ratio_p50",
        &cli.class,
    )?;
    let echo_bar_ms = budget_max(&budget_file, "echo", "view_p99_ms", &cli.class)?;

    let baseline_path = baseline_path(&cli.class);
    let baseline = baselines::load(&baseline_path)
        .with_context(|| format!("loading {}", baseline_path.display()))?;
    let honest = baseline.cell(&CellId::new("echo", &cli.fixture)).cloned();
    let honest_view_p99_ms = honest.as_ref().and_then(|m| m.get("view_p99_ms")).copied();
    let honest_ratio_p50 = honest.as_ref().and_then(|m| m.get("ratio_p50")).copied();

    let root = scratch_root("rtt-acceptance");
    let _ = std::fs::remove_dir_all(&root);
    let root = ScratchRoot(root);

    let settle_deadline = if cli.fixture == "heavy" {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(30)
    };

    let tiers: Vec<u64> = cli.tiers.clone().unwrap_or_else(|| RTT_TIERS_MS.to_vec());

    let cat = echo_speculated_rtt::cat_path()
        .with_context(|| "no cat binary found to probe the delay relay's own round trip")?;
    // Calibrated once, against RTT 0ms specifically, regardless of whether
    // 0 is itself in `tiers`: the one stable reference every other tier's
    // expected round trip is built from below.
    let floor_ms = median_probe(cat, 0, PROBE_TRIALS)
        .with_context(|| "calibrating the delay relay's own round-trip floor at RTT 0ms")?
        .as_secs_f64()
        * 1000.0;

    // Lower-bound slack for the per-tier floor check below: generous
    // enough that a genuinely working relay's own median-of-PROBE_TRIALS
    // noise (measured spread ~8-10ms at N=11, see PROBE_TRIALS) never
    // trips it, comfortably tight enough that a relay which stopped
    // injecting delay -- whose every tier's actual round trip stays near
    // `floor_ms` regardless of `rtt_ms` -- still falls short of even the
    // smallest advertised tier gap's expected floor by a wide margin: at
    // the smallest default/advertised gap (RTT 25ms, a 50ms round-trip
    // separation from the floor since one probe crosses the relay twice),
    // a broken relay's expected-vs-floor shortfall is `50 - 20 = 30ms`,
    // roughly 3-4x that measured noise spread.
    const FLOOR_SLACK_MS: f64 = 20.0;

    let mut all_pass = true;
    for &rtt_ms in &tiers {
        // one sample's wait crosses the relay twice (input notify out,
        // redraw back), so the default 5s budget -- sized for a local,
        // near-zero-delay round trip -- is widened by a multiple of the
        // tier's own injected delay; a genuine desync (the failure mode
        // this bound exists to catch) still fails well inside the widened
        // window, since it never depends on the relay's sleep at all
        let sample_timeout = Duration::from_secs(5).max(Duration::from_millis(rtt_ms * 20));
        let protocol = Protocol {
            samples: cli.samples,
            warmup: cli.warmup,
            trials: 1,
            sample_timeout,
            ..Protocol::default()
        };
        // the pre-attach shell frame paints once and then holds bit-for-bit
        // static until the real engine attach (register_vim_enter_autocmd,
        // register_bridge, ui_attach -- each its own RPC round trip)
        // replaces it; every one of those round trips pays this tier's
        // relay delay twice. A quiet span sized for a local attach (this
        // scenario's own `DEFAULT_STARTUP_QUIET`) is satisfied by that
        // static frame well before attach's real content ever lands at
        // 100ms+ RTT, so `prepare`'s first settle declares the spawn ready
        // while it is still the placeholder -- the probe types into a
        // session nothing has attached to yet, and the keystrokes queue in
        // the pre-attach drain buffer instead of landing where the probe
        // is watching. Scaled by the same multiplier as `sample_timeout`
        // above so real attach traffic landing before the span elapses
        // resets the quiet clock instead of racing it.
        let startup_quiet = DEFAULT_STARTUP_QUIET.max(Duration::from_millis(rtt_ms * 20));

        // Direct probe of the relay itself, decoupled from anything
        // `echo_speculated::run` measures below: `gated_ratio_p50` and
        // `gated_view_p99_ms` are both -- by design -- largely insensitive
        // to transport RTT once speculation is doing its job, so neither
        // can tell "the relay is genuinely fast at this tier" apart from
        // "the relay stopped injecting delay" (confirmed by mutation
        // testing both against a relay hardcoded to skip its `sleep`: the
        // scenario's own metrics kept printing OK). This probe wraps `cat`
        // through the same relay binary at the same configured `rtt_ms`,
        // outside the scenario entirely, so its round trip has nothing to
        // hide behind.
        //
        // Checked against `floor_ms` (an absolute, once-calibrated
        // reference), not against the *previous* tier's own probe: an
        // earlier version compared each tier's median only to its
        // immediate predecessor's, which is a coin flip a broken relay
        // wins roughly half the time no matter how large that predecessor
        // gap is, or how many trials each median is built from -- when the
        // relay is broken, every tier's actual round trip is an
        // independent draw from the *same* flat floor distribution, so two
        // such draws are equally likely to come back either order,
        // regardless of sample count (mutation-tested directly: 5 of 10
        // `--tiers 0,25` runs against a broken relay still falsely passed
        // under the pairwise design, even at PROBE_TRIALS=11). Comparing
        // against a fixed target instead breaks that symmetry: a broken
        // relay's actual value stays near `floor_ms` for every tier, while
        // the expected value grows with `rtt_ms`, so the shortfall widens
        // rather than staying a coin flip.
        let expected_min_ms = floor_ms + 2.0 * rtt_ms as f64 - FLOOR_SLACK_MS;
        let probe_elapsed = median_probe(cat, rtt_ms, PROBE_TRIALS)?;
        let probe_ms = probe_elapsed.as_secs_f64() * 1000.0;
        if probe_ms < expected_min_ms {
            bail!(
                "RTT {rtt_ms}ms tier's direct relay-probe round trip {probe_ms:.2}ms fell short of \
                 the {expected_min_ms:.2}ms this tier's configured delay requires (calibrated \
                 floor {floor_ms:.2}ms + 2x{rtt_ms}ms round trip - {FLOOR_SLACK_MS}ms slack); the \
                 relay may have stopped injecting the configured delay"
            );
        }

        let tier_root = root.join(format!("tier-{rtt_ms}"));
        let view_side = side_setup(&cli.fixture, "view", &tier_root)?;
        let nvim_side = side_setup(&cli.fixture, "nvim", &tier_root)?;
        let tap_path = view_side.cwd.join("tap.fifo");
        let pipe = TapPipe::create(&tap_path)
            .with_context(|| format!("creating the tap FIFO for RTT {rtt_ms}ms"))?;
        let view_spec = echo_speculated_rtt::remote_rtt_view_spec(
            view_side.cwd,
            view_side.env,
            &taps_view_bin,
            &cli.nvim_bin,
            &view_side.scratch_file,
            &tap_path,
            rtt_ms,
        )
        .with_context(|| format!("arming the RTT {rtt_ms}ms view spawn"))?;
        let nvim_spec = SpawnSpec {
            program: cli.nvim_bin.clone(),
            args: vec![nvim_side.scratch_file.into_os_string()],
            env: nvim_side.env,
            cwd: Some(nvim_side.cwd),
        };

        let tier_start = Instant::now();
        let outcome = echo_speculated::run(
            ViewSpec(&view_spec),
            NvimSpec(&nvim_spec),
            &pipe,
            &protocol,
            settle_deadline,
            startup_quiet,
        )
        .with_context(|| format!("echo_speculated at RTT {rtt_ms}ms failed"))?;
        let tier_wall = tier_start.elapsed();
        if let Some(reason) = outcome.refusal() {
            bail!("RTT {rtt_ms}ms: {reason}");
        }

        // `gated_view_p99_ms` is printed for visibility, but not asserted
        // on: it is the scenario's *speculated* view-side p99, which
        // speculation is specifically designed to keep close to flat
        // across RTT tiers -- confirmed by running it against a genuinely
        // working relay (no mutation) at RTT 0ms and 300ms, where it came
        // back lower at the higher tier. A metric that fails on a correct
        // relay is worse than one that never fires: the refusal above
        // (`probe_elapsed`, measured outside the scenario against the same
        // relay) is the falsifiable signal instead.
        let tier_view_p99_ms = outcome.echo.gated_view_p99_ms;

        let ratio = outcome.echo.gated_ratio_p50;
        let ok = ratio <= speculated_max;
        all_pass &= ok;
        let verdict = if ok { "OK" } else { "FAIL" };

        if rtt_ms == 0 {
            let honest_column = match honest_ratio_p50 {
                Some(honest) => format!("echo.minimal (honest, unspeculated) {honest:.2}"),
                None => format!(
                    "echo.minimal (honest, unspeculated) unrecorded on class {}",
                    cli.class
                ),
            };
            println!("RTT {rtt_ms:>3}ms | echo_speculated ratio_p50 {ratio:.2}  | {honest_column}  probe {probe_elapsed:?} view_p99 {tier_view_p99_ms:.2}ms (wall {tier_wall:?})  {verdict}");
        } else {
            let equivalent_column = match honest_view_p99_ms {
                Some(honest_ms) => {
                    let equivalent_ms = unspeculated_equivalent_ms(honest_ms, rtt_ms);
                    let multiple = equivalent_ms / echo_bar_ms;
                    format!("unspeculated equivalent would be ~{multiple:.0}x over the {echo_bar_ms:.1}ms bar")
                }
                None => format!("unspeculated equivalent unrecorded on class {}", cli.class),
            };
            println!("RTT {rtt_ms:>3}ms | echo_speculated ratio_p50 {ratio:.2}  | {equivalent_column}  probe {probe_elapsed:?} view_p99 {tier_view_p99_ms:.2}ms (wall {tier_wall:?})  {verdict}");
        }
    }

    if all_pass {
        println!(
            "PASS: every RTT tier's speculated_ratio_p50 stayed under the {speculated_max:.2} budget"
        );
        Ok(())
    } else {
        bail!(
            "one or more RTT tiers exceeded the speculated_ratio_p50 budget of \
             {speculated_max:.2}"
        );
    }
}
