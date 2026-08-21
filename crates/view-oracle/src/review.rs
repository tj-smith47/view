//! The diff-review leg of a differential run: the one path where view
//! writes a buffer through `nvim_buf_set_text` instead of typing keys.
//!
//! Every other corpus entry drives both sides with the same script, because
//! both sides are supposed to reach the same state by the same route. This
//! one cannot: view's side applies an agent's proposal through
//! [`DiffReviewState`]'s own effects (the hunk engine's row/byte-column
//! arithmetic, carried out by `EngineHandle::set_buf_text`), and the
//! reference side reaches the same text the way a person would -- by typing
//! it. The comparison is then worth something it never was before: an
//! off-by-one in the column arithmetic writes bytes no manual edit would
//! ever produce, and the two sides' buffer text stops matching.
//!
//! The two routes leave different editing residue behind them: an API write
//! moves no cursor and sets no change marks, while typing sets `'[`, `']`,
//! `'^`, the jump mark and the unnamed register. [`NORMALIZE_KEYS`] is what
//! makes the comparison about the text rather than about that residue --
//! both sides run the same trailing edit and jump afterwards, which
//! overwrites every one of those from the same starting text, so anything
//! still differing afterwards is the text itself.
//!
//! The review is driven through `DiffReviewState` directly rather than
//! through key dispatch, on the same terms as
//! `crates/view-engine/tests/diff_review_undo_live.rs`: the panel's key
//! bindings are covered by their own dispatch tests, while the thing no
//! unit test can answer is what nvim's buffer holds after the write.

use std::path::PathBuf;

use view_core::msg::{BufferHandle, Effect, RpcCall};
use view_core::native::ai_panel::DiffReviewState;
use view_core::native::diff::{hunk, BufTextChangedEvent};
use view_engine::nvim_api::BufWriteOutcome;

use crate::parity::BUFFER_TEXT_EXPR;
use crate::{EngineSession, OracleError};

/// The generation every review this module drives is stamped with. A driver
/// runs one review at a time against one session, so the value only has to
/// be the same one the folds and write outcomes carry back.
const GENERATION: u64 = 1;

/// The path a driven review names. Never resolved: this driver binds the
/// session's own current buffer directly rather than through
/// `RpcCall::LoadHidden`, so nothing here reads the path back out.
const REVIEW_PATH: &str = "/oracle/diff-review";

/// The keys both sides run after a case's decisions, before the comparison.
///
/// `o<Esc>dd` is one identical edit performed by both sides from identical
/// text, which resets the change marks, the insert-exit mark and the
/// unnamed and numbered registers to the same values on each; `gg0` then
/// lands both cursors on the same cell and leaves both jump marks at the
/// same row. Without it every diff-review entry would report a cursor and
/// mark divergence that says nothing about the write under test, since the
/// reference side got where it is by typing and view's side by an API call.
pub const NORMALIZE_KEYS: &str = "<Esc>ggo<Esc>ddgg0";

/// One shape of hunk decision a corpus entry can name. The corpus file
/// carries the name; everything the case does lives in [`Self::steps`], so
/// an entry and its case cannot drift into describing different scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffReviewCase {
    /// One hunk at the buffer's last row, accepted.
    SingleHunkAccept,
    /// Two hunks, one accepted on its own and the rest by accept-all, then
    /// undone: the review's whole write must retract as one step.
    MultiHunkAcceptAll,
    /// A hunk declined, and a second proposal for the same file accepted in
    /// its place.
    RejectThenRepropose,
    /// A hunk staled by the user's own edit inside its anchor, re-diffed
    /// against what they left, then accepted.
    StaleReDiffThenAccept,
}

impl DiffReviewCase {
    /// Every case, in the order a corpus-coverage check reports them.
    pub const ALL: [Self; 4] = [
        Self::SingleHunkAccept,
        Self::MultiHunkAcceptAll,
        Self::RejectThenRepropose,
        Self::StaleReDiffThenAccept,
    ];

