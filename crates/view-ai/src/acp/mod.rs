//! The Agent Client Protocol adapter: the wire, the session that speaks it,
//! and the correlation state between them.

mod driver;
pub(crate) mod fs;
pub mod session;
pub mod wire;
