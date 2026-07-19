//! `bench`: the scenario-by-fixture measurement matrix under the design
//! spec's measurement protocol -- paired view/nvim runs against pinned
//! fixture configs, per-machine-class baselines, and a regression gate.
//!
//! ```text
//! bench --scenario echo --fixture minimal --class dev-linux
//! bench --all --class dev-linux --record   # writes baselines/<class>.toml
//! bench --all --class dev-linux --gate     # exit 1 on any breach
//! ```
//!
//! The machine class is a required argument: shared-runner numbers must
//! never silently gate as if they came from a dedicated box, so the
//! harness refuses to run at all without an explicit class naming where
//! the numbers came from.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use view_bench::report;
use view_bench::scenarios::{echo, first_paint, flood, memory, scroll, taps, Protocol};
use view_bench::session::SpawnSpec;
use view_harness::baselines::{self, BaselineFile, CellMetrics};
use view_harness::fixture::{
    cache_root, copy_dir_recursive, current_engine_pin, fixtures_root, lockfile_cache_key,
    verify_nvim_matches_pin, workspace_root,
};

/// Every measurable cell of the matrix, in run order.
const MATRIX: &[(&str, &str)] = &[
    ("echo", "minimal"),
    ("echo", "heavy"),
    ("scroll", "minimal"),
    ("scroll", "heavy"),
    ("first_paint", "minimal"),
    ("first_paint", "heavy"),
    ("memory", "minimal"),
    ("flood", "minimal"),
    ("input_path", "minimal"),
    ("output_path", "minimal"),
];

/// Ceiling on the measured tap-operation p99 before the taps rows are
/// allowed to run at all: 5% of the input-path row's 100 microsecond
/// budget. Above this, the instrumentation itself would materially
/// distort the number it measures.
const TAP_OVERHEAD_BAR_US: f64 = 5.0;

/// Minimum sampling discipline for a number that may be recorded or
/// gated, per the measurement protocol; ad hoc smaller runs are allowed
/// only for report-only invocations.
const MIN_RECORDED_SAMPLES: usize = 1000;
const MIN_RECORDED_WARMUP: usize = 100;

/// Lines one `:terminal` flood produces. Sized from measurement, not
/// guessed: a 200k-line flood drained in ~850ms with only ~70 observed
/// frame changes (the UI coalesces to roughly a 12ms cadence), so the
/// cadence percentile needs a drain long enough for 1000+ painted
/// frames.
const FLOOD_LINES: usize = 3_000_000;

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Parser)]
#[command(
    name = "bench",
    about = "Paired view/nvim measurement matrix with per-machine-class baselines"
)]
struct Cli {
    /// Scenario to run (echo, ...); required unless --all
    #[arg(long, conflicts_with = "all")]
    scenario: Option<String>,
    /// Fixture to run the scenario against (minimal, heavy)
    #[arg(long, conflicts_with = "all")]
    fixture: Option<String>,
    /// Run every cell of the matrix
    #[arg(long)]
    all: bool,
    /// Machine class the numbers belong to (e.g. dev-linux); baselines
    /// are stored and gated per class
    #[arg(long)]
    class: String,
    /// Record measured values into baselines/<class>.toml
    #[arg(long, conflicts_with = "gate")]
    record: bool,
    /// Gate measured values against baselines/<class>.toml; exits 1 on
    /// any breach
    #[arg(long)]
    gate: bool,
    /// Path to the release view binary
    #[arg(long)]
    view_bin: Option<PathBuf>,
    /// Path to the nvim binary (must match .engine-pin)
    #[arg(long)]
    nvim_bin: Option<PathBuf>,
    /// Path to the bench-taps build of view (internal-boundary rows)
    #[arg(long)]
    taps_view_bin: Option<PathBuf>,
    /// Measured samples per side per trial
    #[arg(long, default_value_t = 1000)]
    samples: usize,
    /// Warmup samples per side per trial (excluded from statistics)
    #[arg(long, default_value_t = 100)]
    warmup: usize,
    /// Interleaved trials per cell; the gated statistic is the median
    #[arg(long, default_value_t = 3)]
    trials: usize,
}

/// Where one machine class's baselines live, inside the repo so recorded
/// bars are versioned alongside the code they measure.
fn baseline_path(class: &str) -> PathBuf {
    workspace_root()
        .join("crates")
        .join("view-bench")
        .join("baselines")
        .join(format!("{class}.toml"))
}

