//! Embedded Neovim lifecycle and msgpack-RPC client.

pub mod handle;
pub mod process;
pub mod rpc;
pub mod ui_events;

pub use handle::{EngineError, EngineHandle, EngineNotification};
pub use process::{ApiInfo, Engine, EngineConfig};
pub use rpc::{RpcError, RpcMessage};
