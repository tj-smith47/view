//! Fixtures shared across every crate whose tests need them: `ScratchDir`,
//! a panic-safe temp directory; `settle_mtime`, the sleep a second write
//! needs to land on an mtime the first one is distinguishable from;
//! `CountingAllocator`, for an allocation-count budget; and [`HostBudget`],
//! the wall clock a test may give a live process without gating on what
//! else the host is doing.
//!
//! Before this crate existed, `view-native::config`, `view::native`, and
//! the `cli_live`/`supersede_live` integration tests each hand-rolled the
//! same `std::env::temp_dir().join(...)` + `create_dir_all` + trailing
//! `remove_dir_all` shape, with the cleanup call sitting *after* the
//! fallible assertions the fixture existed to drive -- so a failing
//! assertion, not just a failing fixture setup, left the directory behind
//! on disk. A single `Drop` impl runs on every exit path, panic included,
//! which a trailing statement never can.
//!
//! A dedicated leaf crate rather than a home in `view-core`: `view-core` is
//! pure by charter (no I/O, no filesystem -- see its own module doc), and
//! `view-native`'s tests and `view`'s tests are two different downstream
//! branches of `core ← surface ← {native, ai}` with no other common
//! dependency both can reach a shared helper through. This crate sits
//! outside that graph entirely -- nothing in it depends on `ScratchDir`,
//! and `ScratchDir` depends on nothing in it -- so pulling it in as a
//! `[dev-dependencies]` entry adds no edge to the direction
//! `scripts/audit-deps.sh` enforces.

use std::alloc::{GlobalAlloc, Layout, System};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// A directory under the OS temp root, owned for the fixture's lifetime and
/// removed on every exit path via [`Drop`] -- including a panicking
/// assertion between construction and whatever cleanup a test used to do by
/// hand.
///
/// Derefs to [`Path`] so it drops into every call site that used to pass
/// `&PathBuf`/`&Path` (`&scratch` where a `&Path` parameter is expected)
/// without changing their signatures.
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// Creates a fresh, empty directory named `view-<label>-<pid>` under
    /// [`std::env::temp_dir`]. Any directory a previous run of the same
    /// label leaked (a process kill mid-test, before this type existed) is
    /// removed first, so a stale fixture from an earlier crash can never be
    /// read as this run's own state.
    ///
    /// `label` distinguishes fixtures within one test binary; the process
    /// id distinguishes concurrent test binaries (`cargo test` runs
    /// multiple integration test binaries as separate processes) sharing
    /// the same temp root.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] if the directory cannot be
    /// created. This crate carries the workspace's panic-free lint set like
    /// every other lib crate -- there is no dev-only carve-out for it, test
    /// code included -- so the caller, not this constructor, decides how a
    /// setup failure surfaces (typically `.expect(...)` at the test's own
    /// `#[cfg(test)]` boundary, where that lint is already relaxed).
    pub fn new(label: &str) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("view-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    /// The directory's path, for a call site that wants it explicitly
    /// rather than through `Deref`.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Deref for ScratchDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for ScratchDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Sleeps long enough for the next write to land on a filesystem mtime
/// distinguishable from the last one.
///
/// Coarse filesystem mtime resolution can otherwise leave a fixture's own
/// write and the "external" write that follows it inside the same clock
/// tick, which nvim's own file-changed check cannot tell apart -- the same
/// reason `docs/checktime-wire-capture.md`'s capture method sleeps between
/// the two writes of every case needing two distinct disk mtimes.
pub fn settle_mtime() {
    std::thread::sleep(std::time::Duration::from_millis(1100));
}

/// A [`GlobalAlloc`] that forwards every call unchanged to [`System`] while
/// counting `alloc` calls, for a test that asserts an allocation-count
/// budget on a hot path (a fresh `Vec`/`String` per call where amortized
/// growth was meant to hold is the regression this exists to catch).
///
/// Only the counting logic lives here, not a `static` declaration: a
/// process has exactly one `#[global_allocator]`, and setting it applies to
/// the whole binary, so a consuming integration-test crate declares its own
/// `static ALLOCATOR: CountingAllocator = CountingAllocator::new();` rather
/// than this crate declaring one on every consumer's behalf -- most crates
/// pull this crate in only for [`ScratchDir`], and swapping their allocator
/// out from under them because one unrelated test file wanted a counter
/// would be silent, process-wide collateral.
pub struct CountingAllocator {
    count: AtomicUsize,
}

impl CountingAllocator {
    /// A fresh counter at zero. `const fn` because `#[global_allocator]`
    /// statics must be const-initialized.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
        }
    }

    /// Allocations counted since construction or the last [`Self::reset`].
    /// Relaxed: a test reads this from the same thread that drove the
    /// allocations under measurement, so no cross-thread ordering is
    /// needed.
    #[must_use]
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// Zeroes the counter, so a test can isolate the allocations one
    /// operation makes from whatever ran earlier in the same binary.
    pub fn reset(&self) {
        self.count.store(0, Ordering::Relaxed);
    }
}

