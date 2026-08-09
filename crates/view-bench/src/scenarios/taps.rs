//! The internal-boundary rows: key at pty to RPC bytes written
//! (`input_path`) and redraw parsed to terminal write (`output_path`).
//! Both drive the bench-taps build of view, which emits `<tag> <seq>
//! <nanos>\n` records over a FIFO the harness owns; timestamps on both
//! sides come from the same `CLOCK_MONOTONIC`, so harness-to-child
//! deltas are valid on one machine.
//!
//! Dropped records are detectable, never silent: each crate's tap
//! sequence is contiguous, so a hole in the reassembled sequence fails
//! the run instead of mispairing timestamps.

use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::pairing::{paired_summary, NvimSamples, PairedSummary, ViewSamples};
use crate::sampling::{interleave_schedule, median_of_trials, Distribution, Side};
use crate::scenarios::clock::monotonic_nanos;
use crate::scenarios::echo::{label, SideState};
use crate::scenarios::Protocol;
use crate::session::{
    BenchSession, NvimSpec, SettleBound, SpawnSpec, ViewSpec, GRID_COLS, GRID_ROWS,
};
use crate::BenchError;

/// One parsed tap record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapRecord {
    pub tag: u8,
    pub seq: u64,
    pub nanos: i64,
}

/// Parses one `<tag> <seq> <nanos>` line.
#[must_use]
pub fn parse_record(line: &str) -> Option<TapRecord> {
    let mut parts = line.split_ascii_whitespace();
    let tag = *parts.next()?.as_bytes().first()?;
    let seq = parts.next()?.parse().ok()?;
    let nanos = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(TapRecord { tag, seq, nanos })
}

/// Every tap tag, grouped by the crate whose sequence counter numbers it.
/// A tag missing from this table is a tag whose loss no drop check can
/// see, so the chain walkers below assert their own tags against it.
const TAG_ORIGINS: [(&str, &[u8]); 2] = [("view-engine", b"WRS"), ("view-tui", b"TKUBFPGC")];

/// Verifies each crate's tap sequence stream is contiguous. The engine's
/// two tags share one counter and the tui's tags share their own, so the
/// streams are checked per origin.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] naming the missing span when records
/// were dropped (a full pipe under a non-blocking tap write).
pub fn verify_no_drops(records: &[TapRecord]) -> Result<(), BenchError> {
    for (name, tags) in TAG_ORIGINS {
        let mut seqs: Vec<u64> = records
            .iter()
            .filter(|r| tags.contains(&r.tag))
            .map(|r| r.seq)
            .collect();
        seqs.sort_unstable();
        for pair in seqs.windows(2) {
            if pair[1] != pair[0] + 1 {
                return Err(BenchError::Desync {
                    context: format!(
                        "{name} tap records dropped between seq {} and {} \
                         (pipe overflow); timings cannot be paired trustworthily",
                        pair[0], pair[1]
                    ),
                });
            }
        }
    }
    Ok(())
}

