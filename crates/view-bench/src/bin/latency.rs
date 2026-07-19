//! `latency <label> <path-to-binary>`: measures keypress-to-paint latency of
//! one editor binary running in a real pty, one character at a time.
//!
//! `task bench-latency` invokes this binary twice, once for `view` and once
//! for `nvim`, so a single run only ever measures one target. Pairing the
//! two runs into the comparison table happens across invocations via a
//! timestamped scratch file in `target/`; a prior entry older than
//! [`STALENESS_BOUND_MS`] is treated as unrelated leftover data rather than
//! silently paired, and the paired table always prints `view` before
//! `nvim` regardless of which one ran first. See [`report`] and
//! [`decide_pairing`].

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use view_oracle::PtySession;

/// Keystroke-to-paint samples collected per target, matching the brief's
/// measurement protocol.
const SAMPLE_COUNT: usize = 200;
/// Fixed settle time before driving input; this harness has no
/// deterministic redraw-settled signal to wait on instead.
const READY_WAIT: Duration = Duration::from_secs(2);
/// Gap between samples so one measurement's tail doesn't bleed into the
/// next keystroke's paint.
const INTER_SAMPLE_SLEEP: Duration = Duration::from_millis(10);
/// Upper bound on how long one sample waits for its character to appear
/// before the run is declared desynced rather than silently skewing the
/// percentiles with a missing sample.
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(2);
/// The character typed for every sample; chosen because it is not a Vim
/// command-mode digraph or motion trigger on its own in insert mode.
const SAMPLE_CHAR: u8 = b'x';

/// Sorted elapsed-time samples in milliseconds, ready for percentile
/// extraction.
struct Samples(Vec<f64>);

impl Samples {
    fn from_durations(mut raw: Vec<Duration>) -> Self {
        raw.sort();
        Self(
            raw.iter()
                .map(Duration::as_secs_f64)
                .map(|s| s * 1000.0)
                .collect(),
        )
    }

    /// Nearest-rank percentile for informational reporting; a simple rank
    /// is sufficient without interpolation.
    fn percentile(&self, pct: f64) -> f64 {
        let len = self.0.len();
        if len == 0 {
            return 0.0;
        }
        let idx = ((pct / 100.0) * (len - 1) as f64).round() as usize;
        self.0[idx.min(len - 1)]
    }

    fn max(&self) -> f64 {
        self.0.last().copied().unwrap_or(0.0)
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

/// One target's measured stats, the unit both printed and staged across
/// invocations for pairing.
#[derive(Clone, Debug, PartialEq)]
struct Stats {
    label: String,
    p50_ms: f64,
    p99_ms: f64,
    max_ms: f64,
    samples: usize,
}

impl Stats {
    fn from_samples(label: &str, samples: &Samples) -> Self {
        Self {
            label: label.to_string(),
            p50_ms: samples.percentile(50.0),
            p99_ms: samples.percentile(99.0),
            max_ms: samples.max(),
            samples: samples.len(),
        }
    }

    /// Plain-text scratch encoding: one line, whitespace separated. Kept
    /// intentionally dependency-free (no serde) for a bin this small.
    fn to_line(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.label, self.p50_ms, self.p99_ms, self.max_ms, self.samples
        )
    }

    fn from_line(line: &str) -> Option<Self> {
        let mut parts = line.split_whitespace();
        Some(Self {
            label: parts.next()?.to_string(),
            p50_ms: parts.next()?.parse().ok()?,
            p99_ms: parts.next()?.parse().ok()?,
            max_ms: parts.next()?.parse().ok()?,
            samples: parts.next()?.parse().ok()?,
        })
    }
}

/// How long a staged scratch entry may sit before it is treated as leftover
/// data from an unrelated invocation rather than the other half of the
/// current `task bench-latency` pair. The two paired invocations run
/// seconds apart, so 60s is generous enough to never reject a real pair
/// while still catching a stray solo run left over from manual debugging.
const STALENESS_BOUND_MS: u64 = 60_000;

/// Current wall-clock time in epoch milliseconds, used to stamp scratch
/// entries on write and check their freshness on read.
fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A [`Stats`] line staged in the pairing scratch file, stamped with the
/// wall-clock time it was written so a stale entry can be detected on read
/// instead of silently paired with an unrelated run.
struct ScratchEntry {
    stats: Stats,
    written_at_ms: u64,
}

impl ScratchEntry {
    fn now(stats: Stats) -> Self {
        Self {
            written_at_ms: now_epoch_ms(),
            stats,
        }
    }