impl Default for CountingAllocator {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: every call forwards unchanged to `System`, itself a valid
// `GlobalAlloc`; the only added behavior is a relaxed counter increment
// around the forwarded `alloc` call, which changes nothing about what
// memory is returned or how it must be freed.
#[allow(unsafe_code)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.count.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
}

/// The most [`HostBudget`] will widen a host's share by.
///
/// A bound that scales without limit stops being a bound: past this much
/// contention the run is measuring the neighbours, and the honest outcome
/// is a failure that says so rather than a pass that could not have gone
/// any other way.
pub const MAX_LOAD_FACTOR: f64 = 3.0;

/// The wall clock a test gives a live process, split into the part the
/// code's own constants own and the part the host owns.
///
/// A gated test that spawns nvim, drives a pty or waits on an RPC round
/// trip is timing two different things at once. One is the property under
/// test -- a probe deadline, a handshake bound, a settle sleep -- and it
/// costs the same on every machine. The other is a process spawn, a
/// scheduler and a page cache, and it costs whatever the host has left
/// over. A single hand-picked wall clock covering both fails on a loaded
/// host without saying anything about the code, which is a defect in the
/// gate rather than a finding about the code; splitting them lets the
/// host's half scale with the contention the run actually started under
/// while the code's half stays exactly where it was.
///
/// An idle host and a host that publishes no load average both keep the
/// unscaled bound, so this only ever widens, never narrows, what a bound
/// asserted before it was derived this way.
pub struct HostBudget {
    fixed: Duration,
    host_share: Duration,
    load: Option<f64>,
    factor: f64,
}

impl HostBudget {
    /// A budget whose constants cost `fixed` and whose host work is allowed
    /// `host_share` before scaling.
    #[must_use]
    pub fn new(fixed: Duration, host_share: Duration) -> Self {
        let load = host_load();
        Self {
            fixed,
            host_share,
            load,
            factor: load.map_or(1.0, load_factor),
        }
    }

    /// A budget that is all host work: a bound on an operation whose whole
    /// cost is the host answering, such as one live RPC round trip.
    #[must_use]
    pub fn host_only(host_share: Duration) -> Self {
        Self::new(Duration::ZERO, host_share)
    }

    /// The whole bound: the fixed part plus the host's scaled share.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.fixed + self.host_share.mul_f64(self.factor)
    }

    /// What the sequence's own constants cost.
    #[must_use]
    pub fn fixed(&self) -> Duration {
        self.fixed
    }

    /// What the host is allowed, after scaling.
    #[must_use]
    pub fn allowance(&self) -> Duration {
        self.host_share.mul_f64(self.factor)
    }

    /// The 1-minute load average this budget was built under, or `None` on
    /// a host that publishes none.
    #[must_use]
    pub fn load(&self) -> Option<f64> {
        self.load
    }
}

