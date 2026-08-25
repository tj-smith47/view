//! ACP adapter auto-provisioning: resolving a known adapter id to a
//! downloaded, checksum-verified, extracted, launchable entry path.
//!
//! Mechanism only, per the charter's own split: this module owns
//! downloading, verifying, extracting, and caching against a hand-
//! maintained [`AdapterPin`] table closed at compile time, on
//! `view-core::native::registry`'s own precedent (a runtime-registered
//! table could vanish an adapter silently instead of reporting it present
//! and unprovisionable). Wiring that table into a signed, automatically
//! updated manifest sourced from a release pipeline is separate packaging
//! work with no dependency on anodizer's release pipeline; nothing here
//! reaches toward one.
//!
//! The pinned `claude-code` row's checksum was captured live against the
//! adapter's real, currently published release, not guessed:
//!
//! ```text
//! curl -sS -L -o claude-agent-acp-0.69.0.tgz \
//!   https://registry.npmjs.org/@agentclientprotocol/claude-agent-acp/-/claude-agent-acp-0.69.0.tgz
//! sha256sum claude-agent-acp-0.69.0.tgz
//! # 73334255e17f5f48f08030fa4e0c54c118e820f9aaaf29f4629aa230e48c65c2
//! ```
//!
//! That artifact is a single, platform-independent npm tarball (the
//! `claude-code` ACP agent ships as TypeScript run under `node`, not a
//! compiled per-target binary), which is why the one row below carries a
//! literal URL rather than one built from `{os}`/`{arch}` substitution --
//! [`ensure_adapter`] still performs that substitution unconditionally, so
//! a future row for a real cross-compiled release needs no change to the
//! resolution path, only a template that actually contains the tokens.
//! Because the artifact is an npm tarball rather than a standalone
//! executable, provisioning does not stop at a verified download:
//! [`ensure_adapter`] also extracts it into a versioned cache directory and
//! returns the path to the package's own declared entry script (the
//! registry's `bin` field, `dist/index.js`, prefixed with the npm tarball's
//! own `package/` root), which [`crate::ClaudeCodeAdapter::provisioned`]
//! then runs under a `node` resolved from `PATH`.
//!
//! An npm tarball carries a package's own files and none of the packages it
//! imports, so extraction alone produces an entry script `node` refuses to
//! load (`ERR_MODULE_NOT_FOUND` on the first bare import). The declared
//! runtime dependencies are therefore installed into the extraction before
//! it is published -- from a lockfile pinned beside the row, never resolved
//! fresh. That is the same guarantee the tarball checksum gives, extended
//! to everything `node` will actually execute: a resolved-at-provision-time
//! tree is a hundred packages whose bytes nothing verified, running as the
//! user in the user's own project on every session, under a pin that
//! covered only the wrapper around them. `npm ci` installs exactly the
//! lockfile's tree and verifies every package against the `integrity` hash
//! recorded in it, so a tampered or moved dependency fails the install
//! instead of being launched. `--ignore-scripts` keeps that tree from
//! executing install hooks on the way in, and `--omit=dev` leaves a test
//! and build toolchain the adapter never runs off the machine entirely.
//!
//! The pinned lockfile is captured once, at pin time, from the pinned
//! tarball itself:
//!
//! ```text
//! tar xzf claude-agent-acp-0.69.0.tgz
//! cd package && npm install --package-lock-only --omit=dev --ignore-scripts
//! # -> crates/view-ai/adapters/claude-code-0.69.0-package-lock.json
//! ```

use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::trust::env_dir;

// `ProvisionError` follows `TrustError`/`AiConfigError`'s own size
// discipline: a variant growing past `clippy::result_large_err`'s 128-byte
// threshold is a lint failure here too, not a review note.
const _: () = assert!(std::mem::size_of::<ProvisionError>() <= 128);

/// Everything that can stop [`ensure_adapter`] from returning a verified,
/// runnable path.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    /// `id` names no row in the compile-time adapter table.
    #[error("no adapter named `{id}` is known to this build")]
    UnknownAdapter {
        /// The id that was looked up.
        id: String,
    },
    /// The download itself failed -- a network error, a non-2xx status, or
    /// a malformed response. Boxed for the same reason `AiConfigError::Toml`
    /// boxes its `toml::de::Error`: `ureq::Error` is well over the 128-byte
    /// large-error threshold on its own.
    #[error("could not download {url}: {source}")]
    Download {
        /// The URL that was requested.
        url: String,
        /// The underlying `ureq` error.
        source: Box<ureq::Error>,
    },
    /// The downloaded (or previously cached) tarball bytes do not hash to
    /// the pinned checksum -- a corrupted transfer, a tampered cache file,
    /// or a release that moved out from under its pin. Never launched:
    /// this is the one variant [`ensure_adapter`]'s own doc calls a hard
    /// error, not a warning.
    #[error(
        "adapter `{id}` failed checksum verification: expected {expected}, got {actual} \
         (cache path: {}) -- delete that file to force a clean re-download; if the mismatch \
         recurs against a fresh download, the release has moved out from under its pin and must \
         be re-pinned against a freshly captured checksum, not worked around",
        cache_path.display()
    )]
    ChecksumMismatch {
        /// The adapter id being provisioned.
        id: String,
        /// The checksum the pin expects.
        expected: String,
        /// The checksum the bytes actually hashed to.
        actual: String,
        /// Where the tarball is (or would be) cached.
        cache_path: PathBuf,
    },
    /// The tarball unpacked without error, but the pin's own declared
    /// [`AdapterPin::entry`] was not among the extracted files -- the
    /// release's own layout no longer matches what this row expects.
    #[error("adapter `{id}` extracted with no entry file at `{entry}`")]
    EntryMissing {
        /// The adapter id being provisioned.
        id: String,
        /// The entry path the pin declared.
        entry: String,
    },
    /// The tarball could not be unpacked as a gzip'd tar archive.
    #[error("could not extract {path}: {source}")]
    Extract {
        /// The destination directory extraction was attempted into.
        path: PathBuf,
        /// The underlying I/O error `tar`/`flate2` reported.
        source: std::io::Error,
    },
    /// The cache directory could not be created, or a verified download or
    /// extraction could not be written into it.
    #[error("could not write {path}: {source}")]
    Write {
        /// The path that failed to write.
        path: PathBuf,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// No platform cache directory could be resolved (`$XDG_CACHE_HOME`,
    /// `$HOME`, and `%LOCALAPPDATA%` all absent).
    #[error("could not resolve a platform cache directory to provision AI adapters into")]
    NoCacheDir,
    /// [`resolve_node`] found no `node`/`node.exe` on `PATH`. Surfaced
    /// before a spawn is ever attempted: an adapter whose entry script is
    /// JavaScript cannot run without an interpreter, and failing here says
    /// so directly instead of letting the eventual `Command::new("node")`
    /// fail with a bare "not found".
    #[error(
        "node was not found on PATH; the claude-code adapter runs under Node.js -- install \
         Node.js (https://nodejs.org) and ensure `node` is on PATH, then retry"
    )]
    NodeNotFound,
    /// The extracted package declares runtime dependencies and `npm` was not
    /// on `PATH` to install them. Its own variant rather than a reuse of
    /// [`ProvisionError::NodeNotFound`]: a host can carry a `node` from a
    /// tarball or a version manager shim with no `npm` beside it, and
    /// telling that host to install Node.js again is a remedy that does
    /// nothing.
    #[error(
        "npm was not found on PATH; the claude-code adapter's own dependencies are installed \
         with it -- ensure the `npm` that ships with Node.js is on PATH, then retry"
    )]
    NpmNotFound,
    /// The pinned lockfile and the pin itself disagree -- a row whose
    /// package declares dependencies with no lockfile beside it, or a
    /// lockfile locking a different release than the row names. Refused
    /// rather than installed from: a lockfile that does not belong to the
    /// pinned tarball pins nothing about what this build will run.
    #[error(
        "adapter `{id}`'s pinned lockfile does not match its pin: {detail} -- re-capture the \
         lockfile from the pinned tarball (see this module's own header), since installing \
         from a disagreeing lockfile would run packages the pin never covered"
    )]
    LockfileMismatch {
        /// The adapter id being provisioned.
        id: String,
        /// How the two disagree.
        detail: String,
    },
    /// The dependency install ran and refused, or outlived its own bound.
    /// Carries what npm itself said, because the causes (an offline host, a
    /// registry that requires a proxy, a package whose bytes no longer hash
    /// to the lockfile's recorded `integrity`) are distinguishable only
    /// from its own output.
    #[error("could not install adapter `{id}`'s dependencies: {detail}")]
    DependencyInstall {
        /// The adapter id being provisioned.
        id: String,
        /// npm's own report, trimmed to its tail.
        detail: String,
    },
}

