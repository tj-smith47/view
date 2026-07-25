//! The memory scenario: the view process's settled memory after the
//! standard workload (10 buffers opened and visited), sampled under the
//! metric its platform defines (spec 3.4) -- `pss_mb` from
//! `smaps_rollup` on Linux, `phys_footprint_mb` from the kernel's
//! per-task footprint ledger on macOS. A proportional/footprint measure
//! rather than peak RSS, and the view process only: the embedded nvim is
//! a separate process the view-side budget deliberately excludes.

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

/// The metric name this platform records the memory row under, or
/// `None` where no memory measurement is defined for the platform.
///
/// The name differs per platform because the quantity does: PSS and
/// phys_footprint are related but not the same number. Each platform
/// therefore records under its own name, so a baseline recorded on one
/// can never be read as a bar for the other, and a platform whose
/// measurement is undefined yields `None` rather than a lookalike number
/// under a borrowed name.
pub const METRIC: Option<&str> = if cfg!(target_os = "linux") {
    Some("pss_mb")
} else if cfg!(target_os = "macos") {
    Some("phys_footprint_mb")
} else {
    None
};

/// Reads this platform's memory metric for `pid`, in megabytes, rejecting
/// any reading that is not a positive finite number.
///
/// Every platform's reader goes through the floor, because every one of
/// them can report success while yielding zero: Linux's `smaps_rollup`
/// parses a literal `Pss: 0 kB` without complaint, and the macOS ledger is
/// a plain struct field the kernel fills in. Zero is not a plausible
/// footprint for a running editor, and it is the one wrong value that
/// gates green forever once recorded, since every later measurement is
/// then a breach of a zero bar rather than a pass.
fn read_memory_mb(pid: u32) -> Result<f64, BenchError> {
    let mb = read_platform_memory_mb(pid)?;
    require_positive_mb(mb, pid)
}

/// The positivity floor [`read_memory_mb`] applies, split out from the
/// platform readers so one rule covers all of them and can be exercised
/// without a live process to measure.
fn require_positive_mb(mb: f64, pid: u32) -> Result<f64, BenchError> {
    if mb.is_finite() && mb > 0.0 {
        return Ok(mb);
    }
    Err(BenchError::Desync {
        context: format!(
            "memory reading for pid {pid} was {mb} MB; a running process cannot occupy \
             a non-positive amount of memory, so this is a failed or empty read, not a \
             measurement"
        ),
    })
}

/// Reads the `Pss:` line of `/proc/<pid>/smaps_rollup`, in megabytes.
#[cfg(target_os = "linux")]
fn read_platform_memory_mb(pid: u32) -> Result<f64, BenchError> {
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

/// Reads the kernel's `phys_footprint` ledger for `pid`, in megabytes.
#[cfg(target_os = "macos")]
fn read_platform_memory_mb(pid: u32) -> Result<f64, BenchError> {
    // libproc rather than task_info(TASK_VM_INFO): the measured process
    // is a child, and reading another task's info needs its task port
    // from task_for_pid, which is root/entitlement gated. proc_pid_rusage
    // reports the same per-task phys_footprint ledger for any process of
    // the same user, in one syscall per sample like the Linux read.
    let pid = i32::try_from(pid).map_err(|source| BenchError::Desync {
        context: format!("pid {pid} does not fit a C int: {source}"),
    })?;
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: the buffer is a live, correctly aligned `rusage_info_v2`,
    // which is the layout RUSAGE_INFO_V2 selects; the call writes only
    // into it and reports failure through its return value.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, info.as_mut_ptr().cast()) };
    if rc != 0 {
        return Err(BenchError::Desync {
            context: format!(
                "proc_pid_rusage(pid {pid}) failed: {}",
                std::io::Error::last_os_error()
            ),
        });
    }
    // SAFETY: the call returned success, so it initialized the buffer.
    #[allow(unsafe_code)]
    let footprint = unsafe { info.assume_init() }.ri_phys_footprint;
    Ok(footprint as f64 / (1024.0 * 1024.0))
}

/// Fails on a platform for which no memory measurement is defined; the
/// caller keeps such a platform out of the matrix via [`METRIC`], and
/// this exists so the scenario still compiles there.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_platform_memory_mb(_pid: u32) -> Result<f64, BenchError> {
    Err(BenchError::Desync {
        context: "no memory metric is defined for this platform".to_string(),
    })
}

/// The memory run's outcome.
#[derive(Debug)]
pub struct MemoryOutcome {
    pub distribution: Distribution,
    /// The metric name the reading was taken under, carried out of the
    /// run so the caller records the number under the name the platform
    /// actually measured rather than a name chosen at the call site.
    pub metric: &'static str,
    /// p99 of the post-workload reads, in megabytes.
    pub gated_mb: f64,
}

/// Spawns view, drives the 10-buffer workload, then reads this
/// platform's memory metric `protocol.warmup + protocol.samples` times.
/// Memory after a settled workload is a stable quantity; repeated reads
/// sample allocator and cache jitter around it so the recorded number is
/// a distribution statistic like every other cell, not a single lucky
/// read.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if the platform defines no memory
/// metric, the session never settles, the pid is unavailable, or the
/// per-process reading cannot be taken.
pub fn run(view: &SpawnSpec, protocol: &Protocol) -> Result<MemoryOutcome, BenchError> {
    let Some(metric) = METRIC else {
        return Err(BenchError::Desync {
            context: "no memory metric is defined for this platform".to_string(),
        });
    };
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
        raw_mb.push(read_memory_mb(pid)?);
        let next = Instant::now() + pace;
        while Instant::now() < next {
            std::thread::yield_now();
        }
    }
    session.shutdown();

    let distribution = Distribution::from_samples(&raw_mb, protocol.warmup)?;
    let gated_mb = distribution.p99();
    Ok(MemoryOutcome {
        distribution,
        metric,
        gated_mb,
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn own_process_memory_is_readable_and_positive() {
        let mb = read_memory_mb(std::process::id()).unwrap();
        assert!(mb > 0.0, "own memory reading must be positive, got {mb}");
    }

    #[test]
    fn a_non_positive_reading_is_refused_rather_than_recorded() {
        for bogus in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let err = require_positive_mb(bogus, 4242).expect_err(&format!(
                "a {bogus} MB reading must fail loudly, not become a bar every later run passes"
            ));
            let message = err.to_string();
            assert!(
                message.contains("4242"),
                "the refusal must name the pid it read, got {message:?}"
            );
        }
    }

    #[test]
    fn a_positive_reading_passes_the_floor_unchanged() {
        assert!((require_positive_mb(3.5, 1).unwrap() - 3.5).abs() < f64::EPSILON);
    }

    #[test]
    fn metric_name_is_pinned_per_platform() {
        let expected = if cfg!(target_os = "linux") {
            Some("pss_mb")
        } else if cfg!(target_os = "macos") {
            Some("phys_footprint_mb")
        } else {
            None
        };
        assert_eq!(METRIC, expected);
    }
}
