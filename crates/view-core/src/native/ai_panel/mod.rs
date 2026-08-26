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

use std::collections::BTreeMap;

use super::geometry::{interior_text_width, LIST_MARKER_COLS};
use super::views::{AiPanelView, Span};

mod permission;
mod review;
mod transcript;

pub use permission::PermissionPrompt;
pub(crate) use permission::StandingAnswer;
pub use review::{DiffReviewState, Refusal, ReviewSync};
pub use transcript::{
    Transcript, TranscriptAnchor, TranscriptEntry, TranscriptEntryKind, TranscriptRole,
    SPINNER_INTERVAL,
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
    /// The panel's own prompt-composition line, as typed. Longer than one
    /// painted row is ordinary: [`Self::view`] wraps it (see
    /// [`composer_width`]), so this is never cut to what fits.
    input: String,
    /// The byte offset of every line break in [`Self::input`], ascending --
    /// the row boundaries [`wrap_window`] opens its window on.
    ///
    /// Cached rather than searched for, because searching is a scan of the
    /// whole input on every frame, which is the one cost the window exists
    /// to refuse. Every break rather than the last one, because the window
    /// opens wherever the panel's own height puts it, and the boundary it
    /// has to land on is the break before *that* point -- with only the
    /// last one cached, a short closing line under a long paste leaves the
    /// window a column off, and the prompt visibly reflows when it is sent.
    ///
    /// Eight bytes per line held in the composer, and a binary search per
    /// paint ([`Self::input`] is the text; this is only where its rows
    /// start). Maintained by the three methods that change that text
    /// ([`Self::push_input`], [`Self::pop_input`], [`Self::take_input`]),
    /// which are the only writers there are, since the text itself is
    /// private.
    breaks: Vec<usize>,
    /// The first and last byte offsets in [`Self::input`] holding a
    /// character that is not ASCII, or `None` while every character is.
    ///
    /// [`wrap_window`]'s grid steps a row at a time in bytes, and a row is
    /// `width` bytes only where a cell is one byte. One accented character
    /// ahead of the window's opening therefore slides that opening off the
    /// row boundary it is defined to land on, and the composer paints rows
    /// the whole input's wrap never had -- text that visibly re-flows as
    /// the next character is typed and changes shape again once the prompt
    /// is sent, since the transcript wraps the whole prompt. The two
    /// offsets are what lets the grid be used only over a stretch it is
    /// exact for.
    ///
    /// Both ends rather than a flag, because either end alone leaves a
    /// whole shape on the slow path for nothing: a long single-line paste
    /// carrying one emoji near its end is exact over everything before it,
    /// and a long prompt whose only accented character is in its first line
    /// is exact over every line after it.
    ///
    /// Maintained by the same three methods [`Self::breaks`] is, and
    /// deliberately only in the safe direction: a backspace over the very
    /// character an offset names leaves that offset where it was, which
    /// widens the stretch the grid is refused on and never narrows it.
    non_ascii: Option<(usize, usize)>,
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
    /// same way any other focus-taking overlay works. The option digits
    /// and `<Esc>` reach the pending permission prompt (`route_key`'s
    /// `Focus::Native(OverlayKind::Ai)` arm) only through that real focus,
    /// never through a side channel ahead of it -- with the panel merely
    /// open and this `false`, every key, including those digits as
    /// ordinary engine counts, reaches nvim exactly as if the panel were
    /// not there at all.
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
    /// The standing answer each tool kind carries, from an always-allow or
    /// always-reject the user chose, for this session only -- nothing here
    /// is persisted, so a standing answer dies with the session it was given
    /// in.
    ///
    /// view keeps this because the pinned adapter does not: answering with
    /// the agent's own `allow_always` option id is answered correctly and
    /// then ignored, and the user is asked again on every later call
    /// (`.superpowers/sdd/2026-08-21-dogfood-fixes/task-21-rootcause.md`
    /// proves it with a probe containing no view code). A prompt that
    /// promises "Always Allow" and asks again is a promise view can keep on
    /// its own side, so it does: a later request naming a kind with a
    /// standing answer is answered without asking. An adapter that honours
    /// the wire's own standing permission never sends that later request at
    /// all, and this simply never fires. `allow_always` and `reject_always`
    /// are answered symmetrically because they are the same promise in
    /// opposite directions, and an "always" that only holds one way is the
    /// defect this exists to close.
    ///
    /// Private, with [`Self::record_standing_answer`],
    /// [`Self::standing_answer`] and [`Self::clear_standing_answers`] the
    /// only ways to it: a standing answer is how a question stops being
    /// asked, so what installs one and what drops the whole set are worth
    /// being able to find.
    standing_answers: BTreeMap<String, StandingAnswer>,
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
            breaks: Vec::new(),
            non_ascii: None,
            pending_permission: None,
            focused: false,
            turn_in_flight: false,
            local_error: None,
            usage: None,
            pending_diff: None,
            pending_diff_next: None,
            hidden_generation: 0,
            standing_answers: BTreeMap::new(),
            transcript_top: None,
        }
    }

    /// Appends `text` to the composer, wherever it came from: one typed
    /// character, the line break a composer-newline key inserts, or a whole
    /// bracketed paste.
    ///
    /// Verbatim, control characters and all -- what a paste reads as is the
    /// wrap's question, never this one -- and [`Self::breaks`] is carried
    /// forward by scanning only the appended text.
    pub fn push_input(&mut self, text: &str) {
        let base = self.input.len();
        self.breaks
            .extend(text.match_indices(['\n', '\r']).map(|(at, _)| base + at));
        if !text.is_ascii() {
            let first = text.char_indices().find(|(_, ch)| !ch.is_ascii());
            let last = text.char_indices().rev().find(|(_, ch)| !ch.is_ascii());
            if let (Some((first, _)), Some((last, _))) = (first, last) {
                let seen = self.non_ascii.get_or_insert((base + first, base + last));
                seen.1 = base + last;
            }
        }
        self.input.push_str(text);
    }

    /// Removes the composer's last character, reporting whether there was
    /// one to remove.
    ///
    /// The removed character is the composer's last, so the break it may
    /// have been is the last one recorded: nothing is re-derived and
    /// nothing is scanned. [`Self::non_ascii`] is deliberately left alone
    /// for the same reason -- re-deriving either end of it is the scan this
    /// avoids, and an offset that outlives its character only widens the
    /// stretch [`wrap_window`]'s grid is refused on.
    pub fn pop_input(&mut self) -> bool {
        let Some(ch) = self.input.pop() else {
            return false;
        };
        if ch == '\n' || ch == '\r' {
            self.breaks.pop();
        }
        true
    }

    /// Takes the composed prompt, leaving the composer empty -- what
    /// submitting one does.
    pub fn take_input(&mut self) -> String {
        self.breaks.clear();
        self.non_ascii = None;
        std::mem::take(&mut self.input)
    }

    /// The prompt as composed so far. Read-only: [`Self::breaks`] holds
    /// where its rows start, and a text written past these three methods
    /// would leave that answer describing a prompt nobody typed.
    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Records `answer` as `tool_kind`'s standing answer, so a later request
    /// naming it is answered without asking again (see
    /// [`Self::standing_answers`]). A later opposite answer replaces it:
    /// the user's most recent standing choice is the one that holds.
    pub(crate) fn record_standing_answer(&mut self, tool_kind: String, answer: StandingAnswer) {
        self.standing_answers.insert(tool_kind, answer);
    }

    /// The standing answer this session carries for `tool_kind`, if any.
    /// Looked up verbatim against the agent's own kind string: view reads no
    /// meaning into it, so it cannot decide that two spellings are the same
    /// permission.
    #[must_use]
    pub(crate) fn standing_answer(&self, tool_kind: &str) -> Option<StandingAnswer> {
        self.standing_answers.get(tool_kind).copied()
    }

    /// Drops every standing answer. One is scoped to the session it was
    /// given in, and a new session is a new agent with new work in front of
    /// it -- carrying them across would answer for a session the user never
    /// saw a question from.
    pub(crate) fn clear_standing_answers(&mut self) {
        self.standing_answers.clear();
    }

    /// Whether something the panel is showing owns its keys, so the
    /// composer is not the thing input reaches.
    ///
    /// The single list of those states. `update::route_key`'s
    /// `OverlayKind::Ai` arm reaches its composer section for exactly the
    /// states this answers `false` for -- plus the ways out of the panel,
    /// which every state honors -- and the paste path asks this directly. A
    /// state added to one of the two and not the other is text landing
    /// under a modal the reader is looking at.
    ///
    /// An open review is deliberately not one of them: its keys are
    /// buffer-local nvim mappings on the file being reviewed, not panel
    /// keys, so a review no longer stands between the reader and their own
    /// composer (see [`DiffReviewState::marks`]).
    #[must_use]
    pub fn an_owner_holds_the_keys(&self) -> bool {
        self.pending_permission.is_some()
    }

    /// How many transcript rows a panel `panel_height` terminal rows tall
    /// and `panel_width` columns wide has room for, once the overlay's own
    /// chrome has had its share.
    ///
    /// This is the room, not the distance a page moves: a held window
    /// spends one of these rows on [`MORE_BELOW`], and paging by the room
    /// rather than by what was drawn would step over the line that row
    /// displaced. [`Self::scroll_transcript`] takes that off, in the one
    /// place both it and [`Self::view`] read.
    ///
    /// A composer wrapped over more than one row is subtracted, unlike the
    /// header rows below, because a scroll key does reach the transcript
    /// with a half-written prompt on the composer (`ai_scroll_for`'s keys
    /// are exactly the ones the composer cannot type). A window left
    /// counting rows the composer has taken would have its oldest rows cut
    /// by the overlay instead, which slides a held window the user is
    /// reading.
    ///
    /// A pending permission's rows and an open review's summary are
    /// deliberately not subtracted. A window a few rows taller than the
    /// panel can draw is cut back to its newest rows by the overlay itself
    /// (`Body::items_keep_tail`) -- the end a reader is at, and the end a
    /// follower wants kept. Counting those rows here would instead have to
    /// be kept in step with `view-surface`'s own header assembly, which
    /// this crate cannot see.
    #[must_use]
    pub fn transcript_viewport(&self, panel_height: usize, panel_width: usize) -> usize {
        self.transcript_rows(
            panel_height,
            self.composer_rows(panel_height, panel_width).len(),
        )
    }

    /// The rows a `panel_height`-row panel leaves for its composer and its
    /// transcript to share: everything past [`CHROME_ROWS`] and whichever
    /// of the accounting row and the crash banner is currently showing.
    ///
    /// The single accounting both [`Self::composer_cap`] and
    /// [`Self::transcript_rows`] budget from. Two derivations counting
    /// different sets of rows is how the composer comes to be capped
    /// against a bigger pool than the transcript is measured out of, and
    /// the transcript ends up with fewer rows than the composer it was
    /// supposed to outlast -- which is the defect, not a rounding
    /// difference. Whatever row a later feature adds to the panel's chrome
    /// belongs here, once, and both sides stay in step by construction.
    fn shared_rows(&self, panel_height: usize) -> usize {
        panel_height
            .saturating_sub(CHROME_ROWS)
            .saturating_sub(usize::from(self.usage.is_some()))
            .saturating_sub(usize::from(self.local_error.is_some()))
    }

    /// [`Self::transcript_viewport`] for a caller that has already laid the
    /// composer out, so a paint wraps the input once rather than twice.
    /// [`CHROME_ROWS`] already counts the composer's first row, so only the
    /// rows it wrapped onto come off the shared pool.
    fn transcript_rows(&self, panel_height: usize, composer_rows: usize) -> usize {
        self.shared_rows(panel_height)
            .saturating_sub(composer_rows.saturating_sub(1))
    }

    /// The most rows the composer may grow to: half the rows it shares with
    /// the transcript, so a prompt long enough to fill the panel still
    /// leaves the transcript at least as many rows as it took. A longer
    /// prompt scrolls inside those rows (see [`Self::composer_rows`])
    /// rather than taking more.
    ///
    /// Never zero, so the composer is painted at every panel height a frame
    /// can be drawn at. That is also the one case where the transcript ends
    /// up with fewer rows than the composer: a panel whose chrome and
    /// banners already fill it has no transcript row to give, and the
    /// composer keeps its one.
    fn composer_cap(&self, panel_height: usize) -> usize {
        (self.shared_rows(panel_height) / 2).max(1)
    }

    /// The composer's painted rows: [`Self::input`] wrapped to what one row
    /// of a `panel_width`-wide panel holds, cut to the last
    /// [`Self::composer_cap`] of them.
    ///
    /// The cut keeps the tail, never the head: the composer only appends
    /// and backspaces, so the cursor is at the end of the input, and the
    /// last row is where the next character lands. Cutting the other way is
    /// the reported defect -- a prompt past the panel's width kept typing
    /// into a row nothing painted.
    fn composer_rows(&self, panel_height: usize, panel_width: usize) -> Vec<String> {
        let width = composer_width(panel_width);
        // A frame with no room for a prompt character clips every composer
        // row to the bare mark, so a composer that grew there would spend
        // the transcript's rows and paint nothing for them. The floor on
        // the width itself keeps the wrap from opening a row per character
        // in that same corner.
        let cap = if width == 0 {
            1
        } else {
            self.composer_cap(panel_height)
        };
        let width = width.max(1);
        wrap(
            wrap_window(&self.input, width, cap, &self.breaks, self.non_ascii),
            width,
            cap,
            Break::Cell,
        )
    }

    /// Where the next character typed will land: an index into the rows
    /// [`Self::view`] paints as the composer, and a column across that row
    /// in cells.
    ///
    /// Moves the transcript window for one scroll key, reporting whether it
    /// moved -- which is what the caller marks the model dirty on.
    ///
    /// Takes the panel's own height and width, the same numbers
    /// [`Self::view`] paints from, so the distance a page moves and the rows
    /// a page drew are derived from one input and cannot disagree: a page
    /// lands exactly where the last one stopped, in either direction,
    /// skipping nothing and repeating nothing.
    pub fn scroll_transcript(
        &mut self,
        scroll: TranscriptScroll,
        panel_height: usize,
        panel_width: usize,
    ) -> bool {
        let viewport = self.transcript_viewport(panel_height, panel_width);
        let width = transcript_width(panel_width);
        let page = viewport.saturating_sub(MARKER_ROWS);
        let distance = match scroll {
            TranscriptScroll::PageBack | TranscriptScroll::PageForward => page,
            TranscriptScroll::HalfPageBack | TranscriptScroll::HalfPageForward => page.div_ceil(2),
        };
        let (from, tail) = self.window(viewport, width);
        let next = match scroll {
            TranscriptScroll::PageBack | TranscriptScroll::HalfPageBack => {
                self.transcript.scrolled_back(from, distance, width)
            }
            // Nothing forward of the tail to reach. The assignment is what
            // retires an anchor the panel has since grown past: nothing
            // moved on screen, but the state stops claiming to be held.
            _ if from == tail => {
                self.transcript_top = None;
                return false;
            }
            _ => self.transcript.scrolled_forward(from, distance, width),
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
    ///
    /// `width` is re-checked for the same reason the viewport is: an entry
    /// wraps to the panel's width, so a terminal made narrower turns one
    /// entry into more rows and moves the tail just as growing it taller
    /// does.
    fn window(&self, viewport: usize, width: usize) -> (TranscriptAnchor, TranscriptAnchor) {
        let tail = self.transcript.tail_anchor(viewport, width);
        (
            self.transcript_top
                .map(|top| self.transcript.normalized(top, width))
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
    /// `panel_height` and `panel_width` are the overlay's own size in
    /// terminal cells; [`Self::composer_rows`] turns the width into the
    /// composer's painted rows and
    /// [`Self::transcript_viewport`] turns what is left into the row budget
    /// this frame renders and clones. Bounded because a transcript grows without
    /// limit over a session and rows past what the panel can paint are cut
    /// by the overlay anyway (see [`Transcript::rows_from`]). The window
    /// ends at the newest row unless a scroll key has held it elsewhere, in
    /// which case its last row says so instead: a reader scrolled back
    /// otherwise has no way to tell a quiet agent from a transcript that
    /// has moved on beneath them.
    ///
    /// An open review adds its summary rows and nothing else: the diff
    /// itself is drawn in the file, by nvim, over the real rows
    /// ([`DiffReviewState::marks`]).
    #[must_use]
    pub fn view(&self, panel_height: usize, panel_width: usize) -> AiPanelView {
        let composer = self.composer_rows(panel_height, panel_width);
        let visible_rows = self.transcript_rows(panel_height, composer.len());
        let width = transcript_width(panel_width);
        let (start, tail) = self.window(visible_rows, width);
        let rows = if start == tail {
            self.transcript.rows_from(tail, visible_rows, width)
        } else {
            // The marker spends a row of the window rather than sitting
            // above it: it is the last row, where a reader looking for the
            // newest line looks, and it lands in the same tail the overlay
            // keeps when the panel is shorter than this budget.
            let mut rows =
                self.transcript
                    .rows_from(start, visible_rows.saturating_sub(MARKER_ROWS), width);
            rows.push(vec![Span::plain(MORE_BELOW)]);
            rows
        };
        let mut view = AiPanelView::new(if self.focused { FOCUSED_TITLE } else { TITLE })
            .with_input_rows(composer)
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
            view = view.with_review(rows);
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
                    .with_permission_answer(prompt.answer_cell())
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

/// The cells the composer's prompt mark and the space after it take on its
/// first row, and the matching indent every wrapped row after it carries --
/// so every composer row breaks at the same column.
///
/// Named here, where the wrap is computed, rather than beside the mark
/// `view-surface` paints: a wrap width and a paint width that disagree by
/// one cell put a character past the frame's edge, which is the whole of
/// the defect this exists to close. `view-surface`'s own framing tests
/// measure its painted prefix against this.
pub const PROMPT_COLS: usize = 2;

/// The cells one composer row has for prompt text, on a panel `panel_width`
/// terminal columns wide: the framed interior (see
/// [`interior_text_width`]) less the prompt mark's own columns.
///
/// Zero at a panel too narrow to paint a prompt character at all -- the
/// honest answer, and the one [`AiPanelState::composer_rows`] reads to stop
/// the composer growing where nothing of it could show.
///
/// ```
/// use view_core::native::ai_panel::composer_width;
/// assert_eq!(composer_width(60), 54);
/// assert_eq!(composer_width(6), 0);
/// ```
#[must_use]
pub fn composer_width(panel_width: usize) -> usize {
    let panel = u16::try_from(panel_width).unwrap_or(u16::MAX);
    usize::from(interior_text_width(panel)).saturating_sub(PROMPT_COLS)
}

/// The cells one transcript row has, on a panel `panel_width` terminal
/// columns wide: the framed interior (see [`interior_text_width`]) less the
/// columns the framing's own item marker takes ([`LIST_MARKER_COLS`]) --
/// the transcript is painted as a list's items, and every item row opens
/// with one.
///
/// The transcript's own entry marker is spent out of what is left rather
/// than taken off here, because that marker is part of the row the
/// transcript renders, not chrome the frame adds around it.
///
/// ```
/// use view_core::native::ai_panel::transcript_width;
/// assert_eq!(transcript_width(60), 54);
/// ```
#[must_use]
pub fn transcript_width(panel_width: usize) -> usize {
    let panel = u16::try_from(panel_width).unwrap_or(u16::MAX);
    usize::from(interior_text_width(panel).saturating_sub(LIST_MARKER_COLS))
}

/// Where the next character typed lands in `rows` -- already-wrapped
/// composer rows -- as an index into them and a column across that row in
/// cells.
///
/// The one definition of the composer's insertion point, and what
/// `view-surface` places the real terminal caret at while the panel owns
/// input. The row is always the last one (the composer only appends and
/// backspaces, and the wrap's tail-keeping cut can never drop that row), the
/// column is always inside the width that row was wrapped to, and empty rows
/// answer the first cell -- where the panel's own empty prompt line is
/// painted, and where a prompt ending on a line break leaves the caret.
///
/// Takes the rows rather than the state so a frame can ask it of the rows it
/// actually painted ([`crate::native::views::AiPanelView::composer_cursor`]),
/// instead of re-wrapping the input a second time and placing a caret from a
/// derivation the painted panel never saw. A caller holding the state asks
/// [`AiPanelState::composer_rows`] for those rows first.
#[must_use]
pub fn composer_cursor_of(rows: &[String]) -> (usize, usize) {
    let column = rows
        .last()
        .map(|row| row.chars().map(char_cells).sum())
        .unwrap_or_default();
    (rows.len().saturating_sub(1), column)
}

/// One character's width in cells, the ASCII-doubling upper bound the
/// composer both wraps and places its cursor with -- see [`wrap`] for why
/// over-wide is the safe direction.
///
/// An upper bound, not the display width: accented Latin, Cyrillic and Greek
/// each measure two where a terminal paints one, so a row of them breaks a
/// few columns early. Nothing is lost or truncated by that -- the cost is
/// unused columns at the frame's edge -- and it is the same measure the
/// cursor is placed with, which is what keeps the caret on the column the
/// panel painted to.
fn char_cells(ch: char) -> usize {
    1 + usize::from(!ch.is_ascii())
}

/// The tail of `input` that the last `keep` rows of its wrap can be read
/// from, so one paint costs what the panel can paint rather than whatever
/// the clipboard held.
///
/// A paste puts an arbitrary amount of text into the composer in one
/// gesture, and [`wrap`] walks every character it is given on every frame:
/// past a few hundred kilobytes that walk alone is over the whole output
/// budget, spent on rows that were never going to be painted. Only the last
/// `keep` rows can be, a row holds at most `width` cells, and a cell is at
/// most four bytes of UTF-8, so `4 * width * (keep + 1)` bytes always hold
/// more than `keep` rows' worth of text -- one row spare, because the
/// window may open in the middle of one.
///
/// Its rows are the tail of the whole input's wrap, because it opens where
/// that wrap opens a row: the start of the line the last break before the
/// window began, plus a whole number of `width`-cell rows into it -- a
/// grid, never a fixed distance from the end, so appending a character does
/// not slide the window and reflow a row the reader has already read.
///
/// A row of that grid is `width` bytes because a cell is one byte, which
/// holds for the ASCII a composer is overwhelmingly typed in and not for a
/// line of multibyte characters longer than the window. Finding a row
/// boundary in such a line means measuring cells from the line's start,
/// because a row is greedy -- one that cannot fit the next two-cell glyph
/// ends a cell early -- so where its rows fall is a function of the whole
/// line and not of any count. That measurement is the walk, and the line
/// is the bound on it: the window such a stretch falls back to is the
/// line, which for a prompt of one line is the prompt.
///
/// So the return is `4 * width * (keep + 1)` bytes for ASCII and for
/// multibyte text under a line break, and the line itself for a single
/// line carrying a character wider than a byte. The alternative is rows
/// that sit a column off the wrap the transcript will give the same text,
/// re-flowing as the next character is typed -- see [`AiPanelState`]'s
/// `non_ascii`, which exists to keep the grid off exactly that stretch.
///
/// `breaks` is [`AiPanelState::breaks`], every line break in ascending
/// order, binary-searched for the last one at or before the window's own
/// start. The offset it lands on is checked against the text rather than
/// trusted, so a break list that has drifted from the input costs alignment
/// and never correctness. The window never opens later than the span above
/// allows, so it always holds more rows than the panel paints.
fn wrap_window<'a>(
    input: &'a str,
    width: usize,
    keep: usize,
    breaks: &[usize],
    non_ascii: Option<(usize, usize)>,
) -> &'a str {
    let span = keep
        .saturating_add(1)
        .saturating_mul(width)
        .saturating_mul(BYTES_PER_CELL);
    let floor = input.len().saturating_sub(span);
    let bytes = input.as_bytes();
    let phase = breaks[..breaks.partition_point(|&at| at <= floor)]
        .last()
        .filter(|&&at| matches!(bytes.get(at), Some(b'\n' | b'\r')))
        .map_or(0, |&at| {
            // `\r\n` is one break and the row after it opens past both of
            // its bytes: an opening on the `\n` is read as a second break
            // and paints an empty row the input never had
            let crlf = bytes.get(at) == Some(&b'\r') && bytes.get(at + 1) == Some(&b'\n');
            at + 1 + usize::from(crlf)
        });
    // saturating because a break landing exactly on `floor` opens the next
    // line one byte past it, which is a whole row nearer the end and never
    // fewer rows than the panel paints
    let mut start = phase + (floor.saturating_sub(phase) / width) * width;
    // forward to a boundary, never back: a start inside a character would
    // panic the slice, and the row spare above is what pays for the shift
    while !input.is_char_boundary(start) {
        start += 1;
    }
    // The grid above counts a row as `width` bytes, which is `width` cells
    // only while every character it steps over is ASCII. One that is not
    // slides the opening off the row boundary it is defined to land on, and
    // the composer then paints rows the whole input's wrap never had. The
    // line's own start is the nearest opening that is provably a boundary
    // whatever the text holds, so a stretch the grid is not exact for is
    // wrapped from there instead: the line, which for a prompt of one line
    // is the prompt.
    if non_ascii.is_some_and(|(first, last)| first < start && last >= phase) {
        return &input[phase..];
    }
    &input[start..]
}

/// The widest UTF-8 encoding of one character, which is also the most bytes
/// one terminal cell of composer text can cost -- see [`wrap_window`].
const BYTES_PER_CELL: usize = 4;

/// Where a row that runs out of width breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Break {
    /// At the cell the width runs out on, the way a terminal wraps a long
    /// line.
    Cell,
    /// At the last space before it, moving the partial word down with the
    /// break. A stretch with no space in it still breaks at the cell --
    /// there is nowhere else to put it.
    Word,
}

/// The last `keep` of the rows `input` breaks into at `width` cells each,
/// in order, always at least one row.
///
/// [`Break::Cell`] for the composer, the way a terminal wraps a long line:
/// the row and column the next character lands on are then derivable from
/// what has been typed alone, and no keystroke reflows a row the user has
/// already read -- both of which a greedy word wrap gives up, for a
/// composer that only ever appends and backspaces.
///
/// The transcript echoes the user's own prompt through the same
/// [`Break::Cell`], so the two halves of the panel measure that text the
/// same way and put a wrap break in the same column: a typed prompt reads
/// the same after it is sent as it did while it was being typed. Prose the
/// user never typed has no composer twin to stay in step with, so it takes
/// [`Break::Word`] and reads as prose.
///
/// A line break ends a row wherever it falls, so a pasted multi-line prompt
/// reads in the composer as the lines it was copied as, and reads the same
/// again once sent. All three endings break: `\n`, `\r\n` as the one break
/// it is, and a lone `\r` -- which is not a line ending in a file but is
/// what a terminal hands a paste through tmux's own buffer. The break is the
/// row's end and never a cell of it, so a text ending on one ends on an
/// empty row, which is where the next character goes.
///
/// Cells are the ASCII-doubling upper bound this crate measures text with:
/// one per ASCII character, two for anything else. Over-wide leaves a
/// column unused at the frame's edge; under-wide would push a glyph past
/// it, which is the failure that loses text.
///
/// `keep` bounds the allocation, not only the result: a row scrolling off
/// the top is emptied and reused as the row opening at the bottom, so a
/// prompt thousands of characters long costs `keep` strings per frame
/// instead of one per row it would have had. It still walks the whole
/// input, because where the breaks fall is what decides which rows the
/// last ones are.
fn wrap(input: &str, width: usize, keep: usize, breaks: Break) -> Vec<String> {
    let mut rows: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    rows.push_back(String::new());
    let mut used = 0_usize;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' || ch == '\r' {
            // the `\n` of a `\r\n` is the same break as its `\r`
            if ch == '\r' {
                let _ = chars.next_if_eq(&'\n');
            }
            open_row(&mut rows, keep);
            used = 0;
            continue;
        }
        let cells = char_cells(ch);
        // `used > 0` keeps a glyph wider than the whole row on a row of its
        // own instead of opening an empty one ahead of it
        if used > 0 && used + cells > width {
            // a space landing on the break *is* the break: kept, it would
            // open the next row on whitespace the reader cannot see and
            // cannot delete
            if breaks == Break::Word && ch == ' ' {
                open_row(&mut rows, keep);
                used = 0;
                continue;
            }
            let carried = (breaks == Break::Word)
                .then(|| rows.back_mut().and_then(take_partial_word))
                .flatten();
            open_row(&mut rows, keep);
            used = 0;
            if let (Some(word), Some(row)) = (carried, rows.back_mut()) {
                used = word.chars().map(char_cells).sum();
                row.push_str(&word);
            }
        }
        if let Some(row) = rows.back_mut() {
            row.push(ch);
        }
        used += cells;
    }
    rows.into()
}

