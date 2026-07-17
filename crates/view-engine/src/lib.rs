//! Embedded Neovim lifecycle and msgpack-RPC client.

pub mod handle;
pub mod rpc;

pub use handle::{EngineError, EngineHandle, EngineNotification};
pub use rpc::{RpcError, RpcMessage};
