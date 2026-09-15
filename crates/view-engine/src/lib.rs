//! Embedded Neovim lifecycle and msgpack-RPC client.

use std::sync::OnceLock;

pub mod damage;
pub mod env;
pub mod handle;
pub mod heartbeat;
mod hidden_buffers;
pub mod nvim_api;
mod outbox;
pub mod process;
pub mod rpc;
pub mod stall;
#[cfg(all(unix, feature = "bench-taps"))]
mod tap;
// test-only: a peer that parks inside a write, which no real engine can be
// asked to become, shared by every crate whose tests need one wedged
#[cfg(any(test, feature = "test-support"))]
pub mod test_peer;
pub mod ui_events;
// the engine's stdin channel has to be built a particular way on Windows for
// the outbox's inline path to be able to ask about it at all
#[cfg(windows)]
mod winpipe;
mod wire;

pub use damage::{DamagePump, SinkCutover};
pub use handle::{EngineError, EngineHandle};
pub use heartbeat::{
    wedge_kind, HeartbeatProber, HeartbeatWatch, Liveness, HEARTBEAT_PROBE_INTERVAL,
    HEARTBEAT_WEDGE_THRESHOLD,
};
// test-only: EngineNotification is the type EngineHandle::start's unbounded
// channel carries, and that constructor is itself test-support-gated (see
// handle.rs) since production always goes through the pumped, bounded path.
#[cfg(any(test, feature = "test-support"))]
pub use handle::EngineNotification;
pub use nvim_api::{
    ui_ext_options_shipped, ui_ext_options_shipped_multigrid, MULTIGRID_NAME, UI_EXT_OPTIONS,
    UI_EXT_OPTIONS_MULTIGRID,
};
pub use process::{
    remote_reconnect_backoff, ApiInfo, Engine, EngineConfig, RemoteSpec, ShutdownOutcome,
    ShutdownPath, REMOTE_RECONNECT_BACKOFF_BASE, REMOTE_RECONNECT_MAX_ATTEMPTS,
};
pub use rpc::{RpcError, RpcMessage};
pub use stall::{OutboxStallWatch, WRITER_STALL_THRESHOLD};

/// Where this crate's own diagnostic lines go, for a session that opened a
/// log to receive them.
///
/// The crate has no logger of its own and cannot have one: the `VIEW_LOG`
/// sink is opened by the binary, which sits above every library here, and
/// the reader thread that sees a malformed notification is several layers
/// below it. So the binary hands this crate a writer once, at startup, and
/// a session that opened no log leaves it unset -- one `OnceLock` read per
/// call site and nothing formatted.
static DIAGNOSTICS: OnceLock<fn(&str)> = OnceLock::new();

/// Installs the writer [`diagnose`] hands its lines to, under whatever
/// topic the caller logs them at.
///
/// First call wins and later ones are ignored, the [`OnceLock`] contract:
/// the sink belongs to the process, not to an engine, and a session that
/// restarts its engine keeps writing to the log it opened.
pub fn set_diagnostics(sink: fn(&str)) {
    let _ = DIAGNOSTICS.set(sink);
}

/// Writes one diagnostic line, building the payload only once a sink is
/// known to exist -- the same closure shape, and for the same reason, as
/// the logger on the other side of [`set_diagnostics`].
pub(crate) fn diagnose(payload: impl FnOnce() -> String) {
    if let Some(sink) = DIAGNOSTICS.get() {
        sink(&payload());
    }
}
