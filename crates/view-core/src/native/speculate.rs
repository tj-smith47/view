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
use crate::grid::registry::{GridId, GLOBAL_GRID};
use crate::model::Model;
use crate::msg::{Effect, RpcCall};

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
///
/// The speculated palette's own backstop ([`cmdline_backstop`]) is capped
/// here and never reaches past it, so a guess the engine never answers
/// comes off no later than a predicted glyph does.
pub const SPECULATION_MAX_AGE: Duration = Duration::from_secs(1);

/// The floor under the speculated palette's backstop, whatever the link
/// says.
///
/// A wrong guess nothing else refutes -- a `:` a plugin's own `getchar()`
/// swallowed, a `:` inside a multi-key mapping the gate cannot see -- is an
/// empty palette standing for this long, so the floor is the longest such
/// box a local session is asked to look at. Sized to the round trip of a
/// link where a correct guess is answered well inside it, which is what
/// keeps the floor from blanking guesses that were right.
pub const CMDLINE_SPECULATION_BACKSTOP_MIN: Duration = Duration::from_millis(250);

/// How many round trips a guess is given before the backstop takes it back.
///
/// The `cmdline_show` that answers a correct guess is one round trip behind
/// the key. The margin is what a session that is answering, but answering
/// slowly, needs before a bound starts blanking correct guesses: nvim
/// reading the key, running whatever a `CmdlineEnter` autocommand does, and
/// flushing.
const CMDLINE_BACKSTOP_ROUND_TRIPS: u32 = 3;

/// How long a speculated palette may stand unanswered on this session's
/// link.
///
/// The bound the wrong guesses that move nothing are paid out of, and the
/// only clock the palette has: everything else takes a guess back on
/// evidence ([`fold_redraw`]). Sizing it to the link rather than to
/// [`SPECULATION_MAX_AGE`] is what parts the two failures. A local session
/// answers every `:` far inside the floor, so a guess a plugin swallowed
/// comes off in [`CMDLINE_SPECULATION_BACKSTOP_MIN`] instead of standing
/// for the whole of the glyphs' bound. A remote one, whose correct guesses
/// are answered on the tiers `view-bench`'s `echo_speculated_rtt` scenario
/// ships in `RTT_TIERS_MS`, keeps a bound its own answers fit inside, so
/// the palette is not drawn, blanked and drawn again on every `:`.
///
/// The reading behind it is [`crate::model::EngineModel::key_round_trips`]'s
/// -- the longest of the last [`KEY_ROUND_TRIPS`] key-to-batch round trips,
/// which is the half of the pair that cannot be too short without also
/// being a bound the session has since stopped deserving.
#[must_use]
pub fn cmdline_backstop(model: &Model) -> Duration {
    model
        .engine
        .key_round_trips
        .iter()
        .flatten()
        .max()
        .and_then(|trip| trip.checked_mul(CMDLINE_BACKSTOP_ROUND_TRIPS))
        .unwrap_or(CMDLINE_SPECULATION_BACKSTOP_MIN)
        .clamp(CMDLINE_SPECULATION_BACKSTOP_MIN, SPECULATION_MAX_AGE)
}

/// How many recent key-to-answering-batch round trips
/// [`cmdline_backstop`] reads.
///
/// Eight, because the bound has to follow the link and not the one slow
/// answer a plugin's first-run work produced: at a user's own typing rate
/// eight keys is a second or two of history, long enough that a genuinely
/// slow link keeps its bound through a pause in typing and short enough
/// that a hiccup is gone by the next sentence. The scan is eight compares
/// on a path already walking a redraw batch.
pub const KEY_ROUND_TRIPS: usize = 8;

/// The normal- and visual-mode keys after which nvim reads the next
/// keystroke as that command's own argument rather than as a command.
///
/// nvim announces no mode for a command waiting on its argument -- there is
/// no such name among the ones it sends in `mode_info_set`, which is where
/// [`CMDLINE_GATE_MODES`] is drawn from -- so with `f` on the wire the
/// model still reads `normal`, and a `:` typed there is the `f` target and
/// opens no command line. Reading this set is the only thing that tells the
/// two apart.
///
/// Taken from the pinned engine's `:help index.txt` (nvim 0.12.4): every
/// normal-mode entry whose form is `x{char}` (`"{register}`,
/// `'{a-zA-Z0-9}`, `@{a-z}`, `F{char}`, `T{char}`, `` `{a-zA-Z0-9} ``,
/// `f{char}`, `m{A-Za-z}`, `q{0-9a-zA-Z"}`, `r{char}`, `t{char}`, the
/// `g{char}`/`z{char}`/`[{char}`/`]{char}` command prefixes, `CTRL-W
/// {char}` and `CTRL-\`), plus `Z`, whose two commands are spelled by the
/// key after it, and the visual-mode text-object prefixes `i` and `a`
/// (`v_iw`, `v_ap` and their siblings). Operators are absent because they
/// take a motion rather than a literal and nvim does announce a mode for
/// one (`operator`), which [`CMDLINE_GATE_MODES`] already excludes.
pub const CMDLINE_LITERAL_KEYS: [&str; 21] = [
    "\"", "'", "@", "F", "T", "Z", "[", "]", "`", "a", "f", "g", "i", "m", "q", "r", "t", "z",
    "<C-w>", "<C-W>", "<C-\\>",
];

/// The `mode_change` modes a typed `:` opens a command line from.
///
/// nvim's own names, as the pinned engine sends them in `mode_info_set`
/// (`normal`, `visual`, `insert`, `replace`, `cmdline_normal`,
/// `cmdline_insert`, `cmdline_replace`, `operator`, `visual_select`,
/// `cmdline_hover`, `statusline_hover`, `statusline_drag`, `vsep_hover`,
/// `vsep_drag`, `more`, `more_lastline`, `showmatch`, `terminal`). Every
/// name outside this set either types the `:` into something (insert,
/// replace, terminal, the `cmdline_*` family) or spends it on a pending
/// operator, so none of them opens the palette.
///
/// Select mode is reported as `visual` too, and there a `:` replaces the
/// selection instead of opening anything: the withdraw on the `insert`
/// that follows is what takes the palette back off, at the cost of one
/// empty box for the length of that keystroke.
pub const CMDLINE_GATE_MODES: [&str; 3] = ["normal", "visual", "visual_select"];

/// A `:` view has sent the engine and expects a `cmdline_show` for, so the
/// palette can be on screen in the keystroke's own frame instead of after
/// the engine's silence.
///
/// Display-only, on the same terms as [`PredictedCell`]: nothing is held
/// here that the engine is not being told separately, and the `:` reaches
/// nvim on exactly the path it always did.
// Constructible by design (no `#[non_exhaustive]`): the crate that paints
// the frame a speculated `:` produces has to be able to build one to have
// anything to paint it from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdlineSpeculation {
    /// When the `:` went out, as the host measured it. Bounded by
    /// [`cmdline_backstop`].
    pub since: SpecStamp,
    /// The grid the `:` was typed on -- the one holding the cursor when it
    /// went out -- so a cursor move somewhere else is not read as the key
    /// having gone somewhere else.
    ///
    /// A cmdline plugin that could not hand the surface back draws its own
    /// box in a float and puts the cursor inside it, inside exactly the
    /// silence a guess stands in. That move is addressed to the float's own
    /// grid, and withdrawing on it would blank a *correct* guess on every
    /// `:` of such a session.
    ///
    /// The cursor's grid, which is [`GLOBAL_GRID`] whenever no visible pane
    /// owns the cursor (before the session's first `grid_cursor_goto`, or
    /// while the cursor's window is hidden): under `ext_multigrid` nvim
    /// addresses cursor moves to window grids, so a guess recorded on that
    /// fallback matches no evidence and rides [`cmdline_backstop`] instead
    /// -- a wrong guess standing longer, never a correct one blanked.
    pub grid: GridId,
}