/// One entry in the pinned-adapter manifest -- a compile-time table on
/// `registry.rs`'s own precedent (closed at compile time, not runtime-
/// registered), scoped to adapters this build knows how to provision.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct AdapterPin {
    /// The stable id [`ensure_adapter`] is called with, and the same id
    /// [`AgentAdapter::id`](crate::AgentAdapter::id) reports once a session
    /// is running -- the two must never drift, since a caller that
    /// provisioned one id and then launched a different one would silently
    /// run a binary its trust and diagnostics never asked for.
    pub id: &'static str,
    /// The pinned release version, reported by
    /// [`ClaudeCodeAdapter::pinned_version`](crate::ClaudeCodeAdapter::pinned_version)
    /// once built from this row.
    pub version: &'static str,
    /// The download URL, with `{os}`/`{arch}` substituted for
    /// [`std::env::consts::OS`]/[`std::env::consts::ARCH`] at resolve
    /// time. A row whose real release is platform-independent (this
    /// crate's one row today) simply carries no such tokens; substitution
    /// on a literal URL is a no-op, not a special case this code has to
    /// know about. Enforced `https://` at the table -- see the `const`
    /// assertion below `ADAPTERS`.
    pub url_template: &'static str,
    /// The expected SHA-256 of the downloaded tarball bytes, lowercase hex.
    pub sha256: &'static str,
    /// The registry-declared entry point inside the extracted tarball --
    /// for an npm package, its own `package/` root plus the `bin` field's
    /// relative path (`package/dist/index.js` for `claude-code`).
    /// [`ensure_adapter`] resolves this against the extraction directory
    /// and returns that path.
    pub entry: &'static str,
    /// The `package-lock.json` captured against this exact release, embedded
    /// at compile time and written into the extraction before the install
    /// runs. `None` only for a row whose package declares no runtime
    /// dependencies at all -- a row that declares them and carries no
    /// lockfile is [`ProvisionError::LockfileMismatch`], never a fresh
    /// resolve, since the whole point of the row's checksum is that nothing
    /// unverified runs.
    pub lockfile: Option<&'static str>,
}

// This build knows how to provision exactly one adapter, matching
// `AiConfig::default`'s own claim ("the one adapter this build knows how to
// provision on its own"). A second row is what a second pinned adapter
// looks like; nothing here assumes there is only ever one.
static ADAPTERS: [AdapterPin; 1] = [AdapterPin {
    id: "claude-code",
    version: "0.69.0",
    url_template: "https://registry.npmjs.org/@agentclientprotocol/claude-agent-acp/-/claude-agent-acp-0.69.0.tgz",
    sha256: "73334255e17f5f48f08030fa4e0c54c118e820f9aaaf29f4629aa230e48c65c2",
    entry: "package/dist/index.js",
    lockfile: Some(include_str!(
        "../adapters/claude-code-0.69.0-package-lock.json"
    )),
}];

/// `url` starts with `https://`, evaluated at compile time so the check
/// below can run inside a `const` block.
const fn url_is_https(url: &str) -> bool {
    let bytes = url.as_bytes();
    let prefix = b"https://";
    if bytes.len() < prefix.len() {
        return false;
    }
    let mut i = 0;
    while i < prefix.len() {
        if bytes[i] != prefix[i] {
            return false;
        }
        i += 1;
    }
    true
}

// Enforced at the table, not inside `resolved_url`/`download`: the tests
// legitimately serve `http://` from a loopback stub server through the
// same `resolve_in` production code uses, so a runtime check on every
// `AdapterPin` would have to special-case test pins. A compile-time table
// assertion applies only to what this build actually ships -- a maintainer
// who pins an `http://` URL by mistake gets a build failure, not a
// cleartext download of a binary this crate is about to run.
const _: () = {
    let mut i = 0;
    while i < ADAPTERS.len() {
        assert!(
            url_is_https(ADAPTERS[i].url_template),
            "AdapterPin::url_template must be an https:// URL"
        );
        i += 1;
    }
};

/// The compile-time row for `id`, or `None` if this build has no pin for
/// it.
fn lookup(id: &str) -> Option<&'static AdapterPin> {
    ADAPTERS.iter().find(|pin| pin.id == id)
}

/// The pinned version `ensure_adapter(id)` would provision, without
/// touching the network or the filesystem -- the metadata half of the seam
/// [`ClaudeCodeAdapter::provisioned`](crate::ClaudeCodeAdapter::provisioned)
/// needs, so its `pinned_version` field is read from the same row
/// `ensure_adapter` resolves against rather than a second, hand-typed
/// literal that could drift from it.
#[must_use]
pub fn pinned_version(id: &str) -> Option<&'static str> {
    lookup(id).map(|pin| pin.version)
}

/// Resolves `id` to a local, verified, extracted entry script path: returns
/// the cached path if already downloaded, checksum-verified, and extracted,
/// otherwise downloads, verifies, extracts, and caches under the platform
/// cache dir before returning. Never returns a path backed by a tarball
/// whose checksum doesn't match, or by an extraction that no longer matches
/// what was extracted -- a corrupted or tampered download, or a corrupted
/// extracted tree, is a hard error or a silent re-extraction, never a
/// blindly trusted stale path.
///
/// # Errors
///
/// Returns [`ProvisionError::UnknownAdapter`] if `id` names no compile-time
/// row, [`ProvisionError::NoCacheDir`] if no platform cache directory can
/// be resolved, [`ProvisionError::Download`] if the network request fails,
/// [`ProvisionError::ChecksumMismatch`] if the downloaded (or a corrupted
/// cached) tarball's hash does not match the pin,
/// [`ProvisionError::Extract`] if the tarball cannot be unpacked,
/// [`ProvisionError::EntryMissing`] if the pin's declared entry is absent
/// from the extracted tree, and [`ProvisionError::Write`] if the verified
/// bytes cannot be cached to disk.
pub fn ensure_adapter(id: &str) -> Result<PathBuf, ProvisionError> {
    let pin = lookup(id).ok_or_else(|| ProvisionError::UnknownAdapter { id: id.to_string() })?;
    resolve(pin)
}

/// Whether `id` is already provisioned on this machine: a complete,
/// self-consistent extraction with its dependencies beside it, so
/// [`ensure_adapter`] would return without downloading or installing
/// anything.
///
/// For a caller that owes the user a word before a wait, not a
/// precondition for calling [`ensure_adapter`] -- which re-checks
/// everything this reads and does the work when the answer is `false`.
/// Answering `false` for an unknown id or an unresolvable cache directory
/// is deliberate: both are cases where a session is about to fail or stall,
/// and neither is a case where staying silent helps.
#[must_use]
pub fn adapter_is_ready(id: &str) -> bool {
    let Some(pin) = lookup(id) else {
        return false;
    };
    let Ok(root) = cache_root() else {
        return false;
    };
    ready_in(pin, &root)
}

/// [`adapter_is_ready`]'s mechanism, parameterized on `cache_root` for the
/// same reason [`resolve_in`] is: one implementation, exercised by the
/// tests against a scratch directory rather than process-global state.
fn ready_in(pin: &AdapterPin, cache_root: &Path) -> bool {
    let extract_dir = extract_dir(&pin_dir(pin, cache_root));
    extraction_is_valid(pin, &extract_dir.join(pin.entry), &extract_dir)
}

/// `node`/`node.exe` resolved from `PATH`, or
/// [`ProvisionError::NodeNotFound`] with a remedy. `pub(crate)`, not
/// public: this is specific to launching a JavaScript entry script the way
/// [`crate::ClaudeCodeAdapter::provisioned`] needs to, not a general "find
/// an interpreter" utility this module means to expose.
pub(crate) fn resolve_node() -> Result<PathBuf, ProvisionError> {
    let exe_name = if cfg!(windows) { "node.exe" } else { "node" };
    on_path(exe_name).ok_or(ProvisionError::NodeNotFound)
}

