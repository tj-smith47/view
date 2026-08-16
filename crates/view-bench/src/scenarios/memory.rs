//! The memory scenario: the view process's settled memory after the
//! standard workload (10 buffers opened and visited), sampled under the
//! metric its platform defines (spec 3.4) -- `pss_mb` from
//! `smaps_rollup` on Linux, `phys_footprint_mb` from the kernel's
//! per-task footprint ledger on macOS. A proportional/footprint measure
//! rather than peak RSS, and the view process only: the embedded nvim is
//! a separate process the view-side budget deliberately excludes.
//!
//! [`run_nvim`] and [`run_view_tree`] extend the same workload to the
//! equivalence-matrix resource leg (spec 3.4, ledger E2): a bare-nvim
//! reading and a view-tree reading (view's own process plus the embedded
//! nvim engine child it spawns, summed) so a caller can state an honest
//! apples-to-apples comparison instead of pairing view's own-process
//! number against nvim's whole-process one.

use std::time::{Duration, Instant};

use crate::sampling::Distribution;
use crate::scenarios::Protocol;
use crate::session::{BenchSession, NvimSpec, SettleBound, SpawnSpec, ViewSpec};
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

/// Reads `pid`'s memory metric plus every process it directly spawned,
/// summed -- the honest apples-to-apples reading against a bare editor's
/// own whole-process number, unlike [`read_memory_mb`], which by policy
/// (see the module doc) excludes the embedded nvim engine child view
/// spawns as a separate process.
///
/// Only direct children are walked: view spawns nvim directly and nothing
/// else, so one level covers the whole tree view is responsible for.
#[cfg(target_os = "linux")]
fn read_tree_memory_mb(pid: u32) -> Result<f64, BenchError> {
    let mut total = read_platform_memory_mb(pid)?;
    for child in direct_children(pid)? {
        total += read_platform_memory_mb(child)?;
    }
    require_positive_mb(total, pid)
}

/// Direct child pids of `pid`, read from the kernel's own child-tracking
/// files. Not a `/proc`-wide scan matching on `PPid`: that walk is racy
/// against processes exiting mid-scan and reads every process on the host
/// to find one relationship this file states directly.
///
/// The children file is per-thread, not per-process: a fork/exec attributes
/// the child to the specific thread that called it, so
/// `/proc/<pid>/task/<pid>/children` alone only sees children forked from
/// the main thread and silently misses one forked from any other thread in
/// a multi-threaded process (view's engine spawn is not guaranteed to run
/// on the main thread). Every thread under `/proc/<pid>/task/` is read and
/// the results unioned so a worker-thread spawn is not a silent
/// under-count. A thread that exits between listing and reading its
/// children file is skipped rather than failing the whole read: threads
/// come and go independently of the children relationship being measured.
#[cfg(target_os = "linux")]
fn direct_children(pid: u32) -> Result<Vec<u32>, BenchError> {
    let task_dir = format!("/proc/{pid}/task");
    let entries = std::fs::read_dir(&task_dir).map_err(|source| BenchError::Desync {
        context: format!("reading {task_dir}: {source}"),
    })?;
    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| BenchError::Desync {
            context: format!("reading an entry of {task_dir}: {source}"),
        })?;
        let path = entry.path().join("children");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(BenchError::Desync {
                    context: format!("reading {}: {source}", path.display()),
                })
            }
        };
        for token in text.split_whitespace() {
            let child = token.parse::<u32>().map_err(|source| BenchError::Desync {
                context: format!(
                    "unparseable child pid {token:?} in {}: {source}",
                    path.display()
                ),
            })?;
            children.push(child);
        }
    }
    children.sort_unstable();
    children.dedup();
    Ok(children)
}

/// macOS exposes no equivalent of Linux's child-tracking file without the
/// `proc_listchildpids` buffer-sizing dance libproc does not simplify, so
/// the tree reading on this platform is the single-process reading: a
/// documented under-measurement, not a silent one. A caller comparing this
/// against a Linux tree reading must know this floor omits whatever the
/// embedded engine child costs.
#[cfg(target_os = "macos")]
fn read_tree_memory_mb(pid: u32) -> Result<f64, BenchError> {
    read_memory_mb(pid)
}

