//! Pure state for the agent panel overlay: the transcript, the composer
//! line, and whatever the agent is waiting on. No ACP or `view-ai` types
//! reach this module -- transcript entries are copied down to plain strings
//! and a closed role vocabulary, and a permission prompt's options carry
//! `view-core`'s own [`crate::native::ai_event::PermissionOption`] (already
//! a closed, wire-free vocabulary), never a raw wire value, the same way
//! `native::tree` never holds a `git2` type and `native::picker` never
//! holds a ripgrep match struct.
//!
//! This module owns the shape, not the behaviour: streaming a transcript
//! chunk, answering a permission request, and reviewing a diff all fold new
//! state into these types in place, rather than a live session replacing
//! them with a shape of its own.

use super::views::{AiPanelView, Span};

mod permission;
mod review;
mod transcript;

pub use permission::PermissionPrompt;
pub use review::{AcceptRefusal, DiffReviewState, ReviewSync};
pub use transcript::{
    Transcript, TranscriptAnchor, TranscriptEntry, TranscriptEntryKind, TranscriptRole,
};

/// How far, and which way, one scroll key moves the AI panel's transcript
/// window. See [`AiPanelState::scroll_transcript`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptScroll {
    PageBack,
    PageForward,
    HalfPageBack,
    HalfPageForward,
}

/// The session's context-window and cost accounting, folded from
/// [`crate::native::ai_event::AiEvent::UsageUpdated`]. A panel stat, not a
/// transcript row: the wire's own `usage_update` discriminant carries a
/// snapshot to display beside the conversation, not an event that happened
/// in it, so it replaces [`AiPanelState::usage`] in place rather than
/// folding into [`AiPanelState::transcript`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct UsageStats {
    pub used: u64,
    pub size: u64,
    pub cost: Option<crate::native::ai_event::Cost>,
}

impl UsageStats {
    /// This snapshot as the panel's one accounting row.
    ///
    /// The currency is printed as the code the agent sent rather than
    /// mapped to a symbol: the wire places no constraint on it at all, and
    /// a table of guesses would render an unknown code as somebody else's
    /// money.
    #[must_use]
    pub fn render(&self) -> String {
        let mut row = format!("context {}/{}", self.used, self.size);
        if let Some(cost) = &self.cost {
            row.push_str(&format!(", cost {:.2} {}", cost.amount, cost.currency));
        }
        row
    }
}

