//! `bench`: the scenario-by-fixture measurement matrix under the design
//! spec's measurement protocol -- paired view/nvim runs against pinned
//! fixture configs, per-machine-class baselines, and a regression gate.
//!
//! ```text
//! bench --scenario echo --fixture minimal --class dev-linux
//! bench --all --class dev-linux --record   # writes baselines/<class>.toml
//! bench --all --class dev-linux --gate     # exit 1 on any breach
//! bench --all --class gh-linux --gate --bootstrap   # record instead if no baseline yet
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
#[cfg(unix)]
use view_bench::scenarios::echo_control;
use view_bench::scenarios::{echo, first_paint, flood, memory, scroll, Protocol};

// The internal-boundary and echo_path rows run through the unix-only tap
// channel; their driver code lives in a cfg-gated child module so it is
// absent as a unit off unix rather than gated line by line.
#[cfg(unix)]
#[path = "bench/taps_rows.rs"]
mod taps_rows;

#[path = "bench/rows.rs"]
mod rows;
use rows::run_cell;

use view_bench::session::SpawnSpec;
use view_harness::baselines::{self, CellMetrics};
use view_harness::budgets;
use view_harness::fixture::{
    cache_root, copy_dir_recursive, current_engine_pin, fixtures_root, lockfile_cache_key,
    verify_nvim_matches_pin, workspace_root,
};

/// Every measurable cell of the matrix, in run order.
const MATRIX: &[(&str, &str)] = &[
    ("echo", "minimal"),
    ("echo", "heavy"),
    ("echo_control", "minimal"),
    ("echo_control", "heavy"),
    ("scroll", "minimal"),
    ("scroll", "heavy"),
    ("first_paint", "minimal"),
    ("first_paint", "heavy"),
    ("memory", "minimal"),
    ("flood", "minimal"),
    ("input_path", "minimal"),
    ("output_path", "minimal"),
];

/// Cells that decompose a gated row instead of being one. They are
/// measured on demand, never selected by `--all`, and refused under
/// `--record`/`--gate`: a bar recorded from one would gate the
/// decomposition rather than the row it exists to explain, and the
/// decomposition runs the instrumented build, whose numbers are not the
/// quantity any recorded bar was taken from.
const DIAGNOSTIC_MATRIX: &[(&str, &str)] = &[("echo_path", "minimal"), ("echo_path", "heavy")];

/// Minimum sampling discipline for a number that may be recorded or
/// gated, per the measurement protocol; ad hoc smaller runs are allowed
/// only for report-only invocations.
const MIN_RECORDED_SAMPLES: usize = 1000;
const MIN_RECORDED_WARMUP: usize = 100;

/// How long one `:terminal` flood samples steady output. A duration, not a
/// line count: hosts drain a fixed line count at wildly different rates (a
/// 3M-line flood took ~12.6s on dev-linux but ~845ms on mbp, ~1020 vs ~12
/// observed frame changes), so a line count cannot make the cadence sample
/// count comparable across hosts, but a fixed window does by construction.
/// Sized from measurement: at the UI's ~12ms coalesced cadence a 12s window
/// yielded ~960 gaps on a loaded dev-linux, just under the 1000-gap floor;
/// 15s clears it with margin on both hosts (dev-linux ~1200, mbp ~1350).
const FLOOD_WINDOW: Duration = Duration::from_secs(15);

/// Null-pair calibration sampling: two instances of the pinned nvim
/// interleaved per-sample under the echo driver. 200 measured samples is
/// enough for a stable median (the gated statistic is a p50) while
/// keeping the calibration to seconds, not a full protocol run.
const NULL_CAL_SAMPLES: usize = 200;
const NULL_CAL_WARMUP: usize = 20;

/// Refusal floor for the null-pair calibration, applied to the median
/// ratio's symmetric deviation from 1.0 (`max(r, 1/r)`; a null pair at
/// 0.87 is exactly as noisy as one at 1.15). Pinned from measurement,
/// not guessed: six calibration runs on a quiet host measured ratios
/// 0.9357..1.0652 (deviations 1.013..1.069), so 1.15 doubles the worst
/// observed quiet excess while still refusing the ambient noise that
/// moved identical-pair medians past 1.14 under load.
const NULL_RATIO_FLOOR: f64 = 1.15;

