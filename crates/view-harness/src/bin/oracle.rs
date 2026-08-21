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
//! Further subcommands extend the runner past the bare pass/fail report:
//! `minimize PATH` shrinks a corpus entry that already reproduces a
//! divergence or timeout to a locally 1-minimal input (rewriting the entry
//! in place), and `fuzz --seed N` drives seeded, reproducible random
//! scripts through the same stack, quarantining (already minimized) any
//! round that fails. Both share [`run_tokens`], the same
//! spawn/drain/quiesce/compare engine the plain corpus run above uses, so
//! all three modes see identical parity semantics.
//!
//! The `compat` subcommand's own scenario runner lives beside this file in
//! [`compat`]: it drives the real `view` binary over a pty against pinned
//! plugin fixtures, which is a different subject from the two embedded
//! engines everything above compares, and it is large enough to own its
//! file.

// a bin target's own file is a crate root, so its child modules resolve
// beside it rather than under a directory of its name; the same `#[path]`
// the bench bin's row modules use puts this one in `oracle/`
#[path = "oracle/compat.rs"]
mod compat;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use view_harness::corpus::{self, CorpusEntry};
use view_harness::fixture::{current_engine_pin, verify_nvim_matches_pin, workspace_root};
use view_harness::fuzz;
use view_harness::page;
use view_harness::results::load_results;
use view_oracle::review::{DiffReviewCase, ReviewDriver, ReviewStep, NORMALIZE_KEYS};
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
    /// Which transport legs a corpus run drives (default `both`, which is
    /// the gate's own value). `local` is the one to reach for when iterating
    /// on a single divergent entry, which pays a stub spawn per round
    /// otherwise. Refused rather than ignored when a subcommand is given:
    /// every subcommand drives a transport of its own choosing and none of
    /// them reads this.
    #[arg(long, value_enum)]
    route: Option<RouteArg>,
}

/// The `--route` selector's surface. A separate type from [`EngineRoute`]
/// because the two answer different questions: this one is a caller's
/// request, which `both` is a legal answer to, and that one names a single
/// leg a run is currently on.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum RouteArg {
    /// A local `nvim --embed` only.
    Local,
    /// The stand-in ssh client only.
    Remote,
    /// Both, local first.
    Both,
}

impl RouteArg {
    /// The legs this selector asks for, in the order a run drives them.
    fn routes(self) -> &'static [EngineRoute] {
        match self {
            Self::Local => &[EngineRoute::Local],
            Self::Remote => &[EngineRoute::StubRemote],
            Self::Both => &[EngineRoute::Local, EngineRoute::StubRemote],
        }
    }

    /// How this value is spelled on the command line, for a refusal to quote
    /// back what it refused.
    const fn spelling(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Both => "both",
        }
    }
}

