//! The messages `Executor` answers an effect with itself, and the parking
//! that keeps delivering one from ever blocking the loop thread it runs on.
//! Split out of `runtime.rs` to keep that file's effect dispatcher under the
//! crate's production-line ceiling.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use view_core::msg::Msg;

/// Messages the executor answers an effect with itself -- a write outcome
/// (`Msg::BufWriteApplied`/`Msg::BufWriteRefused`) and the
/// `Msg::HiddenBufferLoaded` a locally-refused `RpcCall::LoadHidden` stands
/// in for -- held here when the loop's message channel is too full to take
/// one, in arrival order.
///
/// These are the only messages the executor produces on the loop thread
/// itself, which is also the channel's only consumer: a blocking send there
/// deadlocks the editor outright, and the channel is at its fullest exactly
/// when a write completes (an open review holds a buffer subscription, so
/// every keystroke in that buffer is already queueing a
/// `Msg::BufTextChanged` and a redraw behind it). Dropping is not the
/// alternative either -- a lost outcome leaves the review believing a write
/// is still in flight and naming a tick the buffer has moved past, so every
/// later accept is refused for no reason -- so a refused message parks here
/// and is carried through by [`LoopMsgOutbox::flush`], the same
/// hold-and-retry shape `view_engine`'s `Route` uses for the replies its
/// reader thread must deliver without blocking.
///
/// A queue rather than a single slot: each message describes a distinct
/// event, and none supersedes the one before it. A `Mutex` because the
/// executor's own `run` takes `&self`.
#[derive(Default)]
pub(crate) struct LoopMsgOutbox {
    parked: Mutex<VecDeque<Msg>>,
}

impl LoopMsgOutbox {
    /// Routes one message to the loop, blocking on nothing and dropping
    /// nothing (see this type's own doc for why neither is available). A
    /// refused message parks, and one already parked makes this one queue
    /// behind it rather than overtake it: a write refusal reported ahead of
    /// the apply that preceded it would put hunks back that the buffer
    /// already holds.
    ///
    /// `tx` is `None` only for a bare test `Executor`, which has no loop to
    /// deliver to at all -- the same degrade every other channel that type
    /// carries takes when it is not wired.
    pub(crate) fn route(&self, tx: Option<&crate::wake::LoopSender>, msg: Msg) {
        let Some(tx) = tx else {
            return;
        };
        let mut parked = self.parked.lock().unwrap_or_else(PoisonError::into_inner);
        retry(tx, &mut parked);
        if !parked.is_empty() {
            parked.push_back(msg);
            return;
        }
        if let Err(std::sync::mpsc::TrySendError::Full(msg)) = tx.try_send(msg) {
            parked.push_back(msg);
        }
    }

    /// Re-attempts every parked message, in arrival order, stopping at the
    /// first the channel still refuses.
    ///
    /// Called once per loop pass, at the top: what it carries is therefore
    /// what the *previous* pass parked, and a message parked later in this
    /// pass -- the resize and supervision dispatches both run after it --
    /// waits for the next pass. That wait is bounded by the same fact that
    /// made the parking necessary: a channel too full to take a message is
    /// a channel with something in it, and the loop's wait returns
    /// immediately while anything is queued, so the next pass is the next
    /// thing that happens rather than whatever the user does next.
    ///
    /// Costs one uncontended lock and a `VecDeque::is_empty` per pass,
    /// which is the whole steady state.
    pub(crate) fn flush(&self, tx: Option<&crate::wake::LoopSender>) {
        let mut parked = self.parked.lock().unwrap_or_else(PoisonError::into_inner);
        if parked.is_empty() {
            return;
        }
        let Some(tx) = tx else {
            return;
        };
        retry(tx, &mut parked);
    }
}

/// Hands `parked` back to `tx` in arrival order, stopping at the first the
/// channel still refuses: a full channel would refuse everything queued
/// behind it too, and delivering a later message first would report a write
/// as refused before the one it was joined onto was reported as applied. A
/// disconnected channel means the loop those messages described is gone, so
/// what is left goes with it rather than waiting for a delivery nothing can
/// make.
fn retry(tx: &crate::wake::LoopSender, parked: &mut VecDeque<Msg>) {
    while let Some(msg) = parked.pop_front() {
        if let Err(std::sync::mpsc::TrySendError::Full(msg)) = tx.try_send(msg) {
            parked.push_front(msg);
            break;
        }
    }
}
