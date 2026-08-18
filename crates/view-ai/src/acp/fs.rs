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
}

impl PendingFsReplies {
    /// Registers `reply` against `id`, which the caller allocated from the
    /// session's single boundary-id counter -- the registry does not number
    /// requests itself, so filesystem ids and permission ids cannot collide.
    ///
    /// A poisoned lock drops the registration rather than propagating: the
    /// caller's `await` on the paired receiver then resolves as a closed
    /// channel, which is already the path a lost answer takes, and a panic
    /// here would take the session task down with it.
    pub fn register(&self, id: u64, reply: PendingReply) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(id, reply);
        }
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
        let id = 7;
        pending.register(id, PendingReply::Read(tx));

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
    async fn each_id_resolves_to_the_channel_registered_against_it() {
        let pending = PendingFsReplies::default();
        let (read_tx, _read_rx) = oneshot::channel();
        let (write_tx, write_rx) = oneshot::channel();
        let (read_id, write_id) = (11, 12);
        pending.register(read_id, PendingReply::Read(read_tx));
        pending.register(write_id, PendingReply::Write(write_tx));

        let Some(PendingReply::Write(sender)) = pending.take(write_id) else {
            panic!("the write id resolves to the write channel")
        };
        sender.send(Err(FsError::PermissionDenied)).unwrap();
        assert_eq!(write_rx.await.unwrap(), Err(FsError::PermissionDenied));
    }
}
