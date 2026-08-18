//! ACP adapter auto-provisioning: resolving a known adapter id to a
//! downloaded, checksum-verified binary on disk.
//!
//! Mechanism only, per the charter's own split: this module owns
//! downloading, verifying, and caching against a hand-maintained
//! [`AdapterPin`] table closed at compile time, on `view-core::native::
//! registry`'s own precedent (a runtime-registered table could vanish an
//! adapter silently instead of reporting it present and unprovisionable).
//! Wiring that table into a signed, automatically updated manifest sourced
//! from a release pipeline is separate packaging work with no dependency on
//! anodizer's release pipeline; nothing here reaches toward one.
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

use std::io::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::trust::env_dir;

// `ProvisionError` follows `TrustError`/`AiConfigError`'s own size
// discipline: a variant growing past `clippy::result_large_err`'s 128-byte
// threshold is a lint failure here too, not a review note.
const _: () = assert!(std::mem::size_of::<ProvisionError>() <= 128);

/// Everything that can stop [`ensure_adapter`] from returning a verified
/// path.
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
    /// The downloaded (or previously cached) bytes do not hash to the
    /// pinned checksum -- a corrupted transfer, a tampered cache file, or a
    /// release that moved out from under its pin. Never launched: this is
    /// the one variant [`ensure_adapter`]'s own doc calls a hard error, not
    /// a warning.
    #[error("adapter `{id}` failed checksum verification: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// The adapter id being provisioned.
        id: String,
        /// The checksum the pin expects.
        expected: String,
        /// The checksum the bytes actually hashed to.
        actual: String,
    },
    /// The cache directory could not be created, or a verified download
    /// could not be written into it.
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
    /// know about.
    pub url_template: &'static str,
    /// The expected SHA-256 of the downloaded bytes, lowercase hex.
    pub sha256: &'static str,
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
}];

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

/// Resolves `id` to a local, verified binary path: returns the cached path
/// if already downloaded and checksum-verified, otherwise downloads,
/// verifies, and caches under the platform cache dir before returning.
/// Never launches a binary whose checksum doesn't match -- a corrupted or
/// tampered download is a hard error, not a warning.
///
/// # Errors
///
/// Returns [`ProvisionError::UnknownAdapter`] if `id` names no compile-time
/// row, [`ProvisionError::NoCacheDir`] if no platform cache directory can
/// be resolved, [`ProvisionError::Download`] if the network request fails,
/// [`ProvisionError::ChecksumMismatch`] if the downloaded (or a corrupted
/// cached) file's hash does not match the pin, and
/// [`ProvisionError::Write`] if the verified bytes cannot be cached to
/// disk.
pub fn ensure_adapter(id: &str) -> Result<PathBuf, ProvisionError> {
    let pin = lookup(id).ok_or_else(|| ProvisionError::UnknownAdapter { id: id.to_string() })?;
    resolve(pin)
}