/// The legs to drive, refusing a `--route` that named something no
/// subcommand can honour.
///
/// A flag accepted and then discarded is the failure this whole path exists
/// to refuse: `oracle --route local remote` would parse, print remote-battery
/// lines, and leave whoever ran it believing they had narrowed something. No
/// subcommand can honour the flag -- each drives a transport its own case
/// list decides -- so the combination is a mistake with no correct reading,
/// and the refusal names both halves so the reader can see which one to drop.
fn routes_for(
    command: Option<&Command>,
    route: Option<RouteArg>,
) -> Result<&'static [EngineRoute]> {
    match (command, route) {
        (Some(command), Some(route)) => bail!(
            "--route {} was given with the `{}` subcommand, which drives its \
             own transport and never reads it. Drop one: `oracle --route {}` \
             runs the corpus on that leg, `oracle {}` runs the subcommand.",
            route.spelling(),
            command.spelling(),
            route.spelling(),
            command.spelling(),
        ),
        (Some(_), None) => Ok(&[]),
        (None, route) => Ok(route.unwrap_or(RouteArg::Both).routes()),
    }
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
    /// Drives `view_oracle::hang`'s reproduced hang schedules against a real
    /// pinned engine: a read-side wedge, a killed connection, and the
    /// key-wait control the first two are only meaningful against.
    ///
    /// Each schedule is bounded by what the supervision stack promises, plus
    /// half a second for the cost of reading a verdict. On a host that
    /// deschedules an observer for longer than that, set
    /// `VIEW_ORACLE_SLACK_SCALE` to the whole number to multiply that half
    /// second by; it moves nothing else, and anything unparseable or zero
    /// leaves the shipped bound alone.
    Hang {
        /// One schedule by name (`read-side-wedge`, `dead-connection`,
        /// `blocked-on-key`) instead of all three.
        #[arg(long)]
        schedule: Option<String>,
    },
    /// Drives `view_oracle::speculate`'s battery against a real pinned
    /// engine: every shape a display-only prediction can be answered,
    /// contradicted or invalidated in, each ending with view's own screen
    /// diffed against nvim's own over the rows the settled frame does not
    /// mask.
    ///
    /// A predicted glyph is a cell view paints that no redraw carried, so a
    /// divergence found here is a parity failure rather than a latency
    /// reading: the exit code is the corpus runner's own contract.
    ///
    /// The one case no redraw retires is bounded by the age bound plus half
    /// a second for the cost of reading the retirement. On a host that
    /// deschedules an observer for longer than that, set
    /// `VIEW_ORACLE_SLACK_SCALE` to the whole number to multiply that half
    /// second by -- the same knob, and the same claim about the host, as the
    /// `hang` runner's.
    Speculate {
        /// One case by name (`burst-tail`, `mispredict`, `age-bound`, ...)
        /// instead of all of them.
        #[arg(long)]
        case: Option<String>,
    },
    /// Drives `view_oracle::remote`'s battery against the committed
    /// stand-in ssh client: the stand-in's own fidelity, and the remote
    /// spawn path held against the local one it must be
    /// indistinguishable from.
    ///
    /// The broadest form of that same claim is not here but in the bare
    /// `oracle [PATH]` run, which drives the whole corpus through the
    /// remote path as a second leg. This subcommand is the narrow one, for
    /// reproducing a single case.
    ///
    /// A host with no POSIX shell to re-parse a joined command line reports
    /// the leg as SKIPPED and exits 0: the remote path's contract is a
    /// POSIX one, and a host that cannot make the claim must say so rather
    /// than fail for a leg it never drove.
    Remote {
        /// One case by name (`stub-flattening`, `parentless-open`) instead
        /// of all of them.
        #[arg(long)]
        case: Option<String>,
    },
}

