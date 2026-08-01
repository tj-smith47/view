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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use portable_pty::CommandBuilder;
use view_harness::corpus::{self, CorpusEntry};
use view_harness::fixture::{
    cache_root, copy_dir_recursive, current_engine_pin, fixtures_root, lockfile_cache_key,
    scratch_root, verify_nvim_matches_pin, workspace_root,
};
use view_harness::fuzz;
use view_harness::page;
use view_harness::results::{
    load_results, write_results, ResultsFile, ScenarioResult, ScenarioStatus,
};
use view_harness::scenario::{self, ScenarioFile};
use view_oracle::compat::{CompatSession, PluginClass, ScenarioState};
use view_oracle::{
    compare, ddmin, join_tokens, masked_rows, snapshot, tokenize, Divergence, EngineSession,
    ReferenceSession, ReferenceSide, ViewSide,
};

/// Terminal size every corpus entry runs at. Fixed rather than a
/// per-entry override (unlike the quiesce timing overrides): no seed entry
/// needs a different canvas, and a shared size keeps every entry's
/// PARITY/DIVERGENCE report line directly comparable.
const COLS: u16 = 60;
const ROWS: u16 = 12;

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

/// Terminal size every compat scenario runs at: roomier than the
/// differential oracle's own fixed [`COLS`]x[`ROWS`] canvas, since a
/// compat scenario is driving real plugin UI (a statusline, a floating
/// picker) rather than a bare grid comparison and needs realistic room to
/// render in.
const COMPAT_COLS: u16 = 100;
const COMPAT_ROWS: u16 = 30;

/// Bound on [`CompatSession::prime_probe_channel`]/`await_probe_channel`'s
/// own bounded retry: generous relative to a `serverstart` call (the first
/// statement any committed fixture's `init.lua` runs, so this is really
/// bounding `view`'s own spawn + `ui_attach` handshake time, not any
/// plugin's), short enough that a session that never got that far still
/// fails a scenario promptly.
const PROBE_CHANNEL_TIMEOUT: Duration = Duration::from_secs(15);

/// [`CompatSession::wait_for_screen_quiescence`]'s window for a
/// fixture-less (daily-config) scenario: how long the screen must stay
/// unchanged, and the overall bound, before typing the priming command.
const SCREEN_QUIESCE_SILENCE: Duration = Duration::from_millis(500);
const SCREEN_QUIESCE_DEADLINE: Duration = Duration::from_secs(10);

/// Disambiguates concurrently-generated scratch paths (a hermetic XDG home,
/// a probe socket) within one process, the same role
/// `view-oracle/tests/common::ScratchPaths`' own atomic counter plays.
static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Parent of every compat scenario's scratch world and probe socket; why
/// `target/` and not the system temp dir is documented on
/// [`view_harness::fixture::scratch_root`].
fn compat_scratch_root() -> PathBuf {
    scratch_root("compat-scratch")
}

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
    /// Drives `compat/scenarios/*.toml` (or a single scenario file) through
    /// the real `view` binary over a pty, per the compat harness's own
    /// scenario schema (`view_harness::scenario`).
    Compat {
        /// Scenario file or directory.
        #[arg(default_value = "compat/scenarios")]
        path: PathBuf,
    },
    /// Renders the compat-evidence page `docs/compat.md` from the latest
    /// `compat/results.json`, refusing if the recorded engine pin no
    /// longer matches `.engine-pin` (`view_harness::page` holds the
    /// rendering and staleness logic; this arm only reads inputs and
    /// writes the file).
    Page,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Minimize {
            path,
            inject_divergence_at,
        }) => minimize_command(&path, inject_divergence_at),
        Some(Command::Fuzz { seed, rounds, keys }) => fuzz_command(seed, rounds, keys),
        Some(Command::Compat { path }) => compat_command(&path),
        Some(Command::Page) => page_command(),
        None => run_command(&cli.path),
    }
}

