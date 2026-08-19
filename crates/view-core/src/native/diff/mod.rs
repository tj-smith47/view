//! The diff review's engine: turning an agent's whole-file before/after
//! pair into reviewable hunks ([`hunk`]), and keeping those hunks anchored
//! to a buffer the user goes on editing underneath them ([`rebase`]).
//!
//! Pure data and pure functions, on the same terms as the rest of
//! `view-core`: nothing here reads a buffer, issues an RPC, or holds
//! authoritative text. A hunk describes an edit; only
//! [`crate::msg::RpcCall::BufSetText`] ever performs one.

pub mod hunk;
pub mod rebase;

pub use hunk::{Hunk, HunkStatus};
pub use rebase::{rebase, BufTextChangedEvent};
