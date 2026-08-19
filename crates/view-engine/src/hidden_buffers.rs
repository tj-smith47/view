//! Per-path refcounted bookkeeping for `RpcCall::LoadHidden`/
//! `RpcCall::ReleaseHidden`, split out of `handle.rs` to keep that file's
//! reader thread under the crate's production-line ceiling. Holds the shape
//! of one hold, the delete-decision logic shared between a caller's
//! decrement and the reader thread's own resolve (the reader thread only has
//! `EngineHandle::hidden_bufs`'s `Arc<Mutex<..>>` directly, not a full
//! `EngineHandle`, so that half is free functions rather than methods), and
//! the `EngineHandle` methods a `load_hidden`/`release_hidden` caller drives
//! directly. `hidden_bufs` itself is `pub(crate)` on `EngineHandle` for
//! exactly this split to reach it from a second `impl` block here.

use crate::handle::EngineHandle;
use rmpv::Value;
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

/// One path's hidden-buffer hold: the buffer `RpcCall::LoadHidden` last
/// resolved it to (`None` until that reply lands), whether this connection
/// itself is what created that buffer, and how many un-released calls are
/// still holding it open. See `EngineHandle::hidden_bufs`.
///
/// An entry is inserted by `EngineHandle::note_hidden_acquire` at
/// `load_hidden` issue time, before any request has even reached the wire,
/// and only ever updated afterward -- never re-inserted -- by whichever of
/// `EngineHandle::note_hidden_release` (a caller decrementing the count) or
/// the reader thread's own resolve (filling in `buf`/`owned`/`answered` once
/// `load_hidden`'s reply decodes) reaches a zero count with its reply landed
/// last. Symmetric by construction: every `load_hidden` call takes exactly
/// one hold up front, so every matching `release_hidden` call always has a
/// count to decrement no matter which of the two arrives first.
pub(crate) struct HiddenHold {
    /// `None` until `load_hidden`'s reply names a real buffer. Only ever
    /// set, never cleared back to `None` -- a later reply for the same path
    /// that names no buffer (a refused load racing an already-successful
    /// one) leaves a previously-resolved handle alone rather than losing
    /// track of it, and so does one naming a *different* buffer -- see
    /// [`resolve_hidden_hold`] for why a second buffer under one key is
    /// refused rather than adopted.
    pub(crate) buf: Option<view_core::msg::BufferHandle>,
    /// Whether any of this path's `load_hidden` calls reported
    /// [`Created::Yes`]. OR'd into the entry rather than overwritten on each
    /// reply: once one call's reply says this connection made the buffer, a
    /// later reply for the same path finding it already there
    /// (`Created::No`, because that call's own scan raced behind the first)
    /// must not erase that fact. The delete-on-release decision reads this
    /// flag, not `Created` directly, because a bare per-call create/reuse
    /// flag would let two overlapping holders on the same path race each
    /// other's cleanup -- only one of them could ever be told "you created
    /// it, you may delete it" and the other would have nothing to decide by.
    pub(crate) owned: bool,
    pub(crate) count: u64,
    /// Whether a `load_hidden` reply for this path has landed yet, set once
    /// by [`resolve_hidden_hold`] regardless of what it decoded. A refused
    /// or errored load still answers `buf: None`, and without this flag
    /// [`take_hidden_delete`] could not tell that apart from a reply still
    /// in flight -- the former must let a zero-count entry be removed once
    /// and for all, the latter must leave it for the reply that is still
    /// coming.
    pub(crate) answered: bool,
}

impl HiddenHold {
    pub(crate) fn fresh() -> Self {
        Self {
            buf: None,
            owned: false,
            count: 1,
            answered: false,
        }
    }
}

/// If `path`'s hold in `holds` has both a zero count and a landed reply,
/// decides whether to delete it and removes the entry either way. Shared
/// tail for `EngineHandle::note_hidden_release` (whose own decrement can
/// bring the count to zero) and [`resolve_hidden_hold`] (whose reply can be
/// the one that lands after the count already reached zero) -- both need
/// the identical "count zero, reply landed, only delete what this
/// connection made" decision, just reached from different directions, and
/// the entry must be removed by whichever of the two resolves last so the
/// other is never left recording an already-decided hold.
///
/// Gating removal on `answered` rather than `buf.is_some()` is what lets a
/// refused or errored load's entry be cleared too: `buf` stays `None` for
/// that reply exactly like one still in flight, and only `answered`
/// distinguishes "nothing is ever coming to name a buffer here" from "the
/// reply has not landed yet, wait for it."
pub(crate) fn take_hidden_delete(
    holds: &mut HashMap<String, HiddenHold>,
    path: &str,
) -> Option<view_core::msg::BufferHandle> {
    let hold = holds.get(path)?;
    if hold.count > 0 || !hold.answered {
        return None;
    }
    let resolved = holds.remove(path)?;
    if resolved.owned {
        resolved.buf
    } else {
        None
    }
}