    /// How a corpus entry's `diff_review` field spells this case.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SingleHunkAccept => "single-hunk-accept",
            Self::MultiHunkAcceptAll => "multi-hunk-accept-all",
            Self::RejectThenRepropose => "reject-then-repropose",
            Self::StaleReDiffThenAccept => "stale-re-diff-then-accept",
        }
    }

    /// The case `name` spells, or `None` for a name no case answers to.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|case| case.name() == name)
    }

    /// This case's script, in order.
    ///
    /// Every proposal below changes the fixture's last row, which is the
    /// shape that makes the byte columns observable at all: a hunk with a
    /// row below it is addressed at column 0 on whole rows, while one
    /// running to the last row is addressed from the end of the row above
    /// to the end of the last replaced row -- byte offsets into text this
    /// fixture deliberately spells with multi-byte characters, so a
    /// character-column reading of them splices at the wrong offset.
    #[must_use]
    pub fn steps(self) -> &'static [ReviewStep] {
        match self {
            Self::SingleHunkAccept => &[
                ReviewStep::Propose("ünïcode\ntwo\nthree\nfôur\nFÎVE"),
                ReviewStep::Accept(0),
                ReviewStep::Reference("GccFÎVE<Esc>"),
            ],
            // The two undos are the assertion. One retracts the whole
            // review, the second retracts the typing that seeded the
            // buffer, so both sides end empty -- and a review that had
            // written two undo entries instead of one would still be
            // holding its seeded text after the same two keys.
            // `undojoin` is how the reference side reaches one entry by
            // typing, which is the same claim stated in nvim's own terms.
            Self::MultiHunkAcceptAll => &[
                ReviewStep::Propose("ünïcode\nTWÖ\nthree\nfôur\nFÎVE"),
                ReviewStep::Accept(0),
                ReviewStep::AcceptAll,
                ReviewStep::Reference("2GccTWÖ<Esc><Cmd>undojoin | normal! GccFÎVE<CR>"),
                ReviewStep::Shared("uu"),
            ],
            Self::RejectThenRepropose => &[
                ReviewStep::Propose("ünïcode\ntwo\nthree\nfôur\nWRÖNG"),
                ReviewStep::Reject(0),
                ReviewStep::Propose("ünïcode\ntwo\nthree\nfôur\nRÎGHT"),
                ReviewStep::Accept(0),
                ReviewStep::Reference("GccRÎGHT<Esc>"),
            ],
            // The user's edit lands on the row the hunk anchors to, not on
            // the row it replaces: that is what leaves the anchor usable
            // (so the hunk re-diffs rather than dying) while changing the
            // byte length of the row every column in the write is measured
            // from.
            Self::StaleReDiffThenAccept => &[
                ReviewStep::Propose("ünïcode\ntwo\nthree\nfôur\nFÎVE"),
                ReviewStep::Shared("4Gccfôurtëën<Esc>"),
                ReviewStep::FoldRow(3),
                ReviewStep::ReDiff(0),
                ReviewStep::Accept(0),
                ReviewStep::Reference("GccFÎVE<Esc>"),
            ],
        }
    }
}

