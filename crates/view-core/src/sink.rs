//! The producer-side seam over the runtime loop's [`Msg`] channel.
//!
//! Every thread that feeds the loop -- the engine's RPC reader, the picker
//! matcher worker, one-shot timer threads -- sends through this trait
//! rather than a concrete `SyncSender<Msg>`, so the loop's owner can pair
//! the queue with a wakeup mechanism of its own (the runtime loop sleeps
//! in an fd readiness poll, which a bare channel send cannot interrupt)
//! without this crate knowing anything about file descriptors. The trait
//! reuses `std::sync::mpsc`'s error vocabulary so a plain `SyncSender<Msg>`
//! -- every test harness, and any consumer without a poll-based loop --
//! satisfies it verbatim through the blanket impl below.

use crate::msg::Msg;
use std::sync::mpsc::{SendError, SyncSender, TrySendError};

/// A destination for runtime-loop messages, with the same non-blocking /
/// blocking split `SyncSender` has: producers on threads that must never
/// block (the RPC reader) use [`try_send`](Self::try_send), producers with
/// nothing else left to do use [`send`](Self::send).
pub trait MsgSink {
    /// Queues `msg` without blocking, failing when the channel is full or
    /// its receiver is gone.
    ///
    /// # Errors
    ///
    /// Returns [`TrySendError::Full`] when the bounded channel has no
    /// capacity left, [`TrySendError::Disconnected`] when the receiver is
    /// gone; both hand `msg` back to the caller.
    fn try_send(&self, msg: Msg) -> Result<(), TrySendError<Msg>>;

    /// Queues `msg`, blocking while the channel is full.
    ///
    /// # Errors
    ///
    /// Returns [`SendError`] (handing `msg` back) when the receiver is
    /// gone.
    fn send(&self, msg: Msg) -> Result<(), SendError<Msg>>;
}

impl MsgSink for SyncSender<Msg> {
    fn try_send(&self, msg: Msg) -> Result<(), TrySendError<Msg>> {
        SyncSender::try_send(self, msg)
    }

    fn send(&self, msg: Msg) -> Result<(), SendError<Msg>> {
        SyncSender::send(self, msg)
    }
}

// the &T forward mirrors `EngineOps`'s blanket impl in the bin crate: a
// worker holding a borrowed sink sends through the same code path an owned
// one does, without a second set of signatures
impl<T: MsgSink + ?Sized> MsgSink for &T {
    fn try_send(&self, msg: Msg) -> Result<(), TrySendError<Msg>> {
        (**self).try_send(msg)
    }

    fn send(&self, msg: Msg) -> Result<(), SendError<Msg>> {
        (**self).send(msg)
    }
}