static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Parent of every per-cell scratch world; why `target/` and not the
/// system temp dir is documented on [`view_harness::fixture::scratch_root`].
fn scratch_root() -> PathBuf {
    view_harness::fixture::scratch_root("bench-scratch")
}

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
    /// are stored and gated per class. The name must carry exactly one of
    /// linux, macos or windows as a hyphen-delimited segment, and it must
    /// be this host's own platform: rows are measured per-platform under
    /// per-platform metric names, so a class naming another platform is
    /// refused rather than gated against incomparable numbers. A
    /// "controlled-" name prefix opts the class into tail-metric gating
    /// (ratio_p99, paired-delta p99); any other name records tails without
    /// gating them
    #[arg(long)]
    class: String,
    /// Record measured values into baselines/<class>.toml
    #[arg(long, conflicts_with = "gate")]
    record: bool,
    /// Gate measured values against baselines/<class>.toml; exits 1 on
    /// any breach
    #[arg(long)]
    gate: bool,
    /// With --gate: when no baseline exists for this class yet, record
    /// one instead of failing the provenance rule. Intended for a class's
    /// first CI run, which uploads the recorded TOML for review and
    /// commit; once the baseline is committed the flag comes back out
    #[arg(long, requires = "gate")]
    bootstrap: bool,
    /// Path to the release view binary
    #[arg(long)]
    view_bin: Option<PathBuf>,
    /// Path to the nvim binary (must match .engine-pin)
    #[arg(long)]
    nvim_bin: Option<PathBuf>,
    /// Path to the bench-taps build of view (internal-boundary rows)
    #[cfg(unix)]
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

/// Where the spec 3.1 budget table lives. One file for every class: a
/// budget is a property of the spec, not of the machine that measures it,
/// and the entries that do vary by class say so in their own `classes`
/// field rather than by living in separate files.
fn budgets_path() -> PathBuf {
    workspace_root()
        .join("crates")
        .join("view-bench")
        .join("budgets.toml")
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
        let hermetic_dir = scratch_root().join(format!("view-bench-{id}"));
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
    ///
    /// The four directories are all this resolves. Every environment
    /// variable that redirects an editor's configuration from outside them
    /// (`$NVIM_APPNAME` voids the config directory below even after it is
    /// pointed at the fixture, `$VIMINIT` runs host commands inside the
    /// measured process) is dropped by `PtySession::spawn_configured`,
    /// which every spawn on both sides of a pair goes through.
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
            // the only input to view's truecolor bit, and `Tier::Full` --
            // the tier the budget rows name -- requires it, so a session
            // without it measures a child that never reached the stated
            // condition. Today only the sync bit changes emitted bytes, so
            // this costs nothing measurable; it is set now because the
            // alternative is a bench that starts measuring the cheap path
            // silently on the day theming consumes the bit. Set on both
            // sides of a pair, because the two arms must face one terminal
            ("COLORTERM", "truecolor".as_ref()),
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
///
/// Nothing here strips the measured editor down: the fixture config is the
/// subject of the measurement, and `view` spawns its engine through
/// `EngineConfig::default` precisely so that config survives into it. An
/// argument such as `--clean` added on either side, or an isolated engine
/// config swapped in below `view`, would measure a plugin-free editor
/// against baselines recorded with the fixture's full plugin set, report it
/// as a large improvement, and gate green.
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

/// Builds a bare-nvim spawn spec against one resolved side setup.
fn nvim_spec_from(side: SideSetup, nvim_bin: &Path) -> SpawnSpec {
    SpawnSpec {
        program: nvim_bin.to_path_buf(),
        args: vec![side.scratch_file.into_os_string()],
        env: side.env,
        cwd: Some(side.cwd),
    }
}

/// Builds the paired view/nvim spawn specs for one cell.
/// Buffer content the first-paint boundary waits for.
///
/// Both sides open the same scratch path, so this is the one event both
/// can be timed to: the editor showing the file. Without it the boundary
/// is "any cell has ink", which view satisfies with its own pre-attach
/// placeholder chrome and bare nvim satisfies with its empty-buffer
/// window -- two different events, timed as if they were one, and a view
/// that stopped attaching its engine at all would still record a healthy
/// number.
///
/// Deliberately not a word an editor's own chrome could ever print.
const FIRST_PAINT_MARKER: &str = "VIEWBENCHCOLDSTARTMARKER";

/// How many buffer lines carry the marker.
///
/// One line is not enough, and the failure was silent in the worst
/// direction: a plugin-heavy config raises floating notifications that
/// overlay the top rows, and the boundary check needs the whole marker
/// contiguous on one row. Observed on the heavy fixture -- three stacked
/// `nvim-notify` popups began at column 20 and clipped the marker to
/// `VIEWBENCHCOLDSTARTM`, so the row did not match until the toasts faded
/// about seven seconds later. That recorded view at 7133 ms against bare
/// nvim's 225 ms and read as a 31x cold-start regression; view had in fact
/// painted the buffer immediately. Bare nvim never showed the artifact
/// because its messages go to the command line, which sits below the
/// buffer rather than over it.
///
/// Filling well past the terminal's row count means an overlay would have
/// to cover every visible row to hide the marker -- which is a genuinely
/// unpainted buffer, exactly what this boundary should refuse to time.
const FIRST_PAINT_MARKER_LINES: usize = 60;

/// Writes [`FIRST_PAINT_MARKER`] into the scratch file `spec` opens, on
/// [`FIRST_PAINT_MARKER_LINES`] lines so it lands in the first painted
/// frame on a row no corner overlay can reach.
fn plant_first_paint_marker(spec: &SpawnSpec) -> Result<()> {
    let target = spec
        .args
        .first()
        .map(PathBuf::from)
        .context("a paired spawn spec must open the scratch file as its first argument")?;
    let body = std::iter::repeat_n(FIRST_PAINT_MARKER, FIRST_PAINT_MARKER_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&target, format!("{body}\n"))
        .with_context(|| format!("planting the first-paint marker in {}", target.display()))?;
    Ok(())
}

fn paired_specs(
    world: &CellWorld,
    fixture: &str,
    view_bin: &Path,
    nvim_bin: &Path,
) -> Result<(SpawnSpec, SpawnSpec)> {
    let view_side = world.side(fixture, "view")?;
    let nvim_side = world.side(fixture, "nvim")?;
    let view_spec = view_spec_from(view_side, view_bin, nvim_bin);
    let nvim_spec = nvim_spec_from(nvim_side, nvim_bin);
    Ok((view_spec, nvim_spec))
}

/// Measures the host's ambient pairing noise before any cell is recorded
/// or gated: the pinned nvim interleaved against itself under the echo
/// driver. A null pair's median ratio is 1.0 by construction, so its
/// deviation from 1.0 is a direct read of exactly the noise every real
/// gated ratio is exposed to on this host, measured by the same
/// machinery.
fn null_calibration(bins: &Bins) -> Result<f64> {
    let world = CellWorld::create("minimal")?;
    let spec_a = nvim_spec_from(world.side("minimal", "null-a")?, &bins.nvim);
    let spec_b = nvim_spec_from(world.side("minimal", "null-b")?, &bins.nvim);
    let protocol = Protocol {
        samples: NULL_CAL_SAMPLES,
        warmup: NULL_CAL_WARMUP,
        trials: 1,
        ..Protocol::default()
    };
    let outcome = echo::run(&spec_a, &spec_b, &protocol, settle_deadline("minimal"))
        .context("null-pair calibration run failed")?;
    Ok(outcome.gated_ratio_p50)
}

/// Symmetric deviation of a null-pair ratio from 1.0 (`max(r, 1/r)`): a
/// pair at 0.87 is exactly as noisy as one at 1.15. Both calibration
/// brackets reduce their ratio to this before comparing it against
/// [`NULL_RATIO_FLOOR`].
fn null_pair_deviation(ratio: f64) -> f64 {
    ratio.max(1.0 / ratio)
}

/// Whether the host stayed quiet across the whole cell loop, judged by the
/// null-pair calibration taken at each end of it. A recording or gating run
/// needs BOTH brackets under [`NULL_RATIO_FLOOR`], not just the start: the
/// end bracket is what a mid-run ambient burst trips, closing the window
/// the start-only gate left open. A burst that lands after the start
/// calibration and inflates an absolute bar while the cells run leaves the
/// start bracket quiet and the end bracket noisy, so a start-only gate
/// would record or gate a contaminated run.
fn run_stayed_quiet(start_deviation: f64, end_deviation: f64) -> bool {
    start_deviation <= NULL_RATIO_FLOOR && end_deviation <= NULL_RATIO_FLOOR
}

/// This host's one-minute load average, or `None` where it cannot be
/// read.
///
/// Every number a bench run prints is only interpretable next to what else
/// the machine was doing while it was taken, and a load written down after
/// the fact is a load nobody wrote down. Reading it here makes the run's
/// own output carry it.
fn host_load() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/loadavg")
            .ok()?
            .split_ascii_whitespace()
            .next()?
            .parse()
            .ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // `{ 1.23 4.56 7.89 }`
        let out = std::process::Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .ok()?;
        String::from_utf8(out.stdout)
            .ok()?
            .split_ascii_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }
}