impl Command {
    /// This subcommand's name on the command line, for a refusal to quote
    /// back what it refused.
    const fn spelling(&self) -> &'static str {
        match self {
            Self::Minimize { .. } => "minimize",
            Self::Fuzz { .. } => "fuzz",
            Self::Compat { .. } => "compat",
            Self::Page => "page",
            Self::Hang { .. } => "hang",
            Self::Speculate { .. } => "speculate",
            Self::Remote { .. } => "remote",
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let routes = routes_for(cli.command.as_ref(), cli.route)?;
    match cli.command {
        Some(Command::Minimize {
            path,
            inject_divergence_at,
        }) => minimize_command(&path, inject_divergence_at),
        Some(Command::Fuzz { seed, rounds, keys }) => fuzz_command(seed, rounds, keys),
        Some(Command::Compat { path }) => compat::command(&path),
        Some(Command::Page) => page_command(),
        Some(Command::Hang { schedule }) => {
            if view_harness::hang::run(schedule.as_deref())? {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        Some(Command::Remote { case }) => {
            if view_harness::remote::run(case.as_deref())? {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        Some(Command::Speculate { case }) => {
            if view_harness::speculate::run(case.as_deref())? {
                Ok(())
            } else {
                std::process::exit(1)
            }
        }
        None => run_command(&cli.path, routes),
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

/// Where a run's engine side is spawned. The reference applier is always
/// local: it is the side that exists to be trusted, and routing it through
/// the same transport as the side under test would leave a transport fault
/// common to both and invisible to the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EngineRoute {
    /// A local `nvim --embed`, the way every entry has always run.
    Local,
    /// The same engine reached through the committed stand-in ssh client
    /// (`view_oracle::remote`), which joins its trailing arguments and hands
    /// them to a shell exactly as a real client does.
    ///
    /// The transport is the whole of the difference, and
    /// `view_oracle::remote::stub_config` is what keeps that true. A config
    /// aimed at a real destination plans from named constants and exempts
    /// `HOME`, because it cannot see the far side; this route's far side is
    /// this host, so the same prepared hermetic home and the same host
    /// sweep the local leg gets are applied to it as well. An entry that
    /// probes anything home-shaped, or that a host variable would reach,
    /// therefore reads the same on both legs -- and a divergence marked
    /// `(remote)` is about the transport, which is the only reason that
    /// label is worth printing.
    StubRemote,
}

impl EngineRoute {
    /// What a report line calls this route. The local route says nothing,
    /// so every line a run has ever printed keeps its shape and only the
    /// second leg's lines are marked.
    const fn label(self) -> &'static str {
        match self {
            Self::Local => "",
            Self::StubRemote => " (remote)",
        }
    }
}

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
///
/// `route` decides where the engine side runs, and nothing else about the
/// comparison: the reference applier stays local either way, so a remote
/// route asks whether reaching the same engine over an `ssh` client changed
/// any answer it gives. The minimizer and the fuzz runner always pass the
/// local route -- both reduce toward, or generate against, a failure
/// signature, and a transport that can fail on its own would put a second
/// variable inside that predicate.
fn run_tokens(
    tokens: &[String],
    cols: u16,
    rows: u16,
    silence: Duration,
    deadline: Duration,
    route: EngineRoute,
) -> Result<EntryOutcome, view_oracle::OracleError> {
    let start = Instant::now();
    let (mut engine, mut reference) = spawn_pair(cols, rows, silence, deadline, route)?;

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

    compare_pair(
        &mut engine,
        &mut reference,
        EngineSettled(engine_settled),
        ReferenceSettled(reference_settled),
        start,
    )
}

/// Spawns one side-by-side pair at `cols`x`rows`, drains their startup
/// traffic and warms the engine side's renderer cache -- everything every
/// run does before its script's first key, whichever script shape it is.
///
/// Startup quiescence is drained, not gated on: a slow-starting nvim's own
/// splash/plugin traffic settling late here is not itself a divergence,
/// only a post-input settle decides pass/fail. A probe error, unlike a slow
/// settle, still propagates: it means the session is broken, not merely
/// late.
///
/// Warming the cached renderer is what makes every run's later captures
/// decide reuse-versus-rebuild against a frame that predates the script's
/// edits, so each one exercises the production frame-to-frame render path
/// (and, in debug builds, its equivalence guard) across a real model change
/// instead of only capturing from a cold cache.
fn spawn_pair(
    cols: u16,
    rows: u16,
    silence: Duration,
    deadline: Duration,
    route: EngineRoute,
) -> Result<(EngineSession, ReferenceSession), view_oracle::OracleError> {
    let mut engine = match route {
        EngineRoute::Local => EngineSession::spawn(cols, rows)?,
        EngineRoute::StubRemote => view_oracle::remote::spawn_stub_session(cols, rows)?,
    };
    let mut reference = ReferenceSession::spawn(cols, rows)?;
    let _ = engine.quiesce(silence, deadline)?;
    let _ = reference.quiesce(silence, deadline)?;
    let _ = engine.surface();
    Ok((engine, reference))
}

/// Probes both sides and diffs them: the comparison tail every run shape
/// shares, so a plain corpus entry and a diff-review one are scored by the
/// same state probes, the same masked grid diff, and the same rule about an
/// unsettled side.
fn compare_pair(
    engine: &mut EngineSession,
    reference: &mut ReferenceSession,
    engine_settled: EngineSettled,
    reference_settled: ReferenceSettled,
    start: Instant,
) -> Result<EntryOutcome, view_oracle::OracleError> {
    let surface = engine.surface();
    let view_screen = engine.screen();
    let mask = masked_rows(&surface);
    let ref_screen = reference.screen();

    let view_state = snapshot(engine)?;
    let ref_state = snapshot(reference)?;

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

    let EngineSettled(engine_settled) = engine_settled;
    let ReferenceSettled(reference_settled) = reference_settled;
    Ok(EntryOutcome {
        engine_settled,
        reference_settled,
        elapsed_ms: start.elapsed().as_millis(),
        divergences,
    })
}

/// Drives one corpus entry at its own quiesce overrides -- the plain
/// corpus-run path `Command`'s `None` (bare `oracle [PATH]`) arm uses.
///
/// An entry naming a diff-review case runs the two sides on deliberately
/// different scripts (see [`run_review_entry`]); every other entry drives
/// both sides with its own tokenized input.
fn run_entry(
    entry: &CorpusEntry,
    route: EngineRoute,
) -> Result<EntryOutcome, view_oracle::OracleError> {
    let silence = Duration::from_millis(entry.quiesce_silence_ms);
    let deadline = Duration::from_millis(entry.quiesce_deadline_ms);
    match entry.diff_review {
        Some(case) => run_review_entry(entry, case, silence, deadline, route),
        None => run_tokens(
            &tokenize(&entry.input),
            COLS,
            ROWS,
            silence,
            deadline,
            route,
        ),
    }
}

/// Drives one diff-review entry: `entry.input` seeds both sides with the
/// same text, then `case`'s own steps take the two sides apart on purpose
/// -- view's side applies an agent's proposal through the review's
/// `nvim_buf_set_text` write, the reference side types the same change --
/// and [`NORMALIZE_KEYS`] brings the incidental editing residue back
/// together so the comparison is about the text the write produced.
///
/// Every keys step settles the side it typed into before the next one, and
/// a write step settles the engine side before anything types into it
/// again: `arm_and_input`'s marker protocol owes an already-settled
/// session, and a review's write leaves redraw traffic in flight that a
/// marker armed on top of would fire inside.
fn run_review_entry(
    entry: &CorpusEntry,
    case: DiffReviewCase,
    silence: Duration,
    deadline: Duration,
    route: EngineRoute,
) -> Result<EntryOutcome, view_oracle::OracleError> {
    let start = Instant::now();
    let (mut engine, mut reference) = spawn_pair(COLS, ROWS, silence, deadline, route)?;

    engine.arm_and_input(&entry.input)?;
    reference.arm_and_input(&entry.input)?;
    let mut engine_settled = engine.quiesce(silence, deadline)?;
    let mut reference_settled = reference.quiesce(silence, deadline)?;

    let mut driver = ReviewDriver::default();
    for step in case.steps() {
        match *step {
            ReviewStep::Shared(keys) => {
                engine.arm_and_input(keys)?;
                reference.arm_and_input(keys)?;
                engine_settled &= engine.quiesce(silence, deadline)?;
                reference_settled &= reference.quiesce(silence, deadline)?;
            }
            ReviewStep::Reference(keys) => {
                reference.arm_and_input(keys)?;
                reference_settled &= reference.quiesce(silence, deadline)?;
            }
            // Spelled out rather than caught by a wildcard: a step kind
            // added later that delivers keys must fail to compile here
            // instead of routing to a driver that has no session to type
            // into
            step @ (ReviewStep::Propose(_)
            | ReviewStep::Accept(_)
            | ReviewStep::AcceptAll
            | ReviewStep::Reject(_)
            | ReviewStep::ReDiff(_)
            | ReviewStep::FoldRow(_)) => {
                if driver.apply(&mut engine, step)? {
                    engine_settled &= engine.quiesce(silence, deadline)?;
                }
            }
        }
    }

    engine.arm_and_input(NORMALIZE_KEYS)?;
    reference.arm_and_input(NORMALIZE_KEYS)?;
    engine_settled &= engine.quiesce(silence, deadline)?;
    reference_settled &= reference.quiesce(silence, deadline)?;

    compare_pair(
        &mut engine,
        &mut reference,
        EngineSettled(engine_settled),
        ReferenceSettled(reference_settled),
        start,
    )
}

/// Prints one entry's report line plus, on anything but clean PARITY,
/// every [`Divergence`] found -- the exit-contract report shape the corpus
/// runner's own interface (see this crate's module docs) commits to.
fn print_outcome(name: &str, outcome: &EntryOutcome, route: EngineRoute) {
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
        "oracle: {name}{} ... {status} ({COLS}x{ROWS}, {settle_word}, {}ms)",
        route.label(),
        outcome.elapsed_ms
    );
    for divergence in &outcome.divergences {
        println!("  {divergence:?}");
    }
}

/// The bare `oracle [PATH]` run: every entry under `path`, on each leg of
/// `routes`, reported and exit-coded per this crate's own module docs.
///
/// Two legs by default, not one. The corpus runs first against a local
/// engine, then again with the engine side reached over the committed
/// stand-in ssh client. A remote session is supposed to differ from a local
/// one in nothing but its transport, and the corpus is the broadest
/// statement of that this tree has -- narrower cases can only assert what
/// somebody thought to assert, while a second full pass fails on anything
/// the whole corpus already covers. `--route` narrows it for an iteration
/// loop rather than for the gate, whose value is the default. The remote leg
/// is skipped, with a line saying so, on a host that cannot run a POSIX
/// stand-in.
fn run_command(path: &Path, routes: &[EngineRoute]) -> Result<()> {
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
    for (entry_path, entry) in &entries {
        if entry.engine_pin != pin {
            eprintln!(
                "oracle: WARNING: {} ({}) was authored against engine pin {} but the current \
                 pin is {pin}; running anyway",
                entry.name,
                entry_path.display(),
                entry.engine_pin,
            );
        }
    }
    for &route in routes {
        if route == EngineRoute::StubRemote && !view_oracle::remote::stub_available() {
            println!(
                "oracle: remote leg ... SKIPPED (no POSIX stand-in client at {})",
                view_oracle::remote::stub_client().display()
            );
            continue;
        }
        for (_, entry) in &entries {
            match run_entry(entry, route) {
                Ok(outcome) => {
                    print_outcome(&entry.name, &outcome, route);
                    if !outcome.is_success() {
                        any_failed = true;
                    }
                }
                Err(err) => {
                    println!("oracle: {}{} ... ERROR: {err}", entry.name, route.label());
                    any_failed = true;
                }
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
        run_tokens(candidate, cols, rows, silence, deadline, EngineRoute::Local)
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
    if let Some(case) = entry.diff_review {
        // ddmin reduces a key script toward a failure signature, and a
        // diff-review entry's failure is made of its case's own decisions
        // rather than of the keys that seed the buffer; a rewrite here
        // would also drop the case name the entry carries, since the
        // writer has no field for it
        bail!(
            "{} drives the diff-review case {}, which the minimizer cannot reduce",
            entry.name,
            case.name()
        );
    }
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

    let baseline = run_tokens(&tokens, COLS, ROWS, silence, deadline, EngineRoute::Local)
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
        |tokens| {
            run_tokens(
                tokens,
                COLS,
                ROWS,
                quiesce.silence,
                quiesce.deadline,
                EngineRoute::Local,
            )
        },
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
    // the env-mutation sites below are the ones ENV_MUTATION_LOCK exists to
    // bound; each holds the guard across its own restore
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::disallowed_methods,
        clippy::panic
    )]
    use super::*;

    /// The selector's default is the gate's value, and a narrowed one drives
    /// exactly the leg it names. A default that quietly became `local` would
    /// leave the remote path uncovered by the only two commands that ever
    /// exercise it automatically, and nothing would report the loss.
    #[test]
    fn the_route_selector_defaults_to_every_leg_and_narrows_to_one() {
        assert_eq!(
            RouteArg::Both.routes(),
            [EngineRoute::Local, EngineRoute::StubRemote],
            "the default run must drive both legs, local first"
        );
        assert_eq!(RouteArg::Local.routes(), [EngineRoute::Local]);
        assert_eq!(RouteArg::Remote.routes(), [EngineRoute::StubRemote]);
        let bare = Cli::parse_from(["oracle"]);
        assert_eq!(
            routes_for(bare.command.as_ref(), bare.route).unwrap(),
            RouteArg::Both.routes(),
            "a bare invocation must be the gate's own run"
        );
        let narrowed = Cli::parse_from(["oracle", "--route", "local"]);
        assert_eq!(
            routes_for(narrowed.command.as_ref(), narrowed.route).unwrap(),
            RouteArg::Local.routes()
        );
    }

    /// A flag accepted and then discarded is the defect class this runner
    /// refuses everywhere else, and `--route` sits on the top-level parser
    /// where every subcommand can be handed one. The refusal has to name
    /// both halves: a message saying only "unsupported" leaves the reader
    /// guessing which of the two words they typed is the wrong one.
    #[test]
    fn a_route_no_subcommand_can_honour_is_refused_rather_than_ignored() {
        let cli = Cli::parse_from(["oracle", "--route", "local", "remote"]);
        let refused = routes_for(cli.command.as_ref(), cli.route)
            .expect_err("a route given to a subcommand must not be silently dropped")
            .to_string();
        assert!(
            refused.contains("--route local") && refused.contains("`remote`"),
            "the refusal names neither the flag it refused nor the subcommand \
             it was given with: {refused}"
        );
        let alone = Cli::parse_from(["oracle", "remote"]);
        assert!(
            routes_for(alone.command.as_ref(), alone.route)
                .unwrap()
                .is_empty(),
            "a subcommand with no --route drives its own transport and must \
             not inherit the corpus run's default"
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

    /// The corpus must claim every diff-review case, exactly once. Deleting
    /// one of those entries is the way this coverage silently disappears:
    /// the runner would report the same PARITY-for-every-entry line it
    /// always does, one case lighter, and the buffer-write path would stop
    /// being exercised without a single failure to say so. This is the
    /// assertion that turns that deletion into a red test.
    #[test]
    fn every_diff_review_case_is_claimed_by_exactly_one_corpus_entry() {
        let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        dir.pop(); // crates/
        dir.pop(); // workspace root
        dir.push("corpus");
        let mut claimed: Vec<(DiffReviewCase, String)> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("failed to read corpus/") {
            let path = entry.expect("failed to read corpus/ entry").path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let corpus_entry = corpus::load_file(&path)
                .map_err(|err| format!("failed to load {}: {err}", path.display()))
                .expect("every corpus/*.toml entry must load");
            if let Some(case) = corpus_entry.diff_review {
                claimed.push((case, corpus_entry.name));
            }
        }
        for case in DiffReviewCase::ALL {
            let entries: Vec<&str> = claimed
                .iter()
                .filter(|(claimed_case, _)| *claimed_case == case)
                .map(|(_, name)| name.as_str())
                .collect();
            assert_eq!(
                entries.len(),
                1,
                "the diff-review case {} must be driven by exactly one corpus entry, but {} \
                 claim it: {entries:?}",
                case.name(),
                entries.len()
            );
        }
        assert_eq!(
            claimed.len(),
            DiffReviewCase::ALL.len(),
            "every diff-review entry must name a case that exists, and every case must be \
             named once: {claimed:?}"
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

    /// A diff-review entry must be refused by the minimizer before it
    /// spawns anything. The writer has no `diff_review` field, so a
    /// reduction that ran would rewrite the entry without its case name and
    /// leave a file that still loads, still reports PARITY, and no longer
    /// drives the write path at all.
    #[test]
    fn minimizing_a_diff_review_entry_is_refused() {
        let dir = std::env::temp_dir().join(format!(
            "view-harness-oracle-review-minimize-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("failed to create scratch dir");
        let path = dir.join("review.toml");
        std::fs::write(
            &path,
            format!(
                "schema = 1\nname = \"scratch-review\"\ninput = \"x\"\nengine_pin = \
                 \"test-pin\"\next_set = \"default\"\ndiff_review = \"{}\"\n",
                DiffReviewCase::SingleHunkAccept.name()
            ),
        )
        .expect("failed to write scratch entry");

        let err = minimize_command(&path, None).expect_err("a diff-review entry must be refused");

        assert!(
            err.to_string().contains("the minimizer cannot reduce"),
            "expected the refusal to name the minimizer's own limit, got {err}"
        );
        assert!(
            std::fs::read_to_string(&path)
                .expect("the scratch entry must still be readable")
                .contains("diff_review"),
            "the refused entry must be left exactly as it was"
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
