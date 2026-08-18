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
    /// The store's in-memory content could not be encoded as TOML. The
    /// common real cause: `toml`'s `PathBuf` serialization requires valid
    /// UTF-8, so a project root containing non-UTF-8 bytes fails here on
    /// every `set_trusted` call for it, forever -- fails closed, since the
    /// project is simply never durably trusted, but never surfaces as this
    /// specific cause anywhere else.
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
    /// time, or the write to disk fails. On any `Err`, `self` is left
    /// exactly as it was before the call -- the candidate set is built and
    /// persisted first, and only committed to `self` once the write is
    /// known to have succeeded, so the in-memory model can never claim a
    /// grant the store does not durably hold (a caller that read
    /// `is_trusted` straight after a failed `set_trusted` must see the same
    /// answer it would have before calling it).
    pub fn set_trusted(&mut self, project_root: &Path, trusted: bool) -> Result<(), TrustError> {
        let key =
            std::fs::canonicalize(project_root).map_err(|source| TrustError::Canonicalize {
                path: project_root.to_path_buf(),
                source,
            })?;
        let mut candidate = self.trusted.clone();
        if trusted {
            candidate.insert(key);
        } else {
            candidate.remove(&key);
        }
        persist(self.path.as_deref(), &candidate)?;
        self.trusted = candidate;
        Ok(())
    }
}