/// Hermetic scratch world for one cell run: per-side XDG homes, scratch
/// files, and sockets, removed on drop.
struct CellWorld {
    hermetic_dir: PathBuf,
}

impl Drop for CellWorld {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.hermetic_dir);
    }
}

/// One side's resolved spawn inputs.
struct SideSetup {
    env: Vec<(OsString, OsString)>,
    cwd: PathBuf,
    scratch_file: PathBuf,
}

impl CellWorld {
    fn create(fixture: &str) -> Result<Self> {
        let id = format!(
            "{}-{}",
            std::process::id(),
            SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let hermetic_dir = std::env::temp_dir().join(format!("view-bench-{id}"));
        std::fs::create_dir_all(&hermetic_dir)
            .with_context(|| format!("creating {}", hermetic_dir.display()))?;
        let world = Self { hermetic_dir };

        let fixture_dir = fixtures_root().join(fixture);
        if !fixture_dir.join("nvim").join("init.lua").exists() {
            bail!(
                "fixture {fixture:?} has no {}/nvim/init.lua",
                fixture_dir.display()
            );
        }
        Ok(world)
    }

    /// Resolves one side's hermetic environment: a private copy of the
    /// fixture config (a plugin manager may rewrite its own lockfile in
    /// place), private state/cache homes, and a data home pointed at the
    /// shared lockfile-keyed plugin cache so both sides (and the compat
    /// harness) reuse one plugin install instead of cloning per run.
    fn side(&self, fixture: &str, side_tag: &str) -> Result<SideSetup> {
        let side_dir = self.hermetic_dir.join(side_tag);
        std::fs::create_dir_all(&side_dir)
            .with_context(|| format!("creating {}", side_dir.display()))?;
        let fixture_dir = fixtures_root().join(fixture);

        let xdg_config_home = side_dir.join("xdg_config_home");
        copy_dir_recursive(&fixture_dir, &xdg_config_home)
            .with_context(|| format!("copying fixture {fixture:?} for the {side_tag} side"))?;

        let lockfile_path = fixture_dir.join("nvim").join("lazy-lock.json");
        let xdg_data_home = if lockfile_path.exists() {
            let bytes = std::fs::read(&lockfile_path)
                .with_context(|| format!("reading {}", lockfile_path.display()))?;
            cache_root().join(lockfile_cache_key(&bytes))
        } else {
            side_dir.join("xdg_data_home")
        };

        let sock = side_dir.join("compat.sock");
        let env: Vec<(OsString, OsString)> = [
            ("XDG_CONFIG_HOME", xdg_config_home.as_os_str()),
            ("XDG_DATA_HOME", xdg_data_home.as_os_str()),
            (
                "XDG_STATE_HOME",
                side_dir.join("xdg_state_home").as_os_str(),
            ),
            (
                "XDG_CACHE_HOME",
                side_dir.join("xdg_cache_home").as_os_str(),
            ),
            ("VIEW_COMPAT_SOCK", sock.as_os_str()),
            ("TERM", "xterm-256color".as_ref()),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v.to_os_string()))
        .collect();

        Ok(SideSetup {
            env,
            cwd: side_dir.clone(),
            scratch_file: side_dir.join("scratch.txt"),
        })
    }
}

/// Settle bound before sampling starts: the heavy fixture's first-ever
/// run may clone plugins into the shared cache, which dwarfs any paint
/// settle; a warm cache settles in a couple of seconds.
fn settle_deadline(fixture: &str) -> Duration {
    if fixture == "heavy" {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(30)
    }
}

/// Builds the view-side spawn spec against one resolved side setup; the
/// engine binary is always passed explicitly so both halves of a pair
/// exercise the same pin-verified nvim.
fn view_spec_from(side: SideSetup, view_bin: &Path, nvim_bin: &Path) -> SpawnSpec {
    SpawnSpec {
        program: view_bin.to_path_buf(),
        args: vec![
            side.scratch_file.into_os_string(),
            OsString::from("--nvim-bin"),
            nvim_bin.as_os_str().to_os_string(),
        ],
        env: side.env,
        cwd: Some(side.cwd),
    }
}

