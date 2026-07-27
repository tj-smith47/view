//! Embedded Neovim lifecycle and msgpack-RPC client.

pub mod damage;
pub mod env;
pub mod handle;
pub mod nvim_api;
mod outbox;
pub mod process;
pub mod rpc;
#[cfg(feature = "bench-taps")]
mod tap;
pub mod ui_events;
mod wire;

pub use damage::{DamagePump, SinkCutover};
pub use handle::{EngineError, EngineHandle};
// test-only: EngineNotification is the type EngineHandle::start's unbounded
// channel carries, and that constructor is itself test-support-gated (see
// handle.rs) since production always goes through the pumped, bounded path.
#[cfg(any(test, feature = "test-support"))]
pub use handle::EngineNotification;
pub use process::{ApiInfo, Engine, EngineConfig, ShutdownOutcome, ShutdownPath};
pub use rpc::{RpcError, RpcMessage};
