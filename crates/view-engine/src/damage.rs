//! Damage coalescing for the reader thread: folds decoded `redraw` batches
//! into a compacted buffer under one lock, and stages the small set of
//! non-coalescible engine-initiated `Msg`s until the runtime loop attaches
//! its bounded channel.
//!
//! # Bounded channel contract
//!
//! The runtime's `Msg` channel (created by the runtime, size 64) carries
//! four kinds of traffic with different loss tolerances:
//!
//! - **Coalescible**: `Msg::RedrawReady`. A `try_send` that fails because the
//!   channel is full means this fold's token never reached the channel, so
//!   `PumpShared::fold_redraw` disarms `DamageBuffer`'s `pending` flag
//!   back to `false` instead of leaving it armed for a token that was never
//!   sent. Staged damage is therefore always in exactly one of three states:
//!   a token already queued in the channel, a disarmed flag that the next
//!   fold will re-arm and retry, or an awake consumer already draining (once
//!   the runtime loop's residue drain exists, it can pick up staged damage
//!   with no token at all). No fold ever leaves damage staged with no path
//!   left to reach the consumer.
//! - **Non-coalescible, retriable-by-caller**: `Msg::EngineRequest`. A failed
//!   `try_send` here means the runtime loop is gone or wedged behind a full
//!   channel; there is no compaction that can recover a dropped request, and
//!   a dropped `EngineRequest` leaves the peer (nvim) blocked on its
//!   `rpcrequest` forever. The reader thread treats this as fatal: it stops
//!   reading further messages from the wire.
//! - **Non-coalescible, terminal**: `Msg::EngineStopped`. Sent with a
//!   blocking `send`, not `try_send`: the reader thread is already exiting
//!   when it sends this, so blocking costs nothing it was not already
//!   paying, while dropping it on a momentarily-full channel would be an
//!   unrecoverable correctness bug rather than a tolerable loss -- the pump
//!   retains a `SyncSender` clone for the channel's lifetime, so a lost
//!   `EngineStopped` also means the runtime's `recv()` never disconnects,
//!   hanging forever with no message left to wake it. If the sink is already
//!   disconnected, `send` returns `Err` immediately (nothing left to block
//!   on); that failure is safe to ignore, since a disconnected receiver has
//!   nothing left to signal.
//! - **Non-coalescible, retriable-by-pump**: `Msg::HlProbeReply`. Neither
//!   fatal nor droppable: the reader thread must not block, but a lost reply
//!   silently strands the theme's confirmed background for the rest of the
//!   session, since only a later colorscheme change issues a fresh probe.
//!   `PumpShared::route_probe_reply` therefore holds a refused reply in a
//!   single slot, and every later routing attempt retries it. One slot, not
//!   a queue: a newer generation's answer supersedes an older one outright.
//!
//! # Lock design
//!
//! The `pending` wakeup flag lives inside the same [`Mutex`] as the staged
//! events, not in a separate atomic. Folding and arming the flag happen
//! under one lock hold (`DamageBuffer::fold_batch`); clearing the flag and
//! draining the buffer happen under another single lock hold
//! (`DamageBuffer::take`). A design with the flag outside the buffer's
//! lock can lose a wakeup: a fold landing between the drain and the flag
//! clear would see `pending == true` already and skip sending a token, yet
//! the drain that already ran never observed that fold's event, leaving it
//! staged with nothing to wake the consumer for it.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::TrySendError;
use std::sync::{Arc, Mutex, PoisonError};

use view_core::sink::MsgSink;

use view_core::events::UiEvent;
use view_core::msg::Msg;

/// One event staged in a [`DamageBuffer`], tombstoned rather than physically
/// removed when compaction supersedes it: removal-by-index would shift the
/// [`DamageBuffer::flush_index`] boundary on every drop, while a tombstone
/// (`alive = false`) is filtered out at drain time and leaves every other
/// index stable.
struct StagedEvent {
    event: UiEvent,
    alive: bool,
    /// The owning grid's `barrier_epoch` at staging time (0 for non-grid
    /// events). See [`GridEpochs`].
    barrier_epoch: u64,
    /// The owning grid's `scroll_epoch` at staging time (0 for non-grid
    /// events). See [`GridEpochs`].
    scroll_epoch: u64,
}

/// Per-grid compaction-barrier counters. `barrier_epoch` bumps only on
/// `GridResize` and gates every drop rule (a resize is a full barrier: a
/// `GridClear`'s cell-content drop and a `GridLine`'s full-coverage elision
/// both refuse to touch anything staged at an older `barrier_epoch`).
/// `scroll_epoch` bumps on `GridResize` and `GridScroll` and additionally
/// gates `GridLine` elision specifically: a scroll relocates earlier rows,
/// so a later same-row write must not assume it supersedes content staged
/// before the scroll.
#[derive(Default, Clone, Copy)]
struct GridEpochs {
    barrier_epoch: u64,
    scroll_epoch: u64,
}

/// The grid id a [`UiEvent`] targets, or `None` for events with no grid
/// (highlight, mode, cmdline, message, tabline, popupmenu, `Flush`,
/// `Unknown`).
///
/// `WinViewport` is deliberately in the second group despite carrying a
/// grid id: every rule keyed on this answer drops or elides *cell content*,
/// and a viewport report is not cell content. Answering with its grid would
/// only expose it to those rules, and a dropped viewport is a retirement
/// `native::speculate` never gets to make.
fn grid_of(ev: &UiEvent) -> Option<u64> {
    match ev {
        UiEvent::GridResize { grid, .. }
        | UiEvent::GridLine { grid, .. }
        | UiEvent::GridCursorGoto { grid, .. }
        | UiEvent::GridScroll { grid, .. }
        | UiEvent::GridClear { grid } => Some(*grid),
        _ => None,
    }
}

/// Total cell width a `GridLine` run covers, i.e. the sum of every cell's
/// `repeat`. Saturating: a malformed wire value cannot overflow this into a
/// false "does not cover" verdict that would then wrongly retain a run that
/// really is superseded (retaining is always safe; the reverse is not).
fn grid_line_span(cells: &[view_core::events::GridCell]) -> u64 {
    cells
        .iter()
        .fold(0u64, |acc, c| acc.saturating_add(c.repeat))
}

/// Coalesced `redraw` damage plus the wakeup flag guarding it (see module
/// docs for why the flag lives here instead of a separate atomic).
///
/// Compaction never reorders events for the same cell and never drops an
/// event unless a later staged event already accounts for its entire
/// effect; over-retaining is always legal (a `Grid` is idempotent under
/// repeated painting), under-retaining never is.
#[derive(Default)]
pub(crate) struct DamageBuffer {
    staged: Vec<StagedEvent>,
    /// Index (inclusive) of the most recently staged `Flush` in `staged`,
    /// or `None` if nothing paintable has been staged since the last drain.
    flush_index: Option<usize>,
    grids: HashMap<u64, GridEpochs>,
    pending: bool,
}

impl DamageBuffer {
    /// Folds one decoded `redraw` batch (all events from a single
    /// notification), applying compaction, then arms the pending flag.
    ///
    /// Returns `true` iff this call transitioned the flag `false` -> `true`;
    /// the caller sends `Msg::RedrawReady` iff this returns `true` (already
    /// pending means a token is already in flight or will be re-sent by
    /// [`take`](Self::take)'s next caller).
    pub(crate) fn fold_batch(&mut self, events: impl IntoIterator<Item = UiEvent>) -> bool {
        for ev in events {
            self.fold_one(ev);
        }
        if self.pending {
            false
        } else {
            self.pending = true;
            true
        }
    }

    /// Index of the first `staged` slot compaction is allowed to touch: the
    /// slot after the most recently staged `Flush`, or `0` if nothing has
    /// been flushed since the last drain. The flushed prefix (everything at
    /// or before `flush_index`) is a batch [`take`](Self::take) may drain at
    /// any time, on any thread, independent of what folds after it; letting
    /// a later, still-unflushed event tombstone something in that prefix
    /// would let `take()` return a frame that never existed on the wire (an
    /// event dropped for a reason the drained batch itself gives no
    /// evidence of).
    fn compaction_start(&self) -> usize {
        self.flush_index.map_or(0, |i| i + 1)
    }

