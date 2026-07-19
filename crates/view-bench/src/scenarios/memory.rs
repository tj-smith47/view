//! The memory scenario: view-side PSS from `smaps_rollup` after the
//! standard workload (10 buffers opened and visited). PSS rather than
//! peak RSS, and the view process only: the embedded nvim is a separate
//! process the view-side budget deliberately excludes.

use std::time::{Duration, Instant};

use crate::sampling::Distribution;
use crate::scenarios::Protocol;
use crate::session::{BenchSession, SpawnSpec};
use crate::BenchError;

/// Buffers the standard workload opens.
pub const WORKLOAD_BUFFERS: usize = 10;

/// The workload's buffer file names, relative to the session cwd; the
/// caller creates these before spawning so `:e` opens real content.
#[must_use]
pub fn workload_files() -> Vec<String> {
    (1..=WORKLOAD_BUFFERS)
        .map(|index| format!("buf{index:02}.txt"))
        .collect()
}

/// Content for one workload buffer: enough distinct lines that opening
/// and navigating it exercises real grid traffic, small enough to stay a
/// text-editing (not file-streaming) workload.
#[must_use]
pub fn workload_content(index: usize) -> String {
    let mut content = String::new();
    for line in 1..=200 {
        content.push_str(&format!("buffer {index:02} line {line:03} content\n"));
    }
    content
}

/// Reads the `Pss:` line of `/proc/<pid>/smaps_rollup`, in megabytes.
fn read_pss_mb(pid: u32) -> Result<f64, BenchError> {
    let path = format!("/proc/{pid}/smaps_rollup");
    let text = std::fs::read_to_string(&path).map_err(|source| BenchError::Desync {
        context: format!("reading {path}: {source}"),
    })?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Pss:") {
            let kb: f64 = rest
                .trim()
                .trim_end_matches("kB")
                .trim()
                .parse()
                .map_err(|e| BenchError::Desync {
                    context: format!("unparseable Pss line {line:?}: {e}"),
                })?;
            return Ok(kb / 1024.0);
        }
    }
    Err(BenchError::Desync {
        context: format!("{path} has no Pss line"),
    })
}

/// The memory run's outcome.
#[derive(Debug)]
pub struct MemoryOutcome {
    pub distribution: Distribution,
    /// p99 of post-workload PSS reads, in megabytes.
    pub gated_pss_mb: f64,
}

/// Spawns view, drives the 10-buffer workload, then samples PSS
/// `protocol.warmup + protocol.samples` times. PSS after a settled
/// workload is a stable quantity; repeated reads sample allocator and
/// cache jitter around it so the recorded number is a distribution
/// statistic like every other cell, not a single lucky read.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if the session never settles, the pid
/// is unavailable, or `smaps_rollup` cannot be read.
pub fn run(view: &SpawnSpec, protocol: &Protocol) -> Result<MemoryOutcome, BenchError> {
    let mut session = BenchSession::spawn(view)?;
    if !session.settle(Duration::from_secs(2), Duration::from_secs(60)) {
        return Err(BenchError::Desync {
            context: format!(
                "startup never went quiet; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    let Some(pid) = session.pid() else {
        return Err(BenchError::Desync {
            context: "platform exposed no pid for the view process".to_string(),
        });
    };

    for file in workload_files() {
        session.send(format!(":e {file}\r").as_bytes())?;
        // bottom-then-top visit forces the whole buffer through the grid
        // at least once rather than leaving unread file content unmapped
        session.send(b"G")?;
        session.send(b"gg")?;
        std::thread::sleep(Duration::from_millis(150));
    }
    if !session.settle(Duration::from_secs(1), Duration::from_secs(30)) {
        return Err(BenchError::Desync {
            context: format!(
                "workload never went quiet; screen:\n{}",
                session.screen_text()
            ),
        });
    }

    let total = protocol.warmup + protocol.samples;
    let mut raw_mb = Vec::with_capacity(total);
    let pace = Duration::from_millis(2);
    for _ in 0..total {
        raw_mb.push(read_pss_mb(pid)?);
        let next = Instant::now() + pace;
        while Instant::now() < next {
            std::thread::yield_now();
        }
    }
    session.shutdown();

    let distribution = Distribution::from_samples(&raw_mb, protocol.warmup)?;
    let gated_pss_mb = distribution.p99();
    Ok(MemoryOutcome {
        distribution,
        gated_pss_mb,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn workload_names_ten_distinct_files() {
        let files = workload_files();
        assert_eq!(files.len(), 10);
        assert_eq!(files[0], "buf01.txt");
        assert_eq!(files[9], "buf10.txt");
    }

    #[test]
    fn own_process_pss_is_readable_and_positive() {
        let mb = read_pss_mb(std::process::id()).unwrap();
        assert!(mb > 0.0, "own PSS must be positive, got {mb}");
    }
}