/// The agent panel's state: which session it belongs to, its transcript so
/// far, its own composer line, and whatever it is currently blocked on.
#[must_use]
#[derive(Debug, Clone, PartialEq)]
pub struct AiPanelState {
    pub session_id: Option<String>,
    /// Agent output, folded per message id as chunks stream in, and tool
    /// calls folded per `tool_call_id` as their status advances.
    pub transcript: Transcript,
    /// The panel's own prompt-composition line.
    pub input: String,
    /// A single slot by design, not a queue: ACP blocks the agent's own
    /// turn on the reply, so a conformant agent never has two outstanding
    /// at once, and a second arriving request is a protocol violation
    /// rather than a capacity problem. The permission handler owns what
    /// that second request is answered with; this field's only contract is
    /// that it holds at most one prompt and is never overwritten by an
    /// unanswered one.
    pub pending_permission: Option<PermissionPrompt>,
    /// Whether the panel's own input line currently owns the keyboard, set
    /// only by the user's own explicit `open`/`focus`/`toggle` invocation
    /// and cleared on close (by `Model::close_ai_panel`, the single
    /// authoritative closing point) or by `<Esc>` while entered -- never by
    /// a `PermissionRequested` arriving while the panel is closed (see
    /// `update::open_ai_panel`'s doc): the panel is non-modal, so becoming
    /// visible and taking the keyboard are two different things.
    ///
    /// This is not merely consulted by the focus machinery -- it *is* the
    /// focus machinery for this overlay: `Model::takes_focus_now` reads it
    /// directly, so `model.focus()` names the AI panel overlay exactly
    /// when this is `true` and nothing else on the stack outranks it, the
    /// same way any other focus-taking overlay works. `y`/`n`/`a`/`<Esc>`
    /// reach the pending permission prompt (`route_key`'s
    /// `Focus::Native(OverlayKind::Ai)` arm) only through that real focus,
    /// never through a side channel ahead of it -- with the panel merely
    /// open and this `false`, every key, including `y`/`n` as ordinary
    /// engine commands, reaches nvim exactly as if the panel were not
    /// there at all.
    pub focused: bool,
    /// Whether a prompt this panel submitted is still awaiting
    /// `AiEvent::TurnEnded` (or a crash that ends the turn without one).
    /// Set only by a `<CR>` submission (`route_key`'s `Some(OverlayKind::Ai)`
    /// arm) and cleared by `on_ai_event`'s `TurnEnded`/`SessionCrashed`
    /// arms, regardless of which session reported them: this is a UI gate
    /// on the cancel key, not a session identity check, so a stale flag
    /// left set by a session that died without a `TurnEnded` is exactly
    /// what those two arms both clearing it exists to prevent. Gates
    /// `<C-c>`: cancelling with nothing in flight has no turn to cancel.
    pub turn_in_flight: bool,
    /// Panel-local crash surface, deliberately not a transient toast: a
    /// crashed long-running session is easy to miss in four seconds.
    pub local_error: Option<String>,
    /// The session's last-reported context-window and cost accounting, or
    /// `None` before the first `usage_update` arrives. See [`UsageStats`].
    pub usage: Option<UsageStats>,
    /// The diff proposal currently under review, if any.
    ///
    /// A review the user is part way through is never replaced out from
    /// under them: a proposal arriving while this slot is full waits in
    /// [`Self::pending_diff_next`] instead.
    pub pending_diff: Option<DiffReviewState>,
    /// The one proposal waiting for the open review to end, on the
    /// [`Self::pending_permission`] precedent: a single slot, not a queue.
    ///
    /// It opens -- resolve, attach, and all -- the moment [`Self::pending_diff`]
    /// clears, so an announcement is never the last the user hears of a
    /// proposal. A third proposal, with this slot already full, is the one
    /// case that is announced and dropped, and it is dropped at the driver
    /// too (`AiCommand::DiscardProposal`) so the agent restating it later
    /// proposes it again rather than being deduplicated against a proposal
    /// nobody ever saw.
    ///
    /// It goes with a crashed session (`on_ai_event`'s `SessionCrashed`
    /// arm) and survives a turn ending, unlike
    /// [`Self::pending_permission`], which is cleared by both. The two are
    /// not the same kind of state: a permission prompt is a question whose
    /// answer goes back on the wire, so a turn nobody is waiting out any
    /// more has nothing left to answer, while a review is decided entirely
    /// against nvim and needs no session at all to be honoured. A turn
    /// ending is also the ordinary case here rather than an edge: an agent
    /// that edits two files in one turn queues its second proposal in this
    /// very slot, and dropping it at `TurnEnded` would take back a diff the
    /// user was told to expect moments before they could look at it.
    pub pending_diff_next: Option<DiffReviewState>,
    /// The counter every `RpcCall::LoadHidden` this crate issues draws its
    /// generation from -- a diff review's own resolve and an agent
    /// filesystem request's alike -- bumped per resolve on the
    /// `PickerState::generation` precedent.
    ///
    /// One counter for both, not one each: the two kinds of holder share
    /// the `Msg::HiddenBufferLoaded` reply, and separate counters would let
    /// a review and a filesystem request wear the same generation and each
    /// claim the other's answer. Private, with
    /// [`Self::next_hidden_generation`] the only way to move it, so a
    /// future third holder cannot start its own sequence by accident.
    hidden_generation: u64,
    /// Where the transcript window starts while the panel is held away from
    /// the newest row, or `None` while it follows the tail.
    ///
    /// `None` is the resting state, not merely the initial one: a panel
    /// that follows re-derives its window from the newest row on every
    /// frame, so a chunk streaming in scrolls the transcript the way a
    /// terminal does. A held anchor is the user's own position and nothing
    /// but a scroll key, a submitted prompt, or the window catching the
    /// tail again moves it -- an appended row must not slide a window
    /// somebody is reading.
    ///
    /// Private, so the follow-vs-held distinction cannot be set to a state
    /// [`Self::view`] would paint as held while it sits at the tail: the
    /// two scroll methods are the only writers, and both settle it against
    /// [`Transcript::tail_anchor`] before storing it.
    transcript_top: Option<TranscriptAnchor>,
}

impl AiPanelState {
    /// The next tag for a holder of `Msg::HiddenBufferLoaded`, one ahead of
    /// the last one handed out.
    ///
    /// Reached from [`Model::next_hidden_generation`](crate::model::Model::next_hidden_generation),
    /// which is where callers ask; this is the single place the counter
    /// moves.
    pub fn next_hidden_generation(&mut self) -> u64 {
        self.hidden_generation += 1;
        self.hidden_generation
    }