/// Prints this host's load under `when`, or says it is unavailable rather
/// than printing nothing (a missing line reads as "nobody measured").
fn announce_load(when: &str) {
    match host_load() {
        Some(load) => println!("host load (1 min, {when}): {load:.2}"),
        None => println!("host load (1 min, {when}): unavailable on this host"),
    }
}

/// The resolved binaries one invocation measures with.
struct Bins {
    view: PathBuf,
    /// The bench-taps build; only required (and existence-checked) when a
    /// taps row actually runs.
    #[cfg(unix)]
    taps_view: PathBuf,
    nvim: PathBuf,
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

/// Why `scenario` cannot be measured on this host's platform, if it
/// cannot. The memory row is defined per-platform under that platform's
/// own metric name (spec 3.4), so the row is blocked exactly where the
/// scenario itself defines no metric rather than on a separate list of
/// platforms that could drift away from what the scenario measures.
fn platform_block(scenario: &str) -> Option<&'static str> {
    if scenario == "memory" && memory::METRIC.is_none() {
        return Some("no memory metric is defined for this platform (spec 3.4 defines pss_mb on Linux and phys_footprint_mb on macOS)");
    }
    // The tap channel is FIFO + raw-CLOCK_MONOTONIC based and exists only on
    // unix (see scenarios::taps); the internal-boundary rows and the echo_path
    // decomposition built on it cannot run where the channel is absent.
    #[cfg(not(unix))]
    if matches!(scenario, "input_path" | "output_path" | "echo_path") {
        return Some("the tap channel (FIFO + raw CLOCK_MONOTONIC) is a unix-only mechanism; the internal-boundary and echo_path rows built on it are not measured off unix");
    }
    // The control arm reaches its headless server over a unix socket path.
    #[cfg(not(unix))]
    if scenario == "echo_control" {
        return Some("the out-of-process control arm attaches its remote UI over a unix socket path; the named-pipe equivalent is unvalidated, so the row is not measured off unix");
    }
    None
}

