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
pub use transcript::{Transcript, TranscriptEntry, TranscriptEntryKind, TranscriptRole};

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

/// One diff hunk the agent has proposed and the user has not yet acted on.
/// The plainest shape that can prove a countable, resolved/unresolved
/// distinction -- diff review (`native::ai_panel::review`, not yet built)
/// grows this into the real hunk content in place; nothing here anticipates
/// that shape ahead of the task that owns it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEdit {
    pub id: String,
    pub resolved: bool,
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
    /// Diff hunks the agent has proposed, oldest first.
    /// `AiStatus::derive` (`native::ai_registry`) counts the unresolved
    /// ones for its doctor-facing `pending_edit_count`.
    pub pending_edits: Vec<PendingEdit>,
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
    /// One slot, not a queue, on the same terms as
    /// [`Self::pending_permission`]: a proposal is the direct consequence
    /// of an in-flight agent turn, and a review the user is part way
    /// through must never be replaced out from under them by a second
    /// proposal -- `update::ai`'s own arm is what decides what the second
    /// one is answered with.
    pub pending_diff: Option<DiffReviewState>,
    /// The generation stamped on the next review's own async replies,
    /// bumped per review on the `PickerState::generation` precedent.
    pub review_generation: u64,
}

impl AiPanelState {
    /// A freshly opened panel: no session bound yet, an empty transcript,
    /// nothing typed, nothing pending.
    pub fn new() -> Self {
        Self {
            session_id: None,
            transcript: Transcript::new(),
            input: String::new(),
            pending_edits: Vec::new(),
            pending_permission: None,
            focused: false,
            turn_in_flight: false,
            local_error: None,
            usage: None,
            pending_diff: None,
            review_generation: 0,
        }
    }

    /// The panel's current paint frame: the composer line as typed, the
    /// transcript rendered oldest first (see [`Transcript::rendered_rows`]
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
    #[must_use]
    pub fn view(&self) -> AiPanelView {
        // An open review takes over the scrolling rows rather than
        // appending to them: its scroll region is its own hunks and their
        // context, never the whole transcript with a diff somewhere in it
        // (and never the whole buffer -- see `DiffReviewState::hunk_rows`).
        // The transcript is still in state and comes back the moment the
        // review closes.
        let rows = match &self.pending_diff {
            Some(review) => review.hunk_rows(),
            None => self.transcript.rendered_rows(),
        };
        let mut view = AiPanelView::new(if self.focused { FOCUSED_TITLE } else { TITLE })
            .with_input(self.input.clone())
            .with_rows(rows);
        if let Some(review) = &self.pending_diff {
            let mut rows = review.summary_rows();
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

    #[test]
    fn a_new_panel_is_empty() {
        let state = AiPanelState::new();
        assert_eq!(state.session_id, None);
        assert!(state.transcript.is_empty());
        assert_eq!(state.input, "");
        assert!(state.pending_edits.is_empty());
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
        let view = state.view();
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
        let view = state.view();
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
            state.view().local_error,
            vec![vec![Span::plain(format!(
                "Error: gone -- {DISMISS_KEY_HINT}"
            ))]]
        );
        state.focused = false;
        assert_eq!(
            state.view().local_error,
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
        assert_eq!(state.view().title, TITLE);
        state.focused = true;
        let view = state.view();
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
        let view = state.view();
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
        let view = state.view();
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
        let view = state.view();
        assert!(
            view.pending_permission
                .iter()
                .all(|row| row != &vec![Span::plain(ENTER_HINT)]),
            "a focused panel already answers keys; the hint would be stale: {:?}",
            view.pending_permission
        );
    }

    #[test]
    fn a_transcript_entry_renders_with_its_speaker_prefix() {
        let mut state = AiPanelState::new();
        state.transcript.append_or_extend(Some("1"), "hi", false);
        state.transcript.append_or_extend(Some("2"), "hello", true);
        let view = state.view();
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
        let view = state.view();
        assert_eq!(view.rows, vec![vec![Span::plain("running: Read file")]]);
    }
}
