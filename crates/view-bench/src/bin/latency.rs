//! `latency <label> <path-to-binary>`: measures keypress-to-paint latency of
//! one editor binary running in a real pty, one character at a time.
//!
//! `task bench-latency` invokes this binary twice, once for `view` and once
//! for `nvim`, so a single run only ever measures one target. Pairing the
//! two runs into the comparison table happens across invocations via a
//! scratch file in `target/`; see [`report`].

use anyhow::{bail, Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Keystroke-to-paint samples collected per target, matching the brief's
/// measurement protocol.
const SAMPLE_COUNT: usize = 200;
/// Fixed settle time before driving input, per the v0 protocol; the
/// deterministic redraw-settled signal arrives with the real P3 harness.
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

    /// Nearest-rank percentile; informational at this phase, so a simple
    /// rank is sufficient without interpolation.
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

/// Forwards every chunk read from `reader` onto a channel so the caller can
/// poll with a bounded timeout instead of blocking on a single `read` that
/// may return only part of the child's output.
fn spawn_reader(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0_u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

/// A target process running inside a real pty, with everything needed to
/// drive it and observe its screen.
struct PtyTarget {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    parser: vt100::Parser,
    scratch: PathBuf,
    isolated_home: PathBuf,
}

/// Resolves `bin_path` to an absolute path when it names an existing file,
/// leaving bare command names (e.g. `nvim`) untouched.
///
/// `portable_pty`'s spawn only treats a relative path as cwd-relative when
/// it explicitly starts with `./` or `../`; a plain `target/release/view`
/// argument falls through to a PATH search and fails to spawn even though
/// the file exists relative to the current directory. Canonicalizing first
/// sidesteps that without requiring every caller to spell the `./` prefix.
fn resolve_bin_path(bin_path: &str) -> String {
    std::fs::canonicalize(bin_path)
        .map(|abs| abs.to_string_lossy().into_owned())
        .unwrap_or_else(|_| bin_path.to_string())
}

impl PtyTarget {
    /// Opens a 24x80 pty and spawns `bin_path` against a scratch file, with
    /// the host's real editor config isolated out of the way.
    ///
    /// Without a file argument, both `view` and `nvim` fall back to
    /// whatever startup UI the host's own config wires up (a dashboard, a
    /// file-explorer sidebar); observed directly on this host, where the
    /// user's `nvim` config opens `nvim-tree` and a lazy.nvim dashboard on
    /// a bare launch, so the sample character never lands where the
    /// harness expects it. A scratch file argument plus isolated
    /// `XDG_*_HOME` variables (the same fix `view-oracle`'s pty smoke tests
    /// use) guarantees a plain buffer regardless of the host's config.
    fn spawn(bin_path: &str) -> Result<Self> {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("failed to open pty")?;

        let pid = std::process::id();
        let scratch = std::env::temp_dir().join(format!("view-bench-latency-{pid}.txt"));
        let isolated_home = std::env::temp_dir().join(format!("view-bench-latency-home-{pid}"));
        std::fs::create_dir_all(&isolated_home)
            .context("failed to create isolated XDG home for bench target")?;

        let mut cmd = CommandBuilder::new(resolve_bin_path(bin_path));
        cmd.arg(&scratch);
        for var in [
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
        ] {
            cmd.env(var, isolated_home.join(var.to_lowercase()));
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("failed to spawn {bin_path}"))?;
        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone pty reader")?;
        let writer = pair
            .master
            .take_writer()
            .context("failed to take pty writer")?;
        // the slave fd must not outlive the child's own copy, or the master
        // never sees EOF once the child exits
        drop(pair.slave);

        let rx = spawn_reader(reader);
        let parser = vt100::Parser::new(24, 80, 0);

        Ok(Self {
            child,
            rx,
            writer,
            parser,
            scratch,
            isolated_home,
        })
    }

    /// Pulls every chunk already buffered on the channel into the parser
    /// without blocking, so a stale count from before the settle wait never
    /// pollutes the baseline the sampling loop starts from.
    fn drain_available(&mut self) {
        while let Ok(chunk) = self.rx.try_recv() {
            self.parser.process(&chunk);
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .write_all(bytes)
            .context("failed to write to pty")?;
        self.writer.flush().context("failed to flush pty writer")
    }

    fn count_char(&self, target: u8) -> usize {
        self.parser
            .screen()
            .contents()
            .bytes()
            .filter(|&b| b == target)
            .count()
    }

    /// Blocks (up to `timeout`) until at least `expected` occurrences of
    /// `target` are visible on screen, returning whether that happened.
    fn wait_for_count(&mut self, target: u8, expected: usize, timeout: Duration) -> bool {
        if self.count_char(target) >= expected {
            return true;
        }
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(20)) {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    if self.count_char(target) >= expected {
                        return true;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        false
    }

    /// Best-effort shutdown: ask nicely, then kill if the process hasn't
    /// exited quickly. A hung measurement target must never hang the bench
    /// task itself.
    fn shutdown(&mut self) {
        let _ = self.write(b"\x1b:qa!\r");
        std::thread::sleep(Duration::from_millis(200));
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
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
    target.drain_available();
    target.write(b"i")?;
    // let insert-mode entry settle before the baseline count is taken, or a
    // slow-starting target's own redraw could be mistaken for a sample
    std::thread::sleep(Duration::from_millis(200));
    target.drain_available();

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
                target.parser.screen().contents()
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

fn print_ratio(first: &Stats, second: &Stats) {
    let p50_ratio = if first.p50_ms == 0.0 {
        0.0
    } else {
        second.p50_ms / first.p50_ms
    };
    let p99_ratio = if first.p99_ms == 0.0 {
        0.0
    } else {
        second.p99_ms / first.p99_ms
    };
    println!("{:<7} {:>7.2} {:>7.2}", "ratio", p50_ratio, p99_ratio);
}

/// Prints this run's row and, if a prior run from a different label is
/// staged in the scratch file, also prints the paired comparison table
/// (both rows plus the ratio row) and clears the scratch file.
///
/// The single-label CLI (`latency <label> <path>`) cannot itself produce a
/// two-row table in one process, since `task bench-latency` runs this
/// binary once per target. Staging the first run's stats in `target/` lets
/// the second run complete the comparison without changing the CLI
/// contract or introducing a supervising process.
fn report(stats: &Stats) -> Result<()> {
    let path = scratch_path();
    let prior = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| Stats::from_line(content.trim()));

    match prior {
        Some(prior) if prior.label != stats.label => {
            print_header();
            print_row(&prior);
            print_row(stats);
            print_ratio(&prior, stats);
            std::fs::remove_file(&path).context("failed to clear latency scratch file")?;
        }
        _ => {
            print_header();
            print_row(stats);
            std::fs::write(&path, stats.to_line())
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
}
