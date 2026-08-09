//! The picker scenario: the two spec 3.1 picker rows, measured end to end
//! through the terminal against generated file corpora.
//!
//! - **match** -- keystroke to first results painted with the 100k-entry
//!   corpus already resident in the matcher: each sample types or erases
//!   one query character and waits for the vt100-parsed frame whose result
//!   rows can only belong to the new query.
//! - **scan** -- picker open over a 1M-file tree: each sample opens the
//!   picker fresh (a close tears the matcher session down, so every open
//!   re-walks the tree) and waits for the first page of result rows. The
//!   row's streaming promise is asserted by observation, not timing: a
//!   held query's visible result set must grow between two paints with no
//!   input in between, which can only happen if results were painted
//!   while the scan was still supplying candidates.
//!
//! Both corpora are generated, not checked in: a million committed inodes
//! would swamp the repository for bytes that carry no information. The
//! corpus alphabet is the load-bearing part -- see [`ensure_corpora`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::boundaries::{screen_holds, screen_lines};
use crate::sampling::{median_of_trials, Distribution};
use crate::scenarios::Protocol;
use crate::session::{BenchSession, SettleBound, SpawnSpec, ViewSpec};
use crate::BenchError;

/// Bulk entries in the match corpus: the spec row's "100k resident
/// entries".
pub const MATCH_CORPUS_FILES: usize = 100_000;

/// Bulk entries in the scan corpus: the spec row's "1M-file tree".
pub const SCAN_CORPUS_FILES: usize = 1_000_000;

/// Bulk files per directory; keeps every directory a size `readdir`
/// handles without pathological htree depth.
const FILES_PER_DIR: usize = 1000;

/// The token every bulk label carries and nothing else on the bench
/// screen does: bulk files are named `zf{index}.txt` under `d{index}`
/// directories, an alphabet chosen to exclude the query letters below so
/// a query's result set is decidable from the screen alone.
const BULK_TOKEN: &str = "zf";

/// The one label in the match corpus containing the letter `a`, so the
/// sampled query `a` has exactly one possible result row.
const MATCH_TARGET: &str = "qa.txt";

/// The sampled query character for the match phase. No bulk label, probe
/// label, directory name or fixed chrome string contains an `a`, so the
/// keystroke's answer -- [`MATCH_TARGET`] alone on screen, every
/// [`BULK_TOKEN`] row gone -- can only be painted from a match pass over
/// the new query.
const MATCH_QUERY: &[u8] = b"a";

/// Label token of the spread probe files (`rsentfile{k}.txt`), and the
/// query that matches exactly them: `rsent` is a subsequence of no bulk
/// label (bulk names carry no `r`), so its result count on screen is the
/// number of probe files the matcher has received so far.
const PROBE_TOKEN: &str = "rsentfile";
const PROBE_QUERY: &[u8] = b"rsent";

/// How many probe files each corpus spreads across its directories. Ten,
/// one per tenth of the directory range: whatever order the filesystem
/// walk visits directories, the chance that every probe lands in the
/// fraction of the walk finished before the first page paints is
/// negligible, which is what makes the growth observation below reliable
/// rather than lucky.
const PROBE_FILES: usize = 10;

/// The ex-command that opens the file picker; `:View` registers
/// unconditionally (see `view-native`'s mappings doc), so this needs no
/// leader key from the fixture config. Drift is loud, not silent: if the
/// command's shape changes, the picker never opens and every sample
/// desyncs with the screen attached.
const OPEN_COMMAND: &[u8] = b":View picker files";

/// Result rows that must be visible before the scan phase counts the
/// first page as painted: comfortably above one straggler row, below the
/// overlay's own row budget at the bench grid size.
const FIRST_PAGE_ROWS: usize = 5;

/// Measured/warmup picker opens per scan trial. Scenario-owned rather
/// than read off the protocol (the same split flood makes): one sample
/// here costs a picker open against a million-file walk, so a
/// 1000-sample protocol run would spend most of an hour measuring a
/// boundary that stabilizes within a dozen opens.
const SCAN_SAMPLES: usize = 12;

/// Warmup opens per scan trial, excluded from every statistic; public so
/// the report can state the discipline the numbers were taken under.
pub const SCAN_WARMUP: usize = 2;

/// Bound on one picker open reaching its first page before the run is
/// declared desynced; generous against the 100 ms budget because a
/// desync bound is a liveness check, not a bar.
const FIRST_PAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on corpus residency and streaming observations: a full 1M-file
/// walk on a cold cache is disk-bound and legitimately slow.
const FULL_WALK_TIMEOUT: Duration = Duration::from_secs(300);

