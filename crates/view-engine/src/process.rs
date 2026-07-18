//! Spawning `nvim --embed` and performing the API-info handshake.
//!
//! `Engine` owns the child process, its RPC handle, and the damage pump for
//! the process's lifetime. Its `Drop` impl attempts a graceful
//! shutdown (`qa!` sent over the writer thread, then a bounded wait) before
//! falling back to `SIGKILL`, so a normally-responsive nvim gets the chance
//! to flush shada and remove its swap file instead of leaving behind a
//! recovery prompt on the next open.

use crate::damage::{DamagePump, PumpShared, SinkCutover};
use crate::handle::{EngineError, EngineHandle};
use rmpv::Value;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::{Duration, Instant};
use view_core::msg::{ExitInfo, Msg};

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
    /// Maximum time to wait for the child to exit on its own after a
    /// graceful `qa!` is sent during shutdown ([`Engine::shutdown`] or
    /// `Drop`). Defaults to 500 milliseconds. A child still running once
    /// this elapses is force-killed instead.
    pub shutdown_timeout: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            nvim_bin: PathBuf::from("nvim"),
            extra_args: vec![],
            handshake_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_millis(500),
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

/// A spawned embedded Neovim process with its RPC handle and damage pump.
///
/// `Engine` owns the child process for its entire lifetime: once
/// `Engine::spawn` returns `Ok`, dropping the `Engine` always shuts the
/// child down (`Drop` attempts a graceful `qa!` first, then force-kills and
/// reaps; see [`shutdown`](Self::shutdown) for the same sequence with an
/// observable exit status). The child itself is a private field: callers
/// cannot block on it directly, only read its pid, attach the runtime
/// loop's damage pump, or consume the `Engine` to shut it down explicitly.
pub struct Engine {
    /// The RPC client for issuing requests to the engine. `Clone` and
    /// `Send`, so requests can be issued from other threads while the
    /// runtime loop owns the [`DamagePump`] returned by
    /// [`start_pump`](Self::start_pump).
    pub handle: EngineHandle,
    child: Child,
    shutdown_timeout: Duration,
    /// The engine's API version and channel id, captured at handshake time.
    pub api_info: ApiInfo,
    /// Damage/request pump state, live from `spawn` so redraws and known
    /// requests arriving before [`start_pump`](Self::start_pump) attaches a
    /// sink are staged rather than lost. See `crate::damage` for the full
    /// contract.
    pump: Arc<PumpShared>,
}

/// Owns a spawned child during [`Engine::spawn`] so every early-return path
/// (pipe capture failure, handshake error or timeout) reaps it instead of
/// leaking a zombie. Disarmed via `.0.take()` once `spawn` has everything it
/// needs to build the long-lived `Engine`, which then owns reaping itself.
///
/// This guard only covers the pre-handshake window, where the child has
/// never answered anything, so there is no session state worth saving; it
/// always force-kills rather than attempting the graceful shutdown
/// `Engine`'s own `Drop` uses once a connection is actually live.
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
        // built before EngineHandle::start_pumped so the reader thread can
        // fold and stage from its very first message, before start_pump is
        // ever called
        let pump = PumpShared::new();
        let handle = EngineHandle::start_pumped(stdout, stdin, Arc::clone(&pump));
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
            child,
            shutdown_timeout: cfg.shutdown_timeout,
            api_info,
            pump,
        })
    }

    /// Attaches the runtime loop's bounded `Msg` channel and returns the
    /// [`DamagePump`] handle for draining compacted damage from it, plus
    /// [`SinkCutover`]: everything that arrived between `spawn` and this
    /// call (a `view_vim_enter` firing during the window before this call,
    /// most notably, plus whether damage was already pending), returned
    /// rather than sent into `sink`. `sink` has no guaranteed consumer yet
    /// at the moment this call is made, so nothing here performs a send at
    /// all -- see [`PumpShared::attach_sink`]'s doc comment for why. The
    /// caller resolves the returned state through its own dispatch path
    /// once a consumer is guaranteed (see `view`'s `startup::run_cutover`).
    #[must_use]
    pub fn start_pump(&mut self, sink: SyncSender<Msg>) -> (DamagePump, SinkCutover) {
        self.pump.attach_sink(sink)
    }

    /// Resolves the engine's exit status into an [`ExitInfo`], for the
    /// runtime loop to call once its reader signals `Msg::EngineStopped`
    /// (the reader thread's stream ended, so the connection is already
    /// gone; this determines the child's real exit status).
    ///
    /// Reuses `graceful_kill`'s bounded-wait-then-kill sequence rather
    /// than duplicating it: sending `qa!` again here is a harmless no-op
    /// once the connection is already closed (`notify` just fails silently
    /// and the very next `try_wait` typically finds the child already
    /// exited). `code: None` means the exit status itself was unreadable
    /// (a `std::io::Error` from `try_wait`/`kill`/`wait`), which `update()`
    /// maps to exit code 1 rather than treating as success.
    #[must_use]
    pub fn wait_exit(&mut self) -> ExitInfo {
        match graceful_kill(&self.handle, &mut self.child, self.shutdown_timeout) {
            Ok(status) => exit_info_from_status(status),
            Err(_) => ExitInfo {
                code: None,
                by_signal: false,
            },
        }
    }

    /// The OS process id of the spawned child. For diagnostics and tests
    /// that need to verify the process was actually reaped; does not block
    /// or observe exit status (see [`shutdown`](Self::shutdown) for that).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Consumes the `Engine` and runs the same graceful-then-forced
    /// shutdown sequence as `Drop`, returning the child's real exit status.
    ///
    /// `Drop` alone cannot surface this: it runs on every drop path
    /// (including a panic unwinding through an `Engine`), discards errors,
    /// and has no return value. Call `shutdown` explicitly to distinguish a
    /// graceful exit from a forced kill (e.g. via `ExitStatusExt::signal`
    /// on Unix) or to forward the real exit code, such as via
    /// `std::process::exit`.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if `try_wait`, `kill`, or
    /// `wait` on the child process fails.
    pub fn shutdown(mut self) -> std::io::Result<ExitStatus> {
        graceful_kill(&self.handle, &mut self.child, self.shutdown_timeout)
    }
}

