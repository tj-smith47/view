//! Spawning `nvim --embed` and performing the API-info handshake.

use crate::handle::{EngineError, EngineHandle, EngineNotification};
use rmpv::Value;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::Duration;

/// Configuration for spawning an embedded Neovim process.
pub struct EngineConfig {
    /// Path to the `nvim` binary. Defaults to `"nvim"`, resolved via `PATH`;
    /// release packaging replaces this default with a bundled binary path.
    pub nvim_bin: PathBuf,
    /// Additional arguments passed to `nvim` after `--embed`.
    pub extra_args: Vec<OsString>,
    /// Maximum time to wait for the `nvim_get_api_info` handshake response
    /// during [`Engine::spawn`]. Defaults to 5 seconds. A process that
    /// spawns but never replies (wedged, wrong binary, hung under a
    /// debugger) fails `spawn()` with `EngineError::Timeout` instead of
    /// blocking the caller forever; the child is reaped before the error
    /// is returned.
    pub handshake_timeout: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            nvim_bin: PathBuf::from("nvim"),
            extra_args: vec![],
            handshake_timeout: Duration::from_secs(5),
        }
    }
}

/// The engine's reported API version and RPC channel id, from
/// `nvim_get_api_info`.
pub struct ApiInfo {
    /// The msgpack-RPC channel id assigned to this connection.
    pub channel_id: u64,
    /// Major version component of the running Neovim's API.
    pub version_major: u64,
    /// Minor version component of the running Neovim's API.
    pub version_minor: u64,
}

/// A spawned embedded Neovim process with its RPC handle and notification
/// receiver.
///
/// `Engine` owns the child process for its entire lifetime: once
/// `Engine::spawn` returns `Ok`, dropping the `Engine` always kills and
/// reaps the child (`Drop` runs `kill()` then `wait()`, ignoring errors
/// since the process may already have exited on its own, e.g. after a
/// graceful `:q`). Callers that want the real exit status of a
/// normally-exited process should call `child.wait()` themselves before
/// the `Engine` drops; the subsequent `Drop`-driven `kill()`/`wait()` on an
/// already-reaped child is a harmless no-op error that is discarded.
pub struct Engine {
    /// The RPC client for issuing requests to the engine.
    pub handle: EngineHandle,
    /// Receiver for notifications (e.g., `redraw` events) from the engine.
    pub notifications: Receiver<EngineNotification>,
    /// The child process handle, for lifecycle control (e.g., reading the
    /// exit status after a graceful `:q`). `Engine`'s `Drop` impl kills and
    /// reaps this child unconditionally, so manual cleanup is only needed
    /// to observe the exit code, never to prevent a zombie.
    pub child: Child,
    /// The engine's API version and channel id, captured at handshake time.
    pub api_info: ApiInfo,
}

/// Owns a spawned child during [`Engine::spawn`] so every early-return path
/// (pipe capture failure, handshake error or timeout) reaps it instead of
/// leaking a zombie. Disarmed via `.0.take()` once `spawn` has everything it
/// needs to build the long-lived `Engine`, which then owns reaping itself.
struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // best-effort: the child may already be gone (e.g. it exited on
            // its own before the guard dropped), so errors here are not
            // actionable and are discarded rather than propagated
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Engine {
    /// Spawns `nvim --embed` per `cfg` and performs the `nvim_get_api_info`
    /// handshake.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Io` if the process fails to spawn or its pipes
    /// cannot be captured, or the error returned by the handshake request
    /// itself (`EngineError::Rpc`, `Remote`, `Closed`, or `Timeout` if the
    /// engine does not answer within `cfg.handshake_timeout`). On any error
    /// after a successful process spawn, the child is killed and reaped
    /// before the error is returned; no zombie survives a failed `spawn`.
    pub fn spawn(cfg: EngineConfig) -> Result<Self, EngineError> {
        let mut guard = ChildGuard(Some(
            Command::new(&cfg.nvim_bin)
                .arg("--embed")
                .args(&cfg.extra_args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        ));
        // unreachable ok_or: nothing clears guard.0 before this point
        let child = guard
            .0
            .as_mut()
            .ok_or_else(|| EngineError::Io(std::io::Error::other("child slot empty")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| EngineError::Io(std::io::Error::other("stdout pipe not captured")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| EngineError::Io(std::io::Error::other("stdin pipe not captured")))?;
        let (handle, notifications) = EngineHandle::start(stdout, stdin);
        let api_info = decode_api_info(handle.request_timeout(
            "nvim_get_api_info",
            vec![],
            cfg.handshake_timeout,
        )?)?;
        // handshake succeeded: disarm the guard and hand the child to the
        // long-lived Engine, which now owns reaping it via its own Drop
        // unreachable else: nothing clears guard.0 before this point
        let Some(child) = guard.0.take() else {
            return Err(EngineError::Io(std::io::Error::other("child slot empty")));
        };
        Ok(Self {
            handle,
            notifications,
            child,
            api_info,
        })
    }
}

impl Drop for Engine {
    /// Kills and reaps the child on every drop path, mirroring
    /// [`ChildGuard`]'s discipline for the pre-handshake window. Errors are
    /// discarded: a `kill()` on an already-exited process (e.g. one whose
    /// exit status a caller already collected via `child.wait()`) is not
    /// actionable and must not panic or be surfaced from a `Drop` impl.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn decode_api_info(v: Value) -> Result<ApiInfo, EngineError> {
    let bad = || EngineError::Remote(Value::from("unexpected api_info shape"));
    let Value::Array(parts) = v else {
        return Err(bad());
    };
    let channel_id = parts.first().and_then(Value::as_u64).ok_or_else(bad)?;
    let meta = parts.get(1).ok_or_else(bad)?;
    let version = map_get(meta, "version").ok_or_else(bad)?;
    Ok(ApiInfo {
        channel_id,
        version_major: map_get(&version, "major")
            .and_then(|v| v.as_u64())
            .ok_or_else(bad)?,
        version_minor: map_get(&version, "minor")
            .and_then(|v| v.as_u64())
            .ok_or_else(bad)?,
    })
}

fn map_get(v: &Value, key: &str) -> Option<Value> {
    let Value::Map(pairs) = v else { return None };
    pairs
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, val)| val.clone())
}
