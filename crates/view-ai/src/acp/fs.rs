//! The correlation map for agent-initiated filesystem requests.
//!
//! An agent's `fs/read_text_file` and `fs/write_text_file` are requests
//! addressed to this client, and this client cannot answer them itself: the
//! authoritative text lives in nvim, which only `view-engine` may speak to.
//! So the request leaves as an [`AiEvent`](view_core::native::ai_event::AiEvent)
//! and its answer comes back as an
//! [`AiCommand`](view_core::native::ai_event::AiCommand) through the
//! ordinary effect path, and this map is what holds the two ends together
//! across that round trip.
//!
//! The map never crosses the crate boundary. `view-core` sees only the
//! closed event and command vocabulary; the ids it carries are opaque to it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use view_core::native::ai_event::FsError;

/// One outstanding filesystem request's reply channel, in the shape its
/// answer arrives in.
#[derive(Debug)]
pub enum PendingReply {
    /// Awaiting a `FsReadReply`.
    Read(oneshot::Sender<Result<String, FsError>>),
    /// Awaiting a `FsWriteReply`.
    Write(oneshot::Sender<Result<(), FsError>>),
}

/// The `request_id`-keyed registry of outstanding filesystem requests.
///
/// A plain `Mutex` rather than an async one: every operation is a hash-map
/// insert or remove, so the lock is held for a handful of instructions and
/// never across an await -- an async mutex here would buy nothing and cost
/// a scheduling hop per filesystem request.
#[derive(Debug, Clone, Default)]
pub struct PendingFsReplies {
    inner: Arc<Mutex<HashMap<u64, PendingReply>>>,
    next_id: Arc<AtomicU64>,
}

impl PendingFsReplies {
    /// Allocates a fresh `request_id`, registers `reply` against it, and
    /// returns the id the answering command must carry.
    ///
    /// A poisoned lock drops the registration rather than propagating: the
    /// caller's `await` on the paired receiver then resolves as a closed
    /// channel, which is already the path a lost answer takes, and a panic
    /// here would take the session task down with it.
    pub fn register(&self, reply: PendingReply) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = self.inner.lock() {
            map.insert(id, reply);
        }
        id
    }

    /// Removes and returns the reply channel registered against `id`, or
    /// `None` if nothing was waiting on it -- a duplicate or invented
    /// answer, which is dropped rather than acted on.
    #[must_use]
    pub fn take(&self, id: u64) -> Option<PendingReply> {
        self.inner.lock().ok()?.remove(&id)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[tokio::test]
    async fn a_registered_read_is_answered_exactly_once() {
        let pending = PendingFsReplies::default();
        let (tx, rx) = oneshot::channel();
        let id = pending.register(PendingReply::Read(tx));

        let Some(PendingReply::Read(sender)) = pending.take(id) else {
            panic!("the registered read is the one taken")
        };
        sender.send(Ok("fn main() {}".to_string())).unwrap();
        assert_eq!(rx.await.unwrap().unwrap(), "fn main() {}");

        assert!(
            pending.take(id).is_none(),
            "a second answer to the same id finds nothing waiting"
        );
    }

    #[tokio::test]
    async fn ids_are_distinct_across_both_request_kinds() {
        let pending = PendingFsReplies::default();
        let (read_tx, _read_rx) = oneshot::channel();
        let (write_tx, write_rx) = oneshot::channel();
        let read_id = pending.register(PendingReply::Read(read_tx));
        let write_id = pending.register(PendingReply::Write(write_tx));
        assert_ne!(read_id, write_id);

        let Some(PendingReply::Write(sender)) = pending.take(write_id) else {
            panic!("the write id resolves to the write channel")
        };
        sender.send(Err(FsError::PermissionDenied)).unwrap();
        assert_eq!(write_rx.await.unwrap(), Err(FsError::PermissionDenied));
    }
}
