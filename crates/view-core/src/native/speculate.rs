//! Display-only prediction of what insert-mode typing is about to show.
//!
//! Pure and headless-testable, on the same terms as
//! [`supervision`](crate::native::supervision): nothing here reads a clock,
//! holds an RPC handle, or knows a connection exists. What reaches this
//! module is a keystroke somebody else decoded, a cursor position somebody
//! else read off the authoritative grid, and a stamp somebody else measured.
//!
//! # Where the state machine ends and the folds begin
//!
//! [`SpeculateState`] is the decision -- what may be claimed, and what a
//! redraw takes back. [`fold_engine_call`], [`fold_redraw`] and
//! [`fold_expiry`] are the three places a host drives it from, and they live
//! here rather than in the host for one reason: every consumer that drives a
//! real engine (the runtime loop, and the differential battery that has to
//! answer whether the runtime is right) must classify a call, a batch and a
//! pass identically, and two copies of that classification can only be
//! pinned against each other, never made equal. They take a stamp instead of
//! reading a clock, which is what keeps them as pure as the state machine
//! they fold.
//!
//! # What a prediction claims, and what it must not
//!
//! nvim owns all buffer text, and a [`PredictedCell`] is not text: it is a
//! guess about one terminal cell, painted above the authoritative grid until
//! that grid says otherwise. Nothing held here is ever sent to the engine or
//! turned into an edit. The keystroke that produced a prediction reaches nvim
//! on exactly the path it always did, and nvim's own redraw remains the only
//! authority on what the buffer contains -- which is why a wrong prediction
//! costs a corrected cell on the next redraw and nothing else.
//!
//! # Why so little is predicted
//!
//! Speculation is worth having only where the guess is nearly always right,
//! because a wrong guess is a visible flicker. A plain character typed in
//! insert mode is that case: it appears at the cursor, and the cursor moves
//! on by one cell. Everything else -- motions, deletions, replace mode,
//! composed and double-width characters -- has no such single-cell answer,
//! so none of it is predicted, and every one of them invalidates whatever is
//! pending: the characters by reaching [`SpeculateState::predict`], the rest
//! by the caller contract that method states (see also [`Epoch`]).
//! Unpredicted is never wrong; it is only unaccelerated.

use std::time::Duration;

use crate::events::{GridCell, UiEvent, WinHandle};
use crate::model::Model;
use crate::msg::RpcCall;

/// When a prediction was made, as elapsed time from one fixed origin the
/// host chose once and keeps for the whole session.
///
/// A point on a monotonic timeline, never an interval: two stamps are
/// comparable only when both were built from the same origin, and the
/// difference between them is the only thing anything reads. A stamp taken
/// from an origin that restarts -- one measured from the start of some
/// episode, say -- would make every prediction look freshly made, and
/// [`SpeculateState::expire_stale`] would then never expire one. Hence a
/// type of its own rather than a duration, or a stamp shaped for a different
/// question.
///
/// Constructed by the host from its own clock and carried in, because
/// `view-core` reads no clock of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct SpecStamp(Duration);

impl SpecStamp {
    /// A stamp for the moment `since_origin` after the host's origin.
    #[must_use]
    pub const fn new(since_origin: Duration) -> Self {
        Self(since_origin)
    }

