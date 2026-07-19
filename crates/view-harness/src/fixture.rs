//! Fixture and engine-pin plumbing shared by the harness bins: workspace
//! paths, the `.engine-pin` read + binary verification, the hermetic
//! fixture-copy primitive, and the lockfile-keyed plugin cache. One home
//! for these because two consumers spawn editors against the same
//! `compat/fixtures/` trees (the compat driver and the bench matrix), and
//! a second copy of the cache-key or copy logic would let the two drift
//! into measuring different worlds.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Errors resolving fixtures or verifying the pinned engine.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FixtureError {
    #[error("reading engine pin from {path}: {source}")]
    PinRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("running {bin} --version: {source}")]
    NvimVersionProbe {
        bin: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "nvim binary {bin} reports version {reported:?} but .engine-pin names {pin:?}; \
         install/select the pinned nvim before running"
    )]
    PinMismatch {
        bin: PathBuf,
        reported: String,
        pin: String,
    },
    #[error("copying {path}: {source}")]
    Copy {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Resolved from this crate's own manifest dir rather than the caller's
/// cwd: `task` targets always run from the repo root today, but a direct
/// `cargo run -p view-harness` invocation from a subdirectory must not
/// silently read a stale or absent path instead.
#[must_use]
pub fn workspace_root() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path
}

/// Parent directory for one harness bin's scratch worlds (`name` keeps
/// the bins' scratch trees apart). Lives under `target/` rather than the
/// system temp dir because /tmp is commonly a RAM-backed tmpfs (a leaked
/// fixture copy would cost memory, not disk) and `target/` is already the
/// disk-backed, gitignored home of build byproducts.
#[must_use]
pub fn scratch_root(name: &str) -> PathBuf {
    workspace_root().join("target").join(name)
}

/// Path to the repo-root `.engine-pin` file.
#[must_use]
pub fn engine_pin_path() -> PathBuf {
    workspace_root().join(".engine-pin")
}

/// Reads and trims the current `.engine-pin` value -- the single source of
/// truth `scripts/check-engine-pin.sh` gates CI against -- never a
/// hardcoded version literal here, so a pin bump does not require a
/// harness code change to stay accurate.
///
/// # Errors
///
/// Returns [`FixtureError::PinRead`] if `.engine-pin` cannot be read.
pub fn current_engine_pin() -> Result<String, FixtureError> {
    let path = engine_pin_path();
    let raw = std::fs::read_to_string(&path).map_err(|source| FixtureError::PinRead {
        path: path.clone(),
        source,
    })?;
    Ok(raw.trim().to_string())
}

/// Confirms `nvim_bin` (resolved via `PATH` unless overridden) actually
/// reports the version `.engine-pin` names, before any run stamps a result
/// row, a corpus entry, or a baseline with that pin. `.engine-pin` alone
/// only says what version a run is *supposed* to use; a stale or wrong
/// `nvim` on `PATH` could otherwise silently produce evidence claiming a
/// pin the run never really exercised.
///
/// # Errors
///
/// Returns [`FixtureError::NvimVersionProbe`] if `nvim_bin --version`
/// cannot be run, or [`FixtureError::PinMismatch`] if its reported version
/// does not match `pin`.
pub fn verify_nvim_matches_pin(nvim_bin: &Path, pin: &str) -> Result<(), FixtureError> {
    let output = std::process::Command::new(nvim_bin)
        .arg("--version")
        .output()
        .map_err(|source| FixtureError::NvimVersionProbe {
            bin: nvim_bin.to_path_buf(),
            source,
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported = stdout
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    if reported != pin {
        return Err(FixtureError::PinMismatch {
            bin: nvim_bin.to_path_buf(),
            reported: reported.to_string(),
            pin: pin.to_string(),
        });
    }
    Ok(())
}

/// `compat/fixtures/`, the directory every named fixture is a
/// subdirectory of.
#[must_use]
pub fn fixtures_root() -> PathBuf {
    workspace_root().join("compat").join("fixtures")
}

/// `compat/.cache/`, the shared, persistent (never per-run-hermetic)
/// plugin install cache a heavy-style fixture's `XDG_DATA_HOME` is pointed
/// at, keyed by its own `lazy-lock.json` hash. Gitignored: this is a
/// build/test cache, not a durable artifact the way `corpus/quarantine/`
/// is.
#[must_use]
pub fn cache_root() -> PathBuf {
    workspace_root().join("compat").join(".cache")
}

/// Hashes `bytes` (a fixture's `lazy-lock.json` content) into a stable,
/// filesystem-safe cache-directory name. [`DefaultHasher`] rather than a
/// cryptographic hash: this key only has to agree with itself across runs
/// of the *same* compiled harness binary (a toolchain upgrade invalidating
/// the cache and forcing one re-clone is an acceptable, self-healing cost,
/// not a correctness bug -- lazy.nvim's own "already installed?" check
/// still governs whether that re-clone actually happens).
#[must_use]
pub fn lockfile_cache_key(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Recursively copies `src` into `dst`, creating `dst` and any needed
/// subdirectories. Regular files only: none of the committed fixture trees
/// contain a symlink, so one found unexpectedly is skipped rather than
/// copied as a symlink or followed, to avoid silently escaping `src`.
///
/// # Errors
///
/// Returns [`FixtureError::Copy`] if any file or directory under `src`
/// cannot be read, or cannot be created/written under `dst`.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), FixtureError> {
    let ctx = |path: &Path| {
        let path = path.to_path_buf();
        move |source| FixtureError::Copy { path, source }
    };
    std::fs::create_dir_all(dst).map_err(ctx(dst))?;
    for entry in std::fs::read_dir(src).map_err(ctx(src))? {
        let entry = entry.map_err(ctx(src))?;
        let file_type = entry.file_type().map_err(ctx(&entry.path()))?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &dst_path).map_err(ctx(&entry.path()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn lockfile_cache_key_is_stable_for_identical_bytes() {
        assert_eq!(lockfile_cache_key(b"abc"), lockfile_cache_key(b"abc"));
    }

    #[test]
    fn lockfile_cache_key_differs_for_different_bytes() {
        assert_ne!(lockfile_cache_key(b"abc"), lockfile_cache_key(b"abd"));
    }

    #[test]
    fn copy_dir_recursive_copies_nested_files() {
        let base = std::env::temp_dir().join(format!("view-fixture-copy-{}", std::process::id()));
        let src = base.join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("a.txt"), b"top").unwrap();
        std::fs::write(src.join("nested").join("b.txt"), b"deep").unwrap();
        let dst = base.join("dst");
        copy_dir_recursive(&src, &dst).unwrap();
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"top");
        assert_eq!(std::fs::read(dst.join("nested/b.txt")).unwrap(), b"deep");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