    /// A freshly opened panel: no session bound yet, an empty transcript,
    /// nothing typed, nothing pending.
    pub fn new() -> Self {
        Self {
            session_id: None,
            transcript: Transcript::new(),
            input: String::new(),
            pending_permission: None,
            focused: false,
            turn_in_flight: false,
            local_error: None,
            usage: None,
            pending_diff: None,
            pending_diff_next: None,
            hidden_generation: 0,
            transcript_top: None,
        }
    }

    /// How many transcript rows a panel `panel_height` terminal rows tall
    /// has room for, once the overlay's own chrome has had its share.
    ///
    /// This is the room, not the distance a page moves: a held window
    /// spends one of these rows on [`MORE_BELOW`], and paging by the room
    /// rather than by what was drawn would step over the line that row
    /// displaced. [`Self::scroll_transcript`] takes that off, in the one
    /// place both it and [`Self::view`] read.
    ///
    /// A pending permission's rows and an open review's summary are
    /// deliberately not subtracted. Neither state lets a scroll key through
    /// to the transcript at all (`route_key`'s `reaches_past_a_panel_owner`
    /// is the gate), so the window under them is following the tail, and a
    /// window a few rows taller than the panel can draw is cut back to its
    /// newest rows by the overlay itself -- the end a follower wants kept.
    /// Counting them here would instead have to be kept in step with
    /// `view-surface`'s own header assembly, which this crate cannot see.
    #[must_use]
    pub fn transcript_viewport(&self, panel_height: usize) -> usize {
        panel_height
            .saturating_sub(CHROME_ROWS)
            .saturating_sub(usize::from(self.usage.is_some()))
            .saturating_sub(usize::from(self.local_error.is_some()))
    }

    /// Moves the transcript window for one scroll key, reporting whether it
    /// moved -- which is what the caller marks the model dirty on.
    ///
    /// Takes the panel's own height, the same number [`Self::view`] paints
    /// from, so the distance a page moves and the rows a page drew are
    /// derived from one input and cannot disagree: a page lands exactly
    /// where the last one stopped, in either direction, skipping nothing
    /// and repeating nothing.
    pub fn scroll_transcript(&mut self, scroll: TranscriptScroll, panel_height: usize) -> bool {
        let viewport = self.transcript_viewport(panel_height);
        let page = viewport.saturating_sub(MARKER_ROWS);
        let distance = match scroll {
            TranscriptScroll::PageBack | TranscriptScroll::PageForward => page,
            TranscriptScroll::HalfPageBack | TranscriptScroll::HalfPageForward => page.div_ceil(2),
        };
        let (from, tail) = self.window(viewport);
        let next = match scroll {
            TranscriptScroll::PageBack | TranscriptScroll::HalfPageBack => {
                self.transcript.scrolled_back(from, distance)
            }
            // Nothing forward of the tail to reach. The assignment is what
            // retires an anchor the panel has since grown past: nothing
            // moved on screen, but the state stops claiming to be held.
            _ if from == tail => {
                self.transcript_top = None;
                return false;
            }
            _ => self.transcript.scrolled_forward(from, distance),
        };
        self.settle(next, tail)
    }

    /// Returns the transcript to following the newest row -- what submitting
    /// a prompt does, since an answer the panel would have to be scrolled
    /// back down to read is an answer the user cannot see arriving.
    pub fn follow_transcript_tail(&mut self) {
        self.transcript_top = None;
    }

    /// Where the transcript window starts for a `viewport`-row panel, and
    /// the tail anchor it is measured against; the two are equal exactly
    /// when the panel is following.
    ///
    /// A held anchor is re-checked against the current viewport rather than
    /// trusted, because the viewport is not the one it was set against: a
    /// terminal grown taller moves the tail back past a window held on the
    /// smaller panel, and painting that as held would draw [`MORE_BELOW`]
    /// over nothing. Every reader of [`Self::transcript_top`] goes through
    /// here, so paint and scroll cannot disagree about which state the
    /// panel is in.
    fn window(&self, viewport: usize) -> (TranscriptAnchor, TranscriptAnchor) {
        let tail = self.transcript.tail_anchor(viewport);
        (
            self.transcript_top
                .filter(|top| *top < tail)
                .unwrap_or(tail),
            tail,
        )
    }

