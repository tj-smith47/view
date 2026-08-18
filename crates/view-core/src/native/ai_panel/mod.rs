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

use super::views::AiPanelView;

mod permission;
mod transcript;

pub use permission::PermissionPrompt;
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
    /// Panel-local crash surface, deliberately not a transient toast: a
    /// crashed long-running session is easy to miss in four seconds.
    pub local_error: Option<String>,
    /// The session's last-reported context-window and cost accounting, or
    /// `None` before the first `usage_update` arrives. See [`UsageStats`].
    pub usage: Option<UsageStats>,
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
            local_error: None,
            usage: None,
        }
    }

    /// The panel's current paint frame: the composer line as typed, and the
    /// transcript rendered oldest first (see [`Transcript::rendered_rows`]
    /// for how a paint that follows a lone folded chunk avoids re-rendering
    /// every earlier entry).
    #[must_use]
    pub fn view(&self) -> AiPanelView {
        AiPanelView::new(TITLE)
            .with_input(self.input.clone())
            .with_rows(self.transcript.rendered_rows())
    }
}

impl Default for AiPanelState {
    fn default() -> Self {
        Self::new()
    }
}

/// The overlay's title, drawn into its top border.
const TITLE: &str = "AI Agent";

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