/// Takes the unfinished word off the end of `row`, or `None` when the row
/// is one word with nowhere to break -- which is the case that falls back
/// to the cell break.
///
/// The space goes with it: it is the break, and left behind it would be a
/// cell of trailing whitespace on a row nothing else can reach.
fn take_partial_word(row: &mut String) -> Option<String> {
    let space = row.rfind(' ')?;
    let word = (space + 1 < row.len()).then(|| row.split_off(space + 1))?;
    row.pop();
    Some(word)
}

/// Opens [`wrap`]'s next row, reusing the string of the row falling off the
/// top once `keep` of them are held -- the allocation bound `wrap`'s own doc
/// promises, in the one place both a width break and a line break go
/// through.
fn open_row(rows: &mut std::collections::VecDeque<String>, keep: usize) {
    let mut next = if rows.len() >= keep {
        rows.pop_front().unwrap_or_default()
    } else {
        String::new()
    };
    next.clear();
    rows.push_back(next);
}

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
/// the one state where the prompt is visible but its own keys all reach
/// the engine instead of it.
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

    use std::fmt::Write as _;

    use super::super::ai_event::ToolCallStatus;
    use super::super::views::{Span, StyleRole};
    use super::*;

    /// A row budget larger than any transcript these tests build; what the
    /// budget itself does is pinned in [`Transcript::rows_from`]'s own
    /// tests.
    const ROOM: usize = 1_000;

    /// A panel height whose transcript window works out to ten rows, so the
    /// scrolling tests below can name the exact lines a page moves by.
    const TEN_ROW_PANEL: usize = 10 + CHROME_ROWS;

    /// A panel wide enough that none of the transcript tests below wrap
    /// their composer, so the row budget they name is the one they get.
    const WIDE_PANEL: usize = 60;

    /// A panel narrow enough that a `line NN` entry wraps onto two rows, so
    /// a window held at this width names a row [`WIDE_PANEL`] does not have.
    const NARROW_PANEL: usize = 10;

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
        transcript_texts_at(state, panel_height, WIDE_PANEL)
    }

    fn transcript_texts_at(
        state: &AiPanelState,
        panel_height: usize,
        panel_width: usize,
    ) -> Vec<String> {
        state
            .view(panel_height, panel_width)
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
        assert_eq!(texts.last().unwrap(), "● line 49");
        assert_eq!(texts.first().unwrap(), "● line 40");
    }

    /// Scrolling back stops the panel following, and an agent that keeps
    /// talking must not drag the window along behind it -- a reader who has
    /// to chase the text they are reading cannot read it.
    #[test]
    fn a_scrolled_back_window_holds_while_the_agent_keeps_talking() {
        let mut state = panel_with_lines(50);
        assert!(state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL, WIDE_PANEL));

        let held = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(held.first().unwrap(), "● line 31");

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

    /// A window held while the panel was narrow opens on the entry the
    /// reader was reading once the panel is widened. The row it held names
    /// a place in a wrap the wider panel does not have, and a window that
    /// took that count literally would drop the entry off its own top --
    /// scrolling the reader without a scroll key.
    #[test]
    fn a_window_held_on_a_narrow_panel_keeps_its_entry_when_the_panel_widens() {
        let mut state = panel_with_lines(50);
        // far enough back that the window is still behind the row the
        // wider panel's own tail starts at -- a window that has caught up
        // with the tail follows again and would pass this whatever it held
        for _ in 0..8 {
            assert!(state.scroll_transcript(
                TranscriptScroll::PageBack,
                TEN_ROW_PANEL,
                NARROW_PANEL
            ));
        }

        // four cells of text a row, so the narrow window opens part way
        // down an entry and its first row is that entry's tail fragment --
        // which is the end of the line the widened window must open on
        let narrow = transcript_texts_at(&state, TEN_ROW_PANEL, NARROW_PANEL);
        let tail: String = narrow
            .iter()
            .take_while(|row| !row.starts_with('\u{25cf}'))
            .map(|row| row.trim())
            .collect();
        let widened = transcript_texts(&state, TEN_ROW_PANEL);

        assert!(
            !tail.is_empty(),
            "the narrow window must open part way down an entry for this to \
             have a subject: {narrow:?}"
        );
        assert!(
            widened.first().is_some_and(|row| row.ends_with(&tail)),
            "the widened window opens on the entry the narrow one did: \
             {narrow:?} became {widened:?}"
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
            "● line 49",
            "a following panel has nothing below it to point at"
        );

        state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL, WIDE_PANEL);

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
        state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL, WIDE_PANEL);

        assert!(state.scroll_transcript(TranscriptScroll::PageForward, TEN_ROW_PANEL, WIDE_PANEL));
        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL).last().unwrap(),
            "● line 49"
        );

        state
            .transcript
            .append_or_extend(Some("m50"), "line 50", TranscriptRole::Agent);
        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL).last().unwrap(),
            "● line 50",
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
        assert_eq!(following.first().unwrap(), "● line 40");

        state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL, WIDE_PANEL);
        let held = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(
            held.last().unwrap(),
            MORE_BELOW,
            "the last row is the marker, so nine transcript rows precede it"
        );
        assert_eq!(
            held[..held.len() - 1].last().unwrap(),
            "● line 39",
            "the page must stop on the line directly above the one it came from"
        );

        state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL, WIDE_PANEL);
        let further = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(
            further[..further.len() - 1].last().unwrap(),
            "● line 30",
            "and so must the next one"
        );

        state.scroll_transcript(TranscriptScroll::PageForward, TEN_ROW_PANEL, WIDE_PANEL);
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
        state.scroll_transcript(TranscriptScroll::HalfPageBack, TEN_ROW_PANEL, WIDE_PANEL);

        let held = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(held.first().unwrap(), "● line 35");
        assert_eq!(held[..held.len() - 1].last().unwrap(), "● line 43");
    }

    /// A window held on a short panel can sit at or past the tail once the
    /// terminal grows. Painting it as held would put a "more below" marker
    /// over nothing, so the anchor is re-checked against the viewport of
    /// the frame being painted rather than the one it was set against.
    #[test]
    fn a_window_the_panel_grew_past_stops_claiming_there_is_more_below() {
        let mut state = panel_with_lines(20);
        state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL, WIDE_PANEL);
        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL).last().unwrap(),
            MORE_BELOW
        );

        let grown = 20 + CHROME_ROWS;
        let texts = transcript_texts(&state, grown);
        assert_eq!(texts.last().unwrap(), "● line 19");
        assert!(
            !texts.iter().any(|row| row == MORE_BELOW),
            "the whole transcript fits now, so nothing is below it: {texts:?}"
        );
        assert!(
            !state.scroll_transcript(TranscriptScroll::PageForward, grown, WIDE_PANEL),
            "and the key that follows the tail again finds it already there"
        );
    }

    /// A transcript with nothing above what is already on screen has no
    /// scroll to hold: pinning it would stop the panel following for a
    /// window indistinguishable from the one it already had.
    #[test]
    fn a_transcript_shorter_than_the_panel_keeps_following() {
        let mut state = panel_with_lines(3);

        assert!(!state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL, WIDE_PANEL));

        state
            .transcript
            .append_or_extend(Some("m3"), "line 3", TranscriptRole::Agent);
        assert_eq!(
            transcript_texts(&state, TEN_ROW_PANEL).last().unwrap(),
            "● line 3"
        );
    }

    /// The accounting row and the crash banner both come out of the same
    /// rows the transcript would have had, so a window sized without them
    /// would page past lines the panel never drew.
    #[test]
    fn the_transcript_window_gives_up_a_row_to_the_usage_line_and_the_banner() {
        let mut state = AiPanelState::new();
        assert_eq!(state.transcript_viewport(TEN_ROW_PANEL, WIDE_PANEL), 10);

        state.usage = Some(UsageStats {
            used: 1,
            size: 2,
            cost: None,
        });
        assert_eq!(state.transcript_viewport(TEN_ROW_PANEL, WIDE_PANEL), 9);

        state.local_error = Some("gone".to_string());
        assert_eq!(state.transcript_viewport(TEN_ROW_PANEL, WIDE_PANEL), 8);
    }

    /// The reported complaint, as an assertion: past the panel's width the
    /// prompt stopped showing, so the user was typing into a row nothing
    /// painted. Every character typed is now on a row, and the tail is on
    /// the last of them.
    #[test]
    fn an_input_past_the_panels_width_keeps_every_character_and_its_tail() {
        let width = composer_width(WIDE_PANEL);
        let typed: String = (0..width * 3 + 7)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let mut state = AiPanelState::new();
        state.input.clone_from(&typed);

        let rows = state.view(ROOM, WIDE_PANEL).input;

        assert_eq!(rows.concat(), typed, "no character is dropped by the wrap");
        assert!(rows.len() > 1, "a prompt past the width takes more rows");
        assert!(
            rows.iter().all(|row| row.chars().count() <= width),
            "no row is wider than the panel paints: {rows:?}"
        );
        assert!(
            rows.last()
                .is_some_and(|last| typed.ends_with(last.as_str())),
            "the last row holds the tail: {rows:?}"
        );
        assert_eq!(
            composer_cursor_of(&state.composer_rows(ROOM, WIDE_PANEL)),
            (3, 7),
            "the cursor is past the tail"
        );
    }

    /// A pasted line break ends the composer's row, so a multi-line prompt
    /// reads as the lines it was copied as -- and reads the same again in
    /// the transcript once it is sent. All three endings break, and a
    /// `\r\n` breaks once.
    #[test]
    fn a_pasted_line_break_ends_the_composers_row() {
        let rows = |input: &str| {
            let mut state = AiPanelState::new();
            state.input = input.to_string();
            state.view(ROOM, WIDE_PANEL).input
        };

        for (name, input) in [
            ("a bare newline", "ab\ncd"),
            ("a carriage return and newline", "ab\r\ncd"),
            ("a lone carriage return", "ab\rcd"),
        ] {
            assert_eq!(
                rows(input),
                vec!["ab".to_string(), "cd".to_string()],
                "{name} breaks the row once"
            );
        }
        assert_eq!(
            rows("ab\n\ncd"),
            vec!["ab".to_string(), String::new(), "cd".to_string()],
            "the blank line between two paragraphs keeps its row"
        );
    }

    /// The composer is where the next character goes, so a text ending on a
    /// line break ends on the empty row that character opens -- and the
    /// caret is on its first cell, not left on the line above.
    #[test]
    fn a_prompt_ending_on_a_line_break_puts_the_caret_on_the_empty_row_below() {
        let mut state = AiPanelState::new();
        state.input = "ab\n".to_string();

        assert_eq!(
            state.view(ROOM, WIDE_PANEL).input,
            vec!["ab".to_string(), String::new()]
        );
        assert_eq!(
            composer_cursor_of(&state.composer_rows(ROOM, WIDE_PANEL)),
            (1, 0)
        );

        state.input.push('c');
        assert_eq!(
            composer_cursor_of(&state.composer_rows(ROOM, WIDE_PANEL)),
            (1, 1),
            "and the character lands on that row"
        );
    }

    /// A prompt of many short lines takes rows the same way a prompt of one
    /// long line does: the composer keeps the last of them and never grows
    /// past the share of the panel it may take, whichever kind of break
    /// made the rows.
    #[test]
    fn a_multi_line_prompt_is_capped_at_the_composers_share_of_the_panel() {
        let cap = AiPanelState::new().composer_cap(TEN_ROW_PANEL);
        let mut state = AiPanelState::new();
        state.input = (0..cap * 4).fold(String::new(), |mut acc, i| {
            let _ = writeln!(acc, "line {i}");
            acc
        });

        let rows = state.view(TEN_ROW_PANEL, WIDE_PANEL).input;

        assert_eq!(rows.len(), cap, "the composer keeps its cap: {rows:?}");
        assert_eq!(
            rows.first().map(String::as_str),
            Some(format!("line {}", cap * 4 - cap + 1).as_str()),
            "and the rows it kept are the newest ones: {rows:?}"
        );
        assert_eq!(
            composer_cursor_of(&state.composer_rows(TEN_ROW_PANEL, WIDE_PANEL)),
            (cap - 1, 0),
            "with the caret on the empty row the trailing break opened"
        );
        assert!(
            state.transcript_viewport(TEN_ROW_PANEL, WIDE_PANEL) >= cap,
            "and the transcript still has at least as many rows as the \
             composer took"
        );
    }

    /// A line break is the row's end and never a cell of it, so the width a
    /// row is broken at is the width the panel paints -- a break carried as
    /// a cell would have the cursor a column past every line.
    #[test]
    fn a_line_break_costs_the_row_no_column() {
        let width = composer_width(WIDE_PANEL);
        let mut state = AiPanelState::new();
        state.input = format!("{}\ny", "x".repeat(width));

        let rows = state.composer_rows(ROOM, WIDE_PANEL);

        assert_eq!(rows, vec!["x".repeat(width), "y".to_string()]);
        assert_eq!(composer_cursor_of(&rows), (1, 1));
    }

    /// One paint costs what the panel can paint, never what was on the
    /// clipboard. A megabyte pasted into the composer is a megabyte walked
    /// on every frame if the wrap is handed all of it -- milliseconds, for
    /// rows nothing was ever going to draw -- so the wrap is handed the tail
    /// the visible rows come out of instead, and the rows that come back
    /// are the ones the whole input would have produced: a window opening
    /// anywhere but a row boundary reflows every row the reader has already
    /// read, and a prompt would change shape the moment it was sent.
    ///
    /// The three shapes the window has to open on a boundary of: no break
    /// at all, where a multiple of the width is one; many breaks, where the
    /// last of them moves every boundary off those multiples; and one line
    /// longer than the window with a break before it, which is the shape
    /// the window used to sit a column off on.
    #[test]
    fn the_window_wraps_to_the_tail_of_the_whole_inputs_wrap() {
        let width = composer_width(WIDE_PANEL);
        let cap = AiPanelState::new().composer_cap(TEN_ROW_PANEL);
        let letters = |n: usize| -> String {
            (0..n)
                .map(|i: usize| char::from(b'a' + (i % 26) as u8))
                .collect()
        };
        let mut many_lines = String::new();
        for i in 0..2_000 {
            let _ = writeln!(
                many_lines,
                "line {i} of a prompt pasted from somewhere else"
            );
        }

        for (name, input) in [
            ("a megabyte with no break in it", letters(1 << 20)),
            ("two thousand short lines", many_lines),
            (
                "one line longer than the window, after a break",
                format!("first\n{}", letters(1 << 14)),
            ),
            (
                "a short last line under a line longer than the window",
                format!("first\n{}\nshort", letters(1 << 14)),
            ),
        ] {
            let mut state = AiPanelState::new();
            state.push_input(&input);

            let rows = state.view(TEN_ROW_PANEL, WIDE_PANEL).input;

            assert_eq!(
                rows,
                wrap(&input, width, cap, Break::Cell),
                "{name}: the rows are the whole input's own, column for column"
            );
            let window = wrap_window(&input, width, cap, &state.breaks, state.non_ascii);
            assert!(
                window.len() <= (cap + 2) * width * BYTES_PER_CELL,
                "{name}: the walk is bounded by the rows the panel has, not \
                 by the {} bytes composed: {} bytes",
                input.len(),
                window.len()
            );
        }
    }

    /// The cache the window opens on, across every gesture that moves it: a
    /// paste scans only what it pasted, a typed break lands at the end, a
    /// backspace over a break drops the last one, and a submitted prompt
    /// leaves none.
    #[test]
    fn the_breaks_follow_the_composers_text() {
        let mut state = AiPanelState::new();
        state.push_input("no break here");
        assert!(state.breaks.is_empty());

        state.push_input(" and\nthen one\r\nmore");
        assert_eq!(
            state.breaks,
            every_break(state.input()),
            "a paste records each of its own breaks, `\\r\\n` as the two \
             bytes it is"
        );

        state.push_input("\n");
        assert_eq!(
            state.breaks.last().copied(),
            Some(state.input().len() - 1),
            "a typed break is the last one"
        );

        assert!(state.pop_input());
        assert_eq!(
            state.breaks,
            every_break(state.input()),
            "and removing it drops exactly that one"
        );
        let unchanged = state.breaks.clone();
        assert!(state.pop_input());
        assert_eq!(
            state.breaks, unchanged,
            "a backspace over an ordinary character drops none"
        );

        assert!(!state.take_input().is_empty());
        assert!(state.breaks.is_empty(), "and a sent prompt leaves none");
        assert!(!state.pop_input(), "an empty composer has nothing to pop");
    }

    /// Every line break in `text`, the scan the cache exists to avoid --
    /// here as the independent answer to check it against.
    fn every_break(text: &str) -> Vec<usize> {
        text.match_indices(['\n', '\r']).map(|(at, _)| at).collect()
    }

    /// The guard that makes the cache safe to be wrong: a break list that no
    /// longer describes the text names an offset that is not a break, and
    /// the window opens the way it does for a text with no break at all
    /// rather than a row boundary of nothing.
    #[test]
    fn a_break_list_that_no_longer_describes_the_text_costs_only_alignment() {
        let width = composer_width(WIDE_PANEL);
        let cap = AiPanelState::new().composer_cap(TEN_ROW_PANEL);
        let letters: String = (0..(1 << 16))
            .map(|i: usize| char::from(b'a' + (i % 26) as u8))
            .collect();

        let mut state = AiPanelState::new();
        state.push_input("first\nsecond\n");
        state.input.clone_from(&letters);

        let rows = state.view(TEN_ROW_PANEL, WIDE_PANEL).input;

        assert_eq!(
            rows,
            wrap(&letters, width, cap, Break::Cell),
            "the rows are still the whole text's own"
        );
        assert!(
            wrap_window(&letters, width, cap, &state.breaks, state.non_ascii).len()
                <= (cap + 2) * width * BYTES_PER_CELL,
            "and the walk is still bounded"
        );
    }

    /// The wrap boundary itself, where an off-by-one puts the cursor on a
    /// row that is not there: a row exactly full keeps the cursor on its
    /// own last column, and the next character opens the next row.
    #[test]
    fn the_composers_cursor_tracks_the_input_across_the_wrap_boundary() {
        let width = composer_width(WIDE_PANEL);
        let mut state = AiPanelState::new();

        state.input = "x".repeat(width - 1);
        assert_eq!(
            composer_cursor_of(&state.composer_rows(ROOM, WIDE_PANEL)),
            (0, width - 1)
        );

        state.input = "x".repeat(width);
        assert_eq!(
            composer_cursor_of(&state.composer_rows(ROOM, WIDE_PANEL)),
            (0, width),
            "a row exactly full is still one row"
        );

        state.input = "x".repeat(width + 1);
        assert_eq!(
            composer_cursor_of(&state.composer_rows(ROOM, WIDE_PANEL)),
            (1, 1),
            "the character past it opens the next row"
        );
    }

    /// A wide glyph is measured as two cells on the way in, so a row of
    /// them breaks at half the count rather than painting past the frame's
    /// right edge. Over-wide is the safe direction and the one this crate
    /// takes everywhere it measures text without a width table.
    #[test]
    fn a_wide_glyph_composer_row_never_measures_past_the_frame() {
        let width = composer_width(WIDE_PANEL);
        let mut state = AiPanelState::new();
        state.input = "界".repeat(width);

        let rows = state.view(ROOM, WIDE_PANEL).input;

        assert_eq!(rows.concat(), state.input, "no glyph is dropped");
        for row in &rows {
            let cells: usize = row.chars().map(char_cells).sum();
            assert!(cells <= width, "a row measured {cells} cells over {width}");
        }
    }

    /// A prompt longer than the panel can hold, for a test that cares only
    /// that the composer is over its ceiling.
    fn overlong_prompt(width: usize, height: usize) -> String {
        // a repeating pattern rather than one character, so a row carrying
        // anything but its own share of the input is visible in the joined
        // text rather than hidden by every character matching
        (0..width.max(1) * height.max(1) * 4)
            .map(|i| char::from(b'a' + u8::try_from(i % 26).unwrap_or(0)))
            .collect()
    }

    /// The composer's own ceiling: a prompt long enough to fill the panel
    /// takes at most half the rows the frame leaves it and scrolls inside
    /// those, so the tail stays last.
    #[test]
    fn a_very_long_prompt_stops_at_half_the_panel_and_scrolls_inside_it() {
        let width = composer_width(WIDE_PANEL);
        let mut state = AiPanelState::new();
        state.input = overlong_prompt(width, TEN_ROW_PANEL);

        let rows = state.view(TEN_ROW_PANEL, WIDE_PANEL).input;

        assert_eq!(
            rows.len(),
            (TEN_ROW_PANEL - CHROME_ROWS) / 2,
            "capped at half the rows the frame leaves the panel"
        );
        assert!(
            state.input.ends_with(&rows.concat()),
            "the rows kept are the last ones, so the cursor is still on screen"
        );
        assert!(
            rows.iter().all(|row| row.chars().count() == width),
            "and each of them is a full row of the wrap: {rows:?}"
        );
    }

    /// The other half of that ceiling, at the heights where getting it
    /// wrong costs the whole transcript: the composer never takes more rows
    /// than it leaves, so a short panel with a long prompt on it still
    /// shows the agent talking.
    ///
    /// Run against every combination of the rows that also come out of the
    /// transcript's share. A cap budgeting over rows the accounting row and
    /// the crash banner have already spent hands the composer a share the
    /// transcript cannot match -- and a session that has finished one turn
    /// and then lost its agent is showing both of them.
    #[test]
    fn a_maxed_composer_never_leaves_the_transcript_fewer_rows_than_itself() {
        let width = composer_width(WIDE_PANEL);
        for height in [6_usize, 8, 10, 12, 14, 40] {
            for (usage, error) in [(false, false), (true, false), (false, true), (true, true)] {
                let mut state = AiPanelState::new();
                state.input = overlong_prompt(width, height);
                if usage {
                    state.usage = Some(UsageStats {
                        used: 1,
                        size: 2,
                        cost: None,
                    });
                }
                if error {
                    state.local_error = Some("gone".to_string());
                }

                let composer = state.view(height, WIDE_PANEL).input.len();
                let transcript = state.transcript_viewport(height, WIDE_PANEL);
                let banners = usize::from(usage) + usize::from(error);
                let banner_note = format!("usage={usage} error={error}");

                if height <= CHROME_ROWS + banners {
                    assert_eq!(
                        (composer, transcript),
                        (1, 0),
                        "a {height}-row panel with no room past its chrome and \
                         banners ({banner_note}) still paints the prompt row"
                    );
                    continue;
                }

                assert!(
                    transcript > 0,
                    "a {height}-row panel ({banner_note}) keeps transcript \
                     rows: {composer} composer"
                );
                assert!(
                    transcript >= composer,
                    "a {height}-row panel ({banner_note}) gives the transcript \
                     at least the composer's share: {transcript} against \
                     {composer}"
                );
            }
        }
    }

    /// A panel narrowed past the frame's own chrome can paint no prompt
    /// character at all, so the composer stays one row instead of growing
    /// to its cap and spending the transcript's rows on rows that all clip
    /// to the bare prompt mark.
    #[test]
    fn a_panel_too_narrow_to_paint_a_prompt_character_never_grows_its_composer() {
        for panel_width in [0_usize, 1, 2, 3, 4, 6] {
            assert_eq!(
                composer_width(panel_width),
                0,
                "a {panel_width}-wide panel has no room for prompt text"
            );

            let mut state = AiPanelState::new();
            state.input = "z".repeat(200);

            assert_eq!(
                state.view(TEN_ROW_PANEL, panel_width).input.len(),
                1,
                "a {panel_width}-wide panel keeps its composer to one row"
            );
            assert_eq!(
                state.transcript_viewport(TEN_ROW_PANEL, panel_width),
                TEN_ROW_PANEL - CHROME_ROWS,
                "so the transcript keeps every row it had"
            );
        }
    }

    /// A composer that grew takes its rows from the transcript's window,
    /// not from the panel's frame: a window still counting them would have
    /// its oldest rows cut by the overlay instead, which is a scroll
    /// nobody asked for.
    #[test]
    fn a_grown_composer_costs_the_transcript_window_its_rows() {
        let width = composer_width(WIDE_PANEL);
        let mut state = panel_with_lines(50);
        assert_eq!(state.transcript_viewport(TEN_ROW_PANEL, WIDE_PANEL), 10);

        state.input = "z".repeat(width * 2 + 1);
        assert_eq!(
            state.transcript_viewport(TEN_ROW_PANEL, WIDE_PANEL),
            8,
            "three composer rows cost the two the frame did not already count"
        );

        let texts = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(texts.len(), 8);
        assert_eq!(
            texts.last().unwrap(),
            "● line 49",
            "a following panel still ends on its newest line"
        );
    }

    /// A held window is the user's own position, and a composer growing
    /// under it must not move it: the rows it drops are the newest, the
    /// same end a shorter panel drops, never the top the reader is on.
    #[test]
    fn a_held_window_does_not_jump_when_the_composer_grows() {
        let width = composer_width(WIDE_PANEL);
        let mut state = panel_with_lines(50);
        assert!(state.scroll_transcript(TranscriptScroll::PageBack, TEN_ROW_PANEL, WIDE_PANEL));
        let held_top = transcript_texts(&state, TEN_ROW_PANEL)
            .first()
            .cloned()
            .unwrap();

        state.input = "z".repeat(width * 2 + 1);
        let grown = transcript_texts(&state, TEN_ROW_PANEL);

        assert_eq!(
            grown.first().unwrap(),
            &held_top,
            "the row the reader is on is still the window's first"
        );
        assert_eq!(grown.len(), 8, "the window shrank by the composer's rows");
        assert_eq!(
            grown.last().unwrap(),
            MORE_BELOW,
            "and still says it is held"
        );
    }

    /// The composer shrinking gives the rows back, so a cleared prompt
    /// leaves the transcript exactly the window it had before anything was
    /// typed.
    #[test]
    fn clearing_the_composer_gives_the_transcript_its_rows_back() {
        let width = composer_width(WIDE_PANEL);
        let mut state = panel_with_lines(50);
        let before = transcript_texts(&state, TEN_ROW_PANEL);

        state.input = "z".repeat(width * 2 + 1);
        state.input.clear();

        assert_eq!(transcript_texts(&state, TEN_ROW_PANEL), before);
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
        let view = state.view(ROOM, WIDE_PANEL);
        assert_eq!(view.title, TITLE);
        assert_eq!(view.input, vec!["hello".to_string()]);
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
        let view = state.view(ROOM, WIDE_PANEL);
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
            state.view(ROOM, WIDE_PANEL).local_error,
            vec![vec![Span::plain(format!(
                "Error: gone -- {DISMISS_KEY_HINT}"
            ))]]
        );
        state.focused = false;
        assert_eq!(
            state.view(ROOM, WIDE_PANEL).local_error,
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
        assert_eq!(state.view(ROOM, WIDE_PANEL).title, TITLE);
        state.focused = true;
        let view = state.view(ROOM, WIDE_PANEL);
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
    fn a_pending_permission_renders_the_question_its_keyed_options_and_the_hint() {
        let mut state = AiPanelState::new();
        state.focused = true;
        state.pending_permission = Some(PermissionPrompt::new(
            1,
            "call_1",
            Some("Delete config.yaml".to_string()),
            Some("edit".to_string()),
            vec![crate::native::ai_event::PermissionOption {
                option_id: "allow-once".to_string(),
                name: "Allow once".to_string(),
                kind: crate::native::ai_event::PermissionOptionKind::AllowOnce,
            }],
        ));
        let view = state.view(ROOM, WIDE_PANEL);
        assert_eq!(
            view.pending_permission,
            vec![
                vec![Span::new(
                    "Permission requested for Delete config.yaml",
                    StyleRole::AiPermissionAsk
                )],
                vec![Span::new(
                    "  1 Allow once (allow_once)",
                    StyleRole::AiPermissionAllow
                )],
                vec![Span::new(permission::KEY_HINT, StyleRole::AiPermissionAsk)],
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
            Some("edit".to_string()),
            vec![crate::native::ai_event::PermissionOption {
                option_id: "allow-once".to_string(),
                name: "Allow once".to_string(),
                kind: crate::native::ai_event::PermissionOptionKind::AllowOnce,
            }],
        ));
        let view = state.view(ROOM, WIDE_PANEL);
        assert_eq!(
            view.pending_permission.last(),
            Some(&vec![Span::plain(ENTER_HINT)]),
            "an unanswerable prompt must name the way in: {:?}",
            view.pending_permission
        );
    }

    /// The mirror case: once the user has entered the panel, the option
    /// digits already reach the prompt, so the hint would be stale advice and must
    /// not be drawn.
    #[test]
    fn a_pending_permission_on_a_focused_panel_carries_no_enter_hint() {
        let mut state = AiPanelState::new();
        state.focused = true;
        state.pending_permission = Some(PermissionPrompt::new(
            1,
            "call_1",
            Some("Delete config.yaml".to_string()),
            Some("edit".to_string()),
            vec![crate::native::ai_event::PermissionOption {
                option_id: "allow-once".to_string(),
                name: "Allow once".to_string(),
                kind: crate::native::ai_event::PermissionOptionKind::AllowOnce,
            }],
        ));
        let view = state.view(ROOM, WIDE_PANEL);
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
            state.view(ROOM, WIDE_PANEL).usage.is_empty(),
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
            state.view(ROOM, WIDE_PANEL).usage,
            vec![vec![Span::plain("context 100/1000, cost 0.05 USD")]]
        );
    }

    /// Who spoke is carried by the marker glyph and the row's style role,
    /// and by nothing else: a word prefix on every line spends the start of
    /// the row restating what a reader learns from the color once. The
    /// roles are the assertion, not decoration -- two voices painted in one
    /// role is the same panel the prefixes were removed from, minus the
    /// only thing that told them apart.
    #[test]
    fn each_voice_opens_with_its_own_marker_and_style_role() {
        let mut state = AiPanelState::new();
        state
            .transcript
            .append_or_extend(Some("1"), "hi", TranscriptRole::User);
        state
            .transcript
            .append_or_extend(Some("2"), "hello", TranscriptRole::Agent);
        state
            .transcript
            .append_or_extend(Some("3"), "weighing it", TranscriptRole::Thought);
        let view = state.view(ROOM, WIDE_PANEL);

        assert_eq!(view.rows.len(), 3);
        let roles: Vec<Vec<StyleRole>> = view
            .rows
            .iter()
            .map(|row| row.iter().map(|span| span.role).collect())
            .collect();
        assert_eq!(
            roles,
            vec![
                vec![StyleRole::AiUser, StyleRole::AiUser],
                vec![StyleRole::AiAgent, StyleRole::AiAgent],
                vec![StyleRole::AiThought, StyleRole::AiThought],
            ]
        );
        let texts: Vec<String> = view
            .rows
            .iter()
            .map(|row| row.iter().map(|span| span.text.as_str()).collect())
            .collect();
        assert_eq!(texts, vec!["❯ hi", "● hello", "◦ weighing it"]);
        for text in &texts {
            for word in ["You", "Agent", "Thinking"] {
                assert!(
                    !text.contains(word),
                    "no speaker word belongs on a transcript row: {text:?}"
                );
            }
        }
    }

    /// A tool call says how it went in a glyph and its color, on the same
    /// terms the message rows do.
    #[test]
    fn a_tool_call_marks_its_status_with_a_glyph_rather_than_a_word() {
        for (status, mark, role) in [
            (ToolCallStatus::Pending, "· ", StyleRole::AiToolRunning),
            (ToolCallStatus::Completed, "✓ ", StyleRole::AiToolDone),
            (ToolCallStatus::Failed, "✗ ", StyleRole::AiToolFailed),
        ] {
            let mut state = AiPanelState::new();
            state.transcript.upsert_tool_call(
                "call_1".to_string(),
                "Read file".to_string(),
                status,
                None,
            );
            let view = state.view(ROOM, WIDE_PANEL);
            assert_eq!(
                view.rows,
                vec![vec![
                    Span::new(mark, role),
                    Span::plain("Read file".to_string()),
                ]],
                "{status:?} must render as its glyph, in its own role"
            );
        }
    }

    /// The running state is the one that moves: the marker cycles through
    /// the braille frames while the call is unresolved, and the row it sits
    /// on is the only row a tick changes.
    #[test]
    fn a_running_tool_calls_marker_advances_on_a_tick_and_stops_when_it_resolves() {
        let mut state = AiPanelState::new();
        state
            .transcript
            .append_or_extend(Some("m1"), "before", TranscriptRole::Agent);
        state.transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        assert!(
            state.transcript.is_spinning(),
            "a call in flight is what buys the panel a clock"
        );

        let first = state.view(ROOM, WIDE_PANEL).rows;
        state.transcript.advance_spinner();
        let second = state.view(ROOM, WIDE_PANEL).rows;

        assert_eq!(
            first[0], second[0],
            "a spinner frame must not repaint the message above it"
        );
        assert_ne!(first[1][0], second[1][0], "the marker moved");
        assert_eq!(
            first[1][1], second[1][1],
            "and nothing but the marker moved on its own row"
        );
        assert_eq!(second[1][0].role, StyleRole::AiToolRunning);

        state.transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Completed,
            None,
        );
        assert!(
            !state.transcript.is_spinning(),
            "a resolved call leaves nothing to animate"
        );
        assert_eq!(
            state.view(ROOM, WIDE_PANEL).rows[1][0],
            Span::new("✓ ", StyleRole::AiToolDone),
            "the last frame painted is the call's own outcome"
        );
    }

    /// The reported defect: the panel showed the agent's half of the
    /// conversation and never the user's, because nothing wrote a prompt
    /// into the transcript until an adapter replayed it, and the adapter
    /// view ships against does not.
    #[test]
    fn a_submitted_prompt_is_echoed_into_the_transcript_at_the_tail() {
        let mut state = panel_with_lines(50);
        state.transcript.echo_user_prompt("what changed here?");

        let texts = transcript_texts(&state, TEN_ROW_PANEL);
        assert_eq!(
            texts.last().unwrap(),
            "⠋ what changed here?",
            "a followed panel shows the prompt the moment it is submitted, \
             under the spinner it wears until the agent answers"
        );
    }

    /// The reported defect's other half: a prompt longer than the panel is
    /// wide kept only the columns one row had and dropped the rest, so a
    /// user who wrote a paragraph could never read back what they sent. It
    /// wraps to the panel instead -- every character on screen, no row past
    /// the frame's interior, and none of it dragging the frame open.
    #[test]
    fn a_long_echoed_prompt_wraps_inside_the_panel_rather_than_losing_its_tail() {
        let mut state = AiPanelState::new();
        let prompt = "word ".repeat(60);
        state.transcript.echo_user_prompt(&prompt);

        let rows = state.view(ROOM, 24).rows;
        // asked of the framing's own arithmetic rather than of the panel's
        // reading of it, so the bound stands independent of what the panel
        // decided a row's width was
        let interior = usize::from(interior_text_width(24) - LIST_MARKER_COLS);

        assert!(rows.len() > 1, "a prompt past one row wraps onto more");
        for row in &rows {
            // The marker glyph and the space after it are one display cell
            // each -- the columns the wrapped rows' own indent stands in --
            // so the row fits exactly when its body does. The body is
            // measured the way it was wrapped, which is the ASCII-doubling
            // upper bound `char_cells` is.
            let cells: usize = row
                .get(1)
                .into_iter()
                .flat_map(|span| span.text.chars())
                .map(char_cells)
                .sum();
            assert!(
                cells + PROMPT_COLS <= interior,
                "a row past the frame's interior: {row:?}"
            );
        }
        let painted: String = rows
            .iter()
            .filter_map(|row| row.get(1))
            .map(|span| span.text.as_str())
            .collect();
        assert_eq!(
            painted, prompt,
            "every character the user sent is on screen"
        );
    }

    /// The reported leftover: the same huge paste wrapped in two different
    /// columns depending on what had been typed into the composer before
    /// it, so a prompt re-flowed as it was composed and changed shape again
    /// in the transcript once it was sent. The composer's rows are the tail
    /// of the whole input's own wrap or they are wrong, and here the input
    /// carries a character costing more bytes than cells -- which is what put the
    /// window's byte grid off the row boundary it opens on.
    #[test]
    fn a_paste_after_a_typed_break_wraps_where_the_same_paste_alone_does() {
        let width = composer_width(WIDE_PANEL);
        let cap = AiPanelState::new().composer_cap(TEN_ROW_PANEL);
        let paste: String = std::iter::once('\u{754c}')
            .chain((0..(1 << 14)).map(|i: usize| char::from(b'a' + (i % 26) as u8)))
            .collect();

        let mut alone = AiPanelState::new();
        alone.push_input(&paste);

        let mut after_break = AiPanelState::new();
        after_break.push_input("first");
        after_break.push_input("\n");
        after_break.push_input(&paste);

        let rows_alone = alone.view(TEN_ROW_PANEL, WIDE_PANEL).input;
        assert_eq!(
            rows_alone,
            wrap(&paste, width, cap, Break::Cell),
            "the paste alone wraps as its own text does"
        );
        assert_eq!(
            after_break.view(TEN_ROW_PANEL, WIDE_PANEL).input,
            wrap(&format!("first\n{paste}"), width, cap, Break::Cell),
            "and the same paste under a typed break wraps as that whole \
             input does, in the same columns"
        );
        assert_eq!(
            rows_alone.last(),
            after_break.view(TEN_ROW_PANEL, WIDE_PANEL).input.last(),
            "which is the same last row either way"
        );
    }

    /// The window against the whole input's wrap over every shape a
    /// composer is handed: widths and heights across the panel sizes a
    /// frame is drawn at, and text mixing ASCII, accented Latin, a wide
    /// glyph and all three line endings, at lengths straddling the window's
    /// own span.
    ///
    /// A walk rather than the four named shapes above it: the opening this
    /// pins is a function of where the last break, the window's span and
    /// the first character wider than a byte fall relative to one another,
    /// and every combination of those three is a shape somebody pastes. The
    /// seed is fixed, so a failure is reproducible and a passing run is the
    /// same four thousand shapes every time.
    #[test]
    fn the_window_opens_on_a_row_boundary_whatever_the_composer_holds() {
        let mut seed = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet: Vec<char> = "ab \n\r\u{e9}\u{754c}c".chars().collect();
        for _ in 0..4_000 {
            let panel_w = 12 + (next() % 90) as usize;
            let panel_h = 4 + (next() % 30) as usize;
            let width = composer_width(panel_w).max(1);
            let cap = if composer_width(panel_w) == 0 {
                1
            } else {
                AiPanelState::new().composer_cap(panel_h)
            };
            let target = (cap + 1) * width * BYTES_PER_CELL + (next() % 200) as usize;
            let mut input = String::new();
            while input.len() < target {
                let r = next();
                if r % 20 == 0 {
                    input.push(alphabet[(r >> 8) as usize % alphabet.len()]);
                } else {
                    input.push(char::from(b'a' + ((r >> 8) % 26) as u8));
                }
            }
            let mut state = AiPanelState::new();
            state.push_input(&input);

            assert_eq!(
                state.composer_rows(panel_h, panel_w),
                wrap(&input, width, cap, Break::Cell),
                "panel {panel_w}x{panel_h}: the rows are the whole input's \
                 own, column for column, over {} bytes",
                input.len()
            );
        }
    }

    /// The bound the window exists for, on the text it is exact over: a
    /// composer holding only ASCII walks the rows the panel can paint and
    /// not the megabyte behind them, whether or not a line break sits in
    /// front of the opening.
    #[test]
    fn a_composer_walks_the_panels_rows_and_not_the_whole_paste() {
        let width = composer_width(WIDE_PANEL);
        let cap = AiPanelState::new().composer_cap(TEN_ROW_PANEL);
        let letters: String = (0..(1 << 20))
            .map(|i: usize| char::from(b'a' + (i % 26) as u8))
            .collect();

        for (name, input) in [
            ("no break at all", letters.clone()),
            ("a typed break in front of it", format!("first\n{letters}")),
            (
                "a pasted CRLF in front of it",
                format!("first\r\n{letters}"),
            ),
        ] {
            let mut state = AiPanelState::new();
            state.push_input(&input);

            let window = wrap_window(&input, width, cap, &state.breaks, state.non_ascii);

            assert!(
                window.len() <= (cap + 2) * width * BYTES_PER_CELL,
                "{name}: {} bytes walked for {cap} rows",
                window.len()
            );
        }
    }

    /// The worst case the window has, stated as what it costs.
    ///
    /// A row is greedy: one that cannot fit the next two-cell glyph ends a
    /// cell early, so where a line's rows fall is a function of the line
    /// and not of any count that could be carried forward. The line is
    /// therefore the only opening a multibyte stretch has that is provably
    /// a row boundary, and for a prompt of one line that is the prompt --
    /// this pins that cost as the cost, against painting rows a column off
    /// the wrap the transcript gives the same text.
    #[test]
    fn one_long_line_carrying_a_wide_character_costs_that_line() {
        let width = composer_width(WIDE_PANEL);
        let cap = AiPanelState::new().composer_cap(TEN_ROW_PANEL);
        let letters: String = (0..(1 << 16))
            .map(|i: usize| char::from(b'a' + (i % 26) as u8))
            .collect();

        for (name, input, line_at) in [
            ("no break to fall back to", format!("\u{2014}{letters}"), 0),
            (
                "a typed break in front of it",
                format!("first\n\u{2014}{letters}"),
                "first\n".len(),
            ),
        ] {
            let mut state = AiPanelState::new();
            state.push_input(&input);

            let window = wrap_window(&input, width, cap, &state.breaks, state.non_ascii);

            assert_eq!(
                window,
                &input[line_at..],
                "{name}: the window is the line the wrap has to open from"
            );
            assert_eq!(
                state.composer_rows(TEN_ROW_PANEL, WIDE_PANEL),
                wrap(&input, width, cap, Break::Cell),
                "{name}: and what that buys is rows the whole input's wrap \
                 has, column for column"
            );
        }
    }
}