    fn fold_one(&mut self, ev: UiEvent) {
        let boundary = self.compaction_start();
        match &ev {
            UiEvent::GridResize { grid, .. } => {
                let e = self.grids.entry(*grid).or_default();
                e.barrier_epoch = e.barrier_epoch.saturating_add(1);
                e.scroll_epoch = e.scroll_epoch.saturating_add(1);
            }
            UiEvent::GridScroll { grid, .. } => {
                let e = self.grids.entry(*grid).or_default();
                e.scroll_epoch = e.scroll_epoch.saturating_add(1);
            }
            UiEvent::GridClear { grid } => {
                let epochs = self.grids.entry(*grid).or_default();
                let barrier = epochs.barrier_epoch;
                for s in &mut self.staged[boundary..] {
                    if !s.alive || s.barrier_epoch != barrier {
                        // dead already, or staged before an intervening
                        // resize barrier that protects it from this clear
                        continue;
                    }
                    let same_grid_cell_content = matches!(
                        &s.event,
                        UiEvent::GridLine { grid: g, .. }
                        | UiEvent::GridScroll { grid: g, .. }
                        | UiEvent::GridClear { grid: g }
                            if *g == *grid
                    );
                    if same_grid_cell_content {
                        s.alive = false;
                    }
                }
            }
            UiEvent::GridLine {
                grid,
                row,
                col_start,
                cells,
            } => {
                let epochs = self.grids.entry(*grid).or_default();
                let (barrier, scroll) = (epochs.barrier_epoch, epochs.scroll_epoch);
                let new_start = *col_start;
                let new_end = col_start.saturating_add(grid_line_span(cells));
                for s in &mut self.staged[boundary..] {
                    if !s.alive || s.barrier_epoch != barrier || s.scroll_epoch != scroll {
                        // dead, or staged before an intervening resize/scroll
                        // barrier: a scroll or resize may have relocated the
                        // row this old run painted, so its content is not
                        // provably superseded by a same-row-number write now
                        continue;
                    }
                    let UiEvent::GridLine {
                        grid: g2,
                        row: r2,
                        col_start: c2,
                        cells: cells2,
                    } = &s.event
                    else {
                        continue;
                    };
                    if *g2 != *grid || *r2 != *row {
                        continue;
                    }
                    let old_start = *c2;
                    let old_end = c2.saturating_add(grid_line_span(cells2));
                    if new_start <= old_start && new_end >= old_end {
                        s.alive = false;
                    }
                }
            }
            _ => {}
        }

        let (barrier_epoch, scroll_epoch) = grid_of(&ev)
            .map(|g| {
                let e = self.grids.entry(g).or_default();
                (e.barrier_epoch, e.scroll_epoch)
            })
            .unwrap_or_default();
        let is_flush = matches!(ev, UiEvent::Flush);
        self.staged.push(StagedEvent {
            event: ev,
            alive: true,
            barrier_epoch,
            scroll_epoch,
        });
        if is_flush {
            self.flush_index = Some(self.staged.len() - 1);
        }
    }

    /// Clears the pending flag and drains every staged event up to and
    /// including the last staged `Flush`, in this one lock acquisition.
    /// Events staged after that `Flush` (a batch still in progress) are
    /// left in place for the next call. Returns an empty `Vec` if nothing
    /// has reached a `Flush` yet.
    pub(crate) fn take(&mut self) -> Vec<UiEvent> {
        self.pending = false;
        let Some(flush_idx) = self.flush_index else {
            return Vec::new();
        };
        self.flush_index = None;
        self.staged
            .drain(..=flush_idx)
            .filter(|s| s.alive)
            .map(|s| s.event)
            .collect()
    }

    /// Whether damage is currently staged and armed (a fold has happened
    /// since the last `take`), which stays true across a sink attaching, so
    /// damage that arrived before there was anywhere to send a token is
    /// still reported afterwards.
    pub(crate) fn is_pending(&self) -> bool {
        self.pending
    }

    /// Folds the `pending` flag back to `false` after a fold's
    /// `RedrawReady` token failed to reach the channel (see module docs'
    /// bounded channel contract). Never touches `staged`: the damage itself
    /// is not lost, only the wakeup for it, and a later fold re-arms the
    /// flag and retries the send.
    pub(crate) fn disarm_pending(&mut self) {
        self.pending = false;
    }
}

/// The sink half of [`PumpShared`]: the runtime's channel once installed,
/// plus the arrival-order FIFO of non-coalescible `Msg`s staged before it
/// was.
#[derive(Default)]
struct Route {
    sink: Option<Arc<dyn MsgSink + Send + Sync>>,
    presink: VecDeque<Msg>,
    /// The newest `Msg::HlProbeReply` an attached-but-full sink refused,
    /// held for the next routing attempt to retry.
    ///
    /// One slot rather than a queue: only the newest probe generation's
    /// answer can still be the right one, so an older held reply is
    /// superseded rather than kept.
    deferred_probe: Option<Msg>,
    /// The newest `Msg::HeartbeatReply` an attached-but-full sink refused,
    /// held for the next routing attempt to retry.
    ///
    /// One slot rather than a queue, on the same terms as
    /// [`Route::deferred_probe`]: only the newest generation's
    /// acknowledgement can still move a liveness verdict, and an older held
    /// one is superseded rather than kept. Held rather than dropped because
    /// a discarded acknowledgement is indistinguishable from an engine that
    /// never answered, which is exactly the reading that raises a wedge.
    deferred_heartbeat: Option<Msg>,
    /// The `Msg::MappingsClaimed` an attached-but-full sink refused, held
    /// for the next routing attempt to retry.
    ///
    /// Its own slot rather than sharing [`Route::deferred_probe`]: a probe
    /// reply is superseded by the next one, a claim report never is (it is
    /// sent once per session), so one shared slot would let a probe reply
    /// arriving a moment later evict the report for good.
    deferred_claims: Option<Msg>,
    /// The newest `Msg::PickerBufferList` an attached-but-full sink refused,
    /// held for the next routing attempt to retry.
    ///
    /// One slot, on the same terms as [`Route::deferred_probe`]: a picker
    /// issues at most one `RpcCall::ListBuffers` per open, so only the
    /// latest generation's answer can still be the one a live picker is
    /// waiting on.
    deferred_buffer_list: Option<Msg>,
    /// The newest `Msg::HiddenBufferLoaded` an attached-but-full sink
    /// refused, held for the next routing attempt to retry.
    ///
    /// One slot, on the same terms as [`Route::deferred_buffer_list`]: a
    /// diff review resolves its buffer once per proposal and stamps the
    /// request with its own generation, so only the latest generation's
    /// answer can still be the one a live review is waiting on.
    deferred_hidden_buffer_loaded: Option<Msg>,
    /// The newest `Msg::PickerPreviewReply` an attached-but-full sink
    /// refused, held for the next routing attempt to retry.
    ///
    /// One slot, on the same terms as [`Route::deferred_buffer_list`]: only
    /// the latest preview generation's answer is still the one a live
    /// picker is waiting on, and an older held reply is superseded rather
    /// than kept.
    deferred_preview: Option<Msg>,
    /// The newest `Msg::FloatRows` an attached-but-full sink refused, held
    /// for the next routing attempt to retry.
    ///
    /// One slot, on the same terms as [`Route::deferred_preview`]: the rows
    /// an absorbed float is showing right now are the only ones worth
    /// painting, and an older held read is superseded rather than kept.
    deferred_float_rows: Option<Msg>,
    /// The `Msg::TreeRenameReply` an attached-but-full sink refused, held
    /// for the next routing attempt to retry.
    ///
    /// One slot: the tree issues at most one rename at a time (a second
    /// rename cannot be started while the first is still in flight, since
    /// there is only ever one selected row to act on), so there is never
    /// more than one outstanding reply to hold.
    deferred_rename: Option<Msg>,
    /// The `Msg::TreeCreatePromptReply` an attached-but-full sink refused,
    /// held for the next routing attempt to retry.
    ///
    /// One slot, on the same terms as [`Route::deferred_rename`]: the tree
    /// issues at most one create prompt at a time.
    deferred_create_prompt: Option<Msg>,
    /// The `Msg::TreeRenamePromptReply` an attached-but-full sink refused,
    /// held for the next routing attempt to retry.
    ///
    /// One slot, on the same terms as [`Route::deferred_rename`]: the tree
    /// issues at most one rename prompt at a time, for the same
    /// one-selected-row reason `deferred_rename` documents.
    deferred_rename_prompt: Option<Msg>,
    /// The `Msg::TreeDeleteConfirmReply` an attached-but-full sink refused,
    /// held for the next routing attempt to retry.
    ///
    /// One slot, on the same terms as [`Route::deferred_rename`]: the tree
    /// issues at most one delete confirmation at a time.
    deferred_delete_confirm: Option<Msg>,
    /// The `Msg::SwapRecovered` an attached-but-full sink refused, held for
    /// the next routing attempt to retry.
    ///
    /// One slot, and never superseded: the probe behind it is issued once
    /// per connection, so a held answer is the only one this session will
    /// ever get. Dropping it would leave a user editing text the file on
    /// disk does not contain with nothing on screen saying where it came
    /// from, and the report nvim wrote about it still covering the buffer.
    deferred_swap_recovery: Option<Msg>,
    /// Every `Msg::BufDetached` and agent filesystem answer an
    /// attached-but-full sink refused, held for the next routing attempt to
    /// retry.
    ///
    /// A queue, not a single slot like the `deferred_*` fields above: each
    /// carries a distinct event, so a second one arriving while the first
    /// is still parked must never evict it the way a superseded probe reply
    /// would. There is no "next event for this buffer" to re-carry a lost
    /// `Msg::BufDetached` the way `Msg::BufTextChanged::desynced` recovers
    /// a lost text-change (see its own doc comment) -- once the local
    /// attach-generation entry is removed, this is the only remaining copy
    /// -- and a lost `Msg::AiFsReadReply`/`Msg::AiFsWriteReply` leaves the
    /// agent that asked blocked on a request nothing else will ever settle.
    /// Every one queued here must eventually be delivered, not merely the
    /// newest.
    ///
    /// One queue for both kinds rather than one each: what they share is
    /// the "never drop, never reorder" contract, and a second queue would
    /// only let a message in one overtake a message in the other, which
    /// nothing wants.
    deferred_queued: VecDeque<Msg>,
}