    /// Stores `next` as the window's position, or `None` when it has caught
    /// up with `tail`. The single writer of [`Self::transcript_top`], so
    /// "at the tail means following" cannot be true on one path and false
    /// on another.
    fn settle(&mut self, next: TranscriptAnchor, tail: TranscriptAnchor) -> bool {
        let held = (next < tail).then_some(next);
        let moved = held != self.transcript_top;
        self.transcript_top = held;
        moved
    }

    /// The panel's current paint frame: the composer line as typed, the
    /// transcript rendered oldest first (see [`Transcript::rows_from`]
    /// for how a paint that follows a lone folded chunk avoids re-rendering
    /// every earlier entry), the crash banner when [`Self::local_error`] is
    /// set, and the pending permission prompt's own rows when one is
    /// outstanding (see [`PermissionPrompt::render_rows`]).
    ///
    /// A prompt sitting on an un-entered panel (auto-opened, see
    /// [`Self::focused`]'s doc) is otherwise unanswerable -- nothing on
    /// screen would say how a blocked agent gets its reply -- so this is
    /// the one place that appends [`ENTER_HINT`] to the prompt's own rows;
    /// [`PermissionPrompt::render_rows`] stays unaware of focus, since the
    /// hint depends on this state, not on the prompt's own content.
    /// An entered panel also announces itself in its title: entry swallows
    /// every key but `<Esc>`, and unlike the modal, centered overlays that
    /// state is not self-evident from geometry alone, so the border is the
    /// one always-visible place to say how to get back out.
    ///
    /// `panel_height` is the overlay's own height in terminal rows;
    /// [`Self::transcript_viewport`] turns it into the row budget this
    /// frame renders and clones. Bounded because a transcript grows without
    /// limit over a session and rows past what the panel can paint are cut
    /// by the overlay anyway (see [`Transcript::rows_from`]). The window
    /// ends at the newest row unless a scroll key has held it elsewhere, in
    /// which case its last row says so instead: a reader scrolled back
    /// otherwise has no way to tell a quiet agent from a transcript that
    /// has moved on beneath them.
    ///
    /// An open review is not bounded the same way -- its rows are a fixed
    /// set of hunks the user scrolls through by cursor, so the window it
    /// needs is wherever that cursor is, not the first screenful.
    #[must_use]
    pub fn view(&self, panel_height: usize) -> AiPanelView {
        let visible_rows = self.transcript_viewport(panel_height);
        // An open review takes over the scrolling rows rather than
        // appending to them: its scroll region is its own hunks and their
        // context, never the whole transcript with a diff somewhere in it
        // (and never the whole buffer -- see `DiffReviewState::hunk_rows`).
        // The transcript is still in state and comes back the moment the
        // review closes.
        let rows = match &self.pending_diff {
            Some(review) => review.hunk_rows(),
            None => {
                let (start, tail) = self.window(visible_rows);
                if start == tail {
                    self.transcript.rows_from(tail, visible_rows)
                } else {
                    // The marker spends a row of the window rather than
                    // sitting above it: it is the last row, where a reader
                    // looking for the newest line looks, and it lands in
                    // the same tail the overlay keeps when the panel is
                    // shorter than this budget.
                    let mut rows = self
                        .transcript
                        .rows_from(start, visible_rows.saturating_sub(MARKER_ROWS));
                    rows.push(vec![Span::plain(MORE_BELOW)]);
                    rows
                }
            }
        };
        let mut view = AiPanelView::new(if self.focused { FOCUSED_TITLE } else { TITLE })
            .with_input(self.input.clone())
            .with_rows(rows);
        if let Some(usage) = &self.usage {
            view = view.with_usage(vec![Span::plain(usage.render())]);
        }
        if let Some(review) = &self.pending_diff {
            let mut rows = review.summary_rows();
            if let Some(queued) = &self.pending_diff_next {
                rows.push(vec![Span::plain(format!(
                    "{} is queued and opens when this review ends",
                    queued.path.display()
                ))]);
            }
            if !self.focused {
                rows.push(vec![Span::plain(ENTER_HINT)]);
            }
            view = view.with_review(rows, review.cursor_row());
        }
        if let Some(message) = &self.local_error {
            let hint = if self.focused {
                DISMISS_KEY_HINT
            } else {
                DISMISS_VERB_HINT
            };
            // One row, not an error row plus a hint row: the overlay's
            // tail-keep truncation would keep a trailing hint row and drop
            // the error itself at the tightest budget.
            view = view.with_local_error(vec![vec![Span::plain(format!(
                "Error: {message} -- {hint}"
            ))]]);
        }
        match &self.pending_permission {
            Some(prompt) => {
                let mut rows = prompt.render_rows();
                if !self.focused {
                    rows.push(vec![Span::plain(ENTER_HINT)]);
                }
                view.with_pending_permission(rows)
            }
            None => view,
        }
    }
}