/// One step of a case's script. Key steps are the runner's to deliver (it
/// owns both sessions); every other step is the review driver's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReviewStep {
    /// Keys both sides receive: the user's own editing, or an undo, which
    /// happens to both sessions identically.
    Shared(&'static str),
    /// Keys only the reference side receives: the manual edit sequence that
    /// reaches by typing what view's side reached through the review.
    Reference(&'static str),
    /// An agent proposes this text for the buffer under review, replacing
    /// any review already open.
    Propose(&'static str),
    /// Accept the hunk at this index.
    Accept(usize),
    /// Accept every hunk still fresh, as one write.
    AcceptAll,
    /// Decline the hunk at this index.
    Reject(usize),
    /// Re-anchor the stale hunk at this index.
    ReDiff(usize),
    /// Fold the current text of this 0-indexed buffer row into the review,
    /// the way the loop folds one `Msg::BufTextChanged`.
    FoldRow(u32),
}

/// Drives one case's review steps against an [`EngineSession`]'s own
/// engine, carrying every write out and folding nvim's answer back in --
/// the `Executor::run` plus `update()` pair, for the subset of the loop a
/// review needs.
#[derive(Debug, Default)]
pub struct ReviewDriver {
    review: Option<DiffReviewState>,
}

impl ReviewDriver {
    /// Carries out one step. Returns whether the step wrote to the buffer,
    /// which the caller owes a settle before typing into that session
    /// again.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if a probe or the write fails,
    /// [`OracleError::Parse`] if a probe's reply is not the number it must
    /// be, or [`OracleError::Review`] if the step itself cannot be carried
    /// out -- a proposal that yields no hunks, an accept the review
    /// refuses, a write nvim refuses. Every one of those means the case's
    /// own script no longer describes what the code does, which is a
    /// failure to report rather than a state to continue from.
    pub fn apply(
        &mut self,
        session: &mut EngineSession,
        step: ReviewStep,
    ) -> Result<bool, OracleError> {
        match step {
            ReviewStep::Shared(_) | ReviewStep::Reference(_) => Ok(false),
            ReviewStep::Propose(text) => {
                self.propose(session, text)?;
                Ok(false)
            }
            ReviewStep::Accept(index) => {
                let effects = self.review_mut()?.accept(index).map_err(|refusal| {
                    OracleError::Review(format!("accept({index}) refused: {refusal:?}"))
                })?;
                self.carry_out(session, &effects)?;
                Ok(true)
            }
            ReviewStep::AcceptAll => {
                let effects = self.review_mut()?.accept_all();
                if effects.is_empty() {
                    return Err(OracleError::Review(
                        "accept-all answered no write, so no hunk was still fresh".to_string(),
                    ));
                }
                self.carry_out(session, &effects)?;
                Ok(true)
            }
            ReviewStep::Reject(index) => {
                if self.review_mut()?.reject(index) {
                    Ok(false)
                } else {
                    Err(OracleError::Review(format!(
                        "reject({index}) refused: the hunk is not open"
                    )))
                }
            }
            ReviewStep::ReDiff(index) => {
                if self.review_mut()?.re_diff(index) {
                    Ok(false)
                } else {
                    Err(OracleError::Review(format!(
                        "re-diff({index}) refused: the hunk is not stale with an intact anchor"
                    )))
                }
            }
            ReviewStep::FoldRow(row) => {
                self.fold_row(session, row)?;
                Ok(false)
            }
        }
    }

    /// Opens a review of `proposal` against the text the session's current
    /// buffer holds right now, bound to that buffer.
    ///
    /// The bind's `RpcCall::BufAttach` is deliberately not carried out: this
    /// driver has no message sink to receive `nvim_buf_lines_event` on (an
    /// `EngineSession`'s own sink is unread by design), so
    /// [`ReviewStep::FoldRow`] reads the changed row back from nvim instead
    /// -- the same rows the event would have carried, from the same place.
    fn propose(&mut self, session: &mut EngineSession, proposal: &str) -> Result<(), OracleError> {
        let buf = probe_u64(session, "bufnr('%')")?;
        let changedtick = probe_u64(session, "b:changedtick")?;
        let old = session.eval_str(BUFFER_TEXT_EXPR)?;
        let hunks = hunk::diff(Some(&old), proposal);
        if hunks.is_empty() {
            return Err(OracleError::Review(format!(
                "the proposal yields no hunks against the buffer's own text {old:?}"
            )));
        }
        let mut review = DiffReviewState::new(1, PathBuf::from(REVIEW_PATH), GENERATION, hunks);
        let _ = review.bind(GENERATION, Some(BufferHandle(buf)), changedtick);
        self.review = Some(review);
        Ok(())
    }

    /// Folds the current text of buffer row `row` into the open review, in
    /// the shape nvim's own single-row change event carries.
    fn fold_row(&mut self, session: &mut EngineSession, row: u32) -> Result<(), OracleError> {
        let changedtick = probe_u64(session, "b:changedtick")?;
        let line = session.eval_str(&format!("getline({})", row.saturating_add(1)))?;
        let review = self.review_mut()?;
        let Some(buf) = review.buffer else {
            return Err(OracleError::Review(
                "the review never bound a buffer to fold a change into".to_string(),
            ));
        };
        review.apply_change(&BufTextChangedEvent {
            buf,
            generation: GENERATION,
            firstline: u64::from(row),
            lastline: u64::from(row).saturating_add(1),
            linedata: vec![line],
            changedtick,
            desynced: false,
        });
        Ok(())
    }

    /// Applies the review's own write against the live engine and folds
    /// nvim's answer back into it, exactly as `Executor::run` and `update()`
    /// do between them.
    fn carry_out(
        &mut self,
        session: &mut EngineSession,
        effects: &[Effect],
    ) -> Result<(), OracleError> {
        for effect in effects {
            let Effect::Rpc(RpcCall::BufSetText {
                buf,
                edits,
                undojoin,
                expected_changedtick,
                generation,
            }) = effect
            else {
                continue;
            };
            let outcome = session.engine.handle.set_buf_text(
                *buf,
                edits,
                *undojoin,
                *expected_changedtick,
            )?;
            let review = self.review_mut()?;
            match outcome {
                BufWriteOutcome::Applied { changedtick } => {
                    review.note_write_applied(*buf, *generation, changedtick);
                }
                BufWriteOutcome::BufferAdvanced => {
                    review.note_write_refused(*buf, *generation);
                    return Err(OracleError::Review(
                        "nvim refused the write: the buffer had moved past the tick the review \
                         named, which no case here scripts"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The open review, or the failure that says a case's step arrived
    /// before any proposal opened one.
    fn review_mut(&mut self) -> Result<&mut DiffReviewState, OracleError> {
        self.review
            .as_mut()
            .ok_or_else(|| OracleError::Review("no proposal has opened a review yet".to_string()))
    }
}

/// Reads one `nvim_eval` probe that must answer a number.
fn probe_u64(session: &mut EngineSession, expr: &str) -> Result<u64, OracleError> {
    let raw = session.eval_str(expr)?;
    raw.trim()
        .parse()
        .map_err(|_| OracleError::Parse(format!("{expr} answered {raw:?}, which is not a number")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn every_case_name_round_trips() {
        for case in DiffReviewCase::ALL {
            assert_eq!(
                DiffReviewCase::from_name(case.name()),
                Some(case),
                "{} must resolve back to its own case",
                case.name()
            );
        }
    }

    #[test]
    fn an_unknown_case_name_resolves_to_nothing() {
        assert_eq!(DiffReviewCase::from_name("accept-everything"), None);
    }

    /// Every case must end with the reference side having been given its own
    /// manual edit sequence: a case that never types on the reference side
    /// compares view's write against a buffer nobody edited, which passes
    /// only when the write did nothing.
    #[test]
    fn every_case_scripts_a_reference_edit() {
        for case in DiffReviewCase::ALL {
            assert!(
                case.steps()
                    .iter()
                    .any(|step| matches!(step, ReviewStep::Reference(_))),
                "{} scripts no reference-side edit",
                case.name()
            );
        }
    }
}