/// Which never-drop slot a refused `Msg` waits in.
#[derive(Debug, Clone, Copy)]
enum Held {
    Probe,
    Heartbeat,
    Claims,
    BufferList,
    HiddenBufferLoaded,
    Preview,
    FloatRows,
    Rename,
    CreatePrompt,
    RenamePrompt,
    DeleteConfirm,
    SwapRecovery,
}

impl Route {
    fn slot(&mut self, which: Held) -> &mut Option<Msg> {
        match which {
            Held::Probe => &mut self.deferred_probe,
            Held::Heartbeat => &mut self.deferred_heartbeat,
            Held::Claims => &mut self.deferred_claims,
            Held::BufferList => &mut self.deferred_buffer_list,
            Held::HiddenBufferLoaded => &mut self.deferred_hidden_buffer_loaded,
            Held::Preview => &mut self.deferred_preview,
            Held::FloatRows => &mut self.deferred_float_rows,
            Held::Rename => &mut self.deferred_rename,
            Held::CreatePrompt => &mut self.deferred_create_prompt,
            Held::RenamePrompt => &mut self.deferred_rename_prompt,
            Held::DeleteConfirm => &mut self.deferred_delete_confirm,
            Held::SwapRecovery => &mut self.deferred_swap_recovery,
        }
    }

    /// Re-attempts every held message, putting each back if the sink is
    /// still full. Independent of whatever the caller is routing: these
    /// landing or not says nothing about that send.
    fn retry_deferred(&mut self) {
        for which in [
            Held::Probe,
            Held::Heartbeat,
            Held::Claims,
            Held::BufferList,
            Held::HiddenBufferLoaded,
            Held::Preview,
            Held::FloatRows,
            Held::Rename,
            Held::CreatePrompt,
            Held::RenamePrompt,
            Held::DeleteConfirm,
            Held::SwapRecovery,
        ] {
            let Some(msg) = self.slot(which).take() else {
                continue;
            };
            let Some(sink) = self.sink.clone() else {
                *self.slot(which) = Some(msg);
                continue;
            };
            self.hold_if_refused(which, sink.try_send(msg));
        }
        self.retry_deferred_queued();
    }

    /// Re-attempts every never-drop queued message, in arrival order,
    /// stopping at the first the sink still refuses: a still-full sink
    /// would also refuse everything queued behind it, and sending a later
    /// one first would misreport which buffer detached before which.
    /// Cheap when nothing is parked (the overwhelmingly common case, since
    /// `Route` is shared with every ordinary `route_msg`/`fold_redraw`
    /// call the reader thread makes): one `VecDeque::is_empty` check,
    /// nothing else.
    fn retry_deferred_queued(&mut self) {
        if self.deferred_queued.is_empty() {
            return;
        }
        let Some(sink) = self.sink.clone() else {
            return;
        };
        while let Some(msg) = self.deferred_queued.pop_front() {
            if let Err(TrySendError::Full(msg)) = sink.try_send(msg) {
                self.deferred_queued.push_front(msg);
                break;
            }
        }
    }

    /// Holds a `try_send` result's message when the sink was merely full,
    /// and lets it go when the sink is disconnected: the runtime loop that
    /// would have painted the answer is gone, so there is nothing left for
    /// a later attempt to reach.
    fn hold_if_refused(&mut self, which: Held, sent: Result<(), TrySendError<Msg>>) {
        match sent {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(msg)) => *self.slot(which) = Some(msg),
        }
    }
}

/// State shared between the reader thread (which folds redraws and routes
/// non-coalescible `Msg`s) and the runtime-facing [`DamagePump`] handle.
/// Exists from `Engine::spawn`, before any sink is attached, so damage and
/// requests that arrive in the setup window between spawn and
/// `Engine::start_pump` are never lost: they stage in [`DamageBuffer`] and
/// [`Route::presink`] respectively, and `attach_sink` catches both up.
pub(crate) struct PumpShared {
    damage: Mutex<DamageBuffer>,
    route: Mutex<Route>,
    generation: u64,
}

/// Hands every pump -- and so every connection, since a pump is built once
/// per [`crate::process::Engine::spawn`] -- an id no other connection in
/// this process has held.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

impl PumpShared {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            damage: Mutex::new(DamageBuffer::default()),
            route: Mutex::new(Route::default()),
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// This connection's generation: the tag every terminal message it
    /// routes carries, so a loop holding a replacement can tell a stop that
    /// describes the engine it is running from one that describes the
    /// engine it replaced.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Folds a decoded `redraw` batch and, iff this fold transitions the
    /// pending flag, sends `Msg::RedrawReady` through the installed sink.
    /// A no-op send when no sink is installed yet: [`attach_sink`]'s
    /// catch-up check picks up the still-armed flag.
    ///
    /// [`attach_sink`]: Self::attach_sink
    pub(crate) fn fold_redraw(&self, events: impl IntoIterator<Item = UiEvent>) {
        let transitioned = {
            let mut buf = self.damage.lock().unwrap_or_else(PoisonError::into_inner);
            buf.fold_batch(events)
        };
        if !transitioned {
            return;
        }
        let sink = {
            let mut route = self.route.lock().unwrap_or_else(PoisonError::into_inner);
            // the frequent routing attempt, and so the one that actually
            // carries a held message through once the runtime loop starts
            // draining again
            route.retry_deferred();
            route.sink.clone()
        };
        let Some(sink) = sink else {
            return;
        };
        if sink.try_send(Msg::RedrawReady).is_err() {
            // the token for this transition never reached the channel: fold
            // the flag back to false so a later fold sees false -> true
            // again and retries the send, instead of believing a token is
            // already in flight for a send that never happened. Racing this
            // against a concurrent take()/fold() that has since legitimately
            // re-armed pending is safe: the worst case is one redundant
            // extra send, which is always legal (see module docs).
            let mut buf = self.damage.lock().unwrap_or_else(PoisonError::into_inner);
            buf.disarm_pending();
        }
    }

    /// Routes a non-coalescible, caller-retriable `Msg` (`EngineRequest`) to
    /// the sink if attached, or stages it in the arrival-order FIFO
    /// otherwise. `Err` means the sink is attached but rejected the send
    /// (full or disconnected); see module docs for why the reader treats
    /// this as fatal.
    pub(crate) fn route_msg(&self, msg: Msg) -> Result<(), ()> {
        let mut route = self.route.lock().unwrap_or_else(PoisonError::into_inner);
        route.retry_deferred();
        match &route.sink {
            Some(sink) => sink.try_send(msg).map_err(|_| ()),
            None => {
                route.presink.push_back(msg);
                Ok(())
            }
        }
    }