/// Stamp file marking a corpus directory complete, with the layout
/// version inside. A dotfile on purpose: the picker's walk skips hidden
/// files, so the stamp never appears as a corpus entry.
const STAMP: &str = ".complete";

/// Bumped whenever the generated layout changes, so a stale cached
/// corpus regenerates instead of silently measuring the old shape.
const LAYOUT_VERSION: &str = "picker-corpus-v1";

/// The two generated corpus roots, under the cache directory the caller
/// names.
#[derive(Debug, Clone)]
pub struct CorpusRoots {
    pub match_root: PathBuf,
    pub scan_root: PathBuf,
}

/// Free space a generation run requires before it starts, checked on
/// unix via `statvfs`: the corpora cost ~1.1M inodes and directory
/// blocks, and running the disk to zero mid-generation would take the
/// host down with it rather than failing one bench row.
const REQUIRED_BYTES: u64 = 1 << 30;
const REQUIRED_INODES: u64 = 1_300_000;

/// Generates (or reuses) both corpora under `root`, returning their
/// paths. Generation is idempotent and stamped: a directory whose stamp
/// matches [`LAYOUT_VERSION`] is trusted as-is, anything else is wiped
/// and rebuilt, so an interrupted generation can never be half-measured.
///
/// # Errors
///
/// Returns [`BenchError::CorpusSetup`] when the filesystem refuses
/// (space, inodes, permissions), with the free-space numbers it saw.
pub fn ensure_corpora(root: &Path) -> Result<CorpusRoots, BenchError> {
    let match_root = root.join("match");
    let scan_root = root.join("scan");
    ensure_corpus(&match_root, MATCH_CORPUS_FILES, true)?;
    ensure_corpus(&scan_root, SCAN_CORPUS_FILES, false)?;
    Ok(CorpusRoots {
        match_root,
        scan_root,
    })
}

fn setup_err(path: &Path, context: String) -> BenchError {
    BenchError::CorpusSetup {
        path: path.display().to_string(),
        context,
    }
}

/// Whether `dir` carries a stamp for the current layout.
fn stamped(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(STAMP)).is_ok_and(|body| body.trim() == LAYOUT_VERSION)
}