/// Writes `trusted` to `path` as TOML, or `NoStateDir` when `path` is
/// `None`. A free function taking the state it needs rather than a
/// `&TrustStore` method: [`TrustStore::set_trusted`] must persist a
/// candidate set before committing to it (see that method's own doc), so
/// this has to be callable against a set that is not `self.trusted` yet.
///
/// Writes to a nonce-tagged temp file in the same directory and renames it
/// over `path`, rather than truncating `path` directly: a crash mid-write
/// must leave whatever the store held before intact, not a half-written
/// file that fails every later `load()` with `TrustError::Parse` and loses
/// every prior trust record, not just the one being set. On unix the temp
/// file is chmod'd `0600` before the rename, so the record of which
/// projects were granted AI agent access -- a security decision -- is never
/// briefly nor durably world-readable; there is no equivalent bit to set on
/// the platforms this falls back to (`%LOCALAPPDATA%`), where per-user NTFS
/// ACLs already restrict the directory.
fn persist(path: Option<&Path>, trusted: &BTreeSet<PathBuf>) -> Result<(), TrustError> {
    let Some(path) = path else {
        return Err(TrustError::NoStateDir);
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|source| TrustError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    let wire = WireStore {
        trusted: trusted.iter().cloned().collect(),
    };
    let text =
        toml::to_string_pretty(&wire).map_err(|source| TrustError::Serialize(Box::new(source)))?;
    // pid alone collides between two concurrent `set_trusted` calls in one
    // process: `Effect::AiTrustSet` runs on a spawned thread per effect, so
    // two answers in flight share a pid. The counter makes each call's temp
    // name unique within the process too; `rename` is still what makes each
    // individual write atomic.
    static TMP_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = TMP_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = parent.join(format!(
        ".trusted-projects.toml.{}.{nonce}.tmp",
        std::process::id()
    ));
    std::fs::write(&tmp_path, &text).map_err(|source| TrustError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| TrustError::Write {
                path: path.to_path_buf(),
                source,
            },
        )?;
    }
    std::fs::rename(&tmp_path, path).map_err(|source| TrustError::Write {
        path: path.to_path_buf(),
        source,
    })
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
///
/// `pub(crate)`, not private: `provision.rs`'s cache directory resolves the
/// same XDG-style fallback chain this module's own `store_path` does, and
/// both live in this one crate, so reusing this helper is the DRY choice --
/// the mutual `view-ai`/`view-native` forbid this module's own doc explains
/// is about crossing a crate boundary, which sharing a helper inside a
/// single crate never does.
pub(crate) fn env_dir(var: &str) -> Option<PathBuf> {
    match std::env::var(var) {
        Ok(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

/// Canonicalizing containment check: resolves both `root` and `candidate`
/// (symlinks and `..` components included) before comparing, so a
/// trusted-root escape via a symlink planted inside the project or a
/// literal `../` in an agent-supplied path is caught the same way -- a
/// prefix comparison over un-resolved paths would miss both.
///
/// `candidate` need not exist yet -- the common case for the write side of
/// an agent's file access is a file being created, not one already there.
/// Only `candidate`'s deepest *existing* ancestor is canonicalized; the
/// literal, unresolved tail beyond it (the components that do not exist on
/// disk yet) is re-appended to that resolved ancestor before the
/// containment comparison. A tail containing a `..` component is refused
/// outright (`Ok(None)`) rather than joined: a `..` inside a directory that
/// does not exist yet cannot be resolved safely, since there is nothing on
/// disk to prove where it would actually land once created.
///
/// `Ok(Some(canonical))` means contained -- `canonical` is the fully
/// resolved path the answer was computed against, and **is what a caller
/// must open**, not the original `candidate`: this function's answer is a
/// point-in-time fact about the filesystem, and re-deriving anything from
/// `candidate` afterward (including a second canonicalize) re-walks the
/// same symlinks this one already resolved, at a later instant that could
/// disagree with this one. Operating on the returned path bounds that race
/// to "whatever the resolved path itself does between this call and the
/// caller's next syscall," the same residual TOCTOU window every
/// canonicalize-then-use pattern carries and no path-containment check can
/// close outright. `Ok(None)` is the answer for "resolves outside `root`,"
/// for a tail that cannot be safely resolved (see above), or for a relative
/// `candidate` (see precondition below) -- never an `Err` for either.
///
/// `candidate` must be absolute. The ACP wire shapes this gates (an agent's
/// read/write requests) always carry absolute paths, so a relative
/// `candidate` reaching this function is already a caller bug, not a
/// filesystem fact to report -- resolving it against this process's cwd
/// would answer a question about the wrong directory, and refusing it as
/// `Err` would read as a filesystem problem rather than the policy refusal
/// it is. A relative `candidate` therefore returns `Ok(None)` deterministically,
/// without touching the filesystem at all.
///
/// Free function, not a `TrustStore` method: no store state is needed, and
/// both the read and the write side of an agent's file access run this on
/// every request, whether or not a `TrustStore` instance happens to be in
/// scope. A client that advertises safe file access must not let a read
/// reach further than a write may.
///
/// # Errors
///
/// Returns [`TrustError::Canonicalize`] when `root` cannot be resolved to
/// its canonical form, or when `candidate`'s deepest existing ancestor has
/// a filesystem entry that still cannot be resolved -- a dangling symlink
/// in the existing chain, or a permission-denied directory. That case is
/// deliberately never folded into `Ok(None)`: it means the filesystem itself
/// cannot answer the question, not that this function found an answer and
/// it was "outside."
pub fn path_is_contained(root: &Path, candidate: &Path) -> Result<Option<PathBuf>, TrustError> {
    if !candidate.is_absolute() {
        return Ok(None);
    }
    let root = std::fs::canonicalize(root).map_err(|source| TrustError::Canonicalize {
        path: root.to_path_buf(),
        source,
    })?;
    let (base, tail) = resolve_deepest_existing_ancestor(candidate)?;
    if tail
        .iter()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Ok(None);
    }
    let mut full = base;
    for component in &tail {
        full.push(component.as_os_str());
    }
    Ok(full.starts_with(&root).then_some(full))
}

/// Splits `path` into its deepest existing ancestor, canonicalized, paired
/// with the literal trailing components beyond it that have no filesystem
/// entry yet -- the walk [`path_is_contained`] needs to answer for a
/// not-yet-existing candidate instead of refusing it outright.
///
/// An ancestor is "existing" by [`Path::symlink_metadata`] (an `lstat`,
/// following no symlink), not by whether it canonicalizes: a dangling
/// symlink or a permission-denied directory *has* an entry there, so
/// finding one stops the walk and canonicalizes it immediately, surfacing
/// whatever error that produces rather than treating the broken entry as
/// "not there yet" and walking past it into the tail. Only a component with
/// no entry at all -- `symlink_metadata` failing -- is genuinely absent and
/// gets folded into the tail instead.
fn resolve_deepest_existing_ancestor(
    path: &Path,
) -> Result<(PathBuf, Vec<std::path::Component<'_>>), TrustError> {
    let components: Vec<std::path::Component<'_>> = path.components().collect();
    let mut split = components.len();
    loop {
        let prefix: PathBuf = components[..split].iter().collect();
        if prefix.symlink_metadata().is_ok() {
            let canonical =
                std::fs::canonicalize(&prefix).map_err(|source| TrustError::Canonicalize {
                    path: prefix,
                    source,
                })?;
            return Ok((canonical, components[split..].to_vec()));
        }
        if split == 0 {
            // reachable only for an absolute candidate whose filesystem
            // root itself has no entry, which does not happen on any real
            // system (path_is_contained already refuses relative candidates
            // before calling this). Every prefix down to the empty one has
            // already failed symlink_metadata above, so a canonicalize call
            // here could not report anything canonicalize itself would not
            // also report as NotFound -- fabricate that error directly
            // instead of spending a syscall to reproduce it.
            return Err(TrustError::Canonicalize {
                path: path.to_path_buf(),
                source: std::io::Error::from(std::io::ErrorKind::NotFound),
            });
        }
        split -= 1;
    }
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

    /// Restores one environment variable to whatever it held before a test
    /// redirected or cleared it, on every exit from the guarded scope --
    /// including a panicking assertion, the exact moment a RED-phase test
    /// fails. A straight-line restore after the guarded work returns is
    /// skipped entirely by an unwind, leaving the redirection in place (and
    /// the mutex released, since its guard drops too) for every later test
    /// in the same process; a `Drop` impl runs regardless. Named per-`var`
    /// rather than hardcoded to `XDG_STATE_HOME` so the one no-state-dir
    /// test that must also clear `HOME`/`LOCALAPPDATA` can reuse it instead
    /// of hand-rolling a second copy.
    struct EnvRestoreGuard {
        var: &'static str,
        prev: Option<String>,
    }

    impl EnvRestoreGuard {
        fn capture(var: &'static str) -> Self {
            Self {
                var,
                prev: std::env::var(var).ok(),
            }
        }
    }

    impl Drop for EnvRestoreGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.var, v),
                None => std::env::remove_var(self.var),
            }
        }
    }

    /// A scratch state directory under the workspace's own `target/tmp`,
    /// nonce-tagged so parallel test binaries (and repeated runs) never
    /// collide, with `XDG_STATE_HOME` pointed at it for the guarded
    /// duration of the closure.
    fn with_scratch_state_dir<R>(nonce: &str, f: impl FnOnce() -> R) -> R {
        let _guard = env_mutation_guard();
        let _restore = EnvRestoreGuard::capture("XDG_STATE_HOME");
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("view-ai-trust-{}-{}", std::process::id(), nonce));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch state dir");
        std::env::set_var("XDG_STATE_HOME", &dir);
        let result = f();
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

    /// The fail-closed contract `set_trusted`'s own doc states: a write
    /// that cannot persist must leave `self` exactly as it was, never
    /// insert into the in-memory set first and persist second. Cleared
    /// rather than redirected so `store_path()` falls all the way through
    /// to `NoStateDir` instead of finding a usable directory at any of its
    /// three fallbacks.
    #[test]
    fn a_failed_persist_leaves_the_in_memory_grant_false() {
        let _guard = env_mutation_guard();
        let _restores: Vec<EnvRestoreGuard> = ["XDG_STATE_HOME", "HOME", "LOCALAPPDATA"]
            .into_iter()
            .map(EnvRestoreGuard::capture)
            .collect();
        for var in ["XDG_STATE_HOME", "HOME", "LOCALAPPDATA"] {
            std::env::remove_var(var);
        }

        let root = scratch_dir("no-state-dir-root");
        let mut store =
            TrustStore::load().expect("no state dir still loads an empty store, not an error");
        assert!(!store.is_trusted(&root), "a fresh store trusts nothing");

        let err = store
            .set_trusted(&root, true)
            .expect_err("no state dir must fail the write");
        assert!(
            matches!(err, TrustError::NoStateDir),
            "expected NoStateDir, got {err:?}"
        );
        assert!(
            !store.is_trusted(&root),
            "a failed persist must never leave the in-memory model believing the grant succeeded"
        );

        let _ = std::fs::remove_dir_all(&root);
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
        let canonical_root = std::fs::canonicalize(&root).expect("canonicalize root");
        assert_eq!(
            path_is_contained(&root, &inner).expect("must resolve"),
            Some(canonical_root.join("file.txt"))
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The write side of an agent's file access: a candidate that does not
    /// exist yet, inside a root that does, must still resolve -- the whole
    /// point `path_is_contained` exists for a not-yet-created file rather
    /// than refusing every agent file creation outright.
    #[test]
    fn path_is_contained_accepts_a_not_yet_existing_file_under_root() {
        let root = scratch_dir("new-file-root");
        let candidate = root.join("agent-created.txt");
        assert!(!candidate.exists(), "fixture must not pre-exist");
        let canonical_root = std::fs::canonicalize(&root).expect("canonicalize root");
        assert_eq!(
            path_is_contained(&root, &candidate).expect("must resolve"),
            Some(canonical_root.join("agent-created.txt")),
            "a new file directly under an existing root must resolve inside it"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The write-side counterpart to the `..`-escape test below: a
    /// not-yet-existing candidate whose tail walks back out of `root` via a
    /// literal `..` must be refused exactly like the existing-file escape
    /// is, even though nothing on either side of the `..` exists yet.
    #[test]
    fn path_is_contained_rejects_a_not_yet_existing_file_escaping_via_dot_dot() {
        let root = scratch_dir("new-file-escape-root");
        let candidate = root.join("..").join("agent-created-outside.txt");
        assert!(!candidate.exists(), "fixture must not pre-exist");
        assert_eq!(
            path_is_contained(&root, &candidate).expect("must resolve"),
            None,
            "a new file whose path walks out of root via `..` must be refused"
        );
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
        assert_eq!(
            path_is_contained(&root, &escape).expect("must resolve"),
            None,
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
        assert_eq!(
            path_is_contained(&root, &candidate).expect("must resolve"),
            None,
            "a symlink planted inside root pointing outside it must resolve outside"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A dangling symlink is an *existing* entry (its own `lstat` succeeds)
    /// that cannot be resolved -- the one case this function must still
    /// answer `Err` for, never silently fold into `Ok(None)`: `resolve_
    /// deepest_existing_ancestor` finds it has metadata and stops there
    /// rather than treating it as "not created yet" and walking past it
    /// into the tail.
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
                "a candidate whose existing entry cannot be resolved must be Err, not Ok(None)"
            );
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// The single most load-bearing line in the containment check: the
    /// `ParentDir` refusal on the *unresolved tail*. Neither of the other
    /// `..` tests reaches it -- both use `root/..`, which exists on disk, so
    /// `resolve_deepest_existing_ancestor` consumes the `..` during
    /// canonicalization and it never lands in the tail at all. This drives a
    /// `..` behind a component that does not exist yet (`nodir`), so the
    /// walk stops at `root` and the tail is `["nodir", "..", "..",
    /// "escaped-evil", "target.txt"]`: without the refusal, the literal
    /// re-append would produce `root/nodir/../../escaped-evil/target.txt`,
    /// whose *components* share `root`'s prefix, so an unresolved
    /// `starts_with` would wrongly answer `Some` for a path two levels
    /// outside `root`.
    #[test]
    fn path_is_contained_rejects_a_dot_dot_escape_via_component_arithmetic_in_an_unresolved_tail() {
        let root = scratch_dir("tail-dotdot-root");
        let candidate = root
            .join("nodir")
            .join("..")
            .join("..")
            .join("escaped-evil")
            .join("target.txt");
        assert!(!root.join("nodir").exists(), "fixture must not pre-exist");
        assert_eq!(
            path_is_contained(&root, &candidate).expect("must resolve"),
            None,
            "a `..` that only exists in the unresolved tail must still be refused"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// [`path_is_contained`]'s absolute-candidate precondition: a relative
    /// candidate must refuse deterministically rather than resolving against
    /// this process's cwd (a question about the wrong directory) or erroring
    /// as if the filesystem itself could not answer.
    #[test]
    fn path_is_contained_refuses_a_relative_candidate_without_touching_the_filesystem() {
        let root = scratch_dir("relative-candidate-root");
        assert_eq!(
            path_is_contained(&root, Path::new("relative-file.txt")).expect("must resolve"),
            None,
            "a relative candidate must be refused, not resolved against the process cwd"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