    /// Routes a `Msg::BufDetached` without ever dropping it on a full sink.
    ///
    /// Unlike `route_msg`'s `Err`, which the reader thread treats as fatal
    /// (see module docs), and unlike `Msg::BufTextChanged`'s own
    /// drop-and-desync contract (there is always "the next event for this
    /// buffer" to own up to a lost text-change), a detach has no such
    /// recovery: the local attach-generation entry is already removed by
    /// the time this is called, so there is no "this buffer's next event"
    /// left to carry a lost `Msg::BufDetached` forward. A refused send is
    /// queued in [`Route::deferred_queued`] instead, retried at the
    /// head of every subsequent `route_msg`/`route_buf_detached` call and
    /// every `fold_redraw` (the reader thread's most frequent routing
    /// attempt of all, per [`fold_redraw`](Self::fold_redraw)'s own doc).
    pub(crate) fn route_buf_detached(&self, msg: Msg) {
        self.route_queued(msg);
    }

    /// Routes a `Msg::AiFsReadReply`/`Msg::AiFsWriteReply` without ever
    /// dropping it on a full sink, on the identical never-drop,
    /// never-reorder terms [`route_buf_detached`](Self::route_buf_detached)
    /// states.
    ///
    /// A dropped filesystem answer is the worst loss in this file: the
    /// agent that asked is blocked on the JSON-RPC request it belongs to,
    /// nothing re-issues it, and no later message carries the answer
    /// forward -- the agent simply waits for the rest of the session.
    pub(crate) fn route_ai_fs(&self, msg: Msg) {
        self.route_queued(msg);
    }

    /// Routes a `Msg::CheckTimeReply` without ever dropping it on a full
    /// sink, on the identical never-drop, never-reorder terms
    /// [`route_ai_fs`](Self::route_ai_fs) states.
    ///
    /// A dropped reply here is a missed conflict: the watcher saw a write
    /// nothing else observed, and if this answer never reaches `update()`
    /// the modified buffer it names is left silently diverged from disk,
    /// with nothing left to raise the prompt that was the whole point of
    /// the round trip.
    pub(crate) fn route_checktime(&self, msg: Msg) {
        self.route_queued(msg);
    }

    fn route_queued(&self, msg: Msg) {
        let mut route = self.route.lock().unwrap_or_else(PoisonError::into_inner);
        route.retry_deferred();
        if !route.deferred_queued.is_empty() {
            // an older message is still stuck behind a full sink; queue
            // behind it rather than risk this one landing out of order
            route.deferred_queued.push_back(msg);
            return;
        }
        let Some(sink) = route.sink.clone() else {
            route.presink.push_back(msg);
            return;
        };
        if let Err(TrySendError::Full(msg)) = sink.try_send(msg) {
            route.deferred_queued.push_back(msg);
        }
    }

    /// Routes an `Msg::HlProbeReply` without ever dropping it on a full
    /// sink, and without blocking.
    ///
    /// A dropped probe reply fails silently and for the rest of the session:
    /// `Theme::from_hl` trusts a confirmed background only while its
    /// generation matches the live probe generation, so a lost reply leaves
    /// a real `guibg=#000000` painting as though the background were unset
    /// until some later colorscheme change happens to issue a fresh probe.
    /// Blocking is not the alternative -- this runs on the RPC reader
    /// thread, which must never block -- so a refused reply waits in
    /// [`Route::deferred_probe`] for the next routing attempt to carry it.
    pub(crate) fn route_probe_reply(&self, msg: Msg) {
        self.route_held(msg, Held::Probe);
    }

    /// Routes a `Msg::HeartbeatReply` without ever dropping it on a full
    /// sink, and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped acknowledgement reads as an engine that did not answer,
    /// and enough of them in a row read as a wedge: the one failure this
    /// reply exists to report would then be reported about a connection
    /// that was healthy the whole time.
    pub(crate) fn route_heartbeat(&self, msg: Msg) {
        self.route_held(msg, Held::Heartbeat);
    }

    /// Routes a `Msg::MappingsClaimed` without ever dropping it on a full
    /// sink, and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped claim report is silent and permanent: mappings register
    /// once per session, so nothing re-issues the answer, and a user whose
    /// `<leader>ff` view has just taken over would never be told which
    /// switch gives it back.
    pub(crate) fn route_claims(&self, msg: Msg) {
        self.route_held(msg, Held::Claims);
    }

    /// Routes a `Msg::PickerBufferList` without ever dropping it on a full
    /// sink, and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped buffer list is silent for the rest of that picker session:
    /// nothing re-issues `RpcCall::ListBuffers` on its own, so a refused
    /// reply that was simply discarded would leave `Source::Buffers` picker
    /// showing no rows with no way for the user to tell "empty" apart from
    /// "lost".
    pub(crate) fn route_buffer_list(&self, msg: Msg) {
        self.route_held(msg, Held::BufferList);
    }

    /// Routes a `Msg::HiddenBufferLoaded` without ever dropping it on a full
    /// sink, and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped resolve reply strands the diff review that asked for it:
    /// nothing re-issues `RpcCall::LoadHidden` on its own, so the review
    /// would sit forever with no buffer bound, unable to attach and unable
    /// to write a hunk the user accepts.
    pub(crate) fn route_hidden_buffer_loaded(&self, msg: Msg) {
        self.route_held(msg, Held::HiddenBufferLoaded);
    }

    /// Routes a `Msg::SwapRecovered` without ever dropping it on a full sink,
    /// and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped answer is silent and permanent: the probe is issued once per
    /// connection, off that connection's own `VimEnter`, so nothing re-asks.
    /// What would be lost is both halves of the recovery's account of itself
    /// -- the notice, and the redraw that takes nvim's report off the buffer.
    pub(crate) fn route_swap_recovery(&self, msg: Msg) {
        self.route_held(msg, Held::SwapRecovery);
    }

    /// Routes a `Msg::PickerPreviewReply` without ever dropping it on a full
    /// sink, and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped preview reply is silent for the rest of that picker
    /// session: nothing re-issues `RpcCall::PreviewBuffer` on its own, so a
    /// refused reply that was simply discarded would leave the preview pane
    /// showing stale or empty content with no way for the user to tell
    /// "genuinely empty" apart from "lost".
    pub(crate) fn route_preview(&self, msg: Msg) {
        self.route_held(msg, Held::Preview);
    }

    /// Routes a `Msg::FloatRows` without ever dropping it on a full sink,
    /// and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped rows reply is the one the palette cannot tell apart from a
    /// menu with nothing in it: the absorption already hid the plugin's own
    /// window, so the user would be reading an empty palette while the
    /// candidates stood in a buffer nobody is painting, until the next scan
    /// happened to ask again.
    pub(crate) fn route_float_rows(&self, msg: Msg) {
        self.route_held(msg, Held::FloatRows);
    }

    /// Routes a `Msg::TreeRenameReply` without ever dropping it on a full
    /// sink, and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped rename reply is the one this crate cannot afford to lose
    /// silently: the on-disk rename has already happened by the time this
    /// fires, so a lost reply would leave the tree's rescan (and, on
    /// failure, the notice telling the user why nothing moved) permanently
    /// un-triggered rather than merely stale.
    pub(crate) fn route_rename(&self, msg: Msg) {
        self.route_held(msg, Held::Rename);
    }

    /// Routes a `Msg::TreeCreatePromptReply` without ever dropping it on a
    /// full sink, and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped reply here leaves the create prompt open with no path
    /// forward: nothing re-issues the `RpcCall::TreeCreatePrompt` on its
    /// own, so the user's typed name would simply vanish.
    pub(crate) fn route_create_prompt(&self, msg: Msg) {
        self.route_held(msg, Held::CreatePrompt);
    }

    /// Routes a `Msg::TreeRenamePromptReply` without ever dropping it on a
    /// full sink, and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped reply here leaves the rename prompt open with no path
    /// forward, on the same terms as [`route_create_prompt`](Self::route_create_prompt).
    pub(crate) fn route_rename_prompt(&self, msg: Msg) {
        self.route_held(msg, Held::RenamePrompt);
    }

    /// Routes a `Msg::TreeDeleteConfirmReply` without ever dropping it on a
    /// full sink, and without blocking, on the same terms as
    /// [`route_probe_reply`](Self::route_probe_reply).
    ///
    /// A dropped reply here is the one this crate cannot afford to lose
    /// silently: `vim.fn.confirm` has already returned by the time this
    /// fires, so a lost reply would leave the user's choice permanently
    /// un-acted-on rather than merely stale.
    pub(crate) fn route_delete_confirm(&self, msg: Msg) {
        self.route_held(msg, Held::DeleteConfirm);
    }