    /// How long after `earlier` this stamp was taken.
    ///
    /// Zero for a stamp that is not after `earlier` at all: two stamps
    /// arriving out of order say nothing about age, and reporting a
    /// saturating zero keeps the prediction alive for the next observation
    /// to judge rather than expiring it on a reading that never happened.
    #[must_use]
    pub fn age_since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

/// nvim's own name for insert mode in the `mode_change` event, and the one
/// mode this module speculates in.
///
/// Matched exactly rather than by prefix: `replace` overwrites the cell
/// ahead instead of inserting one, and the `cmdline_insert` family types
/// into a command line that is not the grid at all. Both would need a
/// different prediction, so neither gets one.
const INSERT_MODE: &str = "insert";

/// How long a prediction may sit pending without any authoritative redraw
/// having touched its cell before it is force-discarded, regardless of
/// epoch.
///
/// Bounds the case per-cell reconciliation against a redraw cannot close on
/// its own: a redraw batch that never happens to re-send the exact predicted
/// cell -- a partial repaint that misses the predicted columns, a batch
/// carrying only an overlay (a popup, a highlight) with no grid content at
/// all, or a `win_viewport` too short to decode and so read as
/// [`UiEvent::Unknown`] instead of the topline move it was -- would
/// otherwise leave a stale glyph on screen indefinitely. Sized well above
/// the slowest round trip a healthy remote session is expected to take, so
/// a legitimately slow but working link is
/// never mistaken for staleness; at a second, a user who somehow reaches it
/// is already looking at an editor that has stopped answering.
pub const SPECULATION_MAX_AGE: Duration = Duration::from_secs(1);

/// How many windows' `topline` this session tracks before the least
/// recently touched one is pruned to keep [`SpeculateState::viewports`]
/// bounded.
///
/// A floating window nvim mints for a completion popup or a preview is
/// closed within the same session that opened it, and without a cap each
/// one would leave its entry behind forever, growing the list -- and the
/// per-viewport scan every reconcile does over it -- without limit for a
/// session that opens enough of them. Pruning is safe to be wrong in the
/// direction it is: a pruned handle's next `win_viewport` reads as
/// first-seen (see [`SpeculateState::note_viewport`]), which never retires
/// by itself, so at worst a shift this cap evicted is left for
/// [`SPECULATION_MAX_AGE`] to catch instead of `reconcile` catching it
/// immediately. Sized well past any split or popup layout a real session
/// has open at once.
const MAX_TRACKED_VIEWPORTS: usize = 64;

/// Monotonic epoch, advanced on every mode change and on every keystroke
/// this module refuses to predict.
///
/// A predicted cell is meaningless once its epoch is stale: the context that
/// produced it -- the mode, and the run of single-width characters the
/// column arithmetic counted on -- no longer holds. Comparing epochs is what
/// lets a late or reordered redraw be judged without measuring the link it
/// arrived over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Epoch(u64);

impl Epoch {
    /// The epoch that supersedes this one.
    ///
    /// Saturating, so the successor of the last representable epoch is
    /// itself rather than a panic or a wrap back onto an epoch already
    /// used. Unreachable in any real session -- a keystroke per nanosecond
    /// for five centuries -- and the saturating reading is the safe one
    /// regardless: predictions tagged with a saturated epoch stay valid
    /// instead of being resurrected by a wrapped one.
    #[must_use]
    const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// One display-only predicted glyph: what plain-character insert-mode typing
/// is expected to show at `row`/`col` before the authoritative redraw
/// confirms or corrects it.
///
/// `predicted_at` bounds how long the prediction may survive an
/// authoritative redraw that never happens to touch its exact cell; see
/// [`SPECULATION_MAX_AGE`] and [`SpeculateState::expire_stale`].
///
/// # A cell outside the grid is dropped, never clamped
///
/// `row`/`col` are a prediction, so they can name a cell the live grid does
/// not have: the character that wraps to the next line, the one nvim's
/// `textwidth` breaks before, a prediction made just before a resize shrank
/// the grid under it. A consumer that finds such a cell must skip it. The
/// clamping reflex that is right for chrome geometry is wrong here -- a
/// clamped prediction paints a glyph the user did not type at the last real
/// column, and it stays there until an authoritative redraw touches that
/// exact cell or [`SPECULATION_MAX_AGE`] runs out. Skipping costs one
/// unaccelerated character; clamping shows a wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PredictedCell {
    /// The grid row the glyph is expected on.
    pub row: u16,
    /// The grid column the glyph is expected at, which may be past the live
    /// grid's last column (see this type's own doc).
    pub col: u16,
    /// The glyph itself, always a single-width character (see
    /// [`SpeculateState::predict`]).
    pub glyph: char,
    /// The epoch this prediction was made in. A prediction outliving its
    /// epoch is discarded unread.
    pub epoch: Epoch,
    /// When the prediction was made, as the host measured it.
    pub predicted_at: SpecStamp,
}

/// Pure prediction state: no I/O, no RPC awareness, no clock.
///
/// Mode-gated, so a picker query or a normal-mode motion never accumulates
/// speculative state that would have to be unwound.
#[derive(Debug, Clone, Default)]
pub struct SpeculateState {
    epoch: Epoch,
    pending: Vec<PredictedCell>,
    /// The last `topline` each window reported, so a viewport that moves can
    /// be told from one that merely re-reported itself (nvim sends
    /// `win_viewport` whenever the cursor moves within a window, which is
    /// every keystroke of a typing burst).
    ///
    /// Per window rather than one number for the frame: a split has several
    /// windows reporting viewports of their own, and a single slot would
    /// read every alternation between two of them as a shift and retire the
    /// burst that is being typed into one of them. Bounded at
    /// [`MAX_TRACKED_VIEWPORTS`], least-recently-touched entry evicted first,
    /// so a session that opens and closes many transient floating windows
    /// cannot grow this past its cap or the per-reconcile scan over it past
    /// a fixed size; see that constant's doc for what an eviction costs.
    viewports: Vec<(WinHandle, u64)>,
}

impl SpeculateState {
    /// Folds one insert-mode plain-character keystroke into a new predicted
    /// cell, tagged with the current epoch and `now`.
    ///
    /// `cursor` is the engine's last-known cursor position, in grid
    /// coordinates. `now` is elapsed time from whatever fixed origin the
    /// host stamps every call with; only differences between stamps are read
    /// here, never a stamp's absolute value.
    ///
    /// Returns `None` for every character that is not a plain character
    /// typed in insert mode. Those advance the epoch and discard what is
    /// pending instead of adding to it, via [`Self::reset_epoch`].
    ///
    /// # What the caller still owes
    ///
    /// Only keystrokes with a `char` form reach here, so the keys that have
    /// none -- `<Left>`, `<C-w>`, `<Esc>`, every notation key -- cannot
    /// invalidate anything by arriving. A caller that routes one of those to
    /// the engine must call [`Self::reset_epoch`] for it, and likewise on
    /// every mode change: each of them moves the cursor or changes what
    /// typing means, and a prediction that outlives either is a glyph
    /// standing where nothing was typed.
    ///
    /// # Where the cell goes
    ///
    /// A burst typed faster than the engine answers reports the same
    /// `cursor` for every keystroke in it -- that lag is the whole reason
    /// speculation exists -- so the cell is placed past the predictions this
    /// epoch already put on that row rather than on top of them, one column
    /// per predicted character.
    ///
    /// That arithmetic is a guess about a line that keeps going, and two
    /// things break it: a line long enough to wrap, and a `textwidth` that
    /// makes nvim break the line itself, both of which put the real
    /// character on the next row instead. The predicted column then runs off
    /// the grid, which is a cell a consumer drops rather than clamps (see
    /// [`PredictedCell`]); at the last representable column it saturates,
    /// leaving two predictions sharing a cell no terminal that wide has.
    /// Either way the authoritative redraw is what corrects it, and
    /// [`SPECULATION_MAX_AGE`] is what bounds how long it can be wrong.
    #[must_use]
    pub fn predict(
        &mut self,
        mode: &str,
        key: char,
        cursor: (u16, u16),
        now: SpecStamp,
    ) -> Option<PredictedCell> {
        if mode != INSERT_MODE || !is_plain(key) {
            self.reset_epoch();
            return None;
        }
        let (row, col) = cursor;
        let col = self
            .pending
            .iter()
            .filter(|cell| cell.row == row && cell.col >= col)
            .map(|cell| cell.col)
            .max()
            .map_or(col, |taken| taken.saturating_add(1));
        let cell = PredictedCell {
            row,
            col,
            glyph: key,
            epoch: self.epoch,
            predicted_at: now,
        };
        self.pending.push(cell);
        Some(cell)
    }

    /// Supersedes the epoch and discards every pending prediction.
    ///
    /// How speculation is invalidated. [`Self::predict`] calls it for every
    /// character it refuses, and the caller owes it for everything that
    /// never reaches `predict` as a character at all: a mode change, and any
    /// notation key ([`Self::predict`] states that contract in full).
    /// Discarding is unconditional because a prediction is only ever as good
    /// as the context that made it, and nothing that reaches here leaves
    /// that context intact.
    pub fn reset_epoch(&mut self) {
        self.epoch = self.epoch.next();
        self.pending.clear();
    }