/// Whether a `:` about to reach the engine is one view can put the palette
/// up for, read entirely from state the model already holds.
///
/// No RPC and no clock: every term here is a field `update()` has already
/// folded. The mapping fact is the one that could not be derived locally,
/// and it travels once with the claim report
/// ([`crate::msg::Msg::MappingsClaimed`]) rather than being asked for per
/// keystroke.
#[must_use]
pub fn may_speculate_cmdline(model: &Model) -> bool {
    model.palette_enabled
        && !model.colon_mapped()
        && model.focus() == crate::model::Focus::Engine
        && model.pending_chord.is_none()
        && model.engine.cmdline.is_none()
        // a wedged engine answers no `cmdline_show`, and the modal saying so
        // is the thing the user is reading
        && model.engine_busy().is_none()
        // both of these say the mode below is a reading of an editor state
        // the keys already in flight have moved on from: this `:` is the
        // argument of a command still waiting for one, or it follows a key
        // whose own `mode_change` has not come back yet
        && !model.engine.literal_pending
        && model.engine.key_unanswered.is_none()
        && CMDLINE_GATE_MODES.contains(&model.engine.mode.current.as_str())
}

/// Whether `mode` is one of nvim's command-line modes, which is what a
/// speculated `:` is waiting to be told it reached.
#[must_use]
pub fn is_cmdline_mode(mode: &str) -> bool {
    mode.starts_with("cmdline_")
}

/// Takes a speculated palette back down, marking the frame when one was up.
///
/// Every withdrawal goes through here, so an empty palette cannot be left
/// on screen with nothing to repaint it -- the one failure this
/// speculation can produce that a later redraw does not fix by itself.
pub fn withdraw_cmdline_speculation(model: &mut Model) {
    if model.engine.cmdline_speculated.take().is_none() {
        return;
    }
    model.dirty = true;
}

/// Folds one engine-bound key into the palette's speculation: a `:` that
/// passes [`may_speculate_cmdline`] puts the palette up now, and a
/// `mode_change` out of the gate's modes, a cursor move on the grid it was
/// typed on, a `cmdline_show`, a `cmdline_hide` or [`cmdline_backstop`]
/// takes it back.
///
/// Every other key is left alone while one is pending: the `cmdline_show`
/// that follows carries whatever was typed into the command line, so a key
/// arriving in the silence neither confirms the guess nor refutes it.
fn fold_cmdline_key(model: &mut Model, notation: &str, now: SpecStamp) {
    if notation == ":" && may_speculate_cmdline(model) {
        model.engine.cmdline_speculated = Some(CmdlineSpeculation {
            since: now,
            // the window the key was typed into, which is the one nvim
            // moves the cursor in if the `:` turns out to have been a
            // command's argument rather than a command line
            grid: model.engine.grids().cursor_local().0,
        });
        model.dirty = true;
    }
}

/// The host's per-pass age check on a speculated palette.
///
/// A backstop rather than the rule: a wrong guess that moves anything is
/// taken back by the evidence [`fold_cmdline_batch`] reads, so the clock
/// only has to cover the guesses nothing refutes -- a `:` a plugin read
/// with its own `getchar()`, one inside a mapping the gate cannot see, and
/// an engine that says nothing at all. [`cmdline_backstop`] is what sizes
/// that wait to the link instead of to the glyphs' own bound.
fn expire_cmdline_speculation(model: &mut Model, now: SpecStamp) {
    let bound = cmdline_backstop(model);
    if model
        .engine
        .cmdline_speculated
        .is_some_and(|open| now.age_since(open.since) >= bound)
    {
        withdraw_cmdline_speculation(model);
    }
}