    fn route_held(&self, msg: Msg, which: Held) {
        let mut route = self.route.lock().unwrap_or_else(PoisonError::into_inner);
        route.retry_deferred();
        let Some(sink) = route.sink.clone() else {
            route.presink.push_back(msg);
            return;
        };
        route.hold_if_refused(which, sink.try_send(msg));
    }

    /// Routes this connection's terminal `Msg::EngineStopped`, stamped with
    /// [`generation`](Self::generation), with a blocking `send` rather than
    /// `try_send` (see module docs' bounded channel contract for why a
    /// dropped `EngineStopped` is unrecoverable, unlike a dropped
    /// `RedrawReady`). Stages it in the arrival-order FIFO if no sink is
    /// attached yet, same as [`route_msg`](Self::route_msg).
    ///
    /// The stamp is what makes a restart safe. Every connection this process
    /// opens shares one loop channel, and the reader of a connection being
    /// replaced posts its stop *after* the replacement is live -- the
    /// teardown a restart performs is what produces that stop. Unstamped, a
    /// loop reading it would resolve it against the engine it is now running
    /// and tear down the replacement it had just brought up.
    pub(crate) fn route_engine_stopped(&self, reason: Option<String>) {
        self.route_terminal(Msg::EngineStopped {
            generation: self.generation,
            reason,
        });
    }

    fn route_terminal(&self, msg: Msg) {
        let mut route = self.route.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(sink) = route.sink.clone() else {
            route.presink.push_back(msg);
            return;
        };
        drop(route);
        // blocks only if the channel is momentarily full; the reader thread
        // calling this is already exiting and has nothing else left to do.
        // Err means the receiver end is already gone, which has nothing
        // left to signal either.
        let _ = sink.send(msg);
    }

    /// Installs `sink` and returns what was already staged before this call
    /// existed, instead of sending any of it: the presink FIFO's messages in
    /// arrival order, and whether damage was already pending. `sink` is not
    /// guaranteed to have a consumer draining it yet at the moment this call
    /// is made -- the runtime loop that eventually reads it can start well
    /// after this call returns -- so a send performed here has no bound on
    /// how long it can block: an unconsumed channel already holding other
    /// traffic (other producer threads send into the same channel,
    /// blocking) can fill up and wedge both senders permanently. Returning the staged state instead lets the
    /// caller resolve it through its own dispatch path once it knows a
    /// consumer exists, exactly the way [`DamagePump::take_damage`]'s
    /// `Msg::RedrawReady` catch-up is resolved by `runtime::run`'s loop.
    /// Routing installed by this call for *steady-state* traffic afterward
    /// ([`fold_redraw`](Self::fold_redraw), [`route_msg`](Self::route_msg),
    /// [`route_terminal`](Self::route_terminal)) is unchanged: those still
    /// write into `sink` directly, because by the time they run, `sink`'s
    /// real consumer is guaranteed to already be draining it.
    pub(crate) fn attach_sink(
        self: &Arc<Self>,
        sink: impl MsgSink + Send + Sync + 'static,
    ) -> (DamagePump, SinkCutover) {
        let presink = {
            let mut route = self.route.lock().unwrap_or_else(PoisonError::into_inner);
            route.sink = Some(Arc::new(sink));
            route.presink.drain(..).collect()
        };
        let redraw_pending = {
            let buf = self.damage.lock().unwrap_or_else(PoisonError::into_inner);
            buf.is_pending()
        };
        (
            DamagePump {
                shared: Arc::clone(self),
            },
            SinkCutover {
                presink,
                redraw_pending,
            },
        )
    }

    fn take_damage(&self) -> Vec<UiEvent> {
        let mut buf = self.damage.lock().unwrap_or_else(PoisonError::into_inner);
        buf.take()
    }
}

/// The runtime-facing handle for draining compacted damage. Cheap to clone
/// via [`Engine::start_pump`](crate::process::Engine::start_pump)'s
/// `Arc`-backed state, though the runtime loop's call site only ever needs
/// one.
pub struct DamagePump {
    shared: Arc<PumpShared>,
}

/// What [`PumpShared::attach_sink`] found already staged before `sink`
/// existed, returned instead of sent so the caller can resolve it through
/// its own dispatch path against a channel that may not have a consumer
/// running yet. `presink` is in arrival order; `redraw_pending` mirrors
/// [`DamageBuffer::is_pending`] at the moment of the call, for the caller to
/// resolve via [`DamagePump::take_damage`] the same way `runtime::run`'s
/// loop resolves a live `Msg::RedrawReady`.
pub struct SinkCutover {
    pub presink: Vec<Msg>,
    pub redraw_pending: bool,
}

impl DamagePump {
    /// Clears the pending flag and drains every compacted event staged up
    /// to the last `Flush`, in one lock acquisition. Non-blocking: this
    /// never waits on the reader thread.
    #[must_use]
    pub fn take_damage(&self) -> Vec<UiEvent> {
        self.shared.take_damage()
    }
}

/// Deterministic synthetic "redraw storm" generator, and the fold+drain
/// entry point that exercises it against a real [`DamageBuffer`] -- the
/// hot path `benches/damage_fold.rs` measures. Gated behind the
/// `bench-support` feature as well as `cfg(test)`: an external bench binary
/// only ever sees this crate's `pub` items (the same boundary a downstream
/// consumer crosses), so benching the fold hot path against its own
/// generator needs *some* public surface, but neither belongs in
/// view-engine's default API -- only this module's own `#[cfg(test)]`
/// property tests (`compaction_preserves_final_grid_and_non_grid_subsequence`
/// and its multi-take sibling) and the `bench-support`-gated bench need it.
#[cfg(any(test, feature = "bench-support"))]
pub mod storm {
    use super::DamageBuffer;
    use view_core::events::{GridCell, UiEvent};

    fn cell(text: &str, repeat: u64) -> GridCell {
        GridCell {
            text: text.to_string(),
            hl_id: 0,
            repeat,
        }
    }

    /// A single-run `GridLine` covering `len` cells of `"x"` starting at
    /// `(row, col_start)` on grid 1: the storm generator's and the ordinary
    /// unit tests' shared building block for "some cell content changed
    /// here."
    #[must_use]
    pub fn line(row: u64, col_start: u64, len: u64) -> UiEvent {
        UiEvent::GridLine {
            grid: 1,
            row,
            col_start,
            cells: vec![cell("x", len)],
        }
    }

    /// Minimal xorshift64: no external randomness dependency for a
    /// hand-rolled generative test, deterministic per seed for reproducible
    /// failures (and reproducible bench inputs). `pub(super)`: this
    /// module's sibling `mod tests` reuses it (with its own, differently
    /// mixed seed) to schedule `fold`/`take` calls in
    /// `compaction_multi_take_schedule_matches_raw_at_each_flush_boundary`,
    /// a generative concern unrelated to the storm generator itself but
    /// sharing the same "no external randomness dependency" tradeoff.
    pub(super) struct Xorshift(u64);
    impl Xorshift {
        pub(super) fn new(seed: u64) -> Self {
            Self(seed)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        pub(super) fn below(&mut self, n: u64) -> u64 {
            self.next_u64() % n.max(1)
        }
    }

    const GRID_W: u64 = 6;
    const GRID_H: u64 = 4;