/// Builds the paired view/nvim spawn specs for one cell.
fn paired_specs(
    world: &CellWorld,
    fixture: &str,
    view_bin: &Path,
    nvim_bin: &Path,
) -> Result<(SpawnSpec, SpawnSpec)> {
    let view_side = world.side(fixture, "view")?;
    let nvim_side = world.side(fixture, "nvim")?;
    let view_spec = view_spec_from(view_side, view_bin, nvim_bin);
    let nvim_spec = SpawnSpec {
        program: nvim_bin.to_path_buf(),
        args: vec![nvim_side.scratch_file.clone().into_os_string()],
        env: nvim_side.env,
        cwd: Some(nvim_side.cwd),
    };
    Ok((view_spec, nvim_spec))
}

/// The resolved binaries one invocation measures with.
struct Bins {
    view: PathBuf,
    /// The bench-taps build; only required (and existence-checked) when a
    /// taps row actually runs.
    taps_view: PathBuf,
    nvim: PathBuf,
}

/// Wraps a view spawn in a `sh` shim that opens the tap FIFO at a fixed
/// descriptor before exec'ing the real binary. The pty spawn path closes
/// every inherited fd above stdio in the child, so the descriptor
/// `VIEW_BENCH_TAP_FD` names must be (re)opened after that point; the
/// shell's own `exec 9>` runs post-exec, exactly late enough.
fn shim_taps_spec(inner: SpawnSpec, tap_path: &Path) -> SpawnSpec {
    let mut args = vec![
        OsString::from("-c"),
        OsString::from("exec 9>\"$VIEW_BENCH_TAP_PATH\"; exec \"$0\" \"$@\""),
        inner.program.into_os_string(),
    ];
    args.extend(inner.args);
    let mut env = inner.env;
    env.push((
        OsString::from("VIEW_BENCH_TAP_PATH"),
        tap_path.as_os_str().to_os_string(),
    ));
    env.push((OsString::from("VIEW_BENCH_TAP_FD"), OsString::from("9")));
    SpawnSpec {
        program: PathBuf::from("sh"),
        args,
        env,
        cwd: inner.cwd,
    }
}

/// Runs one taps row (`input_path` or `output_path`) end to end:
/// characterizes tap overhead first and refuses to measure through taps
/// that would distort the row's own budget.
fn run_taps_row(
    scenario: &str,
    fixture: &str,
    world: &CellWorld,
    bins: &Bins,
    protocol: &Protocol,
) -> Result<CellMetrics> {
    if !bins.taps_view.exists() {
        bail!(
            "taps view binary {} does not exist; run via `task bench` (which builds it) or pass              --taps-view-bin",
            bins.taps_view.display()
        );
    }
    let side = world.side(fixture, "view")?;
    let tap_path = side.cwd.join("tap.fifo");
    let pipe = taps::TapPipe::create(&tap_path)?;

    let overhead = taps::characterize_overhead(&tap_path, 100_000)?;
    println!(
        "      tap overhead p50 {:.3}us p99 {:.3}us over {} iterations (bar {TAP_OVERHEAD_BAR_US}us)",
        overhead.p50(),
        overhead.p99(),
        overhead.len()
    );
    if overhead.p99() > TAP_OVERHEAD_BAR_US {
        bail!(
            "measured tap overhead p99 {:.3}us exceeds {TAP_OVERHEAD_BAR_US}us (5% of the              input-path budget); the tap design must change before this row can be trusted",
            overhead.p99()
        );
    }

    let spec = shim_taps_spec(view_spec_from(side, &bins.taps_view, &bins.nvim), &tap_path);
    let deadline = settle_deadline(fixture);
    let (outcome, metric_key, unit) = match scenario {
        "input_path" => (
            taps::run_input_path(&spec, &pipe, protocol, deadline)
                .with_context(|| format!("input_path/{fixture} run failed"))?,
            "p99_us",
            "us",
        ),
        _ => (
            taps::run_output_path(&spec, &pipe, protocol, deadline)
                .with_context(|| format!("output_path/{fixture} run failed"))?,
            "p99_ms",
            "ms",
        ),
    };
    for dist in &outcome.trial_distributions {
        println!(
            "{}",
            report::absolute_cell(
                scenario,
                fixture,
                "boundary-delta",
                report::AbsoluteStats {
                    p50: dist.p50(),
                    p99: dist.p99(),
                    unit,
                    samples: dist.len(),
                    warmup: protocol.warmup,
                }
            )
        );
    }
    println!(
        "{}",
        report::aggregate_line(metric_key, outcome.gated_p99, protocol.trials)
    );
    let mut metrics = CellMetrics::new();
    metrics.insert(metric_key.to_string(), outcome.gated_p99);
    Ok(metrics)
}