    /// Plain-text scratch encoding: timestamp followed by the stats line,
    /// whitespace separated. Kept dependency-free (no serde) for a bin this
    /// small.
    fn to_line(&self) -> String {
        format!("{} {}", self.written_at_ms, self.stats.to_line())
    }

    fn from_line(line: &str) -> Option<Self> {
        let (timestamp, rest) = line.split_once(char::is_whitespace)?;
        Some(Self {
            written_at_ms: timestamp.parse().ok()?,
            stats: Stats::from_line(rest)?,
        })
    }
}

/// Why the current run stands alone rather than completing a pair.
#[derive(Debug, PartialEq)]
enum SoloReason {
    /// No scratch entry was staged yet; this is genuinely the first half.
    NoPriorEntry,
    /// The prior entry belongs to a rerun of the same target; it is
    /// overwritten rather than paired with itself.
    SameLabel,
    /// The prior entry is older than [`STALENESS_BOUND_MS`], so pairing it
    /// with a fresh run would silently mix data from unrelated invocations.
    Stale { age_ms: u64 },
    /// The prior entry's timestamp is ahead of the current clock reading.
    /// The wall clock stepped backwards between invocations, so the entry's
    /// true age is unknowable and pairing it cannot be trusted.
    FutureTimestamp { skew_ms: u64 },
}

/// Result of attempting to pair the current run against a previously staged
/// [`ScratchEntry`]; pure so the pairing rules are unit-testable without
/// touching the filesystem or the clock.
#[derive(Debug, PartialEq)]
enum PairDecision {
    Solo(SoloReason),
    /// Both halves of a completed pair, canonically ordered `view` first.
    Paired(Stats, Stats),
}

/// Orders two paired stats with `view` first regardless of which half was
/// staged first, so the printed table's row order never depends on which
/// binary happened to finish first under `task bench-latency`.
fn order_view_first(a: Stats, b: Stats) -> (Stats, Stats) {
    if b.label == "view" && a.label != "view" {
        (b, a)
    } else {
        (a, b)
    }
}

/// Decides whether `current` completes a pair with `prior`, staying pure
/// (no I/O, an injected clock reading) so every branch is directly
/// unit-testable: a fresh different-label prior pairs; a same-label prior
/// is an overwrite, not a pair; a prior older than [`STALENESS_BOUND_MS`]
/// is discarded rather than silently pairing stale data with a fresh run.
fn decide_pairing(prior: Option<ScratchEntry>, current: &Stats, now_ms: u64) -> PairDecision {
    let Some(prior) = prior else {
        return PairDecision::Solo(SoloReason::NoPriorEntry);
    };
    if prior.stats.label == current.label {
        return PairDecision::Solo(SoloReason::SameLabel);
    }
    // a saturating age would read a future-stamped entry as freshly written
    // (age 0) and pair it; reject it instead, since a backwards clock step
    // makes the entry's true age unknowable
    if prior.written_at_ms > now_ms {
        return PairDecision::Solo(SoloReason::FutureTimestamp {
            skew_ms: prior.written_at_ms - now_ms,
        });
    }
    let age_ms = now_ms - prior.written_at_ms;
    if age_ms > STALENESS_BOUND_MS {
        return PairDecision::Solo(SoloReason::Stale { age_ms });
    }
    let (view, nvim) = order_view_first(prior.stats, current.clone());
    PairDecision::Paired(view, nvim)
}

/// A target process running inside a real pty, with everything needed to
/// drive it and observe its screen. Delegates the pty-level mechanics
/// (spawn, write, screen text, bounded shutdown) to `view-oracle`'s shared
/// [`PtySession`] so this bench and the oracle's own tests drive
/// byte-identical sessions, keeping only the bench-specific concerns
/// [`PtySession`] deliberately doesn't know about: an isolated scratch
/// file, isolated `XDG_*_HOME` variables, and a sample-character
/// occurrence count.
struct PtyTarget {
    session: PtySession,
    scratch: PathBuf,
    isolated_home: PathBuf,
}

/// Resolves `bin_path` to an absolute path when it names an existing file,
/// leaving bare command names (e.g. `nvim`) untouched.
///
/// The underlying pty spawn only treats a relative path as cwd-relative
/// when it explicitly starts with `./` or `../`; a plain
/// `target/release/view` argument falls through to a PATH search and fails
/// to spawn even though the file exists relative to the current directory.
/// Canonicalizing first sidesteps that without requiring every caller to
/// spell the `./` prefix.
fn resolve_bin_path(bin_path: &str) -> String {
    std::fs::canonicalize(bin_path)
        .map(|abs| abs.to_string_lossy().into_owned())
        .unwrap_or_else(|_| bin_path.to_string())
}

impl PtyTarget {
    /// Opens a 24x80 pty (via [`PtySession::spawn`]) and spawns `bin_path`
    /// against a scratch file, with the host's real editor config isolated
    /// out of the way.
    ///
    /// Without a file argument, both `view` and `nvim` fall back to
    /// whatever startup UI the host's own config wires up (a dashboard, a
    /// file-explorer sidebar); observed directly on this host, where the
    /// user's `nvim` config opens `nvim-tree` and a lazy.nvim dashboard on
    /// a bare launch, so the sample character never lands where the
    /// harness expects it. A scratch file argument plus isolated
    /// `XDG_*_HOME` variables (the same fix `view-oracle`'s pty smoke tests
    /// use) guarantees a plain buffer regardless of the host's config.
    ///
    /// `PtySession::spawn` takes a bare `cmd`/`args` pair with no env-var
    /// hook, so the isolation is expressed by wrapping the real command in
    /// coreutils `env NAME=VALUE... cmd args...` rather than reaching past
    /// the shared session type for its lower-level configured-command
    /// entry point.
    fn spawn(bin_path: &str) -> Result<Self> {
        let pid = std::process::id();
        let scratch = std::env::temp_dir().join(format!("view-bench-latency-{pid}.txt"));
        let isolated_home = std::env::temp_dir().join(format!("view-bench-latency-home-{pid}"));
        std::fs::create_dir_all(&isolated_home)
            .context("failed to create isolated XDG home for bench target")?;

        let mut env_args: Vec<String> = [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ]
        .into_iter()
        .map(|var| {
            format!(
                "{var}={}",
                isolated_home.join(var.to_lowercase()).to_string_lossy()
            )
        })
        .collect();
        env_args.push(resolve_bin_path(bin_path));
        env_args.push(scratch.to_string_lossy().into_owned());
        let arg_refs: Vec<&str> = env_args.iter().map(String::as_str).collect();

        let session = PtySession::spawn("env", &arg_refs, 80, 24)
            .with_context(|| format!("failed to spawn {bin_path} inside a pty"))?;

        Ok(Self {
            session,
            scratch,
            isolated_home,
        })
    }