    fn gen_event(rng: &mut Xorshift) -> UiEvent {
        match rng.below(10) {
            0 => line(rng.below(GRID_H), rng.below(GRID_W), 1 + rng.below(GRID_W)),
            1 => UiEvent::GridScroll {
                grid: 1,
                top: 0,
                bot: GRID_H,
                left: 0,
                right: GRID_W,
                rows: if rng.below(2) == 0 { 1 } else { -1 },
            },
            2 => UiEvent::GridClear { grid: 1 },
            3 => UiEvent::GridCursorGoto {
                grid: 1,
                row: rng.below(GRID_H),
                col: rng.below(GRID_W),
            },
            4 => UiEvent::HlAttrDefine {
                id: rng.below(4),
                fg: Some(rng.below(0xff_ffff) as u32),
                bg: None,
                bold: rng.below(2) == 0,
                italic: false,
                underline: false,
                reverse: false,
            },
            5 => UiEvent::DefaultColorsSet {
                fg: Some(rng.below(0xff_ffff) as u32),
                bg: Some(rng.below(0xff_ffff) as u32),
                sp: None,
            },
            6 => UiEvent::ModeChange {
                mode: "insert".to_string(),
                mode_idx: rng.below(3),
            },
            7 => UiEvent::MsgShow {
                kind: "echomsg".to_string(),
                content: vec![(0, format!("m{}", rng.below(1000)))],
                replace_last: rng.below(2) == 0,
            },
            // deliberately a small range close to GRID_W/GRID_H rather than
            // an unrelated size: out-of-bounds cell ops are safe no-ops
            // (Grid::apply), but a resize wildly smaller than the row/col
            // range GridLine/GridCursorGoto generate would make most of
            // them land off-grid and exercise nothing
            8 => UiEvent::GridResize {
                grid: 1,
                width: 1 + rng.below(GRID_W + 2),
                height: 1 + rng.below(GRID_H + 2),
            },
            _ => UiEvent::Flush,
        }
    }

    /// Generates one deterministic synthetic redraw storm: a leading full
    /// grid resize, `len` random cell/scroll/clear/cursor/highlight/mode/
    /// message events, then a trailing `Flush` -- the same shape (mixed
    /// event kinds, scroll-interleaved, occasionally resize-barriered) a
    /// real edit-and-scroll session produces, without spawning nvim.
    #[must_use]
    pub fn gen_sequence(seed: u64, len: usize) -> Vec<UiEvent> {
        let mut rng = Xorshift::new(seed.wrapping_mul(2_685_821_657) | 1);
        let mut out = vec![UiEvent::GridResize {
            grid: 1,
            width: GRID_W,
            height: GRID_H,
        }];
        for _ in 0..len {
            out.push(gen_event(&mut rng));
        }
        out.push(UiEvent::Flush);
        out
    }