/// Runs one matrix cell and returns the metrics the baseline records for
/// it.
fn run_cell(
    scenario: &str,
    fixture: &str,
    bins: &Bins,
    protocol: &Protocol,
) -> Result<CellMetrics> {
    let view_bin = bins.view.as_path();
    let nvim_bin = bins.nvim.as_path();
    let world = CellWorld::create(fixture)?;
    match scenario {
        "input_path" | "output_path" => run_taps_row(scenario, fixture, &world, bins, protocol),
        "echo" => {
            let (view_spec, nvim_spec) = paired_specs(&world, fixture, view_bin, nvim_bin)?;
            let outcome = echo::run(&view_spec, &nvim_spec, protocol, settle_deadline(fixture))
                .with_context(|| format!("echo/{fixture} run failed"))?;
            for summary in &outcome.trials {
                println!(
                    "{}",
                    report::paired_cell(scenario, fixture, summary, protocol.warmup)
                );
            }
            println!(
                "{}",
                report::aggregate_line("ratio_p50", outcome.gated_ratio_p50, outcome.trials.len())
            );
            println!(
                "{}",
                report::aggregate_line("ratio_p99", outcome.gated_ratio_p99, outcome.trials.len())
            );
            println!(
                "{}",
                report::aggregate_line(
                    "paired_delta_p99_ms",
                    outcome.gated_paired_delta_p99_ms,
                    outcome.trials.len()
                )
            );
            let mut metrics = CellMetrics::new();
            metrics.insert("ratio_p50".to_string(), outcome.gated_ratio_p50);
            metrics.insert("ratio_p99".to_string(), outcome.gated_ratio_p99);
            metrics.insert(
                "paired_delta_p99_ms".to_string(),
                outcome.gated_paired_delta_p99_ms,
            );
            Ok(metrics)
        }
        "scroll" => {
            let (mut view_spec, mut nvim_spec) = paired_specs(&world, fixture, view_bin, nvim_bin)?;
            for spec in [&mut view_spec, &mut nvim_spec] {
                let file = spec
                    .args
                    .first()
                    .map(PathBuf::from)
                    .context("scroll spec has no file argument")?;
                std::fs::write(&file, scroll::fixture_content())
                    .with_context(|| format!("writing scroll fixture {}", file.display()))?;
            }
            let outcome = scroll::run(&view_spec, &nvim_spec, protocol, settle_deadline(fixture))
                .with_context(|| format!("scroll/{fixture} run failed"))?;
            for summary in &outcome.trials {
                println!(
                    "{}",
                    report::paired_cell(scenario, fixture, summary, protocol.warmup)
                );
            }
            println!(
                "{}",
                report::aggregate_line(
                    "staleness_p99_ms",
                    outcome.gated_staleness_p99_ms,
                    outcome.trials.len()
                )
            );
            println!(
                "{}",
                report::aggregate_line("ratio_p50", outcome.gated_ratio_p50, outcome.trials.len())
            );
            println!(
                "{}",
                report::aggregate_line("ratio_p99", outcome.gated_ratio_p99, outcome.trials.len())
            );
            let mut metrics = CellMetrics::new();
            metrics.insert(
                "staleness_p99_ms".to_string(),
                outcome.gated_staleness_p99_ms,
            );
            metrics.insert("ratio_p50".to_string(), outcome.gated_ratio_p50);
            metrics.insert("ratio_p99".to_string(), outcome.gated_ratio_p99);
            Ok(metrics)
        }
        "first_paint" => {
            let (view_spec, nvim_spec) = paired_specs(&world, fixture, view_bin, nvim_bin)?;
            let outcome = first_paint::run(&view_spec, &nvim_spec, protocol)
                .with_context(|| format!("first_paint/{fixture} run failed"))?;
            println!(
                "{}",
                report::paired_cell(scenario, fixture, &outcome.summary, protocol.warmup)
            );
            println!(
                "{}",
                report::aggregate_line("cold_ms", outcome.gated_cold_ms, 1)
            );
            println!(
                "{}",
                report::aggregate_line("ratio_vs_nvim", outcome.gated_ratio_vs_nvim, 1)
            );
            verify_fixture_copies_untouched(&world, fixture)?;
            let mut metrics = CellMetrics::new();
            metrics.insert("cold_ms".to_string(), outcome.gated_cold_ms);
            metrics.insert("ratio_vs_nvim".to_string(), outcome.gated_ratio_vs_nvim);
            Ok(metrics)
        }
        "memory" => {
            let side = world.side(fixture, "view")?;
            for (index, name) in memory::workload_files().iter().enumerate() {
                std::fs::write(side.cwd.join(name), memory::workload_content(index + 1))
                    .with_context(|| format!("writing workload buffer {name}"))?;
            }
            let view_spec = view_spec_from(side, view_bin, nvim_bin);
            let outcome = memory::run(&view_spec, protocol)
                .with_context(|| format!("memory/{fixture} run failed"))?;
            println!(
                "{}",
                report::absolute_cell(
                    scenario,
                    fixture,
                    "view-pss",
                    report::AbsoluteStats {
                        p50: outcome.distribution.p50(),
                        p99: outcome.gated_pss_mb,
                        unit: "MB",
                        samples: outcome.distribution.len(),
                        warmup: protocol.warmup,
                    }
                )
            );
            println!(
                "{}",
                report::aggregate_line("pss_mb", outcome.gated_pss_mb, 1)
            );
            let mut metrics = CellMetrics::new();
            metrics.insert("pss_mb".to_string(), outcome.gated_pss_mb);
            Ok(metrics)
        }
        "flood" => {
            let (view_spec, nvim_spec) = paired_specs(&world, fixture, view_bin, nvim_bin)?;
            let outcome = flood::run(
                &view_spec,
                &nvim_spec,
                protocol.trials,
                protocol.samples,
                settle_deadline(fixture),
                FLOOD_LINES,
            )
            .with_context(|| format!("flood/{fixture} run failed"))?;
            for (view_side, nvim_side) in outcome.view_trials.iter().zip(&outcome.nvim_trials) {
                println!(
                    "flood/{fixture}: view drain {:.0}ms ({} frame gaps) | nvim drain {:.0}ms  \
                     ratio {:.2}",
                    view_side.drain_ms,
                    view_side.cadence_gaps_ms.len(),
                    nvim_side.drain_ms,
                    view_side.drain_ms / nvim_side.drain_ms
                );
            }
            println!(
                "{}",
                report::aggregate_line("drain_ratio", outcome.gated_drain_ratio, protocol.trials)
            );
            println!(
                "{}",
                report::aggregate_line(
                    "cadence_p99_ms",
                    outcome.gated_cadence_p99_ms,
                    protocol.trials
                )
            );
            println!(
                "      view worst no-paint gap {:.2}ms (reported, not gated)",
                outcome.view_stall_max_ms
            );
            let mut metrics = CellMetrics::new();
            metrics.insert("drain_ratio".to_string(), outcome.gated_drain_ratio);
            metrics.insert("cadence_p99_ms".to_string(), outcome.gated_cadence_p99_ms);
            Ok(metrics)
        }
        other => bail!(
            "unknown scenario {other:?}; known: {}",
            known_scenarios().join(", ")
        ),
    }
}