impl std::fmt::Display for HostBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} = {:?} of the code's own constants + {:?} for the host",
            self.total(),
            self.fixed,
            self.allowance()
        )?;
        match self.load {
            Some(load) => write!(
                f,
                " ({:?} x{:.2} at 1-min load {load:.2} over {} cpu(s))",
                self.host_share,
                self.factor,
                cpus()
            ),
            None => write!(
                f,
                " (this host publishes no load average, so the host's share \
                 keeps its unscaled {:?})",
                self.host_share
            ),
        }
    }
}

/// `base` widened for the load this host started the call under: the
/// one-liner for a bound with no fixed part of its own.
#[must_use]
pub fn host_deadline(base: Duration) -> Duration {
    HostBudget::host_only(base).total()
}

/// How long a test may make no progress at all before its own process ends
/// it, before [`watchdog`] scales it for the host's load.
///
/// Generous on purpose: this bounds a wedge, not a runtime. A test that has
/// not reached its own end in this long is not a slow test, it is one
/// blocked on something that will never arrive -- a read on a descriptor
/// whose bytes another parser already took, a lock nobody releases.
const WEDGE_BOUND: Duration = Duration::from_secs(20);

/// Disarms the watchdog that produced it when dropped. Bind it (`let
/// _watchdog = ...`) for the length of the test rather than discarding it.
pub struct Watchdog(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Ends the process, non-zero, if the test that armed this has not finished
/// within [`WEDGE_BOUND`] scaled by the load this run started under.
///
/// For the tests whose own failure mode is a block rather than a wrong
/// answer: a drain that never returns reports nothing at all, and the suite
/// stalls until whatever outer timeout is watching it, which is minutes of
/// nothing instead of one named failure.
///
/// It aborts rather than panics because a panic on this thread is not the
/// test's panic: the blocked thread stays blocked and the harness still
/// never finishes. Aborting is the only exit a wedged process can be given
/// from the outside, and it is what makes the wedge a reported failure.
#[must_use]
pub fn watchdog() -> Watchdog {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let armed = std::sync::Arc::clone(&done);
    let bound = host_deadline(WEDGE_BOUND);
    std::thread::spawn(move || {
        std::thread::sleep(bound);
        if !armed.load(Ordering::Relaxed) {
            eprintln!(
                "no progress in {bound:?}: a test that blocks reports \
                 nothing, so this ends the process instead"
            );
            std::process::abort();
        }
    });
    Watchdog(done)
}

/// What a 1-minute load average of `load` multiplies the host's share by.
///
/// Contention, not raw runnable count: a load of four is idle on a
/// twelve-core host and a threefold overcommit on one core, and a bound has
/// to mean the same thing on both. Split out from the reading so the rule
/// can be asserted without a load to produce it.
#[must_use]
pub fn load_factor(load: f64) -> f64 {
    (1.0 + load / f64::from(cpus())).clamp(1.0, MAX_LOAD_FACTOR)
}

/// This host's logical cpu count, or one where it cannot be determined --
/// the conservative reading, since it makes any load look like full
/// contention rather than none.
#[must_use]
pub fn cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(u32::MAX))
}