/// Applies a `Waiter::LoadHidden` reply to `path`'s hold: records the
/// buffer and ownership `load_hidden`'s reply named (if any), then attempts
/// [`take_hidden_delete`]. Called from the reader thread, which only has
/// `hidden_bufs`'s `Arc<Mutex<..>>` directly rather than a full
/// `EngineHandle` -- taking the map by reference here (instead of adding a
/// second `EngineHandle` method) is what lets that thread call this without
/// one.
///
/// Returns the buffer to delete now, iff this connection's own count for
/// `path` already reached zero before this reply arrived -- a
/// `release_hidden` call that overtook its own `load_hidden`'s reply. The
/// caller (the reader thread) is responsible for actually issuing
/// `RELEASE_HIDDEN_CHUNK` for it, since nothing else is left to.
///
/// A reply naming a *different* buffer than this hold already recorded is
/// refused rather than folded in: one key over two buffers is the exact
/// shape that let a hold created for one buffer inherit `owned` over
/// another, so the recorded buffer and its ownership are left exactly as
/// they were and no delete can ever be authorized by a buffer this hold was
/// not keyed on. The refused buffer outlives the connection unreleased,
/// which is the safe half of the trade -- adopting it would hand this
/// hold's delete decision to a buffer whichever holder resolved onto it is
/// still using. Unreachable from the spellings that can reach a hold at all
/// (`hidden_path_refusal` refuses every one `bufadd` and
/// `canonical_hidden_key` resolve differently), and kept as an explicit
/// branch so a future spelling that reopens the class leaks a buffer
/// instead of reopening the foreign-buffer delete.
pub(crate) fn resolve_hidden_hold(
    hidden_bufs: &Mutex<HashMap<String, HiddenHold>>,
    path: &str,
    buf: Option<view_core::msg::BufferHandle>,
    created: bool,
) -> Option<view_core::msg::BufferHandle> {
    let mut holds = hidden_bufs.lock().unwrap_or_else(PoisonError::into_inner);
    let hold = holds.get_mut(path)?;
    hold.answered = true;
    match (hold.buf, buf) {
        (Some(recorded), Some(named)) if recorded != named => {}
        (_, Some(named)) => {
            hold.buf = Some(named);
            hold.owned |= created;
        }
        (_, None) => {}
    }
    take_hidden_delete(&mut holds, path)
}

/// Whether one `EngineHandle::load_hidden` call created the buffer it
/// resolved to, or found one already there (a previous `load_hidden` call's
/// hold, or a real window's own buffer). Folded into [`HiddenHold::owned`]
/// rather than read directly by `EngineHandle::release_hidden`'s delete
/// decision -- see that field's own doc for why a bare per-call flag cannot
/// answer it alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    /// This call's resolve produced the buffer.
    Yes,
    /// This call found and reused a buffer that already existed.
    No,
}

/// Decodes a `LOAD_HIDDEN_CHUNK` reply into the handle nvim answered with. A
/// non-positive or absent number is `None` rather than a handle: `buf = 0`
/// is the chunk's own "refused" answer (a directory, a path `bufadd` would
/// not take), and `BufferHandle(0)` is separately nvim's "current buffer"
/// sentinel, which `RpcCall::BufAttach`'s own doc forbids -- attaching under
/// it would record a generation no event this attach produces can ever be
/// looked up by. `created` degrades to [`Created::No`] on the same
/// absent-handle path: there is no buffer to have created either way.
pub(crate) fn decode_load_hidden_reply(
    result: &Value,
) -> (Option<view_core::msg::BufferHandle>, Created, u64) {
    let Some(pairs) = result.as_map() else {
        return (None, Created::No, 0);
    };
    let handle = crate::wire::map_find(pairs, "buf")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if handle == 0 {
        return (None, Created::No, 0);
    }
    let created = crate::wire::map_find(pairs, "created")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let changedtick = crate::wire::map_find(pairs, "changedtick")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (
        Some(view_core::msg::BufferHandle(handle)),
        if created { Created::Yes } else { Created::No },
        changedtick,
    )
}

impl EngineHandle {
    /// Takes one hold on `path`'s hidden buffer, called from
    /// [`crate::nvim_api::EngineHandle::load_hidden`] before its request
    /// even reaches the wire. Inserts a fresh entry (`buf: None, owned:
    /// false, count: 1`) the first time `path` is held, or increments an
    /// existing one -- either way, the entry exists the instant this method
    /// returns, so [`note_hidden_release`](Self::note_hidden_release)
    /// always has a count to decrement regardless of how the matching
    /// `load_hidden` call's reply is timed.
    pub(crate) fn note_hidden_acquire(&self, path: &str) {
        let mut holds = self
            .hidden_bufs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        holds
            .entry(path.to_owned())
            .and_modify(|hold| hold.count += 1)
            .or_insert_with(HiddenHold::fresh);
    }