/// Confirms the per-side fixture config copies still match the committed
/// fixture byte for byte after a run that reuses one copy across many
/// cold spawns: "untouched fixture" is part of the cold definition, and a
/// plugin manager rewriting its lockfile mid-run would silently change
/// what later samples measured.
fn verify_fixture_copies_untouched(world: &CellWorld, fixture: &str) -> Result<()> {
    let source = fixtures_root().join(fixture);
    for side_tag in ["view", "nvim"] {
        let copy = world.hermetic_dir.join(side_tag).join("xdg_config_home");
        for entry in walk_files(&source)? {
            let rel = entry
                .strip_prefix(&source)
                .context("fixture walk escaped its root")?;
            let original =
                std::fs::read(&entry).with_context(|| format!("reading {}", entry.display()))?;
            let copied = std::fs::read(copy.join(rel))
                .with_context(|| format!("reading {}", copy.join(rel).display()))?;
            if original != copied {
                bail!(
                    "fixture copy for the {side_tag} side diverged from {} during the run \
                     (cold samples after the change measured a different config)",
                    entry.display()
                );
            }
        }
    }
    Ok(())
}

/// Every regular file under `root`, recursively.
fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                stack.push(path);
            } else if entry.file_type()?.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn known_scenarios() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = MATRIX.iter().map(|(s, _)| *s).collect();
    names.dedup();
    names
}