/// Lines a full-matrix run prints for a platform-skipped cell. Under
/// GitHub Actions a `::warning::` workflow command is added so the
/// skipped cell surfaces on the checks page instead of only inside the
/// run log.
fn skip_announcements(scenario: &str, fixture: &str, reason: &str, under_gha: bool) -> Vec<String> {
    let mut lines = vec![format!("skipping {scenario}/{fixture}: {reason}")];
    if under_gha {
        lines.push(format!(
            "::warning::bench cell {scenario}/{fixture} skipped on this platform: {reason}"
        ));
    }
    lines
}

fn known_scenarios() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = MATRIX
        .iter()
        .chain(DIAGNOSTIC_MATRIX)
        .map(|(s, _)| *s)
        .collect();
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

    // Drop-based cleanup is skipped when a run is killed by a signal, so
    // stale scratch worlds from interrupted runs would otherwise pile up
    // silently. Clearing the whole parent is safe because concurrent
    // harness runs are out of contract anyway: two runs contending for
    // the same cores would corrupt each other's latency numbers.
    let _ = std::fs::remove_dir_all(scratch_root());

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

    // before any measurement: the class selects both the baseline file
    // and the per-platform metric names inside it, so a class from
    // another platform can only ever produce a verdict about numbers
    // this host does not measure
    baselines::require_host_platform(&cli.class)?;

    let pin = current_engine_pin()?;
    let nvim_bin = cli
        .nvim_bin
        .clone()
        .unwrap_or_else(|| PathBuf::from("nvim"));
    verify_nvim_matches_pin(&nvim_bin, &pin)?;
    let bins = Bins {
        view: resolve_view_bin(&cli)?,
        #[cfg(unix)]
        taps_view: cli.taps_view_bin.clone().unwrap_or_else(|| {
            workspace_root()
                .join("target")
                .join("taps")
                .join("release")
                .join("view")
        }),
        nvim: nvim_bin,
    };

    // resolved before anything expensive runs: a gate that was always
    // going to fail the provenance rule must say so up front, not after
    // half an hour of measurement
    let path = baseline_path(&cli.class);
    let bootstrapping = cli.bootstrap && !path.exists();
    let recording = cli.record || bootstrapping;
    let mut masked_regressions = 0usize;
    let gating = cli.gate && !bootstrapping;
    if cli.bootstrap {
        if bootstrapping {
            println!(
                "--bootstrap: no baseline at {}; this run records one instead of gating",
                path.display()
            );
        } else {
            println!(
                "--bootstrap: baseline {} already exists; gating normally",
                path.display()
            );
        }
    } else if gating && !path.exists() {
        bail!(
            "gating requires a recorded baseline at {}; run --record for this class (or pass \
             --bootstrap in CI) before gating",
            path.display()
        );
    }

    let under_gha = std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true");
    let mut skipped: Vec<(String, String)> = Vec::new();
    let cells: Vec<(String, String)> = if cli.all {
        let mut selected = Vec::new();
        for &(scenario, fixture) in MATRIX {
            if let Some(reason) = platform_block(scenario) {
                for line in skip_announcements(scenario, fixture, reason, under_gha) {
                    println!("{line}");
                }
                skipped.push((scenario.to_string(), fixture.to_string()));
            } else {
                selected.push((scenario.to_string(), fixture.to_string()));
            }
        }
        selected
    } else {
        let scenario = cli
            .scenario
            .clone()
            .context("--scenario is required unless --all is given")?;
        let fixture = cli
            .fixture
            .clone()
            .context("--fixture is required unless --all is given")?;
        let cell_named = |cells: &[(&str, &str)]| {
            cells
                .iter()
                .any(|(s, f)| *s == scenario.as_str() && *f == fixture.as_str())
        };
        if !cell_named(MATRIX) && !cell_named(DIAGNOSTIC_MATRIX) {
            bail!(
                "no matrix cell {scenario}/{fixture}; cells: {}",
                MATRIX
                    .iter()
                    .chain(DIAGNOSTIC_MATRIX)
                    .map(|(s, f)| format!("{s}/{f}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if cell_named(DIAGNOSTIC_MATRIX) && (cli.record || cli.gate) {
            bail!(
                "{scenario}/{fixture} is a report-only diagnostic cell: it decomposes another \
                 row on the instrumented build and holds no bar of its own, so it can be \
                 neither recorded nor gated"
            );
        }
        if let Some(reason) = platform_block(&scenario) {
            bail!("{scenario}/{fixture} cannot run on this platform: {reason}");
        }
        vec![(scenario, fixture)]
    };

    let protocol = Protocol {
        samples: cli.samples,
        warmup: cli.warmup,
        trials: cli.trials,
        ..Protocol::default()
    };

    announce_load("start");

    // A diagnostic cell reports ratios of its own, so it needs the same
    // read of this host's ambient pairing noise that a gated ratio gets --
    // taken in this run, at this load, rather than borrowed from another.
    // It is not refused on a noisy host the way a record or a gate is: a
    // decomposition of a loaded run is still a decomposition, as long as
    // the number it must be divided by is measured beside it.
    let diagnostic_selected = cells.iter().any(|(scenario, fixture)| {
        DIAGNOSTIC_MATRIX
            .iter()
            .any(|(s, f)| *s == scenario.as_str() && *f == fixture.as_str())
    });
    // gating on a noisy host produces false verdicts, and recording on
    // one poisons the baseline every later quiet run is judged against;
    // both therefore verify their own precondition before any cell runs.
    // The deviation is kept for the end bracket below: an absolute bar is
    // trustworthy only if the host stayed quiet across the whole cell loop,
    // not merely at the moment it started (see run_stayed_quiet).
    let mut start_deviation: Option<f64> = None;
    if recording || gating || diagnostic_selected {
        if recording || gating {
            // the class name alone selects the tail-gating policy, so a
            // mis-typed class silently weakens the gate unless every run
            // states the policy it derived
            println!(
                "class {}: {}",
                cli.class,
                if baselines::is_controlled_class(&cli.class) {
                    "controlled policy, tail metrics gated"
                } else {
                    "shared policy, tail metrics recorded but not gated"
                }
            );
        }
        let ratio = null_calibration(&bins)?;
        let deviation = null_pair_deviation(ratio);
        start_deviation = Some(deviation);
        println!(
            "null-pair calibration (start): ratio_p50 {ratio:.4} (deviation {deviation:.4}, floor \
             {NULL_RATIO_FLOOR})"
        );
        if deviation > NULL_RATIO_FLOOR {
            if recording || gating {
                bail!(
                    "host too noisy to {}: null-pair (nvim vs nvim) ratio_p50 measured \
                     {ratio:.4}, deviation {deviation:.4} from 1.0 exceeds the calibration floor \
                     {NULL_RATIO_FLOOR}; re-run when the host is quiet",
                    if recording { "record" } else { "gate" }
                );
            }
            println!(
                "      WARNING: deviation {deviation:.4} exceeds the calibration floor \
                 {NULL_RATIO_FLOOR}; every ratio below carries this host's noise and must be \
                 divided by {ratio:.4} before it is compared with a ratio from another run"
            );
        }
    }

    let mut measured: Vec<baselines::MeasuredCell> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for (scenario, fixture) in &cells {
        match run_cell(scenario, fixture, &bins, &protocol) {
            Ok(metrics) => measured.push((scenario.clone(), fixture.clone(), metrics)),
            // one cell failing says nothing about the cells that already
            // measured, and the cells still queued behind it are worth the
            // minutes the matrix has already spent getting here: keep
            // going, and refuse the run once, at the end, with the whole
            // picture rather than only the first thing that broke
            Err(err) => {
                eprintln!("CELL FAILED [{scenario}.{fixture}]: {err:#}");
                failed.push(format!("{scenario}.{fixture}: {err:#}"));
            }
        }
    }

    if !failed.is_empty() {
        // never written into the authoritative baseline: a full-matrix
        // record rebuilds that file, so recording 11 of 12 cells would
        // delete the twelfth's bars outright, and a gate verdict drawn from
        // an incomplete matrix is a pass that proves nothing about the cell
        // that never ran. The measurements that did land are still real, so
        // they go somewhere the operator can diff them against the baseline
        // by hand.
        let partial_path = path.with_extension("partial.toml");
        let partial = baselines::plan_record(
            None,
            baselines::RecordMode::FullMatrix,
            &cli.class,
            &pin,
            &measured,
        );
        baselines::save(&partial_path, &partial.file)?;
        eprintln!(
            "MATRIX INCOMPLETE: {} of {} cell(s) measured, {} failed; nothing was recorded and \
             no gate verdict was reached. The measured cells are in {} (raw, not ratcheted).",
            measured.len(),
            cells.len(),
            failed.len(),
            partial_path.display()
        );
        for line in &failed {
            eprintln!("  {line}");
        }
        std::process::exit(EXIT_INCOMPLETE_MATRIX);
    }

    announce_load("end");

    // The end bracket: the start calibration only certifies the instant
    // before the cells ran, so a mid-run ambient burst that inflates an
    // absolute bar (first_paint cold_ms, input_path/scroll p99) while the
    // cells run leaves the start clean and slips a false breach through. A
    // second null-pair calibration here brackets the whole cell loop; a run
    // that did not stay quiet at BOTH ends is refused (record poisons every
    // later baseline, gate emits a false verdict) rather than trusted. The
    // per-trial median already absorbs an isolated single-trial burst, so
    // this specifically guards the sustained-burst case (a foreign build
    // spanning the run) the median cannot.
    if let Some(start) = start_deviation {
        let end_ratio = null_calibration(&bins)?;
        let end = null_pair_deviation(end_ratio);
        println!(
            "null-pair calibration (end): ratio_p50 {end_ratio:.4} (deviation {end:.4}, floor \
             {NULL_RATIO_FLOOR})"
        );
        // A recording/gating run already bailed above if the start was
        // noisy, so reaching here past the fail-fast means the start was
        // quiet: run_stayed_quiet turning false is then exactly a noisy end
        // -- a burst that arrived while the cells ran. A diagnostic-only run
        // was not bailed, so this warns for either bracket rather than
        // claiming the noise was mid-run.
        if !run_stayed_quiet(start, end) {
            if recording || gating {
                bail!(
                    "host became noisy while the cells ran: end null-pair (nvim vs nvim) \
                     ratio_p50 measured {end_ratio:.4}, deviation {end:.4} from 1.0 exceeds the \
                     calibration floor {NULL_RATIO_FLOOR}; a mid-run ambient burst may have \
                     inflated an absolute bar, so this run is refused -- re-run when the host is \
                     quiet"
                );
            }
            println!(
                "      WARNING: the host was noisy at the start and/or end bracket (start \
                 deviation {start:.4}, end {end:.4}, floor {NULL_RATIO_FLOOR}); any absolute bar \
                 above may carry ambient noise"
            );
        }
    }

    if recording {
        let mode = if cli.all {
            baselines::RecordMode::FullMatrix
        } else {
            baselines::RecordMode::SingleCell
        };
        let existing = if path.exists() {
            let existing = baselines::load(&path)?;
            // a single-cell record keeps the file's other cells, so a pin or
            // class mismatch there would silently invalidate them; a
            // full-matrix record rebuilds the file and tolerates an
            // incomparable existing one (plan_record sets it aside)
            if matches!(mode, baselines::RecordMode::SingleCell) {
                baselines::require_pin_match(&existing, &pin, &path)?;
                baselines::require_class_match(&existing, &cli.class, &path)?;
            }
            Some(existing)
        } else {
            None
        };

        let plan = baselines::plan_record(existing, mode, &cli.class, &pin, &measured);
        baselines::save(&path, &plan.file)?;
        let report = plan.report(&path.display().to_string());
        for line in report.info {
            println!("{line}");
        }
        for line in &report.alerts {
            eprintln!("{line}");
        }
        // the record itself succeeded, so this is not a failure exit -- but a
        // masked regression is a worse measurement reported by a command that
        // otherwise looks entirely successful, and exit 0 is what makes it
        // skippable in a CI log. A distinct code says "written, and you have
        // something to look at" without colliding with the gate's own 1.
        masked_regressions = plan.masked_regressions();
    }

    if gating {
        let file = baselines::load(&path).with_context(|| {
            format!("gating requires a recorded baseline at {}", path.display())
        })?;
        baselines::require_pin_match(&file, &pin, &path)?;
        baselines::require_class_match(&file, &cli.class, &path)?;
        let mut breaches = Vec::new();
        // a cell that ran but stopped producing one of its recorded
        // numbers passes the forward walk, which compares only the
        // metrics both sides hold; naming those makes a silently
        // untested bar as loud as a dropped cell
        let mut unmeasured = Vec::new();
        for (scenario, fixture, metrics) in &measured {
            let Some(recorded) = file.cell(scenario, fixture) else {
                bail!(
                    "{} has no [{scenario}.{fixture}] cell; record it before gating",
                    path.display()
                );
            };
            breaches.extend(baselines::gate_cell(
                scenario,
                fixture,
                metrics,
                recorded,
                &cli.class,
                &file.headroom,
            ));
            for metric in baselines::unmeasured_metrics(metrics, recorded) {
                unmeasured.push((scenario.clone(), fixture.clone(), metric));
            }
        }
        for breach in &breaches {
            eprintln!("{breach}");
        }
        for (scenario, fixture, metric) in &unmeasured {
            eprintln!(
                "GATE COVERAGE FAIL [{scenario}.{fixture}] {metric}: the baseline records this \
                 metric but the run measured no value for it"
            );
        }
        // the forward walk proves measured cells sit inside their bars; a
        // full-coverage run must also prove the baseline holds no cell the
        // run silently dropped, or a cell that falls out of the matrix
        // stays green forever with bars that are never re-tested
        let mut uncovered = Vec::new();
        if cli.all {
            let measured_cells: Vec<(String, String)> = measured
                .iter()
                .map(|(scenario, fixture, _)| (scenario.clone(), fixture.clone()))
                .collect();
            uncovered = baselines::uncovered_cells(&file, &measured_cells, &skipped);
            for (scenario, fixture) in &uncovered {
                eprintln!(
                    "GATE COVERAGE FAIL [{scenario}.{fixture}]: baseline cell was neither \
                     measured nor platform-skipped this run"
                );
            }
        }
        // the recorded bars answer "is this worse than last time"; the spec
        // budgets answer "is this where the spec says it must be". Both run,
        // and they are reported apart, because a metric can be regression
        // green forever at a value the spec never accepted
        let budget_path = budgets_path();
        let budget_file = budgets::load(&budget_path).with_context(|| {
            format!(
                "gating requires the budget table at {}",
                budget_path.display()
            )
        })?;
        let mut findings = Vec::new();
        for (scenario, fixture, metrics) in &measured {
            findings.extend(budgets::check_cell(
                &budget_file,
                scenario,
                fixture,
                metrics,
                &cli.class,
                &file.headroom,
            ));
        }
        let budget_failures = findings
            .iter()
            .filter(|finding| finding.verdict.is_failure())
            .count();
        for finding in &findings {
            // an accepted shortfall prints every run on purpose: it is the
            // only thing that keeps an unmet budget from going quiet
            if finding.verdict != budgets::Verdict::Inside {
                eprintln!("{finding}");
            }
        }
        // a shortfall the run measured back inside its budget has been fixed
        // and its entry now describes nothing; only a full-coverage run can
        // tell that apart from a cell it simply did not visit
        let mut stale_shortfalls = Vec::new();
        if cli.all {
            stale_shortfalls = budgets::unreached_shortfalls(&budget_file, &cli.class, &findings);
            for shortfall in &stale_shortfalls {
                eprintln!(
                    "BUDGET SHORTFALL STALE [{}.{}] {} on {}: measured inside its budget this \
                     run, so the [[shortfall]] entry accepting {} is spent and should be deleted",
                    shortfall.scenario,
                    shortfall.fixture,
                    shortfall.metric,
                    shortfall.class,
                    shortfall.accepted
                );
            }
        }
        let clean = breaches.is_empty()
            && uncovered.is_empty()
            && unmeasured.is_empty()
            && budget_failures == 0
            && stale_shortfalls.is_empty();
        if clean {
            let held = findings
                .iter()
                .filter(|finding| matches!(finding.verdict, budgets::Verdict::Held { .. }))
                .count();
            println!(
                "gate OK: {} cell(s) within recorded bars, {} metric(s) checked against spec 3.1 \
                 budgets, {held} accepted shortfall(s) still held",
                measured.len(),
                findings.len()
            );
        } else {
            std::process::exit(EXIT_GATE_BREACH);
        }
    }

    if masked_regressions > 0 {
        std::process::exit(EXIT_RECORD_MASKED_REGRESSION);
    }

    Ok(())
}

/// A gate run found a measurement outside its recorded bar, a baseline cell
/// nothing measured, a recorded metric this run produced no value for, a
/// spec 3.1 budget missed with no shortfall recording it (or a recorded one
/// that widened), or a spent shortfall entry left behind after the metric
/// came back inside its budget.
const EXIT_GATE_BREACH: i32 = 1;

/// A record run wrote its baseline, but held at least one bar against a
/// measurement that would breach it: the file is correct and the run is not
/// a failure, yet there is a regression behind the better recorded number.
const EXIT_RECORD_MASKED_REGRESSION: i32 = 3;

/// At least one cell failed to measure, so the matrix is incomplete: no
/// baseline was written and no gate verdict was reached. Distinct from a
/// breach, which is a complete matrix reporting a real regression.
const EXIT_INCOMPLETE_MATRIX: i32 = 4;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Arguments that would leave a measured editor without the fixture
    /// configuration the cell exists to measure it under.
    const CONFIG_STRIPPING_ARGS: &[&str] = &["--clean", "-u", "-U", "--noplugin"];

    /// A pair of specs built the way a cell builds them, against binary
    /// paths that are never spawned: what the arguments and environment
    /// say is the whole subject here.
    fn paired_specs_for_test() -> (CellWorld, SpawnSpec, SpawnSpec) {
        let world = CellWorld::create("minimal").unwrap();
        let bin = Path::new("/nonexistent/never-spawned");
        let (view, nvim) = paired_specs(&world, "minimal", bin, bin).unwrap();
        (world, view, nvim)
    }

    #[test]
    fn both_sides_of_a_pair_open_the_scratch_file_as_their_first_argument() {
        // what plant_first_paint_marker writes through: if either side's
        // first argument stopped being the opened file, the marker would
        // land somewhere the editor never shows and every first_paint
        // sample would time out instead of measuring
        let (_world, view, nvim) = paired_specs_for_test();
        for spec in [&view, &nvim] {
            let first = spec.args.first().map(PathBuf::from);
            assert_eq!(
                first.as_ref().and_then(|p| p.file_name()),
                Some(std::ffi::OsStr::new("scratch.txt")),
                "expected the scratch file first, got {:?}",
                spec.args
            );
        }
    }

    #[test]
    fn planting_the_marker_writes_it_as_the_buffers_first_line() {
        let (_world, view, _nvim) = paired_specs_for_test();
        plant_first_paint_marker(&view).unwrap();
        let planted = std::fs::read_to_string(PathBuf::from(&view.args[0])).unwrap();
        assert_eq!(planted.lines().next(), Some(FIRST_PAINT_MARKER));
    }

    #[test]
    fn neither_side_of_a_pair_is_stripped_of_the_fixture_config() {
        let (_world, view, nvim) = paired_specs_for_test();
        for spec in [&view, &nvim] {
            for arg in &spec.args {
                let arg = arg.to_string_lossy().to_string();
                assert!(
                    !CONFIG_STRIPPING_ARGS.contains(&arg.as_str()),
                    "{arg} strips the measured editor of the fixture config, so the \
                     cell would measure a plugin-free editor against a baseline \
                     recorded with the fixture's plugins and pass its gate as an \
                     improvement; args {:?}",
                    spec.args
                );
            }
        }
    }

    #[test]
    fn each_side_is_pointed_at_its_own_copy_of_the_fixture_config() {
        let (_world, view, nvim) = paired_specs_for_test();
        let mut homes = vec![];
        for spec in [&view, &nvim] {
            let home = spec
                .env
                .iter()
                .find(|(name, _)| name == "XDG_CONFIG_HOME")
                .map(|(_, value)| PathBuf::from(value))
                .expect("a side with no XDG_CONFIG_HOME reads the host's own config");
            assert!(
                home.join("nvim").join("init.lua").is_file(),
                "{} holds no fixture config, so the side measures an unconfigured editor",
                home.display()
            );
            homes.push(home);
        }
        assert_ne!(
            homes[0], homes[1],
            "both sides share one config copy, so whichever runs first can rewrite \
             what the other measures"
        );
    }

    #[test]
    fn a_diagnostic_cell_is_never_a_recordable_matrix_cell() {
        // --all walks MATRIX alone, and every cell it walks must have a
        // baseline bar; a diagnostic cell appearing there would fail the
        // gate's coverage walk on a baseline that can never legitimately
        // hold it
        for (scenario, fixture) in DIAGNOSTIC_MATRIX {
            assert!(
                !MATRIX.iter().any(|(s, f)| s == scenario && f == fixture),
                "{scenario}/{fixture} is both a gated cell and a report-only diagnostic"
            );
        }
        assert!(known_scenarios().contains(&"echo_path"));
    }

    #[test]
    fn the_end_calibration_bracket_catches_a_burst_the_start_bracket_missed() {
        let floor = NULL_RATIO_FLOOR;
        // the mid-run burst: the start bracket saw a quiet host and passed,
        // then a burst arrived while the cells ran and lifted the end
        // bracket over the floor. Bracketing both ends is what turns the
        // false-breach a start-only gate would have shipped into a refusal.
        assert!(
            !run_stayed_quiet(1.05, floor + 0.15),
            "a quiet start with a noisy end is a mid-run burst -- not trustworthy"
        );
        // the case the start-only gate already handled stays handled
        assert!(!run_stayed_quiet(floor + 0.15, 1.05));
        // a run quiet at both ends is trustworthy; the floor is inclusive
        assert!(run_stayed_quiet(1.05, 1.08));
        assert!(run_stayed_quiet(floor, floor));
        // deviation is symmetric: a null pair at 1/1.15 reads like 1.15
        assert!((null_pair_deviation(1.0 / 1.15) - 1.15).abs() < 1e-9);
    }

    #[test]
    fn the_memory_row_is_blocked_exactly_where_it_has_no_metric() {
        assert_eq!(
            platform_block("memory").is_some(),
            memory::METRIC.is_none(),
            "the skip and the measurement must never disagree about whether the row can run"
        );
        assert!(platform_block("echo").is_none());
    }

    #[test]
    fn the_taps_rows_are_blocked_exactly_off_unix() {
        // the tap channel is a unix-only mechanism, so the internal-boundary
        // rows and the echo_path decomposition it drives run on unix and are
        // skipped on every other platform
        for scenario in ["input_path", "output_path", "echo_path"] {
            assert_eq!(
                platform_block(scenario).is_some(),
                cfg!(not(unix)),
                "{scenario} must be measured on unix and skipped off it"
            );
        }
    }

    #[test]
    fn skip_announcement_adds_a_checks_page_warning_only_under_gha() {
        let plain = skip_announcements("memory", "minimal", "linux-only metric", false);
        assert_eq!(
            plain,
            vec!["skipping memory/minimal: linux-only metric".to_string()]
        );
        let gha = skip_announcements("memory", "minimal", "linux-only metric", true);
        assert_eq!(gha.len(), 2);
        assert_eq!(gha[0], plain[0]);
        assert!(
            gha[1].starts_with("::warning::") && gha[1].contains("memory/minimal"),
            "annotation must be a workflow command naming the cell: {}",
            gha[1]
        );
    }
}