/// See [`read_platform_memory_mb`]'s fallback: no tree reading is defined
/// where no single-process reading is either.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read_tree_memory_mb(_pid: u32) -> Result<f64, BenchError> {
    Err(BenchError::Desync {
        context: "no tree memory metric is defined for this platform".to_string(),
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
pub fn run(view_spec: ViewSpec<'_>, protocol: &Protocol) -> Result<MemoryOutcome, BenchError> {
    let ViewSpec(view) = view_spec;
    run_with_reader(view, protocol, read_memory_mb)
}

/// Same workload and sampling as [`run`], reading view's memory plus its
/// embedded nvim engine child's, summed -- the equivalence-matrix leg's
/// apples-to-apples reading against [`run_nvim`]'s whole-process number.
/// See [`read_tree_memory_mb`] for what each platform actually sums.
///
/// # Errors
///
/// Same as [`run`].
pub fn run_view_tree(
    view_spec: ViewSpec<'_>,
    protocol: &Protocol,
) -> Result<MemoryOutcome, BenchError> {
    let ViewSpec(view) = view_spec;
    run_with_reader(view, protocol, read_tree_memory_mb)
}

/// Same workload and sampling as [`run`], against a bare nvim spawn
/// instead of view -- the equivalence-matrix leg's baseline reading. Bare
/// nvim spawns no child of its own, so its whole-process reading already
/// is its "tree" reading; there is no separate `run_nvim_tree`.
///
/// # Errors
///
/// Same as [`run`].
pub fn run_nvim(nvim_spec: NvimSpec<'_>, protocol: &Protocol) -> Result<MemoryOutcome, BenchError> {
    let NvimSpec(nvim) = nvim_spec;
    run_with_reader(nvim, protocol, read_memory_mb)
}

/// Shared driver behind [`run`], [`run_view_tree`] and [`run_nvim`]:
/// spawn `spec`, drive the standard 10-buffer workload to settle, then
/// sample `reader(pid)` `protocol.warmup + protocol.samples` times. The
/// three public entry points differ only in which spec they spawn and
/// which reader they sample with, so the spawn/workload/settle/sample
/// sequence -- the part a transposed argument or a skipped settle wait
/// would silently corrupt -- exists exactly once.
fn run_with_reader(
    spec: &SpawnSpec,
    protocol: &Protocol,
    reader: fn(u32) -> Result<f64, BenchError>,
) -> Result<MemoryOutcome, BenchError> {
    let Some(metric) = METRIC else {
        return Err(BenchError::Desync {
            context: "no memory metric is defined for this platform".to_string(),
        });
    };
    let mut session = BenchSession::spawn(spec)?;
    if !session.settle(SettleBound {
        quiet: Duration::from_secs(2),
        deadline: Duration::from_secs(60),
    }) {
        return Err(BenchError::Desync {
            context: format!(
                "startup never went quiet; screen:\n{}",
                session.screen_text()
            ),
        });
    }
    let Some(pid) = session.pid() else {
        return Err(BenchError::Desync {
            context: "platform exposed no pid for the measured process".to_string(),
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
    if !session.settle(SettleBound {
        quiet: Duration::from_secs(1),
        deadline: Duration::from_secs(30),
    }) {
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
        // a settled screen only proves the screen stopped changing, never
        // that the process is still alive to read from: a spawn that exits
        // right after settling (a remote attach failure printed once and
        // then a quiet exit, say) passes settle() and dies before the first
        // sample, so the reader's own error carries no clue why. The screen
        // at the moment of failure is that clue -- the same diagnostic the
        // settle-timeout branch above already prints.
        raw_mb.push(reader(pid).map_err(|source| BenchError::Desync {
            context: format!("{source}; screen at failure:\n{}", session.screen_text()),
        })?);
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
    #[cfg(target_os = "linux")]
    fn direct_children_includes_a_freshly_spawned_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawning a throwaway child process for the test");
        let child_pid = child.id();
        let children = direct_children(std::process::id())
            .expect("reading this test process's own children file");
        assert!(
            children.contains(&child_pid),
            "expected spawned child {child_pid} among direct children {children:?}"
        );
        child.kill().expect("killing the throwaway child");
        let _ = child.wait();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn tree_memory_of_a_spawned_child_at_least_matches_the_parents_own_reading() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawning a throwaway child process for the test");
        // let the child fault in its own pages so its PSS reading is non-trivial
        std::thread::sleep(Duration::from_millis(100));
        let own = read_platform_memory_mb(std::process::id())
            .expect("reading this test process's own PSS");
        let tree =
            read_tree_memory_mb(std::process::id()).expect("reading this test process's tree PSS");
        assert!(
            tree >= own,
            "a tree reading with a live child must be at least the parent's own reading: \
             own={own} tree={tree}"
        );
        child.kill().expect("killing the throwaway child");
        let _ = child.wait();
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn tree_memory_falls_back_to_the_single_process_reading() {
        // Read against a quiescent subject rather than this process. Both
        // calls sample a live quantity, and a test binary running its suite
        // in parallel moves its own footprint by a page or two between two
        // of them -- which an exact comparison then reports as a
        // child-tracking reader that does not exist. A shell waiting on a
        // background sleeper allocates nothing while it waits, and its
        // having a live child is what makes the equality mean anything: on
        // macOS the tree reading *is* the single-process reading, so the
        // child's own footprint must not appear in it.
        let mut subject = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 2 & wait")
            .spawn()
            .expect("spawning a quiescent subject that has a child of its own");
        let pid = subject.id();
        // long enough for the shell to reach its wait and the sleeper to
        // fault its own pages in, so a tree reading that counted the child
        // would differ by far more than the noise this avoids
        std::thread::sleep(Duration::from_millis(200));
        let own = read_memory_mb(pid).unwrap();
        let tree = read_tree_memory_mb(pid).unwrap();
        subject.kill().expect("killing the throwaway subject");
        let _ = subject.wait();
        assert!(
            (tree - own).abs() < f64::EPSILON,
            "macOS has no child-tracking reader, so tree must equal own exactly: \
             own={own} tree={tree}"
        );
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