/// The download-verify-cache mechanism `ensure_adapter` drives, taking an
/// explicit [`AdapterPin`] rather than an id: the tests exercise this
/// directly against a pin pointed at a stub server, keeping the checksum
/// and caching behavior fully testable without ever making a real network
/// request in `cargo test`.
fn resolve(pin: &AdapterPin) -> Result<PathBuf, ProvisionError> {
    let dir = cache_dir(pin)?;
    std::fs::create_dir_all(&dir).map_err(|source| ProvisionError::Write {
        path: dir.clone(),
        source,
    })?;
    let path = dir.join(cache_file_name(pin));

    // A cache hit is only ever a hit after re-verifying the bytes on disk:
    // between two calls, the file on disk could have been partially
    // written by a crash or tampered with externally, and trusting its
    // mere presence would launch exactly the corrupted-or-tampered binary
    // this whole module exists to refuse.
    if let Ok(existing) = std::fs::read(&path) {
        if sha256_hex(&existing) == pin.sha256 {
            return Ok(path);
        }
    }

    let bytes = download(pin)?;
    let actual = sha256_hex(&bytes);
    if actual != pin.sha256 {
        // Nothing is written to the cache dir on a mismatch: verification
        // happens against the in-memory buffer, before any file at `path`
        // is ever touched, so a tampered payload can never leave a
        // partial-write-then-trust artifact behind for the next call to
        // find.
        return Err(ProvisionError::ChecksumMismatch {
            id: pin.id.to_string(),
            expected: pin.sha256.to_string(),
            actual,
        });
    }

    write_atomically(&path, &bytes)?;
    Ok(path)
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

/// The cache directory a pin's verified download lives under:
/// `<platform cache dir>/view/adapters/<id>/<version>`. Scoped by version
/// so bumping a pin's `version` field provisions fresh rather than
/// colliding with (or being shadowed by) an older cached download.
fn cache_dir(pin: &AdapterPin) -> Result<PathBuf, ProvisionError> {
    Ok(cache_root()?
        .join("view")
        .join("adapters")
        .join(pin.id)
        .join(pin.version))
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

/// The cached file's own name: the resolved URL's last path segment, or
/// the adapter id if the URL ends in a trailing slash or has none -- the
/// name only needs to be stable and collision-free within
/// [`cache_dir`]'s already-scoped `<id>/<version>` directory, not
/// meaningful on its own.
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

/// Writes `bytes` to `path` as an executable file, atomically: a temp file
/// in the same directory, mode-bit'd before the rename rather than after,
/// then renamed over `path`. The same temp+rename shape
/// [`crate::trust::persist`] uses for the trust store, for the same
/// reason -- a crash mid-write must never leave a half-written file for
/// the next call's cache-hit check to trust -- with the mode bits set to
/// `0o755` instead of that function's `0600`: a trust record is a secret
/// meant for one user to read, a provisioned adapter is a binary meant to
/// be executed.
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
    let mut file = std::fs::File::create(&tmp_path).map_err(|source| ProvisionError::Write {
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
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755)).map_err(
            |source| ProvisionError::Write {
                path: path.to_path_buf(),
                source,
            },
        )?;
    }
    std::fs::rename(&tmp_path, path).map_err(|source| ProvisionError::Write {
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
    /// server crate: this module's one new dependency is the HTTP client
    /// under test, and a real client exercised against a from-scratch
    /// listener is a more faithful RED-phase disconfirm than mocking the
    /// client itself would be.
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
            format!("http://{}/adapter.bin", self.addr)
        }

        fn request_count(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    /// A scratch cache root under the workspace's own `target/tmp`,
    /// nonce-tagged so parallel test binaries never collide, on the same
    /// terms `view_test_support::ScratchDir` provides -- used directly
    /// here (rather than pointing `XDG_CACHE_HOME` at one) because
    /// `cache_dir` is exercised through `resolve`'s explicit pin, not
    /// through the real `XDG_CACHE_HOME` fallback chain that
    /// `trust.rs`'s own tests cover for `env_dir` already.
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
        }
    }

    #[test]
    fn a_tampered_payload_fails_verification_and_writes_nothing_to_cache() {
        let server = StubServer::start(b"not the real bytes".to_vec());
        // the checksum a genuine payload would hash to; the stub above
        // serves something else entirely, so verification must fail
        // against this pin regardless of what the real digest is
        let expected_sha256: &'static str =
            Box::leak(sha256_hex(b"the genuine bytes").into_boxed_str());
        let pin = test_pin(server.url(), expected_sha256);
        let cache_root = scratch_cache_root("tampered");

        let dir = cache_root
            .join("view")
            .join("adapters")
            .join(pin.id)
            .join(pin.version);
        let path_before = dir.join("adapter.bin");
        assert!(!path_before.exists());

        let result = resolve_against(&pin, &cache_root);

        assert!(
            matches!(result, Err(ProvisionError::ChecksumMismatch { .. })),
            "expected ChecksumMismatch, got {result:?}"
        );
        assert!(
            !path_before.exists(),
            "a tampered payload must leave no file in the cache dir"
        );
    }

    #[test]
    fn a_correct_payload_is_cached_and_the_second_call_makes_no_network_request() {
        let body = b"a perfectly good adapter binary".to_vec();
        let sha256 = sha256_hex(&body);
        let server = StubServer::start(body);
        let pin = test_pin(server.url(), Box::leak(sha256.into_boxed_str()));
        let cache_root = scratch_cache_root("cache-hit");

        let first = resolve_against(&pin, &cache_root).expect("first resolve succeeds");
        assert_eq!(server.request_count(), 1);

        let second = resolve_against(&pin, &cache_root).expect("second resolve succeeds");
        assert_eq!(first, second, "the cached path must be stable across calls");
        assert_eq!(
            server.request_count(),
            1,
            "a cache hit must not make a second network request"
        );
    }

    #[test]
    fn a_corrupted_cached_file_is_re_verified_and_re_downloaded() {
        let body = b"the genuine article".to_vec();
        let sha256 = sha256_hex(&body);
        let server = StubServer::start(body);
        let pin = test_pin(server.url(), Box::leak(sha256.clone().into_boxed_str()));
        let cache_root = scratch_cache_root("corrupt-recover");

        let first = resolve_against(&pin, &cache_root).expect("first resolve succeeds");
        assert_eq!(server.request_count(), 1);

        // simulate a partial write or external tampering between two calls
        std::fs::write(&first, b"corrupted on disk").expect("corrupt cached file");

        let second =
            resolve_against(&pin, &cache_root).expect("second resolve re-downloads and succeeds");
        assert_eq!(first, second);
        assert_eq!(
            server.request_count(),
            2,
            "a corrupted cache entry must not be trusted blindly -- it must re-verify, find the \
             mismatch, and re-download"
        );
        assert_eq!(sha256_hex(&std::fs::read(&second).unwrap()), sha256);
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
    }

    /// `resolve`, but against an explicit cache root instead of the real
    /// platform cache directory -- the seam these tests need to stay
    /// hermetic without mutating process-global environment state the way
    /// `trust.rs`'s tests do for `XDG_STATE_HOME`. `resolve` itself always
    /// goes through `cache_dir`/`cache_root`; this helper duplicates only
    /// the directory-join, not the download/verify/cache logic, which is
    /// exactly what these tests are proving.
    fn resolve_against(pin: &AdapterPin, cache_root: &Path) -> Result<PathBuf, ProvisionError> {
        let dir = cache_root
            .join("view")
            .join("adapters")
            .join(pin.id)
            .join(pin.version);
        std::fs::create_dir_all(&dir).map_err(|source| ProvisionError::Write {
            path: dir.clone(),
            source,
        })?;
        let path = dir.join(cache_file_name(pin));

        if let Ok(existing) = std::fs::read(&path) {
            if sha256_hex(&existing) == pin.sha256 {
                return Ok(path);
            }
        }

        let bytes = download(pin)?;
        let actual = sha256_hex(&bytes);
        if actual != pin.sha256 {
            return Err(ProvisionError::ChecksumMismatch {
                id: pin.id.to_string(),
                expected: pin.sha256.to_string(),
                actual,
            });
        }

        write_atomically(&path, &bytes)?;
        Ok(path)
    }
}