    /// Pulls every chunk already buffered from the pty into the screen
    /// state without inspecting it, so a stale count from before the
    /// settle wait never pollutes the baseline the sampling loop starts
    /// from.
    fn drain(&mut self) {
        let _ = self.session.screen();
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.session.send(bytes).context("failed to write to pty")
    }

    fn count_char(&mut self, target: u8) -> usize {
        self.session
            .screen()
            .bytes()
            .filter(|&b| b == target)
            .count()
    }

    /// Blocks (up to `timeout`) until at least `expected` occurrences of
    /// `target` are visible on screen, returning whether that happened.
    ///
    /// A tight spin (no sleep between polls) rather than a fixed-interval
    /// poll: [`PtySession::screen`] has no lower-level channel-blocking
    /// entry point exposed to a caller outside the oracle crate, and this
    /// bench measures sub-millisecond keypress-to-paint latency, well below
    /// the OS scheduler's sleep-wakeup granularity -- a periodic sleep
    /// there was confirmed (by running the measurement) to inject its own
    /// poll interval directly into every sample, flattening view and nvim
    /// to indistinguishable ~20ms readings instead of the real sub-2ms
    /// figures.
    fn wait_for_count(&mut self, target: u8, expected: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.count_char(target) >= expected {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::hint::spin_loop();
        }
    }