impl Default for AiPanelState {
    fn default() -> Self {
        Self::new()
    }
}

/// The overlay's title, drawn into its top border.
const TITLE: &str = "AI Agent";

/// Rows of every AI panel that are never transcript: the frame's top and
/// bottom borders, the composer line, and the rule beneath it. Fixed
/// because those four are drawn whatever the session is doing; the header
/// rows that come and go are counted alongside it in
/// [`AiPanelState::transcript_viewport`].
const CHROME_ROWS: usize = 4;

/// The held window's last row, standing in for the newest rows it is
/// scrolled away from. It names the key that gets back rather than only
/// reporting the state: a reader who cannot see the newest line needs the
/// way back to it, not a notification that they are lost.
const MORE_BELOW: &str = "-- more below, <PageDown> follows again --";

/// Rows [`MORE_BELOW`] costs a held window. Subtracted in both the place
/// that renders the window and the place that decides how far a page moves
/// it, which is the whole reason it is named once here.
const MARKER_ROWS: usize = 1;

/// The entered panel's title: the border is the one surface that shows in
/// every state, so it carries the fact that keys now belong to the panel
/// and names the way back out.
const FOCUSED_TITLE: &str = "AI Agent -- focused, Esc returns";

/// Named after the verb it points at (`update::mod`'s `feature == "ai" &&
/// (verb == "open" || verb == "focus")` arm) -- shown beneath a pending
/// permission's own rows exactly when [`AiPanelState::focused`] is `false`,
/// the one state where the prompt is visible but `y`/`n`/`a`/`<Esc>` all
/// reach the engine instead of it.
const ENTER_HINT: &str = "Not focused -- run :View ai focus to answer";

/// The banner's own way out, shown beside the error itself: the entered
/// panel's composer consumes every printable, so the in-panel dismissal is
/// a named notation (`update::mod`'s `<C-d>` arm), while an un-entered
/// reader still has the `dismiss` verb. A persistent banner with no visible
/// exit reads as a stuck state.
const DISMISS_KEY_HINT: &str = "<C-d> dismisses";