    /// Discards every prediction that has been pending for
    /// [`SPECULATION_MAX_AGE`] as of `now`, whatever epoch it carries.
    ///
    /// The epoch is deliberately left alone: age says a prediction was never
    /// answered, not that the context it was made in has ended, and bumping
    /// the epoch here would discard the predictions that are still young
    /// alongside the one that timed out.
    ///
    /// Belongs at a call site the host reaches every pass, never one gated
    /// on a redraw arriving: a redraw that never comes is exactly the
    /// condition this bound exists to survive.
    pub fn expire_stale(&mut self, now: SpecStamp) {
        self.pending
            .retain(|cell| now.age_since(cell.predicted_at) < SPECULATION_MAX_AGE);
    }

    /// Retires every prediction one authoritative redraw batch has answered,
    /// and every prediction that batch is no longer about at all.
    ///
    /// Four readings, in the order they are taken:
    ///
    /// - A batch in which a window's `topline` moved is showing different
    ///   buffer lines at the same screen rows, so every prediction is now
    ///   over content it was never made for, and all of them are retired.
    ///   This is the relocation `grid_scroll` does not cover: nvim repaints
    ///   a jumped viewport line by line, and those lines need not reach the
    ///   predicted columns, so a per-cell reading sees nothing and a glyph
    ///   would stand over moved text until [`Self::expire_stale`]. Typing
    ///   never triggers it -- a burst that does not scroll the window keeps
    ///   `topline` exactly where it was, and one that does scroll the window
    ///   has moved its own predictions and is right to lose them.
    /// - A batch carrying a `mode_change` ends the context every pending
    ///   prediction was made in, so the whole epoch turns over
    ///   ([`Self::reset_epoch`]) and nothing else is read. This is the half
    ///   of that method's caller contract no keystroke can deliver: leaving
    ///   insert mode is something the engine announces, not something the
    ///   user types at this frontend.
    /// - A prediction not tagged with the current epoch is dropped
    ///   unconditionally, whatever the batch says about its cell and whether
    ///   or not the glyph would have matched. Nothing here reaches that
    ///   branch today: [`Self::reset_epoch`] is what advances the epoch, and
    ///   it clears every pending prediction in the same call, so `pending`
    ///   is epoch-homogeneous by construction and the compare can only ever
    ///   be true. It is kept as the second, independent enforcement of that
    ///   invariant -- one `u64` compare per pending cell -- because it is
    ///   what would still make a late or reordered redraw safe if some
    ///   future path ever superseded an epoch without clearing what the old
    ///   one left behind. As a conjunction operand it also costs nothing to
    ///   order: the glyph comparison beside it has no side effects, so
    ///   neither reading can change the other's outcome.
    /// - A prediction whose cell the batch answers is dropped. Confirmed and
    ///   mispredicted alike: the authoritative content is on that cell
    ///   either way, so the two differ in what they say about the guess, not
    ///   in what is left to paint over the grid. A prediction the batch
    ///   never reaches survives, for a later batch or for
    ///   [`Self::expire_stale`] to retire.
    ///
    /// Reads the redraw, never the grid the redraw is about to be applied
    /// to, so it is indifferent to whether the caller runs it before or
    /// after that application.
    ///
    /// Must be called for every batch, not only for batches arriving while
    /// something is pending: the viewport reading above is a comparison
    /// against what the last batch reported, and a caller that skips the
    /// quiet batches would compare a burst's first viewport against one from
    /// before the last time the user scrolled.
    pub fn reconcile(&mut self, redraw: &[UiEvent]) {
        let mut mode_changed = false;
        let mut shifted = false;
        for event in redraw {
            match event {
                UiEvent::ModeChange { .. } => mode_changed = true,
                UiEvent::WinViewport { win, topline, .. } => {
                    shifted |= self.note_viewport(*win, *topline);
                }
                _ => {}
            }
        }
        if mode_changed {
            self.reset_epoch();
            return;
        }
        if shifted {
            // the epoch is deliberately left alone, exactly as
            // `expire_stale` leaves it: a shift says the cells these
            // predictions named are showing something else, not that the
            // mode ended. Every pending prediction is cleared, not only the
            // shifted window's, because without ext_multigrid a prediction
            // carries no window of its own to tell a mover's cells from a
            // bystander's -- only the grid row and column shared by every
            // window on screen. Retiring a bystander's prediction over one
            // unaccelerated character is the safe direction to be wrong in;
            // painting a bystander's glyph over a mover's relocated content
            // is not.
            self.pending.clear();
            return;
        }
        let epoch = self.epoch;
        self.pending
            .retain(|cell| cell.epoch == epoch && !answered_by(redraw, cell));
    }

    /// Records `win`'s current `topline`, reporting whether it moved.
    ///
    /// A window seen for the first time never counts as moved: an unknown
    /// viewport is not a viewport that changed, and reading it as one would
    /// retire the first burst of every session for a window nvim had simply
    /// not reported yet.
    ///
    /// A window already tracked is moved to the back of `viewports` on
    /// every touch, so the front is always the least recently reported --
    /// what [`MAX_TRACKED_VIEWPORTS`] evicts when a new window arrives at
    /// the cap. A window mid-burst is touched every keystroke (nvim resends
    /// its viewport on every cursor move), which keeps it off the front for
    /// as long as it is actually the one being typed into.
    fn note_viewport(&mut self, win: WinHandle, topline: u64) -> bool {
        if let Some(index) = self.viewports.iter().position(|(seen, _)| *seen == win) {
            let (_, last) = self.viewports.remove(index);
            let moved = last != topline;
            self.viewports.push((win, topline));
            return moved;
        }
        if self.viewports.len() >= MAX_TRACKED_VIEWPORTS {
            self.viewports.remove(0);
        }
        self.viewports.push((win, topline));
        false
    }

    /// The predictions currently on screen ahead of the engine, in the order
    /// they were made.
    #[must_use]
    pub fn pending(&self) -> &[PredictedCell] {
        &self.pending
    }