    /// Best-effort shutdown: ask nicely, then let
    /// [`PtySession::wait_for_exit`] kill the child if it hasn't exited
    /// quickly. A hung measurement target must never hang the bench task
    /// itself.
    fn shutdown(&mut self) {
        let _ = self.write(b"\x1b:qa!\r");
        let _ = self.session.wait_for_exit(Duration::from_millis(200));
    }
}

impl Drop for PtyTarget {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.scratch);
        let _ = std::fs::remove_dir_all(&self.isolated_home);
    }
}

/// Runs the full sampling protocol against `bin_path`: settle, enter insert
/// mode, then `SAMPLE_COUNT` keypress-to-paint measurements.
fn measure(bin_path: &str) -> Result<Samples> {
    let mut target = PtyTarget::spawn(bin_path)?;

    std::thread::sleep(READY_WAIT);
    target.drain();
    target.write(b"i")?;
    // let insert-mode entry settle before the baseline count is taken, or a
    // slow-starting target's own redraw could be mistaken for a sample
    std::thread::sleep(Duration::from_millis(200));
    target.drain();

    let mut elapsed = Vec::with_capacity(SAMPLE_COUNT);
    let mut expected = target.count_char(SAMPLE_CHAR);
    for i in 0..SAMPLE_COUNT {
        let start = Instant::now();
        target.write(&[SAMPLE_CHAR])?;
        expected += 1;
        if !target.wait_for_count(SAMPLE_CHAR, expected, SAMPLE_TIMEOUT) {
            target.shutdown();
            bail!(
                "sample {i} of {SAMPLE_COUNT} never appeared on screen for {bin_path} \
                 (harness desync, not a real latency reading); last screen:\n{}",
                target.session.screen()
            );
        }
        elapsed.push(start.elapsed());
        std::thread::sleep(INTER_SAMPLE_SLEEP);
    }

    target.shutdown();
    Ok(Samples::from_durations(elapsed))
}

/// Path to the cross-invocation pairing scratch file, resolved from this
/// crate's manifest dir so it does not depend on the caller's cwd.
fn scratch_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("target");
    path.push(".view-bench-latency-scratch.txt");
    path
}

fn print_header() {
    println!("target  p50_ms  p99_ms  max_ms  samples");
}

fn print_row(stats: &Stats) {
    println!(
        "{:<7} {:>7.2} {:>7.2} {:>7.2} {:>8}",
        stats.label, stats.p50_ms, stats.p99_ms, stats.max_ms, stats.samples
    );
}

/// Prints the `view/nvim` ratio row: each column is `view`'s measured time
/// divided by `nvim`'s, so the printed number matches the plain-English
/// claim directly (e.g. `13.45` reads as "view is 13.45x slower than nvim
/// at this percentile") instead of forcing the reader to invert an
/// nvim-over-view fraction.
fn print_ratio(view: &Stats, nvim: &Stats) {
    let p50_ratio = if nvim.p50_ms == 0.0 {
        0.0
    } else {
        view.p50_ms / nvim.p50_ms
    };
    let p99_ratio = if nvim.p99_ms == 0.0 {
        0.0
    } else {
        view.p99_ms / nvim.p99_ms
    };
    println!(
        "{:<18} {:>7.2} {:>7.2}",
        "ratio (view/nvim)", p50_ratio, p99_ratio
    );
}