/// Resolves `path` into a sorted list of `(file path, parsed entry)`
/// pairs: every `*.toml` file directly inside `path` if it is a directory
/// (sorted so a corpus run's entry order is deterministic across
/// invocations and hosts), or `path` itself if it names a file.
fn collect_entries(path: &Path) -> Result<Vec<(PathBuf, CorpusEntry)>> {
    let mut files: Vec<PathBuf> = if path.is_dir() {
        // Deliberately non-recursive: this is what keeps corpus/quarantine/
        // (and any other subdirectory) out of every plain corpus-wide run
        // without a dedicated exclude list -- a walk that ever became
        // recursive would need one, to keep quarantined-but-unfixed
        // failures from silently joining the PARITY gate.
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

/// Whether the engine side -- view's own decode/apply pipeline -- reached
/// quiescence.
///
/// The two sides carry distinct types so that a call transposing them
/// cannot compile. Both are bare bools, and a transposed pair still
/// produces a status word for every combination: a wedged engine side is
/// reported as `TIMEOUT (reference)`, pointing whoever debugs it at the
/// side that exists to be trusted while view's own hang goes unnamed.
#[derive(Debug, Clone, Copy)]
struct EngineSettled(bool);

/// Whether the reference applier's side reached quiescence. See
/// [`EngineSettled`] for why the sides are separate types.
#[derive(Debug, Clone, Copy)]
struct ReferenceSettled(bool);

/// Derives the report-line status word from both sides' settle results and
/// whether any divergence was found. A free function rather than inlined in
/// [`print_outcome`] so the merge decision -- an unsettled side always wins
/// over an empty divergence list, on either side, never falling through to
/// PARITY or DIVERGENCE -- is checkable without spawning a real engine.
fn settle_status(
    engine: EngineSettled,
    reference: ReferenceSettled,
    divergences_empty: bool,
) -> String {
    let EngineSettled(engine_settled) = engine;
    let ReferenceSettled(reference_settled) = reference;
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
/// session alone. Each side still receives its whole script as exactly one
/// `nvim_input` payload either way, sentinel or not: the reference side's
/// settle protocol arms its quiesce marker in front of that payload and
/// depends on the fusion (see `ReferenceSession::arm_and_input`), so a
/// script split across several calls would leave a window for the marker to
/// fire between them and prove nothing.
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

    // startup quiescence is drained, not gated on: a slow-starting nvim's
    // own splash/plugin traffic settling late here is not itself a
    // divergence, only the post-input settle below decides pass/fail. A
    // probe error, unlike a slow settle, still propagates: it means the
    // session is broken, not merely late
    let _ = engine.quiesce(silence, deadline)?;
    let _ = reference.quiesce(silence, deadline)?;

    let (engine_keys, reference_keys) =
        match tokens.iter().position(|t| t == INJECT_DIVERGENCE_TOKEN) {
            Some(pos) => {
                let prefix = join_tokens(&tokens[..pos]);
                let suffix = join_tokens(&tokens[pos + 1..]);
                (
                    format!("{prefix}{suffix}"),
                    format!("{prefix}{INJECT_DIVERGENCE_KEYS}{suffix}"),
                )
            }
            None => {
                let joined = join_tokens(tokens);
                (joined.clone(), joined)
            }
        };
    engine.arm_and_input(&engine_keys)?;
    reference.arm_and_input(&reference_keys)?;

    let engine_settled = engine.quiesce(silence, deadline)?;
    let reference_settled = reference.quiesce(silence, deadline)?;

    let surface = engine.surface();
    let view_screen = engine.screen();
    let mask = masked_rows(&surface);
    let ref_screen = reference.screen();

    let view_state = snapshot(&mut engine)?;
    let ref_state = snapshot(&mut reference)?;

    let divergences = compare(
        ViewSide {
            state: &view_state,
            screen: &view_screen,
        },
        ReferenceSide {
            state: &ref_state,
            screen: &ref_screen,
        },
        &mask,
    );

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
        EngineSettled(outcome.engine_settled),
        ReferenceSettled(outcome.reference_settled),
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
    verify_nvim_matches_pin(Path::new("nvim"), &pin)?;
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

/// The specific failure shape a minimizer run reduces toward. A
/// [`Divergence::State`] carries its own field name (`"mode"`, `"blocked"`,
/// `"cursor"`, `"registers"`, `"marks"`, `"buffer_lines"`) into the
/// signature: two
/// state divergences on different fields are different bugs at different
/// layers -- a mode disagreement does not imply a cursor disagreement --
/// so treating every `Divergence::State` as interchangeable would let a
/// minimizer legally reduce a mode-divergence input down to an unrelated
/// cursor-only reproduction. `State` and `Grid` are separate variants
/// rather than one variant wrapping an `Option<String>`, so a `Grid`
/// signature carrying a field name or a `State` signature missing one are
/// not representable states a caller has to guard against. The
/// `view`/`reference` payload strings stay out of the signature regardless
/// of field: they are exactly what is expected to shift as tokens drop out
/// during reduction, the same reason a diverging grid row index is left
/// out of [`FailureSignature::Grid`], which stays coarse with no per-row
/// identity. [`FailureSignature::Timeout`] covers the remaining case
/// neither divergence variant reaches: which side(s) failed to settle. A
/// minimizer must never shrink a divergent script toward an unrelated
/// timeout, or a timing-out script toward an unrelated divergence, just
/// because either one happens to count as "not PARITY".
#[derive(Debug, Clone, PartialEq, Eq)]
enum FailureSignature {
    State(String),
    Grid,
    Attr,
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
        outcome.divergences.first().map(|d| match d {
            Divergence::State { field, .. } => Self::State(field.clone()),
            Divergence::Grid { .. } => Self::Grid,
            // coarse like Grid (no per-row identity, since a minimized
            // script's diverging row is expected to shift): kept a distinct
            // signature from Grid so a minimizer never reduces an
            // attr-render divergence toward an unrelated text-render one
            Divergence::Attr { .. } => Self::Attr,
        })
    }

    /// Whether `outcome` reproduces this same failure shape: the ddmin
    /// reproduction predicate every minimizer candidate is tested against.
    fn matches(&self, outcome: &EntryOutcome) -> bool {
        Self::from_outcome(outcome).as_ref() == Some(self)
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
/// Returns an error if `path` cannot be loaded, `.engine-pin` cannot be
/// read, the `nvim` on `PATH` does not report the version `.engine-pin`
/// names, the baseline run's probes fail, `path`'s entry currently
/// reproduces neither a divergence nor a timeout (nothing to minimize), or
/// the minimized entry cannot be written back.
fn minimize_command(path: &Path, inject_divergence_at: Option<usize>) -> Result<()> {
    let entry = corpus::load_file(path)
        .with_context(|| format!("loading corpus entry {}", path.display()))?;
    // same gate as every other session-spawning mode: the minimized entry
    // is rewritten still stamped with an engine pin, and a reduction
    // performed by an off-pin nvim would re-author the entry against a
    // version its pin field does not name
    let pin = current_engine_pin()?;
    verify_nvim_matches_pin(Path::new("nvim"), &pin)?;
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
        // the just-verified current pin, not entry.engine_pin: the
        // minimized input's reproduction was established on this run's
        // binary, so a stale authored-against pin must not be carried
        // forward as if it were the evidence's provenance
        &pin,
        &entry.ext_set,
        entry.quiesce_silence_ms,
        entry.quiesce_deadline_ms,
    )
    .with_context(|| format!("writing minimized entry back to {}", path.display()))?;

    Ok(())
}

/// Directory quarantined fuzz-discovered failures are written to: durable,
/// reviewable artifacts (matching `corpus/`'s own convention), not scratch
/// files that vanish after one run. Takes the containing directory as a
/// parameter (rather than hardcoding `corpus/quarantine`) so a pinning
/// test can point it at a scratch directory instead of writing into the
/// real corpus tree.
fn quarantine_path(dir: &Path, seed: u64, round: u32) -> PathBuf {
    dir.join(format!("fuzz-{seed}-{round}.toml"))
}

/// One fuzz campaign's outcome tally: how many rounds landed in each of
/// the three buckets [`fuzz_rounds`]'s per-round match can produce.
/// Returned as data rather than printed inline so a pinning test can
/// assert on the counts directly instead of parsing captured stdout.
#[derive(Debug, Default, PartialEq, Eq)]
struct FuzzSummary {
    divergence_count: u32,
    timeout_count: u32,
    error_count: u32,
}

/// The quiesce silence/deadline pair a fuzz campaign's probe and minimizer
/// calls settle against, grouped into one value rather than two more
/// positional `Duration` parameters on [`fuzz_rounds`]: the pairing itself
/// is what must stay single-sourced (see that function's own docs), so
/// giving it a name keeps a future caller from passing the two durations
/// in the wrong order or from only one of them.
#[derive(Debug, Clone, Copy)]
struct QuiesceWindow {
    silence: Duration,
    deadline: Duration,
}

/// Drives `rounds` generated scripts through `probe`, an injectable
/// closure seam matching the one [`ddmin`] already takes for its own
/// candidate checks: a live campaign passes a closure that spawns nvim
/// through [`run_tokens`]; a pinning test passes a fake one that returns a
/// scripted result sequence with no engine involved. `quiesce` is the
/// caller's already-derived timing window, threaded through rather than
/// rederived here, so [`fuzz_command`] stays the one place that turns
/// corpus defaults or a future CLI override into the timing values both
/// the probe closure and this function's own minimizer call run at. Every
/// round's result is matched, never unwrapped with `?` -- a
/// round's own probe failure (an eval_str reply that never arrives,
/// distinct from and more severe than the settle-timeout
/// `FailureSignature::Timeout` already covers) must not abort the whole
/// campaign: a wide-alphabet fuzz campaign will hit session wedges sooner
/// or later, and each one is a finding worth recording, not a reason to
/// abort the run -- every round after it deserves the same chance to run
/// as if the wedge had never happened, exactly as `run_command`'s own
/// per-entry error handling already treats one entry's error as that
/// entry's failure, not the whole corpus run's. Swapping this match for
/// `probe(&tokens)?` would compile cleanly and pass every test that only
/// exercises the happy path, then silently truncate a live campaign the
/// first time a round's probe errors -- the reason this seam exists is to
/// let a test drive that exact scenario without spawning nvim.
///
/// # Errors
///
/// Returns an error if a quarantine entry cannot be written.
fn fuzz_rounds<P>(
    seed: u64,
    rounds: u32,
    keys: usize,
    pin: &str,
    quarantine_dir: &Path,
    quiesce: QuiesceWindow,
    mut probe: P,
) -> Result<FuzzSummary>
where
    P: FnMut(&[String]) -> Result<EntryOutcome, view_oracle::OracleError>,
{
    let mut summary = FuzzSummary::default();

    for round in 0..rounds {
        let tokens = fuzz::generate_round(seed, round, keys);
        match probe(&tokens) {
            Ok(outcome) => {
                let Some(target) = FailureSignature::from_outcome(&outcome) else {
                    continue;
                };
                match &target {
                    FailureSignature::State(_)
                    | FailureSignature::Grid
                    | FailureSignature::Attr => {
                        summary.divergence_count += 1;
                    }
                    FailureSignature::Timeout { .. } => summary.timeout_count += 1,
                }
                let minimized = minimize_tokens(
                    tokens,
                    target.clone(),
                    COLS,
                    ROWS,
                    quiesce.silence,
                    quiesce.deadline,
                );
                let path = quarantine_entry(quarantine_dir, seed, round, &minimized, pin)?;
                println!(
                    "fuzz: round {round} ... {target:?}, quarantined to {}",
                    path.display()
                );
            }
            Err(err) => {
                summary.error_count += 1;
                // Minimizing an error round would itself require rerunning
                // this same probe repeatedly, and a probe error already
                // means candidate pass/fail cannot be trusted the way a
                // settled comparison can be; the raw generated script is
                // quarantined unminimized instead.
                let path = quarantine_entry(quarantine_dir, seed, round, &tokens, pin)?;
                println!(
                    "fuzz: round {round} ... ERROR: {err}, quarantined to {}",
                    path.display()
                );
            }
        }
    }

    Ok(summary)
}

/// The `fuzz --seed N` subcommand: generates `rounds` reproducible random
/// scripts from `seed` (see [`fuzz::generate_round`]), drives each through
/// [`run_tokens`] via [`fuzz_rounds`]'s probe seam, and for any round that
/// is not clean PARITY, minimizes it toward its own failure shape and
/// writes the (already minimized) result under `corpus/quarantine`. The
/// sole site that turns the corpus quiesce defaults into a
/// [`QuiesceWindow`]: both the probe closure below and `fuzz_rounds`' own
/// internal minimizer call run at the same values threaded in from here,
/// so a future CLI override only ever needs to change this one derivation
/// to reach both.
///
/// # Errors
///
/// Returns an error if `.engine-pin` cannot be read, the `nvim` on `PATH`
/// does not report the version `.engine-pin` names, or a quarantine entry
/// cannot be written.
fn fuzz_command(seed: u64, rounds: u32, keys: usize) -> Result<()> {
    let quiesce = QuiesceWindow {
        silence: Duration::from_millis(corpus::DEFAULT_QUIESCE_SILENCE_MS),
        deadline: Duration::from_millis(corpus::DEFAULT_QUIESCE_DEADLINE_MS),
    };
    let pin = current_engine_pin()?;
    verify_nvim_matches_pin(Path::new("nvim"), &pin)?;

    let summary = fuzz_rounds(
        seed,
        rounds,
        keys,
        &pin,
        Path::new("corpus/quarantine"),
        quiesce,
        |tokens| run_tokens(tokens, COLS, ROWS, quiesce.silence, quiesce.deadline),
    )?;

    println!(
        "fuzz: {rounds} rounds, seed {seed} ... {} divergences, {} timeouts, {} errors",
        summary.divergence_count, summary.timeout_count, summary.error_count
    );

    if summary.divergence_count > 0 || summary.timeout_count > 0 || summary.error_count > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// Writes `tokens` to [`quarantine_path`]`(dir, seed, round)` as a corpus
/// entry named `fuzz-<seed>-<round>`, stamped with the live `.engine-pin`
/// value `engine_pin` -- the write every non-PARITY fuzz round shares,
/// whether its script has already been minimized or (an error round) is
/// being quarantined raw.
///
/// # Errors
///
/// Returns an error if the entry cannot be written.
fn quarantine_entry(
    dir: &Path,
    seed: u64,
    round: u32,
    tokens: &[String],
    engine_pin: &str,
) -> Result<PathBuf> {
    let path = quarantine_path(dir, seed, round);
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

/// Builds the `view` binary (always, not gated on an existence check -- see
/// `view-oracle/tests/common::view_bin_path`'s own doc comment for why a
/// stale binary is worse than one extra up-to-date `cargo build`) and
/// returns its path.
///
/// # Errors
///
/// Returns an error if `cargo build -p view` cannot be invoked or fails.
fn ensure_view_bin() -> Result<PathBuf> {
    let profile_dir = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let path = workspace_root()
        .join("target")
        .join(profile_dir)
        .join("view");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = std::process::Command::new(&cargo)
        .args(["build", "-p", "view"])
        .status()
        .context("failed to invoke cargo build -p view")?;
    if !status.success() {
        bail!("cargo build -p view failed");
    }
    Ok(path)
}

/// `YYYY-MM-DD` for the current instant, in UTC. Hand-rolled rather than a
/// `chrono`/`time` dependency: this is the only date computation anywhere
/// in the workspace, for one report-row stamp
/// ([`view_harness::results::ScenarioResult::date`]).
fn today_date_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's days-since-epoch -> proleptic Gregorian civil date
/// algorithm (public domain: <http://howardhinnant.github.io/date_algorithms.html>),
/// pinned by [`civil_from_days_matches_known_dates`] against independently
/// computed reference values rather than trusted from transcription alone.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Removes the scratch state a compat scenario run created once the
/// scenario finishes, on every path (success, failure, or an early `?`
/// return) via `Drop` rather than a manual cleanup call at each return
/// site. `cold_cache_dir` is only ever `Some` for a `cold_bootstrap`
/// scenario's own run-unique cache key -- the normal, shared
/// `compat/.cache/<hash>/` a warm run reuses is never touched here.
struct ScenarioScratch {
    hermetic_dir: PathBuf,
    cold_cache_dir: Option<PathBuf>,
    sock_path: PathBuf,
}

impl Drop for ScenarioScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.hermetic_dir);
        if let Some(dir) = &self.cold_cache_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

/// A fixture resolved to concrete XDG homes a `view` invocation can be
/// spawned against.
struct ReadyFixture {
    xdg_config_home: PathBuf,
    xdg_data_home: PathBuf,
    xdg_state_home: PathBuf,
    xdg_cache_home: PathBuf,
    /// Whether the driver itself must type `:call serverstart(...)` --
    /// true only for a fixture-less (daily-config) scenario, whose
    /// `init.lua` this harness does not own and so cannot rely on already
    /// carrying that call.
    needs_priming: bool,
    /// Held only for its `Drop` cleanup; never read.
    _scratch: ScenarioScratch,
}

/// [`resolve_fixture`]'s result: either a [`ReadyFixture`] to spawn `view`
/// against, or a reason to report the scenario SKIPPED without spawning
/// anything (today, only "fixture-less and `$VIEW_DAILY_CONFIG` unset").
enum FixtureResolution {
    // Boxed so the enum is not sized to this large variant next to the tiny
    // `Skipped`: on the msvc target `PathBuf` is wide enough that the four here
    // trip clippy::large_enum_variant, which `-D warnings` makes a Windows CI
    // hard error while linux stays just under the threshold.
    Ready(Box<ReadyFixture>),
    Skipped { notice: String },
}

/// Resolves `scenario`'s `fixture` field (or its absence) into a
/// [`FixtureResolution`]: XDG homes to spawn `view` against, plus a
/// [`ScenarioScratch`] guard that cleans up every scratch path this
/// function created once the caller's session finishes and the guard
/// drops. `sock_path` is threaded in (not generated here) so the caller's
/// own `CompatSession::spawn_configured` and this resolution agree on
/// exactly one socket path.
///
/// A named fixture's `XDG_CONFIG_HOME` always points at a per-run copy
/// under `hermetic_dir`, never `compat/fixtures/<name>` itself: a plugin
/// manager sourced from its own config directory can rewrite files inside
/// it in place (lazy.nvim's own lockfile, in particular), so spawning
/// `view` with the checked-in fixture tree itself as its config home would
/// leave the committed fixture modified on disk after every run.
///
/// # Errors
///
/// Returns an error if a named fixture has no `nvim/init.lua`, its
/// `lazy-lock.json` cannot be read, the fixture cannot be copied into a
/// hermetic config dir, `$VIEW_DAILY_CONFIG` names a directory with no
/// `init.lua`/`init.vim`, or (fixture-less, non-Unix host) the isolated
/// config symlink cannot be created.
fn resolve_fixture(scenario: &ScenarioFile, sock_path: &Path) -> Result<FixtureResolution> {
    let scratch_id = format!(
        "{}-{}",
        std::process::id(),
        SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let hermetic_dir = compat_scratch_root().join(format!("view-compat-{scratch_id}"));
    std::fs::create_dir_all(&hermetic_dir)
        .with_context(|| format!("creating scratch dir {}", hermetic_dir.display()))?;
    let xdg_state_home = hermetic_dir.join("xdg_state_home");
    let xdg_cache_home = hermetic_dir.join("xdg_cache_home");

    match &scenario.fixture {
        Some(name) => {
            let fixture_dir = fixtures_root().join(name);
            let init_lua = fixture_dir.join("nvim").join("init.lua");
            if !init_lua.exists() {
                bail!(
                    "fixture {name:?} has no {} (compat/fixtures/{name}/nvim/init.lua)",
                    init_lua.display()
                );
            }
            let lockfile_path = fixture_dir.join("nvim").join("lazy-lock.json");
            let xdg_data_home = if lockfile_path.exists() {
                let bytes = std::fs::read(&lockfile_path)
                    .with_context(|| format!("reading {}", lockfile_path.display()))?;
                let key = if scenario.cold_bootstrap {
                    format!("cold-{scratch_id}")
                } else {
                    lockfile_cache_key(&bytes)
                };
                cache_root().join(key)
            } else {
                hermetic_dir.join("xdg_data_home")
            };
            let cold_cache_dir = scenario.cold_bootstrap.then(|| xdg_data_home.clone());

            let xdg_config_home = hermetic_dir.join("xdg_config_home");
            copy_dir_recursive(&fixture_dir, &xdg_config_home)
                .with_context(|| format!("copying fixture {name:?} into a hermetic config dir"))?;

            Ok(FixtureResolution::Ready(Box::new(ReadyFixture {
                xdg_config_home,
                xdg_data_home,
                xdg_state_home,
                xdg_cache_home,
                needs_priming: false,
                _scratch: ScenarioScratch {
                    hermetic_dir,
                    cold_cache_dir,
                    sock_path: sock_path.to_path_buf(),
                },
            })))
        }
        None => {
            let Ok(daily) = std::env::var("VIEW_DAILY_CONFIG") else {
                let _ = std::fs::remove_dir_all(&hermetic_dir);
                return Ok(FixtureResolution::Skipped {
                    notice: "VIEW_DAILY_CONFIG is unset; fixture-less scenario skipped".to_string(),
                });
            };
            let daily_path = PathBuf::from(&daily);
            if !daily_path.join("init.lua").exists() && !daily_path.join("init.vim").exists() {
                bail!("VIEW_DAILY_CONFIG={daily} has no init.lua/init.vim");
            }
            let xdg_config_home = hermetic_dir.join("xdg_config_home");
            std::fs::create_dir_all(&xdg_config_home)
                .with_context(|| format!("creating {}", xdg_config_home.display()))?;
            symlink_daily_config(&daily_path, &xdg_config_home.join("nvim"))?;
            Ok(FixtureResolution::Ready(Box::new(ReadyFixture {
                xdg_config_home,
                xdg_data_home: hermetic_dir.join("xdg_data_home"),
                xdg_state_home,
                xdg_cache_home,
                needs_priming: true,
                _scratch: ScenarioScratch {
                    hermetic_dir,
                    cold_cache_dir: None,
                    sock_path: sock_path.to_path_buf(),
                },
            })))
        }
    }
}

/// Links `link` (inside a per-run hermetic `XDG_CONFIG_HOME`) to `target`
/// (`$VIEW_DAILY_CONFIG`'s real path), so the maintainer's actual nvim
/// config is what `view` sources while every *other* XDG home
/// (state/data/cache) stays per-run hermetic. Unix-only (symlinks): the
/// daily-config scenario is a maintainer-machine standing scenario, not a
/// CI-gated one, so a non-Unix host simply cannot run it yet.
///
/// # Errors
///
/// Returns an error if the symlink cannot be created, or (non-Unix) always.
#[cfg(unix)]
fn symlink_daily_config(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("symlinking {} -> {}", link.display(), target.display()))
}

#[cfg(not(unix))]
fn symlink_daily_config(_target: &Path, _link: &Path) -> Result<()> {
    bail!("VIEW_DAILY_CONFIG scenarios require a Unix symlink-capable host")
}

fn class_str(class: PluginClass) -> &'static str {
    match class {
        PluginClass::Semantic => "semantic",
        PluginClass::UiAdjacent => "ui-adjacent",
        PluginClass::UiOwning => "ui-owning",
    }
}

fn state_str(state: ScenarioState) -> &'static str {
    match state {
        ScenarioState::Present => "present",
    }
}

/// Best-effort plugin commit lookup from a named fixture's `lazy-lock.json`,
/// for [`ScenarioResult::plugin_version`]'s row in the design spec's own
/// compat-evidence schema ("plugin, version, engine pin, ..."). Tries
/// `scenario.plugin` as a literal lockfile key first (a plugin spec'd
/// without lazy.nvim's default `<repo>.nvim` naming), then with a `.nvim`
/// suffix (lazy.nvim's own default when a spec sets no custom `name`),
/// then with a `.lua` suffix (the other repo-naming convention in the
/// committed `heavy` fixture: `nvim-tree.lua`). Returns `None`
/// (never an error) for a fixture-less scenario, a fixture with no
/// lockfile, or a plugin name the lockfile does not contain -- a missing
/// version is a gap in the report, not a reason to fail the scenario that
/// already passed or failed on its own merits.
fn resolve_plugin_version(scenario: &ScenarioFile) -> Option<String> {
    let name = scenario.fixture.as_ref()?;
    let lockfile_path = fixtures_root()
        .join(name)
        .join("nvim")
        .join("lazy-lock.json");
    let text = std::fs::read_to_string(lockfile_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let obj = json.as_object()?;
    // candidates probed in a fixed preference order (exact name first, then
    // the common repo-naming suffixes) so that a lockfile holding more than
    // one candidate key for a plugin resolves by intent, not map iteration
    // order
    let suffixed_nvim = format!("{}.nvim", scenario.plugin);
    let suffixed_lua = format!("{}.lua", scenario.plugin);
    let key = [scenario.plugin.as_str(), &suffixed_nvim, &suffixed_lua]
        .into_iter()
        .find(|candidate| obj.contains_key(*candidate))?;
    let commit = obj.get(key)?.get("commit")?.as_str()?;
    Some(commit.get(..7).unwrap_or(commit).to_string())
}

/// Builds a [`ScenarioResult`] for a scenario that never spawned a session
/// (SKIPPED) or whose session failed before or during a step
/// (`failing_step`/`detail` set; `failing_step == Some(scenario.steps.len())`
/// means the implicit zero-error epilogue is what failed, not a scripted
/// step). Shared by every non-OK exit path in [`run_scenario`] and
/// [`compat_command`]'s own top-level `Err` catch, so the report row shape
/// is defined exactly once.
fn scenario_result(
    scenario_path: &Path,
    scenario: &ScenarioFile,
    pin: &str,
    status: ScenarioStatus,
    failing_step: Option<usize>,
    detail: Option<String>,
    elapsed_ms: u128,
) -> ScenarioResult {
    ScenarioResult {
        scenario_path: scenario_path.display().to_string(),
        plugin: scenario.plugin.clone(),
        plugin_version: resolve_plugin_version(scenario),
        class: class_str(scenario.class).to_string(),
        fixture: scenario.fixture.clone(),
        state: state_str(scenario.state).to_string(),
        engine_pin: pin.to_string(),
        status,
        failing_step,
        steps_total: scenario.steps.len(),
        detail,
        elapsed_ms,
        date: today_date_string(),
    }
}

/// The binary under test: the `view` build a scenario spawns as its
/// session.
///
/// The two sides carry distinct types so that a call transposing them
/// cannot compile. Both are paths, and a scenario driven with the sides
/// swapped runs bare nvim as the session and hands `view` to it as the
/// engine to embed: the run still spawns, still settles and still reports,
/// with the reference side of the differential recorded as the side under
/// test and every PARITY line describing nvim against itself.
#[derive(Debug, Clone, Copy)]
struct ViewBin<'a>(&'a Path);

/// The pinned engine binary a scenario's session embeds, and the reference
/// side of the differential. See [`ViewBin`] for why the sides are separate
/// types.
#[derive(Debug, Clone, Copy)]
struct NvimBin<'a>(&'a Path);

/// Drives one scenario end to end: resolves its fixture, spawns `view`
/// against it, opens the probe channel, runs every step in order, then the
/// implicit zero-error epilogue, and always attempts a clean `:qa!` shutdown
/// regardless of outcome. Never propagates a step/probe failure as an `Err`
/// -- those become a [`ScenarioStatus::Failed`] result, the same tolerance
/// `run_tokens`'s own callers apply to a corpus entry's failure, so one
/// scenario's wedge cannot abort the whole compat run. Only a resolution
/// failure that means no session could even be attempted (a missing
/// fixture, an unreadable lockfile) surfaces as `Err`.
///
/// # Errors
///
/// Returns an error if `scenario`'s fixture cannot be resolved.
fn run_scenario(
    scenario_path: &Path,
    scenario: &ScenarioFile,
    pin: &str,
    view_bin: ViewBin<'_>,
    nvim_bin: NvimBin<'_>,
) -> Result<ScenarioResult> {
    let ViewBin(view_bin) = view_bin;
    let NvimBin(nvim_bin) = nvim_bin;
    let start = Instant::now();
    let sock_path = compat_scratch_root().join(format!(
        "view-compat-{}-{}.sock",
        std::process::id(),
        SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let ready = match resolve_fixture(scenario, &sock_path)? {
        FixtureResolution::Ready(ready) => ready,
        FixtureResolution::Skipped { notice } => {
            return Ok(scenario_result(
                scenario_path,
                scenario,
                pin,
                ScenarioStatus::Skipped,
                None,
                Some(notice),
                0,
            ));
        }
    };

    let mut cmd = CommandBuilder::new(view_bin);
    cmd.env("XDG_CONFIG_HOME", &ready.xdg_config_home);
    cmd.env("XDG_DATA_HOME", &ready.xdg_data_home);
    cmd.env("XDG_STATE_HOME", &ready.xdg_state_home);
    cmd.env("XDG_CACHE_HOME", &ready.xdg_cache_home);
    cmd.env("VIEW_COMPAT_SOCK", &sock_path);

    let mut session = match CompatSession::spawn_configured(
        cmd,
        COMPAT_COLS,
        COMPAT_ROWS,
        nvim_bin.to_path_buf(),
        sock_path.clone(),
    ) {
        Ok(session) => session,
        Err(err) => {
            return Ok(scenario_result(
                scenario_path,
                scenario,
                pin,
                ScenarioStatus::Failed,
                None,
                Some(err.to_string()),
                start.elapsed().as_millis(),
            ));
        }
    };

    let channel_result = if ready.needs_priming {
        // Best-effort settle before typing the priming command: a daily
        // config's own startup content is unknown to this harness (see
        // wait_for_screen_quiescence's own doc comment), so an unsettled
        // screen here does not itself abort the scenario -- the priming
        // retry loop right below is what actually confirms success.
        let _ = session.wait_for_screen_quiescence(SCREEN_QUIESCE_SILENCE, SCREEN_QUIESCE_DEADLINE);
        session.prime_probe_channel(PROBE_CHANNEL_TIMEOUT)
    } else {
        session.await_probe_channel(PROBE_CHANNEL_TIMEOUT)
    };
    if let Err(err) = channel_result {
        // kill alone only requests termination; reaping (bounded, matching
        // PtySession::wait_for_exit's own kill-then-wait standard) is what
        // keeps a channel-failure exit from leaving a zombie entry in the
        // process table for the rest of this run
        session.pty().kill();
        let _ = session.pty().wait_for_exit(Duration::from_secs(2));
        return Ok(scenario_result(
            scenario_path,
            scenario,
            pin,
            ScenarioStatus::Failed,
            None,
            Some(err.to_string()),
            start.elapsed().as_millis(),
        ));
    }

    let mut failing_step = None;
    let mut detail = None;
    for (index, step) in scenario.steps.iter().enumerate() {
        if let Err(err) = session.drive_step(step) {
            failing_step = Some(index);
            detail = Some(err.to_string());
            break;
        }
    }
    if failing_step.is_none() {
        if let Err(err) = session.zero_error_check() {
            failing_step = Some(scenario.steps.len());
            detail = Some(err.to_string());
        }
    }

    // Best-effort clean shutdown regardless of outcome, so a scenario never
    // leaves a `view` process running past its own run; failures here are
    // not this scenario's own result (a session that already failed a step
    // may well fail to reach a cmdline prompt to type `:qa!` into).
    let _ = session.pty().send(b"\x1b:qa!\r");
    let _ = session.pty().wait_for_exit(Duration::from_secs(5));

    let status = if failing_step.is_some() {
        ScenarioStatus::Failed
    } else {
        ScenarioStatus::Ok
    };
    Ok(scenario_result(
        scenario_path,
        scenario,
        pin,
        status,
        failing_step,
        detail,
        start.elapsed().as_millis(),
    ))
}

/// Resolves `path` into a sorted list of `(file path, parsed scenario)`
/// pairs, mirroring [`collect_entries`]'s own non-recursive directory walk.
fn collect_scenarios(path: &Path) -> Result<Vec<(PathBuf, ScenarioFile)>> {
    let mut files: Vec<PathBuf> = if path.is_dir() {
        std::fs::read_dir(path)
            .with_context(|| format!("reading scenario directory {}", path.display()))?
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
            let scenario = scenario::load_file(&path)
                .with_context(|| format!("loading scenario {}", path.display()))?;
            Ok((path, scenario))
        })
        .collect()
}

/// Prints one scenario's report line in a fixed shape:
/// `compat: lualine (heavy, present) ... OK (4 steps, 2.1s)`.
fn print_scenario_result(result: &ScenarioResult) {
    let fixture = result.fixture.as_deref().unwrap_or("none");
    // the scenario file's own stem, not result.plugin: more than one
    // scenario file can share a plugin name (lualine.toml and
    // cold-bootstrap.toml are both "lualine"), which would otherwise make
    // the two indistinguishable in the report
    let scenario = Path::new(&result.scenario_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(result.scenario_path.as_str());
    let secs = result.elapsed_ms as f64 / 1000.0;
    match result.status {
        ScenarioStatus::Ok => println!(
            "compat: {scenario} ({fixture}, {}) ... OK ({} steps, {secs:.1}s)",
            result.state, result.steps_total
        ),
        ScenarioStatus::Failed => {
            let step_label = result
                .failing_step
                .map_or_else(|| "epilogue".to_string(), |i| i.to_string());
            println!(
                "compat: {scenario} ({fixture}, {}) ... FAILED at step {step_label} ({} steps total, {secs:.1}s): {}",
                result.state,
                result.steps_total,
                result.detail.as_deref().unwrap_or("unknown failure")
            );
        }
        ScenarioStatus::Skipped => println!(
            "compat: {scenario} ({fixture}, {}) ... SKIPPED: {}",
            result.state,
            result.detail.as_deref().unwrap_or("")
        ),
    }
}

/// The `compat [PATH]` subcommand: every scenario under `path` (default
/// `compat/scenarios`), reported per [`print_scenario_result`] and written
/// to `compat/results.json` for the `page` subcommand to render.
/// Exit code: 0 unless at least one scenario reports
/// [`ScenarioStatus::Failed`] -- a SKIPPED scenario (no daily config on
/// this host, the expected state in CI) does not fail the run, since there
/// is no daily config on that host for the scenario to actually exercise.
///
/// # Errors
///
/// Returns an error if no scenario files are found under `path`, a
/// scenario file fails to parse, `.engine-pin` cannot be read, the `nvim`
/// on `PATH` does not report the version `.engine-pin` names, `view`
/// cannot be built, or `compat/results.json` cannot be written.
fn compat_command(path: &Path) -> Result<()> {
    let scenarios = collect_scenarios(path)?;
    if scenarios.is_empty() {
        bail!(
            "no scenario files found under {} (expected *.toml files)",
            path.display()
        );
    }

    let pin = current_engine_pin()?;
    let nvim_bin = PathBuf::from("nvim");
    verify_nvim_matches_pin(&nvim_bin, &pin)?;
    let view_bin = ensure_view_bin()?;
    std::fs::create_dir_all(cache_root()).context("creating compat/.cache")?;

    // Drop-based cleanup is skipped when a run is killed by a signal, so
    // stale scratch worlds from interrupted runs would otherwise pile up
    // silently. Clearing the whole parent is safe because concurrent
    // compat runs are already out of contract: both would rewrite
    // compat/results.json wholesale, clobbering each other's evidence.
    let _ = std::fs::remove_dir_all(compat_scratch_root());
    std::fs::create_dir_all(compat_scratch_root())
        .with_context(|| format!("creating scratch root {}", compat_scratch_root().display()))?;

    let mut results = ResultsFile::default();
    let mut any_failed = false;
    for (scenario_path, scenario) in &scenarios {
        let result = match run_scenario(
            scenario_path,
            scenario,
            &pin,
            ViewBin(&view_bin),
            NvimBin(&nvim_bin),
        ) {
            Ok(result) => result,
            Err(err) => scenario_result(
                scenario_path,
                scenario,
                &pin,
                ScenarioStatus::Failed,
                None,
                Some(err.to_string()),
                0,
            ),
        };
        print_scenario_result(&result);
        if result.status == ScenarioStatus::Failed {
            any_failed = true;
        }
        results.results.push(result);
    }

    write_results(
        &workspace_root().join("compat").join("results.json"),
        &results,
    )
    .context("writing compat/results.json")?;

    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

/// The `page` subcommand: reads the latest `compat/results.json` and
/// `.engine-pin`, renders the compat-evidence page via
/// [`view_harness::page::render_page`], and writes `docs/compat.md`. The
/// staleness refusal lives in the renderer; a stale pin surfaces here as
/// an `Err`, which exits 1 with the drift named.
///
/// # Errors
///
/// Returns an error if `compat/results.json` or `.engine-pin` cannot be
/// read, the results are empty or stale relative to the current pin, or
/// `docs/compat.md` cannot be written.
fn page_command() -> Result<()> {
    let root = workspace_root();
    let results = load_results(&root.join("compat").join("results.json"))?;
    let pin = current_engine_pin()?;
    let rendered = page::render_page(&results, &pin)?;

    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&docs_dir)
        .with_context(|| format!("creating {}", docs_dir.display()))?;
    let out_path = docs_dir.join("compat.md");
    std::fs::write(&out_path, &rendered.markdown)
        .with_context(|| format!("writing {}", out_path.display()))?;

    println!(
        "wrote docs/compat.md ({} rows, pin {}, {})",
        rendered.rows, rendered.engine_pin, rendered.run_date
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Reference values independently computed via Python's
    /// `datetime.date` (`epoch + timedelta(days=N)`), not transcribed from
    /// the Hinnant algorithm's own worked examples -- an independent
    /// derivation path, per this codebase's own re-derive-don't-recognize
    /// standard, catches a transcription bug a self-referential check
    /// would not.
    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        assert_eq!(civil_from_days(366), (1971, 1, 2));
        assert_eq!(civil_from_days(1000), (1972, 9, 27));
        assert_eq!(civil_from_days(19570), (2023, 8, 1));
        assert_eq!(civil_from_days(20653), (2026, 7, 19));
    }

    /// The committed heavy fixture pins nvim-tree under its real repo name
    /// `nvim-tree.lua`, while the scenario names the plugin `nvim-tree`:
    /// the lockfile lookup must bridge the `.lua` repo-naming convention
    /// the same way it bridges lazy.nvim's default `.nvim` suffix, or the
    /// evidence page's version cell goes blank for a plugin the lockfile
    /// does pin.
    #[test]
    fn plugin_version_resolves_lua_suffixed_lockfile_key() {
        let scenario = ScenarioFile {
            plugin: "nvim-tree".to_string(),
            class: PluginClass::UiOwning,
            fixture: Some("heavy".to_string()),
            state: ScenarioState::Present,
            cold_bootstrap: false,
            steps: Vec::new(),
        };
        let version = resolve_plugin_version(&scenario);
        assert_eq!(
            version.as_deref(),
            Some("4213bd6"),
            "heavy fixture's lazy-lock.json pins nvim-tree.lua at 4213bd6..."
        );
    }

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
            settle_status(EngineSettled(false), ReferenceSettled(true), true),
            "TIMEOUT (engine)",
            "an unsettled engine side with no divergences must not read as PARITY"
        );
        assert_eq!(
            settle_status(EngineSettled(false), ReferenceSettled(true), false),
            "TIMEOUT (engine)",
            "an unsettled engine side must not read as DIVERGENCE"
        );
    }

    #[test]
    fn reference_side_timeout_is_reported_distinctly() {
        assert_eq!(
            settle_status(EngineSettled(true), ReferenceSettled(false), true),
            "TIMEOUT (reference)"
        );
        assert_eq!(
            settle_status(EngineSettled(true), ReferenceSettled(false), false),
            "TIMEOUT (reference)"
        );
    }

    #[test]
    fn both_sides_unsettled_names_both() {
        assert_eq!(
            settle_status(EngineSettled(false), ReferenceSettled(false), true),
            "TIMEOUT (engine, reference)"
        );
    }

    #[test]
    fn both_settled_falls_through_to_parity_or_divergence() {
        assert_eq!(
            settle_status(EngineSettled(true), ReferenceSettled(true), true),
            "PARITY"
        );
        assert_eq!(
            settle_status(EngineSettled(true), ReferenceSettled(true), false),
            "DIVERGENCE"
        );
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

    fn state_divergence_outcome(field: &str) -> EntryOutcome {
        EntryOutcome {
            engine_settled: true,
            reference_settled: true,
            elapsed_ms: 0,
            divergences: vec![Divergence::State {
                field: field.to_string(),
                view: "a".to_string(),
                reference: "b".to_string(),
            }],
        }
    }

    fn grid_divergence_outcome() -> EntryOutcome {
        EntryOutcome {
            engine_settled: true,
            reference_settled: true,
            elapsed_ms: 0,
            divergences: vec![Divergence::Grid {
                row: 0,
                view: "a".to_string(),
                reference: "b".to_string(),
            }],
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
        let outcome = grid_divergence_outcome();
        assert_eq!(
            FailureSignature::from_outcome(&outcome),
            Some(FailureSignature::Grid)
        );
    }

    #[test]
    fn failure_signature_reads_the_diverging_states_field_name() {
        let outcome = state_divergence_outcome("mode");
        assert_eq!(
            FailureSignature::from_outcome(&outcome),
            Some(FailureSignature::State("mode".to_string()))
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
        let target = FailureSignature::Grid;
        assert!(target.matches(&grid_divergence_outcome()));
        assert!(!target.matches(&state_divergence_outcome("mode")));

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

    #[test]
    fn failure_signature_state_divergences_on_different_fields_do_not_match() {
        // The field name alone distinguishes them: a mode disagreement
        // and a cursor disagreement are different bugs, so a minimizer
        // targeting one must reject a candidate that only reproduces the
        // other, even though both are FailureSignature::State.
        let mode_target = FailureSignature::State("mode".to_string());
        assert!(mode_target.matches(&state_divergence_outcome("mode")));
        assert!(!mode_target.matches(&state_divergence_outcome("cursor")));
        assert!(!mode_target.matches(&state_divergence_outcome("registers")));
        assert!(!mode_target.matches(&state_divergence_outcome("marks")));
        assert!(!mode_target.matches(&state_divergence_outcome("buffer_lines")));
    }

    #[test]
    fn failure_signature_state_divergences_on_the_same_field_match_despite_different_payload() {
        // Payload-insensitivity: row/content values are expected to shift
        // as tokens drop out during reduction, so only the field name --
        // never the view/reference contents -- is part of the identity.
        let target = FailureSignature::State("buffer_lines".to_string());
        let outcome = EntryOutcome {
            engine_settled: true,
            reference_settled: true,
            elapsed_ms: 0,
            divergences: vec![Divergence::State {
                field: "buffer_lines".to_string(),
                view: "totally different content".to_string(),
                reference: "still different".to_string(),
            }],
        };
        assert!(target.matches(&outcome));
    }

    #[test]
    fn failure_signature_grid_divergences_stay_coarse_on_row_index() {
        // Grid is deliberately not sharpened the way State was: a
        // diverging row index is exactly the kind of value expected to
        // shift during reduction, same as FailureSignature::Grid already
        // discards it.
        let target = FailureSignature::Grid;
        let outcome = EntryOutcome {
            engine_settled: true,
            reference_settled: true,
            elapsed_ms: 0,
            divergences: vec![Divergence::Grid {
                row: 7,
                view: "x".to_string(),
                reference: "y".to_string(),
            }],
        };
        assert!(target.matches(&outcome));
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
        assert_eq!(
            minimized.engine_pin,
            current_engine_pin().expect("reading .engine-pin"),
            "the rewritten entry must be stamped with the pin the run was verified \
             against, not the scratch entry's authored-against value"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pins `fuzz_rounds`'s continue-past-error contract with a fake probe,
    /// no nvim involved: round `error_round` of `rounds` errors, every
    /// other round reports clean PARITY. A campaign matching (not `?`-ing)
    /// its probe result must still run all `rounds` rounds, count exactly
    /// one error, and quarantine the errored round's raw generated script.
    /// Disconfirmed by temporarily replacing this seam's `match` with
    /// `probe(&tokens)?` and rerunning this test: the campaign aborts at
    /// `error_round`, `probed_rounds.len()` comes up short, and this test
    /// fails loudly instead of the silent truncation a live campaign would
    /// suffer from the same change.
    #[test]
    fn fuzz_rounds_continues_past_a_probe_error_and_quarantines_it_raw() {
        let dir = std::env::temp_dir().join(format!(
            "view-harness-oracle-fuzz-pin-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("failed to create scratch dir");

        let seed = 7u64;
        let rounds = 5u32;
        let keys = 10usize;
        let error_round = 2u32;
        let quiesce = QuiesceWindow {
            silence: Duration::from_millis(corpus::DEFAULT_QUIESCE_SILENCE_MS),
            deadline: Duration::from_millis(corpus::DEFAULT_QUIESCE_DEADLINE_MS),
        };
        let mut probed_rounds: Vec<u32> = Vec::new();

        let summary = fuzz_rounds(seed, rounds, keys, "test-pin", &dir, quiesce, |_tokens| {
            let round = u32::try_from(probed_rounds.len()).unwrap_or(u32::MAX);
            probed_rounds.push(round);
            if round == error_round {
                Err(view_oracle::OracleError::Pty(
                    "scripted probe failure".to_string(),
                ))
            } else {
                Ok(EntryOutcome {
                    engine_settled: true,
                    reference_settled: true,
                    elapsed_ms: 0,
                    divergences: Vec::new(),
                })
            }
        })
        .expect("fuzz_rounds must not abort the campaign on a single round's probe error");

        assert_eq!(
            probed_rounds.len(),
            rounds as usize,
            "every round must be probed even though round {error_round} errored"
        );
        assert_eq!(
            summary,
            FuzzSummary {
                divergence_count: 0,
                timeout_count: 0,
                error_count: 1,
            }
        );

        let quarantined = quarantine_path(&dir, seed, error_round);
        assert!(
            quarantined.exists(),
            "the errored round must be quarantined raw at {}",
            quarantined.display()
        );
        let entry = corpus::load_file(&quarantined)
            .expect("the quarantined error round must still parse as a corpus entry");
        let expected_tokens = fuzz::generate_round(seed, error_round, keys);
        assert_eq!(
            entry.input,
            join_tokens(&expected_tokens),
            "the quarantined error round must hold its raw (unminimized) generated script"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pins the dependency `collect_entries`'s WHY comment states: the walk
    /// is non-recursive, so a `.toml` file living in a subdirectory of the
    /// corpus dir (exactly `corpus/quarantine`'s own shape) is never
    /// collected into a plain corpus-wide run.
    #[test]
    fn collect_entries_does_not_recurse_into_subdirectories() {
        let dir = std::env::temp_dir().join(format!(
            "view-harness-oracle-collect-entries-{}",
            std::process::id()
        ));
        let subdir = dir.join("quarantine");
        std::fs::create_dir_all(&subdir).expect("failed to create scratch dirs");

        corpus::write_entry(
            &dir.join("top-level.toml"),
            "top-level",
            "ihello<Esc>",
            "test-pin",
            "default",
            corpus::DEFAULT_QUIESCE_SILENCE_MS,
            corpus::DEFAULT_QUIESCE_DEADLINE_MS,
        )
        .expect("failed to write top-level entry");
        corpus::write_entry(
            &subdir.join("nested.toml"),
            "nested",
            "ihello<Esc>",
            "test-pin",
            "default",
            corpus::DEFAULT_QUIESCE_SILENCE_MS,
            corpus::DEFAULT_QUIESCE_DEADLINE_MS,
        )
        .expect("failed to write nested entry");

        let entries = collect_entries(&dir).expect("collect_entries failed");
        let names: Vec<&str> = entries.iter().map(|(_, e)| e.name.as_str()).collect();

        assert_eq!(
            names,
            vec!["top-level"],
            "a .toml file in a subdirectory of the corpus dir must not be collected"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
