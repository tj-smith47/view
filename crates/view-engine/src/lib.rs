//! Embedded Neovim lifecycle and msgpack-RPC client.

pub mod damage;
pub mod env;
pub mod handle;
pub mod heartbeat;
pub mod nvim_api;
mod outbox;
pub mod process;
pub mod rpc;
pub mod stall;
#[cfg(feature = "bench-taps")]
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
pub use process::{ApiInfo, Engine, EngineConfig, ShutdownOutcome, ShutdownPath};
pub use rpc::{RpcError, RpcMessage};
pub use stall::{OutboxStallWatch, WRITER_STALL_THRESHOLD};