    /// Reverses [`note_hidden_acquire`](Self::note_hidden_acquire) for a
    /// `load_hidden` call whose own request never reached the wire, called
    /// from [`crate::nvim_api::EngineHandle::load_hidden`] only on that
    /// `Err` path. That one call's own request has no reply coming, but
    /// `path`'s entry can still be shared with a concurrent `load_hidden`
    /// call whose request *did* reach the wire and whose reply has not
    /// landed yet -- so the removal decision below goes through
    /// [`take_hidden_delete`], the identical `!hold.answered`-gated check
    /// [`note_hidden_release`](Self::note_hidden_release) and
    /// [`resolve_hidden_hold`] already share, rather than a third copy of
    /// its own that could drift from theirs. Gating on `hold.buf.is_none()`
    /// here (as this used to) is exactly the bug [`HiddenHold::answered`]
    /// exists to close: it cannot tell a reply that has not landed yet from
    /// one that landed naming no buffer, so a still-pending concurrent
    /// reply's entry could be removed out from under it by this call's own
    /// unrelated send failure.
    pub(crate) fn note_hidden_acquire_failed(&self, path: &str) {
        let mut holds = self
            .hidden_bufs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(hold) = holds.get_mut(path) {
            hold.count = hold.count.saturating_sub(1);
        }
        take_hidden_delete(&mut holds, path);
    }

    /// Decrements `path`'s hidden-buffer hold count, called from
    /// [`crate::nvim_api::EngineHandle::release_hidden`]. Returns the
    /// buffer to delete iff this decrement brought the count to zero, the
    /// matching `load_hidden` call's reply has already landed, and that
    /// reply named a buffer this connection owns -- `None` for a `path` with
    /// no recorded hold at all (a defensive extra release), one whose count
    /// is still above zero after decrementing (another holder still needs
    /// it), one whose reply has not landed yet ([`resolve_hidden_hold`]
    /// finishes the decision when it lands, from the reader thread), or one
    /// whose reply already landed naming no buffer at all (a refused or
    /// errored load -- nothing to delete, and the entry is still removed so
    /// it does not linger for the rest of this connection's life).
    ///
    /// A poisoned lock is recovered rather than degrading to "nothing to
    /// delete": the reader thread's own handling of this same map already
    /// recovers a poison the same way, and disagreeing between the two would
    /// let one path double-count a hold the other just cleared.
    pub(crate) fn note_hidden_release(&self, path: &str) -> Option<view_core::msg::BufferHandle> {
        let mut holds = self
            .hidden_bufs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let hold = holds.get_mut(path)?;
        hold.count = hold.count.saturating_sub(1);
        take_hidden_delete(&mut holds, path)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use view_core::msg::BufferHandle;

    fn holds_with(hold: HiddenHold) -> Mutex<HashMap<String, HiddenHold>> {
        Mutex::new(HashMap::from([("/tmp/one.rs".to_owned(), hold)]))
    }

    /// One key, two buffers: the reply naming the second must not become
    /// the hold's buffer, and -- the part that matters -- must not hand the
    /// hold `owned` over a buffer it was never keyed on. That is the
    /// foreign-buffer delete, reached through the key instead of through
    /// the gate.
    #[test]
    fn a_reply_naming_a_second_buffer_never_overwrites_the_hold_or_its_ownership() {
        let holds = holds_with(HiddenHold {
            buf: Some(BufferHandle(2)),
            owned: false,
            count: 1,
            answered: true,
        });
        assert_eq!(
            resolve_hidden_hold(&holds, "/tmp/one.rs", Some(BufferHandle(3)), true),
            None
        );
        let guard = holds.lock().unwrap();
        let hold = guard.get("/tmp/one.rs").expect("the hold survives");
        assert_eq!(
            hold.buf,
            Some(BufferHandle(2)),
            "the hold must keep the buffer it was resolved to first"
        );
        assert!(
            !hold.owned,
            "a buffer this hold was not keyed on must never authorize its delete"
        );
    }

    /// The ordinary path is untouched: a reply naming the buffer already
    /// recorded still folds its `created` in, so a second holder's own
    /// create is not lost to the mismatch branch.
    #[test]
    fn a_reply_naming_the_recorded_buffer_still_folds_its_ownership_in() {
        let holds = holds_with(HiddenHold {
            buf: Some(BufferHandle(2)),
            owned: false,
            count: 1,
            answered: true,
        });
        assert_eq!(
            resolve_hidden_hold(&holds, "/tmp/one.rs", Some(BufferHandle(2)), true),
            None
        );
        let guard = holds.lock().unwrap();
        let hold = guard.get("/tmp/one.rs").expect("the hold survives");
        assert_eq!(hold.buf, Some(BufferHandle(2)));
        assert!(hold.owned, "the same buffer's own create still counts");
    }
}
