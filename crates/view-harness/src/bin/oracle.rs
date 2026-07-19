//! `oracle [PATH]`: drives the corpus TOML format (`view_harness::corpus`)
//! through `view-oracle`'s parity stack -- `EngineSession` (view's own
//! decode/apply pipeline) against `ReferenceSession` (the independent naive
//! grid applier) -- and reports PARITY/DIVERGENCE/TIMEOUT per entry.
//!
//! `PATH` (default `corpus/`) names either a directory, walked
//! non-recursively for `*.toml` files in sorted order, or a single entry
//! file. Exit code is the runner's contract: 0 when every entry reaches
//! quiescence with no [`view_oracle::Divergence`], 1 otherwise -- an
//! unsettled session counts as a failure even with an empty divergence
//! list, since state read from a session that never finished processing
//! its input is not evidence of agreement, only of a race this run got
//! lucky on.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use view_harness::corpus::{self, CorpusEntry};
use view_oracle::{compare, masked_rows, snapshot, Divergence, EngineSession, ReferenceSession};

/// Terminal size every corpus entry runs at. Fixed rather than a
/// per-entry override (unlike the quiesce timing overrides): no seed entry
/// needs a different canvas, and a shared size keeps every entry's
/// PARITY/DIVERGENCE report line directly comparable.
const COLS: u16 = 60;
const ROWS: u16 = 12;

/// Bound on each startup/post-input drain loop
/// (`while engine.pump_until_flush(STARTUP_DRAIN) {}`): short enough that
/// a healthy already-quiet engine falls through in one iteration, long
/// enough that a genuine startup/redraw burst is never mistaken for
/// silence mid-burst. Matches the window `view-oracle`'s own end-to-end
/// parity test drains startup traffic with.
const STARTUP_DRAIN: Duration = Duration::from_millis(500);

#[derive(Parser)]
#[command(
    name = "oracle",
    about = "Differential oracle runner: drives a TOML corpus through view-oracle's parity stack"
)]
struct Cli {
    /// Corpus directory or a single entry file.
    #[arg(default_value = "corpus")]
    path: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let entries = collect_entries(&cli.path)?;
    if entries.is_empty() {
        bail!(
            "no corpus entries found under {} (expected *.toml files)",
            cli.path.display()
        );
    }

