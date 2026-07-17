//! msgpack-RPC message codec: the wire encoding used to talk to the
//! embedded nvim process. Converts between `RpcMessage` and the tagged
//! msgpack arrays nvim's RPC API sends and expects.

pub mod msg;
pub use msg::{RpcError, RpcMessage};