/// [`DISMISS_KEY_HINT`]'s un-entered counterpart, pointing at the verb arm
/// (`feature == "ai" && verb == "dismiss"`) since panel keys are not routed
/// here while un-entered.
const DISMISS_VERB_HINT: &str = "Run :View ai dismiss to clear";

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::super::ai_event::ToolCallStatus;
    use super::super::views::Span;
    use super::*;

    /// A row budget larger than any transcript these tests build; what the
    /// budget itself does is pinned in [`Transcript::rows_from`]'s own
    /// tests.
    const ROOM: usize = 1_000;

    /// A panel height whose transcript window works out to ten rows, so the
    /// scrolling tests below can name the exact lines a page moves by.
    const TEN_ROW_PANEL: usize = 10 + CHROME_ROWS;

    /// A panel holding `lines` one-row agent messages, `line 0` upward.
    fn panel_with_lines(lines: usize) -> AiPanelState {
        let mut state = AiPanelState::new();
        for i in 0..lines {
            state.transcript.append_or_extend(
                Some(&format!("m{i}")),
                &format!("line {i}"),
                TranscriptRole::Agent,
            );
        }
        state
    }

    fn transcript_texts(state: &AiPanelState, panel_height: usize) -> Vec<String> {
        state
            .view(panel_height)
            .rows
            .into_iter()
            .map(|row| row.into_iter().map(|span| span.text).collect())
            .collect()
    }

    /// The reproduced complaint, as an assertion: a session taller than the
    /// panel showed its oldest screenful and nothing else, so the newest
    /// line -- the one the user is waiting on -- was the one line never on
    /// screen.
    #[test]
    fn a_panel_that_has_not_been_scrolled_shows_its_newest_line() {
        let state = panel_with_lines(50);

        let texts = transcript_texts(&state, TEN_ROW_PANEL);

        assert_eq!(texts.len(), 10, "the window is the panel's own height");
        assert_eq!(texts.last().unwrap(), "Agent: line 49");
        assert_eq!(texts.first().unwrap(), "Agent: line 40");
    }

    /// Scrolling back stops the panel following, and an agent that keeps
    /// talking must not drag the window along behind it -- a reader who has
    /// to chase the text they are reading cannot read it.
    #[test]
    fn a_scrolled_back_window_holds_while_the_agent_keeps_talking() {
        let mut state = panel_with_lines(50);
        assert!(state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL));

        let held = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(held.first().unwrap(), "Agent: line 31");

        for i in 50..70 {
            state.transcript.append_or_extend(
                Some(&format!("m{i}")),
                &format!("line {i}"),
                TranscriptRole::Agent,
            );
        }

        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL),
            held,
            "twenty appended lines must not move a window the user is holding"
        );
    }

    /// A held window says so on its own last row: without it, a reader who
    /// has scrolled back cannot tell a quiet agent from one whose newest
    /// output is off screen.
    #[test]
    fn a_held_window_names_the_key_that_follows_the_tail_again() {
        let mut state = panel_with_lines(50);

        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL).last().unwrap(),
            "Agent: line 49",
            "a following panel has nothing below it to point at"
        );

        state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL);

        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL).last().unwrap(),
            MORE_BELOW
        );
    }

    /// Reaching the tail again resumes following, so the next chunk to
    /// stream in scrolls the panel rather than landing out of sight behind
    /// a window that never let go.
    #[test]
    fn scrolling_forward_onto_the_tail_starts_following_again() {
        let mut state = panel_with_lines(50);
        state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL);

        assert!(state.scroll_transcript(TranscriptScroll::PageForward, TEN_ROW_PANEL));
        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL).last().unwrap(),
            "Agent: line 49"
        );

        state
            .transcript
            .append_or_extend(Some("m50"), "line 50", TranscriptRole::Agent);
        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL).last().unwrap(),
            "Agent: line 50",
            "following means the newest line, not the one that was newest when \
             the scroll ended"
        );
    }

    /// A page lands exactly where the last one stopped. A held window
    /// spends a row on [`MORE_BELOW`], so paging by the room the panel has
    /// rather than by the rows it drew steps over the line that row
    /// displaced -- once per page, invisibly, in both directions.
    #[test]
    fn a_page_lands_where_the_last_one_stopped_in_both_directions() {
        let mut state = panel_with_lines(50);
        let following = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(following.first().unwrap(), "Agent: line 40");

        state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL);
        let held = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(
            held.last().unwrap(),
            MORE_BELOW,
            "the last row is the marker, so nine transcript rows precede it"
        );
        assert_eq!(
            held[..held.len() - 1].last().unwrap(),
            "Agent: line 39",
            "the page must stop on the line directly above the one it came from"
        );

        state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL);
        let further = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(
            further[..further.len() - 1].last().unwrap(),
            "Agent: line 30",
            "and so must the next one"
        );

        state.scroll_transcript(TranscriptScroll::PageForward, TEN_ROW_PANEL);
        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL),
            held,
            "paging back the other way returns to the same rows"
        );
    }

    /// Half a page is half of what a page moves, so the two keys cannot
    /// disagree about what a page is either.
    #[test]
    fn a_half_page_moves_half_of_what_a_page_moves() {
        let mut state = panel_with_lines(50);
        state.scroll_transcript(TranscriptScroll::HalfPageBack, TEN_ROW_PANEL);

        let held = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(held.first().unwrap(), "Agent: line 35");
        assert_eq!(held[..held.len() - 1].last().unwrap(), "Agent: line 43");
    }

    /// A window held on a short panel can sit at or past the tail once the
    /// terminal grows. Painting it as held would put a "more below" marker
    /// over nothing, so the anchor is re-checked against the viewport of
    /// the frame being painted rather than the one it was set against.
    #[test]
    fn a_window_the_panel_grew_past_stops_claiming_there_is_more_below() {
        let mut state = panel_with_lines(20);
        state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL);
        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL).last().unwrap(),
            MORE_BELOW
        );

        let grown = 20 + CHROME_ROWS;
        let texts = transcript_texts(&state, grown);
        assert_eq!(texts.last().unwrap(), "Agent: line 19");
        assert!(
            !texts.iter().any(|row| row == MORE_BELOW),
            "the whole transcript fits now, so nothing is below it: {texts:?}"
        );
        assert!(
            !state.scroll_transcript(TranscriptScroll::PageForward, grown),
            "and the key that follows the tail again finds it already there"
        );
    }

    /// A transcript with nothing above what is already on screen has no
    /// scroll to hold: pinning it would stop the panel following for a
    /// window indistinguishable from the one it already had.
    #[test]
    fn a_transcript_shorter_than_the_panel_keeps_following() {
        let mut state = panel_with_lines(3);

        assert!(!state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL));

        state
            .transcript
            .append_or_extend(Some("m3"), "line 3", TranscriptRole::Agent);
        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL).last().unwrap(),
            "Agent: line 3"
        );
    }

    /// The accounting row and the crash banner both come out of the same
    /// rows the transcript would have had, so a window sized without them
    /// would page past lines the panel never drew.
    #[test]
    fn the_transcript_window_gives_up_a_row_to_the_usage_line_and_the_banner() {
        let mut state = AiPanelState::new();
        assert_eq!(state.transcript_viewport(TEN_ROW_PANEL), 10);

        state.usage = Some(UsageStats {
            used: 1,
            size: 2,
            cost: None,
        });
        assert_eq!(state.transcript_viewport(TEN_ROW_PANEL), 9);

        state.local_error = Some("gone".to_string());
        assert_eq!(state.transcript_viewport(TEN_ROW_PANEL), 8);
    }

    #[test]
    fn a_new_panel_is_empty() {
        let state = AiPanelState::new();
        assert_eq!(state.session_id, None);
        assert!(state.transcript.is_empty());
        assert_eq!(state.input, "");
        assert_eq!(state.pending_permission, None);
        assert!(!state.focused);
        assert!(!state.turn_in_flight);
        assert_eq!(state.local_error, None);
        assert_eq!(state.usage, None);
    }

    #[test]
    fn an_empty_panel_views_as_an_empty_transcript_with_the_typed_input() {
        let mut state = AiPanelState::new();
        state.input = "hello".to_string();
        let view = state.view(ROOM);
        assert_eq!(view.title, TITLE);
        assert_eq!(view.input, "hello");
        assert!(view.rows.is_empty());
        assert!(
            view.pending_permission.is_empty(),
            "no permission is pending, so there is nothing extra to draw"
        );
        assert!(
            view.local_error.is_empty(),
            "nothing crashed, so there is no banner to draw"
        );
    }

    /// The falsifiable half of the crash-surfacing contract: a session
    /// that set `local_error` must show up in the paint frame itself, not
    /// only in state nothing ever reads -- the same "state without a view
    /// arm is invisible" bar `pending_permission`'s own test above holds.
    #[test]
    fn a_local_error_renders_as_the_panels_own_banner_row() {
        let mut state = AiPanelState::new();
        state.local_error = Some("the agent exited (signal: 9)".to_string());
        let view = state.view(ROOM);
        assert_eq!(
            view.local_error,
            vec![vec![Span::plain(format!(
                "Error: the agent exited (signal: 9) -- {DISMISS_VERB_HINT}"
            ))]]
        );
    }

    /// The banner names its own way out, and the way out depends on where
    /// the reader is: an entered panel's composer eats printables so the
    /// hint names `<C-d>`; un-entered, keys are not routed to the panel at
    /// all so it names the `dismiss` verb instead.
    #[test]
    fn the_crash_banners_dismiss_hint_follows_focus() {
        let mut state = AiPanelState::new();
        state.local_error = Some("gone".to_string());
        state.focused = true;
        assert_eq!(
            state.view(ROOM).local_error,
            vec![vec![Span::plain(format!(
                "Error: gone -- {DISMISS_KEY_HINT}"
            ))]]
        );
        state.focused = false;
        assert_eq!(
            state.view(ROOM).local_error,
            vec![vec![Span::plain(format!(
                "Error: gone -- {DISMISS_VERB_HINT}"
            ))]]
        );
    }

    /// Entry swallows every key but `<Esc>`, so an entered panel must look
    /// different from an un-entered one in every state -- including idle,
    /// where no prompt rows exist to hang a hint off. The title is the one
    /// surface drawn in every state.
    #[test]
    fn an_entered_panel_announces_itself_in_its_title_even_while_idle() {
        let mut state = AiPanelState::new();
        assert_eq!(state.view(ROOM).title, TITLE);
        state.focused = true;
        let view = state.view(ROOM);
        assert_eq!(view.title, FOCUSED_TITLE);
        assert!(
            view.title.contains("Esc"),
            "the entered title must name the way back out: {:?}",
            view.title
        );
    }

    /// The panel's own `view()` must carry and render a pending prompt, not
    /// just hold it in state -- a `pending_permission` the paint frame
    /// never surfaces is a prompt the user cannot see or answer. Focused,
    /// so the answer is already reachable and no hint row is appended (see
    /// the two tests below for the un-entered/entered hint split itself).
    #[test]
    fn a_pending_permission_renders_the_question_and_its_options_with_their_kinds() {
        let mut state = AiPanelState::new();
        state.focused = true;
        state.pending_permission = Some(PermissionPrompt::new(
            1,
            "call_1",
            Some("Delete config.yaml".to_string()),
            vec![crate::native::ai_event::PermissionOption {
                option_id: "allow-once".to_string(),
                name: "Allow once".to_string(),
                kind: crate::native::ai_event::PermissionOptionKind::AllowOnce,
            }],
        ));
        let view = state.view(ROOM);
        assert_eq!(
            view.pending_permission,
            vec![
                vec![Span::plain("Permission requested for Delete config.yaml")],
                vec![Span::plain("  Allow once (allow_once)".to_string())],
            ]
        );
    }

    /// The discoverability half of the round-3 ruling: a prompt sitting on
    /// an un-entered panel (`focused` stays `false` after an agent's own
    /// auto-open) is otherwise unanswerable on screen, so `view()` must
    /// append [`ENTER_HINT`] naming the way in.
    #[test]
    fn a_pending_permission_on_an_unfocused_panel_appends_the_enter_hint() {
        let mut state = AiPanelState::new();
        assert!(!state.focused, "auto-open never sets focused");
        state.pending_permission = Some(PermissionPrompt::new(
            1,
            "call_1",
            Some("Delete config.yaml".to_string()),
            vec![crate::native::ai_event::PermissionOption {
                option_id: "allow-once".to_string(),
                name: "Allow once".to_string(),
                kind: crate::native::ai_event::PermissionOptionKind::AllowOnce,
            }],
        ));
        let view = state.view(ROOM);
        assert_eq!(
            view.pending_permission.last(),
            Some(&vec![Span::plain(ENTER_HINT)]),
            "an unanswerable prompt must name the way in: {:?}",
            view.pending_permission
        );
    }

    /// The mirror case: once the user has entered the panel, `y`/`n`/`a`
    /// already reach the prompt, so the hint would be stale advice and must
    /// not be drawn.
    #[test]
    fn a_pending_permission_on_a_focused_panel_carries_no_enter_hint() {
        let mut state = AiPanelState::new();
        state.focused = true;
        state.pending_permission = Some(PermissionPrompt::new(
            1,
            "call_1",
            Some("Delete config.yaml".to_string()),
            vec![crate::native::ai_event::PermissionOption {
                option_id: "allow-once".to_string(),
                name: "Allow once".to_string(),
                kind: crate::native::ai_event::PermissionOptionKind::AllowOnce,
            }],
        ));
        let view = state.view(ROOM);
        assert!(
            view.pending_permission
                .iter()
                .all(|row| row != &vec![Span::plain(ENTER_HINT)]),
            "a focused panel already answers keys; the hint would be stale: {:?}",
            view.pending_permission
        );
    }

    #[test]
    fn what_the_session_has_spent_is_on_the_panel_once_the_agent_reports_it() {
        let mut state = AiPanelState::new();
        assert!(
            state.view(ROOM).usage.is_empty(),
            "nothing is claimed about a session that has reported nothing"
        );

        state.usage = Some(UsageStats {
            used: 100,
            size: 1000,
            cost: Some(crate::native::ai_event::Cost {
                amount: 0.05,
                currency: "USD".to_string(),
            }),
        });

        assert_eq!(
            state.view(ROOM).usage,
            vec![vec![Span::plain("context 100/1000, cost 0.05 USD")]]
        );
    }

    #[test]
    fn a_transcript_entry_renders_with_its_speaker_prefix() {
        let mut state = AiPanelState::new();
        state
            .transcript
            .append_or_extend(Some("1"), "hi", TranscriptRole::User);
        state
            .transcript
            .append_or_extend(Some("2"), "hello", TranscriptRole::Agent);
        let view = state.view(ROOM);
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.rows[0], vec![Span::plain("You: hi")]);
        assert_eq!(view.rows[1], vec![Span::plain("Agent: hello")]);
    }

    #[test]
    fn a_tool_call_entry_renders_with_its_status_prefix() {
        let mut state = AiPanelState::new();
        state.transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        let view = state.view(ROOM);
        assert_eq!(view.rows, vec![vec![Span::plain("running: Read file")]]);
    }
}
