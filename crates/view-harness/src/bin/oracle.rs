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
//!
//! Two subcommands extend the runner past the bare pass/fail report:
//! `minimize PATH` shrinks a corpus entry that already reproduces a
//! divergence or timeout to a locally 1-minimal input (rewriting the entry
//! in place), and `fuzz --seed N` drives seeded, reproducible random
//! scripts through the same stack, quarantining (already minimized) any
//! round that fails. Both share [`run_tokens`], the same
//! spawn/drain/quiesce/compare engine the plain corpus run above uses, so
//! all three modes see identical parity semantics.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use view_harness::corpus::{self, CorpusEntry};
use view_harness::fuzz;
use view_oracle::{
    compare, ddmin, join_tokens, masked_rows, snapshot, tokenize, Divergence, DivergenceKind,
    EngineSession, ReferenceSession,
};

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

/// Test-support sentinel `Command::Minimize`'s hidden `--inject-divergence-at`
/// flag splices into a token stream at a caller-chosen index: a value no
/// real key-notation escape or literal character in any generated or
/// hand-authored script can ever collide with (the paired NUL bytes are
/// not producible by this crate's tokenizer or fuzz alphabet), so
/// [`run_tokens`] can find it unambiguously in a minimizer candidate no
/// matter how ddmin has reshuffled the surrounding tokens.
const INJECT_DIVERGENCE_TOKEN: &str = "\u{0}inject-divergence\u{0}";

/// The key sequence [`run_tokens`] sends to the reference session alone
/// when [`INJECT_DIVERGENCE_TOKEN`] appears in its input. Wrapped in
/// `<Cmd>...<CR>`, the same mode-agnostic mechanism `ReferenceSession`'s
/// own quiesce hooks use (see `view_oracle::reference`'s module docs) and
/// for the identical reason: it executes without leaving whatever mode the
/// session is currently in. Overwrites line 1 with a fixed sentinel so the
/// two sides' `buffer_lines` state disagrees from that point forward
/// regardless of what surrounds it in the script, giving the minimizer
/// exactly one real thing to find.
const INJECT_DIVERGENCE_KEYS: &str = "<Cmd>call setline(1, 'DDMIN_INJECTED_DIVERGENCE')<CR>";

#[derive(Parser)]
#[command(
    name = "oracle",
    about = "Differential oracle runner: drives a TOML corpus through view-oracle's parity stack"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Corpus directory or a single entry file. Ignored when a subcommand
    /// (`minimize`, `fuzz`) is given.
    #[arg(default_value = "corpus")]
    path: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Notation-token-aware ddmin minimizer: shrinks a corpus entry that
    /// already reproduces a divergence or timeout to a minimal input,
    /// rewriting the entry in place.
    Minimize {
        /// Corpus entry file to minimize.
        path: PathBuf,
        /// Test-support only, not a real reproduction mechanism: splices a
        /// forced-divergence token into the entry's input at token index N
        /// before minimizing, so the minimizer can be proven end-to-end
        /// against real nvim without hand-authoring a corpus entry that
        /// happens to diverge naturally.
        #[arg(long, hide = true)]
        inject_divergence_at: Option<usize>,
    },
    /// Seeded fuzz generator: drives random key-notation scripts through
    /// the same parity stack, quarantining (already minimized) any round
    /// that diverges or times out.
    Fuzz {
        /// RNG seed. Required, not defaulted: a fuzz run must be exactly
        /// reproducible (see `view_harness::fuzz`'s own module docs), which
        /// only holds when the seed is always an explicit, recorded input.
        #[arg(long)]
        seed: u64,
        /// Number of rounds to run.
        #[arg(long, default_value_t = 100)]
        rounds: u32,
        /// Number of key-notation tokens per round's generated script.
        #[arg(long, default_value_t = 150)]
        keys: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Minimize {
            path,
            inject_divergence_at,
        }) => minimize_command(&path, inject_divergence_at),
        Some(Command::Fuzz { seed, rounds, keys }) => fuzz_command(seed, rounds, keys),
        None => run_command(&cli.path),
    }
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

