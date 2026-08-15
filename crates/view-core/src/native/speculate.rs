//! Display-only prediction of what insert-mode typing is about to show.
//!
//! Pure and headless-testable, on the same terms as
//! [`supervision`](crate::native::supervision): nothing here reads a clock,
//! holds an RPC handle, or knows a connection exists. What reaches this
//! module is a keystroke somebody else decoded, a cursor position somebody
//! else read off the authoritative grid, and a stamp somebody else measured.
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
/// cell -- a partial update, or a scroll that moves content without
/// resending it -- would otherwise leave a stale glyph on screen
/// indefinitely. Sized well above the slowest round trip a healthy remote
/// session is expected to take, so a legitimately slow but working link is
/// never mistaken for staleness; at a second, a user who somehow reaches it
/// is already looking at an editor that has stopped answering.
pub const SPECULATION_MAX_AGE: Duration = Duration::from_secs(1);

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn stamp(millis: u64) -> SpecStamp {
        SpecStamp::new(Duration::from_millis(millis))
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
            pending: Vec::new(),
        };
        assert!(state.predict("insert", 'a', (0, 0), stamp(0)).is_some());

        state.reset_epoch();

        assert_eq!(state.epoch(), Epoch(u64::MAX));
        assert!(state.pending().is_empty());
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
}
