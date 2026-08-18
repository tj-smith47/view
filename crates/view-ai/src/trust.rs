//! Per-project AI trust: whether this session's project has been granted
//! permission to launch an AI agent with file access, durably recorded so
//! the answer survives a restart, and the canonicalizing path-containment
//! check every agent-facing file read and write is gated on.
//!
//! `view-core` cannot depend on this crate (`scripts/audit-deps.sh`), so
//! nothing here is named from the pure core: the trust fact crosses that
//! boundary as a plain `bool` (`Effect::AiTrustSet` out, `Msg::AiTrustResolved`
//! back), and `crates/view/src/main.rs`/`crates/view/src/runtime.rs` are the
//! only callers of [`TrustStore`] itself.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Every `Result` in this module carries `TrustError` by value, so a variant
/// growing past `clippy::result_large_err`'s 128-byte threshold is a lint
/// failure rather than a review note, the same reason `AiConfigError` pins
/// its own size.
const _: () = assert!(std::mem::size_of::<TrustError>() <= 128);

/// Everything that can go wrong reading, writing, or resolving a trust
/// decision.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    /// The store file exists but could not be read.
    #[error("could not read the AI trust store {path}: {source}")]
    Read {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The store file exists and was read, but is not valid TOML.
    #[error("could not parse the AI trust store {path}: {source}")]
    Parse {
        /// The path whose contents failed to parse.
        path: PathBuf,
        /// The underlying TOML error, with its line and column.
        source: Box<toml::de::Error>,
    },
    /// The store could not be written back to disk.
    #[error("could not write the AI trust store {path}: {source}")]
    Write {
        /// The path that failed to write.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The store's in-memory content could not be encoded as TOML.
    #[error("could not encode the AI trust store: {0}")]
    Serialize(Box<toml::ser::Error>),
    /// No platform state directory could be resolved to persist trust
    /// decisions into (`$XDG_STATE_HOME`/`$HOME`/`%LOCALAPPDATA%` all
    /// absent). [`TrustStore::load`] still succeeds against an empty,
    /// unpersisted store in this case; only a write is refused.
    #[error("could not resolve a platform state directory to persist AI trust decisions")]
    NoStateDir,
    /// A path given to [`TrustStore::set_trusted`] or [`path_is_contained`]
    /// could not be resolved to its canonical form: it does not exist, a
    /// non-final component is not a directory, or a symlink along the way
    /// is dangling.
    #[error("could not resolve the canonical path of {path}: {source}")]
    Canonicalize {
        /// The path that failed to canonicalize.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
}

/// Durable record of which project roots have been granted AI agent access.
///
/// Only trust is durable; a decline is not written as "never ask again" --
/// see [`TrustStore::set_trusted`]'s own doc. Keyed by each project root's
/// canonicalized path, never a symlink-relative one, so two paths to the
/// same directory trust or distrust together.
#[must_use]
#[derive(Debug, Clone)]
pub struct TrustStore {
    /// Where this store persists to, or `None` when no platform state
    /// directory could be resolved -- see [`TrustError::NoStateDir`].
    path: Option<PathBuf>,
    trusted: BTreeSet<PathBuf>,
}

impl TrustStore {
    /// Reads (or initializes) the trust store at the platform state dir
    /// (`$XDG_STATE_HOME/view/trusted-projects.toml`, or the platform's own
    /// state-directory equivalent). Keyed by the project root's
    /// canonicalized path, never a symlink-relative one -- two paths to the
    /// same directory must trust or distrust together.
    ///
    /// A store file that does not exist yet, or a platform with no state
    /// directory to resolve at all, both answer the config-absent shape:
    /// an empty store reporting `is_trusted` false for every path, on the
    /// same terms `AiConfig::load` treats an absent `view.toml`. Only a
    /// store that exists and cannot be read or parsed is an error.
    ///
    /// # Errors
    ///
    /// Returns [`TrustError`] when the store file exists but cannot be
    /// read, or is not valid TOML.
    pub fn load() -> Result<Self, TrustError> {
        let Some(path) = store_path() else {
            return Ok(Self {
                path: None,
                trusted: BTreeSet::new(),
            });
        };
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let wire: WireStore = toml::from_str(&s).map_err(|source| TrustError::Parse {
                    path: path.clone(),
                    source: Box::new(source),
                })?;
                Ok(Self {
                    path: Some(path),
                    trusted: wire.trusted.into_iter().collect(),
                })
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path: Some(path),
                trusted: BTreeSet::new(),
            }),
            Err(source) => Err(TrustError::Read { path, source }),
        }
    }

    /// Whether `project_root` has been granted AI agent access.
    ///
    /// `project_root` is canonicalized before the lookup, the same key
    /// [`TrustStore::set_trusted`] stores under; a `project_root` that
    /// cannot be canonicalized (does not exist) is compared as given
    /// instead of erroring -- this method has no `Result` to carry a
    /// canonicalize failure through, and falling back to a literal
    /// comparison can only ever under-match a stored canonical key, never
    /// grant a trust the store did not actually record.
    #[must_use]
    pub fn is_trusted(&self, project_root: &Path) -> bool {
        let key =
            std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
        self.trusted.contains(&key)
    }

    /// Records the user's answer and persists it immediately -- a crash
    /// between answering and the next launch must not re-prompt.
    ///
    /// `trusted: true` inserts `project_root`'s canonicalized path;
    /// `trusted: false` removes it (a no-op write if it was never trusted,
    /// the common case for a first decline). Only `true` is durable in the
    /// "never ask again" sense: nothing here records *that* the user was
    /// asked and said no, so a distrusted project's next invocation, in
    /// this or a later process, is a fresh question rather than a
    /// remembered refusal.
    ///
    /// # Errors
    ///
    /// Returns [`TrustError`] when `project_root` cannot be canonicalized,
    /// no platform state directory was resolved at [`TrustStore::load`]
    /// time, or the write to disk fails.
    pub fn set_trusted(&mut self, project_root: &Path, trusted: bool) -> Result<(), TrustError> {
        let key =
            std::fs::canonicalize(project_root).map_err(|source| TrustError::Canonicalize {
                path: project_root.to_path_buf(),
                source,
            })?;
        if trusted {
            self.trusted.insert(key);
        } else {
            self.trusted.remove(&key);
        }
        self.persist()
    }

    fn persist(&self) -> Result<(), TrustError> {
        let Some(path) = &self.path else {
            return Err(TrustError::NoStateDir);
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| TrustError::Write {
                path: path.clone(),
                source,
            })?;
        }
        let wire = WireStore {
            trusted: self.trusted.iter().cloned().collect(),
        };
        let text = toml::to_string_pretty(&wire)
            .map_err(|source| TrustError::Serialize(Box::new(source)))?;
        std::fs::write(path, text).map_err(|source| TrustError::Write {
            path: path.clone(),
            source,
        })
    }
}