    /// The epoch every pending prediction carries.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
}

/// Whether typing `key` in insert mode is expected to put exactly this
/// character into exactly one cell.
///
/// The ASCII graphic characters and the space are the whole set, and every
/// one of them holds a single cell, composes with nothing, and reaches the
/// grid as itself. Past ASCII none of that is derivable here: a combining
/// mark joins the glyph before it, an East Asian character claims two cells,
/// and `view-core` carries no character-width table that could tell either
/// apart from the characters that do neither. Those keystrokes are shown the
/// way they always were, by the engine's own redraw.
fn is_plain(key: char) -> bool {
    key.is_ascii_graphic() || key == ' '
}

/// Whether `redraw` has put authoritative content in `cell`'s own cell, or
/// moved whatever was there out from under it.
///
/// Matched variant by variant rather than through a wildcard, the same
/// discipline [`UiEvent`]'s own doc asks of every exhaustive match over it: a
/// redraw kind added later must be classified here by whoever adds it, since
/// the two wrong answers are a prediction painted over content that moved and
/// a prediction retired for an event that never touched it.
fn answered_by(redraw: &[UiEvent], cell: &PredictedCell) -> bool {
    redraw.iter().any(|ev| match ev {
        // every event's `grid` id is deliberately unread: view attaches
        // `ext_linegrid` without `ext_multigrid`, so nvim sends one grid and
        // a prediction can only ever be about that one. The day a second
        // grid arrives, this is what has to read the id first -- until then
        // reading it would only add a comparison whose answer is fixed
        UiEvent::GridLine {
            row,
            col_start,
            cells,
            ..
        } => *row == u64::from(cell.row) && covers_column(*col_start, cells, cell.col),
        // content relocated, dropped or reshaped without the cells it moved
        // being re-sent: the coordinates a prediction holds no longer name
        // the place its glyph was predicted for, and no later batch is
        // obliged to say so cell by cell
        UiEvent::GridScroll { .. } | UiEvent::GridClear { .. } | UiEvent::GridResize { .. } => true,
        // a viewport that moved is read by `reconcile` itself, one reading
        // earlier and against the last one this window reported -- the event
        // on its own says nothing, since nvim sends it for every cursor move
        // inside an unmoved window
        UiEvent::WinViewport { .. } => false,
        // everything else moves no grid content: chrome, highlights,
        // overlays, and the cursor's own position (which is read at predict
        // time, never re-read here)
        UiEvent::GridCursorGoto { .. }
        | UiEvent::HlAttrDefine { .. }
        | UiEvent::DefaultColorsSet { .. }
        | UiEvent::HlGroupSet { .. }
        | UiEvent::Flush
        | UiEvent::ModeInfoSet { .. }
        | UiEvent::ModeChange { .. }
        | UiEvent::CmdlineShow { .. }
        | UiEvent::CmdlinePos { .. }
        | UiEvent::CmdlineHide
        | UiEvent::MsgShow { .. }
        | UiEvent::MsgClear
        | UiEvent::MsgShowmode { .. }
        | UiEvent::MsgShowcmd { .. }
        | UiEvent::MsgRuler { .. }
        | UiEvent::TablineUpdate { .. }
        | UiEvent::PopupmenuShow { .. }
        | UiEvent::PopupmenuSelect { .. }
        | UiEvent::PopupmenuHide
        | UiEvent::MouseOn
        | UiEvent::MouseOff
        | UiEvent::Unknown { .. } => false,
    })
}

/// Whether a `grid_line` run starting at `col_start` reaches `col`.
///
/// A run's width is the sum of its cells' `repeat` counts, not its cell
/// count: nvim collapses a stretch of identical cells into one entry, and
/// reading the entry count instead would leave every prediction past the
/// first collapsed run un-answered until the age bound caught it.
fn covers_column(col_start: u64, cells: &[GridCell], col: u16) -> bool {
    let width = cells
        .iter()
        .map(|cell| cell.repeat)
        .fold(0, u64::saturating_add);
    let col = u64::from(col);
    col >= col_start && col < col_start.saturating_add(width)
}

/// Folds one call the host is sending the engine into what speculation may
/// still claim, marking the frame when that changed what is pending.
///
/// Three calls can touch a prediction, and they are the three that reach the
/// buffer or the cursor: the keystroke a prediction is made from, and the
/// paste and mouse events that move one or the other without any character
/// ever reaching [`SpeculateState::predict`] -- the caller obligation that
/// method states, for the two paths that are not keys. Everything else a
/// frontend sends nvim (an option, a resize, a reply, a registration) leaves
/// both the buffer and the cursor where the prediction found them; a resize
/// additionally comes back as a `grid_resize` that [`fold_redraw`] reads for
/// itself.
///
/// `now` is a stamp the host took from its own fixed origin, since nothing
/// in this module reads a clock.
pub fn fold_engine_call(model: &mut Model, call: &RpcCall, now: SpecStamp) {
    match call {
        RpcCall::Input { notation } => fold_keystroke(model, notation, now),
        RpcCall::Paste { .. } | RpcCall::InputMouse { .. } => fold_invalidation(model),
        // `RpcCall` is `#[non_exhaustive]`: a call added later is assumed to
        // touch neither the buffer nor the cursor until someone decides
        // otherwise, which is the reading that costs an unaccelerated
        // character rather than a wrong one
        _ => {}
    }
}

/// Judges every pending prediction against the redraw batch that just
/// arrived, and marks the frame when that retired one.
///
/// Runs for every batch, including the batches arriving with nothing pending:
/// [`SpeculateState::reconcile`] is where a window's viewport is read, and
/// that reading is only worth what the batch before it recorded.
pub fn fold_redraw(model: &mut Model, redraw: &[UiEvent]) {
    let before = model.speculate.pending().len();
    model.speculate.reconcile(redraw);
    mark_retirement(model, before);
}

/// The host's per-pass age check on what speculation is still holding.
///
/// Belongs at a call site reached whether or not a redraw arrived: a redraw
/// that never comes is the condition [`SPECULATION_MAX_AGE`] exists for.
pub fn fold_expiry(model: &mut Model, now: SpecStamp) {
    // the pending list is read before anything else so a steady-state pass
    // costs one length compare: expiring an empty list is a no-op, and a
    // session outside a typing burst takes that pass forever
    if model.speculate.pending().is_empty() {
        return;
    }
    let before = model.speculate.pending().len();
    model.speculate.expire_stale(now);
    mark_retirement(model, before);
}

/// Folds one engine-bound keystroke into the display-only prediction it is
/// expected to produce.
fn fold_keystroke(model: &mut Model, notation: &str, now: SpecStamp) {
    let Some(key) = lone_char(notation) else {
        // the caller obligation `predict` states: a notation key moves the
        // cursor or changes what typing means without ever reaching it as a
        // character, so nothing pending can survive one
        fold_invalidation(model);
        return;
    };
    let cursor = model.engine.grid().cursor();
    let mode = model.engine.mode.current.as_str();
    let before = model.speculate.pending().len();
    // the refusal path is why the answer is read off the pending list rather
    // than off this `Option`: a character `predict` declines discards
    // everything pending inside `predict` itself, and the caller sees only
    // the `None` it shares with a mode that was never predicting at all
    let _ = model.speculate.predict(mode, key, cursor, now);
    mark_retirement(model, before);
}

/// Discards everything pending because something that is not a predictable
/// keystroke has reached the buffer or the cursor.
fn fold_invalidation(model: &mut Model) {
    let before = model.speculate.pending().len();
    model.speculate.reset_epoch();
    mark_retirement(model, before);
}

/// Marks the frame when a fold has changed what speculation is holding since
/// `before`.
///
/// Every fold here ends in this call, and that is the point: a frame is
/// painted only once something has marked it, so a fold that retires an
/// already-painted prediction without marking one leaves the glyph on the
/// terminal with nothing left to take it off -- the age bound cannot rescue
/// it either, since it has nothing pending left to find stale. Marking on any
/// change rather than on a shrink covers the appearing prediction with the
/// same rule; marking on no change at all is what keeps every `<Esc>`, click
/// and normal-mode key of a session outside a typing burst from buying a
/// repaint.
fn mark_retirement(model: &mut Model, before: usize) {
    model.dirty |= model.speculate.pending().len() != before;
}

/// The one character `notation` is, or `None` when it is a notation key.
///
/// nvim notation writes every key that is not a single printable character as
/// a bracketed name -- `<Esc>`, `<C-w>`, `<S-Tab>`, and `<lt>` for a literal
/// `<` -- so "exactly one char" separates the two with no table of key names
/// to keep in step with the encoder that produced them.
fn lone_char(notation: &str) -> Option<char> {
    let mut chars = notation.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn stamp(millis: u64) -> SpecStamp {
        SpecStamp::new(Duration::from_millis(millis))
    }

    /// One `grid_line` run of single-width cells, the shape nvim sends for a
    /// freshly typed character.
    fn grid_line(row: u64, col_start: u64, text: &str) -> UiEvent {
        UiEvent::GridLine {
            grid: 1,
            row,
            col_start,
            cells: text
                .chars()
                .map(|ch| GridCell {
                    text: ch.to_string(),
                    hl_id: 0,
                    repeat: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn an_insert_mode_plain_character_is_predicted_at_the_cursor() {
        let mut state = SpeculateState::default();
        let predicted = state.predict("insert", 'a', (3, 7), stamp(120));
        let cell = predicted.expect("a plain character typed in insert mode is predictable");
        assert_eq!(cell.row, 3);
        assert_eq!(cell.col, 7);
        assert_eq!(cell.glyph, 'a');
        assert_eq!(cell.predicted_at, stamp(120));
        assert_eq!(state.pending(), &[cell]);
    }

    #[test]
    fn a_control_character_discards_what_is_pending_and_supersedes_the_epoch() {
        let mut state = SpeculateState::default();
        let before = state.epoch();
        assert!(state.predict("insert", 'a', (0, 0), stamp(0)).is_some());

        assert_eq!(state.predict("insert", '\u{8}', (0, 1), stamp(10)), None);
        assert!(state.pending().is_empty());
        assert!(state.epoch() > before);
    }

    /// The disconfirming case for the epoch model: predictions made before a
    /// mode change must not merely be hidden, they must be gone, and the
    /// prediction after the change must be distinguishable from them.
    #[test]
    fn no_prediction_survives_the_mode_change_that_invalidated_it() {
        let mut state = SpeculateState::default();
        for (offset, key) in "hello".chars().enumerate() {
            let col = u16::try_from(offset).unwrap();
            assert!(state.predict("insert", key, (2, col), stamp(0)).is_some());
        }
        let stale = state.epoch();
        assert_eq!(state.pending().len(), 5);

        assert_eq!(state.predict("normal", 'j', (2, 5), stamp(20)), None);
        assert!(state.pending().is_empty());

        let cell = state
            .predict("insert", 'x', (2, 5), stamp(30))
            .expect("insert mode resumes predicting after the epoch turns over");
        assert!(cell.epoch > stale);
        assert_eq!(state.pending(), &[cell]);
    }

    #[test]
    fn a_key_typed_outside_insert_mode_is_never_predicted() {
        let mut state = SpeculateState::default();
        for mode in ["normal", "visual", "replace", "cmdline_normal", "terminal"] {
            assert_eq!(state.predict(mode, 'a', (0, 0), stamp(0)), None, "{mode}");
            assert!(state.pending().is_empty(), "{mode}");
        }
    }

    /// A character `view-core` cannot size is left to the engine: predicting
    /// a single cell for a combining mark or a double-width glyph would put
    /// every later prediction in the run one column out.
    #[test]
    fn a_character_outside_ascii_is_left_to_the_engine() {
        let mut state = SpeculateState::default();
        for key in ['\u{301}', '\u{4e16}', 'é'] {
            assert_eq!(state.predict("insert", key, (0, 0), stamp(0)), None);
            assert!(state.pending().is_empty());
        }
    }

    /// The case speculation exists for: keystrokes typed faster than the
    /// link answers, so every one of them sees the same stale cursor.
    #[test]
    fn a_burst_typed_ahead_of_the_engine_cursor_lands_on_consecutive_cells() {
        let mut state = SpeculateState::default();
        for key in ['a', 'b', 'c'] {
            assert!(state.predict("insert", key, (4, 9), stamp(0)).is_some());
        }
        let cells: Vec<(u16, u16, char)> = state
            .pending()
            .iter()
            .map(|cell| (cell.row, cell.col, cell.glyph))
            .collect();
        assert_eq!(cells, vec![(4, 9, 'a'), (4, 10, 'b'), (4, 11, 'c')]);
    }

    /// The bound on a prediction no redraw ever contradicts or confirms,
    /// because no redraw happens to cover its cell. Age closes that window
    /// on its own, without an epoch change and without a redraw.
    #[test]
    fn a_prediction_no_redraw_ever_reaches_expires_on_age_alone() {
        let mut state = SpeculateState::default();
        let old = state
            .predict("insert", 'a', (1, 1), stamp(0))
            .expect("plain insert-mode character");
        let recent = state
            .predict("insert", 'b', (1, 1), stamp(900))
            .expect("plain insert-mode character");
        let epoch = state.epoch();

        state.expire_stale(stamp(1000));

        assert_eq!(
            state.pending(),
            &[recent],
            "{old:?} reached the age bound; {recent:?} did not"
        );
        assert_eq!(
            state.epoch(),
            epoch,
            "expiry retires predictions, it does not invalidate the context they were made in"
        );
    }

    #[test]
    fn expiry_reads_the_age_bound_as_reached_rather_than_passed() {
        let mut state = SpeculateState::default();
        assert!(state.predict("insert", 'a', (0, 0), stamp(0)).is_some());

        state.expire_stale(SpecStamp::new(
            SPECULATION_MAX_AGE.saturating_sub(Duration::from_nanos(1)),
        ));
        assert_eq!(state.pending().len(), 1);

        state.expire_stale(SpecStamp::new(SPECULATION_MAX_AGE));
        assert!(state.pending().is_empty());
    }

    /// Stamps that arrive out of order carry no age reading, so the
    /// prediction waits for one that does instead of being expired on a
    /// measurement that never happened.
    #[test]
    fn a_stamp_older_than_the_prediction_expires_nothing() {
        let mut state = SpeculateState::default();
        let cell = state
            .predict("insert", 'a', (0, 0), stamp(5_000))
            .expect("plain insert-mode character");

        state.expire_stale(stamp(0));

        assert_eq!(state.pending(), &[cell]);
        assert_eq!(stamp(0).age_since(stamp(5_000)), Duration::ZERO);
    }

    /// Expiry retires the cells that timed out and nothing else, so the next
    /// prediction still lands past the survivors rather than reusing the
    /// column an expired one had.
    #[test]
    fn placement_after_a_partial_expiry_lands_past_the_survivors() {
        let mut state = SpeculateState::default();
        assert!(state.predict("insert", 'a', (1, 1), stamp(0)).is_some());
        let survivor = state
            .predict("insert", 'b', (1, 1), stamp(900))
            .expect("plain insert-mode character");

        state.expire_stale(stamp(1000));
        assert_eq!(state.pending(), &[survivor]);

        let next = state
            .predict("insert", 'c', (1, 1), stamp(1000))
            .expect("plain insert-mode character");
        assert_eq!((next.col, next.glyph), (3, 'c'));
    }

    /// The epoch counter saturates rather than wrapping: a wrapped epoch
    /// would eventually equal one that predictions still carry, which is the
    /// one reading that could resurrect them.
    #[test]
    fn the_epoch_saturates_instead_of_wrapping_onto_an_epoch_already_used() {
        let mut state = SpeculateState {
            epoch: Epoch(u64::MAX),
            ..SpeculateState::default()
        };
        assert!(state.predict("insert", 'a', (0, 0), stamp(0)).is_some());

        state.reset_epoch();

        assert_eq!(state.epoch(), Epoch(u64::MAX));
        assert!(state.pending().is_empty());
    }

    /// The ordinary case: the engine catches up and says the predicted glyph
    /// is really there, so there is nothing left to paint ahead of it.
    #[test]
    fn a_redraw_that_confirms_a_prediction_retires_it() {
        let mut state = SpeculateState::default();
        assert!(state.predict("insert", 'a', (2, 4), stamp(0)).is_some());

        state.reconcile(&[grid_line(2, 4, "a"), UiEvent::Flush]);

        assert!(state.pending().is_empty());
    }

    /// A wrong guess costs a corrected cell and nothing else: the
    /// authoritative redraw wins, exactly as it would have with no
    /// speculation at all, and no glyph is left painted over it.
    #[test]
    fn a_redraw_that_contradicts_a_prediction_retires_it_just_the_same() {
        let mut state = SpeculateState::default();
        assert!(state.predict("insert", 'a', (2, 4), stamp(0)).is_some());

        state.reconcile(&[grid_line(2, 4, "z")]);

        assert!(state.pending().is_empty());
    }

    /// The reordering case, and the one that says which property is load
    /// bearing: a prediction from a superseded epoch goes even when the
    /// redraw would have confirmed it glyph for glyph, and even when the
    /// redraw never reaches its cell at all. Epoch, not glyph match, is what
    /// makes a late or reordered redraw safe.
    #[test]
    fn a_prediction_from_a_superseded_epoch_is_dropped_whatever_the_redraw_says() {
        let matching = PredictedCell {
            row: 2,
            col: 4,
            glyph: 'a',
            epoch: Epoch(1),
            predicted_at: stamp(0),
        };
        let untouched = PredictedCell {
            row: 7,
            col: 0,
            glyph: 'b',
            ..matching
        };
        let mut state = SpeculateState {
            epoch: Epoch(2),
            pending: vec![matching, untouched],
            ..SpeculateState::default()
        };

        state.reconcile(&[grid_line(2, 4, "a")]);

        assert!(
            state.pending().is_empty(),
            "a superseded epoch retires a prediction whether or not the redraw speaks to its cell"
        );
    }

    /// The window per-cell matching cannot close on its own: a partial
    /// update that never reaches the predicted cell leaves the prediction
    /// standing, and only the age bound retires it.
    #[test]
    fn a_redraw_that_never_reaches_a_predicted_cell_leaves_it_for_the_age_bound() {
        let mut state = SpeculateState::default();
        let cell = state
            .predict("insert", 'a', (2, 40), stamp(0))
            .expect("plain insert-mode character");

        state.reconcile(&[grid_line(2, 0, "hello"), grid_line(9, 40, "elsewhere")]);
        assert_eq!(
            state.pending(),
            &[cell],
            "neither run covers row 2 column 40"
        );

        state.expire_stale(stamp(1_000));
        assert!(state.pending().is_empty());
    }

    /// A run is as wide as its repeat counts say, not as wide as its entry
    /// count: nvim collapses a stretch of identical cells into one entry,
    /// and a prediction inside that stretch has been answered.
    #[test]
    fn a_run_answers_every_column_its_repeat_counts_cover() {
        let mut state = SpeculateState::default();
        assert!(state.predict("insert", 'a', (0, 6), stamp(0)).is_some());

        state.reconcile(&[UiEvent::GridLine {
            grid: 1,
            row: 0,
            col_start: 0,
            cells: vec![GridCell {
                text: " ".to_string(),
                hl_id: 0,
                repeat: 10,
            }],
        }]);

        assert!(state.pending().is_empty());
    }

    /// A scroll, a clear and a resize each relocate, drop or reshape
    /// content without re-sending the cells they moved, so every prediction
    /// they passed over names a place its glyph was never predicted for --
    /// and no later batch is obliged to say so cell by cell.
    #[test]
    fn content_moved_without_being_resent_retires_every_prediction_it_passed_over() {
        for event in [
            UiEvent::GridScroll {
                grid: 1,
                top: 0,
                bot: 10,
                left: 0,
                right: 80,
                rows: 1,
            },
            UiEvent::GridClear { grid: 1 },
            UiEvent::GridResize {
                grid: 1,
                width: 100,
                height: 30,
            },
        ] {
            let mut state = SpeculateState::default();
            assert!(state.predict("insert", 'a', (2, 4), stamp(0)).is_some());

            state.reconcile(std::slice::from_ref(&event));

            assert!(state.pending().is_empty(), "{event:?}");
        }
    }

    /// The half of `predict`'s caller contract no keystroke can deliver:
    /// leaving insert mode is something the engine announces in a redraw
    /// batch, so that batch is where the epoch turns over.
    #[test]
    fn a_mode_change_in_the_batch_supersedes_the_epoch() {
        let mut state = SpeculateState::default();
        assert!(state.predict("insert", 'a', (2, 4), stamp(0)).is_some());
        let before = state.epoch();

        state.reconcile(&[UiEvent::ModeChange {
            mode: "normal".to_string(),
            mode_idx: 0,
        }]);

        assert!(state.pending().is_empty());
        assert!(state.epoch() > before);
    }

    /// A batch that says nothing about the grid leaves speculation exactly
    /// as it was: an unrelated highlight definition or message is not an
    /// answer to a prediction.
    #[test]
    fn a_batch_that_touches_no_grid_content_retires_nothing() {
        let mut state = SpeculateState::default();
        let cell = state
            .predict("insert", 'a', (2, 4), stamp(0))
            .expect("plain insert-mode character");

        state.reconcile(&[
            UiEvent::HlGroupSet {
                name: "Normal".to_string(),
                hl_id: 1,
            },
            UiEvent::MsgClear,
            UiEvent::Flush,
        ]);

        assert_eq!(state.pending(), &[cell]);
    }

    /// One window's viewport report.
    fn viewport(win: u64, topline: u64) -> UiEvent {
        UiEvent::WinViewport {
            grid: 1,
            win: WinHandle(win),
            topline,
            botline: topline + 12,
            curline: topline,
            curcol: 0,
        }
    }

    /// The gap a per-cell reading cannot close: a window that jumps is
    /// repainted line by line, and those lines need not carry the predicted
    /// columns, so the only thing that says the glyph is now over different
    /// content is the viewport itself.
    #[test]
    fn a_window_whose_topline_moved_retires_every_prediction() {
        let mut state = SpeculateState::default();
        state.reconcile(&[viewport(1, 10)]);
        let _ = state
            .predict("insert", 'a', (2, 4), stamp(0))
            .expect("plain insert-mode character");

        state.reconcile(&[viewport(1, 13), UiEvent::Flush]);

        assert!(state.pending().is_empty());
    }

    /// And the reason it cannot simply retire on the event: nvim reports a
    /// viewport for every cursor move inside an unmoved window, which is
    /// every keystroke of the burst the feature exists for.
    #[test]
    fn a_viewport_reported_again_at_the_same_topline_retires_nothing() {
        let mut state = SpeculateState::default();
        state.reconcile(&[viewport(1, 10)]);
        let cell = state
            .predict("insert", 'a', (2, 4), stamp(0))
            .expect("plain insert-mode character");

        state.reconcile(&[viewport(1, 10), UiEvent::Flush]);

        assert_eq!(state.pending(), &[cell]);
    }

    /// A split reports a viewport per window, and two windows parked at
    /// different toplines alternate on the wire. Reading that alternation as
    /// movement would retire every burst typed in either of them.
    #[test]
    fn a_second_windows_viewport_never_reads_as_the_first_one_moving() {
        let mut state = SpeculateState::default();
        state.reconcile(&[viewport(1, 10), viewport(2, 80)]);
        let cell = state
            .predict("insert", 'a', (2, 4), stamp(0))
            .expect("plain insert-mode character");

        state.reconcile(&[viewport(2, 80)]);
        state.reconcile(&[viewport(1, 10)]);

        assert_eq!(state.pending(), &[cell]);
    }

    /// A window nobody has heard from yet has not moved: an unknown viewport
    /// is not a changed one, and reading it as one would cost the first
    /// burst typed into every newly opened window.
    #[test]
    fn a_window_seen_for_the_first_time_is_not_a_window_that_moved() {
        let mut state = SpeculateState::default();
        let cell = state
            .predict("insert", 'a', (2, 4), stamp(0))
            .expect("plain insert-mode character");

        state.reconcile(&[viewport(7, 42)]);

        assert_eq!(state.pending(), &[cell]);
    }

    /// A session that opens and closes many transient floating windows --
    /// one per completion popup, say -- must not grow the viewport map
    /// without bound, and a window that is genuinely still being typed into
    /// (touched between every churn event, exactly as a real burst's
    /// `win_viewport` reports would) must keep tracking correctly the whole
    /// time: the LRU eviction has to be evicting the transient handles, not
    /// the live one, for that to hold.
    #[test]
    fn transient_window_churn_stays_bounded_and_a_live_window_still_retires() {
        let mut state = SpeculateState::default();
        state.reconcile(&[viewport(1, 10)]);
        let _ = state
            .predict("insert", 'a', (2, 4), stamp(0))
            .expect("plain insert-mode character");

        for handle in 0..u64::try_from(MAX_TRACKED_VIEWPORTS * 4).unwrap() {
            state.reconcile(&[viewport(1000 + handle, 0)]);
            state.reconcile(&[viewport(1, 10)]);
            assert!(
                state.viewports.len() <= MAX_TRACKED_VIEWPORTS,
                "viewport map grew past its bound during churn"
            );
        }

        state.reconcile(&[viewport(1, 13), UiEvent::Flush]);

        assert!(
            state.pending().is_empty(),
            "a live window kept off the eviction front by every touch should still retire on a real shift"
        );
    }

    /// The far edge of the column arithmetic: past the last representable
    /// column two predictions share a cell. Pinned rather than defended
    /// against, since a grid that wide does not exist -- and a consumer that
    /// drops out-of-grid cells (see [`PredictedCell`]) paints neither.
    #[test]
    fn the_predicted_column_saturates_at_the_last_representable_cell() {
        let mut state = SpeculateState::default();
        let first = state
            .predict("insert", 'a', (0, u16::MAX), stamp(0))
            .expect("plain insert-mode character");
        let second = state
            .predict("insert", 'b', (0, u16::MAX), stamp(0))
            .expect("plain insert-mode character");

        assert_eq!(first.col, u16::MAX);
        assert_eq!(second.col, u16::MAX);
    }

    // -- the folds a host drives the state machine from --------------------

    /// A model mid-typing-burst: a grid, the cursor where the engine last
    /// reported it, and insert mode active.
    fn typing_model() -> Model {
        use crate::grid::GridOp;
        let mut model = Model::with_term_size(80, 24);
        model.engine.apply_grid(GridOp::Resize {
            width: 80,
            height: 24,
        });
        model
            .engine
            .apply_grid(GridOp::CursorGoto { row: 3, col: 5 });
        model.engine.mode.current = "insert".to_string();
        model.dirty = false;
        model
    }

    /// One engine-bound keystroke.
    fn input(notation: &str) -> RpcCall {
        RpcCall::Input {
            notation: notation.to_string(),
        }
    }

    /// Why [`fold_redraw`] runs on every batch and not only on the batches
    /// arriving mid-burst: the viewport comparison is against what the last
    /// batch reported, so a host that skipped the quiet ones would judge a
    /// burst's first viewport against one from before the user last
    /// scrolled -- and retire the burst for a move that already happened.
    #[test]
    fn a_batch_arriving_with_nothing_pending_still_records_the_viewport() {
        let mut model = typing_model();
        fold_redraw(&mut model, &[viewport(1, 10), UiEvent::Flush]);
        fold_redraw(&mut model, &[viewport(1, 40), UiEvent::Flush]);
        model.dirty = false;

        fold_engine_call(&mut model, &input("x"), stamp(0));
        fold_redraw(&mut model, &[viewport(1, 40), UiEvent::Flush]);

        assert_eq!(
            model.speculate.pending().len(),
            1,
            "a viewport that never moved retired the burst"
        );
    }

    /// A paste and a mouse click both move the cursor or the text under a
    /// prediction without any character reaching `predict`, so both owe the
    /// invalidation a notation key owes -- and both owe the mark, since the
    /// glyph they retire is already on the terminal.
    #[test]
    fn a_paste_or_a_click_retires_a_painted_prediction_and_marks_the_frame() {
        for call in [
            RpcCall::Paste {
                text: "pasted".to_string(),
            },
            RpcCall::InputMouse {
                button: "left".to_string(),
                action: "press".to_string(),
                modifier: String::new(),
                row: 9,
                col: 9,
            },
        ] {
            let mut model = typing_model();
            fold_engine_call(&mut model, &input("x"), stamp(0));
            assert_eq!(model.speculate.pending().len(), 1, "{call:?}");
            model.dirty = false;

            fold_engine_call(&mut model, &call, stamp(1));

            assert!(model.speculate.pending().is_empty(), "{call:?}");
            assert!(
                model.dirty,
                "{call:?} retired a painted prediction and marked no frame"
            );
        }
    }

    /// The same obligation for the character `predict` itself refuses: the
    /// refusal clears what is pending inside `predict`, so the fold sees
    /// only a `None` and has to read the pending list to notice.
    #[test]
    fn a_refused_character_that_retires_a_painted_prediction_marks_the_frame() {
        let mut model = typing_model();
        fold_engine_call(&mut model, &input("x"), stamp(0));
        model.engine.mode.current = "normal".to_string();
        model.dirty = false;

        fold_engine_call(&mut model, &input("j"), stamp(1));

        assert!(model.speculate.pending().is_empty());
        assert!(
            model.dirty,
            "a refused character retired a painted prediction and marked no frame"
        );
    }

    /// Nothing pending is the steady state for all three invalidation
    /// classes, and none of them may mark a frame there: a repaint bought by
    /// every `<Esc>`, every click and every normal-mode key of every session
    /// is a cost the feature never earns back.
    #[test]
    fn an_invalidation_with_nothing_pending_marks_no_frame() {
        let mut model = typing_model();

        fold_engine_call(&mut model, &input("<Left>"), stamp(0));
        fold_engine_call(
            &mut model,
            &RpcCall::Paste {
                text: "pasted".to_string(),
            },
            stamp(1),
        );
        model.engine.mode.current = "normal".to_string();
        fold_engine_call(&mut model, &input("j"), stamp(2));

        assert!(model.speculate.pending().is_empty());
        assert!(
            !model.dirty,
            "an invalidation with nothing to retire repainted"
        );
    }

    /// A call that reaches neither the buffer nor the cursor leaves a
    /// prediction standing: invalidating on one would cost an accelerated
    /// character for nothing.
    #[test]
    fn a_call_that_reaches_neither_the_buffer_nor_the_cursor_retires_nothing() {
        let mut model = typing_model();
        fold_engine_call(&mut model, &input("x"), stamp(0));

        fold_engine_call(
            &mut model,
            &RpcCall::GetDefaultHl { generation: 1 },
            stamp(1),
        );

        assert_eq!(model.speculate.pending().len(), 1);
    }

    /// `<lt>` is nvim's notation for a typed `<`, and it is three characters
    /// on the wire: predicting one glyph per character would put a `<`, a
    /// `l` and a `t` on the grid.
    #[test]
    fn a_bracketed_notation_is_never_mistaken_for_the_characters_that_spell_it() {
        for notation in ["<lt>", "<Esc>", "<C-w>", "<S-Tab>"] {
            assert_eq!(lone_char(notation), None, "{notation}");
        }
        assert_eq!(lone_char("x"), Some('x'));
        assert_eq!(lone_char(" "), Some(' '));
    }

    /// Speculation is mode-gated at the model, so a key typed outside insert
    /// mode reaches nvim exactly as before and leaves nothing behind.
    #[test]
    fn a_key_folded_outside_insert_mode_predicts_nothing() {
        let mut model = typing_model();
        model.engine.mode.current = "normal".to_string();

        fold_engine_call(&mut model, &input("j"), stamp(0));

        assert!(model.speculate.pending().is_empty());
        assert!(!model.dirty, "an unpredicted key repaints nothing");
    }
}