fn resolve_view_bin(cli: &Cli) -> Result<PathBuf> {
    let path = cli
        .view_bin
        .clone()
        .unwrap_or_else(|| workspace_root().join("target").join("release").join("view"));
    if !path.exists() {
        bail!(
            "view binary {} does not exist; run via `task bench` (which builds it) or pass --view-bin",
            path.display()
        );
    }
    Ok(path)
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if (cli.record || cli.gate)
        && (cli.samples < MIN_RECORDED_SAMPLES || cli.warmup < MIN_RECORDED_WARMUP)
    {
        bail!(
            "--record/--gate require at least {MIN_RECORDED_SAMPLES} samples and \
             {MIN_RECORDED_WARMUP} warmup per side (got --samples {} --warmup {})",
            cli.samples,
            cli.warmup
        );
    }

    let pin = current_engine_pin()?;
    let nvim_bin = cli
        .nvim_bin
        .clone()
        .unwrap_or_else(|| PathBuf::from("nvim"));
    verify_nvim_matches_pin(&nvim_bin, &pin)?;
    let bins = Bins {
        view: resolve_view_bin(&cli)?,
        taps_view: cli.taps_view_bin.clone().unwrap_or_else(|| {
            workspace_root()
                .join("target")
                .join("taps")
                .join("release")
                .join("view")
        }),
        nvim: nvim_bin,
    };

    let cells: Vec<(String, String)> = if cli.all {
        MATRIX
            .iter()
            .map(|(s, f)| ((*s).to_string(), (*f).to_string()))
            .collect()
    } else {
        let scenario = cli
            .scenario
            .clone()
            .context("--scenario is required unless --all is given")?;
        let fixture = cli
            .fixture
            .clone()
            .context("--fixture is required unless --all is given")?;
        if !MATRIX
            .iter()
            .any(|(s, f)| *s == scenario.as_str() && *f == fixture.as_str())
        {
            bail!(
                "no matrix cell {scenario}/{fixture}; cells: {}",
                MATRIX
                    .iter()
                    .map(|(s, f)| format!("{s}/{f}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        vec![(scenario, fixture)]
    };

    let protocol = Protocol {
        samples: cli.samples,
        warmup: cli.warmup,
        trials: cli.trials,
        ..Protocol::default()
    };

    let mut measured: Vec<(String, String, CellMetrics)> = Vec::new();
    for (scenario, fixture) in &cells {
        let metrics = run_cell(scenario, fixture, &bins, &protocol)?;
        measured.push((scenario.clone(), fixture.clone(), metrics));
    }

    let path = baseline_path(&cli.class);
    if cli.record {
        let mut file = if cli.all || !path.exists() {
            // a full-matrix record rewrites the file from scratch under
            // the current pin; recorded-but-not-remeasured cells from an
            // older pin must not survive into a fresh full baseline
            BaselineFile::new(&cli.class, &pin)
        } else {
            let existing = baselines::load(&path)?;
            baselines::require_pin_match(&existing, &pin, &path)?;
            existing
        };
        for (scenario, fixture, metrics) in &measured {
            file.upsert_cell(scenario, fixture, metrics.clone());
        }
        baselines::save(&path, &file)?;
        println!(
            "recorded {} cell(s) into {}",
            measured.len(),
            path.display()
        );
    }

    if cli.gate {
        let file = baselines::load(&path).with_context(|| {
            format!("gating requires a recorded baseline at {}", path.display())
        })?;
        baselines::require_pin_match(&file, &pin, &path)?;
        let mut breaches = Vec::new();
        for (scenario, fixture, metrics) in &measured {
            let Some(recorded) = file.cell(scenario, fixture) else {
                bail!(
                    "{} has no [{scenario}.{fixture}] cell; record it before gating",
                    path.display()
                );
            };
            breaches.extend(baselines::gate_cell(scenario, fixture, metrics, recorded));
        }
        if breaches.is_empty() {
            println!("gate OK: {} cell(s) within recorded bars", measured.len());
        } else {
            for breach in &breaches {
                eprintln!("{breach}");
            }
            std::process::exit(1);
        }
    }

    Ok(())
}
