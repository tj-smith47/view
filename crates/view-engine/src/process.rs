//! Spawning `nvim --embed` and performing the API-info handshake.

use crate::handle::{EngineError, EngineHandle, EngineNotification};
use rmpv::Value;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;

/// Configuration for spawning an embedded Neovim process.
pub struct EngineConfig {
    /// Path to the `nvim` binary. Defaults to `"nvim"`, resolved via `PATH`;
    /// release packaging replaces this default with a bundled binary path.
    pub nvim_bin: PathBuf,
    /// Additional arguments passed to `nvim` after `--embed`.
    pub extra_args: Vec<OsString>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            nvim_bin: PathBuf::from("nvim"),
            extra_args: vec![],
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
pub struct Engine {
    /// The RPC client for issuing requests to the engine.
    pub handle: EngineHandle,
    /// Receiver for notifications (e.g., `redraw` events) from the engine.
    pub notifications: Receiver<EngineNotification>,
    /// The child process handle, for lifecycle control (e.g., `kill`).
    pub child: Child,
    /// The engine's API version and channel id, captured at handshake time.
    pub api_info: ApiInfo,
}

impl Engine {
    /// Spawns `nvim --embed` per `cfg` and performs the `nvim_get_api_info`
    /// handshake.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Io` if the process fails to spawn or its pipes
    /// cannot be captured, or the error returned by the handshake request
    /// itself (`EngineError::Rpc`, `Remote`, or `Closed`).
    pub fn spawn(cfg: EngineConfig) -> Result<Self, EngineError> {
        let mut child = Command::new(&cfg.nvim_bin)
            .arg("--embed")
            .args(&cfg.extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = child.stdout.take().ok_or(EngineError::Closed)?;
        let stdin = child.stdin.take().ok_or(EngineError::Closed)?;
        let (handle, notifications) = EngineHandle::start(stdout, stdin);
        let api_info = decode_api_info(handle.request("nvim_get_api_info", vec![])?)?;
        Ok(Self {
            handle,
            notifications,
            child,
            api_info,
        })
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
