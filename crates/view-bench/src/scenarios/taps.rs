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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::sampling::Distribution;
use crate::scenarios::Protocol;
use crate::session::{BenchSession, SpawnSpec};
use crate::BenchError;

/// Current `CLOCK_MONOTONIC` in nanoseconds, the same clock and formula
/// the tap sites use.
#[must_use]
pub fn monotonic_nanos() -> i64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    now.tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(now.tv_nsec)
}

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

/// Verifies each crate's tap sequence stream is contiguous. The engine's
/// two tags share one counter and the tui's tags share their own, so the
/// streams are checked per origin.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] naming the missing span when records
/// were dropped (a full pipe under a non-blocking tap write).
pub fn verify_no_drops(records: &[TapRecord]) -> Result<(), BenchError> {
    for (name, tags) in [
        ("view-engine", b"WRS".as_slice()),
        ("view-tui", b"TKUBF".as_slice()),
    ] {
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

/// The harness end of the tap channel: a FIFO plus a reader thread
/// accumulating parsed records.
pub struct TapPipe {
    records: Arc<Mutex<Vec<TapRecord>>>,
    stop: Arc<AtomicBool>,
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
        rustix::fs::mknodat(
            rustix::fs::CWD,
            path,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            0,
        )
        .map_err(|e| BenchError::Desync {
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
        Ok(Self { records, stop })
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
    if !session.settle(Duration::from_secs(2), settle_deadline) {
        return Err(BenchError::Desync {
            context: format!(
                "startup never went quiet; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    session.send(b"i")?;
    if !session.settle(Duration::from_secs(2), settle_deadline) {
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

/// Measures the key-to-RPC-written path: per sample, one harness-side
/// timestamp immediately before the key byte is written to the pty
/// master, matched against the next `W` tap the instrumented view emits.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] on tap loss, missing responses, or
/// session failures.
pub fn run_input_path(
    spec: &SpawnSpec,
    pipe: &TapPipe,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<TapsOutcome, BenchError> {
    let mut session = prepare(spec, pipe, settle_deadline)?;
    let mut trial_distributions = Vec::with_capacity(protocol.trials);
    let mut all_records = Vec::new();
    let labels = [
        "pty->key-decoded",
        "key-decoded->loop-wake",
        "loop-wake->rpc-handoff",
        "rpc-handoff->rpc-written",
    ];
    let mut pools: Vec<Vec<f64>> = vec![Vec::new(); labels.len()];
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
            #[allow(clippy::cast_precision_loss)]
            deltas_us.push((record.nanos - t0) as f64 / 1000.0);
            if index >= protocol.warmup {
                let window = pipe.records_between(t0, record.nanos);
                if let Some(chain) = tag_chain(&window, b"KUS", t0) {
                    let mut prev = t0;
                    for (pool, hit) in pools.iter_mut().zip(&chain) {
                        pool.push(delta_us(prev, hit.nanos));
                        prev = hit.nanos;
                    }
                    pools[chain.len()].push(delta_us(prev, record.nanos));
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
        segments: summarize_segments(&labels, &pools),
    })
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
    let mut trial_distributions = Vec::with_capacity(protocol.trials);
    let mut all_records = Vec::new();
    let labels = [
        "redraw-parsed->loop-wake",
        "loop-wake->draw-start",
        "draw-start->flush-start",
        "flush-start->term-written",
    ];
    let mut pools: Vec<Vec<f64>> = vec![Vec::new(); labels.len()];
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
            // small grace so the R record (written by a different thread
            // than the T) is guaranteed to have crossed the pipe before
            // the pairing scan below
            std::thread::sleep(Duration::from_millis(1));
            let window = pipe.drain();
            all_records.extend(window.iter().copied());
            let Some(parsed) = all_records
                .iter()
                .filter(|r| r.tag == b'R' && r.nanos >= t0 && r.nanos <= paint.nanos)
                .min_by_key(|r| r.nanos)
            else {
                return Err(BenchError::Desync {
                    context: "a paint arrived with no parsed redraw between the keypress \
                              and the terminal write"
                        .to_string(),
                });
            };
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
                    pools[chain.len()].push(delta_us(prev, paint.nanos));
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
    })
}

/// Measures the tap operation's own cost with the identical code shape
/// the in-process tap sites run (one monotonic clock read, one record
/// format, one non-blocking FIFO write), against a drained FIFO.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if the FIFO cannot be opened for
/// writing.
pub fn characterize_overhead(
    path: &std::path::Path,
    iterations: usize,
) -> Result<Distribution, BenchError> {
    use std::io::Write;
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
    let mut samples_us = Vec::with_capacity(iterations);
    for seq in 0..iterations {
        let start = Instant::now();
        let nanos = monotonic_nanos();
        let line = format!("O {seq} {nanos}\n");
        let _ = (&file).write(line.as_bytes());
        samples_us.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }
    Distribution::from_samples(&samples_us, iterations / 10)
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

    #[test]
    fn monotonic_nanos_is_monotone() {
        let a = monotonic_nanos();
        let b = monotonic_nanos();
        assert!(b >= a);
    }
}