/// `npm`/`npm.cmd` resolved from `PATH`. On Windows npm is a batch shim
/// rather than an executable, which is why the name it is looked up under
/// differs from the plain `.exe` `resolve_node` wants.
fn resolve_npm() -> Result<PathBuf, ProvisionError> {
    let exe_name = if cfg!(windows) { "npm.cmd" } else { "npm" };
    on_path(exe_name).ok_or(ProvisionError::NpmNotFound)
}

/// The first `exe_name` on `PATH`, if any.
fn on_path(exe_name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join(exe_name))
        .find(|candidate| candidate.is_file())
}

/// `ensure_adapter`'s production entry point into the mechanism: resolves
/// the real platform cache directory and defers everything else to
/// [`resolve_in`], the one implementation the tests exercise directly
/// (with an explicit cache root) instead of a parallel copy of it -- there
/// is exactly one download/verify/extract/cache code path in this module,
/// and both production and tests run it.
fn resolve(pin: &AdapterPin) -> Result<PathBuf, ProvisionError> {
    resolve_in(pin, &cache_root()?)
}

/// The full mechanism, parameterized on `cache_root` so tests can point it
/// at a scratch directory instead of mutating process-global environment
/// state: builds the pin's own cache directory, ensures a checksum-verified
/// tarball is present there, ensures it is extracted (re-extracting from
/// the verified tarball if the extracted tree is missing or has been
/// tampered with since), and returns the extracted entry path.
fn resolve_in(pin: &AdapterPin, cache_root: &Path) -> Result<PathBuf, ProvisionError> {
    let dir = pin_dir(pin, cache_root);
    std::fs::create_dir_all(&dir).map_err(|source| ProvisionError::Write {
        path: dir.clone(),
        source,
    })?;

    let tarball = tarball_path(pin, &dir);
    ensure_tarball(pin, &tarball)?;

    let extract_dir = extract_dir(&dir);
    ensure_extracted(pin, &tarball, &extract_dir)
}

/// Ensures a checksum-verified copy of `pin`'s tarball exists at
/// `tarball_path`, downloading it if absent or if the file on disk no
/// longer verifies.
fn ensure_tarball(pin: &AdapterPin, tarball_path: &Path) -> Result<(), ProvisionError> {
    // A cache hit is only ever a hit after re-verifying the bytes on disk:
    // between two calls, the file on disk could have been partially
    // written by a crash or tampered with externally, and trusting its
    // mere presence would extract exactly the corrupted-or-tampered
    // tarball this whole module exists to refuse.
    if let Ok(existing) = std::fs::read(tarball_path) {
        if sha256_hex(&existing) == pin.sha256 {
            return Ok(());
        }
    }

    let bytes = download(pin)?;
    let actual = sha256_hex(&bytes);
    if actual != pin.sha256 {
        // Nothing is written to the cache dir on a mismatch: verification
        // happens against the in-memory buffer, before any file at
        // `tarball_path` is ever touched, so a tampered payload can never
        // leave a partial-write-then-trust artifact behind for the next
        // call to find.
        return Err(ProvisionError::ChecksumMismatch {
            id: pin.id.to_string(),
            expected: pin.sha256.to_string(),
            actual,
            cache_path: tarball_path.to_path_buf(),
        });
    }

    // 0o644: the cached tarball is data, never executed directly -- the
    // extraction step below is what produces anything that runs, and even
    // then only `node` itself is exec'd; the entry script is read by
    // `node`, not exec'd on its own.
    write_atomically(tarball_path, &bytes)
}

/// Ensures `pin`'s tarball at `tarball_path` is extracted under
/// `extract_dir`, with its declared [`AdapterPin::entry`] present and
/// self-consistent, re-extracting from the (already checksum-verified)
/// tarball if the extracted tree is missing, was never fully written, or
/// was modified since. Returns the entry's own path.
fn ensure_extracted(
    pin: &AdapterPin,
    tarball_path: &Path,
    extract_dir: &Path,
) -> Result<PathBuf, ProvisionError> {
    let entry_path = extract_dir.join(pin.entry);
    if extraction_is_valid(pin, &entry_path, extract_dir) {
        return Ok(entry_path);
    }

    let tarball_bytes = std::fs::read(tarball_path).map_err(|source| ProvisionError::Write {
        path: tarball_path.to_path_buf(),
        source,
    })?;

    let parent = extract_dir.parent().unwrap_or_else(|| Path::new("."));
    static TMP_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = TMP_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_dir = parent.join(format!(".extracted.{}.{nonce}.tmp", std::process::id()));
    // a leftover from a killed process, not a live extraction in progress
    // (the nonce makes every call's own name unique) -- safe to clear
    // before reusing the name
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).map_err(|source| ProvisionError::Write {
        path: tmp_dir.clone(),
        source,
    })?;

    if let Err(err) = extract_tarball(&tarball_bytes, &tmp_dir) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(err);
    }
    #[cfg(unix)]
    if let Err(err) = normalize_modes(&tmp_dir) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(err);
    }

    // Before the stamp, so a failed install never publishes: the stamp is
    // what a later call reads as "this extraction is finished".
    if let Err(err) = ensure_dependencies(pin, &tmp_dir) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(err);
    }

    let tmp_entry = tmp_dir.join(pin.entry);
    let entry_bytes = match std::fs::read(&tmp_entry) {
        Ok(bytes) => bytes,
        Err(_) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return Err(ProvisionError::EntryMissing {
                id: pin.id.to_string(),
                entry: pin.entry.to_string(),
            });
        }
    };
    if let Err(err) = std::fs::write(
        tmp_dir.join(ENTRY_STAMP_NAME),
        entry_stamp(pin, &entry_bytes),
    ) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(ProvisionError::Write {
            path: tmp_dir.join(ENTRY_STAMP_NAME),
            source: err,
        });
    }

    // Directory rename cannot atomically replace a non-empty destination
    // on any platform this crate ships on, so a stale (or corrupted)
    // `extract_dir` is removed first. That pair is not atomic against a
    // crash -- or another process -- between the two calls; `publish_extraction`
    // is what absorbs that gap.
    let _ = std::fs::remove_dir_all(extract_dir);
    publish_extraction(pin, &tmp_dir, extract_dir, &entry_path)
}

/// The extracted package's own root: the first component of the pin's
/// declared entry, which for an npm tarball is the archive's `package/`
/// prefix -- the directory holding the `package.json` whose `dependencies`
/// the entry script's bare imports resolve against.
fn package_root(pin: &AdapterPin, extract_dir: &Path) -> Option<PathBuf> {
    let first = Path::new(pin.entry).components().next()?;
    Some(extract_dir.join(first.as_os_str()))
}

/// Whether `package_root`'s manifest declares at least one runtime
/// dependency. A package with none (every test pin in this module, and any
/// future row that ships a self-contained bundle) needs no install step at
/// all, so nothing is spawned for it.
fn declares_dependencies(package_root: &Path) -> bool {
    let Ok(manifest) = std::fs::read_to_string(package_root.join("package.json")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&manifest) else {
        return false;
    };
    value
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|deps| !deps.is_empty())
}

/// The lockfile name npm itself reads, written into the extraction from
/// [`AdapterPin::lockfile`] so `npm ci` has the tree it must reproduce.
const LOCKFILE_NAME: &str = "package-lock.json";

/// How long a dependency install is given before it is stopped. Generous
/// against a slow link and a cold npm cache, and finite against the case
/// the bound exists for: a registry (or a proxy in front of one) that
/// accepts the connection and then never answers, which `Command::output`
/// would wait on for the rest of the process's life with the panel sitting
/// on a turn that never starts.
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// How often the install is checked for having finished. Small next to
/// [`INSTALL_TIMEOUT`] and charged only to a provisioning run, never to a
/// session that starts from a complete cache.
const INSTALL_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Installs the extracted package's declared runtime dependencies in place,
/// so the entry script `node` is handed can actually resolve its imports.
///
/// From the pin's own lockfile, via `npm ci` -- see this module's header for
/// why a fresh resolve is not an option here.
fn ensure_dependencies(pin: &AdapterPin, extract_dir: &Path) -> Result<(), ProvisionError> {
    let Some(root) = package_root(pin, extract_dir) else {
        return Ok(());
    };
    if !declares_dependencies(&root) {
        return Ok(());
    }
    let lockfile = pinned_lockfile(pin)?;
    let lock_path = root.join(LOCKFILE_NAME);
    std::fs::write(&lock_path, lockfile).map_err(|source| ProvisionError::Write {
        path: lock_path,
        source,
    })?;
    let npm = resolve_npm()?;
    run_install(&npm, &root).map_err(|detail| ProvisionError::DependencyInstall {
        id: pin.id.to_string(),
        detail,
    })
}