/// The `trusted-projects.toml` wire shape: one array of canonicalized
/// project root paths.
#[derive(Debug, Default, Serialize, Deserialize)]
struct WireStore {
    #[serde(default)]
    trusted: Vec<PathBuf>,
}

/// The trust store's own path: `$XDG_STATE_HOME/view/trusted-projects.toml`,
/// falling back to the unix convention (`~/.local/state`) or the Windows
/// convention (`%LOCALAPPDATA%`). Duplicated from `view-native::paths`'
/// identical fallback chain rather than shared with it:
/// `scripts/audit-deps.sh` forbids a `view-ai` <-> `view-native` edge in
/// either direction (see that script's own comment on the mutual forbid),
/// so each crate that owns a file of its own resolves its base directory
/// independently, the same way `view-ai`'s `[ai]` table config already does
/// (`config.rs`'s module doc).
fn store_path() -> Option<PathBuf> {
    env_dir("XDG_STATE_HOME")
        .or_else(|| env_dir("HOME").map(|h| h.join(".local").join("state")))
        .or_else(|| env_dir("LOCALAPPDATA"))
        .map(|base| base.join("view").join("trusted-projects.toml"))
}

/// A directory path from environment variable `var`, treating unset and
/// empty identically: shells routinely export empty XDG vars, and an empty
/// base would silently anchor every path built on it at the filesystem
/// root.
fn env_dir(var: &str) -> Option<PathBuf> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

