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
use view_bench::scenarios::{echo, Protocol};
use view_bench::session::SpawnSpec;
use view_harness::baselines::{self, BaselineFile, CellMetrics};
use view_harness::fixture::{
    cache_root, copy_dir_recursive, current_engine_pin, fixtures_root, lockfile_cache_key,
    verify_nvim_matches_pin, workspace_root,
};

/// Every measurable cell of the matrix, in run order.
const MATRIX: &[(&str, &str)] = &[("echo", "minimal"), ("echo", "heavy")];

/// Minimum sampling discipline for a number that may be recorded or
/// gated, per the measurement protocol; ad hoc smaller runs are allowed
/// only for report-only invocations.
const MIN_RECORDED_SAMPLES: usize = 1000;
const MIN_RECORDED_WARMUP: usize = 100;

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

/// Builds the paired view/nvim spawn specs for one cell.
fn paired_specs(
    world: &CellWorld,
    fixture: &str,
    view_bin: &Path,
    nvim_bin: &Path,
) -> Result<(SpawnSpec, SpawnSpec)> {
    let view_side = world.side(fixture, "view")?;
    let nvim_side = world.side(fixture, "nvim")?;
    let view_spec = SpawnSpec {
        program: view_bin.to_path_buf(),
        args: vec![
            view_side.scratch_file.clone().into_os_string(),
            OsString::from("--nvim-bin"),
            nvim_bin.as_os_str().to_os_string(),
        ],
        env: view_side.env,
        cwd: Some(view_side.cwd),
    };
    let nvim_spec = SpawnSpec {
        program: nvim_bin.to_path_buf(),
        args: vec![nvim_side.scratch_file.clone().into_os_string()],
        env: nvim_side.env,
        cwd: Some(nvim_side.cwd),
    };
    Ok((view_spec, nvim_spec))
}

/// Runs one matrix cell and returns the metrics the baseline records for
/// it.
fn run_cell(
    scenario: &str,
    fixture: &str,
    view_bin: &Path,
    nvim_bin: &Path,
    protocol: &Protocol,
) -> Result<CellMetrics> {
    let world = CellWorld::create(fixture)?;
    match scenario {
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
            metrics.insert("ratio_p99".to_string(), outcome.gated_ratio_p99);
            metrics.insert(
                "paired_delta_p99_ms".to_string(),
                outcome.gated_paired_delta_p99_ms,
            );
            Ok(metrics)
        }
        other => bail!(
            "unknown scenario {other:?}; known: {}",
            known_scenarios().join(", ")
        ),
    }
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
    let view_bin = resolve_view_bin(&cli)?;

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
        let metrics = run_cell(scenario, fixture, &view_bin, &nvim_bin, &protocol)?;
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