/// What is left of [`cmdline_backstop`] for the speculated palette, or
/// `None` when none is up.
#[must_use]
pub fn cmdline_expiry_left(model: &Model, now: SpecStamp) -> Option<Duration> {
    let bound = cmdline_backstop(model);
    model
        .engine
        .cmdline_speculated
        .map(|open| bound.saturating_sub(now.age_since(open.since)))
}

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
    /// The grid this prediction was made against -- the cursor's grid at
    /// the moment of the keystroke. Every event that could answer or
    /// invalidate the prediction names its own grid on the wire, so this is
    /// scoped the same way: [`GLOBAL_GRID`](crate::grid::registry::GLOBAL_GRID)
    /// for a single-grid session and for a multigrid one before any window
    /// has claimed the cursor, the window's own grid id once one has.
    pub grid: GridId,
    /// The grid row the glyph is expected on, local to `grid` -- the same
    /// space `grid_line`'s own `row` reports for it, not a screen position.
    /// A consumer that paints this cell (rather than matching it against a
    /// redraw) translates through the pane's own origin first.
    pub row: u16,
    /// The grid column the glyph is expected at, local to `grid` and
    /// possibly past its live last column (see this type's own doc).
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
    /// `grid` is the grid the cursor is in and `cursor` is its position
    /// local to that grid (`GridRegistry::cursor_local`'s own shape), so a
    /// redraw naming a different grid can never be read as answering this
    /// prediction. `now` is elapsed time from whatever fixed origin the
    /// host stamps every call with; only differences between stamps are
    /// read here, never a stamp's absolute value.
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
        grid: GridId,
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
            .filter(|cell| cell.grid == grid && cell.row == row && cell.col >= col)
            .map(|cell| cell.col)
            .max()
            .map_or(col, |taken| taken.saturating_add(1));
        let cell = PredictedCell {
            grid,
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
    ///   buffer lines at the same screen rows, so every prediction made
    ///   against that window's own grid is now over content it was never
    ///   made for, and all of them are retired -- a bystander *window*
    ///   grid's predictions survive, under `ext_multigrid`, where
    ///   `win_viewport` names a real, paintable grid the same way every
    ///   other redraw event does. The global grid is not a bystander: a
    ///   `win_viewport` names a per-window id even without `ext_multigrid`,
    ///   where that id is never `grid_resize`d or `grid_line`d and the shift
    ///   it reports lands on grid 1 regardless (see [`GLOBAL_GRID`]'s own
    ///   retirement below), so every global-grid prediction retires on any
    ///   window's shift whether or not that shift was its own. This is the
    ///   relocation `grid_scroll` does not cover: nvim repaints a jumped
    ///   viewport line by line, and those lines need not reach the
    ///   predicted columns, so a per-cell reading sees nothing and a glyph
    ///   would stand over moved text until [`Self::expire_stale`]. Typing
    ///   never triggers it -- a burst that does not scroll the window keeps
    ///   `topline` exactly where it was, and one that does scroll the
    ///   window has moved its own predictions and is right to lose them.
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
        let mut shifted_grids: Vec<GridId> = Vec::new();
        for event in redraw {
            match event {
                UiEvent::ModeChange { .. } => mode_changed = true,
                UiEvent::WinViewport {
                    win, grid, topline, ..
                } => {
                    // recording the viewport is what reading it costs, so it
                    // happens in the arm rather than in a guard that would
                    // read as a pure test of the variant
                    let moved = self.note_viewport(*win, *topline);
                    if moved {
                        shifted_grids.push(GridId(*grid));
                        // nvim names a per-window grid here even without
                        // `ext_multigrid`, where that id is never
                        // `grid_resize`d or `grid_line`d
                        // (docs/multigrid-wire-capture.md's `win_viewport`
                        // section calls this out by name as "the trap"), so
                        // the shift it reports still lands on grid 1, the
                        // only canvas single-grid ever paints -- grid 1's own
                        // predictions must retire on it whatever phantom id
                        // this event names. Under `ext_multigrid` this is a
                        // no-op past startup: `fold_keystroke` never tags a
                        // prediction with the global grid once a real window
                        // has claimed the cursor.
                        shifted_grids.push(GLOBAL_GRID);
                    }
                }
                _ => {}
            }
        }
        if mode_changed {
            self.reset_epoch();
            return;
        }
        // the epoch is deliberately left alone here, exactly as
        // `expire_stale` leaves it: a shift says the cells a shifted grid's
        // predictions named are showing something else, not that the mode
        // ended. Scoped to the grids that actually shifted -- `win_viewport`
        // names its own grid, and every `PredictedCell` carries the grid it
        // was made against, so a bystander window's predictions survive a
        // mover's scroll under `ext_multigrid` exactly as they always did
        // under single-grid, where every prediction shares the one grid
        // that ever shifts.
        let epoch = self.epoch;
        self.pending.retain(|cell| {
            cell.epoch == epoch && !shifted_grids.contains(&cell.grid) && !answered_by(redraw, cell)
        });
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
    let same_grid = |grid: &u64| GridId(*grid) == cell.grid;
    redraw.iter().any(|ev| match ev {
        // every event names the grid it is about, and a prediction is only
        // ever about the one it was made against -- a coincidence of row
        // and column on some other window's grid must never retire it
        UiEvent::GridLine {
            grid,
            row,
            col_start,
            cells,
        } => {
            same_grid(grid)
                && *row == u64::from(cell.row)
                && covers_column(*col_start, cells, cell.col)
        }
        // content relocated, dropped or reshaped without the cells it moved
        // being re-sent: the coordinates a prediction holds no longer name
        // the place its glyph was predicted for, and no later batch is
        // obliged to say so cell by cell
        UiEvent::GridScroll { grid, .. }
        | UiEvent::GridClear { grid }
        | UiEvent::GridResize { grid, .. } => same_grid(grid),
        // the same answer for the same reason, one level up: a window that
        // moved, hid, closed or died carries its grid's cells to a different
        // place on screen (or off it) without resending one of them
        UiEvent::GridDestroy { grid }
        | UiEvent::WinPos { grid, .. }
        | UiEvent::WinFloatPos { grid, .. }
        | UiEvent::WinExternalPos { grid, .. }
        | UiEvent::WinHide { grid }
        | UiEvent::WinClose { grid }
        | UiEvent::MsgSetPos { grid, .. } => same_grid(grid),
        // a viewport that moved is read by `reconcile` itself, one reading
        // earlier and against the last one this window reported -- the event
        // on its own says nothing, since nvim sends it for every cursor move
        // inside an unmoved window
        UiEvent::WinViewport { .. } => false,
        // margins narrow where the viewport sits inside a grid whose cells
        // nvim resends as `grid_line` either way
        UiEvent::WinViewportMargins { .. } => false,
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
        | UiEvent::UiSend { .. }
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
/// A typed `:` is folded here for a second reason as well: it is the one
/// key whose answer the palette can be drawn ahead of
/// ([`may_speculate_cmdline`]), and this is where a key going to the engine
/// is seen with a stamp on it.
///
/// `now` is a stamp the host took from its own fixed origin, since nothing
/// in this module reads a clock.
pub fn fold_engine_call(model: &mut Model, call: &RpcCall, now: SpecStamp) {
    match call {
        RpcCall::Input { notation } => {
            // a key nvim answered with nothing at all -- an unmapped
            // function key produces no redraw -- is not a key in flight, and
            // every forwarded key re-stamps this flag anyway, so the real
            // cost of leaving it standing is one `:` left unaccelerated: the
            // key nvim never answers, not the rest of the session
            if model
                .engine
                .key_unanswered
                .is_some_and(|sent| now.age_since(sent) >= cmdline_backstop(model))
            {
                model.engine.key_unanswered = None;
            }
            fold_cmdline_key(model, notation, now);
            // after the fold above, which is the one reading of these two
            // that describes the editor this key is arriving at. The stamp
            // is what the batch answering this key subtracts to read the
            // link
            model.engine.key_unanswered = Some(now);
            // recomputed from this key alone, so a key that takes no
            // argument clears whatever the key before it was owed: an `f`
            // whose target is not on the line moves neither the mode nor
            // the cursor, and reading the flag as sticky would leave the
            // gate shut for the rest of the session
            model.engine.literal_pending = CMDLINE_LITERAL_KEYS.contains(&notation.as_str());
            fold_keystroke(model, notation, now);
        }
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
pub fn fold_redraw(model: &mut Model, redraw: &[UiEvent], now: SpecStamp) -> Vec<Effect> {
    let effects = fold_cmdline_batch(model, redraw, now);
    let before = model.speculate.pending().len();
    model.speculate.reconcile(redraw);
    mark_retirement(model, before);
    effects
}

/// What one redraw batch says about the keys view has forwarded and about a
/// palette it is guessing at.
///
/// A batch answers the key the gate was waiting for only when it carries an
/// event nvim sends *because* input was read -- a `mode_change`, a cursor
/// move, a `cmdline_show` or a `grid_line`. Every other batch is somebody
/// else's flush: a plugin on a timer, a spinner, an animation, a status
/// line refresh, an LSP float. Reading the arrival alone as the answer fails in
/// both directions at once on a session that has one of those, which is
/// most login-shaped configs. The gate opens on a key nvim has not read
/// yet, and worse, the round trip below times the timer's own cadence
/// instead of the link: on a genuinely slow link *every* reading comes out
/// short, [`cmdline_backstop`] never lifts off its floor, and every correct
/// guess is blanked shortly before its `cmdline_show` arrives -- the
/// draw-blank-redraw flicker the bound exists to prevent.
///
/// Two of those events say more than that. A `mode_change`
/// or a cursor move is the command that was holding the next keystroke
/// finishing with it, so the argument the next key would have been is no
/// longer owed. And a cursor move *on the grid the `:` was typed on*,
/// while the palette stands on a guess, says the `:` went somewhere other
/// than the command line -- it was an `f` target, a mark, a register --
/// which is the evidence the guess is withdrawn on, rather than a clock:
/// on the config the recorder ran, every silence between the key and its
/// `cmdline_show` carried `msg_showcmd`, `msg_ruler`, `win_viewport` and
/// highlight events and no cursor move at all. A batch that also carries
/// the `cmdline_show` is the guess being answered, not refuted.
///
/// The grid is what parts the key having gone elsewhere from somebody else
/// drawing. A cmdline plugin that never received the takeover's ask to
/// stand down opens a float on `CmdlineEnter` -- which runs before
/// `cmdline_show` -- and puts the cursor inside it, in exactly this
/// silence. That move is addressed to the float's own grid, and reading it
/// as a refusal would blank a correct guess on every `:` of such a session,
/// which is the flicker the evidence exists to avoid. A `:` that really did
/// go elsewhere without moving this grid's cursor is left to
/// [`cmdline_backstop`].
///
/// The answering batch is this module's one reading of the link as well: the
/// key's own stamp is what `key_unanswered` carries, and the difference is
/// what sizes that backstop.
fn fold_cmdline_batch(model: &mut Model, redraw: &[UiEvent], now: SpecStamp) -> Vec<Effect> {
    let guessed_on = model.engine.cmdline_speculated.map(|open| open.grid);
    let cursor_grid = model.engine.grids().cursor_local().0;
    let mut moved_cursor_there = false;
    let mut settled = false;
    let mut shows_cmdline = false;
    let mut answers_input = false;
    for ev in redraw {
        match ev {
            UiEvent::GridCursorGoto { grid, .. } => {
                // a command waiting on its argument is finished by a cursor
                // move wherever nvim addressed one
                settled = true;
                answers_input = true;
                moved_cursor_there |= guessed_on == Some(GridId(*grid));
            }
            UiEvent::ModeChange { .. } => {
                settled = true;
                answers_input = true;
            }
            UiEvent::CmdlineShow { .. } => {
                shows_cmdline = true;
                answers_input = true;
            }
            // text changing under the cursor's own grid: the key was typed
            // and nvim has drawn what it did. A float redrawing itself on
            // some other grid is a plugin's own timer and answers nothing
            UiEvent::GridLine { grid, .. } if GridId(*grid) == cursor_grid => {
                answers_input = true;
            }
            // nvim's count/pending-operator echo, and its mapping-timeout
            // clear -- never `settled`, so a literal-taking key's argument
            // is still owed and the `f`/`r`/`"`/`q`/`@` rows stay gated
            UiEvent::MsgShowcmd { .. } => answers_input = true,
            _ => {}
        }
    }
    if answers_input {
        if let Some(sent) = model.engine.key_unanswered.take() {
            // the one write site, so the read in `cmdline_backstop` is the
            // only place the window's shape is known
            model.engine.key_round_trips[model.engine.key_round_trips_at] =
                Some(now.age_since(sent));
            model.engine.key_round_trips_at =
                (model.engine.key_round_trips_at + 1) % KEY_ROUND_TRIPS;
        }
    }
    // the argument character that is itself a literal-taking key is what
    // this answers: `fa` re-arms the gate on the `a`, and the batch the
    // finished `f` produced is what says that `a` was an argument and not a
    // command waiting for one of its own
    if settled {
        model.engine.literal_pending = false;
    }
    if moved_cursor_there && !shows_cmdline {
        withdraw_cmdline_speculation(model);
    }
    Vec::new()
}

/// The host's per-pass age check on what speculation is still holding.
///
/// Belongs at a call site reached whether or not a redraw arrived: a redraw
/// that never comes is the condition [`SPECULATION_MAX_AGE`] and
/// [`cmdline_backstop`] exist for, for the predicted glyphs and for the
/// speculated palette alike.
pub fn fold_expiry(model: &mut Model, now: SpecStamp) {
    expire_cmdline_speculation(model, now);
    // the pending list is read before anything else so a steady-state pass
    // costs one null check and one length compare: expiring an empty list is
    // a no-op, and a session outside a typing burst takes that pass forever
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
    let registry = model.engine.grids();
    // `cursor_local` falls back to grid 1 when the pane holding the cursor
    // is hidden or gone, which is the right answer for painting a caret and
    // the wrong one for placing a glyph: under `ext_multigrid` grid 1's own
    // cursor field is whatever it was last (never) set to, so a prediction
    // made there lands at (0, 0) rather than under the character typed.
    // `PredictedCell`'s contract is to drop rather than misplace, so this
    // keystroke goes unpredicted and the engine's own redraw shows it.
    if registry
        .cursor_grid()
        .is_some_and(|grid| grid != GLOBAL_GRID && registry.pane_origin(grid).is_none())
    {
        return;
    }
    // never the global grid's own cursor field otherwise: under
    // `ext_multigrid` `grid_cursor_goto` names window grids, not grid 1
    let (grid, row, col) = registry.cursor_local();
    let mode = model.engine.mode.current.as_str();
    let before = model.speculate.pending().len();
    // the refusal path is why the answer is read off the pending list rather
    // than off this `Option`: a character `predict` declines discards
    // everything pending inside `predict` itself, and the caller sees only
    // the `None` it shares with a mode that was never predicting at all
    let _ = model.speculate.predict(mode, grid, key, (row, col), now);
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

    /// One redraw batch as the host folds it. The effects are dropped:
    /// nothing is absorbed in these cases, so a withdrawal here answers
    /// with none (`update::surface_conflict`'s own tests drive that half).
    fn folded(model: &mut Model, redraw: &[UiEvent], now: SpecStamp) {
        assert!(
            fold_redraw(model, redraw, now).is_empty(),
            "no float is absorbed here, so no batch owes a show"
        );
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

    /// [`grid_line`], naming `grid` instead of always 1 -- the shape a
    /// multigrid session sends, where a window's own text arrives addressed
    /// to that window's grid.
    fn grid_line_on(grid: u64, row: u64, col_start: u64, text: &str) -> UiEvent {
        match grid_line(row, col_start, text) {
            UiEvent::GridLine {
                row,
                col_start,
                cells,
                ..
            } => UiEvent::GridLine {
                grid,
                row,
                col_start,
                cells,
            },
            other => other,
        }
    }

    /// Every mode name the pinned engine (v0.12.4) sends in
    /// `mode_info_set`, read off a live attach rather than off memory. The
    /// gate is a subset of this, and the rest of it is what the table test
    /// below refuses one by one -- so a mode nvim adds later is a name this
    /// list does not carry, and the day it matters the list is what has to
    /// be re-read.
    const PINNED_ENGINE_MODES: [&str; 18] = [
        "normal",
        "visual",
        "insert",
        "replace",
        "cmdline_normal",
        "cmdline_insert",
        "cmdline_replace",
        "operator",
        "visual_select",
        "cmdline_hover",
        "statusline_hover",
        "statusline_drag",
        "vsep_hover",
        "vsep_drag",
        "more",
        "more_lastline",
        "showmatch",
        "terminal",
    ];

    /// A session a typed `:` may open the palette from: the feature on, the
    /// engine holding the keyboard, nothing mapped over `:`.
    fn colon_model() -> Model {
        let mut model = Model::with_term_size(80, 24);
        model.palette_enabled = true;
        model.engine.mode.current = "normal".to_string();
        model.dirty = false;
        model
    }

    /// One ordinary key, and the batch that answers it `answered_at`.
    fn answered_key(model: &mut Model, sent: SpecStamp, answered_at: SpecStamp) {
        fold_engine_call(
            model,
            &RpcCall::Input {
                notation: "j".to_string(),
            },
            sent,
        );
        let effects = fold_redraw(
            model,
            &[
                UiEvent::GridCursorGoto {
                    grid: 1,
                    row: 0,
                    col: 0,
                },
                UiEvent::Flush,
            ],
            answered_at,
        );
        assert!(effects.is_empty(), "no float is absorbed here");
    }

    /// One `:` as the loop hands it to the fold.
    fn typed_colon(model: &mut Model, now: SpecStamp) {
        fold_engine_call(
            model,
            &RpcCall::Input {
                notation: ":".to_string(),
            },
            now,
        );
    }

    /// The whole gate, mode by mode, over every name the pinned engine
    /// sends: the three the palette opens from and the fifteen it does not.
    #[test]
    fn the_palette_opens_only_from_the_modes_a_colon_reaches_the_cmdline_from() {
        for mode in PINNED_ENGINE_MODES {
            let mut model = colon_model();
            model.engine.mode.current = mode.to_string();

            typed_colon(&mut model, stamp(0));

            assert_eq!(
                model.engine.cmdline_speculated.is_some(),
                CMDLINE_GATE_MODES.contains(&mode),
                "mode {mode} disagreed with the gate"
            );
        }
    }

    /// The four gate terms that are not the mode. Each one alone closes it,
    /// so the table is read as "this session is otherwise open, except for".
    #[test]
    fn a_mapped_colon_a_focused_overlay_a_pending_chord_or_a_disabled_palette_close_the_gate() {
        /// One way to close the gate, and the words for what it stands for.
        type Closure = (&'static str, fn(&mut Model));
        let cases: [Closure; 4] = [
            ("the user's config maps `:`", |m| {
                m.record_colon_mapped(true)
            }),
            ("an overlay owns the keyboard", |m| {
                m.push_overlay(
                    crate::native::geometry::OverlayBox::new(50, 50),
                    crate::model::OverlayKind::Ai,
                );
                m.ai_panel.focused = true;
            }),
            ("a native chord is half typed", |m| {
                m.pending_chord = Some("<leader>".to_string());
            }),
            ("the palette feature is off", |m| m.palette_enabled = false),
        ];
        for (why, close) in cases {
            let mut model = colon_model();
            close(&mut model);

            typed_colon(&mut model, stamp(0));

            assert!(
                model.engine.cmdline_speculated.is_none(),
                "the palette was speculated although {why}"
            );
        }
    }

    /// The gate against the keys already on the wire, which is what the
    /// mode beside it cannot see: nvim announces no mode for a command
    /// waiting on its argument, and the mode it last announced describes
    /// the editor as of the last key it answered.
    ///
    /// Each row is the keys typed, in order, with `showcmd` standing for the
    /// batch nvim really flushes back after a count or a pending command
    /// (`msg_showcmd` and nothing else, read off the pinned engine under
    /// view's own attach) and `cursor` for the batch a finished motion
    /// produces. Both answer the key that produced them, but only the
    /// second settles a literal-taking key's own argument, which is why a
    /// `:` after `f`/`r`/`"`/`q`/`@` stays gated while a `:` after a count
    /// does not.
    #[test]
    fn a_colon_is_speculated_only_when_no_earlier_key_is_still_holding_it() {
        /// One typed sequence, and whether the `:` ending it opens the
        /// palette.
        type Row = (&'static [&'static str], bool, &'static str);
        let rows: [Row; 9] = [
            (&["f", "showcmd", ":"], false, "the `f` target"),
            (&["r", "showcmd", ":"], false, "the character `r` writes"),
            (&["\"", "showcmd", ":"], false, "the register `\"` names"),
            (
                &["q", "showcmd", ":"],
                false,
                "the register `q` records into",
            ),
            (&["@", "showcmd", ":"], false, "the register `@` replays"),
            (
                &["A", ":"],
                false,
                "typed into the insert `A` opened, whose mode_change is still on the wire",
            ),
            (
                &["2", "showcmd", ":"],
                true,
                "a count whose showcmd nvim sends because the count was read",
            ),
            (&["j", "cursor", ":"], true, "a motion nvim has answered"),
            (
                &["f", "showcmd", "a", "cursor", ":"],
                true,
                "the `f` finished on a target that is itself a literal-taking key, cursor moved",
            ),
        ];
        for (keys, opens, what) in rows {
            let mut model = colon_model();
            for key in keys {
                match *key {
                    "showcmd" => folded(
                        &mut model,
                        &[
                            UiEvent::MsgShowcmd {
                                content: vec![(0, "2".to_string())],
                            },
                            UiEvent::Flush,
                        ],
                        stamp(0),
                    ),
                    "cursor" => folded(
                        &mut model,
                        &[
                            UiEvent::GridCursorGoto {
                                grid: 1,
                                row: 3,
                                col: 9,
                            },
                            UiEvent::Flush,
                        ],
                        stamp(0),
                    ),
                    ":" => typed_colon(&mut model, stamp(0)),
                    key => fold_engine_call(
                        &mut model,
                        &RpcCall::Input {
                            notation: key.to_string(),
                        },
                        stamp(0),
                    ),
                }
            }

            assert_eq!(
                model.engine.cmdline_speculated.is_some(),
                opens,
                "{keys:?}: the `:` is {what}"
            );
        }
    }

    /// The withdrawal the clock is not: a cursor move on the grid says the
    /// `:` was spent on something other than the command line, whatever is
    /// left of the age bound.
    #[test]
    fn a_cursor_move_with_no_cmdline_show_takes_the_guess_back_and_marks_the_frame() {
        let mut model = colon_model();
        typed_colon(&mut model, stamp(0));
        model.dirty = false;

        folded(
            &mut model,
            &[
                UiEvent::GridCursorGoto {
                    grid: 1,
                    row: 3,
                    col: 9,
                },
                UiEvent::Flush,
            ],
            stamp(0),
        );

        assert!(model.engine.cmdline_speculated.is_none());
        assert!(model.dirty, "the withdrawal has to be painted");
    }

    /// The same batch carrying the `cmdline_show` is the guess being
    /// answered: nvim moves the cursor into the command line it is about to
    /// announce, and reading that as a refusal would blank a correct guess.
    #[test]
    fn a_cursor_move_in_the_batch_that_answers_the_guess_leaves_it_standing() {
        let mut model = colon_model();
        typed_colon(&mut model, stamp(0));

        folded(
            &mut model,
            &[
                UiEvent::GridCursorGoto {
                    grid: 1,
                    row: 3,
                    col: 9,
                },
                UiEvent::CmdlineShow {
                    content: vec![(0, String::new())],
                    pos: 0,
                    firstc: ":".to_string(),
                    prompt: String::new(),
                    indent: 0,
                    level: 1,
                },
                UiEvent::Flush,
            ],
            stamp(0),
        );

        assert!(model.engine.cmdline_speculated.is_some());
    }

    /// The open itself: the palette goes up on the keystroke's own fold, and
    /// the frame is marked so it is painted rather than waited for.
    #[test]
    fn a_colon_the_gate_admits_puts_the_palette_up_on_its_own_frame() {
        let mut model = colon_model();

        typed_colon(&mut model, stamp(40));

        assert_eq!(
            model.engine.cmdline_speculated.map(|open| open.since),
            Some(stamp(40))
        );
        assert!(
            model.dirty,
            "a palette nobody paints is a palette nobody sees"
        );
    }

    /// A key typed inside the silence neither confirms the guess nor takes
    /// it back: the `cmdline_show` that follows carries the typed text.
    #[test]
    fn a_key_typed_while_the_palette_is_speculated_leaves_it_standing() {
        let mut model = colon_model();
        typed_colon(&mut model, stamp(0));

        fold_engine_call(
            &mut model,
            &RpcCall::Input {
                notation: "q".to_string(),
            },
            stamp(10),
        );

        assert!(model.engine.cmdline_speculated.is_some());
    }

    /// The backstop, on the pass the loop takes whether or not the engine
    /// said anything: an engine that never answers sends no evidence
    /// either, so it would leave an empty palette on screen with nothing
    /// to take it off.
    #[test]
    fn a_speculated_palette_past_its_bound_is_withdrawn_and_the_frame_marked() {
        let mut model = colon_model();
        let bound = cmdline_backstop(&model);
        typed_colon(&mut model, stamp(0));
        model.dirty = false;

        fold_expiry(&mut model, SpecStamp::new(bound - Duration::from_millis(1)));
        assert!(
            model.engine.cmdline_speculated.is_some(),
            "inside the bound the palette stands"
        );
        assert!(!model.dirty);

        fold_expiry(&mut model, SpecStamp::new(bound));

        assert!(model.engine.cmdline_speculated.is_none());
        assert!(model.dirty, "the withdrawal has to be painted");
    }

    /// The bound in force, link by link. A local session is answered far
    /// inside the floor, so a guess nothing refutes there comes off at the
    /// floor rather than standing for the whole of the glyphs' own bound;
    /// a slow link keeps a bound its own correct answers fit inside; and
    /// no link at all pays the glyphs' bound and no more.
    #[test]
    fn the_backstop_is_the_floor_or_three_round_trips_and_never_past_the_age_bound() {
        /// A round trip the session observed, and the bound it buys.
        type Tier = (Option<u64>, u64, &'static str);
        let tiers: [Tier; 4] = [
            (None, 250, "a session that has observed no round trip yet"),
            (Some(1), 250, "a local link, answered well inside the floor"),
            (Some(300), 900, "the slowest tier the rtt scenario ships"),
            (Some(500), 1000, "a link slow enough to reach the age bound"),
        ];
        for (observed, bound_ms, what) in tiers {
            let mut model = colon_model();
            model.engine.key_round_trips[0] = observed.map(Duration::from_millis);

            assert_eq!(
                cmdline_backstop(&model),
                Duration::from_millis(bound_ms),
                "{what}"
            );

            typed_colon(&mut model, stamp(0));
            fold_expiry(&mut model, stamp(bound_ms - 1));
            assert!(
                model.engine.cmdline_speculated.is_some(),
                "{what}: the guess came off inside its own bound"
            );
            fold_expiry(&mut model, stamp(bound_ms));
            assert!(
                model.engine.cmdline_speculated.is_none(),
                "{what}: the guess outlasted its own bound"
            );
        }
    }

    /// The link reading the bound is built from: the key carries the stamp
    /// it went out with, and the batch that answers it is the other end.
    ///
    /// The longest of the recent window is what is read, because a single
    /// reading can only be short -- but the window is what keeps one slow
    /// answer from pinning the bound: nvim being busy when a key lands (a
    /// lazy-loaded plugin, a first LSP attach) is not the link, and a
    /// session-lifetime maximum would hold that reading for as long as the
    /// session ran.
    #[test]
    fn one_slow_answer_stops_sizing_the_bound_once_the_window_has_moved_past_it() {
        let mut model = colon_model();

        answered_key(&mut model, stamp(0), stamp(334));
        assert_eq!(
            cmdline_backstop(&model),
            SPECULATION_MAX_AGE,
            "the one hiccup the session has seen is all it has to go on"
        );

        for trip in 0..KEY_ROUND_TRIPS as u64 {
            let sent = 1000 + trip * 10;
            answered_key(&mut model, stamp(sent), stamp(sent + 1));
        }

        assert_eq!(
            cmdline_backstop(&model),
            CMDLINE_SPECULATION_BACKSTOP_MIN,
            "eight short readings after the hiccup put the bound back on its floor"
        );
    }

    /// A batch nvim flushed for something other than the key is not that
    /// key's answer, in either direction. It leaves the gate shut, so a `:`
    /// arriving before nvim has read the key ahead of it is unaccelerated
    /// rather than guessed at -- and it records no round trip, so a config
    /// with a plugin flushing on a timer does not read the timer's own
    /// cadence as the link and shrink the bound to its floor.
    #[test]
    fn only_a_batch_answering_input_clears_the_gate_and_times_the_link() {
        let mut model = colon_model();
        fold_engine_call(&mut model, &input("j"), stamp(0));

        folded(
            &mut model,
            &[
                UiEvent::WinViewport {
                    grid: 2,
                    win: WinHandle(1),
                    topline: 0,
                    botline: 20,
                    curline: 0,
                    curcol: 0,
                },
                UiEvent::Flush,
            ],
            stamp(100),
        );

        assert_eq!(
            model.engine.key_unanswered,
            Some(stamp(0)),
            "a timer's own flush is not the answer to a key"
        );
        assert_eq!(
            model.engine.key_round_trips, [None; KEY_ROUND_TRIPS],
            "and it is not a reading of the link either"
        );

        folded(
            &mut model,
            &[
                UiEvent::ModeChange {
                    mode: "normal".to_string(),
                    mode_idx: 0,
                },
                UiEvent::Flush,
            ],
            stamp(300),
        );

        assert_eq!(model.engine.key_unanswered, None);
        assert_eq!(
            model.engine.key_round_trips.iter().flatten().max(),
            Some(&Duration::from_millis(300)),
            "the answering batch times the whole link and not the timer's leg of it"
        );
    }

    /// Every event nvim sends because a key was read, one by one, against
    /// the ones it sends on its own account.
    #[test]
    fn the_events_that_answer_a_key_are_the_ones_a_key_produces() {
        for (event, answers) in [
            (
                UiEvent::ModeChange {
                    mode: "normal".to_string(),
                    mode_idx: 0,
                },
                true,
            ),
            (
                UiEvent::GridCursorGoto {
                    grid: 1,
                    row: 0,
                    col: 1,
                },
                true,
            ),
            (
                UiEvent::CmdlineShow {
                    content: Vec::new(),
                    pos: 0,
                    firstc: ":".to_string(),
                    prompt: String::new(),
                    indent: 0,
                    level: 1,
                },
                true,
            ),
            (grid_line(0, 0, "x"), true),
            (
                UiEvent::MsgShowcmd {
                    content: vec![(0, "2".to_string())],
                },
                true,
            ),
            (UiEvent::Flush, false),
            (
                UiEvent::WinViewport {
                    grid: 2,
                    win: WinHandle(1),
                    topline: 0,
                    botline: 20,
                    curline: 0,
                    curcol: 0,
                },
                false,
            ),
        ] {
            let mut model = colon_model();
            fold_engine_call(&mut model, &input("j"), stamp(0));

            let _ = fold_redraw(&mut model, &[event.clone(), UiEvent::Flush], stamp(50));

            assert_eq!(
                model.engine.key_unanswered.is_none(),
                answers,
                "{event:?} disagreed with the answering-event rule"
            );
            assert_eq!(
                model.engine.key_round_trips.iter().flatten().count() == 1,
                answers,
                "{event:?} disagreed about whether it times the link"
            );
        }
    }

    /// A float redrawing its own grid is a plugin's own flush and answers
    /// nothing: reading any grid's `grid_line` as an answer clears the gate
    /// on a key nvim has not read and records a fraction of the link as the
    /// round trip, on exactly the login-shaped configs the gate exists for.
    #[test]
    fn a_floats_own_grid_line_neither_clears_the_gate_nor_records_a_trip() {
        let mut model = colon_model();
        fold_engine_call(&mut model, &input("j"), stamp(0));

        let _ = fold_redraw(
            &mut model,
            &[grid_line_on(5, 0, 0, "x"), UiEvent::Flush],
            stamp(50),
        );

        assert!(
            model.engine.key_unanswered.is_some(),
            "a redraw on some other grid is not this key's answer"
        );
        assert_eq!(
            model.engine.key_round_trips.iter().flatten().count(),
            0,
            "and it must not be timed as one"
        );
    }

    /// The gate reopens for a key nvim answered with nothing. An unmapped
    /// function key produces no redraw at all, and every forwarded key
    /// re-stamps this flag anyway, so the real cost of leaving it standing
    /// is one `:` left unaccelerated -- the key nvim never answers, not the
    /// rest of the session.
    #[test]
    fn a_key_the_engine_never_answered_stops_holding_the_gate_shut() {
        let mut model = colon_model();
        fold_engine_call(&mut model, &input("<F13>"), stamp(0));
        assert!(model.engine.key_unanswered.is_some());

        let bound = cmdline_backstop(&model).as_millis() as u64;
        typed_colon(&mut model, stamp(bound));

        assert!(
            model.engine.cmdline_speculated.is_some(),
            "a key older than the link's own backstop is no key in flight"
        );
    }

    /// A batch with no key outstanding says nothing about the link: the
    /// gap back to a key already answered is a user's own thinking time.
    #[test]
    fn a_batch_with_no_key_outstanding_records_no_round_trip() {
        let mut model = colon_model();

        folded(
            &mut model,
            &[
                UiEvent::GridCursorGoto {
                    grid: 1,
                    row: 0,
                    col: 0,
                },
                UiEvent::Flush,
            ],
            stamp(900),
        );

        assert_eq!(model.engine.key_round_trips, [None; KEY_ROUND_TRIPS]);
    }

    /// The evidence is the grid the `:` was typed on. A cmdline plugin
    /// that never received the takeover's ask opens its own box in a float
    /// and puts the cursor in it during exactly this silence, and reading
    /// that as a refusal would blank a correct guess on every `:`.
    #[test]
    fn a_cursor_move_on_another_grid_leaves_the_guess_standing() {
        let mut model = colon_model();
        typed_colon(&mut model, stamp(0));
        assert_eq!(
            model.engine.cmdline_speculated.map(|open| open.grid),
            Some(GLOBAL_GRID),
            "the `:` was typed on the grid holding the cursor"
        );
        model.dirty = false;

        folded(
            &mut model,
            &[
                UiEvent::GridCursorGoto {
                    grid: 9,
                    row: 1,
                    col: 4,
                },
                UiEvent::Flush,
            ],
            stamp(1),
        );

        assert!(
            model.engine.cmdline_speculated.is_some(),
            "a cursor move into somebody else's float says nothing about where the `:` went"
        );
        assert!(!model.dirty);
    }

    #[test]
    fn an_insert_mode_plain_character_is_predicted_at_the_cursor() {
        let mut state = SpeculateState::default();
        let predicted = state.predict("insert", GLOBAL_GRID, 'a', (3, 7), stamp(120));
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
        assert!(state
            .predict("insert", GLOBAL_GRID, 'a', (0, 0), stamp(0))
            .is_some());

        assert_eq!(
            state.predict("insert", GLOBAL_GRID, '\u{8}', (0, 1), stamp(10)),
            None
        );
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
            assert!(state
                .predict("insert", GLOBAL_GRID, key, (2, col), stamp(0))
                .is_some());
        }
        let stale = state.epoch();
        assert_eq!(state.pending().len(), 5);

        assert_eq!(
            state.predict("normal", GLOBAL_GRID, 'j', (2, 5), stamp(20)),
            None
        );
        assert!(state.pending().is_empty());

        let cell = state
            .predict("insert", GLOBAL_GRID, 'x', (2, 5), stamp(30))
            .expect("insert mode resumes predicting after the epoch turns over");
        assert!(cell.epoch > stale);
        assert_eq!(state.pending(), &[cell]);
    }

    #[test]
    fn a_key_typed_outside_insert_mode_is_never_predicted() {
        let mut state = SpeculateState::default();
        for mode in ["normal", "visual", "replace", "cmdline_normal", "terminal"] {
            assert_eq!(
                state.predict(mode, GLOBAL_GRID, 'a', (0, 0), stamp(0)),
                None,
                "{mode}"
            );
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
            assert_eq!(
                state.predict("insert", GLOBAL_GRID, key, (0, 0), stamp(0)),
                None
            );
            assert!(state.pending().is_empty());
        }
    }

    /// The case speculation exists for: keystrokes typed faster than the
    /// link answers, so every one of them sees the same stale cursor.
    #[test]
    fn a_burst_typed_ahead_of_the_engine_cursor_lands_on_consecutive_cells() {
        let mut state = SpeculateState::default();
        for key in ['a', 'b', 'c'] {
            assert!(state
                .predict("insert", GLOBAL_GRID, key, (4, 9), stamp(0))
                .is_some());
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
            .predict("insert", GLOBAL_GRID, 'a', (1, 1), stamp(0))
            .expect("plain insert-mode character");
        let recent = state
            .predict("insert", GLOBAL_GRID, 'b', (1, 1), stamp(900))
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
        assert!(state
            .predict("insert", GLOBAL_GRID, 'a', (0, 0), stamp(0))
            .is_some());

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
            .predict("insert", GLOBAL_GRID, 'a', (0, 0), stamp(5_000))
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
        assert!(state
            .predict("insert", GLOBAL_GRID, 'a', (1, 1), stamp(0))
            .is_some());
        let survivor = state
            .predict("insert", GLOBAL_GRID, 'b', (1, 1), stamp(900))
            .expect("plain insert-mode character");

        state.expire_stale(stamp(1000));
        assert_eq!(state.pending(), &[survivor]);

        let next = state
            .predict("insert", GLOBAL_GRID, 'c', (1, 1), stamp(1000))
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
        assert!(state
            .predict("insert", GLOBAL_GRID, 'a', (0, 0), stamp(0))
            .is_some());

        state.reset_epoch();

        assert_eq!(state.epoch(), Epoch(u64::MAX));
        assert!(state.pending().is_empty());
    }

    /// The ordinary case: the engine catches up and says the predicted glyph
    /// is really there, so there is nothing left to paint ahead of it.
    #[test]
    fn a_redraw_that_confirms_a_prediction_retires_it() {
        let mut state = SpeculateState::default();
        assert!(state
            .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
            .is_some());

        state.reconcile(&[grid_line(2, 4, "a"), UiEvent::Flush]);

        assert!(state.pending().is_empty());
    }

    /// A wrong guess costs a corrected cell and nothing else: the
    /// authoritative redraw wins, exactly as it would have with no
    /// speculation at all, and no glyph is left painted over it.
    #[test]
    fn a_redraw_that_contradicts_a_prediction_retires_it_just_the_same() {
        let mut state = SpeculateState::default();
        assert!(state
            .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
            .is_some());

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
            grid: GLOBAL_GRID,
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
            .predict("insert", GLOBAL_GRID, 'a', (2, 40), stamp(0))
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
        assert!(state
            .predict("insert", GLOBAL_GRID, 'a', (0, 6), stamp(0))
            .is_some());

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
            assert!(state
                .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
                .is_some());

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
        assert!(state
            .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
            .is_some());
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
            .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
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
        viewport_on(1, win, topline)
    }

    /// [`viewport`], naming `grid` instead of always 1 -- the shape a real
    /// single-grid nvim actually sends, per
    /// `docs/multigrid-wire-capture.md`'s `win_viewport` section: even
    /// without `ext_multigrid`, this event names a per-window grid id that
    /// is never `grid_resize`d or `grid_line`d, not the global grid every
    /// other redraw event in that mode addresses.
    fn viewport_on(grid: u64, win: u64, topline: u64) -> UiEvent {
        UiEvent::WinViewport {
            grid,
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
            .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
            .expect("plain insert-mode character");

        state.reconcile(&[viewport(1, 13), UiEvent::Flush]);

        assert!(state.pending().is_empty());
    }

    /// The single-grid trap `docs/multigrid-wire-capture.md` names: without
    /// `ext_multigrid`, `win_viewport` still names a per-window grid (here
    /// 2) that is never `grid_resize`d or `grid_line`d, while the content it
    /// reports moving is repainted on grid 1, the only canvas that exists.
    /// A global-grid prediction must retire on this shift the same as it
    /// would on one that (correctly, under single-grid) named grid 1 --
    /// scoping retirement to the literal grid id `win_viewport` carries
    /// would leave it stranded over relocated text forever, since that id
    /// never appears in a `grid_line` for [`SpeculateState::reconcile`]'s
    /// answered-by check to retire it another way.
    #[test]
    fn a_phantom_window_grid_id_still_retires_the_global_grids_own_predictions() {
        let mut state = SpeculateState::default();
        state.reconcile(&[viewport_on(2, 1, 10)]);
        let _ = state
            .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
            .expect("plain insert-mode character");

        state.reconcile(&[viewport_on(2, 1, 13), UiEvent::Flush]);

        assert!(
            state.pending().is_empty(),
            "a global-grid prediction must retire on a viewport shift \
             whatever phantom grid id win_viewport names it with"
        );
    }

    /// Grid-scoped retirement's whole purpose, under the addressing view
    /// ships: two windows typed into, one of them scrolls, and only that
    /// one's predictions go. Predicting on the global grid instead --
    /// which every other leg here does, because that is what a single-grid
    /// session tags with -- cannot see this, since grid 1 retires on any
    /// window's shift by design.
    #[test]
    fn a_window_grids_shift_retires_its_own_predictions_and_no_bystanders() {
        let mut state = SpeculateState::default();
        state.reconcile(&[viewport_on(2, 1, 10), viewport_on(3, 2, 40)]);
        let mover = state
            .predict("insert", GridId(2), 'a', (2, 4), stamp(0))
            .expect("plain insert-mode character");
        let bystander = state
            .predict("insert", GridId(3), 'b', (5, 9), stamp(0))
            .expect("plain insert-mode character");

        state.reconcile(&[viewport_on(2, 1, 13), UiEvent::Flush]);

        assert_eq!(
            state.pending(),
            &[bystander],
            "the shifted window's prediction retires and the other window's stands"
        );

        state.reconcile(&[viewport_on(3, 2, 44), UiEvent::Flush]);

        assert!(
            state.pending().is_empty(),
            "and the same reading applies the other way round: {:?}",
            state.pending()
        );
        assert_ne!(
            mover, bystander,
            "the two predictions must be distinguishable for either assertion to mean anything"
        );
    }

    /// The other half of the same scoping, one reading later: a `grid_line`
    /// answers the cell it names on the grid it names, and a coincidence of
    /// row and column on the window across the split is not an answer.
    /// Under single-grid every event names grid 1 and the question cannot
    /// arise, which is why it needs a leg of its own here.
    #[test]
    fn a_grid_line_on_one_window_answers_no_cell_on_another() {
        let mut state = SpeculateState::default();
        let cell = state
            .predict("insert", GridId(2), 'a', (2, 4), stamp(0))
            .expect("plain insert-mode character");

        state.reconcile(&[grid_line_on(3, 2, 4, "a"), UiEvent::Flush]);

        assert_eq!(
            state.pending(),
            &[cell],
            "grid 3 writing the same row and column must not retire a grid-2 prediction"
        );

        state.reconcile(&[grid_line_on(2, 2, 4, "a"), UiEvent::Flush]);

        assert!(
            state.pending().is_empty(),
            "and the prediction's own grid writing that cell does retire it: {:?}",
            state.pending()
        );
    }

    /// And the reason it cannot simply retire on the event: nvim reports a
    /// viewport for every cursor move inside an unmoved window, which is
    /// every keystroke of the burst the feature exists for.
    #[test]
    fn a_viewport_reported_again_at_the_same_topline_retires_nothing() {
        let mut state = SpeculateState::default();
        state.reconcile(&[viewport(1, 10)]);
        let cell = state
            .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
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
            .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
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
            .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
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
            .predict("insert", GLOBAL_GRID, 'a', (2, 4), stamp(0))
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
            .predict("insert", GLOBAL_GRID, 'a', (0, u16::MAX), stamp(0))
            .expect("plain insert-mode character");
        let second = state
            .predict("insert", GLOBAL_GRID, 'b', (0, u16::MAX), stamp(0))
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
        folded(&mut model, &[viewport(1, 10), UiEvent::Flush], stamp(0));
        folded(&mut model, &[viewport(1, 40), UiEvent::Flush], stamp(0));
        model.dirty = false;

        fold_engine_call(&mut model, &input("x"), stamp(0));
        folded(&mut model, &[viewport(1, 40), UiEvent::Flush], stamp(0));

        assert_eq!(
            model.speculate.pending().len(),
            1,
            "a viewport that never moved retired the burst"
        );
    }

    /// The fallback `cursor_local` offers a painter is not one a predictor
    /// may take: with the cursor's own pane hidden, grid 1's cursor field
    /// is whatever a multigrid session never set it to, so predicting there
    /// puts the glyph at (0, 0) instead of under the typing. Dropping the
    /// prediction costs one unaccelerated character; taking the fallback
    /// paints a wrong one.
    #[test]
    fn a_keystroke_whose_cursor_pane_is_hidden_predicts_nothing() {
        use crate::events::WinHandle;
        use crate::grid::registry::{GridEvent, GridId};
        use crate::grid::GridOp;
        let mut model = typing_model();
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: GridId(4),
            op: GridOp::Resize {
                width: 39,
                height: 23,
            },
        });
        model.engine.apply_grid_event(GridEvent::Window {
            grid: GridId(4),
            win: WinHandle(4),
            startrow: 0,
            startcol: 41,
            width: 39,
            height: 23,
        });
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: GridId(4),
            op: GridOp::CursorGoto { row: 2, col: 5 },
        });

        fold_engine_call(&mut model, &input("x"), stamp(0));
        assert_eq!(
            model.speculate.pending().len(),
            1,
            "a visible cursor pane predicts as it always did"
        );
        assert_eq!(
            model.speculate.pending().first().map(|cell| cell.grid),
            Some(GridId(4)),
            "and predicts in the pane's own grid"
        );

        model
            .engine
            .apply_grid_event(GridEvent::Hide { grid: GridId(4) });
        fold_engine_call(&mut model, &input("y"), stamp(1));

        assert_eq!(
            model.speculate.pending().len(),
            1,
            "the hidden pane's keystroke must add no prediction on the global grid: {:?}",
            model.speculate.pending()
        );
        assert!(
            model
                .speculate
                .pending()
                .iter()
                .all(|cell| cell.grid == GridId(4)),
            "and nothing may be tagged with grid 1: {:?}",
            model.speculate.pending()
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
                grid: crate::grid::registry::GLOBAL_GRID,
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
