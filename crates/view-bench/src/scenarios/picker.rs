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

/// Measured picker opens per scan trial: the resolution the gated
/// `first_page_p50_ms` is read at.
///
/// Sized from what the statistic has to resolve, not from a guess at what
/// a sample costs. A median of `n` samples carries sampling error
/// proportional to `1/sqrt(n)`, so 100 opens read the same boundary
/// `sqrt(100/12) = 2.89x` tighter than the twelve this row used to take.
/// Twelve is where one hosted leg produced trial medians of 6.80, 5.12
/// and 3.59 ms with nothing between them but the draw, so the claim this
/// count once carried -- that the boundary "stabilizes within a dozen
/// opens" -- was a statement about an estimator too coarse to see its own
/// spread, and is retracted. The resampling check below holds both sides
/// of that: this count resolves the median inside the band, twelve does
/// not.
///
/// Scenario-owned still, and now for a measured reason rather than an
/// asserted one. The whole picker cell has taken at most 248 s on the
/// slowest hosted class, of which the match phase's own inter-sample
/// sleeps are 30 s, so no open there cost more than `(248 - 30) / 42 =
/// 5.2 s` even charging the scan phase's one-time warm walk to it. The
/// 288 opens this count adds are at most 25 min under that bound, on a
/// 98-minute leg against a 180-minute job timeout, and are seconds at
/// what the samples themselves report (a first page of 3 to 7 ms,
/// [`Protocol::inter_sample`] of 10 ms, a close wait of the same order).
/// The protocol's own 1000 samples is what the bound refuses: the same
/// arithmetic prices this row above four hours.
const SCAN_SAMPLES: usize = 100;

/// Warmup opens per scan trial, excluded from every statistic; public so
/// the report can state the discipline the numbers were taken under.
///
/// A tenth of the measured count, which is the share the protocol keeps
/// between its own warmup and samples: what a warmup absorbs on this
/// phase is the first open after a close, whose teardown the sample
/// before it may still have been finishing, and a scan open has no reason
/// to need a different share of its trial than a keystroke does.
pub const SCAN_WARMUP: usize = SCAN_SAMPLES / 10;

/// Bound on one picker open reaching its first page before the run is
/// declared desynced; generous against the 100 ms budget because a
/// desync bound is a liveness check, not a bar.
const FIRST_PAGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on corpus residency and streaming observations: a full 1M-file
/// walk on a cold cache is disk-bound and legitimately slow.
const FULL_WALK_TIMEOUT: Duration = Duration::from_secs(300);

