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
//!
//! `--record` and `--gate` are the two modes and neither ever stands in for
//! the other. A class with no committed baseline is armed by a `--record`
//! run whose output is reviewed and committed; `--gate` against a class
//! without one refuses before it measures anything, because a gate with
//! nothing to compare cannot fail and its exit 0 is indistinguishable from
//! a gate that compared everything and passed.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use view_bench::report;
#[cfg(unix)]
use view_bench::scenarios::echo_control;
use view_bench::scenarios::{
    echo, first_paint, flood, memory, picker, scroll, supervision, Protocol,
};

// The internal-boundary and echo_path rows run through the unix-only tap
// channel; their driver code lives in a cfg-gated child module so it is
// absent as a unit off unix rather than gated line by line.
#[cfg(unix)]
#[path = "bench/taps_rows.rs"]
mod taps_rows;

// The stub-arming machinery this row needs is a unix mechanism, the same
// reason taps_rows.rs above is split out.
#[cfg(unix)]
#[path = "bench/remote_rows.rs"]
mod remote_rows;

#[path = "bench/rows.rs"]
mod rows;
use rows::run_cell;

use view_bench::session::{NvimSpec, SpawnSpec, ViewSpec};
use view_harness::baselines::{self, CellId, CellMetrics};
use view_harness::budgets;
use view_harness::builds::{self, scenarios_reading, view_bin_flag, VIEW_BIN_FLAGS};
use view_harness::fixture::{
    cache_root, copy_dir_recursive, current_engine_pin, fixtures_root, lockfile_cache_key,
    verify_nvim_matches_pin, workspace_root,
};

/// Every measurable cell of the matrix, in run order.
const MATRIX: &[(&str, &str)] = &[
    ("echo", "minimal"),
    ("echo", "heavy"),
    ("echo_speculated", "minimal"),
    ("echo_control", "minimal"),
    ("echo_control", "heavy"),
    ("scroll", "minimal"),
    ("scroll", "heavy"),
    ("first_paint", "minimal"),
    ("first_paint", "heavy"),
    ("memory", "minimal"),
    ("remote_memory", "minimal"),
    ("flood", "minimal"),
    ("input_path", "minimal"),
    ("output_path", "minimal"),
    ("picker", "minimal"),
    ("supervision", "minimal"),
];

/// Cells that decompose a gated row instead of being one. They are
/// measured on demand, never selected by `--all`, and refused under
/// `--record`/`--gate`: a bar recorded from one would gate the
/// decomposition rather than the row it exists to explain, and the
/// decomposition runs the instrumented build, whose numbers are not the
/// quantity any recorded bar was taken from.
///
/// `memory/heavy` is the equivalence-matrix resource leg (spec 3.4,
/// ledger E2): it decomposes the gated `memory/minimal` row's own-process
/// number into a three-way reading (bare nvim, view's own process, view's
/// process plus its embedded nvim engine child) under the 14-plugin
/// fixture the gated row does not measure. Diagnostic rather than a new
/// gated cell because arming a new bar across every baseline class is a
/// separate, cross-platform undertaking (see ledger E4 on `picker`); this
/// leg's own value is the recorded evidence its report captures, not a
/// new budget.
const DIAGNOSTIC_MATRIX: &[(&str, &str)] = &[
    ("echo_path", "minimal"),
    ("echo_path", "heavy"),
    ("memory", "heavy"),
];

/// What one matrix row handed back: the metrics it stands behind, and the
/// reason it withheld its number when it did.
///
/// A refusal is neither of its two neighbors. A failed cell voids the
/// whole matrix (no record, no verdict), and a trusted empty cell
/// (echo_path) simply has nothing to record. The refusal lane exists for a
/// row whose own honesty check failed on a class where the refused number
/// never gates: failing the matrix there would let one cell's ambient
/// contention void eleven trusted measurements to protect a number no
/// gate reads.
struct RowOutcome {
    /// The metrics the row stands behind this run.
    metrics: CellMetrics,
    /// Why the row withheld its number, when it did.
    refused: Option<String>,
}

impl RowOutcome {
    /// A row that stands behind every metric it measured.
    fn trusted(metrics: CellMetrics) -> Self {
        Self {
            metrics,
            refused: None,
        }
    }
}

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

