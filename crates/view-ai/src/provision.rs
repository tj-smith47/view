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

/// `node`/`node.exe` resolved from `PATH`, or
/// [`ProvisionError::NodeNotFound`] with a remedy. `pub(crate)`, not
/// public: this is specific to launching a JavaScript entry script the way
/// [`crate::ClaudeCodeAdapter::provisioned`] needs to, not a general "find
/// an interpreter" utility this module means to expose.
pub(crate) fn resolve_node() -> Result<PathBuf, ProvisionError> {
    let exe_name = if cfg!(windows) { "node.exe" } else { "node" };
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join(exe_name))
        .find(|candidate| candidate.is_file())
        .ok_or(ProvisionError::NodeNotFound)
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
    if extraction_is_valid(&entry_path, extract_dir) {
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
    if let Err(err) = std::fs::write(tmp_dir.join(ENTRY_STAMP_NAME), sha256_hex(&entry_bytes)) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return Err(ProvisionError::Write {
            path: tmp_dir.join(ENTRY_STAMP_NAME),
            source: err,
        });
    }

    // Directory rename cannot atomically replace a non-empty destination
    // on any platform this crate ships on, so a stale (or corrupted)
    // `extract_dir` is removed first. That pair is not atomic against a
    // crash between the two calls -- but an absent `extract_dir` is
    // exactly the state `extraction_is_valid` already treats as "needs
    // (re)extraction", so an interruption here only costs a repeat
    // extraction on the next call, never a corrupt-but-trusted one.
    let _ = std::fs::remove_dir_all(extract_dir);
    std::fs::rename(&tmp_dir, extract_dir).map_err(|source| ProvisionError::Write {
        path: extract_dir.to_path_buf(),
        source,
    })?;

    Ok(entry_path)
}

/// The extracted entry stamp's file name: a hex SHA-256 of the entry
/// file's own bytes at extraction time, written alongside it so a later
/// call can detect the entry having been modified on disk since --
/// deliberately scoped to the one file that is ever read as this
/// adapter's launchable script, not a hash of the whole extracted tree.
const ENTRY_STAMP_NAME: &str = ".entry-sha256";

/// Whether `entry_path` (under `extract_dir`) still matches the stamp left
/// at extraction time -- `false` for a missing extraction, a missing
/// entry, a missing or unreadable stamp, or an entry whose bytes no longer
/// hash to what the stamp recorded.
fn extraction_is_valid(entry_path: &Path, extract_dir: &Path) -> bool {
    let Ok(entry_bytes) = std::fs::read(entry_path) else {
        return false;
    };
    let Ok(stamp) = std::fs::read_to_string(extract_dir.join(ENTRY_STAMP_NAME)) else {
        return false;
    };
    stamp.trim() == sha256_hex(&entry_bytes)
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

    #[test]
    fn a_tampered_payload_fails_verification_and_writes_nothing_to_cache() {
        let server = StubServer::start(b"not a valid tarball at all".to_vec());
        // the checksum a genuine payload would hash to; the stub above
        // serves something else entirely, so verification must fail
        // against this pin regardless of what the real digest is
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
    /// the same reason `trust.rs`'s own tests guard `XDG_STATE_HOME`.
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