/// The pin's own lockfile, refused unless it locks the release the pin
/// names. `npm ci` catches the deeper disagreement on its own (a lockfile
/// out of sync with the `package.json` beside it is a hard error there,
/// never a re-resolve); this catches the shallower one a lockfile copied
/// from the wrong release would present, where both files parse and agree
/// with each other while describing something the checksum never covered.
fn pinned_lockfile(pin: &AdapterPin) -> Result<&'static str, ProvisionError> {
    let mismatch = |detail: String| ProvisionError::LockfileMismatch {
        id: pin.id.to_string(),
        detail,
    };
    let Some(lockfile) = pin.lockfile else {
        return Err(mismatch(
            "the package declares runtime dependencies and the row carries no lockfile".to_string(),
        ));
    };
    let locked = serde_json::from_str::<serde_json::Value>(lockfile)
        .ok()
        .and_then(|value| {
            value
                .get("version")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    match locked {
        Some(version) if version == pin.version => Ok(lockfile),
        Some(version) => Err(mismatch(format!(
            "it locks version {version}, the row pins {}",
            pin.version
        ))),
        None => Err(mismatch(
            "it parses as no lockfile with a root version in it".to_string(),
        )),
    }
}

/// Runs the lockfile's install in `root`, bounded by [`INSTALL_TIMEOUT`].
/// `Err` carries npm's own report, tail-trimmed: the whole log of a failed
/// install is thousands of lines of tree output, and the cause is at the
/// end of it.
fn run_install(npm: &Path, root: &Path) -> Result<(), String> {
    let mut child = std::process::Command::new(npm)
        .args([
            "ci",
            "--omit=dev",
            "--ignore-scripts",
            "--no-audit",
            "--no-fund",
        ])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| err.to_string())?;
    // Drained on its own thread rather than after the wait: a failing
    // install writes far more than a pipe buffer holds, and a parent that
    // only watched for exit would block the child on a full pipe until the
    // deadline stopped a job that had already done its work.
    let mut pipe = child.stderr.take();
    let drain = std::thread::Builder::new()
        .name("adapter-install".to_string())
        .spawn(move || {
            let mut text = String::new();
            if let Some(pipe) = pipe.as_mut() {
                let _ = std::io::Read::read_to_string(pipe, &mut text);
            }
            text
        })
        .map_err(|err| err.to_string())?;
    let deadline = std::time::Instant::now() + INSTALL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Err(err) => return Err(err.to_string()),
            Ok(Some(status)) => break status,
            Ok(None) => {}
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "npm ci was still running after {}s and was stopped",
                INSTALL_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(INSTALL_POLL);
    };
    let reported = drain.join().unwrap_or_default();
    if status.success() {
        return Ok(());
    }
    let tail: Vec<&str> = reported.lines().rev().take(8).collect();
    let detail = tail.into_iter().rev().collect::<Vec<_>>().join("; ");
    if detail.trim().is_empty() {
        Err(format!("npm ci exited with {status}"))
    } else {
        Err(detail)
    }
}

/// Publishes a completed extraction at `tmp_dir` into `extract_dir` via
/// rename. A concurrent `view` process provisioning the same adapter can
/// win the same race `ensure_extracted` runs: both remove the stale
/// `extract_dir`, both extract into their own temp dir, and whichever
/// renames second hits a now-repopulated destination (`ENOTEMPTY` on
/// Linux, `rename()` can never atomically replace a non-empty directory on
/// any mainstream OS regardless of privilege). That is not corruption --
/// it is someone else's already-valid extraction -- so a rename failure is
/// only a hard error if `extract_dir` is *still* invalid afterward;
/// otherwise the loser discards its own (unused) temp dir and defers to
/// the winner.
fn publish_extraction(
    pin: &AdapterPin,
    tmp_dir: &Path,
    extract_dir: &Path,
    entry_path: &Path,
) -> Result<PathBuf, ProvisionError> {
    if let Err(source) = std::fs::rename(tmp_dir, extract_dir) {
        let _ = std::fs::remove_dir_all(tmp_dir);
        if extraction_is_valid(pin, entry_path, extract_dir) {
            return Ok(entry_path.to_path_buf());
        }
        return Err(ProvisionError::Write {
            path: extract_dir.to_path_buf(),
            source,
        });
    }

    Ok(entry_path.to_path_buf())
}

/// The extracted entry stamp's file name, holding what [`entry_stamp`]
/// records at extraction time so a later call can detect the entry having
/// been modified on disk since -- deliberately scoped to the one file that
/// is ever read as this adapter's launchable script plus the lockfile its
/// dependencies came from, not a hash of the whole extracted tree.
const ENTRY_STAMP_NAME: &str = ".entry-sha256";

/// What [`ENTRY_STAMP_NAME`] records for `pin`: the entry's own hash, and
/// for a row that pins a lockfile the hash of that lockfile too.
///
/// The lockfile half is what makes an already-installed tree answer the
/// question the tarball checksum cannot: which dependencies are sitting
/// beside the entry. A tree installed from a different lockfile -- or by
/// an earlier build that resolved dependencies freshly, with nothing
/// verifying what it fetched -- stamps differently and so reads as
/// invalid, which re-extracts and re-installs it from the pin.
fn entry_stamp(pin: &AdapterPin, entry_bytes: &[u8]) -> String {
    let entry = sha256_hex(entry_bytes);
    match pin.lockfile {
        Some(lockfile) => format!("{entry}-{}", sha256_hex(lockfile.as_bytes())),
        None => entry,
    }
}

/// Whether `entry_path` (under `extract_dir`) still matches the stamp left
/// at extraction time -- `false` for a missing extraction, a missing
/// entry, a missing or unreadable stamp, or an entry whose bytes -- or
/// whose pinned lockfile -- no longer hash to what the stamp recorded.
///
/// A package that declares dependencies is additionally only valid with a
/// `node_modules` beside it. The stamp covers the entry file alone, so an
/// extraction left behind by a build that did not install them -- or one
/// whose `node_modules` was removed from under it -- hashes as intact while
/// being a script `node` cannot load; re-extracting is what fixes it. The
/// check is presence, not contents: this module's own writes publish a
/// complete tree or none at all (`ensure_extracted` installs into the temp
/// dir before the stamp, and `publish_extraction` renames), so the only way
/// to a half-emptied `node_modules` is an external hand, and the loud
/// failure that hand earns is `node`'s own module-resolution error rather
/// than a full tree walk on every session start.
fn extraction_is_valid(pin: &AdapterPin, entry_path: &Path, extract_dir: &Path) -> bool {
    let Ok(entry_bytes) = std::fs::read(entry_path) else {
        return false;
    };
    let Ok(stamp) = std::fs::read_to_string(extract_dir.join(ENTRY_STAMP_NAME)) else {
        return false;
    };
    if stamp.trim() != entry_stamp(pin, &entry_bytes) {
        return false;
    }
    match package_root(pin, extract_dir) {
        Some(root) => !declares_dependencies(&root) || root.join("node_modules").is_dir(),
        None => true,
    }
}

/// Unpacks a gzip'd tar archive's bytes into `dest`, which must already
/// exist. `tar`+`flate2` rather than a hand-rolled reader: both are the
/// maintained, widely used combination for exactly this format in the Rust
/// ecosystem (the same pairing `cargo` itself vendors for crate source
/// extraction), and re-deriving tar's header parsing or gzip's DEFLATE
/// decoding here would trade an audited implementation for an unaudited
/// one to save two small dependencies.
fn extract_tarball(bytes: &[u8], dest: &Path) -> Result<(), ProvisionError> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    archive
        .unpack(dest)
        .map_err(|source| ProvisionError::Extract {
            path: dest.to_path_buf(),
            source,
        })
}

