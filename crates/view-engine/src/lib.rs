//! Embedded Neovim lifecycle and msgpack-RPC client.

pub mod damage;
pub mod handle;
pub mod nvim_api;
pub mod process;
pub mod rpc;
pub mod ui_events;

pub use damage::{DamagePump, SinkCutover};
pub use handle::{EngineError, EngineHandle, EngineNotification};
pub use process::{ApiInfo, Engine, EngineConfig};
pub use rpc::{RpcError, RpcMessage};
