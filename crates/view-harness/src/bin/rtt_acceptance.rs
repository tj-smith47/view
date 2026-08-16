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
use std::path::PathBuf;
use std::time::Duration;

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
    if !echo_speculated_rtt::delay_relay_available() {
        bail!(
            "no delay-relay / stub-ssh client pair on this host (need {} and {}, both \
             executable); the RTT-injection proof cannot run without them",
            echo_speculated_rtt::delay_relay_client().display(),
            view_oracle::remote::stub_client().display()
        );
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

    let settle_deadline = if cli.fixture == "heavy" {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(30)
    };

    let mut all_pass = true;
    for &rtt_ms in &RTT_TIERS_MS {
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

        let outcome = echo_speculated::run(
            ViewSpec(&view_spec),
            NvimSpec(&nvim_spec),
            &pipe,
            &protocol,
            settle_deadline,
            startup_quiet,
        )
        .with_context(|| format!("echo_speculated at RTT {rtt_ms}ms failed"))?;
        if let Some(reason) = outcome.refusal() {
            bail!("RTT {rtt_ms}ms: {reason}");
        }

        let ratio = outcome.echo.gated_ratio_p50;
        let ok = ratio < speculated_max;
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
            println!("RTT {rtt_ms:>3}ms | echo_speculated ratio_p50 {ratio:.2}  | {honest_column}  {verdict}");
        } else {
            let equivalent_column = match honest_view_p99_ms {
                Some(honest_ms) => {
                    let equivalent_ms = unspeculated_equivalent_ms(honest_ms, rtt_ms);
                    let multiple = equivalent_ms / echo_bar_ms;
                    format!("unspeculated equivalent would be ~{multiple:.0}x over the {echo_bar_ms:.1}ms bar")
                }
                None => format!("unspeculated equivalent unrecorded on class {}", cli.class),
            };
            println!("RTT {rtt_ms:>3}ms | echo_speculated ratio_p50 {ratio:.2}  | {equivalent_column}  {verdict}");
        }
    }

    let _ = std::fs::remove_dir_all(&root);

    if all_pass {
        println!(
            "PASS: every RTT tier's speculated_ratio_p50 stayed under the {speculated_max} budget"
        );
        Ok(())
    } else {
        bail!("one or more RTT tiers exceeded the speculated_ratio_p50 budget of {speculated_max}");
    }
}