/// Refuses to generate on a filesystem without room for the corpus, so
/// the failure names the constraint instead of surfacing as an ENOSPC
/// halfway through a million creates (or worse, as some other process on
/// the host failing first).
fn require_free_space(root: &Path) -> Result<(), BenchError> {
    #[cfg(unix)]
    {
        let stat = rustix::fs::statvfs(root)
            .map_err(|err| setup_err(root, format!("statvfs failed: {err}")))?;
        let free_bytes = stat.f_bavail.saturating_mul(stat.f_frsize);
        if free_bytes < REQUIRED_BYTES {
            return Err(setup_err(
                root,
                format!(
                    "only {free_bytes} bytes free, {REQUIRED_BYTES} required; free disk space \
                     before the picker corpora can be generated"
                ),
            ));
        }
        // f_files == 0 means the filesystem does not account inodes
        // (btrfs); the byte check above is then the only meaningful bound
        if stat.f_files > 0 && stat.f_favail < REQUIRED_INODES {
            return Err(setup_err(
                root,
                format!(
                    "only {} inodes free, {REQUIRED_INODES} required; free inodes before the \
                     picker corpora can be generated",
                    stat.f_favail
                ),
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = root;
    Ok(())
}

/// Generates one corpus of `bulk_files` empty files plus the probe set
/// (and, for the match corpus, the single query target), stamping the
/// directory on completion.
fn ensure_corpus(dir: &Path, bulk_files: usize, with_match_target: bool) -> Result<(), BenchError> {
    if stamped(dir) {
        return Ok(());
    }
    if dir.exists() {
        std::fs::remove_dir_all(dir)
            .map_err(|err| setup_err(dir, format!("clearing stale corpus: {err}")))?;
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| setup_err(dir, format!("creating corpus parent: {err}")))?;
    }
    require_free_space(dir.parent().unwrap_or(dir))?;
    let dirs = bulk_files.div_ceil(FILES_PER_DIR);
    // directory-name width follows the directory count so the two corpus
    // sizes produce d00..d99 and d000..d999 without a second layout knob
    let width = dirs.saturating_sub(1).to_string().len();
    for d in 0..dirs {
        let sub = dir.join(format!("d{d:0width$}"));
        std::fs::create_dir_all(&sub)
            .map_err(|err| setup_err(&sub, format!("creating corpus dir: {err}")))?;
        let in_dir = FILES_PER_DIR.min(bulk_files - d * FILES_PER_DIR);
        for f in 0..in_dir {
            let file = sub.join(format!("zf{f:03}.txt"));
            std::fs::File::create(&file)
                .map_err(|err| setup_err(&file, format!("creating corpus file: {err}")))?;
        }
        // one probe per tenth of the directory range, in the middle of
        // its band, so no walk order can visit them all early
        if probe_index(d, dirs).is_some() {
            let probe = sub.join(format!(
                "rsentfile{}.txt",
                probe_index(d, dirs).unwrap_or(0)
            ));
            std::fs::File::create(&probe)
                .map_err(|err| setup_err(&probe, format!("creating probe file: {err}")))?;
        }
    }
    if with_match_target {
        let target = dir.join(MATCH_TARGET);
        std::fs::File::create(&target)
            .map_err(|err| setup_err(&target, format!("creating match target: {err}")))?;
    }
    std::fs::write(dir.join(STAMP), LAYOUT_VERSION)
        .map_err(|err| setup_err(dir, format!("writing corpus stamp: {err}")))?;
    Ok(())
}

/// Which probe file directory `d` of `dirs` carries, if any: the middle
/// directory of each tenth of the range.
fn probe_index(d: usize, dirs: usize) -> Option<usize> {
    let band = dirs / PROBE_FILES;
    if band == 0 {
        return None;
    }
    (d % band == band / 2 && d / band < PROBE_FILES).then_some(d / band)
}

/// Direct evidence of the scan row's streaming promise: a held query's
/// visible result count grew between two paints with no input in
/// between, so results were on screen while the walk was still
/// supplying candidates.
#[derive(Debug, Clone, Copy)]
pub struct StreamingEvidence {
    /// Zero-based scan trial the observation landed in.
    pub trial: usize,
    /// Probe rows visible at the first paint of the held query's results.
    pub first_seen: usize,
    /// Probe rows visible when the observation concluded.
    pub grew_to: usize,
}

/// The picker run's outcome: per-trial distributions for both phases and
/// the median-across-trials statistics the gate reads, plus the streaming
/// observation.
#[derive(Debug)]
pub struct PickerOutcome {
    pub match_trials: Vec<Distribution>,
    pub scan_trials: Vec<Distribution>,
    pub gated_match_paint_p50_ms: f64,
    pub gated_match_paint_p99_ms: f64,
    pub gated_first_page_p50_ms: f64,
    pub gated_first_page_p99_ms: f64,
    pub streaming: StreamingEvidence,
}

/// Runs both picker phases against `view_spec`, with the working
/// directory swapped per phase to the corpus each one measures.
///
/// # Errors
///
/// Returns a [`BenchError`] when a session desyncs, a sample times out,
/// or the streaming observation never lands.
pub fn run(
    view_spec: ViewSpec<'_>,
    roots: &CorpusRoots,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<PickerOutcome, BenchError> {
    let match_trials = run_match_phase(
        &spec_with_cwd(view_spec.0, &roots.match_root),
        protocol,
        settle_deadline,
    )?;
    let (scan_trials, streaming) = run_scan_phase(
        &spec_with_cwd(view_spec.0, &roots.scan_root),
        protocol,
        settle_deadline,
    )?;
    let match_p50: Vec<f64> = match_trials.iter().map(Distribution::p50).collect();
    let match_p99: Vec<f64> = match_trials.iter().map(Distribution::p99).collect();
    let scan_p50: Vec<f64> = scan_trials.iter().map(Distribution::p50).collect();
    let scan_p99: Vec<f64> = scan_trials.iter().map(Distribution::p99).collect();
    Ok(PickerOutcome {
        gated_match_paint_p50_ms: median_of_trials(&match_p50)?,
        gated_match_paint_p99_ms: median_of_trials(&match_p99)?,
        gated_first_page_p50_ms: median_of_trials(&scan_p50)?,
        gated_first_page_p99_ms: median_of_trials(&scan_p99)?,
        match_trials,
        scan_trials,
        streaming,
    })
}

/// `spec` with its working directory replaced: the picker's `Files` root
/// is the process cwd, so the corpus is selected by where view starts.
fn spec_with_cwd(spec: &SpawnSpec, cwd: &Path) -> SpawnSpec {
    SpawnSpec {
        program: spec.program.clone(),
        args: spec.args.clone(),
        env: spec.env.clone(),
        cwd: Some(cwd.to_path_buf()),
    }
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Occurrences of `token` in the parsed screen text.
fn count_token(session: &mut BenchSession, token: &str) -> usize {
    session.with_screen(|screen| screen_lines(screen).matches(token).count())
}

/// Tight-polls (yielding) until `check` holds, or fails with `what` and
/// the screen attached.
fn wait_for(
    session: &mut BenchSession,
    timeout: Duration,
    what: &str,
    mut check: impl FnMut(&mut BenchSession) -> bool,
) -> Result<(), BenchError> {
    let deadline = Instant::now() + timeout;
    loop {
        if check(session) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BenchError::Desync {
                context: format!(
                    "{what} never held within {timeout:?}; screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::yield_now();
    }
}

/// Spawns and settles one session in the spec's cwd.
fn spawn_settled(spec: &SpawnSpec, settle_deadline: Duration) -> Result<BenchSession, BenchError> {
    let mut session = BenchSession::spawn(spec)?;
    if !session.settle(SettleBound {
        quiet: Duration::from_millis(500),
        deadline: settle_deadline,
    }) {
        return Err(BenchError::Desync {
            context: format!(
                "startup never went quiet within {settle_deadline:?}; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    Ok(session)
}

/// Opens the picker and waits for its first result rows.
fn open_picker(session: &mut BenchSession) -> Result<(), BenchError> {
    session.send(OPEN_COMMAND)?;
    session.send(b"\r")?;
    wait_for(session, FIRST_PAGE_TIMEOUT, "picker first results", |s| {
        count_token(s, BULK_TOKEN) >= 1
    })
}

/// Closes the picker and waits for its rows to leave the screen.
fn close_picker(session: &mut BenchSession) -> Result<(), BenchError> {
    session.send(b"\x1b")?;
    wait_for(session, FIRST_PAGE_TIMEOUT, "picker overlay closed", |s| {
        count_token(s, BULK_TOKEN) == 0 && count_token(s, PROBE_TOKEN) == 0
    })
}

/// The match phase: one session over the 100k corpus, corpus residency
/// proven by the probe query before any sample is timed, then
/// `protocol.trials` trials of alternating type/erase samples.
fn run_match_phase(
    spec: &SpawnSpec,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<Vec<Distribution>, BenchError> {
    let mut session = spawn_settled(spec, settle_deadline)?;
    open_picker(&mut session)?;
    // residency: all ten spread probes answered means the walk has
    // reached every band of the directory range and the matcher answered
    // a query over what it received; combined with the settle below this
    // is what "100k resident" rests on, and the excluded warmup samples
    // absorb any remainder
    session.send(PROBE_QUERY)?;
    wait_for(
        &mut session,
        FULL_WALK_TIMEOUT,
        "all corpus probes resident",
        |s| count_token(s, PROBE_TOKEN) >= PROBE_FILES,
    )?;
    for _ in 0..PROBE_QUERY.len() {
        session.send(b"\x7f")?;
    }
    wait_for(
        &mut session,
        FIRST_PAGE_TIMEOUT,
        "query cleared to full corpus",
        |s| count_token(s, BULK_TOKEN) >= 1,
    )?;
    if !session.settle(SettleBound {
        quiet: Duration::from_secs(2),
        deadline: FULL_WALK_TIMEOUT,
    }) {
        return Err(BenchError::Desync {
            context: format!(
                "match corpus never settled; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    let mut trials = Vec::with_capacity(protocol.trials);
    for _ in 0..protocol.trials {
        let mut samples = Vec::with_capacity(protocol.samples);
        for index in 0..protocol.samples {
            let start = Instant::now();
            if index % 2 == 0 {
                // typing the query: done when the one label containing an
                // `a` is the result set and every bulk row is gone
                session.send(MATCH_QUERY)?;
                wait_for(
                    &mut session,
                    protocol.sample_timeout,
                    "match results for the typed query",
                    |s| {
                        s.with_screen(|screen| {
                            screen_holds(screen, MATCH_TARGET) && !screen_holds(screen, BULK_TOKEN)
                        })
                    },
                )?;
            } else {
                // erasing it: done when bulk rows are back, which needs a
                // fresh match pass over the full resident corpus
                session.send(b"\x7f")?;
                wait_for(
                    &mut session,
                    protocol.sample_timeout,
                    "match results for the erased query",
                    |s| s.with_screen(|screen| screen_holds(screen, BULK_TOKEN)),
                )?;
            }
            samples.push(elapsed_ms(start));
            std::thread::sleep(protocol.inter_sample);
        }
        // an odd sample count leaves the query typed; erase it so every
        // trial starts from the empty query
        if protocol.samples % 2 == 1 {
            session.send(b"\x7f")?;
            wait_for(&mut session, protocol.sample_timeout, "query reset", |s| {
                s.with_screen(|screen| screen_holds(screen, BULK_TOKEN))
            })?;
        }
        trials.push(Distribution::from_samples(&samples, protocol.warmup)?);
    }
    close_picker(&mut session)?;
    session.shutdown();
    Ok(trials)
}

/// The scan phase: one session over the 1M corpus; a full-walk warm pass
/// first, then trials of open-to-first-page samples with one streaming
/// observation attempted per trial.
fn run_scan_phase(
    spec: &SpawnSpec,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<(Vec<Distribution>, StreamingEvidence), BenchError> {
    let mut session = spawn_settled(spec, settle_deadline)?;
    // warm pass: hold the picker open until every probe has been walked,
    // so the measured opens below run against a warm metadata cache --
    // the condition the spec row states
    open_picker(&mut session)?;
    session.send(PROBE_QUERY)?;
    wait_for(
        &mut session,
        FULL_WALK_TIMEOUT,
        "warm-cache full walk",
        |s| count_token(s, PROBE_TOKEN) >= PROBE_FILES,
    )?;
    close_picker(&mut session)?;
    let mut trials = Vec::with_capacity(protocol.trials);
    let mut streaming: Option<StreamingEvidence> = None;
    for trial in 0..protocol.trials {
        let mut samples = Vec::with_capacity(SCAN_SAMPLES + SCAN_WARMUP);
        for index in 0..(SCAN_SAMPLES + SCAN_WARMUP) {
            // the command is typed first and the clock starts at the
            // confirming <CR>, so cmdline echo never counts against the
            // 100 ms first-page budget
            session.send(OPEN_COMMAND)?;
            wait_for(
                &mut session,
                FIRST_PAGE_TIMEOUT,
                "open command echoed",
                |s| s.with_screen(|screen| screen_holds(screen, "View picker files")),
            )?;
            let start = Instant::now();
            session.send(b"\r")?;
            wait_for(&mut session, FIRST_PAGE_TIMEOUT, "first result page", |s| {
                count_token(s, BULK_TOKEN) >= FIRST_PAGE_ROWS
            })?;
            samples.push(elapsed_ms(start));
            // one streaming observation per trial, on the last measured
            // open, where the walk this open started is freshest
            if streaming.is_none() && index == SCAN_SAMPLES + SCAN_WARMUP - 1 {
                streaming = observe_streaming(&mut session, trial)?;
            }
            close_picker(&mut session)?;
            std::thread::sleep(protocol.inter_sample);
        }
        trials.push(Distribution::from_samples(&samples, SCAN_WARMUP)?);
    }
    session.shutdown();
    let Some(streaming) = streaming else {
        return Err(BenchError::Desync {
            context: format!(
                "streaming never observed: in {} attempts the probe result count never grew \
                 after its first paint, so painted-before-scan-complete could not be attested",
                protocol.trials
            ),
        });
    };
    Ok((trials, streaming))
}

/// Types the probe query against the open picker and watches the visible
/// result count. Growth between two paints with no input in between is
/// the streaming observation; a first paint already showing every probe
/// is inconclusive (the walk may or may not have finished) and reports
/// `None` so the caller can try again next trial.
fn observe_streaming(
    session: &mut BenchSession,
    trial: usize,
) -> Result<Option<StreamingEvidence>, BenchError> {
    session.send(PROBE_QUERY)?;
    let deadline = Instant::now() + FULL_WALK_TIMEOUT;
    let mut first_seen: Option<usize> = None;
    loop {
        let count = count_token(session, PROBE_TOKEN);
        match first_seen {
            None if count > 0 => {
                if count >= PROBE_FILES {
                    return Ok(None);
                }
                first_seen = Some(count);
            }
            Some(seen) if count > seen => {
                return Ok(Some(StreamingEvidence {
                    trial,
                    first_seen: seen,
                    grew_to: count,
                }));
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return Err(BenchError::Desync {
                context: format!(
                    "probe results never grew within {FULL_WALK_TIMEOUT:?} (first paint showed \
                     {first_seen:?}); screen:\n{}",
                    session.screen_text()
                ),
            });
        }
        std::thread::yield_now();
    }
}