    /// Folds `events` through a fresh [`DamageBuffer`] and drains the
    /// compacted result: the fold+take pair `PumpShared::fold_redraw` and
    /// `DamagePump::take_damage` drive in production, exposed here since a
    /// bench binary (an external compilation unit) only ever reaches this
    /// crate's `pub` items -- `DamageBuffer` itself stays crate-private.
    #[must_use]
    pub fn fold_and_take(events: Vec<UiEvent>) -> Vec<UiEvent> {
        let mut buf = DamageBuffer::default();
        buf.fold_batch(events);
        buf.take()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::storm::{gen_sequence, line};
    use super::*;
    use view_core::model::Model;
    use view_core::update::update;

    #[test]
    fn take_damage_drains_up_to_last_flush_and_leaves_partial_staged() {
        let mut buf = DamageBuffer::default();
        buf.fold_batch(vec![line(0, 0, 3), UiEvent::Flush]);
        buf.fold_batch(vec![line(1, 0, 3)]); // no Flush yet: must stay staged
        let drained = buf.take();
        assert_eq!(drained, vec![line(0, 0, 3), UiEvent::Flush]);

        // the partial post-flush event is retained; a later Flush drains it
        buf.fold_batch(vec![UiEvent::Flush]);
        let drained2 = buf.take();
        assert_eq!(drained2, vec![line(1, 0, 3), UiEvent::Flush]);
    }

    #[test]
    fn take_damage_with_no_flush_yet_returns_empty_and_keeps_staged() {
        let mut buf = DamageBuffer::default();
        buf.fold_batch(vec![line(0, 0, 3)]);
        assert_eq!(buf.take(), Vec::<UiEvent>::new());
        buf.fold_batch(vec![UiEvent::Flush]);
        assert_eq!(buf.take(), vec![line(0, 0, 3), UiEvent::Flush]);
    }

    #[test]
    fn n_folds_without_a_drain_transition_the_flag_exactly_once() {
        let mut buf = DamageBuffer::default();
        let mut transitions = 0;
        for _ in 0..50 {
            if buf.fold_batch(vec![line(0, 0, 1), UiEvent::Flush]) {
                transitions += 1;
            }
        }
        assert_eq!(
            transitions, 1,
            "expected exactly one false->true transition across 50 undrained folds"
        );
        assert!(buf.is_pending());
    }

    #[test]
    fn fold_after_take_damage_returns_leaves_a_token_pending() {
        // reproduces the lost-wakeup shape a split flag/buffer design
        // allows: drain first (clearing pending under the same lock as the
        // drain), then fold, and the flag must already be armed again for
        // the next caller instead of requiring a second unrelated event to
        // discover it.
        let mut buf = DamageBuffer::default();
        assert!(buf.fold_batch(vec![line(0, 0, 1), UiEvent::Flush]));
        let _ = buf.take();
        assert!(!buf.is_pending(), "take() must clear the flag");

        let transitioned = buf.fold_batch(vec![line(1, 0, 1), UiEvent::Flush]);
        assert!(
            transitioned,
            "a fold landing after take() returned must re-arm the flag"
        );
        assert!(buf.is_pending());
    }

    #[test]
    fn grid_line_replaces_fully_covered_earlier_run_same_row() {
        let mut buf = DamageBuffer::default();
        buf.fold_batch(vec![
            line(0, 0, 3),  // fully covered by the next write
            line(0, 0, 10), // supersedes it
            UiEvent::Flush,
        ]);
        let drained = buf.take();
        assert_eq!(
            drained,
            vec![line(0, 0, 10), UiEvent::Flush],
            "the fully-covered earlier run must be elided, not just retained"
        );
    }

    #[test]
    fn grid_scroll_is_a_compaction_barrier_for_earlier_same_row_runs() {
        let mut buf = DamageBuffer::default();
        let scroll = UiEvent::GridScroll {
            grid: 1,
            top: 0,
            bot: 10,
            left: 0,
            right: 10,
            rows: 1,
        };
        buf.fold_batch(vec![
            line(0, 0, 3),
            scroll.clone(),
            line(0, 0, 10), // same row number, but the scroll relocated it
            UiEvent::Flush,
        ]);
        let drained = buf.take();
        assert_eq!(
            drained,
            vec![line(0, 0, 3), scroll, line(0, 0, 10), UiEvent::Flush],
            "a GridScroll between two same-row runs must block elision"
        );
    }

    #[test]
    fn grid_resize_is_a_compaction_barrier_for_everything_before_it() {
        let mut buf = DamageBuffer::default();
        let resize = UiEvent::GridResize {
            grid: 1,
            width: 20,
            height: 20,
        };
        buf.fold_batch(vec![
            line(0, 0, 3),
            resize.clone(),
            line(0, 0, 10),
            UiEvent::GridClear { grid: 1 },
            UiEvent::Flush,
        ]);
        let drained = buf.take();
        // the pre-resize line survives both the later covering GridLine and
        // the later GridClear; the resize barrier protects it from both
        assert_eq!(
            drained,
            vec![
                line(0, 0, 3),
                resize,
                UiEvent::GridClear { grid: 1 },
                UiEvent::Flush,
            ],
        );
    }

    #[test]
    fn grid_clear_drops_earlier_cell_content_but_keeps_cursor_and_hl() {
        let mut buf = DamageBuffer::default();
        let goto = UiEvent::GridCursorGoto {
            grid: 1,
            row: 2,
            col: 3,
        };
        let hl = UiEvent::HlAttrDefine {
            id: 1,
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        };
        let colors = UiEvent::DefaultColorsSet {
            fg: Some(1),
            bg: Some(2),
            sp: None,
        };
        buf.fold_batch(vec![
            line(0, 0, 3),
            goto.clone(),
            hl.clone(),
            colors.clone(),
            UiEvent::GridClear { grid: 1 },
            UiEvent::Flush,
        ]);
        let drained = buf.take();
        assert_eq!(
            drained,
            vec![
                goto,
                hl,
                colors,
                UiEvent::GridClear { grid: 1 },
                UiEvent::Flush,
            ],
            "GridClear must drop only the earlier cell-content GridLine"
        );
    }

    #[test]
    fn grid_clear_drops_across_an_intervening_scroll_unlike_grid_line_elision() {
        // GridScroll is a barrier for GridLine's same-row elision (see
        // grid_scroll_is_a_compaction_barrier_for_earlier_same_row_runs),
        // but GridClear resets every cell unconditionally, so its drop is
        // correct regardless of what a scroll did to those cells first: the
        // final state after Clear depends only on what is staged after it.
        let mut buf = DamageBuffer::default();
        let scroll = UiEvent::GridScroll {
            grid: 1,
            top: 0,
            bot: 10,
            left: 0,
            right: 10,
            rows: 1,
        };
        buf.fold_batch(vec![
            line(0, 0, 3),
            scroll,
            line(1, 0, 3),
            UiEvent::GridClear { grid: 1 },
            UiEvent::Flush,
        ]);
        let drained = buf.take();
        assert_eq!(
            drained,
            vec![UiEvent::GridClear { grid: 1 }, UiEvent::Flush],
            "GridClear must drop cell-content staged both before and after an intervening scroll"
        );
    }

    #[test]
    fn unflushed_line_does_not_tear_an_already_flushed_same_row_write() {
        // a later, still-unflushed GridLine must not elide an earlier run
        // that already reached a Flush. Without the
        // compaction_start() boundary, this fold's same-row full-coverage
        // check has no reason to distinguish the two batches (no resize or
        // scroll occurred between them) and would tombstone the flushed
        // row's content -- a frame that never existed on the wire.
        let mut buf = DamageBuffer::default();
        let resize = UiEvent::GridResize {
            grid: 1,
            width: 20,
            height: 20,
        };
        buf.fold_batch(vec![
            resize.clone(),
            line(0, 0, 5),
            line(2, 0, 5),
            UiEvent::Flush,
        ]);
        buf.fold_batch(vec![line(0, 0, 10)]); // fully covers row 0, but unflushed

        let drained = buf.take();
        assert_eq!(
            drained,
            vec![resize, line(0, 0, 5), line(2, 0, 5), UiEvent::Flush],
            "the flushed row-0 write must survive the later unflushed same-row fold"
        );

        // the unflushed line is still staged, retained for the next flush
        let drained2 = {
            buf.fold_batch(vec![UiEvent::Flush]);
            buf.take()
        };
        assert_eq!(drained2, vec![line(0, 0, 10), UiEvent::Flush]);
    }

    #[test]
    fn unflushed_grid_clear_does_not_drop_an_already_flushed_line() {
        // a later, still-unflushed GridClear must not drop an earlier run
        // that already reached a Flush. GridClear's
        // drop rule is barrier_epoch-gated, not flush-gated, so without the
        // compaction_start() boundary it would tombstone the flushed line
        // too (no resize happened between the two folds).
        let mut buf = DamageBuffer::default();
        let resize = UiEvent::GridResize {
            grid: 1,
            width: 20,
            height: 20,
        };
        buf.fold_batch(vec![resize.clone(), line(0, 0, 5), UiEvent::Flush]);
        buf.fold_batch(vec![UiEvent::GridClear { grid: 1 }]); // unflushed

        let drained = buf.take();
        assert_eq!(
            drained,
            vec![resize, line(0, 0, 5), UiEvent::Flush],
            "the flushed line must survive the later unflushed GridClear"
        );

        // the unflushed clear is still staged, retained for the next flush
        let drained2 = {
            buf.fold_batch(vec![UiEvent::Flush]);
            buf.take()
        };
        assert_eq!(
            drained2,
            vec![UiEvent::GridClear { grid: 1 }, UiEvent::Flush]
        );
    }

    #[test]
    fn ten_thousand_undrained_folds_produce_at_most_one_channel_token() {
        // watchdog: the pending-flag dedup plus non-blocking try_send means
        // this loop cannot legitimately block; a regression that made
        // fold_redraw block on a full channel would hang this join past the
        // budget instead of failing a normal assertion.
        let shared = PumpShared::new();
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let (_pump, _cutover) = shared.attach_sink(tx);
        let flood = std::thread::spawn(move || {
            for i in 0..10_000u64 {
                shared.fold_redraw(vec![line(0, 0, 1), UiEvent::Flush]);
                let _ = i;
            }
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = flood.join();
            let _ = done_tx.send(());
        });
        assert!(
            done_rx
                .recv_timeout(view_test_support::host_deadline(
                    std::time::Duration::from_secs(2)
                ))
                .is_ok(),
            "10,000 undrained folds did not complete: fold_redraw blocked"
        );
        let mut count = 0;
        while rx.try_recv().is_ok() {
            count += 1;
        }
        assert!(
            count <= 1,
            "expected at most one deduped RedrawReady token, got {count}"
        );
    }

    #[test]
    fn failed_redraw_token_send_disarms_pending_flag_so_next_fold_retries() {
        let shared = PumpShared::new();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(1);
        let (pump, _cutover) = shared.attach_sink(tx.clone());
        // fill the channel's one slot with a dummy Msg so this fold's
        // RedrawReady token has nowhere to land and try_send fails
        tx.try_send(Msg::Resized {
            width: 1,
            height: 1,
        })
        .expect("channel has capacity for the dummy fill");

        shared.fold_redraw(vec![line(0, 0, 1), UiEvent::Flush]);
        assert!(matches!(rx.try_recv(), Ok(Msg::Resized { .. })));
        assert!(
            rx.try_recv().is_err(),
            "no RedrawReady token reached the channel from the failed send"
        );

        // a fold after the disarm must re-arm and retry, now there is room
        shared.fold_redraw(vec![line(1, 0, 1), UiEvent::Flush]);
        assert!(
            matches!(rx.try_recv(), Ok(Msg::RedrawReady)),
            "a fold after a disarmed flag must re-attempt the RedrawReady send"
        );

        // the damage from both folds was never lost, only its wakeup was
        let drained = pump.take_damage();
        assert_eq!(
            drained,
            vec![line(0, 0, 1), UiEvent::Flush, line(1, 0, 1), UiEvent::Flush]
        );
    }

    #[test]
    fn route_terminal_blocks_on_a_full_channel_then_still_delivers_engine_stopped() {
        let shared = PumpShared::new();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(1);
        let (_pump, _cutover) = shared.attach_sink(tx.clone());
        tx.try_send(Msg::Resized {
            width: 1,
            height: 1,
        })
        .expect("channel has capacity for the dummy fill");

        let blocked = Arc::clone(&shared);
        let sender = std::thread::spawn(move || {
            blocked.route_engine_stopped(None);
        });

        // give the blocking send every chance to have wrongly returned
        // early on the full channel (a regression back to try_send)
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !sender.is_finished(),
            "route_terminal must block while the channel has no room, not drop and return"
        );

        // drain the dummy, freeing the slot the blocked send is waiting for
        assert!(matches!(rx.recv(), Ok(Msg::Resized { .. })));
        sender.join().expect("blocked sender thread must not panic");

        assert!(
            matches!(rx.recv(), Ok(Msg::EngineStopped { reason: None, .. })),
            "EngineStopped must arrive once the channel has room, not be dropped"
        );
    }

    #[test]
    fn a_probe_reply_refused_by_a_full_sink_arrives_once_the_channel_drains() {
        let shared = PumpShared::new();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(1);
        let (_pump, _cutover) = shared.attach_sink(tx);
        // fill the sink, the state the cutover replay genuinely reaches: a
        // sink is attached but nothing is draining it yet
        shared
            .route_msg(Msg::Resized {
                width: 9,
                height: 9,
            })
            .expect("the channel has room for the fill");

        shared.route_probe_reply(Msg::HlProbeReply {
            generation: 7,
            fg: Some(0x00ff_ffff),
            bg: Some(0x0000_0000),
        });

        assert!(
            matches!(rx.recv(), Ok(Msg::Resized { width: 9, .. })),
            "the fill message must come out first, in arrival order"
        );
        // any later routing attempt is what carries the held reply through
        shared.fold_redraw(vec![line(0, 0, 1), UiEvent::Flush]);

        let mut seen = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            seen.push(msg);
        }
        assert!(
            seen.iter().any(|m| matches!(
                m,
                Msg::HlProbeReply {
                    generation: 7,
                    bg: Some(0),
                    ..
                }
            )),
            "the probe reply was dropped by the full sink and never retried, \
             so this generation stays unconfirmed and a real black background \
             paints as unset for the rest of the session; saw {seen:?}"
        );
    }

