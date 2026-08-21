//! Fixtures shared across every crate whose tests need them: `ScratchDir`,
//! a panic-safe temp directory; `settle_mtime`, the sleep a second write
//! needs to land on an mtime the first one is distinguishable from; and
//! `CountingAllocator`, for an allocation-count budget.
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