/// Sets every extracted file to `0o644` and every extracted directory to
/// `0o755`, overriding whatever mode bits the archive itself declared:
/// nothing in this module's own launch path ever executes an extracted
/// file directly (`node` runs the entry script; only `node` itself is
/// exec'd), so no extracted file needs an exec bit, and trusting an
/// archive's own mode bits would let a tampered or unusual tarball hand
/// out permissions this code never asked for. Directories need their own
/// execute bit for traversal, which read/write alone does not grant.
#[cfg(unix)]
fn normalize_modes(dir: &Path) -> Result<(), ProvisionError> {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(dir).map_err(|source| ProvisionError::Write {
        path: dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| ProvisionError::Write {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| ProvisionError::Write {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(
                |source| ProvisionError::Write {
                    path: path.clone(),
                    source,
                },
            )?;
            normalize_modes(&path)?;
        } else if file_type.is_file() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).map_err(
                |source| ProvisionError::Write {
                    path: path.clone(),
                    source,
                },
            )?;
        }
    }
    Ok(())
}

/// `{os}`/`{arch}` substituted into `pin.url_template` for the platform
/// this process is running on.
fn resolved_url(pin: &AdapterPin) -> String {
    pin.url_template
        .replace("{os}", std::env::consts::OS)
        .replace("{arch}", std::env::consts::ARCH)
}

/// Downloads `pin`'s resolved URL in full, bounded to 64MiB -- generous
/// against the 161KB real `claude-code` tarball this table pins today,
/// while still refusing to buffer an unbounded response into memory should
/// a future row point at something far larger than expected.
fn download(pin: &AdapterPin) -> Result<Vec<u8>, ProvisionError> {
    let url = resolved_url(pin);
    let mut response = ureq::get(&url)
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .call()
        .map_err(|source| ProvisionError::Download {
            url: url.clone(),
            source: Box::new(source),
        })?;
    response
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_vec()
        .map_err(|source| ProvisionError::Download {
            url,
            source: Box::new(source),
        })
}

/// The cache directory a pin's tarball and extraction live under:
/// `<platform cache dir>/view/adapters/<id>/<version>`. Scoped by version
/// so bumping a pin's `version` field provisions fresh rather than
/// colliding with (or being shadowed by) an older cached download.
fn pin_dir(pin: &AdapterPin, cache_root: &Path) -> PathBuf {
    cache_root
        .join("view")
        .join("adapters")
        .join(pin.id)
        .join(pin.version)
}

/// The tarball's own path within `dir` (a pin's [`pin_dir`]): the resolved
/// URL's last path segment, or the adapter id if the URL ends in a
/// trailing slash or has none -- the name only needs to be stable and
/// collision-free within `dir`, not meaningful on its own.
fn tarball_path(pin: &AdapterPin, dir: &Path) -> PathBuf {
    dir.join(cache_file_name(pin))
}

/// The extraction directory within `dir` (a pin's [`pin_dir`]).
fn extract_dir(dir: &Path) -> PathBuf {
    dir.join("extracted")
}

/// The platform cache directory: `$XDG_CACHE_HOME`, falling back to the
/// unix convention (`~/.cache`) or the Windows convention
/// (`%LOCALAPPDATA%`, which doubles as both the cache and state root
/// there -- the same fallback [`crate::trust::store_path`] resolves its own
/// state directory through, reused via `env_dir` rather than duplicated).
fn cache_root() -> Result<PathBuf, ProvisionError> {
    env_dir("XDG_CACHE_HOME")
        .or_else(|| env_dir("HOME").map(|h| h.join(".cache")))
        .or_else(|| env_dir("LOCALAPPDATA"))
        .ok_or(ProvisionError::NoCacheDir)
}

fn cache_file_name(pin: &AdapterPin) -> String {
    let url = resolved_url(pin);
    let name = url.rsplit('/').next().unwrap_or_default();
    if name.is_empty() {
        pin.id.to_string()
    } else {
        name.to_string()
    }
}

/// Lowercase hex SHA-256 of `bytes`, in the same form `AdapterPin::sha256`
/// is written in.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes `bytes` to `path` atomically as a data file (`0o644`): a temp
/// file in the same directory, mode-bit'd before the rename rather than
/// after, then renamed over `path`. The same temp+rename shape
/// [`crate::trust::persist`] uses for the trust store, for the same reason
/// -- a crash mid-write must never leave a half-written file for the next
/// call's cache-hit check to trust.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), ProvisionError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("adapter");
    // pid+nonce, not pid alone: `trust::persist`'s own comment on why
    // (two concurrent writes in one process would otherwise collide) holds
    // here identically.
    static TMP_NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = TMP_NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.{}.{nonce}.tmp", std::process::id()));
    // Best-effort cleanup on every failure branch below: a mid-write
    // failure must not leave a `.tmp` file sitting in a directory users do
    // look in. `trust::persist` has the identical gap (no cleanup on its
    // own write/chmod failure paths); recorded here rather than changed
    // there, since nothing about that function needed to move for this.
    let result = write_atomically_inner(&tmp_path, path, bytes);
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    result
}