/// Gap a candidate probe-count sample must hold across, unchanged, before
/// [`settled_count_token`] trusts it: comfortably above the scheduler-quantum
/// scale a torn write's remaining bytes take to arrive, negligible against
/// [`FULL_WALK_TIMEOUT`] even summed over every poll in the loop.
const TORN_FRAME_SETTLE: Duration = Duration::from_millis(4);

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
#[cfg(unix)]
const REQUIRED_BYTES: u64 = 1 << 30;
#[cfg(unix)]
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
        if let Some(i) = probe_index(d, dirs) {
            let probe = sub.join(format!("rsentfile{i}.txt"));
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

/// A single OS-level write from the picker can arrive at the bench's pty
/// reader split across more than one channel chunk; a `count_token` read
/// taken between two chunks of the *same* redraw sees a partial count, and
/// the next read (once the rest of that redraw's bytes are drained) sees a
/// higher one -- a single frame torn in transit, misreadable as growth
/// between two distinct paints. Confirming the count is unchanged across a
/// short settle window before trusting it filters that tear out: a
/// still-arriving redraw keeps moving within the window and is reported as
/// not yet trustworthy, while a genuinely finished (or genuinely paused)
/// redraw reads the same value on both sides of it.
fn settled_count_token(session: &mut BenchSession, token: &str) -> Option<usize> {
    let before = count_token(session, token);
    std::thread::sleep(TORN_FRAME_SETTLE);
    let after = count_token(session, token);
    debounced(before, after)
}

/// A read is trustworthy only once it stops moving across the settle
/// window: two chunks of the same torn frame read equal on both sides,
/// while a still-arriving redraw (or a genuine transition to a higher
/// count) reads differently and is reported as not yet settled.
fn debounced(before: usize, after: usize) -> Option<usize> {
    (before == after).then_some(after)
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
/// result count. Growth between two settled samples with no input in
/// between is the streaming observation; a first paint already showing
/// every probe is inconclusive (the walk may or may not have finished)
/// and reports `None` so the caller can try again next trial.
fn observe_streaming(
    session: &mut BenchSession,
    trial: usize,
) -> Result<Option<StreamingEvidence>, BenchError> {
    session.send(PROBE_QUERY)?;
    let deadline = Instant::now() + FULL_WALK_TIMEOUT;
    let mut first_seen: Option<usize> = None;
    loop {
        if let Some(count) = settled_count_token(session, PROBE_TOKEN) {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::{debounced, SCAN_SAMPLES, SCAN_WARMUP};
    use crate::sampling::Distribution;

    /// The count whose estimator this row shipped on, kept here as the
    /// thing the check below refuses rather than as history: a bar that
    /// only the passing side ever meets cannot say whether it bars
    /// anything.
    const REFUSED_SAMPLES: usize = 12;

    /// The leg-to-leg half-width the tighter of the two hosted classes
    /// publishes for this statistic (gh-linux 25.2%, against gh-macos
    /// 35.4%): the spread a headroom factor for the row is sized on.
    const CLASS_HALF_WIDTH: f64 = 0.252;

    /// How wide the sampling spread of the gated median is allowed to be,
    /// as a share of the population's own median.
    ///
    /// Derived rather than drawn around the outcomes it separates: an
    /// estimator whose own draw is the widest term in what a class
    /// publishes makes that band a characterization of the sampler, so
    /// the draw is allowed at most half of the tighter class's own
    /// half-width. That is a bar the count taken can miss on its own
    /// merits -- a count resolving to 0.15 would fail it while still
    /// beating the refused count comfortably.
    const RESOLUTION_BAND: f64 = CLASS_HALF_WIDTH / 2.0;

    /// Resamples per estimator; odd, so the band's own percentiles land on
    /// samples rather than between them.
    const RESAMPLES: usize = 2001;

    /// One draw from a population shaped like the readings this row
    /// records: nine parts first page a few ms apart, one part tail an
    /// order of magnitude out, matching the per-trial p50/p99/max the scan
    /// phase reports on a hosted class (3.6..6.8 ms against ~31 ms). An
    /// LCG rather than a crate, because the point is that the two counts
    /// see the identical population.
    fn draw(state: &mut u64) -> f64 {
        let mut next = || {
            *state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((*state >> 11) as f64) / ((1u64 << 53) as f64)
        };
        let (pick, position) = (next(), next());
        if pick < 0.9 {
            2.5 + position * 6.0
        } else {
            9.0 + position * 24.0
        }
    }

    /// The 5..95 spread of the median of `n` draws, as a share of the
    /// median those draws are taken around: the sampling error the gated
    /// statistic carries at that count, measured rather than assumed.
    fn median_band(n: usize) -> f64 {
        let mut state = 0x5eed;
        let mut medians = Vec::with_capacity(RESAMPLES);
        for _ in 0..RESAMPLES {
            let samples: Vec<f64> = (0..n).map(|_| draw(&mut state)).collect();
            medians.push(Distribution::from_samples(&samples, 0).unwrap().p50());
        }
        let spread = Distribution::from_samples(&medians, 0).unwrap();
        (spread.percentile(95.0) - spread.percentile(5.0)) / 2.0 / spread.p50()
    }

    // The row's breach was the estimator and not the boundary, which is a
    // claim about a sample count and is therefore checkable without a
    // host: over one population, the count the scan trials take resolves
    // its median inside the band and the count they used to take does
    // not. The refused side is what keeps the bar from being a bracket
    // drawn after the fact -- at twelve the estimator's own draw is as
    // wide as everything a hosted class publishes for the row.
    #[test]
    fn the_scan_count_resolves_its_median_where_a_dozen_opens_did_not() {
        let refused = median_band(REFUSED_SAMPLES);
        assert!(
            refused > RESOLUTION_BAND,
            "the count this row refuses must be shown to miss the band, not asserted to: \
             {REFUSED_SAMPLES} samples resolved to {refused:.3} against {RESOLUTION_BAND}"
        );
        let taken = median_band(SCAN_SAMPLES);
        assert!(
            taken <= RESOLUTION_BAND,
            "the gated median must resolve inside the band at the count the phase takes: \
             {SCAN_SAMPLES} samples resolved to {taken:.3} against {RESOLUTION_BAND}"
        );
    }

    // The count was sized on the 1/sqrt(n) law, so the law is what has to
    // hold between the two counts -- a tightening that failed to arrive
    // would leave the size arbitrary even with the band met.
    #[test]
    fn the_scan_count_tightens_the_median_by_the_root_n_law() {
        let tightening = median_band(REFUSED_SAMPLES) / median_band(SCAN_SAMPLES);
        let law = (SCAN_SAMPLES as f64 / REFUSED_SAMPLES as f64).sqrt();
        assert!(
            (tightening - law).abs() / law < 0.2,
            "sampling error goes as 1/sqrt(n), so {SCAN_SAMPLES} against {REFUSED_SAMPLES} \
             samples must tighten the median by about {law:.3}x; measured {tightening:.3}x"
        );
    }

    // Warmup is a share of the trial rather than a fixed pair of opens:
    // the protocol keeps a tenth of its samples as warmup, and a scan open
    // has no reason to want a different share.
    #[test]
    fn scan_warmup_keeps_the_protocol_share_of_its_measured_count() {
        let protocol = crate::scenarios::Protocol::default();
        assert_eq!(
            SCAN_WARMUP * protocol.samples,
            protocol.warmup * SCAN_SAMPLES,
            "scan warmup {SCAN_WARMUP} of {SCAN_SAMPLES} must hold the protocol's own \
             {} of {}",
            protocol.warmup,
            protocol.samples
        );
    }

    // Two reads of the same torn frame must not register as growth --
    // a slow/chunked pty read is indistinguishable at the byte level
    // from the walk actually producing new results unless the debounce
    // itself is pinned.
    #[test]
    fn an_unchanged_read_across_the_settle_window_is_trusted() {
        assert_eq!(debounced(3, 3), Some(3));
    }

    // A read that is still moving across the settle window (the redraw
    // is mid-flight) must not be trusted yet.
    #[test]
    fn a_read_that_moved_across_the_settle_window_is_not_trusted() {
        assert_eq!(debounced(3, 5), None);
    }

    // The debounce has no direction bias: a read that appears to shrink
    // (e.g. an intermediate partial redraw) is equally untrusted.
    #[test]
    fn a_read_that_shrank_across_the_settle_window_is_not_trusted() {
        assert_eq!(debounced(5, 3), None);
    }
}