    #[test]
    fn a_newer_probe_reply_supersedes_a_held_older_one() {
        let shared = PumpShared::new();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(1);
        let (_pump, _cutover) = shared.attach_sink(tx);
        shared
            .route_msg(Msg::Resized {
                width: 9,
                height: 9,
            })
            .expect("the channel has room for the fill");

        // both are refused; only the newer generation can still be the
        // right answer, so the older one must not resurface later and
        // overwrite it
        shared.route_probe_reply(Msg::HlProbeReply {
            generation: 1,
            fg: None,
            bg: None,
        });
        shared.route_probe_reply(Msg::HlProbeReply {
            generation: 2,
            fg: None,
            bg: Some(0x0011_2233),
        });

        assert!(matches!(rx.recv(), Ok(Msg::Resized { width: 9, .. })));
        shared.fold_redraw(vec![line(0, 0, 1), UiEvent::Flush]);

        let mut replies = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let Msg::HlProbeReply { generation, .. } = msg {
                replies.push(generation);
            }
        }
        assert_eq!(
            replies,
            vec![2],
            "expected only the newest generation to be retried"
        );
    }

    #[test]
    fn attach_sink_against_a_full_channel_never_blocks_and_returns_staged_state_instead_of_sending()
    {
        let shared = PumpShared::new();
        // stage a presink message and pending damage before any sink exists,
        // the exact setup-window state attach_sink must hand back rather
        // than write into a channel with no guaranteed consumer
        let _ = shared.route_msg(Msg::Resized {
            width: 1,
            height: 1,
        });
        shared.fold_redraw(vec![line(0, 0, 1), UiEvent::Flush]);

        let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(1);
        tx.try_send(Msg::Resized {
            width: 9,
            height: 9,
        })
        .expect("channel has capacity for the dummy fill");
        // the channel is now full with no consumer draining it

        let (_pump, cutover) = shared.attach_sink(tx);
        assert!(
            matches!(
                cutover.presink.as_slice(),
                [Msg::Resized {
                    width: 1,
                    height: 1
                }]
            ),
            "expected the staged presink message returned to the caller, not sent"
        );
        assert!(
            cutover.redraw_pending,
            "expected the staged damage's pending flag returned as true"
        );
        // the dummy fill is still the only thing in the channel: attach_sink
        // never wrote into it despite there being staged state to deliver
        assert!(matches!(
            rx.try_recv(),
            Ok(Msg::Resized {
                width: 9,
                height: 9
            })
        ));
        assert!(rx.try_recv().is_err());
    }

    // -- compaction property: generative, scroll-interleaved, subsequence-preserving --
    // gen_sequence/line are the storm module above, shared with
    // benches/damage_fold.rs (see this file's storm module doc comment).

    fn is_grid_op_event(ev: &UiEvent) -> bool {
        matches!(
            ev,
            UiEvent::GridResize { .. }
                | UiEvent::GridLine { .. }
                | UiEvent::GridCursorGoto { .. }
                | UiEvent::GridScroll { .. }
                | UiEvent::GridClear { .. }
        )
    }

    /// Enough of `model`'s `Grid` to compare final states: every cell plus
    /// size plus the cursor, since `Grid` itself has no `PartialEq`.
    fn grid_state(model: &Model) -> (Vec<view_core::grid::Cell>, (u16, u16), (u16, u16)) {
        let (w, h) = model.engine.grid().size();
        let mut cells = Vec::with_capacity(usize::from(w) * usize::from(h));
        for r in 0..h {
            for c in 0..w {
                cells.push(model.engine.grid().cell(r, c).cloned().unwrap_or_default());
            }
        }
        (cells, (w, h), model.engine.grid().cursor())
    }

    /// Applies `events` through `update()` (the same `UiEvent` -> `GridOp`
    /// translation the runtime loop uses), from a fresh `Model`, and
    /// returns its final [`grid_state`].
    fn grid_snapshot(events: Vec<UiEvent>) -> (Vec<view_core::grid::Cell>, (u16, u16), (u16, u16)) {
        let mut model = Model::new();
        let _ = update(&mut model, Msg::Redraw(events));
        grid_state(&model)
    }

    #[test]
    fn compaction_preserves_final_grid_and_non_grid_subsequence() {
        for seed in 0..300u64 {
            let raw = gen_sequence(seed, 40);

            let mut buf = DamageBuffer::default();
            buf.fold_batch(raw.clone());
            let compacted = buf.take();

            let raw_snapshot = grid_snapshot(raw.clone());
            let compacted_snapshot = grid_snapshot(compacted.clone());
            assert_eq!(
                raw_snapshot, compacted_snapshot,
                "seed {seed}: compacted batch produced a different final grid; \
                 raw={raw:?} compacted={compacted:?}"
            );

            let raw_other: Vec<&UiEvent> = raw.iter().filter(|e| !is_grid_op_event(e)).collect();
            let compacted_other: Vec<&UiEvent> =
                compacted.iter().filter(|e| !is_grid_op_event(e)).collect();
            assert_eq!(
                raw_other, compacted_other,
                "seed {seed}: non-GridOp subsequence diverged in content or order"
            );
        }
    }

    /// A `DamageBuffer`-shaped shadow with zero compaction: stages every
    /// event as-is and drains through the same flush-boundary rule as
    /// `DamageBuffer::take`. Driven through the exact same fold/take call
    /// schedule as a real `DamageBuffer` in the test below, so its
    /// `flush_index` stays isomorphic to the real buffer's at every step
    /// (neither ever removes or reorders a `Flush`), which makes any
    /// divergence between what the two return from a given `take()` call
    /// attributable only to compaction, not to a difference in which span
    /// of the raw sequence each one thinks is flushed.
    struct RawShadow {
        staged: Vec<UiEvent>,
        flush_index: Option<usize>,
    }

    impl RawShadow {
        fn fold(&mut self, events: impl IntoIterator<Item = UiEvent>) {
            for ev in events {
                let is_flush = matches!(ev, UiEvent::Flush);
                self.staged.push(ev);
                if is_flush {
                    self.flush_index = Some(self.staged.len() - 1);
                }
            }
        }

        fn take(&mut self) -> Vec<UiEvent> {
            let Some(idx) = self.flush_index else {
                return Vec::new();
            };
            self.flush_index = None;
            self.staged.drain(..=idx).collect()
        }
    }

    #[test]
    fn compaction_multi_take_schedule_matches_raw_at_each_flush_boundary() {
        // fold some events, take, fold more, take again, generatively: take()
        // calls do not line up with fold_batch's own chunk boundaries, and
        // gen_event is free to emit Flush and GridResize mid-sequence (not
        // just the fixed leading resize / trailing flush gen_sequence itself
        // appends).
        for seed in 0..300u64 {
            let raw = gen_sequence(seed, 60);

            let mut buf = DamageBuffer::default();
            let mut shadow = RawShadow {
                staged: Vec::new(),
                flush_index: None,
            };
            let mut sched_rng = storm::Xorshift::new(seed.wrapping_mul(48_271) | 1);
            let mut compacted_model = Model::new();
            let mut raw_model = Model::new();
            let mut any_take_verified = false;

            let mut idx = 0usize;
            while idx < raw.len() {
                let chunk = (1 + sched_rng.below(4) as usize).min(raw.len() - idx);
                let end = idx + chunk;
                buf.fold_batch(raw[idx..end].iter().cloned());
                shadow.fold(raw[idx..end].iter().cloned());
                idx = end;

                // take on roughly half of steps, and unconditionally on the
                // last chunk so nothing generated is left unverified
                if sched_rng.below(2) != 0 && idx != raw.len() {
                    continue;
                }
                let compacted_batch = buf.take();
                let raw_batch = shadow.take();
                if compacted_batch.is_empty() && raw_batch.is_empty() {
                    continue;
                }
                any_take_verified = true;

                let _ = update(&mut compacted_model, Msg::Redraw(compacted_batch.clone()));
                let _ = update(&mut raw_model, Msg::Redraw(raw_batch.clone()));
                assert_eq!(
                    grid_state(&compacted_model),
                    grid_state(&raw_model),
                    "seed {seed}: grid state diverged after a mid-schedule \
                     take(); compacted batch={compacted_batch:?} raw \
                     batch={raw_batch:?}"
                );

                let compacted_other: Vec<&UiEvent> = compacted_batch
                    .iter()
                    .filter(|e| !is_grid_op_event(e))
                    .collect();
                let raw_other: Vec<&UiEvent> =
                    raw_batch.iter().filter(|e| !is_grid_op_event(e)).collect();
                assert_eq!(
                    compacted_other, raw_other,
                    "seed {seed}: non-GridOp subsequence diverged mid-schedule"
                );
            }
            assert!(
                any_take_verified,
                "seed {seed}: schedule never actually drained anything"
            );
        }
    }
}
