//! `ScratchDir`: a panic-safe temp-directory fixture, shared across every
//! crate whose tests write one.
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

use std::ops::Deref;
use std::path::{Path, PathBuf};

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
    /// # Panics
    ///
    /// Panics if the directory cannot be created: this crate is never a
    /// normal (non-dev) dependency of anything that ships (see the module
    /// doc), and a fixture that cannot create its own scratch space cannot
    /// run the test it backs either way -- the same tradeoff this repo's
    /// convention already grants a `#[cfg(test)] mod tests` block, applied
    /// here to the one call site that needs it instead of the whole crate.
    #[must_use]
    // silenced, not fixed: this is the one panic the # Panics section above
    // documents and justifies
    #[allow(clippy::panic)]
    pub fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("view-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        if let Err(e) = std::fs::create_dir_all(&path) {
            panic!("ScratchDir::new({label:?}): {path:?}: {e}");
        }
        Self { path }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn new_creates_an_existing_empty_directory() {
        let dir = ScratchDir::new("new-creates");
        assert!(dir.path().is_dir());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn drop_removes_the_directory() {
        let path = {
            let dir = ScratchDir::new("drop-removes");
            dir.path().to_owned()
        };
        assert!(!path.exists(), "the directory must not survive the guard");
    }

    #[test]
    fn drop_removes_the_directory_even_after_a_write() {
        let path = {
            let dir = ScratchDir::new("drop-removes-with-content");
            std::fs::write(dir.join("file.txt"), b"x").unwrap();
            dir.path().to_owned()
        };
        assert!(!path.exists());
    }

    #[test]
    fn deref_reaches_path_methods_directly() {
        let dir = ScratchDir::new("deref");
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

        let dir = ScratchDir::new(label);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "a leaked directory from an earlier run must not leak its contents into this run"
        );
    }
}