/// This host's 1-minute load average, or `None` where it cannot be read.
#[must_use]
pub fn host_load() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/loadavg")
            .ok()?
            .split_ascii_whitespace()
            .next()?
            .parse()
            .ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // `{ 1.23 4.56 7.89 }`
        let out = std::process::Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .ok()?;
        String::from_utf8(out.stdout)
            .ok()?
            .split_ascii_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn new_creates_an_existing_empty_directory() {
        let dir = ScratchDir::new("new-creates").unwrap();
        assert!(dir.path().is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn drop_removes_the_directory() {
        let path = {
            let dir = ScratchDir::new("drop-removes").unwrap();
            dir.path().to_owned()
        };
        assert!(!path.exists(), "the directory must not survive the guard");
    }

    #[test]
    fn drop_removes_the_directory_even_after_a_write() {
        let path = {
            let dir = ScratchDir::new("drop-removes-with-content").unwrap();
            std::fs::write(dir.join("file.txt"), b"x").unwrap();
            dir.path().to_owned()
        };
        assert!(!path.exists());
    }

    #[test]
    fn deref_reaches_path_methods_directly() {
        let dir = ScratchDir::new("deref").unwrap();
        // exercises Deref<Target = Path>: join() is a Path method, not one
        // ScratchDir redeclares
        let file = dir.join("nested.txt");
        assert!(file.starts_with(dir.path()));
    }

    #[test]
    fn a_stale_directory_from_a_prior_run_is_replaced() {
        let label = "stale-replace";
        let leaked = std::env::temp_dir().join(format!("view-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&leaked).unwrap();
        std::fs::write(leaked.join("leftover.txt"), b"stale").unwrap();

        let dir = ScratchDir::new(label).unwrap();
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "a leaked directory from an earlier run must not leak its contents into this run"
        );
    }

    #[test]
    fn new_reports_the_underlying_error_rather_than_panicking() {
        // the exact path new() would create already exists as a regular
        // file: remove_dir_all can't clear a non-directory (its error is
        // swallowed by design, same as a genuinely-absent prior run), so
        // the following create_dir_all is the one that must fail -- this
        // proves the failure comes back as Err rather than a panic, the
        // behavior the Result-returning signature exists to prove
        let label = "new-reports-error";
        let path = std::env::temp_dir().join(format!("view-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::write(&path, b"not a directory").unwrap();

        let result = ScratchDir::new(label);

        assert!(
            result.is_err(),
            "a file occupying the target path must surface as an error, not a panic"
        );
        std::fs::remove_file(&path).unwrap();
    }
}

#[cfg(test)]
mod budget_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn an_idle_host_keeps_the_unscaled_bound() {
        assert!((load_factor(0.0) - 1.0).abs() < f64::EPSILON);
        let budget = HostBudget {
            fixed: Duration::from_millis(550),
            host_share: Duration::from_millis(2450),
            load: Some(0.0),
            factor: 1.0,
        };
        assert_eq!(budget.total(), Duration::from_secs(3));
    }

    #[test]
    fn a_host_that_publishes_no_load_keeps_the_unscaled_bound() {
        let budget = HostBudget {
            fixed: Duration::ZERO,
            host_share: Duration::from_secs(5),
            load: None,
            factor: 1.0,
        };
        assert_eq!(budget.total(), Duration::from_secs(5));
        assert!(
            format!("{budget}").contains("no load average"),
            "the rendering must say why the bound did not scale: {budget}"
        );
    }

    #[test]
    fn contention_scales_the_host_share_and_nothing_else() {
        let full = load_factor(f64::from(cpus()));
        assert!(
            (full - 2.0).abs() < 1e-9,
            "one runnable thread per cpu is one extra host's worth of time, got {full}"
        );
        let budget = HostBudget {
            fixed: Duration::from_millis(550),
            host_share: Duration::from_millis(2450),
            load: Some(1.0),
            factor: 2.0,
        };
        assert_eq!(budget.fixed(), Duration::from_millis(550));
        assert_eq!(budget.allowance(), Duration::from_millis(4900));
    }

    #[test]
    fn a_budget_built_from_a_live_reading_never_narrows_what_it_was_given() {
        let base = Duration::from_secs(5);
        assert!(
            host_deadline(base) >= base,
            "a load-scaled deadline may widen the base, never narrow it"
        );
        let budget = HostBudget::new(Duration::from_millis(550), Duration::from_millis(2450));
        assert_eq!(budget.fixed(), Duration::from_millis(550));
        assert!(
            budget.total() >= Duration::from_secs(3),
            "the whole bound is at least the unscaled sum, got {}",
            budget
        );
        assert!(
            budget.total()
                <= Duration::from_millis(550)
                    + Duration::from_millis(2450).mul_f64(MAX_LOAD_FACTOR),
            "the whole bound stays under the capped sum, got {budget}"
        );
    }

    #[test]
    fn the_factor_cannot_grow_without_limit() {
        assert!((load_factor(f64::from(cpus()) * 100.0) - MAX_LOAD_FACTOR).abs() < f64::EPSILON);
        assert!((load_factor(-5.0) - 1.0).abs() < f64::EPSILON);
    }
}