/// One run's result: whether each side reached quiescence independently,
/// how long the whole drive-and-compare took, and every [`Divergence`]
/// found (state or grid). Tracked per side rather than as a single merged
/// bool: a wedged engine side must fail the run even when the reference
/// side happens to settle (and vice versa), and the report line needs to
/// say which side timed out. Kept as data rather than printed immediately
/// inside [`run_tokens`], so [`print_outcome`] is the single place that
/// decides PARITY vs DIVERGENCE vs TIMEOUT wording.
struct EntryOutcome {
    engine_settled: bool,
    reference_settled: bool,
    elapsed_ms: u128,
    divergences: Vec<Divergence>,
}

impl EntryOutcome {
    /// A run only counts as parity when both sides reached quiescence AND
    /// no divergence was found: state read from a session that never
    /// settled is not evidence either way, so a lucky empty divergence
    /// list on an unsettled run must not report success.
    fn is_success(&self) -> bool {
        self.engine_settled && self.reference_settled && self.divergences.is_empty()
    }
}

/// Derives the report-line status word from both sides' settle results and
/// whether any divergence was found. A free function rather than inlined in
/// [`print_outcome`] so the merge decision -- an unsettled side always wins
/// over an empty divergence list, on either side, never falling through to
/// PARITY or DIVERGENCE -- is checkable without spawning a real engine.
fn settle_status(engine_settled: bool, reference_settled: bool, divergences_empty: bool) -> String {
    match (engine_settled, reference_settled) {
        (true, true) if divergences_empty => "PARITY".to_string(),
        (true, true) => "DIVERGENCE".to_string(),
        (false, true) => "TIMEOUT (engine)".to_string(),
        (true, false) => "TIMEOUT (reference)".to_string(),
        (false, false) => "TIMEOUT (engine, reference)".to_string(),
    }
}

/// Drives one token-vector script through the oracle stack: spawns a fresh
/// `EngineSession`/`ReferenceSession` pair, drains startup traffic, feeds
/// `tokens`, quiesces, and compares. The single execution engine every
/// corpus entry run, minimizer candidate probe, and fuzz round shares, so
/// all three see identical spawn/drain/quiesce/compare semantics and any
/// fix to one applies to all three.
///
/// [`INJECT_DIVERGENCE_TOKEN`] is handled specially when present: the
/// tokens before and after it are fed to both sides as usual, but
/// [`INJECT_DIVERGENCE_KEYS`] is spliced in between, sent to the reference
/// session alone. Every other token is fed identically to both sides via
/// one joined `nvim_input` call each (matching the single-call shape the
/// corpus runner has always used), not one call per token: splitting into
/// per-token calls only when the sentinel demands it keeps the normal,
/// no-injection path's RPC-call count unchanged from before this function
/// existed.
fn run_tokens(
    tokens: &[String],
    cols: u16,
    rows: u16,
    silence: Duration,
    deadline: Duration,
) -> Result<EntryOutcome, view_oracle::OracleError> {
    let start = Instant::now();
    let mut engine = EngineSession::spawn(cols, rows)?;
    let mut reference = ReferenceSession::spawn(cols, rows)?;

    while engine.pump_until_flush(STARTUP_DRAIN) {}
    // startup quiescence is drained, not gated on: a slow-starting nvim's
    // own splash/plugin traffic settling late here is not itself a
    // divergence, only the post-input settle below decides pass/fail
    let _ = reference.quiesce(silence, deadline);

    if let Some(pos) = tokens.iter().position(|t| t == INJECT_DIVERGENCE_TOKEN) {
        let prefix = join_tokens(&tokens[..pos]);
        let suffix = join_tokens(&tokens[pos + 1..]);
        engine.input(&prefix)?;
        reference.input(&prefix)?;
        reference.input(INJECT_DIVERGENCE_KEYS)?;
        engine.input(&suffix)?;
        reference.input(&suffix)?;
    } else {
        let joined = join_tokens(tokens);
        engine.input(&joined)?;
        reference.input(&joined)?;
    }

    let engine_settled = engine.pump_until_flush(deadline);
    while engine.pump_until_flush(STARTUP_DRAIN) {}
    let reference_settled = reference.quiesce(silence, deadline);

    let surface = engine.surface();
    let view_rows = engine.screen_rows();
    let mask = masked_rows(&surface);
    let ref_rows = reference.screen_rows();

    let view_state = snapshot(&mut engine)?;
    let ref_state = snapshot(&mut reference)?;

    let divergences = compare(&view_state, &ref_state, &view_rows, &ref_rows, &mask);

    Ok(EntryOutcome {
        engine_settled,
        reference_settled,
        elapsed_ms: start.elapsed().as_millis(),
        divergences,
    })
}

