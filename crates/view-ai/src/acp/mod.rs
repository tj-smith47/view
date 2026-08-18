//! The Agent Client Protocol adapter: the wire, the session that speaks it,
//! and the correlation state between them.
//!
//! Every module here is crate-private. `AiSession` reaches the outside
//! through one re-export at the crate root; the JSON-RPC frame types are the
//! wire format this crate exists to keep out of everyone else's sight, so
//! none of them is public API.

mod driver;
pub(crate) mod fs;
pub(crate) mod session;
pub(crate) mod wire;