/// Parent of the generated picker corpora. Sibling to the per-run scratch
/// rather than inside it: the corpora cost minutes and a million inodes
/// to build, so they persist across runs (stamped, see
/// `picker::ensure_corpora`) instead of being torn down with each run's
/// hermetic worlds.
fn corpus_root() -> PathBuf {
    view_harness::fixture::scratch_root("bench-corpus")
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
    /// any breach, and refuses to start if the class has no baseline
    #[arg(long)]
    gate: bool,
    /// Path to the release view binary. Scope: the rows that measure the
    /// shipped build (first_paint, scroll, memory, flood, picker,
    /// supervision). The rows that measure a bench arm take the flag naming
    /// that arm, and a run that passes this flag while one of them is
    /// selected without its own flag is refused rather than silently
    /// leaving this one inert. Every one of these flags is refused when no
    /// selected row reads it at all
    #[arg(long)]
    view_bin: Option<PathBuf>,
    /// Path to the nvim binary (must match .engine-pin)
    #[arg(long)]
    nvim_bin: Option<PathBuf>,
    /// Path to the bench-taps build of view. Scope: input_path,
    /// output_path, echo_speculated
    #[cfg(unix)]
    #[arg(long)]
    taps_view_bin: Option<PathBuf>,
    /// Path to the bench-no-speculate build of view. Scope: the echo rows
    #[arg(long)]
    nospec_view_bin: Option<PathBuf>,
    /// Path to the bench-taps + bench-no-speculate build of view. Scope:
    /// echo_path, which decomposes an echo row and so must run the arm
    /// that row runs
    #[cfg(unix)]
    #[arg(long)]
    taps_nospec_view_bin: Option<PathBuf>,
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

/// Refuses a gate run whose class has no recorded baseline to gate
/// against.
///
/// The alternative shape -- fall back to recording and exit 0 -- makes the
/// gate silently inert: every comparison is skipped, no verdict is reached,
/// and the run reports success in a log where that is indistinguishable
/// from a gate that compared every cell and passed. Arming a new class is
/// therefore a `--record` invocation, which says in the command line what
/// the run actually did, and `--gate` from then on either produces a
/// verdict or does not start.
fn require_gatable(gating: bool, class: &str, path: &Path) -> Result<()> {
    ensure!(
        !gating || path.exists(),
        "--gate has no baseline for class {class} at {}, so it would compare nothing and exit as \
         though it had passed. Arm the class first with `task bench -- --all --class {class} \
         --record`, review the file it writes, and commit it",
        path.display()
    );
    Ok(())
}

/// Where the spec 3.1 budget table lives. One file for every class: a
/// budget is a property of the spec, not of the machine that measures it,
/// and the entries that do vary by class say so in their own `classes`
/// field rather than by living in separate files. That is only half the
/// doctrine: attestation requires a witness that can measure it, so the
/// gate reads this table on controlled classes alone, and a shared class
/// says so in its log instead of failing bounds it cannot witness.
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
///
/// `--nvim-bin` must precede the scratch-file positional: view's CLI
/// forwards every token after the first positional to nvim verbatim
/// (`trailing_var_arg`), so the reverse order hands `--nvim-bin <path>` to
/// nvim itself, which exits on the unknown flag and fails every cell at
/// engine attach.
fn view_spec_from(side: SideSetup, bins: EditorBins<'_>) -> SpawnSpec {
    SpawnSpec {
        program: bins.view.to_path_buf(),
        args: vec![
            OsString::from("--nvim-bin"),
            bins.nvim.as_os_str().to_os_string(),
            side.scratch_file.into_os_string(),
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
        .last()
        .map(PathBuf::from)
        .context("a paired spawn spec must open the scratch file as its last argument")?;
    let body = std::iter::repeat_n(FIRST_PAINT_MARKER, FIRST_PAINT_MARKER_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&target, format!("{body}\n"))
        .with_context(|| format!("planting the first-paint marker in {}", target.display()))?;
    Ok(())
}

/// One cell's two spawn specs, named rather than positional.
///
/// A tuple lets a caller destructure it in the wrong order, which builds a
/// row that runs both binaries correctly, refuses nothing, produces
/// entirely plausible numbers, and reports the comparison inverted --
/// view's result under nvim's name and back again. Named fields make that
/// binding say what it is doing.
struct PairedSpecs {
    view: SpawnSpec,
    nvim: SpawnSpec,
}

/// Builds one cell's paired view/nvim spawn specs against its hermetic
/// world.
fn paired_specs(world: &CellWorld, fixture: &str, bins: &Bins) -> Result<PairedSpecs> {
    paired_specs_with(world, fixture, bins.view_bins(), bins)
}

/// A pair whose view side is built from `editor` rather than from the
/// shipped binary, for a row that measures a counterfactual arm.
fn paired_specs_with(
    world: &CellWorld,
    fixture: &str,
    editor: EditorBins<'_>,
    bins: &Bins,
) -> Result<PairedSpecs> {
    let view_side = world.side(fixture, "view")?;
    let nvim_side = world.side(fixture, "nvim")?;
    Ok(PairedSpecs {
        view: view_spec_from(view_side, editor),
        nvim: nvim_spec_from(nvim_side, &bins.nvim),
    })
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
    let outcome = echo::run(
        ViewSpec(&spec_a),
        NvimSpec(&spec_b),
        &protocol,
        settle_deadline("minimal"),
    )
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
    /// The bench-no-speculate build, which the echo rows measure so their
    /// boundary keeps meaning the engine round trip; see [`Bins::echo_bins`].
    ///
    /// Unconditional where `taps_view` is unix-only: the tap channel is a
    /// unix mechanism, while the echo rows and the feature that arms them
    /// run on every platform this matrix measures.
    nospec_view: PathBuf,
    /// Both arms at once, for the one row that needs both: `echo_path`
    /// decomposes the echo round trip through the taps, so it needs the tap
    /// channel of `taps_view` and the silent glyph of `nospec_view`; see
    /// [`Bins::echo_path_bins`].
    #[cfg(unix)]
    taps_nospec_view: PathBuf,
    nvim: PathBuf,
}

/// The two binaries one view-side spawn names: the editor under
/// measurement, and the engine it is told to spawn. Named for the pair it
/// holds rather than for the view side of it: the per-side wrapper types
/// guarding view-against-nvim arguments elsewhere in this workspace are
/// singular, and a view-prefixed plural beside them reads as a collection
/// of those when it is a different concept entirely.
///
/// Both are paths, so passing them as adjacent positional arguments lets a
/// caller transpose them into a spec that spawns bare nvim as the editor
/// and hands it `--nvim-bin <view>` -- which nvim treats as more files to
/// open, so the pair still measures, still settles, and reports nvim
/// against nvim as a view result near 1.0. Naming them at the point they
/// are chosen removes the order from the problem.
#[derive(Clone, Copy)]
struct EditorBins<'a> {
    view: &'a Path,
    nvim: &'a Path,
}

impl Bins {
    /// The measured editor and its engine.
    fn view_bins(&self) -> EditorBins<'_> {
        EditorBins {
            view: &self.view,
            nvim: &self.nvim,
        }
    }

    /// The bench-taps build of the editor and its engine, for the rows
    /// that measure an internal boundary rather than the terminal.
    #[cfg(unix)]
    fn taps_bins(&self) -> Result<EditorBins<'_>> {
        ensure!(
            self.taps_view.exists(),
            "taps view binary {} does not exist; run via `task bench` (which builds it) or pass \
             --taps-view-bin",
            self.taps_view.display()
        );
        Ok(EditorBins {
            view: &self.taps_view,
            nvim: &self.nvim,
        })
    }

    /// The build the `echo_path` decomposition measures: the echo rows' arm
    /// with the tap channel compiled in.
    ///
    /// The decomposition splits the echo row's own interval into the stages
    /// the tap chain resolves, so it has to open and close that interval on
    /// the same events the row does. On a build that predicts, the glyph the
    /// chain's last tag waits for is on screen before the engine answers, the
    /// authoritative write falls outside the window, and every stage resolves
    /// over nothing -- which is why the row refuses a run that resolved no
    /// chain at all rather than printing zeros (see
    /// `taps_rows::unresolved_chain_refusal`).
    #[cfg(unix)]
    fn echo_path_bins(&self) -> Result<EditorBins<'_>> {
        ensure!(
            self.taps_nospec_view.exists(),
            "the echo_path decomposition's bench-taps + bench-no-speculate build {} does not \
             exist; run via `task bench` (which builds it) or pass --taps-nospec-view-bin. \
             Decomposing on a build that predicts leaves every stage resolving over zero \
             samples, because the glyph the chain closes on is painted before the engine \
             answers",
            self.taps_nospec_view.display()
        );
        Ok(EditorBins {
            view: &self.taps_nospec_view,
            nvim: &self.nvim,
        })
    }

    /// The build the echo rows measure: identical to the shipped one except
    /// that it predicts nothing.
    ///
    /// The echo row's boundary is the typed glyph appearing in the cursor's
    /// cell, and a predicted glyph is the same character in the same cell as
    /// the authoritative one -- so on the shipped build this row stops
    /// timing the round trip it is defined as (spec 3.1) and times the
    /// predicted paint instead, silently, reporting a several-fold
    /// improvement it did not make to the quantity it names. No screen
    /// boundary can tell the two apart; an arm that never writes the first
    /// one can. What the shipped build does here is a row of its own,
    /// `echo_speculated`, under metric names of its own.
    ///
    /// One other row shares that boundary and therefore that arm:
    /// `echo_path`, which decomposes this round trip stage by stage and
    /// takes it with the tap channel added ([`Bins::echo_path_bins`]). The
    /// rest keep the shipped binary, because their boundaries are startup,
    /// resident size, scroll staleness, engine output cadence, view's own
    /// picker and a restart the supervisor drives, none of which a
    /// prediction can answer. The tap rows keep the instrumented build:
    /// `input_path` and `output_path` close inside view before anything
    /// reaches the terminal, and `echo_speculated` is the row the predicted
    /// paint is the subject of.
    fn echo_bins(&self) -> Result<EditorBins<'_>> {
        ensure!(
            self.nospec_view.exists(),
            "the echo rows' bench-no-speculate build {} does not exist; run via `task bench` \
             (which builds it) or pass --nospec-view-bin. Measuring these rows with the shipped \
             binary instead would time the predicted paint under the round trip's own metric \
             names",
            self.nospec_view.display()
        );
        Ok(EditorBins {
            view: &self.nospec_view,
            nvim: &self.nvim,
        })
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
    // unix (see scenarios::taps); the internal-boundary rows, the echo_path
    // decomposition and the speculated-echo row's per-sample attribution are
    // all built on it and cannot run where the channel is absent.
    #[cfg(not(unix))]
    if matches!(
        scenario,
        "input_path" | "output_path" | "echo_path" | "echo_speculated"
    ) {
        return Some("the tap channel (FIFO + raw CLOCK_MONOTONIC) is a unix-only mechanism; the internal-boundary, echo_path and speculated-echo rows built on it are not measured off unix");
    }
    // The control arm reaches its headless server over a unix socket path;
    // remote_memory arms PATH against the stub ssh double with a unix
    // symlink (remote_rows.rs). Neither has a validated off-unix path.
    #[cfg(not(unix))]
    if matches!(scenario, "echo_control" | "remote_memory") {
        return Some("this row depends on a unix-only mechanism (echo_control's control socket, remote_memory's stub-ssh PATH symlink) with no validated equivalent off unix");
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

/// Whether `flag` was given a value on this command line.
fn bin_flag_given(cli: &Cli, flag: &str) -> bool {
    match flag {
        builds::VIEW_BIN => cli.view_bin.is_some(),
        #[cfg(unix)]
        builds::TAPS_VIEW_BIN => cli.taps_view_bin.is_some(),
        #[cfg(unix)]
        builds::TAPS_NOSPEC_VIEW_BIN => cli.taps_nospec_view_bin.is_some(),
        builds::NOSPEC_VIEW_BIN => cli.nospec_view_bin.is_some(),
        _ => false,
    }
}

/// How the selected rows read, for a refusal that has to say which builds
/// this run would actually have measured.
fn selected_rows_with_flags(cells: &[CellId]) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    for cell in cells {
        let entry = match view_bin_flag(&cell.scenario) {
            Some(flag) => format!("{} ({flag})", cell.scenario),
            None => format!("{} (no view build)", cell.scenario),
        };
        if !rows.contains(&entry) {
            rows.push(entry);
        }
    }
    rows
}