fn write_atomically_inner(
    tmp_path: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<(), ProvisionError> {
    let mut file = std::fs::File::create(tmp_path).map_err(|source| ProvisionError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(bytes)
        .map_err(|source| ProvisionError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp_path, std::fs::Permissions::from_mode(0o644)).map_err(
            |source| ProvisionError::Write {
                path: path.to_path_buf(),
                source,
            },
        )?;
    }
    std::fs::rename(tmp_path, path).map_err(|source| ProvisionError::Write {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)]

    use std::io::Read as _;
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    /// A minimal, single-purpose HTTP/1.1 server for these tests: accepts
    /// a connection, discards whatever request it sent, and answers every
    /// request with the same canned body. `std`-only rather than a stub
    /// server crate: this module's new dependencies are the HTTP client and
    /// the tar/gzip readers under test, and a real client exercised against
    /// a from-scratch listener is a more faithful RED-phase disconfirm than
    /// mocking the client itself would be.
    struct StubServer {
        addr: SocketAddr,
        requests: Arc<AtomicUsize>,
    }

    impl StubServer {
        fn start(body: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub listener");
            let addr = listener.local_addr().expect("stub listener addr");
            let requests = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&requests);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    counter.fetch_add(1, Ordering::SeqCst);
                    let mut buf = [0_u8; 4096];
                    let _ = stream.read(&mut buf);
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&body);
                }
            });
            Self { addr, requests }
        }

        fn url(&self) -> String {
            format!("http://{}/adapter.tgz", self.addr)
        }

        fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    /// A scratch cache root under the workspace's own `target/tmp`,
    /// nonce-tagged so parallel test binaries never collide, on the same
    /// terms `view_test_support::ScratchDir` provides -- used directly
    /// here (rather than pointing `XDG_CACHE_HOME` at one) because
    /// `resolve_in` is exercised with an explicit cache root, not through
    /// the real `XDG_CACHE_HOME` fallback chain that `trust.rs`'s own
    /// tests cover for `env_dir` already.
    fn scratch_cache_root(nonce: &str) -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!(
                "view-ai-provision-{}-{}",
                std::process::id(),
                nonce
            ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch cache root");
        dir
    }

    fn test_pin(url: String, sha256: &'static str) -> AdapterPin {
        AdapterPin {
            id: "stub-adapter",
            version: "0.0.0-test",
            // leaked deliberately: `AdapterPin::url_template` is
            // `&'static str`, and these tests build one per stub server
            // instance, which only ever exists for the duration of one
            // test -- an intentional, bounded leak, not a growing one.
            url_template: Box::leak(url.into_boxed_str()),
            sha256,
            entry: "package/dist/index.js",
            lockfile: None,
        }
    }

    /// A real, valid `.tar.gz` containing exactly one file at
    /// `package/dist/index.js` holding `content` -- built with the same
    /// `tar`+`flate2` this module uses to read one, so the tests exercise
    /// genuine extraction rather than a payload extraction happens to
    /// accept.
    fn build_test_tarball(content: &[u8]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "package/dist/index.js", content)
                .expect("append tar entry");
            builder.finish().expect("finish tar");
        }
        let mut gz_bytes = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_bytes, flate2::Compression::default());
            encoder.write_all(&tar_bytes).expect("gzip tar bytes");
            encoder.finish().expect("finish gzip");
        }
        gz_bytes
    }

    /// A malicious `.tar.gz` combining the three classic tar-extraction
    /// escape vectors in one archive, in order: a `../`-relative member, an
    /// absolute-path member, and a symlinked directory later written
    /// through. Built to pin `extract_tarball`'s currently-safe behavior --
    /// the `../` member is dropped outright (no file, no error), the
    /// absolute member has its leading `/` stripped and is relocated under
    /// the destination rather than skipped, and the symlink write-through
    /// is the one hard error -- so a future `tar` upgrade that changed any
    /// of the three turns a test red instead of silently reopening an
    /// escape.
    fn build_escaping_tarball(symlink_target: &Path) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            // A hand-crafted malicious archive never goes through this
            // crate's own `Header::set_path` -- it arrives as arbitrary
            // bytes over the wire. `tar`'s safe builder API refuses to
            // *construct* a `..` name at all (the same validation this
            // test exists to make sure `unpack` also enforces), so the
            // relative-escape entry's name is written directly into the
            // header's raw bytes to reproduce what an attacker's archive
            // actually looks like on disk.
            let relative_content = b"escaped via relative path";
            let mut relative = tar::Header::new_gnu();
            let raw_name = b"../escaped-relative.txt";
            relative.as_mut_bytes()[..raw_name.len()].copy_from_slice(raw_name);
            relative.set_size(relative_content.len() as u64);
            relative.set_mode(0o644);
            relative.set_cksum();
            builder
                .append(&relative, &relative_content[..])
                .expect("append relative-escape entry");

            // An absolute member name is representable through the safe
            // API (`preserve_absolute`), unlike `..`, so this one is built
            // through it.
            builder.preserve_absolute(true);
            let absolute_content = b"escaped via absolute path";
            let mut absolute = tar::Header::new_gnu();
            absolute.set_size(absolute_content.len() as u64);
            absolute.set_mode(0o644);
            absolute.set_cksum();
            builder
                .append_data(
                    &mut absolute,
                    "/tmp/escaped-absolute.txt",
                    &absolute_content[..],
                )
                .expect("append absolute-escape entry");

            let mut symlink = tar::Header::new_gnu();
            symlink.set_entry_type(tar::EntryType::Symlink);
            symlink.set_size(0);
            symlink.set_mode(0o777);
            symlink.set_cksum();
            builder
                .append_link(&mut symlink, Path::new("package"), symlink_target)
                .expect("append symlink entry");

            let planted_content = b"planted via symlink write-through";
            let mut planted = tar::Header::new_gnu();
            planted.set_size(planted_content.len() as u64);
            planted.set_mode(0o644);
            planted.set_cksum();
            builder
                .append_data(&mut planted, "package/planted.txt", &planted_content[..])
                .expect("append write-through entry");

            builder.finish().expect("finish tar");
        }
        let mut gz_bytes = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_bytes, flate2::Compression::default());
            encoder.write_all(&tar_bytes).expect("gzip tar bytes");
            encoder.finish().expect("finish gzip");
        }
        gz_bytes
    }

    #[test]
    fn extract_tarball_blocks_relative_absolute_and_symlink_escape_members() {
        let dest = scratch_cache_root("escape-dest");
        let outside = scratch_cache_root("escape-outside");

        let malicious = build_escaping_tarball(&outside);
        let result = extract_tarball(&malicious, &dest);

        // vector 1: a `../` member is dropped entirely -- `tar`'s own
        // traversal check returns `Ok(false)` for it (no error, no file),
        // pinned here at both the plausible outside path and the naively
        // stripped-to-dest path
        assert!(
            !dest
                .parent()
                .expect("dest has a parent")
                .join("escaped-relative.txt")
                .exists(),
            "the relative-escape member must not land outside the destination"
        );
        assert!(
            !dest.join("escaped-relative.txt").exists(),
            "the relative-escape member must not land anywhere -- `tar` skips it outright"
        );

        // vector 2: an absolute member has its leading `/` silently
        // dropped and the remainder treated as relative to `dest`, so it
        // is confined -- not skipped outright, but never escapes
        let relocated = dest.join("tmp").join("escaped-absolute.txt");
        assert!(
            relocated.exists(),
            "the absolute-escape member is expected to be relocated under dest, not skipped: {}",
            relocated.display()
        );
        assert_eq!(
            std::fs::read(&relocated).expect("read relocated absolute member"),
            b"escaped via absolute path",
            "the relocated member's content must be exactly what was archived"
        );

        // vector 3: a symlinked directory later written through is the
        // one hard error -- pinned here so a `tar` upgrade that started
        // allowing it turns this test red instead of silently reopening
        // the escape
        assert!(
            matches!(&result, Err(ProvisionError::Extract { .. })),
            "expected the symlink write-through to be refused, got {result:?}"
        );
        // The refusal's wording is pinned on unix only: windows refuses
        // the symlink member itself (creating one needs a privilege the
        // extracting process does not hold), so the error names that
        // instead. What both platforms must show is the refusal above and
        // the untouched target below.
        #[cfg(unix)]
        {
            let message = result.unwrap_err().to_string();
            assert!(
                message.contains("outside of destination path"),
                "expected an outside-destination refusal, got: {message}"
            );
        }
        assert!(
            !outside.join("planted.txt").exists(),
            "the symlink write-through must never reach the real target it points at"
        );
    }

    #[test]
    fn a_tampered_payload_fails_verification_and_writes_nothing_to_cache() {
        // a genuine, well-formed tarball -- the real supply-chain case is a
        // substituted package that still unpacks cleanly, not corrupted
        // bytes; a malformed payload would kill this test via `Extract`
        // instead of `ChecksumMismatch`, which tests the wrong thing
        let server = StubServer::start(build_test_tarball(b"substituted payload"));
        let expected_sha256: &'static str =
            Box::leak(sha256_hex(b"the genuine bytes").into_boxed_str());
        let pin = test_pin(server.url(), expected_sha256);
        let cache_root = scratch_cache_root("tampered");
        let dir = pin_dir(&pin, &cache_root);

        let result = resolve_in(&pin, &cache_root);

        assert!(
            matches!(result, Err(ProvisionError::ChecksumMismatch { .. })),
            "expected ChecksumMismatch, got {result:?}"
        );
        // the brief's own wording: "no partial-write-then-trust" -- the
        // whole cache dir stays empty, not just the one tarball path a
        // narrower assertion would check
        let remaining = std::fs::read_dir(&dir)
            .map(Iterator::count)
            .unwrap_or_default();
        assert_eq!(
            remaining, 0,
            "a tampered payload must leave no file anywhere in the cache dir"
        );
    }

    #[test]
    fn a_correct_payload_is_extracted_and_the_second_call_makes_no_network_request() {
        let content = b"module.exports = 'the real entry script';";
        let tarball = build_test_tarball(content);
        let sha256 = sha256_hex(&tarball);
        let server = StubServer::start(tarball);
        let pin = test_pin(server.url(), Box::leak(sha256.into_boxed_str()));
        let cache_root = scratch_cache_root("cache-hit");

        let first = resolve_in(&pin, &cache_root).expect("first resolve succeeds");
        assert_eq!(server.request_count(), 1);
        assert!(first.ends_with("package/dist/index.js"));
        assert_eq!(
            std::fs::read(&first).expect("read extracted entry"),
            content,
            "the extracted entry must hold exactly what the tarball archived"
        );

        let second = resolve_in(&pin, &cache_root).expect("second resolve succeeds");
        assert_eq!(first, second, "the entry path must be stable across calls");
        assert_eq!(
            server.request_count(),
            1,
            "a cache hit must not make a second network request"
        );
    }

    #[test]
    fn a_corrupted_cached_tarball_is_re_verified_and_re_downloaded() {
        let content = b"the genuine entry script";
        let tarball = build_test_tarball(content);
        let sha256 = sha256_hex(&tarball);
        let server = StubServer::start(tarball);
        let pin = test_pin(server.url(), Box::leak(sha256.into_boxed_str()));
        let cache_root = scratch_cache_root("corrupt-tarball");

        let first = resolve_in(&pin, &cache_root).expect("first resolve succeeds");
        assert_eq!(server.request_count(), 1);

        // simulate a partial write or external tampering of the cached
        // tarball itself, between two calls
        let dir = pin_dir(&pin, &cache_root);
        let tarball_on_disk = tarball_path(&pin, &dir);
        std::fs::write(&tarball_on_disk, b"corrupted on disk").expect("corrupt cached tarball");

        let second =
            resolve_in(&pin, &cache_root).expect("second resolve re-downloads and succeeds");
        assert_eq!(first, second);
        assert_eq!(
            server.request_count(),
            2,
            "a corrupted cached tarball must not be trusted blindly -- it must re-verify, find \
             the mismatch, and re-download"
        );
        assert_eq!(
            std::fs::read(&second).expect("read re-extracted entry"),
            content
        );
    }

    #[test]
    fn a_corrupted_extracted_entry_is_detected_and_re_extracted_without_a_new_download() {
        let content = b"the genuine entry script";
        let tarball = build_test_tarball(content);
        let sha256 = sha256_hex(&tarball);
        let server = StubServer::start(tarball);
        let pin = test_pin(server.url(), Box::leak(sha256.into_boxed_str()));
        let cache_root = scratch_cache_root("corrupt-extracted");

        let first = resolve_in(&pin, &cache_root).expect("first resolve succeeds");
        assert_eq!(server.request_count(), 1);

        // simulate tampering with the extracted tree directly, leaving the
        // cached tarball untouched
        std::fs::write(&first, b"tampered entry contents").expect("corrupt extracted entry");

        let second = resolve_in(&pin, &cache_root)
            .expect("second resolve re-extracts from the still-valid tarball and succeeds");
        assert_eq!(first, second);
        assert_eq!(
            server.request_count(),
            1,
            "the tarball was still valid -- a corrupted extraction must re-extract from it, \
             never re-download"
        );
        assert_eq!(
            std::fs::read(&second).expect("read re-extracted entry"),
            content,
            "re-extraction must restore the entry's real content"
        );
    }

    /// Writes a complete, self-consistent extraction directly at
    /// `extract_dir` -- the entry file plus its stamp -- without going
    /// through `ensure_extracted`, so a test can pre-populate a "someone
    /// else already published a valid extraction" state deterministically.
    fn write_valid_extraction(pin: &AdapterPin, extract_dir: &Path, entry: &str, content: &[u8]) {
        let entry_path = extract_dir.join(entry);
        std::fs::create_dir_all(entry_path.parent().expect("entry has a parent"))
            .expect("create extracted entry's parent dir");
        std::fs::write(&entry_path, content).expect("write extracted entry");
        // The stamp comes from the one production function so a
        // lockfile-bearing pin stamps here exactly as `ensure_extracted`
        // would, instead of silently writing the bare form.
        std::fs::write(
            extract_dir.join(ENTRY_STAMP_NAME),
            entry_stamp(pin, content),
        )
        .expect("write entry stamp");
    }

    #[test]
    fn publish_extraction_defers_to_a_concurrently_published_valid_destination() {
        let pin = test_pin(String::new(), "");
        let root = scratch_cache_root("publish-race");
        let extract_dir = root.join("extracted");
        let entry_path = extract_dir.join("package/dist/index.js");

        // "someone else" (a concurrent process winning the same race)
        // already published a valid extraction at extract_dir
        write_valid_extraction(
            &pin,
            &extract_dir,
            "package/dist/index.js",
            b"the winner's content",
        );

        // this call's own temp dir, extracted independently and now
        // trying to publish into the same, already-occupied destination
        // -- `rename` onto a non-empty directory always fails, regardless
        // of privilege, so this deterministically forces the collision
        // `ensure_extracted` can hit under real concurrency
        let tmp_dir = root.join(".extracted.loser.tmp");
        write_valid_extraction(
            &pin,
            &tmp_dir,
            "package/dist/index.js",
            b"the loser's content",
        );

        let result = publish_extraction(&pin, &tmp_dir, &extract_dir, &entry_path);

        assert_eq!(
            result.expect("a rename onto an already-valid destination must be treated as benign"),
            entry_path
        );
        assert_eq!(
            std::fs::read(&entry_path).expect("read published entry"),
            b"the winner's content",
            "the already-published valid destination must win -- the loser's content must \
             never overwrite it"
        );
        assert!(
            !tmp_dir.exists(),
            "the loser's now-unused temp dir must be cleaned up"
        );
    }

    #[test]
    fn publish_extraction_still_fails_when_the_destination_is_populated_but_invalid() {
        let pin = test_pin(String::new(), "");
        let root = scratch_cache_root("publish-race-invalid");
        let extract_dir = root.join("extracted");
        let entry_path = extract_dir.join("package/dist/index.js");

        // extract_dir is occupied by something, but it does not form a
        // valid extraction (no stamp) -- the collision must not be waved
        // through just because the destination happens to be non-empty
        std::fs::create_dir_all(extract_dir.join("package/dist"))
            .expect("create unrelated occupant dir");
        std::fs::write(
            extract_dir.join("package/dist/index.js"),
            b"unrelated, unstamped",
        )
        .expect("write unrelated occupant file");

        let tmp_dir = root.join(".extracted.loser.tmp");
        write_valid_extraction(
            &pin,
            &tmp_dir,
            "package/dist/index.js",
            b"the loser's content",
        );

        let result = publish_extraction(&pin, &tmp_dir, &extract_dir, &entry_path);

        assert!(
            matches!(result, Err(ProvisionError::Write { .. })),
            "an occupied-but-invalid destination must still be a hard error, got {result:?}"
        );
    }

    /// Writes a `package.json` beside an already-written extraction, so a
    /// test can say what the extracted package declares.
    fn write_manifest(extract_dir: &Path, dependencies: &str) {
        std::fs::write(
            extract_dir.join("package/package.json"),
            format!("{{\"name\":\"stub\",\"dependencies\":{dependencies}}}"),
        )
        .expect("write package manifest");
    }

    #[test]
    fn an_extraction_missing_the_dependencies_it_declares_is_not_valid() {
        let pin = test_pin(String::new(), "");
        let root = scratch_cache_root("deps-missing");
        let extract_dir = root.join("extracted");
        let entry_path = extract_dir.join(pin.entry);
        write_valid_extraction(&pin, &extract_dir, pin.entry, b"import 'dep';");
        write_manifest(&extract_dir, "{\"dep\":\"^1\"}");

        assert!(
            !extraction_is_valid(&pin, &entry_path, &extract_dir),
            "an intact entry whose imports cannot resolve is not a usable extraction"
        );

        std::fs::create_dir_all(extract_dir.join("package/node_modules"))
            .expect("create node_modules");
        assert!(
            extraction_is_valid(&pin, &entry_path, &extract_dir),
            "the same extraction with its dependencies present is valid"
        );
    }

    #[test]
    fn a_package_declaring_no_dependencies_installs_nothing() {
        let pin = test_pin(String::new(), "");
        let root = scratch_cache_root("deps-none");
        let extract_dir = root.join("extracted");
        let entry_path = extract_dir.join(pin.entry);
        write_valid_extraction(&pin, &extract_dir, pin.entry, b"console.log(1);");

        // no manifest at all, then an empty `dependencies`: neither may
        // reach npm, which is what keeps every other test in this module
        // (and any future self-contained adapter row) off the network
        assert!(extraction_is_valid(&pin, &entry_path, &extract_dir));
        ensure_dependencies(&pin, &extract_dir).expect("a manifest-less package installs nothing");
        write_manifest(&extract_dir, "{}");
        assert!(extraction_is_valid(&pin, &entry_path, &extract_dir));
        ensure_dependencies(&pin, &extract_dir).expect("an empty dependency set installs nothing");
    }

    /// The install is never reached for a package whose dependencies the
    /// row cannot pin: the whole point of the checksum is that nothing
    /// unverified runs, and a fresh resolve is exactly the unverified tree
    /// it exists to refuse.
    #[test]
    fn a_package_whose_dependencies_the_row_cannot_pin_is_never_installed() {
        let pin = test_pin(String::new(), "");
        let root = scratch_cache_root("deps-unpinned");
        let extract_dir = root.join("extracted");
        write_valid_extraction(&pin, &extract_dir, pin.entry, b"import 'dep';");
        write_manifest(&extract_dir, "{\"dep\":\"^1\"}");

        let err = ensure_dependencies(&pin, &extract_dir)
            .expect_err("a lockfile-less row must refuse rather than resolve");
        assert!(
            matches!(err, ProvisionError::LockfileMismatch { .. }),
            "expected a lockfile mismatch, got {err:?}"
        );
    }

    #[test]
    fn a_lockfile_locking_another_release_is_refused() {
        let mut pin = test_pin(String::new(), "");
        pin.lockfile = Some("{\"name\":\"stub\",\"version\":\"9.9.9\",\"lockfileVersion\":3}");

        let err = pinned_lockfile(&pin).expect_err("a lockfile for another release must refuse");
        assert!(
            matches!(err, ProvisionError::LockfileMismatch { .. }),
            "expected a lockfile mismatch, got {err:?}"
        );
        let said = format!("{err}");
        assert!(
            said.contains("9.9.9") && said.contains(pin.version),
            "the refusal must name both versions, said {said:?}"
        );
    }

    /// The shipped row's own lockfile, checked here rather than only on a
    /// machine that provisions: a lockfile re-captured against a bumped
    /// version without the row moving with it would otherwise fail for the
    /// first user to run a cold provision, not for the commit that did it.
    #[test]
    fn the_shipped_row_carries_a_lockfile_for_the_version_it_pins() {
        for pin in &ADAPTERS {
            let checked = pinned_lockfile(pin);
            assert!(
                checked.is_ok(),
                "`{}`'s lockfile must match its pin: {checked:?}",
                pin.id
            );
        }
    }

    /// The adapter release the `allow_always` probe was last run against,
    /// and what it answered.
    ///
    /// `scripts/acp-allow-always-probe.mjs` drove 0.69.0 over a real wire
    /// with real credentials and was re-prompted for all four Edit calls
    /// after answering the adapter's own `allow_always` option id verbatim:
    /// the release does not honor the grant, which is why view carries the
    /// standing-answer store in `crate::acp::permission`. The store is dead
    /// weight -- and its auto-answer a second grant the user did not give --
    /// the day an adapter starts honoring it, so the pin may not move
    /// without the probe moving with it.
    const PROBED_VERSION: &str = "0.69.0";
    const PROBED_HONORS_ALLOW_ALWAYS: bool = false;

    #[test]
    fn the_pinned_adapter_is_the_one_the_allow_always_probe_answered_for() {
        let pinned = pinned_version("claude-code").expect("this build pins claude-code");
        assert_eq!(
            pinned, PROBED_VERSION,
            "the claude-code pin moved to {pinned} and nothing has probed it: re-run the probe \
             (`node scripts/acp-allow-always-probe.mjs`) against {pinned}, then set \
             PROBED_VERSION and PROBED_HONORS_ALLOW_ALWAYS here from its verdict. If \
             `honors_allow_always` came back true, view's standing-answer store \
             (crates/view-ai/src/acp/permission.rs) now auto-answers a request the adapter \
             would have answered itself and must be retired in the same commit."
        );
    }

    /// The other half of the pin assertion, and a compile error rather than
    /// a test failure because the store this guards is shipped code: a probe
    /// verdict of `honors_allow_always: true` means view's standing-answer
    /// store auto-answers a request the adapter would have granted itself,
    /// which is a second grant the user never gave.
    const _: () = assert!(
        !PROBED_HONORS_ALLOW_ALWAYS,
        "the probe recorded the pinned adapter as honoring allow_always: retire the \
         standing-answer store in crates/view-ai/src/acp/permission.rs rather than \
         relaxing this"
    );

    /// The probe named above has to be runnable by the session the assertion
    /// above sends to run it, and it once lived outside the tree.
    #[test]
    fn the_probe_the_pin_assertion_names_is_in_the_tree() {
        let probe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/acp-allow-always-probe.mjs");
        assert!(probe.is_file(), "{} is missing", probe.display());
    }

    #[test]
    fn an_adapter_is_ready_only_once_its_extraction_is_complete() {
        let pin = test_pin(String::new(), "");
        let root = scratch_cache_root("ready");
        assert!(
            !ready_in(&pin, &root),
            "nothing is provisioned in an empty cache"
        );

        let extract_dir = extract_dir(&pin_dir(&pin, &root));
        write_valid_extraction(&pin, &extract_dir, pin.entry, b"console.log(1);");
        assert!(
            ready_in(&pin, &root),
            "a complete extraction needs no download or install"
        );
    }

    #[test]
    fn a_tree_installed_under_another_lockfile_is_provisioned_again() {
        let content = b"the genuine entry script";
        let tarball = build_test_tarball(content);
        let sha256 = sha256_hex(&tarball);
        let server = StubServer::start(tarball);
        let mut pin = test_pin(server.url(), Box::leak(sha256.into_boxed_str()));
        pin.lockfile = Some("{\"version\":\"0.0.0-test\"}");
        let cache_root = scratch_cache_root("stamp-lockfile");

        let entry = resolve_in(&pin, &cache_root).expect("first resolve succeeds");
        let extract_dir = extract_dir(&pin_dir(&pin, &cache_root));

        // exactly what a machine provisioned before this row pinned a
        // lockfile carries: the entry's own hash and nothing about what
        // was installed beside it
        std::fs::write(extract_dir.join(ENTRY_STAMP_NAME), sha256_hex(content))
            .expect("write the pre-lockfile stamp");
        assert!(
            !extraction_is_valid(&pin, &entry, &extract_dir),
            "an extraction stamped without the pinned lockfile must not be reused -- its \
             dependencies came from somewhere the pin never covered"
        );

        resolve_in(&pin, &cache_root).expect("second resolve re-provisions");
        assert_eq!(
            std::fs::read_to_string(extract_dir.join(ENTRY_STAMP_NAME))
                .expect("read the re-provisioned stamp"),
            entry_stamp(&pin, content),
            "re-provisioning must leave a stamp that covers the pinned lockfile"
        );
        assert_eq!(
            server.request_count(),
            1,
            "the cached tarball was still valid -- re-provisioning must reuse it"
        );
    }

    #[test]
    fn an_unknown_id_is_reported_rather_than_panicking() {
        let err = ensure_adapter("no-such-adapter").expect_err("unknown id must be an error");
        assert!(matches!(err, ProvisionError::UnknownAdapter { id } if id == "no-such-adapter"));
    }

    #[test]
    fn the_claude_code_row_is_pinned_and_reachable_by_lookup() {
        assert_eq!(pinned_version("claude-code"), Some("0.69.0"));
        assert_eq!(pinned_version("no-such-adapter"), None);
        let pin = lookup("claude-code").expect("claude-code is this build's one known adapter");
        assert_eq!(pin.sha256.len(), 64, "sha256 must be a full hex digest");
        assert_eq!(pin.entry, "package/dist/index.js");
        assert!(pin.url_template.starts_with("https://"));
    }

    /// Serializes every test here that mutates `PATH`: it is process-global,
    /// and two tests racing their own redirect/restore would interleave --
    /// the same reason `trust.rs`'s own tests guard `XDG_STATE_HOME`. This
    /// lock only serializes the tests *against each other* -- it does
    /// nothing for a concurrently running test elsewhere in this same
    /// `view-ai` lib test binary that reads `PATH` or spawns a
    /// subprocess, since `cargo test` runs the whole binary's tests on
    /// one shared process. Nothing does either today, so this is latent
    /// rather than flaky; a future test that spawns a child process must
    /// take this lock too, or move to a separate test binary/process.
    static PATH_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct PathRestoreGuard {
        prev: Option<std::ffi::OsString>,
    }

    impl PathRestoreGuard {
        fn capture() -> Self {
            Self {
                prev: std::env::var_os("PATH"),
            }
        }
    }

    impl Drop for PathRestoreGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[test]
    fn resolve_node_reports_a_remedy_when_absent_from_path() {
        let _guard = PATH_MUTATION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let empty_dir = scratch_cache_root("no-node-on-path");
        let _restore = PathRestoreGuard::capture();
        std::env::set_var("PATH", &empty_dir);

        let err = resolve_node().expect_err("no node executable exists under an empty PATH dir");
        assert!(matches!(err, ProvisionError::NodeNotFound));
    }

    #[test]
    fn resolve_node_finds_a_node_executable_on_path() {
        let _guard = PATH_MUTATION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = scratch_cache_root("node-on-path");
        let exe_name = if cfg!(windows) { "node.exe" } else { "node" };
        let node_path = dir.join(exe_name);
        std::fs::write(&node_path, b"#!/bin/sh\necho fake node\n").expect("write fake node");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&node_path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake node");
        }
        let _restore = PathRestoreGuard::capture();
        std::env::set_var("PATH", &dir);

        let resolved = resolve_node().expect("node executable should resolve from PATH");
        assert_eq!(resolved, node_path);
    }
}