/// Tokenizes `entry.input` and drives it through [`run_tokens`] at
/// `entry`'s own quiesce overrides -- the plain corpus-run path `Command`'s
/// `None` (bare `oracle [PATH]`) arm uses.
fn run_entry(entry: &CorpusEntry) -> Result<EntryOutcome, view_oracle::OracleError> {
    run_tokens(
        &tokenize(&entry.input),
        COLS,
        ROWS,
        Duration::from_millis(entry.quiesce_silence_ms),
        Duration::from_millis(entry.quiesce_deadline_ms),
    )
}

/// Prints one entry's report line plus, on anything but clean PARITY,
/// every [`Divergence`] found -- the exit-contract report shape the corpus
/// runner's own interface (see this crate's module docs) commits to.
fn print_outcome(name: &str, outcome: &EntryOutcome) {
    let status = settle_status(
        outcome.engine_settled,
        outcome.reference_settled,
        outcome.divergences.is_empty(),
    );
    let settle_word = if outcome.engine_settled && outcome.reference_settled {
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

/// The bare `oracle [PATH]` run: every entry under `path`, reported and
/// exit-coded per this crate's own module docs.
fn run_command(path: &Path) -> Result<()> {
    let entries = collect_entries(path)?;
    if entries.is_empty() {
        bail!(
            "no corpus entries found under {} (expected *.toml files)",
            path.display()
        );
    }

    let pin = current_engine_pin()?;
    let mut any_failed = false;
    for (entry_path, entry) in entries {
        if entry.engine_pin != pin {
            eprintln!(
                "oracle: WARNING: {} ({}) was authored against engine pin {} but the current \
                 pin is {pin}; running anyway",
                entry.name,
                entry_path.display(),
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

/// The specific failure shape a minimizer run reduces toward: either a
/// [`Divergence`]'s top-level [`DivergenceKind`] (state vs grid), or which
/// side(s) failed to settle. Distinguishing the two matters because they
/// are different bugs at different layers (see `view_oracle::parity`'s own
/// module docs for the state/grid split), and a minimizer must never
/// shrink a divergent script toward an unrelated timeout, or a timing-out
/// script toward an unrelated divergence, just because either one happens
/// to count as "not PARITY".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureSignature {
    Divergence(DivergenceKind),
    Timeout {
        engine_settled: bool,
        reference_settled: bool,
    },
}

impl FailureSignature {
    /// Reads `outcome`'s failure shape, or `None` for clean PARITY --
    /// nothing for a minimizer to reduce toward.
    fn from_outcome(outcome: &EntryOutcome) -> Option<Self> {
        if !outcome.engine_settled || !outcome.reference_settled {
            return Some(Self::Timeout {
                engine_settled: outcome.engine_settled,
                reference_settled: outcome.reference_settled,
            });
        }
        outcome
            .divergences
            .first()
            .map(|d| Self::Divergence(d.kind()))
    }

    /// Whether `outcome` reproduces this same failure shape: the ddmin
    /// reproduction predicate every minimizer candidate is tested against.
    fn matches(self, outcome: &EntryOutcome) -> bool {
        Self::from_outcome(outcome) == Some(self)
    }
}

/// Runs [`ddmin`] against `tokens`, reproduction meaning "still exhibits
/// `target`'s failure shape" ([`FailureSignature::matches`]). A probe
/// error (an engine failed to spawn, a state probe did not parse) is
/// treated as non-reproduction rather than propagated: an intermittent
/// probe failure is not evidence a candidate exhibits the target failure,
/// and [`ddmin`]'s own termination argument does not depend on every
/// candidate's probe succeeding, only on the `test` closure returning some
/// `bool`.
fn minimize_tokens(
    tokens: Vec<String>,
    target: FailureSignature,
    cols: u16,
    rows: u16,
    silence: Duration,
    deadline: Duration,
) -> Vec<String> {
    ddmin(tokens, |candidate| {
        run_tokens(candidate, cols, rows, silence, deadline)
            .is_ok_and(|outcome| target.matches(&outcome))
    })
}

/// Builds the token vector [`minimize_command`] hands to [`ddmin`]:
/// `entry`'s own input, tokenized, with [`INJECT_DIVERGENCE_TOKEN`]
/// spliced in at index `inject_at` when the hidden test-support flag is
/// set.
fn build_tokens(entry: &CorpusEntry, inject_at: Option<usize>) -> Vec<String> {
    let mut tokens = tokenize(&entry.input);
    if let Some(n) = inject_at {
        tokens.insert(n.min(tokens.len()), INJECT_DIVERGENCE_TOKEN.to_string());
    }
    tokens
}

/// The `minimize PATH` subcommand: loads `path`, runs it once to establish
/// which failure it currently reproduces, then reduces it toward that same
/// failure and rewrites `path` in place with the result.
///
/// # Errors
///
/// Returns an error if `path` cannot be loaded, the baseline run's probes
/// fail, `path`'s entry currently reproduces neither a divergence nor a
/// timeout (nothing to minimize), or the minimized entry cannot be written
/// back.
fn minimize_command(path: &Path, inject_divergence_at: Option<usize>) -> Result<()> {
    let entry = corpus::load_file(path)
        .with_context(|| format!("loading corpus entry {}", path.display()))?;
    let silence = Duration::from_millis(entry.quiesce_silence_ms);
    let deadline = Duration::from_millis(entry.quiesce_deadline_ms);
    let tokens = build_tokens(&entry, inject_divergence_at);
    let original_len = tokens.len();

    let baseline = run_tokens(&tokens, COLS, ROWS, silence, deadline)
        .with_context(|| format!("running baseline for {}", entry.name))?;
    let Some(target) = FailureSignature::from_outcome(&baseline) else {
        bail!(
            "{} does not currently reproduce a divergence or timeout; nothing to minimize",
            entry.name
        );
    };

    let minimized = minimize_tokens(tokens, target, COLS, ROWS, silence, deadline);
    let minimized_input = join_tokens(&minimized);

    println!("minimized: {original_len} keys -> {} keys", minimized.len());

    corpus::write_entry(
        path,
        &entry.name,
        &minimized_input,
        &entry.engine_pin,
        &entry.ext_set,
        entry.quiesce_silence_ms,
        entry.quiesce_deadline_ms,
    )
    .with_context(|| format!("writing minimized entry back to {}", path.display()))?;

    Ok(())
}

/// Directory quarantined fuzz-discovered failures are written to: durable,
/// reviewable artifacts (matching `corpus/`'s own convention), not scratch
/// files that vanish after one run.
fn quarantine_path(seed: u64, round: u32) -> PathBuf {
    PathBuf::from("corpus/quarantine").join(format!("fuzz-{seed}-{round}.toml"))
}

/// The `fuzz --seed N` subcommand: generates `rounds` reproducible random
/// scripts from `seed` (see [`fuzz::generate_round`]), drives each through
/// [`run_tokens`] at the corpus loader's own default quiesce window, and
/// for any round that is not clean PARITY, minimizes it toward its own
/// failure shape and writes the (already minimized) result to
/// [`quarantine_path`].
///
/// # Errors
///
/// Returns an error if `.engine-pin` cannot be read, a round's probes
/// fail, or a quarantine entry cannot be written.
fn fuzz_command(seed: u64, rounds: u32, keys: usize) -> Result<()> {
    let silence = Duration::from_millis(corpus::DEFAULT_QUIESCE_SILENCE_MS);
    let deadline = Duration::from_millis(corpus::DEFAULT_QUIESCE_DEADLINE_MS);
    let pin = current_engine_pin()?;

    let mut divergence_count = 0u32;
    let mut timeout_count = 0u32;
    let mut error_count = 0u32;

    for round in 0..rounds {
        let tokens = fuzz::generate_round(seed, round, keys);
        // A round's own probe failure (an eval_str reply that never
        // arrives, distinct from and more severe than the settle-timeout
        // FailureSignature::Timeout already covers -- see this crate's own
        // module docs) must not abort the whole fuzz run via `?`: a fuzz
        // generator drawing from a wide alphabet will hit a session wedge
        // sooner or later (live-verified: seed 42 hit one at round 6 of a
        // 10-round run during this feature's own development), and every
        // round after it deserves the same chance to run as if the
        // wedge had never happened, exactly as `run_command`'s own
        // per-entry error handling already treats one entry's error as
        // that entry's failure, not the whole corpus run's.
        match run_tokens(&tokens, COLS, ROWS, silence, deadline) {
            Ok(outcome) => {
                let Some(target) = FailureSignature::from_outcome(&outcome) else {
                    continue;
                };
                match target {
                    FailureSignature::Divergence(_) => divergence_count += 1,
                    FailureSignature::Timeout { .. } => timeout_count += 1,
                }
                let minimized = minimize_tokens(tokens, target, COLS, ROWS, silence, deadline);
                let path = quarantine_entry(seed, round, &minimized, &pin)?;
                println!(
                    "fuzz: round {round} ... {target:?}, quarantined to {}",
                    path.display()
                );
            }
            Err(err) => {
                error_count += 1;
                // Minimizing an error round would itself require rerunning
                // this same probe repeatedly, and a probe error already
                // means candidate pass/fail cannot be trusted the way a
                // settled comparison can be; the raw generated script is
                // quarantined unminimized instead.
                let path = quarantine_entry(seed, round, &tokens, &pin)?;
                println!(
                    "fuzz: round {round} ... ERROR: {err}, quarantined to {}",
                    path.display()
                );
            }
        }
    }

    println!(
        "fuzz: {rounds} rounds, seed {seed} ... {divergence_count} divergences, \
         {timeout_count} timeouts, {error_count} errors"
    );

    if divergence_count > 0 || timeout_count > 0 || error_count > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Writes `tokens` to [`quarantine_path`]`(seed, round)` as a corpus entry
/// named `fuzz-<seed>-<round>`, stamped with the live `.engine-pin` value
/// `engine_pin` -- the write every non-PARITY fuzz round shares, whether
/// its script has already been minimized or (an error round) is being
/// quarantined raw.
///
/// # Errors
///
/// Returns an error if the entry cannot be written.
fn quarantine_entry(seed: u64, round: u32, tokens: &[String], engine_pin: &str) -> Result<PathBuf> {
    let path = quarantine_path(seed, round);
    let name = format!("fuzz-{seed}-{round}");
    corpus::write_entry(
        &path,
        &name,
        &join_tokens(tokens),
        engine_pin,
        "default",
        corpus::DEFAULT_QUIESCE_SILENCE_MS,
        corpus::DEFAULT_QUIESCE_DEADLINE_MS,
    )
    .with_context(|| format!("writing quarantine entry {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The scenario the merge logic exists for: an engine side that never
    /// saw a Flush must report TIMEOUT even when the reference side
    /// happened to settle with an empty divergence list. The state is
    /// reachable through a real corpus run -- an entry whose input
    /// produces no redraw starves only the engine side's Flush while the
    /// reference side's marker still settles -- so this pins the merge
    /// decision at its own seam without spawning engines.
    #[test]
    fn engine_side_timeout_is_not_masked_by_a_settled_reference() {
        assert_eq!(
            settle_status(false, true, true),
            "TIMEOUT (engine)",
            "an unsettled engine side with no divergences must not read as PARITY"
        );
        assert_eq!(
            settle_status(false, true, false),
            "TIMEOUT (engine)",
            "an unsettled engine side must not read as DIVERGENCE"
        );
    }

    #[test]
    fn reference_side_timeout_is_reported_distinctly() {
        assert_eq!(settle_status(true, false, true), "TIMEOUT (reference)");
        assert_eq!(settle_status(true, false, false), "TIMEOUT (reference)");
    }

    #[test]
    fn both_sides_unsettled_names_both() {
        assert_eq!(
            settle_status(false, false, true),
            "TIMEOUT (engine, reference)"
        );
    }

    #[test]
    fn both_settled_falls_through_to_parity_or_divergence() {
        assert_eq!(settle_status(true, true, true), "PARITY");
        assert_eq!(settle_status(true, true, false), "DIVERGENCE");
    }

    #[test]
    fn is_success_requires_both_sides_settled() {
        let base = EntryOutcome {
            engine_settled: true,
            reference_settled: true,
            elapsed_ms: 0,
            divergences: Vec::new(),
        };
        assert!(base.is_success());

        let engine_wedged = EntryOutcome {
            engine_settled: false,
            reference_settled: true,
            elapsed_ms: 0,
            divergences: Vec::new(),
        };
        assert!(!engine_wedged.is_success());

        let reference_wedged = EntryOutcome {
            engine_settled: true,
            reference_settled: false,
            elapsed_ms: 0,
            divergences: Vec::new(),
        };
        assert!(!reference_wedged.is_success());
    }

    fn divergence_outcome(kind: DivergenceKind) -> EntryOutcome {
        let divergence = match kind {
            DivergenceKind::State => Divergence::State {
                field: "buffer_lines".to_string(),
                view: "a".to_string(),
                reference: "b".to_string(),
            },
            DivergenceKind::Grid => Divergence::Grid {
                row: 0,
                view: "a".to_string(),
                reference: "b".to_string(),
            },
        };
        EntryOutcome {
            engine_settled: true,
            reference_settled: true,
            elapsed_ms: 0,
            divergences: vec![divergence],
        }
    }

    #[test]
    fn failure_signature_reads_timeout_before_divergence() {
        // An unsettled side takes priority over whatever the (possibly
        // stale, mid-processing) divergence list happens to say, mirroring
        // settle_status's own precedence.
        let outcome = EntryOutcome {
            engine_settled: false,
            reference_settled: true,
            elapsed_ms: 0,
            divergences: vec![],
        };
        assert_eq!(
            FailureSignature::from_outcome(&outcome),
            Some(FailureSignature::Timeout {
                engine_settled: false,
                reference_settled: true,
            })
        );
    }

    #[test]
    fn failure_signature_reads_the_first_divergences_kind() {
        let outcome = divergence_outcome(DivergenceKind::Grid);
        assert_eq!(
            FailureSignature::from_outcome(&outcome),
            Some(FailureSignature::Divergence(DivergenceKind::Grid))
        );
    }

    #[test]
    fn failure_signature_is_none_for_clean_parity() {
        let outcome = EntryOutcome {
            engine_settled: true,
            reference_settled: true,
            elapsed_ms: 0,
            divergences: vec![],
        };
        assert_eq!(FailureSignature::from_outcome(&outcome), None);
    }

    #[test]
    fn failure_signature_matches_requires_the_same_kind() {
        let target = FailureSignature::Divergence(DivergenceKind::State);
        assert!(target.matches(&divergence_outcome(DivergenceKind::State)));
        assert!(!target.matches(&divergence_outcome(DivergenceKind::Grid)));

        let timeout_target = FailureSignature::Timeout {
            engine_settled: false,
            reference_settled: true,
        };
        let matching_timeout = EntryOutcome {
            engine_settled: false,
            reference_settled: true,
            elapsed_ms: 0,
            divergences: vec![],
        };
        let different_timeout_shape = EntryOutcome {
            engine_settled: true,
            reference_settled: false,
            elapsed_ms: 0,
            divergences: vec![],
        };
        assert!(timeout_target.matches(&matching_timeout));
        assert!(!timeout_target.matches(&different_timeout_shape));
    }

    /// Regression guard for the `run_entry`/`run_tokens` split: every seed
    /// corpus entry's `input` must tokenize and rejoin back to itself
    /// unchanged, since `run_entry` now sends `join_tokens(tokenize(input))`
    /// to both sessions instead of `input` directly. A tokenizer bug here
    /// would silently change what every existing corpus entry actually
    /// types, without any test that spawns nvim ever needing to catch it.
    #[test]
    fn every_seed_corpus_entrys_input_round_trips_through_tokenize() {
        let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        dir.pop(); // crates/
        dir.pop(); // workspace root
        dir.push("corpus");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("failed to read corpus/") {
            let path = entry.expect("failed to read corpus/ entry").path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let corpus_entry = corpus::load_file(&path)
                .map_err(|err| format!("failed to load {}: {err}", path.display()))
                .expect("every corpus/*.toml entry must load");
            assert_eq!(
                join_tokens(&tokenize(&corpus_entry.input)),
                corpus_entry.input,
                "tokenize/join_tokens round trip failed for {}",
                path.display()
            );
            checked += 1;
        }
        assert!(
            checked > 0,
            "expected at least one corpus/*.toml entry to check"
        );
    }

    /// The falsifiable check this whole feature exists for: a script whose
    /// only source of divergence is [`INJECT_DIVERGENCE_TOKEN`], planted at
    /// the midpoint of a filler run via the hidden `--inject-divergence-at`
    /// flag, must minimize to a script no longer than that midpoint (the
    /// plant's prefix) and must still contain the planted token -- proving
    /// `minimize_command`'s real, live reduction against actual nvim
    /// sessions, not a mocked comparison. A candidate with *no* real
    /// engine-side keystrokes left at all is deliberately not expected to
    /// be the minimum: dropping every filler token alongside the sentinel
    /// starves the engine side's own Flush (a timeout, not this run's
    /// divergence), so `FailureSignature::matches` correctly refuses that
    /// smaller-looking candidate and ddmin keeps at least one real
    /// keystroke around the sentinel.
    #[test]
    fn planted_divergence_minimizes_to_no_more_than_the_plants_prefix() {
        let filler: Vec<String> = "a".repeat(30).chars().map(|c| c.to_string()).collect();
        let inject_at = filler.len() / 2;

        let dir = std::env::temp_dir().join(format!(
            "view-harness-oracle-inject-e2e-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("failed to create scratch dir");
        let path = dir.join("planted.toml");
        corpus::write_entry(
            &path,
            "planted",
            &join_tokens(&filler),
            "test-pin",
            "default",
            200,
            1_500,
        )
        .expect("failed to write scratch entry");

        minimize_command(&path, Some(inject_at)).expect("minimize_command failed");

        let minimized = corpus::load_file(&path).expect("minimized entry must still parse");
        let unminimized_len = join_tokens(&filler).len();

        assert!(
            minimized.input.contains(INJECT_DIVERGENCE_TOKEN),
            "expected the minimized entry to retain the planted divergence token, got {:?}",
            minimized.input
        );
        assert!(
            minimized.input.len() < unminimized_len,
            "expected minimize_command to shrink the entry's input ({} chars) below the \
             unminimized length ({unminimized_len} chars)",
            minimized.input.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