/// Refuses a run whose named binaries the selected rows would not read,
/// naming the flag and the rows.
///
/// Two ways a `--*-view-bin` flag goes inert, and both are refused, since
/// every one of them is accepted either way. `--view-bin` names the
/// shipped build, and three row families deliberately measure a build that
/// is not it: a run naming a binary and then measuring a different one for
/// part of the matrix reports numbers about a build nobody asked for.
/// Passing the unreached row's own flag as well resolves that one, because
/// then every selected row has been told which binary it measures. The
/// other way round is the same defect from the far side -- a flag no
/// selected row reads at all, where an operator named a binary, no row
/// spawned it, and the run exits 0 having measured the default build.
fn require_named_bins_reach_their_rows(cli: &Cli, cells: &[CellId]) -> Result<()> {
    if cli.view_bin.is_some() {
        let mut unreached: Vec<String> = Vec::new();
        for cell in cells {
            let Some(flag) = view_bin_flag(&cell.scenario) else {
                continue;
            };
            if bin_flag_given(cli, flag) {
                continue;
            }
            let entry = format!("{} ({flag})", cell.scenario);
            if !unreached.contains(&entry) {
                unreached.push(entry);
            }
        }
        ensure!(
            unreached.is_empty(),
            "--view-bin names the shipped build, which these selected rows do not measure: {}. \
             Pass each row's own flag beside it, or select only rows that measure the shipped \
             build",
            unreached.join(", ")
        );
    }
    for flag in VIEW_BIN_FLAGS {
        if !bin_flag_given(cli, flag) {
            continue;
        }
        if cells
            .iter()
            .any(|cell| view_bin_flag(&cell.scenario) == Some(*flag))
        {
            continue;
        }
        bail!(
            "{flag} names a build no selected row measures: the selected rows measure {}. The \
             rows that read {flag} are {}; drop the flag, or select a row that reads it",
            selected_rows_with_flags(cells).join(", "),
            scenarios_reading(flag).join(", ")
        );
    }
    Ok(())
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
        nospec_view: cli.nospec_view_bin.clone().unwrap_or_else(|| {
            workspace_root()
                .join("target")
                .join("nospec")
                .join("release")
                .join("view")
        }),
        #[cfg(unix)]
        taps_nospec_view: cli.taps_nospec_view_bin.clone().unwrap_or_else(|| {
            workspace_root()
                .join("target")
                .join("taps-nospec")
                .join("release")
                .join("view")
        }),
        nvim: nvim_bin,
    };

    // resolved before anything expensive runs: a gate that was always
    // going to fail the provenance rule must say so up front, not after
    // half an hour of measurement
    let path = baseline_path(&cli.class);
    require_gatable(cli.gate, &cli.class, &path)?;
    // the measured-headroom sidecar is hand-curated and never rewritten by
    // --record; loaded before anything expensive runs for the same reason
    // the baseline path is resolved here
    let headroom_file = baselines::headroom_path(&path);
    let headroom = baselines::load_headroom(&headroom_file, &cli.class)?;
    let recording = cli.record;
    let mut masked_regressions = 0usize;
    let mut spread_refusals = 0usize;
    let gating = cli.gate;
    let controlled = baselines::is_controlled_class(&cli.class);

    let under_gha = std::env::var("GITHUB_ACTIONS").is_ok_and(|v| v == "true");
    let mut skipped: Vec<CellId> = Vec::new();
    let cells: Vec<CellId> = if cli.all {
        let mut selected = Vec::new();
        for &(scenario, fixture) in MATRIX {
            if let Some(reason) = platform_block(scenario) {
                for line in skip_announcements(scenario, fixture, reason, under_gha) {
                    println!("{line}");
                }
                skipped.push(CellId::new(scenario, fixture));
            } else {
                selected.push(CellId::new(scenario, fixture));
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
        vec![CellId { scenario, fixture }]
    };

    // before any measurement, for the same reason the class and the
    // baseline path are resolved early: a binary the selected rows would
    // never have read is a run whose numbers describe a different build
    require_named_bins_reach_their_rows(&cli, &cells)?;

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
    let diagnostic_selected = cells.iter().any(|cell| {
        DIAGNOSTIC_MATRIX
            .iter()
            .any(|(s, f)| *s == cell.scenario.as_str() && *f == cell.fixture.as_str())
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
                if controlled {
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
    let mut refused: Vec<CellId> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for cell in &cells {
        let (scenario, fixture) = (&cell.scenario, &cell.fixture);
        match run_cell(cell, &bins, &protocol, controlled) {
            Ok(outcome) => {
                if let Some(reason) = &outcome.refused {
                    // a refusal is quieter than a failure by design, so on CI
                    // it gets the same checks-page annotation a platform skip
                    // does: visible without opening the run log
                    if under_gha {
                        println!(
                            "::warning::bench cell {scenario}/{fixture} refused its own \
                             measurement: {reason}"
                        );
                    }
                    refused.push(cell.clone());
                }
                measured.push(baselines::MeasuredCell {
                    id: cell.clone(),
                    metrics: outcome.metrics,
                });
            }
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
            &headroom,
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
        require_record_survives_refusal(mode, &refused)?;
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

        let plan = baselines::plan_record(existing, mode, &cli.class, &pin, &measured, &headroom);
        // checked against the file about to be written, not the one being
        // replaced: a record that drops the last cell measuring a
        // characterized metric must refuse rather than orphan the sidecar
        // entry and fail the next load
        let bound = baselines::require_headroom_bound(&headroom, &plan.file, &headroom_file);
        if refused.is_empty() {
            bound?;
        } else {
            // an unbound sidecar entry here may be unbound only because the
            // cell that measures it refused this run; without the naming, the
            // error reads as an invitation to delete a valid entry
            bound.with_context(|| {
                format!(
                    "cell(s) {} refused their own measurement this run, so the file about to be \
                     written holds them empty; if the entry binds through one of them, re-run \
                     when the host is quieter instead of deleting it",
                    refused
                        .iter()
                        .map(|id| format!("{}.{}", id.scenario, id.fixture))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        }
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
        spread_refusals = plan.spread_refusals();
    }

    if gating {
        let file = baselines::load(&path).with_context(|| {
            format!("gating requires a recorded baseline at {}", path.display())
        })?;
        baselines::require_pin_match(&file, &pin, &path)?;
        baselines::require_class_match(&file, &cli.class, &path)?;
        baselines::require_headroom_bound(&headroom, &file, &headroom_file)?;
        let mut breaches = Vec::new();
        // a cell that ran but stopped producing one of its recorded
        // numbers passes the forward walk, which compares only the
        // metrics both sides hold; naming those makes a silently
        // untested bar as loud as a dropped cell
        let mut unmeasured = Vec::new();
        // the mirror gap: a present cell that lost one of its metric keys
        // would otherwise leave that bar silently ungated -- deleting one
        // key must be as loud as deleting the cell
        let mut unrecorded_metric = Vec::new();
        let unrecorded = baselines::unrecorded_cells(&file, &measured);
        for cell in &measured {
            // named by unrecorded_cells above and reported with the rest of
            // the findings; refusing here instead would end the run at the
            // first missing cell with no verdict printed for any of the
            // cells already measured
            let Some(recorded) = file.cell(&cell.id) else {
                continue;
            };
            breaches.extend(baselines::gate_cell(cell, recorded, &cli.class, &headroom));
            // a refused cell's absent metrics are attributed, not coverage
            // gaps: the row said loudly why it withheld its number, and on
            // this class (a controlled one bails before reaching the gate)
            // the recorded bar it left untested is one no gate reads anyway
            let cell_refused = refused.contains(&cell.id);
            for metric in baselines::unmeasured_metrics(cell, recorded) {
                if cell_refused {
                    println!(
                        "GATE SKIP [{}.{}] {metric}: the row refused its own measurement this \
                         run, so the recorded bar was not tested",
                        cell.id.scenario, cell.id.fixture
                    );
                } else {
                    unmeasured.push((cell.id.clone(), metric));
                }
            }
            for metric in baselines::unrecorded_metrics(cell, recorded) {
                unrecorded_metric.push((cell.id.clone(), metric));
            }
        }
        for breach in &breaches {
            eprintln!("{breach}");
        }
        for (id, metric) in &unmeasured {
            eprintln!(
                "GATE COVERAGE FAIL [{}.{}] {metric}: the baseline records this metric but the \
                 run measured no value for it",
                id.scenario, id.fixture
            );
        }
        for (id, metric) in &unrecorded_metric {
            eprintln!(
                "GATE COVERAGE FAIL [{}.{}] {metric}: the run measured this metric but the \
                 baseline cell holds no bar for it; record it before gating",
                id.scenario, id.fixture
            );
        }
        for id in &unrecorded {
            eprintln!(
                "GATE COVERAGE FAIL [{}.{}]: {} has no cell for this row; record it before \
                 gating",
                id.scenario,
                id.fixture,
                path.display()
            );
        }
        // the forward walk proves measured cells sit inside their bars; a
        // full-coverage run must also prove the baseline holds no cell the
        // run silently dropped, or a cell that falls out of the matrix
        // stays green forever with bars that are never re-tested
        let mut uncovered = Vec::new();
        if cli.all {
            let measured_cells: Vec<CellId> = measured.iter().map(|c| c.id.clone()).collect();
            uncovered = baselines::uncovered_cells(&file, &measured_cells, &skipped);
            for id in &uncovered {
                eprintln!(
                    "GATE COVERAGE FAIL [{}.{}]: baseline cell was neither measured nor \
                     platform-skipped this run",
                    id.scenario, id.fixture
                );
            }
        }
        // the recorded bars answer "is this worse than last time"; the spec
        // budgets answer "is this where the spec says it must be". Both are
        // reported apart, because a metric can be regression green forever
        // at a value the spec never accepted -- but the budget verdict runs
        // only where a controlled host can witness it: a shared class keeps
        // the ratchet and announces the skip instead of failing spec bounds
        // its own draws cannot attest
        let budget_file = if controlled {
            let budget_path = budgets_path();
            Some(budgets::load(&budget_path).with_context(|| {
                format!(
                    "gating requires the budget table at {}",
                    budget_path.display()
                )
            })?)
        } else {
            println!(
                "BUDGET SKIP [{}]: spec 3.1 budgets attest on controlled classes only; this \
                 shared class ratchets its recorded bars and leaves the budget verdict to a \
                 witness that can measure it",
                cli.class
            );
            None
        };
        let mut findings = Vec::new();
        let mut budget_failures = 0;
        let mut stale_shortfalls = Vec::new();
        let mut dead_budgets = Vec::new();
        if let Some(budget_file) = &budget_file {
            findings = budgets::check_run(budget_file, &file, &measured, &cli.class, &headroom);
            budget_failures = findings
                .iter()
                .filter(|finding| finding.verdict.is_failure())
                .count();
            for finding in &findings {
                // an accepted shortfall prints every run on purpose: it is
                // the only thing that keeps an unmet budget from going quiet
                if finding.verdict != budgets::Verdict::Inside {
                    eprintln!("{finding}");
                }
            }
            // a shortfall the run measured back inside its budget by more
            // than the class's spread for the statistic has been fixed and
            // its entry now describes nothing; a bare inside reading is one
            // draw of a statistic whose next draw may still need the entry.
            // Stale-entry advice accompanies the full sweep, where the
            // operator is adjudicating the whole ledger; a scoped run keeps
            // its exit status about the cells it measured
            if cli.all {
                stale_shortfalls =
                    budgets::unreached_shortfalls(budget_file, &cli.class, &findings, &headroom);
                for shortfall in &stale_shortfalls {
                    eprintln!(
                        "BUDGET SHORTFALL STALE [{}.{}] {} on {}: measured inside its budget \
                         this run by more than the class's spread for the statistic, so the \
                         [[shortfall]] entry accepting {} is spent and should be deleted",
                        shortfall.scenario,
                        shortfall.fixture,
                        shortfall.metric,
                        shortfall.class,
                        shortfall.accepted
                    );
                }
            }
            // a budget whose scenario ran without producing its metric
            // matched nothing and reported nothing, which is the shape of a
            // pass
            if cli.all {
                dead_budgets = budgets::unreached_budgets(budget_file, &cli.class, &measured);
                for budget in &dead_budgets {
                    eprintln!(
                        "BUDGET UNENFORCED [{}] {} on {}: the run measured this scenario and it \
                         produced no such metric, so the spec bound {} checked nothing [{}]",
                        budget.scenario, budget.metric, cli.class, budget.max, budget.spec_row
                    );
                }
            }
        }
        let tally = GateTally {
            breaches: breaches.len(),
            uncovered_cells: uncovered.len(),
            unmeasured_metrics: unmeasured.len(),
            unrecorded_cells: unrecorded.len(),
            unrecorded_metrics: unrecorded_metric.len(),
            budget_failures,
            stale_shortfalls: stale_shortfalls.len(),
            dead_budgets: dead_budgets.len(),
        };
        if tally.findings() == 0 {
            let held = findings
                .iter()
                .filter(|finding| matches!(finding.verdict, budgets::Verdict::Held { .. }))
                .count();
            // counted apart from the held shortfalls it sits beside in the
            // report: a shortfall is a gap the project has written down and
            // owes work on, while an excursion is this run's weather against
            // a bound the recorded measurement meets. Summing them would
            // report a clean class as carrying a gap it does not have
            let excursions = findings
                .iter()
                .filter(|finding| matches!(finding.verdict, budgets::Verdict::Excursion { .. }))
                .count();
            // a class with an empty headroom table has published no spread
            // for anything, so a count "inside this class's measured spread"
            // would be a sentence about a quantity that does not exist there
            let spread = if headroom.is_empty() {
                String::new()
            } else {
                format!(", {excursions} reading(s) past spec inside this class's measured spread")
            };
            println!(
                "gate OK: {} cell(s) within recorded bars, {} metric(s) checked against spec 3.1 \
                 budgets, {held} accepted shortfall(s) still held{spread}",
                measured.len(),
                findings.len()
            );
        } else {
            std::process::exit(EXIT_GATE_BREACH);
        }
    }

    if let Some(code) = record_exit_code(spread_refusals, masked_regressions) {
        std::process::exit(code);
    }

    Ok(())
}

/// Refuses a record whose written file would erase recorded bars behind a
/// refusal. A single-cell record upserts exactly what the one cell
/// measured, so a refusal there would replace the cell's existing bars
/// with an empty cell under a command that reports success. A full-matrix
/// record survives the same refusal: it rebuilds the whole file, the
/// refusal has already been announced beside eleven trusted cells, and
/// the next quiet record re-adds the bars as fresh metrics.
fn require_record_survives_refusal(mode: baselines::RecordMode, refused: &[CellId]) -> Result<()> {
    if matches!(mode, baselines::RecordMode::SingleCell) {
        if let Some(id) = refused.first() {
            bail!(
                "{}/{} refused its own measurement, and recording the single cell would replace \
                 its recorded bars with nothing; re-run when the host is quieter",
                id.scenario,
                id.fixture
            );
        }
    }
    Ok(())
}

/// Every category of finding one gate run produced, as counts.
///
/// The verdict is read off this rather than off a hand-written conjunction
/// over the vectors that hold them. A category added to the report and left
/// out of such a conjunction prints a `GATE ... FAIL` line and then exits 0
/// -- a gate that lies in the one direction CI reads. [`GateTally::findings`]
/// destructures every field, so a category added here and not added to the
/// sum does not compile.
struct GateTally {
    breaches: usize,
    uncovered_cells: usize,
    unmeasured_metrics: usize,
    unrecorded_cells: usize,
    unrecorded_metrics: usize,
    budget_failures: usize,
    stale_shortfalls: usize,
    dead_budgets: usize,
}

impl GateTally {
    /// How many findings the run produced across every category; zero is
    /// the only clean verdict.
    fn findings(&self) -> usize {
        let Self {
            breaches,
            uncovered_cells,
            unmeasured_metrics,
            unrecorded_cells,
            unrecorded_metrics,
            budget_failures,
            stale_shortfalls,
            dead_budgets,
        } = *self;
        breaches
            + uncovered_cells
            + unmeasured_metrics
            + unrecorded_cells
            + unrecorded_metrics
            + budget_failures
            + stale_shortfalls
            + dead_budgets
    }
}

/// A gate run found a measurement outside its recorded bar, a baseline cell
/// nothing measured, a measured cell the baseline holds no bar for, a
/// recorded metric this run produced no value for, a measured metric its
/// recorded cell holds no bar for, a spec 3.1 budget missed
/// with no shortfall recording it (or a recorded one that widened), a spent
/// shortfall entry left behind after the metric came back inside its
/// budget, or a budget bound to a metric its own scenario never produces.
const EXIT_GATE_BREACH: i32 = 1;

/// A record run wrote its baseline, but held at least one bar against a
/// measurement that would breach it: the file is correct and the run is not
/// a failure, yet there is a regression behind the better recorded number.
const EXIT_RECORD_MASKED_REGRESSION: i32 = 3;

/// At least one cell failed to measure, so the matrix is incomplete: no
/// baseline was written and no gate verdict was reached. Distinct from a
/// breach, which is a complete matrix reporting a real regression.
const EXIT_INCOMPLETE_MATRIX: i32 = 4;

/// A record run refused to move at least one bar further below its
/// recorded value than the class's published spread admits: the file was
/// written with those bars held, none of the refused values are in it, and
/// each refusal's alert names the replicate-campaign command that
/// legitimately re-records the cell. Non-zero because the operator asked
/// for a record the run declined to fully perform; distinct from the
/// masked-regression 3 because the required follow-up is different in kind
/// (run a campaign, not investigate a regression).
const EXIT_RECORD_SPREAD_REFUSED: i32 = 5;

/// The exit code a completed record run reports for its own two possible
/// surprises, or `None` on a quiet record. A pure decision extracted out of
/// the record path so the precedence between the two codes is a fact a unit
/// test can pin directly, rather than only being reachable by driving the
/// whole record pipeline to produce both surprises on the same run.
///
/// One record can produce both surprises on different metrics; the refusal
/// wins because its next step (the replicate-campaign command) exists only
/// in this run's output, while a masked regression re-announces itself at
/// the very next gate.
fn record_exit_code(spread_refusals: usize, masked_regressions: usize) -> Option<i32> {
    if spread_refusals > 0 {
        return Some(EXIT_RECORD_SPREAD_REFUSED);
    }
    if masked_regressions > 0 {
        return Some(EXIT_RECORD_MASKED_REGRESSION);
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Arguments that would leave a measured editor without the fixture
    /// configuration the cell exists to measure it under.
    const CONFIG_STRIPPING_ARGS: &[&str] = &["--clean", "-u", "-U", "--noplugin"];

    /// Binary paths that are never spawned, one distinguishable path per
    /// role.
    ///
    /// Distinct on purpose: a helper that gave all three roles the same
    /// path cannot tell a pair that measured view from a pair that ran
    /// bare nvim on both sides, so every assertion built on it is blind to
    /// the one failure that reports a fabricated 1.0 ratio and gates green.
    const VIEW_BIN_FOR_TEST: &str = "/nonexistent/never-spawned/view";
    #[cfg(unix)]
    const TAPS_VIEW_BIN_FOR_TEST: &str = "/nonexistent/never-spawned/view-taps";
    const NOSPEC_VIEW_BIN_FOR_TEST: &str = "/nonexistent/never-spawned/view-nospec";
    #[cfg(unix)]
    const TAPS_NOSPEC_VIEW_BIN_FOR_TEST: &str = "/nonexistent/never-spawned/view-taps-nospec";
    const NVIM_BIN_FOR_TEST: &str = "/nonexistent/never-spawned/nvim";

    fn bins_for_test() -> Bins {
        Bins {
            view: PathBuf::from(VIEW_BIN_FOR_TEST),
            #[cfg(unix)]
            taps_view: PathBuf::from(TAPS_VIEW_BIN_FOR_TEST),
            nospec_view: PathBuf::from(NOSPEC_VIEW_BIN_FOR_TEST),
            #[cfg(unix)]
            taps_nospec_view: PathBuf::from(TAPS_NOSPEC_VIEW_BIN_FOR_TEST),
            nvim: PathBuf::from(NVIM_BIN_FOR_TEST),
        }
    }

    /// The engine path a view-side spec tells the editor to spawn, read
    /// back off the argument vector.
    fn nvim_bin_argument(spec: &SpawnSpec) -> Option<PathBuf> {
        let flag = spec
            .args
            .iter()
            .position(|arg| arg == std::ffi::OsStr::new("--nvim-bin"))?;
        spec.args.get(flag + 1).map(PathBuf::from)
    }

    /// A pair of specs built the way a cell builds them, against binary
    /// paths that are never spawned: what the program, the arguments and
    /// the environment say is the whole subject here.
    fn paired_specs_for_test() -> (CellWorld, PairedSpecs) {
        let world = CellWorld::create("minimal").unwrap();
        let pair = paired_specs(&world, "minimal", &bins_for_test()).unwrap();
        (world, pair)
    }

    #[test]
    fn each_side_of_a_pair_spawns_the_binary_its_field_name_claims() {
        // the failure this catches leaves no trace in any number: bare nvim
        // spawned as the measured editor takes `--nvim-bin <view>` as more
        // files to open, so both sides settle, both measure, and every
        // paired row reports nvim against nvim as a view result near 1.0
        let (_world, pair) = paired_specs_for_test();
        assert_eq!(
            pair.view.program,
            PathBuf::from(VIEW_BIN_FOR_TEST),
            "the view side of the pair spawned {} instead of the editor under measurement",
            pair.view.program.display()
        );
        assert_eq!(
            pair.nvim.program,
            PathBuf::from(NVIM_BIN_FOR_TEST),
            "the nvim side of the pair spawned {} instead of the pinned engine",
            pair.nvim.program.display()
        );
        assert_eq!(
            nvim_bin_argument(&pair.view),
            Some(PathBuf::from(NVIM_BIN_FOR_TEST)),
            "the view side must be told to spawn the same pinned engine the nvim side runs, \
             got args {:?}",
            pair.view.args
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_taps_side_spawns_the_instrumented_build_against_the_same_engine() {
        // taps_bins is the second producer of the same pair, reached only
        // from the input_path/output_path rows: a pair written wrong here
        // measures the uninstrumented editor and records it under the
        // boundary metric's name
        let world = CellWorld::create("minimal").unwrap();
        let bins = Bins {
            taps_view: world.hermetic_dir.join("view-taps"),
            ..bins_for_test()
        };
        assert!(
            bins.taps_bins().is_err(),
            "a missing instrumented build must be named, not silently swapped for the shipped \
             binary"
        );

        std::fs::write(&bins.taps_view, b"").unwrap();
        let spec = view_spec_from(
            world.side("minimal", "view").unwrap(),
            bins.taps_bins().unwrap(),
        );
        assert_eq!(
            spec.program,
            bins.taps_view,
            "a taps row spawned {} instead of the bench-taps build",
            spec.program.display()
        );
        assert_eq!(
            nvim_bin_argument(&spec),
            Some(PathBuf::from(NVIM_BIN_FOR_TEST)),
            "got args {:?}",
            spec.args
        );
    }

    /// The echo rows' arm, pinned by the same reasoning as the taps one: a
    /// pair written wrong here reports several-fold "improvements" that are
    /// the predicted paint being timed under the round trip's metric names,
    /// and nothing about the resulting number looks wrong.
    #[test]
    fn the_echo_rows_measure_the_build_that_predicts_nothing() {
        let world = CellWorld::create("minimal").unwrap();
        let bins = Bins {
            nospec_view: world.hermetic_dir.join("view-nospec"),
            ..bins_for_test()
        };
        assert!(
            bins.echo_bins().is_err(),
            "a missing arm must be named, not silently swapped for the shipped binary"
        );

        std::fs::write(&bins.nospec_view, b"").unwrap();
        let spec = view_spec_from(
            world.side("minimal", "view").unwrap(),
            bins.echo_bins().unwrap(),
        );
        assert_eq!(
            spec.program,
            bins.nospec_view,
            "the echo row spawned {} instead of the bench-no-speculate build",
            spec.program.display()
        );
        assert_eq!(
            nvim_bin_argument(&spec),
            Some(PathBuf::from(NVIM_BIN_FOR_TEST))
        );
    }

    /// The decomposition has to run the build the row it decomposes runs,
    /// with the taps added. On a predicting build the chain's closing tag
    /// waits for a glyph that was already on screen, so every stage
    /// resolves over nothing while the row still prints a ratio and exits
    /// clean -- the same silent-contamination shape the echo row's arm
    /// exists to prevent, one row over.
    #[cfg(unix)]
    #[test]
    fn the_echo_path_decomposition_measures_the_tapped_arm_that_predicts_nothing() {
        let world = CellWorld::create("minimal").unwrap();
        let bins = Bins {
            taps_nospec_view: world.hermetic_dir.join("view-taps-nospec"),
            ..bins_for_test()
        };
        assert!(
            bins.echo_path_bins().is_err(),
            "a missing arm must be named, not silently swapped for a build that predicts"
        );

        std::fs::write(&bins.taps_nospec_view, b"").unwrap();
        let spec = view_spec_from(
            world.side("minimal", "view").unwrap(),
            bins.echo_path_bins().unwrap(),
        );
        assert_eq!(
            spec.program,
            bins.taps_nospec_view,
            "the echo_path row spawned {} instead of the bench-taps + bench-no-speculate build",
            spec.program.display()
        );
        assert_ne!(
            spec.program,
            PathBuf::from(TAPS_VIEW_BIN_FOR_TEST),
            "the plain taps build predicts, which leaves the chain unresolvable"
        );
    }

    /// The flag-scope table has to name a build for every scenario the
    /// matrix can run, or the flag checks go back to being silently inert
    /// for whichever row the table forgot -- which is why the assertion is
    /// on the entry existing rather than on the flag it holds: a table that
    /// answered for an unnamed scenario could not fail this.
    #[test]
    fn every_matrix_scenario_names_the_build_it_measures() {
        for scenario in known_scenarios() {
            assert!(
                builds::names_a_build(scenario),
                "{scenario} has no entry in the flag-scope table, so a binary named on the \
                 command line would go unread for it without saying so"
            );
        }
        // the one row with no view build in its pair, spelled out here so
        // the table cannot answer `None` for a row that does spawn one and
        // still pass the loop above
        assert_eq!(view_bin_flag("echo_control"), None);
        for scenario in known_scenarios() {
            if scenario == "echo_control" {
                continue;
            }
            let flag = view_bin_flag(scenario);
            assert!(
                flag.is_some_and(|flag| VIEW_BIN_FLAGS.contains(&flag)),
                "{scenario} names {flag:?}, which is not a flag the bench binary accepts"
            );
        }
    }

    /// Every scenario the table names has to be one the matrix can select,
    /// or the table accumulates entries for rows that no longer exist and
    /// the pin above passes over a stale answer.
    #[test]
    fn the_flag_scope_table_names_no_scenario_the_matrix_cannot_run() {
        let known = known_scenarios();
        for (scenario, _) in builds::MEASURED_BUILD {
            assert!(
                known.contains(scenario),
                "the flag-scope table names {scenario}, which no matrix cell runs"
            );
        }
    }

    /// `--view-bin` reaches the rows that measure the shipped build and
    /// nothing else; a run that names it while an arm row is selected is
    /// refused with that row's own flag in the message, rather than
    /// measuring a build nobody named.
    ///
    /// The accepted case is a real shipped-build cell of the matrix, not a
    /// name only this test knows: a fictional row would be accepted by any
    /// table that answered for an unnamed scenario, so the branch would
    /// prove nothing about the rows a run can select.
    #[test]
    fn view_bin_is_refused_where_it_would_not_be_read() {
        let shipped_only = |cells: &[(&str, &str)]| {
            let cells: Vec<CellId> = cells.iter().map(|(s, f)| CellId::new(s, f)).collect();
            let cli = Cli::parse_from([
                "bench",
                "--class",
                "dev-linux",
                "--view-bin",
                VIEW_BIN_FOR_TEST,
            ]);
            require_named_bins_reach_their_rows(&cli, &cells)
        };
        assert!(MATRIX.contains(&("picker", "minimal")));
        assert!(shipped_only(&[("picker", "minimal")]).is_ok());

        let refusal = shipped_only(&[("picker", "minimal"), ("echo", "minimal")])
            .expect_err("an inert --view-bin must be refused, not accepted and ignored");
        let refusal = format!("{refusal:#}");
        assert!(
            refusal.contains("echo (--nospec-view-bin)"),
            "the refusal must name the flag the unreached row takes, got: {refusal}"
        );

        // both flags given: every selected row has been told which binary
        // it measures, so there is nothing inert left to refuse
        let cli = Cli::parse_from([
            "bench",
            "--class",
            "dev-linux",
            "--view-bin",
            VIEW_BIN_FOR_TEST,
            "--nospec-view-bin",
            NOSPEC_VIEW_BIN_FOR_TEST,
        ]);
        assert!(require_named_bins_reach_their_rows(
            &cli,
            &[
                CellId::new("picker", "minimal"),
                CellId::new("echo", "minimal")
            ]
        )
        .is_ok());

        // no --view-bin at all: every row falls back to the build its own
        // default names, which is what the whole matrix runs on
        let cli = Cli::parse_from(["bench", "--class", "dev-linux"]);
        assert!(
            require_named_bins_reach_their_rows(&cli, &[CellId::new("echo", "minimal")]).is_ok()
        );
    }

    /// An arm flag no selected row reads is refused the same way
    /// `--view-bin` is, naming the flag, what the selected rows do measure
    /// and which rows would have read it. Otherwise the flag is accepted,
    /// ignored, and the run reports numbers off the default build -- which
    /// a nonexistent path proves, since a row that read it could not spawn.
    #[test]
    fn an_arm_flag_no_selected_row_reads_is_refused() {
        let with_nospec = |cells: &[(&str, &str)]| {
            let cells: Vec<CellId> = cells.iter().map(|(s, f)| CellId::new(s, f)).collect();
            let cli = Cli::parse_from([
                "bench",
                "--class",
                "dev-linux",
                "--nospec-view-bin",
                NOSPEC_VIEW_BIN_FOR_TEST,
            ]);
            require_named_bins_reach_their_rows(&cli, &cells)
        };
        let refusal = with_nospec(&[("picker", "minimal")])
            .expect_err("an arm flag no row reads must be refused, not accepted and ignored");
        let refusal = format!("{refusal:#}");
        for expected in [
            "--nospec-view-bin names a build no selected row measures",
            "picker (--view-bin)",
            "are echo",
        ] {
            assert!(
                refusal.contains(expected),
                "the refusal must name the flag, the selected rows and the rows that read it; \
                 missing {expected:?} in: {refusal}"
            );
        }
        assert!(with_nospec(&[("echo", "minimal")]).is_ok());

        // the row with no view build at all still reads as a row that does
        // not measure the named binary, rather than being skipped into an
        // accepted run
        let refusal = with_nospec(&[("echo_control", "minimal")])
            .expect_err("a bare-nvim pair reads no named binary either");
        assert!(
            format!("{refusal:#}").contains("echo_control (no view build)"),
            "the refusal must say the selected row spawns no view build, got: {refusal:#}"
        );
    }

    /// The taps flags are refused on the same rule; separate from the
    /// no-speculate case because they exist only where the tap channel
    /// does, and a check that compiled them away on one platform would go
    /// quiet there rather than fail.
    #[cfg(unix)]
    #[test]
    fn a_taps_flag_no_selected_row_reads_is_refused() {
        let cells = [CellId::new("picker", "minimal")];
        for (flag, path, reader) in [
            (
                builds::TAPS_VIEW_BIN,
                TAPS_VIEW_BIN_FOR_TEST,
                "echo_speculated",
            ),
            (
                builds::TAPS_NOSPEC_VIEW_BIN,
                TAPS_NOSPEC_VIEW_BIN_FOR_TEST,
                "echo_path",
            ),
        ] {
            let cli = Cli::parse_from(["bench", "--class", "dev-linux", flag, path]);
            let refusal = require_named_bins_reach_their_rows(&cli, &cells)
                .expect_err("an inert taps flag must be refused");
            let refusal = format!("{refusal:#}");
            assert!(
                refusal.contains(flag) && refusal.contains(reader),
                "the refusal must name {flag} and the rows that read it, got: {refusal}"
            );
        }
        let cli = Cli::parse_from([
            "bench",
            "--class",
            "dev-linux",
            builds::TAPS_NOSPEC_VIEW_BIN,
            TAPS_NOSPEC_VIEW_BIN_FOR_TEST,
        ]);
        assert!(
            require_named_bins_reach_their_rows(&cli, &[CellId::new("echo_path", "minimal")])
                .is_ok()
        );
    }

    #[test]
    fn both_sides_of_a_pair_open_the_scratch_file_as_their_last_argument() {
        // what plant_first_paint_marker writes through: if either side's
        // last argument stopped being the opened file, the marker would
        // land somewhere the editor never shows and every first_paint
        // sample would time out instead of measuring. Last rather than
        // first because the view side's own flags must precede the
        // positional -- everything after it is forwarded to nvim verbatim
        let (_world, pair) = paired_specs_for_test();
        for spec in [&pair.view, &pair.nvim] {
            let last = spec.args.last().map(PathBuf::from);
            assert_eq!(
                last.as_ref().and_then(|p| p.file_name()),
                Some(std::ffi::OsStr::new("scratch.txt")),
                "expected the scratch file last, got {:?}",
                spec.args
            );
        }
    }

    #[test]
    fn planting_the_marker_writes_it_as_the_buffers_first_line() {
        let (_world, pair) = paired_specs_for_test();
        plant_first_paint_marker(&pair.view).unwrap();
        let scratch = pair.view.args.last().unwrap();
        let planted = std::fs::read_to_string(PathBuf::from(scratch)).unwrap();
        assert_eq!(planted.lines().next(), Some(FIRST_PAINT_MARKER));
    }

    #[test]
    fn neither_side_of_a_pair_is_stripped_of_the_fixture_config() {
        let (_world, pair) = paired_specs_for_test();
        for spec in [&pair.view, &pair.nvim] {
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
        let (_world, pair) = paired_specs_for_test();
        let mut homes = vec![];
        for spec in [&pair.view, &pair.nvim] {
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

    /// A gate that prints a finding and exits 0 is worse than one that
    /// prints nothing, because CI reads the code and not the log. Each
    /// category is set alone here, so a category that stops reaching the
    /// verdict fails on its own line rather than hiding behind another.
    ///
    /// Disconfirm: dropping any one term from `GateTally::findings` fails
    /// exactly the entry named for it.
    #[test]
    fn every_category_of_gate_finding_reaches_the_verdict_on_its_own() {
        let zero = GateTally {
            breaches: 0,
            uncovered_cells: 0,
            unmeasured_metrics: 0,
            unrecorded_cells: 0,
            unrecorded_metrics: 0,
            budget_failures: 0,
            stale_shortfalls: 0,
            dead_budgets: 0,
        };
        assert_eq!(
            zero.findings(),
            0,
            "a run with nothing to report must gate clean"
        );

        type Category = (&'static str, fn(&mut GateTally));
        let categories: [Category; 8] = [
            ("breaches", |t| t.breaches = 1),
            ("uncovered_cells", |t| t.uncovered_cells = 1),
            ("unmeasured_metrics", |t| t.unmeasured_metrics = 1),
            ("unrecorded_cells", |t| t.unrecorded_cells = 1),
            ("unrecorded_metrics", |t| t.unrecorded_metrics = 1),
            ("budget_failures", |t| t.budget_failures = 1),
            ("stale_shortfalls", |t| t.stale_shortfalls = 1),
            ("dead_budgets", |t| t.dead_budgets = 1),
        ];
        for (name, set) in categories {
            let mut tally = GateTally {
                breaches: 0,
                uncovered_cells: 0,
                unmeasured_metrics: 0,
                unrecorded_cells: 0,
                unrecorded_metrics: 0,
                budget_failures: 0,
                stale_shortfalls: 0,
                dead_budgets: 0,
            };
            set(&mut tally);
            assert_eq!(
                tally.findings(),
                1,
                "a run whose only finding is {name} would print it and exit 0"
            );
        }
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
    fn a_gate_with_no_baseline_refuses_instead_of_reporting_a_pass() {
        // the failure this forbids is silent by construction: with no
        // baseline every comparison is skipped, the run exits 0, and a CI
        // log cannot tell that from a gate that compared every cell
        let dir = scratch_root().join(format!("gatable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("gh-linux.toml");
        let refusal = require_gatable(true, "gh-linux", &missing)
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("gh-linux") && refusal.contains("--record"),
            "the refusal must name the class and how to arm it, got: {refusal}"
        );
        // a run that is not gating has nothing to compare and never asks
        require_gatable(false, "gh-linux", &missing).unwrap();
        std::fs::write(&missing, "schema = 1\n").unwrap();
        require_gatable(true, "gh-linux", &missing).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_single_cell_record_never_erases_bars_behind_a_refusal() {
        // the failure this forbids reports success: SingleCell mode upserts
        // exactly what the cell measured, so an empty refusal cell would
        // overwrite the recorded bars and the run would still exit 0
        let refused = vec![CellId::new("input_path", "minimal")];
        let err = require_record_survives_refusal(baselines::RecordMode::SingleCell, &refused)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("input_path/minimal") && err.contains("re-run"),
            "the refusal must name the cell and the way forward, got: {err}"
        );
        // a full-matrix record rebuilds the file and keeps the refusal loud
        require_record_survives_refusal(baselines::RecordMode::FullMatrix, &refused).unwrap();
        require_record_survives_refusal(baselines::RecordMode::SingleCell, &[]).unwrap();
    }

    #[test]
    fn every_exit_code_names_a_distinct_verdict() {
        // 0 is success and 2 is clap's own usage-error exit; a code
        // colliding with either -- or with another verdict -- makes a CI
        // log's exit status ambiguous about what actually happened
        let codes = [
            EXIT_GATE_BREACH,
            EXIT_RECORD_MASKED_REGRESSION,
            EXIT_INCOMPLETE_MATRIX,
            EXIT_RECORD_SPREAD_REFUSED,
        ];
        for (i, a) in codes.iter().enumerate() {
            assert!(*a != 0 && *a != 2, "verdict code {a} collides");
            for b in &codes[i + 1..] {
                assert_ne!(a, b, "two verdicts share exit code {a}");
            }
        }
    }

    #[test]
    fn a_spread_refusal_outranks_a_masked_regression_on_the_same_run() {
        assert_eq!(record_exit_code(1, 1), Some(EXIT_RECORD_SPREAD_REFUSED));
    }

    #[test]
    fn a_masked_regression_alone_reports_its_own_code() {
        assert_eq!(record_exit_code(0, 1), Some(EXIT_RECORD_MASKED_REGRESSION));
    }

    #[test]
    fn a_spread_refusal_alone_reports_its_own_code() {
        assert_eq!(record_exit_code(1, 0), Some(EXIT_RECORD_SPREAD_REFUSED));
    }

    #[test]
    fn a_quiet_record_reports_no_exit_code() {
        assert_eq!(record_exit_code(0, 0), None);
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
        for scenario in ["input_path", "output_path", "echo_path", "echo_speculated"] {
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