/// Prints a LOUD warning to stderr when the prior scratch entry was
/// discarded (stale or future-stamped), so a poisoned pairing never
/// happens silently.
fn warn_if_discarded(reason: &SoloReason) {
    match reason {
        SoloReason::Stale { age_ms } => eprintln!(
            "WARNING: discarding stale latency scratch entry ({age_ms}ms old, over the \
             {STALENESS_BOUND_MS}ms staleness bound). Pairing it with this run would \
             silently corrupt the comparison table; treating this run as a fresh solo \
             measurement instead."
        ),
        SoloReason::FutureTimestamp { skew_ms } => eprintln!(
            "WARNING: discarding latency scratch entry stamped {skew_ms}ms in the future; \
             the wall clock stepped backwards, so the entry's age is unknowable. Treating \
             this run as a fresh solo measurement instead."
        ),
        SoloReason::NoPriorEntry | SoloReason::SameLabel => {}
    }
}

/// Prints this run's row and, if a fresh prior run from a different label
/// is staged in the scratch file, also prints the paired comparison table
/// (`view` row, `nvim` row, ratio row, canonically ordered regardless of
/// which binary ran first) and clears the scratch file.
///
/// The single-label CLI (`latency <label> <path>`) cannot itself produce a
/// two-row table in one process, since `task bench-latency` runs this
/// binary once per target. Staging the first run's stats in `target/`,
/// timestamped, lets the second run complete the comparison without
/// changing the CLI contract or introducing a supervising process. A
/// staged entry older than [`STALENESS_BOUND_MS`] is never paired: it is
/// discarded with a loud stderr warning and this run proceeds solo, so a
/// leftover debug invocation can never silently poison the next real pair.
fn report(stats: &Stats) -> Result<()> {
    let path = scratch_path();
    let raw = std::fs::read_to_string(&path).ok();
    let prior = match raw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(content) => {
            let parsed = ScratchEntry::from_line(content);
            if parsed.is_none() {
                eprintln!(
                    "WARNING: ignoring unparseable latency scratch entry at \
                     {}; treating this run as a fresh solo measurement.",
                    path.display()
                );
            }
            parsed
        }
        None => None,
    };
    let now_ms = now_epoch_ms();

    match decide_pairing(prior, stats, now_ms) {
        PairDecision::Paired(view, nvim) => {
            print_header();
            print_row(&view);
            print_row(&nvim);
            print_ratio(&view, &nvim);
            std::fs::remove_file(&path).context("failed to clear latency scratch file")?;
        }
        PairDecision::Solo(reason) => {
            warn_if_discarded(&reason);
            print_header();
            print_row(stats);
            std::fs::write(&path, ScratchEntry::now(stats.clone()).to_line())
                .context("failed to stage latency scratch file")?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, label, bin_path] = args.as_slice() else {
        bail!("usage: latency <label> <path-to-binary>");
    };

    let samples = measure(bin_path).with_context(|| format!("measuring latency for {label}"))?;
    let stats = Stats::from_samples(label, &samples);
    report(&stats)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn percentile_on_sorted_samples_picks_nearest_rank() {
        let samples = Samples(vec![1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(samples.percentile(0.0), 1.0);
        assert_eq!(samples.percentile(100.0), 5.0);
        assert_eq!(samples.percentile(50.0), 3.0);
    }

    #[test]
    fn stats_line_round_trips() {
        let stats = Stats {
            label: "view".to_string(),
            p50_ms: 3.25,
            p99_ms: 6.5,
            max_ms: 7.1,
            samples: 200,
        };
        let parsed = Stats::from_line(&stats.to_line()).expect("round trip must parse");
        assert_eq!(parsed.label, "view");
        assert_eq!(parsed.samples, 200);
        assert!((parsed.p99_ms - 6.5).abs() < f64::EPSILON);
    }

    /// Extracts a [`PairDecision::Paired`] for assertions; callers
    /// `.expect(...)` the result under the module-level allow.
    fn paired(decision: PairDecision) -> Option<(Stats, Stats)> {
        match decision {
            PairDecision::Paired(view, nvim) => Some((view, nvim)),
            PairDecision::Solo(_) => None,
        }
    }

    fn sample_stats(label: &str) -> Stats {
        Stats {
            label: label.to_string(),
            p50_ms: 1.0,
            p99_ms: 2.0,
            max_ms: 3.0,
            samples: 200,
        }
    }

    #[test]
    fn scratch_entry_line_round_trips() {
        let entry = ScratchEntry {
            written_at_ms: 1_753_000_000_123,
            stats: sample_stats("nvim"),
        };
        let parsed = ScratchEntry::from_line(&entry.to_line()).expect("round trip must parse");
        assert_eq!(parsed.written_at_ms, 1_753_000_000_123);
        assert_eq!(parsed.stats.label, "nvim");
        assert_eq!(parsed.stats.samples, 200);
    }

    #[test]
    fn fresh_pair_pairs_and_orders_view_first() {
        let now_ms = 1_000_000;
        let prior = ScratchEntry {
            written_at_ms: now_ms - 5_000,
            stats: sample_stats("nvim"),
        };
        let current = sample_stats("view");
        let decision = decide_pairing(Some(prior), &current, now_ms);
        let (view, nvim) = paired(decision).expect("expected a paired decision");
        assert_eq!(view.label, "view");
        assert_eq!(nvim.label, "nvim");

        // Order is canonical (view first) regardless of which half staged first.
        let prior = ScratchEntry {
            written_at_ms: now_ms - 5_000,
            stats: sample_stats("view"),
        };
        let current = sample_stats("nvim");
        let decision = decide_pairing(Some(prior), &current, now_ms);
        let (view, nvim) = paired(decision).expect("expected a paired decision");
        assert_eq!(view.label, "view");
        assert_eq!(nvim.label, "nvim");
    }

    #[test]
    fn stale_entry_is_rejected_not_paired() {
        let now_ms = 1_000_000;
        let prior = ScratchEntry {
            written_at_ms: now_ms - (STALENESS_BOUND_MS + 1),
            stats: sample_stats("nvim"),
        };
        let current = sample_stats("view");
        let decision = decide_pairing(Some(prior), &current, now_ms);
        assert_eq!(
            decision,
            PairDecision::Solo(SoloReason::Stale {
                age_ms: STALENESS_BOUND_MS + 1
            })
        );
    }

    #[test]
    fn future_stamped_entry_is_rejected_not_paired() {
        let now_ms = 1_000_000;
        let prior = ScratchEntry {
            written_at_ms: now_ms + 30_000,
            stats: sample_stats("nvim"),
        };
        let current = sample_stats("view");
        let decision = decide_pairing(Some(prior), &current, now_ms);
        assert_eq!(
            decision,
            PairDecision::Solo(SoloReason::FutureTimestamp { skew_ms: 30_000 })
        );
    }

    #[test]
    fn same_label_overwrites_instead_of_pairing() {
        let now_ms = 1_000_000;
        let prior = ScratchEntry {
            written_at_ms: now_ms - 1_000,
            stats: sample_stats("nvim"),
        };
        let current = sample_stats("nvim");
        let decision = decide_pairing(Some(prior), &current, now_ms);
        assert_eq!(decision, PairDecision::Solo(SoloReason::SameLabel));
    }

    #[test]
    fn no_prior_entry_is_solo() {
        let current = sample_stats("view");
        let decision = decide_pairing(None, &current, 1_000_000);
        assert_eq!(decision, PairDecision::Solo(SoloReason::NoPriorEntry));
    }
}