impl Drop for Engine {
    /// Attempts a graceful shutdown (`qa!`, then a bounded wait) and falls
    /// back to `SIGKILL` + reap on every drop path. Errors are discarded: a
    /// `kill()` on an already-exited process (e.g. one whose exit status a
    /// caller already collected via [`shutdown`](Self::shutdown)) is not
    /// actionable and must not panic or be surfaced from a `Drop` impl.
    fn drop(&mut self) {
        let _ = graceful_kill(&self.handle, &mut self.child, self.shutdown_timeout);
    }
}

/// Sends `qa!` as a fire-and-forget notification, polls `try_wait` until
/// the child exits or `shutdown_timeout` elapses, then force-kills and
/// reaps it. Shared by [`Engine::shutdown`] and `Engine`'s `Drop` impl so
/// the two sequences can never drift apart.
///
/// The `notify` call is best-effort: if the writer thread is already gone
/// (connection already closed, e.g. nvim crashed or the peer wrote garbage
/// mid-session), sending `qa!` fails and this falls straight through to the
/// poll loop, which sees the child has already exited on the very first
/// `try_wait`.
fn graceful_kill(
    handle: &EngineHandle,
    child: &mut Child,
    shutdown_timeout: Duration,
) -> std::io::Result<ExitStatus> {
    let _ = handle.notify("nvim_command", vec![Value::from("qa!")]);
    let deadline = Instant::now() + shutdown_timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill()?;
    child.wait()
}

/// Maps a child's raw `ExitStatus` to [`ExitInfo`]: a normal exit passes its
/// code through unchanged; a signal death (no exit code at all on Unix) maps
/// to `128 + signal`, the conventional mapping shells already use (`$?`
/// after a `SIGKILL`ed process is 137), so `update()`'s `Effect::Quit` exit
/// code matches what a caller's shell would report for the same death.
#[cfg(unix)]
fn exit_info_from_status(status: ExitStatus) -> ExitInfo {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => ExitInfo {
            code: Some(code),
            by_signal: false,
        },
        None => ExitInfo {
            code: status.signal().map(|sig| 128 + sig),
            by_signal: true,
        },
    }
}

/// Non-Unix fallback: there is no signal concept to map, so a missing exit
/// code becomes `None` (unreadable) rather than the misleading default of
/// success.
#[cfg(not(unix))]
fn exit_info_from_status(status: ExitStatus) -> ExitInfo {
    ExitInfo {
        code: status.code(),
        by_signal: false,
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

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn wait_exit_mapping_passes_normal_exit_code_through() {
        // raw wait-status encoding: exit code lives in bits 8-15
        let status = ExitStatus::from_raw(5 << 8);
        let info = exit_info_from_status(status);
        assert_eq!(info.code, Some(5));
        assert!(!info.by_signal);
    }

    #[test]
    fn wait_exit_mapping_maps_signal_death_to_128_plus_signal() {
        // raw wait-status encoding: a nonzero low 7 bits with no exit code
        // means "terminated by signal N", here SIGKILL (9)
        let status = ExitStatus::from_raw(9);
        let info = exit_info_from_status(status);
        assert_eq!(info.code, Some(137));
        assert!(info.by_signal);
    }
}