/// Canonicalizing containment check: resolves both `root` and `candidate`
/// (symlinks and `..` components included) before comparing, so a
/// trusted-root escape via a symlink planted inside the project or a
/// literal `../` in an agent-supplied path is caught the same way -- a
/// prefix comparison over un-resolved paths would miss both. `Ok(false)`
/// (never an `Err`) is the answer for "resolves outside `root`"; `Err` is
/// reserved for a candidate that cannot be resolved at all (e.g. a
/// dangling symlink), which callers treat as refused all the same.
///
/// Free function, not a `TrustStore` method: no store state is needed, and
/// both the read and the write side of an agent's file access run this on
/// every request, whether or not a `TrustStore` instance happens to be in
/// scope. A client that advertises safe file access must not let a read
/// reach further than a write may.
///
/// # Errors
///
/// Returns [`TrustError::Canonicalize`] when `root` or `candidate` cannot be
/// resolved to its canonical form.
pub fn path_is_contained(root: &Path, candidate: &Path) -> Result<bool, TrustError> {
    let root = std::fs::canonicalize(root).map_err(|source| TrustError::Canonicalize {
        path: root.to_path_buf(),
        source,
    })?;
    let candidate =
        std::fs::canonicalize(candidate).map_err(|source| TrustError::Canonicalize {
            path: candidate.to_path_buf(),
            source,
        })?;
    Ok(candidate.starts_with(&root))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]

    use super::*;

    /// Serializes every test here that mutates `XDG_STATE_HOME`: the base
    /// directory `store_path` resolves is process-global, and two tests
    /// racing their own plant/restore would interleave.
    static ENV_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_mutation_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTATION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A scratch state directory under the workspace's own `target/tmp`,
    /// nonce-tagged so parallel test binaries (and repeated runs) never
    /// collide, with `XDG_STATE_HOME` pointed at it for the guarded
    /// duration of the closure. Restores the prior value (or removes it)
    /// before returning, so a later test in the same process never
    /// observes this test's redirection.
    fn with_scratch_state_dir<R>(nonce: &str, f: impl FnOnce() -> R) -> R {
        let _guard = env_mutation_guard();
        let prev = std::env::var("XDG_STATE_HOME").ok();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("view-ai-trust-{}-{}", std::process::id(), nonce));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch state dir");
        std::env::set_var("XDG_STATE_HOME", &dir);
        let result = f();
        match prev {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn scratch_dir(nonce: &str) -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!(
                "view-ai-trust-project-{}-{}",
                std::process::id(),
                nonce
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch project dir");
        dir
    }

    #[test]
    fn a_fresh_store_trusts_nothing() {
        with_scratch_state_dir("fresh", || {
            let root = scratch_dir("fresh-root");
            let store = TrustStore::load().expect("load a fresh store");
            assert!(!store.is_trusted(&root));
            let _ = std::fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn set_trusted_true_survives_a_reload_and_does_not_leak_to_a_sibling() {
        with_scratch_state_dir("reload", || {
            let root = scratch_dir("reload-root");
            let sibling = scratch_dir("reload-sibling");

            let mut store = TrustStore::load().expect("load a fresh store");
            store
                .set_trusted(&root, true)
                .expect("persist the trust decision");

            let reloaded = TrustStore::load().expect("reload the store from disk");
            assert!(
                reloaded.is_trusted(&root),
                "a freshly reloaded store must see the persisted trust"
            );
            assert!(
                !reloaded.is_trusted(&sibling),
                "trust must not leak to an unrelated sibling directory"
            );

            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&sibling);
        });
    }

    #[test]
    fn a_declined_answer_is_not_durable() {
        with_scratch_state_dir("decline", || {
            let root = scratch_dir("decline-root");
            let mut store = TrustStore::load().expect("load a fresh store");
            store
                .set_trusted(&root, false)
                .expect("persist the decline");

            let reloaded = TrustStore::load().expect("reload the store from disk");
            assert!(
                !reloaded.is_trusted(&root),
                "a decline must not be remembered as a durable fact"
            );
            let _ = std::fs::remove_dir_all(&root);
        });
    }

    #[test]
    fn path_is_contained_accepts_a_path_literally_inside_root() {
        let root = scratch_dir("contained-root");
        let inner = root.join("file.txt");
        std::fs::write(&inner, "x").expect("write inner file");
        assert!(path_is_contained(&root, &inner).expect("must resolve"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The disconfirm a naive string-prefix comparison cannot pass: `root`
    /// and a sibling `root-evil` share a string prefix even though they are
    /// unrelated directories, and `root/../root-evil` (a literal `..`
    /// escape an agent could hand a write handler) string-prefixes as
    /// `root` too. A canonicalizing check must resolve both sides and
    /// compare path components, not bytes, to see that the escape lands
    /// outside `root`.
    #[test]
    fn path_is_contained_rejects_a_dot_dot_escape_sharing_roots_string_prefix() {
        let base = scratch_dir("escape-base");
        let root = base.join("root");
        let evil_sibling = base.join("root-evil");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::create_dir_all(&evil_sibling).expect("create root-evil");
        let target = evil_sibling.join("secret.txt");
        std::fs::write(&target, "x").expect("write secret.txt");

        let escape = root.join("..").join("root-evil").join("secret.txt");
        assert!(
            !path_is_contained(&root, &escape).expect("must resolve"),
            "a `..` escape into a string-prefix-sharing sibling must be rejected"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    #[cfg(unix)]
    fn path_is_contained_rejects_a_symlink_planted_inside_root_pointing_outside_it() {
        let base = scratch_dir("symlink-base");
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::create_dir_all(&outside).expect("create outside");
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "x").expect("write secret.txt");
        let link = root.join("escape-link");
        std::os::unix::fs::symlink(&outside, &link).expect("plant the symlink");

        let candidate = link.join("secret.txt");
        assert!(
            !path_is_contained(&root, &candidate).expect("must resolve"),
            "a symlink planted inside root pointing outside it must resolve outside"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn path_is_contained_errs_on_a_dangling_symlink() {
        #[cfg(unix)]
        {
            let base = scratch_dir("dangling-base");
            let root = base.clone();
            let link = root.join("dangling");
            std::os::unix::fs::symlink(root.join("does-not-exist"), &link)
                .expect("plant a dangling symlink");
            assert!(
                path_is_contained(&root, &link).is_err(),
                "a candidate that cannot be resolved at all must be Err, not Ok(false)"
            );
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}