/// Creates a FIFO at `path`, readable and writable by its owner.
///
/// Two implementations because no one crate here reaches `mkfifo(2)` on
/// both platforms: `rustix` excludes `mknodat` and `mkfifoat` on Apple
/// targets, and the standard library has no FIFO constructor at all. Both
/// arms produce the same file, so every caller above is platform-blind.
#[cfg(not(target_vendor = "apple"))]
fn create_fifo(path: &std::path::Path) -> std::io::Result<()> {
    rustix::fs::mknodat(
        rustix::fs::CWD,
        path,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .map_err(std::io::Error::from)
}

#[cfg(target_vendor = "apple")]
fn create_fifo(path: &std::path::Path) -> std::io::Result<()> {
    nix::unistd::mkfifo(
        path,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .map_err(std::io::Error::from)
}

/// The harness end of the tap channel: a FIFO plus a reader thread
/// accumulating parsed records.
pub struct TapPipe {
    records: Arc<Mutex<Vec<TapRecord>>>,
    stop: Arc<AtomicBool>,
    path: std::path::PathBuf,
}

impl TapPipe {
    /// Creates the FIFO at `path` and opens the reading end (non-blocking,
    /// before any writer exists) with a background accumulator thread.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Desync`] if the FIFO cannot be created or
    /// opened.
    pub fn create(path: &std::path::Path) -> Result<Self, BenchError> {
        create_fifo(path).map_err(|e| BenchError::Desync {
            context: format!("creating tap fifo {}: {e}", path.display()),
        })?;
        let file = rustix::fs::openat(
            rustix::fs::CWD,
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        )
        .map_err(|e| BenchError::Desync {
            context: format!("opening tap fifo {}: {e}", path.display()),
        })?;
        let records = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_records = Arc::clone(&records);
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut file = std::fs::File::from(file);
            let mut pending = String::new();
            let mut buf = [0_u8; 8192];
            while !thread_stop.load(Ordering::Relaxed) {
                match file.read(&mut buf) {
                    Ok(0) => std::thread::sleep(Duration::from_micros(500)),
                    Ok(n) => {
                        pending.push_str(&String::from_utf8_lossy(&buf[..n]));
                        while let Some(newline) = pending.find('\n') {
                            let line: String = pending.drain(..=newline).collect();
                            if let Some(record) = parse_record(line.trim_end()) {
                                if let Ok(mut sink) = thread_records.lock() {
                                    sink.push(record);
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_micros(500));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            records,
            stop,
            path: path.to_path_buf(),
        })
    }

    /// The FIFO this pipe reads, so a caller holding the pipe can open a
    /// second writing end without threading the path alongside it.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Copies (without draining) every record whose timestamp falls in
    /// `[from, to]`, so a sample's sub-boundary taps can be read while the
    /// caller's ordinary drain cadence is left untouched.
    #[must_use]
    pub fn records_between(&self, from: i64, to: i64) -> Vec<TapRecord> {
        self.records
            .lock()
            .map(|sink| {
                sink.iter()
                    .filter(|r| r.nanos >= from && r.nanos <= to)
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Takes every record accumulated so far.
    #[must_use]
    pub fn drain(&self) -> Vec<TapRecord> {
        self.records
            .lock()
            .map(|mut sink| std::mem::take(&mut *sink))
            .unwrap_or_default()
    }

    /// Tight-polls until a record matching `pred` arrives (draining
    /// nothing; the caller drains between samples), or `timeout` passes.
    #[must_use]
    pub fn wait_for(
        &self,
        timeout: Duration,
        mut pred: impl FnMut(&TapRecord) -> bool,
    ) -> Option<TapRecord> {
        let deadline = Instant::now() + timeout;
        let mut seen = 0;
        loop {
            if let Ok(sink) = self.records.lock() {
                for record in sink.iter().skip(seen) {
                    if pred(record) {
                        return Some(*record);
                    }
                }
                seen = sink.len();
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::yield_now();
        }
    }
}

impl Drop for TapPipe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Prepares one taps-build session for key driving: settle, insert mode,
/// settle again (plugin churn), then discard every startup tap record.
fn prepare(
    spec: &SpawnSpec,
    pipe: &TapPipe,
    settle_deadline: Duration,
) -> Result<BenchSession, BenchError> {
    let mut session = BenchSession::spawn(spec)?;
    if !session.settle(SettleBound {
        quiet: Duration::from_secs(2),
        deadline: settle_deadline,
    }) {
        return Err(BenchError::Desync {
            context: format!(
                "startup never went quiet; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    session.send(b"i")?;
    if !session.settle(SettleBound {
        quiet: Duration::from_secs(2),
        deadline: settle_deadline,
    }) {
        return Err(BenchError::Desync {
            context: format!(
                "insert-mode entry never went quiet; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    let _ = pipe.drain();
    Ok(session)
}

/// A taps-row outcome: per-trial p99s and their median, plus every
/// record for the caller's drop verification already applied.
#[derive(Debug)]
pub struct TapsOutcome {
    pub trial_distributions: Vec<Distribution>,
    /// Median across trials of the per-trial p99, in the row's unit
    /// (microseconds for `input_path`, milliseconds for `output_path`).
    pub gated_p99: f64,
    /// Observed sub-boundary decomposition of the row's interval, pooled
    /// across every measured (non-warmup) sample of every trial.
    /// Attribution evidence only, never gated: the gate stays on the
    /// row's end-to-end boundary.
    pub segments: Vec<Segment>,
    /// The tap operation's own cost, characterized against the live
    /// session rather than against an idle host, so the number the bar
    /// compares is the one this row's taps actually paid.
    pub overhead: Distribution,
    /// The write spacing the characterization above needed to reach full
    /// delivery on this host. Reported because a host that needs an
    /// unusual pace is a host whose pipe behaves differently from the one
    /// the base pace was tuned on, and that is worth seeing in the row.
    pub overhead_pace: Duration,
}

/// One observed sub-interval of a taps row.
#[derive(Debug)]
pub struct Segment {
    pub label: &'static str,
    pub p50_us: f64,
    pub p99_us: f64,
    /// Samples whose full tag chain resolved inside the row's window; a
    /// count materially below the row's sample count means the chain was
    /// often broken and the percentiles should not be trusted.
    pub samples: usize,
}

/// Walks `tags` through `records` in order: each tag must appear at or
/// after the previous match's timestamp (starting from `from`). Returns
/// the matched records, or `None` when any tag never appears.
fn tag_chain(records: &[TapRecord], tags: &[u8], from: i64) -> Option<Vec<TapRecord>> {
    let mut bound = from;
    let mut matched = Vec::with_capacity(tags.len());
    for &tag in tags {
        let hit = records
            .iter()
            .filter(|r| r.tag == tag && r.nanos >= bound)
            .min_by_key(|r| r.nanos)?;
        bound = hit.nanos;
        matched.push(*hit);
    }
    Some(matched)
}

/// Reduces pooled per-segment samples to [`Segment`] summaries; a label
/// with no resolved samples is reported with zeroed percentiles rather
/// than dropped, so a broken chain is visible instead of silent.
fn summarize_segments(labels: &[&'static str], pools: &[Vec<f64>]) -> Vec<Segment> {
    labels
        .iter()
        .zip(pools)
        .map(|(label, pool)| {
            let dist = Distribution::from_samples(pool, 0).ok();
            Segment {
                label,
                p50_us: dist.as_ref().map_or(0.0, Distribution::p50),
                p99_us: dist.as_ref().map_or(0.0, Distribution::p99),
                samples: pool.len(),
            }
        })
        .collect()
}

#[allow(clippy::cast_precision_loss)]
fn delta_us(from: i64, to: i64) -> f64 {
    (to - from) as f64 / 1000.0
}

/// The input-path row's tap chain, opening at the key's arrival in view
/// (the gated boundary's start) and closing one tag before the `W` the
/// caller already holds.
const INPUT_CHAIN: &[u8] = b"KUS";

/// One label per interval the chain resolves, including the leading
/// harness-timestamp-to-first-tag one, so there is one more label than
/// chain tag. Only the first is outside the gated boundary.
const INPUT_LABELS: [&str; 4] = [
    "pty->key-read",
    "key-read->loop-wake",
    "loop-wake->rpc-handoff",
    "rpc-handoff->rpc-written",
];

/// Measures the key-to-RPC-written path: per sample, the instrumented
/// view's `K` tap (the key event arriving from the host terminal)
/// matched against the next `W` tap (its RPC bytes written).
///
/// The harness-side timestamp taken before the key byte is written to
/// the pty master opens the *search* window, not the measured interval.
/// It is deliberately not the boundary: the pty transport between that
/// write and view's read is the OS's, is the single largest segment of
/// the round trip, and no view code schedules it, so including it would
/// gate view on a number view cannot move. The `pty->key-read` segment
/// below still reports it, as evidence rather than as a bar.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] on tap loss, missing responses, an
/// unresolvable tap chain, or session failures.
pub fn run_input_path(
    spec: &SpawnSpec,
    pipe: &TapPipe,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<TapsOutcome, BenchError> {
    let mut session = prepare(spec, pipe, settle_deadline)?;
    let (overhead, overhead_pace) =
        characterize_overhead_adaptive(pipe, OVERHEAD_ITERATIONS, OVERHEAD_PACE)?;
    let mut trial_distributions = Vec::with_capacity(protocol.trials);
    let mut all_records = Vec::new();
    let mut pools: Vec<Vec<f64>> = vec![Vec::new(); INPUT_LABELS.len()];
    for _ in 0..protocol.trials {
        let mut deltas_us = Vec::with_capacity(protocol.warmup + protocol.samples);
        for index in 0..(protocol.warmup + protocol.samples) {
            // an untimed fresh line every 100 keys keeps redraw batches
            // representative of steady typing rather than one giant
            // wrapped line; its own taps are drained before the sample
            if index % 100 == 99 {
                session.send(b"\x1bo")?;
                std::thread::sleep(Duration::from_millis(50));
            }
            all_records.extend(pipe.drain());
            let t0 = monotonic_nanos();
            session.send(b"x")?;
            let Some(record) =
                pipe.wait_for(protocol.sample_timeout, |r| r.tag == b'W' && r.nanos >= t0)
            else {
                return Err(BenchError::Desync {
                    context: format!(
                        "no RPC-written tap within {:?} of a keypress; screen:\n{}",
                        protocol.sample_timeout,
                        session.screen_text()
                    ),
                });
            };
            let window = pipe.records_between(t0, record.nanos);
            let Some(chain) = tag_chain(&window, INPUT_CHAIN, t0) else {
                return Err(BenchError::Desync {
                    context: format!(
                        "keypress reached the RPC write with no resolvable K/U/S chain behind \
                         it, so the gated boundary has no opening timestamp; screen:\n{}",
                        session.screen_text()
                    ),
                });
            };
            let Some(read) = chain.first() else {
                return Err(BenchError::Desync {
                    context: "empty tap chain for a keypress".to_string(),
                });
            };
            deltas_us.push(delta_us(read.nanos, record.nanos));
            if index >= protocol.warmup {
                let mut prev = t0;
                for (pool, hit) in pools.iter_mut().zip(&chain) {
                    pool.push(delta_us(prev, hit.nanos));
                    prev = hit.nanos;
                }
                if let Some(pool) = pools.get_mut(chain.len()) {
                    pool.push(delta_us(prev, record.nanos));
                }
            }
            std::thread::sleep(protocol.inter_sample);
        }
        trial_distributions.push(Distribution::from_samples(&deltas_us, protocol.warmup)?);
    }
    all_records.extend(pipe.drain());
    session.shutdown();
    verify_no_drops(&all_records)?;

    let p99s: Vec<f64> = trial_distributions.iter().map(Distribution::p99).collect();
    Ok(TapsOutcome {
        gated_p99: crate::sampling::median_of_trials(&p99s)?,
        trial_distributions,
        segments: summarize_segments(&INPUT_LABELS, &pools),
        overhead,
        overhead_pace,
    })
}

/// One step of pairing a keypress's terminal-write tap with the parsed
/// redraw that explains it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingVerdict {
    /// The earliest `R` at or after the keypress and at or before the
    /// candidate paint: the frame under measurement.
    Paired(TapRecord),
    /// The candidate paint is positively explained by an unclaimed `R`
    /// that predates the keypress (a straggler frame from before the
    /// sample), and a later `T` exists to take its place as the next
    /// candidate.
    Stray(TapRecord),
    /// Nothing decidable yet; the explaining `R` (or a later `T`) may
    /// still be crossing the pipe. A paint with no explaining redraw at
    /// all stays here and lets the sample-timeout abort stand: skipping
    /// it would hide exactly the paints-without-redraw product fault the
    /// desync check exists to catch.
    Pending,
}

/// Decides, from the records seen so far, whether `paint` is explained by
/// a parsed redraw at or after `t0`, is a straggler positively explained
/// by an unclaimed redraw in `(since, t0)` and superseded by a later
/// paint, or cannot be judged yet. `since` is the previous measured
/// frame's paint timestamp: without that floor, every already-claimed `R`
/// from earlier samples in the accumulated record log would count as an
/// explanation and the straggler check could never refuse anything.
fn pair_paint(records: &[TapRecord], since: i64, t0: i64, paint: TapRecord) -> PairingVerdict {
    let explained = records
        .iter()
        .filter(|r| r.tag == b'R' && r.nanos >= t0 && r.nanos <= paint.nanos)
        .min_by_key(|r| r.nanos)
        .copied();
    if let Some(hit) = explained {
        return PairingVerdict::Paired(hit);
    }
    let straggler_explained = records
        .iter()
        .any(|r| r.tag == b'R' && r.nanos > since && r.nanos < t0);
    if !straggler_explained {
        return PairingVerdict::Pending;
    }
    let later = records
        .iter()
        .filter(|r| r.tag == b'T' && r.nanos > paint.nanos)
        .min_by_key(|r| r.nanos)
        .copied();
    match later {
        Some(next) => PairingVerdict::Stray(next),
        None => PairingVerdict::Pending,
    }
}

/// Measures the redraw-parsed-to-terminal-write path: per keypress, the
/// earliest `R` tap after the key is paired with the first `T` tap that
/// follows it (the paint that made the redraw visible).
///
/// # Errors
///
/// Returns [`BenchError::Desync`] on tap loss, a paint with no parsed
/// redraw to explain it, or session failures.
pub fn run_output_path(
    spec: &SpawnSpec,
    pipe: &TapPipe,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<TapsOutcome, BenchError> {
    let mut session = prepare(spec, pipe, settle_deadline)?;
    let (overhead, overhead_pace) =
        characterize_overhead_adaptive(pipe, OVERHEAD_ITERATIONS, OVERHEAD_PACE)?;
    let mut trial_distributions = Vec::with_capacity(protocol.trials);
    let mut all_records = Vec::new();
    let labels = [
        "redraw-parsed->loop-wake",
        "loop-wake->draw-start",
        "draw-start->flush-start",
        "flush-start->term-written",
    ];
    let mut pools: Vec<Vec<f64>> = vec![Vec::new(); labels.len()];
    // the straggler floor for the first sample: redraws older than the
    // prepared, settled session belong to setup frames, not to a burst
    // this loop produced
    let mut last_paint_nanos = monotonic_nanos();
    for _ in 0..protocol.trials {
        let mut deltas_ms = Vec::with_capacity(protocol.warmup + protocol.samples);
        for index in 0..(protocol.warmup + protocol.samples) {
            if index % 100 == 99 {
                session.send(b"\x1bo")?;
                std::thread::sleep(Duration::from_millis(50));
            }
            all_records.extend(pipe.drain());
            let t0 = monotonic_nanos();
            session.send(b"x")?;
            let Some(paint) =
                pipe.wait_for(protocol.sample_timeout, |r| r.tag == b'T' && r.nanos >= t0)
            else {
                return Err(BenchError::Desync {
                    context: format!(
                        "no terminal-write tap within {:?} of a keypress; screen:\n{}",
                        protocol.sample_timeout,
                        session.screen_text()
                    ),
                });
            };
            // grace so the R record (written by a different thread than
            // the T) has crossed the pipe before the pairing scan below.
            // A bounded rescan loop, not one fixed sleep: under host load
            // the parser thread's pipe write can trail the observed T by
            // more than any single grace this pacing could afford, and a
            // scan that ran exactly once then turned a late write into a
            // whole-run desync abort -- reproduced at 1 ms on this class
            // at 1-min loads as low as 0.66, on a tree with no view change
            let sample_deadline = Instant::now() + protocol.sample_timeout;
            let mut paint = paint;
            let mut pairing_deadline = Instant::now() + Duration::from_millis(50);
            let parsed = loop {
                std::thread::sleep(Duration::from_millis(1));
                let window = pipe.drain();
                all_records.extend(window.iter().copied());
                match pair_paint(&all_records, last_paint_nanos, t0, paint) {
                    PairingVerdict::Paired(hit) => break Some(hit),
                    // a paint positively explained by an unclaimed redraw
                    // from before the keypress is a straggler frame (the
                    // insert-mode reset burst can cross the settle under
                    // host load), not the frame under measurement: only
                    // after the R grace has expired may it be skipped for
                    // the next paint, because R records cross the pipe on
                    // a different thread and can trail the T they explain
                    PairingVerdict::Stray(next) if Instant::now() >= pairing_deadline => {
                        paint = next;
                        pairing_deadline = Instant::now() + Duration::from_millis(50);
                    }
                    PairingVerdict::Stray(_) | PairingVerdict::Pending => {
                        if Instant::now() >= pairing_deadline && Instant::now() >= sample_deadline {
                            break None;
                        }
                    }
                }
            };
            let Some(parsed) = parsed else {
                return Err(BenchError::Desync {
                    context: "a paint arrived with no parsed redraw between the keypress \
                              and the terminal write, and no later paint paired within \
                              the sample timeout"
                        .to_string(),
                });
            };
            last_paint_nanos = paint.nanos;
            #[allow(clippy::cast_precision_loss)]
            deltas_ms.push((paint.nanos - parsed.nanos) as f64 / 1_000_000.0);
            if index >= protocol.warmup {
                let start = parsed.nanos;
                let in_window: Vec<TapRecord> = all_records
                    .iter()
                    .filter(|r| r.nanos >= start && r.nanos <= paint.nanos)
                    .copied()
                    .collect();
                if let Some(chain) = tag_chain(&in_window, b"UBF", start) {
                    let mut prev = start;
                    for (pool, hit) in pools.iter_mut().zip(&chain) {
                        pool.push(delta_us(prev, hit.nanos));
                        prev = hit.nanos;
                    }
                    if let Some(pool) = pools.get_mut(chain.len()) {
                        pool.push(delta_us(prev, paint.nanos));
                    }
                }
            }
            std::thread::sleep(protocol.inter_sample);
        }
        trial_distributions.push(Distribution::from_samples(&deltas_ms, protocol.warmup)?);
    }
    all_records.extend(pipe.drain());
    session.shutdown();
    verify_no_drops(&all_records)?;

    let p99s: Vec<f64> = trial_distributions.iter().map(Distribution::p99).collect();
    Ok(TapsOutcome {
        gated_p99: crate::sampling::median_of_trials(&p99s)?,
        trial_distributions,
        segments: summarize_segments(&labels, &pools),
        overhead,
        overhead_pace,
    })
}

/// The tags one keystroke walks, in order, across view's whole echo round
/// trip: key decoded off the host terminal, runtime loop woken, RPC
/// encoded and handed off, RPC bytes written to the engine, then the
/// engine's redraw parsed, the loop woken again, the frame's draw
/// started, its frame prepared, its paint area resolved, its damaged rows
/// composited, its bytes flushed, and the terminal write completed.
const ECHO_CHAIN: &[u8] = b"KUSWRUBPGCFT";

/// One label per interval of [`ECHO_CHAIN`], anchored at the harness's
/// pre-keystroke timestamp and closed by the harness observing the echoed
/// glyph, so the labelled stages tile the whole measured round trip with
/// no gap left implicit.
const ECHO_LABELS: &[&str] = &[
    "pty->key-decoded",
    "key-decoded->loop-wake",
    "loop-wake->rpc-handoff",
    "rpc-handoff->rpc-written",
    "rpc-written->redraw-parsed",
    "redraw-parsed->loop-wake",
    "loop-wake->draw-start",
    "draw-start->frame-prepared",
    "frame-prepared->area-resolved",
    "area-resolved->composed",
    "composed->flush-start",
    "flush-start->term-written",
    "term-written->glyph-seen",
];

/// The chain tags one keystroke's own round trip emits exactly once. `U`
/// is absent because it appears twice by design, once per side, and is
/// guarded per bracket instead.
///
/// The walker takes the earliest match at or after the previous one, so a
/// second occurrence of any of these inside one sample's window lets the
/// keystroke pair with a redraw round that is not its own: the stages
/// spanning the two rounds collapse and their time reappears in the
/// closing stage, understating exactly the part of the round trip an
/// attribution is trying to weigh. Uniqueness over the window forecloses
/// that, so it is counted per sample rather than assumed.
const SINGLE_ROUND_TAGS: [u8; 10] = *b"KSWRBPGCFT";

/// Index into a resolved [`ECHO_CHAIN`] of the `S` (RPC handoff) match:
/// with the `K` match it brackets the input-side loop wake.
const INPUT_WAKE_BRACKET_END: usize = 2;
/// Index of the `R` (redraw parsed) match; with the `B` match below it
/// brackets the output-side loop wake.
const OUTPUT_WAKE_BRACKET_START: usize = 4;
/// Index of the `B` (draw start) match.
const OUTPUT_WAKE_BRACKET_END: usize = 6;

/// A paired echo round trip decomposed into [`ECHO_LABELS`] stages.
#[derive(Debug)]
pub struct EchoPathOutcome {
    pub trials: Vec<PairedSummary>,
    /// Median across trials of the per-trial p50 ratio, the same statistic
    /// the gated echo row reports, measured here on the instrumented
    /// build so the two can be differenced into an instrumentation cost.
    pub gated_ratio_p50: f64,
    /// The stage decomposition, pooled across every measured sample of
    /// every trial.
    pub segments: Vec<Segment>,
    /// The view side's whole measured round trip, pooled the same way, so
    /// the stage sum can be differenced against it rather than against a
    /// percentile computed by another route.
    pub view_total: Segment,
    /// Bare nvim's whole measured round trip, pooled the same way.
    pub nvim_total: Segment,
    /// The tap operation's own cost, characterized against the live
    /// session, so the stage percentiles below can be read against what
    /// the instrumentation between them charged.
    pub overhead: Distribution,
    /// The write spacing the characterization above needed to reach full
    /// delivery on this host. Reported because a host that needs an
    /// unusual pace is a host whose pipe behaves differently from the one
    /// the base pace was tuned on, and that is worth seeing in the row.
    pub overhead_pace: Duration,
    /// Measured view samples whose tag chain never resolved. These
    /// contribute to [`Self::view_total`] but to no stage, so a count
    /// above zero is exactly how far the stage percentiles and the total
    /// stopped describing the same population.
    pub unresolved: usize,
    /// Resolved samples carrying more than one loop wake between `K` and
    /// `S`. The chain's ordering rule takes the earliest wake, so a second
    /// one shifts time between `key-decoded->loop-wake` and
    /// `loop-wake->rpc-handoff` without changing their sum.
    pub ambiguous_input_wakes: usize,
    /// Resolved samples carrying more than one loop wake between `R` and
    /// `B`; the output-side counterpart of
    /// [`Self::ambiguous_input_wakes`].
    pub ambiguous_output_wakes: usize,
    /// Per-tag count of resolved samples whose window held that
    /// [`SINGLE_ROUND_TAGS`] entry more than once, in that table's order.
    /// Any non-zero entry means some published stages describe a redraw
    /// round the chain cannot prove the keystroke caused.
    pub repeated_round_tags: Vec<(u8, usize)>,
    /// Resolved samples carrying a repeat of any [`SINGLE_ROUND_TAGS`]
    /// entry. The per-sample rollup of [`Self::repeated_round_tags`]: one
    /// spurious round repeats several tags at once, so the per-tag counts
    /// do not add.
    pub multiplicity_flagged: usize,
}

impl EchoPathOutcome {
    /// Sum of the stage p50s. Percentiles do not add, so this is not the
    /// total's p50; the difference between the two is the residual the
    /// attribution has to report rather than distribute.
    #[must_use]
    pub fn stage_p50_sum_us(&self) -> f64 {
        self.segments.iter().map(|s| s.p50_us).sum()
    }

    /// Measured total p50 minus [`Self::stage_p50_sum_us`].
    #[must_use]
    pub fn residual_p50_us(&self) -> f64 {
        self.view_total.p50_us - self.stage_p50_sum_us()
    }
}

/// Number of `tag` records in `[from, to]`.
fn count_tag_between(records: &[TapRecord], tag: u8, from: i64, to: i64) -> usize {
    records
        .iter()
        .filter(|r| r.tag == tag && r.nanos >= from && r.nanos <= to)
        .count()
}

/// Accumulates one measured sample's stage deltas into `pools`, reporting
/// whether the chain resolved and how many loop wakes each bracket held.
struct ChainOutcome {
    resolved: bool,
    ambiguous_input_wakes: bool,
    ambiguous_output_wakes: bool,
    /// Which [`SINGLE_ROUND_TAGS`] entries occurred more than once in the
    /// sample's window, positionally aligned with that table.
    repeated_round_tags: [bool; SINGLE_ROUND_TAGS.len()],
}

fn accumulate_chain(
    window: &[TapRecord],
    from: i64,
    to: i64,
    pools: &mut [Vec<f64>],
) -> ChainOutcome {
    let mut repeated_round_tags = [false; SINGLE_ROUND_TAGS.len()];
    for (slot, tag) in repeated_round_tags.iter_mut().zip(SINGLE_ROUND_TAGS) {
        *slot = count_tag_between(window, tag, from, to) > 1;
    }
    let Some(chain) = tag_chain(window, ECHO_CHAIN, from) else {
        return ChainOutcome {
            resolved: false,
            ambiguous_input_wakes: false,
            ambiguous_output_wakes: false,
            repeated_round_tags,
        };
    };
    let mut prev = from;
    for (pool, hit) in pools.iter_mut().zip(&chain) {
        pool.push(delta_us(prev, hit.nanos));
        prev = hit.nanos;
    }
    if let Some(pool) = pools.get_mut(chain.len()) {
        pool.push(delta_us(prev, to));
    }
    let wakes = |start: usize, end: usize| {
        chain
            .get(start)
            .zip(chain.get(end))
            .is_some_and(|(a, b)| count_tag_between(window, b'U', a.nanos, b.nanos) > 1)
    };
    ChainOutcome {
        resolved: true,
        ambiguous_input_wakes: wakes(0, INPUT_WAKE_BRACKET_END),
        ambiguous_output_wakes: wakes(OUTPUT_WAKE_BRACKET_START, OUTPUT_WAKE_BRACKET_END),
        repeated_round_tags,
    }
}

/// Drives the echo scenario against the instrumented build with bare nvim
/// paired in the same run, decomposing every measured view sample into the
/// [`ECHO_LABELS`] stages.
///
/// The view side is driven by the echo scenario's own typing state, so the
/// measured boundary here is the echo row's boundary and not a second,
/// separately-defined one; `W -> R` (the engine's own keystroke
/// processing plus both pipe crossings of the `--embed` protocol) is
/// reported as a stage in its own right rather than derived by subtracting
/// one row's total from another's.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] on tap loss, an editor that stops
/// responding within the sample timeout, or any underlying session error.
pub fn run_echo_path(
    view_spec: ViewSpec<'_>,
    nvim_spec: NvimSpec<'_>,
    pipe: &TapPipe,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<EchoPathOutcome, BenchError> {
    let ViewSpec(view) = view_spec;
    let NvimSpec(nvim) = nvim_spec;
    let mut view_state = SideState::prepare(view, settle_deadline).map_err(|e| label("view", e))?;
    let (overhead, overhead_pace) =
        characterize_overhead_adaptive(pipe, OVERHEAD_ITERATIONS, OVERHEAD_PACE)?;
    let mut nvim_state = SideState::prepare(nvim, settle_deadline).map_err(|e| label("nvim", e))?;
    let _ = pipe.drain();

    let mut trials = Vec::with_capacity(protocol.trials);
    let mut all_records = Vec::new();
    let mut pools: Vec<Vec<f64>> = vec![Vec::new(); ECHO_LABELS.len()];
    let mut view_totals: Vec<f64> = Vec::new();
    let mut nvim_totals: Vec<f64> = Vec::new();
    let mut unresolved = 0;
    let mut ambiguous_input_wakes = 0;
    let mut ambiguous_output_wakes = 0;
    let mut repeats = [0_usize; SINGLE_ROUND_TAGS.len()];
    let mut multiplicity_flagged = 0;

    for trial in 0..protocol.trials {
        if trial > 0 {
            view_state.reset_buffer().map_err(|e| label("view", e))?;
            nvim_state.reset_buffer().map_err(|e| label("nvim", e))?;
        }
        view_state.clear_samples();
        nvim_state.clear_samples();
        let start = if trial % 2 == 0 {
            Side::View
        } else {
            Side::Nvim
        };
        let per_side = protocol.warmup + protocol.samples;
        let (mut view_taken, mut nvim_taken) = (0_usize, 0_usize);
        for block in interleave_schedule(per_side, protocol.block, start) {
            for _ in 0..block.count {
                match block.side {
                    Side::View => {
                        // the drain is what makes the copy below this
                        // sample's records and no other's: everything
                        // older leaves the accumulator here, and
                        // `sample_one`'s trailing inter-sample gap is the
                        // grace that lets the final tap of the sample
                        // cross the fifo before it is read
                        all_records.extend(pipe.drain());
                        let (t0, seen) = view_state
                            .sample_one(protocol)
                            .map_err(|e| label("view", e))?;
                        if view_taken >= protocol.warmup {
                            view_totals.push(delta_us(t0, seen));
                            let window = pipe.records_between(t0, seen);
                            let outcome = accumulate_chain(&window, t0, seen, &mut pools);
                            if outcome.resolved {
                                ambiguous_input_wakes += usize::from(outcome.ambiguous_input_wakes);
                                ambiguous_output_wakes +=
                                    usize::from(outcome.ambiguous_output_wakes);
                                for (total, flagged) in
                                    repeats.iter_mut().zip(outcome.repeated_round_tags)
                                {
                                    *total += usize::from(flagged);
                                }
                                multiplicity_flagged +=
                                    usize::from(outcome.repeated_round_tags.contains(&true));
                            } else {
                                unresolved += 1;
                            }
                        }
                        view_taken += 1;
                    }
                    Side::Nvim => {
                        let (t0, seen) = nvim_state
                            .sample_one(protocol)
                            .map_err(|e| label("nvim", e))?;
                        if nvim_taken >= protocol.warmup {
                            nvim_totals.push(delta_us(t0, seen));
                        }
                        nvim_taken += 1;
                    }
                }
            }
        }
        trials.push(paired_summary(
            ViewSamples(&view_state.raw_ms()),
            NvimSamples(&nvim_state.raw_ms()),
            protocol.warmup,
        )?);
    }

    all_records.extend(pipe.drain());
    view_state.shutdown();
    nvim_state.shutdown();
    verify_no_drops(&all_records)?;

    let totals = summarize_segments(
        &["TOTAL view t0->glyph-seen", "TOTAL nvim t0->glyph-seen"],
        &[view_totals, nvim_totals],
    );
    let mut totals = totals.into_iter();
    let (Some(view_total), Some(nvim_total)) = (totals.next(), totals.next()) else {
        return Err(BenchError::Desync {
            context: "the paired totals did not summarize into two segments".to_string(),
        });
    };
    let ratios: Vec<f64> = trials.iter().map(|t| t.ratio_p50).collect();
    Ok(EchoPathOutcome {
        gated_ratio_p50: median_of_trials(&ratios)?,
        trials,
        segments: summarize_segments(ECHO_LABELS, &pools),
        view_total,
        nvim_total,
        overhead,
        overhead_pace,
        unresolved,
        ambiguous_input_wakes,
        ambiguous_output_wakes,
        repeated_round_tags: SINGLE_ROUND_TAGS.into_iter().zip(repeats).collect(),
        multiplicity_flagged,
    })
}

/// Characters echoed per screen row by the pty floor control; one below
/// the grid width so a wrap can never move the observed cell.
const FLOOR_COLS: u16 = GRID_COLS - 1;

/// Measures this host's bare pty round trip: the harness's write to the
/// pty master, a raw-mode `cat` reading and writing the byte straight
/// back, and the harness's own parse of the echoed cell.
///
/// Both editors pay this floor on every keystroke, so it is what separates
/// "the work view does on its input path" from "what it costs to hand a
/// keystroke to any process at all on this host" -- the comparison the
/// `pty->key-decoded` stage has no in-process equivalent for.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if the control never settles or an
/// echoed byte never appears within `timeout`.
pub fn run_pty_floor(
    cwd: &Path,
    samples: usize,
    warmup: usize,
    timeout: Duration,
) -> Result<Distribution, BenchError> {
    let spec = SpawnSpec {
        program: std::path::PathBuf::from("sh"),
        // raw mode with the terminal's own echo off: the byte that comes
        // back is `cat`'s write, so the sample contains a real process
        // wakeup rather than the line discipline echoing in the kernel
        args: vec![
            std::ffi::OsString::from("-c"),
            std::ffi::OsString::from("stty raw -echo; exec cat"),
        ],
        env: vec![(
            std::ffi::OsString::from("TERM"),
            std::ffi::OsString::from("xterm-256color"),
        )],
        cwd: Some(cwd.to_path_buf()),
    };
    let mut session = BenchSession::spawn(&spec)?;
    if !session.settle(SettleBound {
        quiet: Duration::from_millis(200),
        deadline: Duration::from_secs(10),
    }) {
        return Err(BenchError::Desync {
            context: format!(
                "pty floor control never went quiet; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    let mut samples_us = Vec::with_capacity(samples + warmup);
    let mut at = crate::boundaries::CellPos { row: 0, col: 0 };
    for _ in 0..(samples + warmup) {
        if at.col >= FLOOR_COLS {
            session.send(b"\r\n")?;
            at.col = 0;
            at.row += 1;
        }
        if at.row >= GRID_ROWS - 1 {
            session.send(b"\x1b[2J\x1b[H")?;
            let home = crate::boundaries::CellPos { row: 0, col: 0 };
            if !session.wait_cell(home, " ", timeout) {
                return Err(BenchError::Desync {
                    context: "pty floor control never cleared its screen".to_string(),
                });
            }
            at = home;
        }
        let t0 = monotonic_nanos();
        session.send(b"x")?;
        if !session.wait_cell(at, "x", timeout) {
            return Err(BenchError::Desync {
                context: format!(
                    "pty floor control never echoed at ({}, {}); screen:\n{}",
                    at.row,
                    at.col,
                    session.screen_text()
                ),
            });
        }
        samples_us.push(delta_us(t0, monotonic_nanos()));
        at.col += 1;
    }
    session.shutdown();
    Distribution::from_samples(&samples_us, warmup)
}

/// Interval left between characterization writes.
///
/// Unpaced, the loop fills the FIFO far faster than the reader drains it,
/// and once it is full every remaining write fails immediately with
/// `EAGAIN` -- which is cheap, and nothing like what a tap on the
/// measured path pays. Observed on dev-linux: 100000 unpaced writes
/// delivered 39398 records at p50 0.27us, while paced writes delivered
/// all 20000 of them at p50 1.11us. A bar compared against the unpaced
/// number is a bar against a mostly-failed operation, so the
/// characterization pays for the pace.
pub const OVERHEAD_PACE: Duration = Duration::from_micros(20);

/// Writes the characterization issues. Paced, so this is also a duration:
/// 20000 at 20us is about half a second per row, spent inside the live
/// session rather than against an idle host.
const OVERHEAD_ITERATIONS: usize = 20_000;

/// Spins on the monotonic clock for `pace` instead of sleeping.
///
/// A sleeping pace measures every write on a thread that just woke, cold
/// and possibly on another CPU, which is the one thing the real tap sites
/// never are: they fire on a thread already running. Observed on
/// dev-linux, that artifact lands entirely in the tail -- p50 1.07us
/// either way, p99 6.8us sleeping.
fn spin_for(pace: Duration) {
    let until =
        monotonic_nanos().saturating_add(i64::try_from(pace.as_nanos()).unwrap_or(i64::MAX));
    while monotonic_nanos() < until {
        std::hint::spin_loop();
    }
}

/// Measures the tap operation's own cost with the identical code shape
/// the in-process tap sites run (one monotonic clock read, one record
/// format, one non-blocking FIFO write), left `pace` apart so every write
/// is one the reader actually receives. Callers pass [`OVERHEAD_PACE`];
/// the parameter exists so the delivery guard below can be exercised
/// against a pace that provably violates it.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if the FIFO cannot be opened for
/// writing, or if the reader received fewer records than were written --
/// a dropped write is a write whose cost the samples understate, so a
/// lossy characterization is reported rather than averaged in.
pub fn characterize_overhead(
    pipe: &TapPipe,
    iterations: usize,
    pace: Duration,
) -> Result<Distribution, BenchError> {
    let (dist, delivered) = characterize_once(pipe, iterations, pace)?;
    if delivered < iterations {
        return Err(BenchError::Desync {
            context: format!(
                "tap overhead characterization wrote {iterations} records but the reader received \
                 {delivered}; the dropped writes cost nothing and would understate the \
                 instrumentation the rows are about to measure through"
            ),
        });
    }
    Ok(dist)
}

/// How many times [`characterize_overhead_adaptive`] may back its pace off.
/// Each attempt doubles, so five attempts span 20us to 320us and bound the
/// characterization's wall time at roughly seven seconds.
const OVERHEAD_PACE_ATTEMPTS: u32 = 5;

/// Characterizes the tap's cost at the slowest pace the host needs, and
/// reports which pace that was.
///
/// [`OVERHEAD_PACE`] was tuned on dev-linux, where a 64 KiB pipe buffer and
/// the reader's drain cadence leave it comfortable. It is not portable: on
/// macOS the same pace loses roughly one write in a thousand (observed
/// 19965 and 19985 of 20000 across two runs at different host loads), which
/// the delivery guard correctly refuses -- leaving the row unrunnable on
/// that class rather than merely slow. Doubling until every write lands
/// self-tunes on any host, including ones this project has not seen, and
/// keeps the guard absolute rather than trading it for a per-OS constant
/// that would silently drift as pipe sizes and schedulers change.
///
/// The returned pace is reported by the row, so a host that needs an
/// unusual one is visible in the output instead of hidden inside a retry.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if full delivery is not achieved within
/// [`OVERHEAD_PACE_ATTEMPTS`], naming the best delivery seen -- a host that
/// cannot deliver every write at 320us apart has something wrong with it
/// that a slower pace will not fix, and the row must not report a number
/// measured through a lossy pipe.
pub fn characterize_overhead_adaptive(
    pipe: &TapPipe,
    iterations: usize,
    base_pace: Duration,
) -> Result<(Distribution, Duration), BenchError> {
    // a zero base would never escape by doubling
    let mut pace = base_pace.max(Duration::from_micros(1));
    let mut best = 0_usize;
    for _ in 0..OVERHEAD_PACE_ATTEMPTS {
        let (dist, delivered) = characterize_once(pipe, iterations, pace)?;
        if delivered >= iterations {
            return Ok((dist, pace));
        }
        best = best.max(delivered);
        pace = pace.saturating_mul(2);
    }
    Err(BenchError::Desync {
        context: format!(
            "tap overhead characterization never achieved full delivery: best {best} of \
             {iterations} records across {OVERHEAD_PACE_ATTEMPTS} attempts, up to {pace:?} \
             apart; a pipe this lossy would understate the instrumentation the rows measure \
             through"
        ),
    })
}

/// One characterization pass: writes `iterations` records `pace` apart and
/// reports the sample distribution alongside how many the reader actually
/// received. Makes no judgment about delivery -- the callers above differ
/// only in what they do about a shortfall.
fn characterize_once(
    pipe: &TapPipe,
    iterations: usize,
    pace: Duration,
) -> Result<(Distribution, usize), BenchError> {
    use std::io::Write;
    let path = pipe.path();
    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| BenchError::Desync {
        context: format!(
            "opening tap fifo {} for overhead bench: {e}",
            path.display()
        ),
    })?;
    let file = std::fs::File::from(fd);
    let _ = pipe.drain();
    let mut samples_us = Vec::with_capacity(iterations);
    for seq in 0..iterations {
        let start = Instant::now();
        let nanos = monotonic_nanos();
        let line = format!("O {seq} {nanos}\n");
        let _ = (&file).write(line.as_bytes());
        samples_us.push(start.elapsed().as_secs_f64() * 1_000_000.0);
        spin_for(pace);
    }
    // the reader polls on a sleep cadence rather than blocking, so the
    // last writes need a moment to land before the count means anything
    std::thread::sleep(Duration::from_millis(50));
    let delivered = pipe.drain().iter().filter(|r| r.tag == b'O').count();
    Ok((
        Distribution::from_samples(&samples_us, iterations / 10)?,
        delivered,
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn parse_record_round_trips_the_tap_line_shape() {
        assert_eq!(
            parse_record("W 42 123456789"),
            Some(TapRecord {
                tag: b'W',
                seq: 42,
                nanos: 123_456_789
            })
        );
        assert_eq!(parse_record("W 42"), None);
        assert_eq!(parse_record("W 42 9 extra"), None);
        assert_eq!(parse_record(""), None);
    }

    fn tap(tag: u8, seq: u64, nanos: i64) -> TapRecord {
        TapRecord { tag, seq, nanos }
    }

    #[test]
    fn pair_paint_takes_the_earliest_explaining_redraw() {
        let records = [tap(b'R', 1, 110), tap(b'R', 2, 130), tap(b'T', 1, 150)];
        assert_eq!(
            pair_paint(&records, 20, 100, tap(b'T', 1, 150)),
            PairingVerdict::Paired(tap(b'R', 1, 110))
        );
    }

    #[test]
    fn pair_paint_skips_a_straggler_paint_whose_redraw_predates_the_keypress() {
        // the straggler's own R sits in (since=20, t0=100), so the T at
        // 120 is a frame from before the sample; the paint at 200 with R
        // at 180 is the keypress's frame and must pair once offered as
        // the candidate
        let records = [
            tap(b'R', 1, 40),
            tap(b'T', 1, 120),
            tap(b'R', 2, 180),
            tap(b'T', 2, 200),
        ];
        assert_eq!(
            pair_paint(&records, 20, 100, tap(b'T', 1, 120)),
            PairingVerdict::Stray(tap(b'T', 2, 200))
        );
        assert_eq!(
            pair_paint(&records, 20, 100, tap(b'T', 2, 200)),
            PairingVerdict::Paired(tap(b'R', 2, 180))
        );
    }

    #[test]
    fn pair_paint_refuses_stray_for_a_paint_with_no_explaining_redraw_at_all() {
        // same shape as the straggler case minus its pre-keypress R: a
        // paint nothing explains must not be skipped even though a later
        // paired T exists, because that is the paints-without-redraw
        // product fault the desync abort exists to catch
        let records = [tap(b'T', 1, 120), tap(b'R', 2, 180), tap(b'T', 2, 200)];
        assert_eq!(
            pair_paint(&records, 20, 100, tap(b'T', 1, 120)),
            PairingVerdict::Pending
        );
    }

    #[test]
    fn pair_paint_refuses_stray_when_the_only_earlier_redraw_is_already_claimed() {
        // the R at 10 sits at or before the previous measured frame's
        // paint (since=20), so it explained an earlier sample, not this
        // candidate; counting it would make the straggler check vacuous
        // against the accumulated record log
        let records = [
            tap(b'R', 1, 10),
            tap(b'T', 1, 120),
            tap(b'R', 2, 180),
            tap(b'T', 2, 200),
        ];
        assert_eq!(
            pair_paint(&records, 20, 100, tap(b'T', 1, 120)),
            PairingVerdict::Pending
        );
    }

    #[test]
    fn pair_paint_stays_pending_until_a_redraw_or_a_later_paint_arrives() {
        let records = [tap(b'R', 1, 40), tap(b'T', 1, 120)];
        assert_eq!(
            pair_paint(&records, 20, 100, tap(b'T', 1, 120)),
            PairingVerdict::Pending
        );
    }

    #[test]
    fn verify_no_drops_accepts_contiguous_and_interleaved_tag_streams() {
        // W and R share the engine counter; T counts independently
        let records = [
            TapRecord {
                tag: b'W',
                seq: 0,
                nanos: 1,
            },
            TapRecord {
                tag: b'R',
                seq: 1,
                nanos: 2,
            },
            TapRecord {
                tag: b'T',
                seq: 0,
                nanos: 3,
            },
            TapRecord {
                tag: b'W',
                seq: 2,
                nanos: 4,
            },
            TapRecord {
                tag: b'T',
                seq: 1,
                nanos: 5,
            },
        ];
        assert!(verify_no_drops(&records).is_ok());
    }

    #[test]
    fn verify_no_drops_names_a_hole_in_a_stream() {
        let records = [
            TapRecord {
                tag: b'W',
                seq: 0,
                nanos: 1,
            },
            TapRecord {
                tag: b'W',
                seq: 2,
                nanos: 2,
            },
        ];
        let err = verify_no_drops(&records).unwrap_err();
        assert!(err.to_string().contains("between seq 0 and 2"), "{err}");
    }

    #[test]
    fn tag_chain_matches_tags_in_timestamp_order() {
        let records = [
            TapRecord {
                tag: b'U',
                seq: 0,
                nanos: 50,
            },
            TapRecord {
                tag: b'K',
                seq: 1,
                nanos: 20,
            },
            TapRecord {
                tag: b'S',
                seq: 2,
                nanos: 70,
            },
        ];
        let chain = tag_chain(&records, b"KUS", 10).unwrap();
        assert_eq!(
            chain.iter().map(|r| r.nanos).collect::<Vec<_>>(),
            vec![20, 50, 70]
        );
    }

    #[test]
    fn tag_chain_rejects_a_window_missing_a_tag() {
        let records = [TapRecord {
            tag: b'K',
            seq: 0,
            nanos: 20,
        }];
        assert!(tag_chain(&records, b"KU", 10).is_none());
    }

    #[test]
    fn tag_chain_ignores_records_before_the_window_start() {
        let records = [
            TapRecord {
                tag: b'K',
                seq: 0,
                nanos: 5,
            },
            TapRecord {
                tag: b'K',
                seq: 1,
                nanos: 30,
            },
        ];
        let chain = tag_chain(&records, b"K", 10).unwrap();
        assert_eq!(chain[0].nanos, 30);
    }

    /// Builds tap records from `(tag, nanos)` pairs; sequence numbers are
    /// irrelevant to chain walking and are left contiguous.
    fn records(pairs: &[(u8, i64)]) -> Vec<TapRecord> {
        pairs
            .iter()
            .enumerate()
            .map(|(seq, (tag, nanos))| TapRecord {
                tag: *tag,
                seq: seq as u64,
                nanos: *nanos,
            })
            .collect()
    }

    /// One clean echo round trip: both loop wakes present, in order.
    fn clean_round_trip() -> Vec<TapRecord> {
        records(&[
            (b'K', 10),
            (b'U', 20),
            (b'S', 30),
            (b'W', 40),
            (b'R', 50),
            (b'U', 60),
            (b'B', 70),
            (b'P', 71),
            (b'G', 72),
            (b'C', 74),
            (b'F', 80),
            (b'T', 90),
        ])
    }

    #[test]
    fn the_echo_chain_binds_each_loop_wake_to_its_own_side() {
        let chain = tag_chain(&clean_round_trip(), ECHO_CHAIN, 0).unwrap();
        assert_eq!(
            chain.iter().map(|r| r.nanos).collect::<Vec<_>>(),
            vec![10, 20, 30, 40, 50, 60, 70, 71, 72, 74, 80, 90]
        );
        assert_eq!(chain[1].nanos, 20, "input-side wake");
        assert_eq!(chain[5].nanos, 60, "output-side wake");
    }

    #[test]
    fn a_lost_input_wake_breaks_the_chain_instead_of_stealing_the_output_wake() {
        // without this, the output-side wake at 60 would fill the
        // input-side slot and the sample's stages would be silently
        // mispaired rather than dropped
        let mut window = clean_round_trip();
        window.retain(|r| !(r.tag == b'U' && r.nanos == 20));
        assert!(tag_chain(&window, ECHO_CHAIN, 0).is_none());
    }

    #[test]
    fn the_output_wake_is_bounded_below_by_the_redraw_it_follows() {
        // a previous sample's paint landing inside this sample's window
        // must not pull B/F/T ahead of the redraw that caused them
        let mut window = clean_round_trip();
        window.extend(records(&[
            (b'B', 40),
            (b'P', 41),
            (b'G', 42),
            (b'C', 43),
            (b'F', 44),
            (b'T', 46),
        ]));
        let chain = tag_chain(&window, ECHO_CHAIN, 0).unwrap();
        assert_eq!(chain[4].nanos, 50, "redraw parsed");
        assert_eq!(
            chain[6].nanos, 70,
            "draw start after the redraw, not before"
        );
    }

    #[test]
    fn a_second_redraw_round_does_not_extend_the_first_ones_stages() {
        let mut window = clean_round_trip();
        window.extend(records(&[
            (b'R', 100),
            (b'U', 110),
            (b'B', 120),
            (b'P', 121),
            (b'G', 122),
            (b'C', 124),
            (b'F', 130),
            (b'T', 140),
        ]));
        let chain = tag_chain(&window, ECHO_CHAIN, 0).unwrap();
        assert_eq!(
            chain[ECHO_CHAIN.len() - 1].nanos,
            90,
            "the first paint closes the chain"
        );
    }

    /// The tags a chain outcome saw more than once, in table order.
    fn repeated(outcome: &ChainOutcome) -> Vec<char> {
        SINGLE_ROUND_TAGS
            .iter()
            .zip(outcome.repeated_round_tags)
            .filter(|(_, flagged)| *flagged)
            .map(|(tag, _)| *tag as char)
            .collect()
    }

    #[test]
    fn a_spurious_redraw_round_ahead_of_the_real_one_is_flagged_not_reported_clean() {
        // the whole reason the multiplicity counters exist: this window
        // resolves, both wake brackets read clean, and every stage between
        // the RPC write and the terminal write silently describes a round
        // the keystroke did not cause
        let mut window = clean_round_trip();
        window.extend(records(&[
            (b'R', 42),
            (b'U', 43),
            (b'B', 44),
            (b'P', 45),
            (b'G', 46),
            (b'C', 47),
            (b'F', 48),
            (b'T', 49),
        ]));
        let mut pools: Vec<Vec<f64>> = vec![Vec::new(); ECHO_LABELS.len()];
        let outcome = accumulate_chain(&window, 0, 100, &mut pools);
        assert!(outcome.resolved, "the chain still resolves");
        assert!(
            !outcome.ambiguous_input_wakes,
            "both wake counters read clean"
        );
        assert!(!outcome.ambiguous_output_wakes);
        assert_eq!(
            repeated(&outcome),
            vec!['R', 'B', 'P', 'G', 'C', 'F', 'T'],
            "every tag the spurious round duplicated is named"
        );
        // and the damage it does, pinned so the counter stays tied to the
        // failure it exists to catch rather than to its own shape
        assert!(
            (pools[4][0] - 0.002).abs() < 1e-9,
            "rpc-written->redraw-parsed collapsed onto the spurious round"
        );
        assert!(
            (pools[ECHO_LABELS.len() - 1][0] - 0.051).abs() < 1e-9,
            "and the collapsed time parked in the closing stage"
        );
    }

    #[test]
    fn a_trailing_redraw_round_is_flagged_even_though_the_stages_are_intact() {
        // a second round after the chain closes leaves every stage correct
        // but still means the window held two paints, so the counter fires
        // here too: it reports multiplicity, it does not judge harm
        let mut window = clean_round_trip();
        window.extend(records(&[
            (b'R', 100),
            (b'U', 110),
            (b'B', 120),
            (b'P', 121),
            (b'G', 122),
            (b'C', 124),
            (b'F', 130),
            (b'T', 140),
        ]));
        let mut pools: Vec<Vec<f64>> = vec![Vec::new(); ECHO_LABELS.len()];
        let outcome = accumulate_chain(&window, 0, 150, &mut pools);
        assert!(outcome.resolved);
        assert_eq!(repeated(&outcome), vec!['R', 'B', 'P', 'G', 'C', 'F', 'T']);
        assert!((pools[4][0] - 0.010).abs() < 1e-9, "stages unharmed");
    }

    #[test]
    fn every_single_round_tag_is_a_chain_tag_and_no_wake_is_one() {
        for tag in SINGLE_ROUND_TAGS {
            assert!(
                ECHO_CHAIN.contains(&tag),
                "{} is counted for multiplicity but the chain never walks it",
                tag as char
            );
        }
        assert!(
            !SINGLE_ROUND_TAGS.contains(&b'U'),
            "the loop wake appears twice by design and is bracket-guarded instead"
        );
        for tag in ECHO_CHAIN {
            assert!(
                *tag == b'U' || SINGLE_ROUND_TAGS.contains(tag),
                "chain tag {} has no multiplicity guard of either kind",
                *tag as char
            );
        }
    }

    #[test]
    fn a_resolved_chain_fills_every_stage_and_closes_on_the_observation() {
        let mut pools: Vec<Vec<f64>> = vec![Vec::new(); ECHO_LABELS.len()];
        let outcome = accumulate_chain(&clean_round_trip(), 0, 100, &mut pools);
        assert!(outcome.resolved);
        assert!(!outcome.ambiguous_input_wakes);
        assert!(!outcome.ambiguous_output_wakes);
        assert!(
            !outcome.repeated_round_tags.contains(&true),
            "a single clean round trip repeats no tag"
        );
        let p50s: Vec<f64> = pools.iter().map(|p| p[0]).collect();
        assert_eq!(p50s.len(), ECHO_LABELS.len());
        // every stage is 10ns wide except the closing one, which runs from
        // the last tap to the harness's observation at 100
        assert_eq!(p50s[0], 0.010);
        assert_eq!(p50s[ECHO_LABELS.len() - 1], 0.010);
        let total: f64 = p50s.iter().sum();
        assert!((total - 0.100).abs() < 1e-9, "stages must tile the window");
    }

    #[test]
    fn an_unresolved_chain_contributes_to_no_stage() {
        let mut window = clean_round_trip();
        window.retain(|r| r.tag != b'F');
        let mut pools: Vec<Vec<f64>> = vec![Vec::new(); ECHO_LABELS.len()];
        let outcome = accumulate_chain(&window, 0, 100, &mut pools);
        assert!(!outcome.resolved);
        assert!(
            pools.iter().all(Vec::is_empty),
            "a broken chain must leave every pool untouched, not partially filled"
        );
    }

    #[test]
    fn an_extra_wake_inside_a_bracket_is_counted_not_hidden() {
        let mut window = clean_round_trip();
        window.extend(records(&[(b'U', 25), (b'U', 65)]));
        let mut pools: Vec<Vec<f64>> = vec![Vec::new(); ECHO_LABELS.len()];
        let outcome = accumulate_chain(&window, 0, 100, &mut pools);
        assert!(outcome.resolved);
        assert!(outcome.ambiguous_input_wakes);
        assert!(outcome.ambiguous_output_wakes);
        // the split moved but the bracket totals did not: the stages still
        // tile the window
        let total: f64 = pools.iter().map(|p| p[0]).sum();
        assert!((total - 0.100).abs() < 1e-9);
    }

    #[test]
    fn every_chain_tag_has_its_own_stage_label() {
        assert_eq!(ECHO_LABELS.len(), ECHO_CHAIN.len() + 1);
        assert_eq!(ECHO_CHAIN[INPUT_WAKE_BRACKET_END], b'S');
        assert_eq!(ECHO_CHAIN[OUTPUT_WAKE_BRACKET_START], b'R');
        assert_eq!(ECHO_CHAIN[OUTPUT_WAKE_BRACKET_END], b'B');
    }

    #[test]
    fn every_chain_tag_is_one_a_drop_check_covers() {
        // a tag no origin stream claims is worse than undetectable: it
        // still consumes its crate's sequence numbers, so the drop check
        // reads the numbers it skipped as a pipe overflow and fails every
        // run. Observed when the key-read tag was added to the chain
        // before this table -- "view-tui tap records dropped between seq
        // 48 and 50" on a run that dropped nothing
        for tag in ECHO_CHAIN.iter().chain(INPUT_CHAIN) {
            assert!(
                TAG_ORIGINS.iter().any(|(_, tags)| tags.contains(tag)),
                "chain tag {} belongs to no checked origin stream",
                *tag as char
            );
        }
    }

    #[test]
    fn the_input_chain_has_one_label_per_interval_it_resolves() {
        assert_eq!(INPUT_LABELS.len(), INPUT_CHAIN.len() + 1);
    }

    /// A directory inside the workspace's own build tree for tests that
    /// need a real filesystem object. Never `std::env::temp_dir()`: these
    /// names are predictable, and a checkout's build tree is the one
    /// directory the test already owns.
    fn scratch_root() -> std::path::PathBuf {
        let root = crates_dir()
            .parent()
            .expect("crates/ sits under the workspace root")
            .join("target")
            .join("view-bench-scratch");
        std::fs::create_dir_all(&root).expect("failed to create the scratch root");
        root
    }

    #[test]
    fn an_undelivered_characterization_is_refused_rather_than_reported() {
        // unpaced, the FIFO fills faster than the reader drains it and the
        // rest of the writes fail immediately with EAGAIN. A failed write
        // costs nothing, so its samples describe an operation the tap
        // sites never perform -- which is how the 5us bar came to be
        // compared against a p99 of 0.53us that no tap ever paid
        let path = scratch_root().join(format!("unpaced-{}.fifo", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let pipe = TapPipe::create(&path).expect("tap pipe");
        let err = characterize_overhead(&pipe, 100_000, Duration::ZERO)
            .expect_err("a saturated FIFO must not report a percentile");
        assert!(
            matches!(err, BenchError::Desync { .. }),
            "a lossy characterization is a harness fault, not a latency reading: {err:?}"
        );
        let message = err.to_string();
        assert!(
            message.contains("the reader received"),
            "the refusal must name the delivery shortfall, got: {message}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_pace_too_fast_for_the_host_is_escalated_until_every_write_lands() {
        // the same zero pace the guard test above proves is lossy: the
        // adaptive caller must reach full delivery rather than refuse, and
        // must report a pace slower than the one it was handed, so a host
        // needing an unusual pace shows up in the row's output
        let path = scratch_root().join(format!("adaptive-{}.fifo", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let pipe = TapPipe::create(&path).expect("tap pipe");
        let (dist, pace) = characterize_overhead_adaptive(&pipe, 2_000, Duration::ZERO)
            .expect("doubling the pace must reach full delivery");
        assert_eq!(dist.len(), 2_000 - 2_000 / 10);
        assert!(
            pace > Duration::ZERO,
            "the escalated pace must be reported, got {pace:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_paced_characterization_delivers_every_write() {
        let path = scratch_root().join(format!("paced-{}.fifo", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let pipe = TapPipe::create(&path).expect("tap pipe");
        let dist = characterize_overhead(&pipe, 2_000, OVERHEAD_PACE)
            .expect("a paced characterization delivers every write");
        assert_eq!(dist.len(), 2_000 - 2_000 / 10);
        let _ = std::fs::remove_file(&path);
    }

    /// The workspace's `crates/` directory, reached from this crate's own
    /// manifest so the walk below does not depend on the cwd a test runner
    /// happens to choose.
    fn crates_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("view-bench sits under crates/")
            .to_path_buf()
    }

    /// Every `.rs` file under `crates/*/src`, excluding this crate: the
    /// harness is the tap's consumer and names the tags freely, while the
    /// measured crates are the ones a stray call site would cost.
    fn measured_sources() -> Vec<std::path::PathBuf> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        let Ok(crates) = std::fs::read_dir(crates_dir()) else {
            return out;
        };
        for entry in crates.flatten() {
            if entry.file_name() == "view-bench" {
                continue;
            }
            walk(&entry.path().join("src"), &mut out);
        }
        out
    }

    /// The attribute that keeps a line out of a default build.
    const TAPS_CFG: &str = "#[cfg(feature = \"bench-taps\")]";

    /// Indentation of `line` in spaces.
    fn indent_of(line: &str) -> usize {
        line.len() - line.trim_start().len()
    }

    /// Index of the nearest code line above `index`, skipping blanks and
    /// comment-only lines so a doc comment between an attribute and the
    /// item it guards does not read as an unguarded item.
    fn code_line_above(lines: &[&str], index: usize) -> Option<usize> {
        lines[..index].iter().enumerate().rev().find_map(|(i, l)| {
            let t = l.trim();
            (!t.is_empty() && !t.starts_with("//")).then_some(i)
        })
    }

    /// Whether the item at `index` carries the taps attribute directly.
    fn attributed(lines: &[&str], index: usize) -> bool {
        code_line_above(lines, index).is_some_and(|i| lines[i].trim() == TAPS_CFG)
    }

    /// Whether line `index` is compiled out of a default build: either it
    /// carries the attribute itself, or some block enclosing it does.
    fn cfg_gated(lines: &[&str], index: usize) -> bool {
        let mut at = index;
        loop {
            if attributed(lines, at) {
                return true;
            }
            let depth = indent_of(lines[at]);
            let Some(outer) = lines[..at]
                .iter()
                .enumerate()
                .rev()
                .find(|(_, l)| !l.trim().is_empty() && indent_of(l) < depth)
                .map(|(i, _)| i)
            else {
                return false;
            };
            at = outer;
        }
    }

    #[test]
    fn no_tap_reaches_a_default_build() {
        // Each tap module is itself `#[cfg(feature = "bench-taps")]`, so an
        // unguarded call site fails to compile rather than quietly adding
        // cost to the measured path. That moat is the strongest one
        // available, but nothing in the tree re-checks the two facts it
        // rests on: that the modules stay gated, and that the feature stays
        // out of every default set. Both are checked here, together with
        // the call sites themselves, so the measured build's freedom from
        // tap work survives whoever edits next.
        let mut unguarded = Vec::new();
        let mut ungated_modules = Vec::new();
        let mut call_sites = 0;
        let mut modules = 0;
        for path in measured_sources() {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            for index in 0..lines.len() {
                let trimmed = lines[index].trim();
                if trimmed.ends_with("mod tap;") {
                    modules += 1;
                    if !attributed(&lines, index) {
                        ungated_modules.push(format!("{}: {trimmed}", path.display()));
                    }
                }
                if !trimmed.contains("tap::tap(") {
                    continue;
                }
                call_sites += 1;
                if !cfg_gated(&lines, index) {
                    unguarded.push(format!("{}: {trimmed}", path.display()));
                }
            }
        }
        assert!(
            call_sites > 0 && modules > 0,
            "found {call_sites} call sites and {modules} tap modules; the walk is looking in the \
             wrong place"
        );
        assert!(
            unguarded.is_empty(),
            "tap call sites reachable from a default build:\n{}",
            unguarded.join("\n")
        );
        assert!(
            ungated_modules.is_empty(),
            "tap modules that a default build would compile (the compiler stops being the moat \
             the moment one of these is ungated):\n{}",
            ungated_modules.join("\n")
        );

        let mut enabled_by_default = Vec::new();
        let crates = std::fs::read_dir(crates_dir()).expect("crates/ is readable");
        for entry in crates.flatten() {
            let manifest = entry.path().join("Cargo.toml");
            let Ok(text) = std::fs::read_to_string(&manifest) else {
                continue;
            };
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("default") && trimmed.contains("bench-taps") {
                    enabled_by_default.push(format!("{}: {trimmed}", manifest.display()));
                }
            }
        }
        assert!(
            enabled_by_default.is_empty(),
            "bench-taps is reachable from a default feature set:\n{}",
            enabled_by_default.join("\n")
        );
    }

    #[test]
    fn every_tag_the_measured_crates_declare_is_registered_to_its_origin() {
        // a tag absent from TAG_ORIGINS still burns its crate's sequence
        // numbers, so verify_no_drops reads the skipped numbers as a pipe
        // overflow and every run fails -- even runs whose chain never
        // walks the new tag. Registration is not bookkeeping, it is what
        // keeps the drop check honest, so the declarations are the source
        // of truth and this test reads them rather than trusting the table
        let mut unregistered = Vec::new();
        let mut found = 0;
        for path in measured_sources() {
            let Some(origin) = TAG_ORIGINS
                .iter()
                .find(|(name, _)| path.components().any(|c| c.as_os_str() == *name))
            else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                let trimmed = line.trim();
                if !trimmed.starts_with("pub const TAG_") {
                    continue;
                }
                let Some(tag) = trimmed
                    .split_once("= b'")
                    .and_then(|(_, rest)| rest.as_bytes().first().copied())
                else {
                    continue;
                };
                found += 1;
                if !origin.1.contains(&tag) {
                    unregistered.push(format!("{}: {trimmed}", path.display()));
                }
            }
        }
        assert!(
            found
                >= TAG_ORIGINS
                    .iter()
                    .map(|(_, tags)| tags.len())
                    .sum::<usize>(),
            "found {found} tag declarations for a table naming more; the walk is looking in the \
             wrong place"
        );
        assert!(
            unregistered.is_empty(),
            "tap tags declared but missing from TAG_ORIGINS (every run through them will report a \
             phantom pipe overflow):\n{}",
            unregistered.join("\n")
        );
    }
}