    let pin = current_engine_pin()?;
    let mut any_failed = false;
    for (path, entry) in entries {
        if entry.engine_pin != pin {
            eprintln!(
                "oracle: WARNING: {} ({}) was authored against engine pin {} but the current \
                 pin is {pin}; running anyway",
                entry.name,
                path.display(),
                entry.engine_pin,
            );
        }
        match run_entry(&entry) {
            Ok(outcome) => {
                print_outcome(&entry.name, &outcome);
                if !outcome.is_success() {
                    any_failed = true;
                }
            }
            Err(err) => {
                println!("oracle: {} ... ERROR: {err}", entry.name);
                any_failed = true;
            }
        }
    }

    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

/// Resolves `path` into a sorted list of `(file path, parsed entry)`
/// pairs: every `*.toml` file directly inside `path` if it is a directory
/// (sorted so a corpus run's entry order is deterministic across
/// invocations and hosts), or `path` itself if it names a file.
fn collect_entries(path: &Path) -> Result<Vec<(PathBuf, CorpusEntry)>> {
    let mut files: Vec<PathBuf> = if path.is_dir() {
        std::fs::read_dir(path)
            .with_context(|| format!("reading corpus directory {}", path.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    files.sort();

    files
        .into_iter()
        .map(|path| {
            let entry = corpus::load_file(&path)
                .with_context(|| format!("loading corpus entry {}", path.display()))?;
            Ok((path, entry))
        })
        .collect()
}

/// Path to the repo-root `.engine-pin` file, resolved from this binary's
/// own manifest dir rather than the caller's cwd (mirroring
/// `view-bench`'s `latency` bin's `scratch_path` helper): `task oracle`
/// always runs from the repo root today, but a direct `cargo run -p
/// view-harness` invocation from a subdirectory must not silently read a
/// stale or absent pin file instead.
fn engine_pin_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push(".engine-pin");
    path
}

/// Reads and trims the current `.engine-pin` value -- the single source of
/// truth `scripts/check-engine-pin.sh` gates CI against -- never a
/// hardcoded version literal here, so a pin bump does not require an
/// oracle-runner code change to stay accurate.
///
/// # Errors
///
/// Returns an error if `.engine-pin` cannot be read.
fn current_engine_pin() -> Result<String> {
    let path = engine_pin_path();
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading engine pin from {}", path.display()))?;
    Ok(raw.trim().to_string())
}

/// One entry's run result: whether both sides reached quiescence, how long
/// the whole drive-and-compare took, and every [`Divergence`] found (state
/// or grid). Kept as data rather than printed immediately inside
/// [`run_entry`], so [`print_outcome`] is the single place that decides
/// PARITY vs DIVERGENCE vs TIMEOUT wording.
struct EntryOutcome {
    settled: bool,
    elapsed_ms: u128,
    divergences: Vec<Divergence>,
}

impl EntryOutcome {
    /// An entry only counts as parity when both sides reached quiescence
    /// AND no divergence was found: state read from a session that never
    /// settled is not evidence either way, so a lucky empty divergence
    /// list on an unsettled run must not report success.
    fn is_success(&self) -> bool {
        self.settled && self.divergences.is_empty()
    }
}

/// Drives one corpus entry through the same protocol
/// `view-oracle`'s own end-to-end parity test uses: drain startup
/// traffic on both sides, feed `entry.input` to both, drain/quiesce again,
/// then diff state probes and masked grid rows.
fn run_entry(entry: &CorpusEntry) -> Result<EntryOutcome, view_oracle::OracleError> {
    let start = Instant::now();
    let silence = Duration::from_millis(entry.quiesce_silence_ms);
    let deadline = Duration::from_millis(entry.quiesce_deadline_ms);

    let mut engine = EngineSession::spawn(COLS, ROWS)?;
    let mut reference = ReferenceSession::spawn(COLS, ROWS)?;

    while engine.pump_until_flush(STARTUP_DRAIN) {}
    // startup quiescence is drained, not gated on: a slow-starting nvim's
    // own splash/plugin traffic settling late here is not itself a
    // divergence, only the post-input settle below decides pass/fail
    let _ = reference.quiesce(silence, deadline);

    engine.input(&entry.input)?;
    reference.input(&entry.input)?;

    engine.pump_until_flush(deadline);
    while engine.pump_until_flush(STARTUP_DRAIN) {}
    let settled = reference.quiesce(silence, deadline);

    let surface = engine.surface();
    let view_rows = engine.screen_rows();
    let mask = masked_rows(&surface);
    let ref_rows = reference.screen_rows();

    let view_state = snapshot(&mut engine)?;
    let ref_state = snapshot(&mut reference)?;

    let divergences = compare(&view_state, &ref_state, &view_rows, &ref_rows, &mask);

    Ok(EntryOutcome {
        settled,
        elapsed_ms: start.elapsed().as_millis(),
        divergences,
    })
}

/// Prints one entry's report line plus, on anything but clean PARITY,
/// every [`Divergence`] found -- the exit-contract report shape the corpus
/// runner's own interface (see this crate's module docs) commits to.
fn print_outcome(name: &str, outcome: &EntryOutcome) {
    let status = match (outcome.settled, outcome.divergences.is_empty()) {
        (true, true) => "PARITY",
        (true, false) => "DIVERGENCE",
        (false, _) => "TIMEOUT",
    };
    let settle_word = if outcome.settled {
        "settled"
    } else {
        "unsettled"
    };
    println!(
        "oracle: {name} ... {status} ({COLS}x{ROWS}, {settle_word}, {}ms)",
        outcome.elapsed_ms
    );
    for divergence in &outcome.divergences {
        println!("  {divergence:?}");
    }
}
