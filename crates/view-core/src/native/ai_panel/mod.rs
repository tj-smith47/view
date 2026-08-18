//! Pure state for the agent panel overlay: the transcript, the composer
//! line, and whatever the agent is waiting on. No ACP or `view-ai` types
//! reach this module -- transcript entries and permission prompts are
//! copied down to plain strings and a closed role vocabulary at whatever
//! boundary owns the real client, the same way `native::tree` never holds a
//! `git2` type and `native::picker` never holds a ripgrep match struct.
//!
//! This module owns only the shape; the tasks that drive a live session
//! (streaming transcript chunks, answering a permission request, running a
//! diff review) grow it in place rather than replacing it.

use super::views::{AiPanelView, Span};

/// Who spoke one transcript entry. Closed rather than a free-form string:
/// every renderer that ever styles a row by speaker (the composer's own
/// "you" vs "agent" convention every chat-shaped UI uses) switches on this
/// instead of matching text an adapter could spell differently release to
/// release.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    /// The user's own composed message.
    User,
    /// The agent's reply.
    Agent,
}

/// One folded entry in the transcript: an id chunks stream into, who sent
/// it, and its text so far.
///
/// `id` is what a later chunk folds against rather than appending a new
/// entry -- ACP streams a reply as multiple chunks against one message id,
/// and without an id to fold on the transcript would grow one row per
/// chunk instead of one row per message.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub id: String,
    pub role: TranscriptRole,
    pub text: String,
}

/// One outstanding permission request the agent is blocked on: the question
/// text and the fixed answers it offers. Plain strings rather than a wire
/// enum -- the module that speaks ACP is the one place that ever needs to
/// know an option's real identity, and it stamps that identity onto
/// whatever effect answering this prompt emits, not onto this struct.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPrompt {
    pub prompt: String,
    pub options: Vec<String>,
}

/// The agent panel's state: which session it belongs to, its transcript so
/// far, its own composer line, and whatever it is currently blocked on.
#[must_use]
#[derive(Debug, Clone, PartialEq)]
pub struct AiPanelState {
    pub session_id: Option<String>,
    /// Agent output, folded per message id as chunks stream in.
    pub transcript: Vec<TranscriptEntry>,
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
    /// Panel-local crash surface, deliberately not a transient toast: a
    /// crashed long-running session is easy to miss in four seconds.
    pub local_error: Option<String>,
}

impl AiPanelState {
    /// A freshly opened panel: no session bound yet, an empty transcript,
    /// nothing typed, nothing pending.
    pub fn new() -> Self {
        Self {
            session_id: None,
            transcript: Vec::new(),
            input: String::new(),
            pending_permission: None,
            local_error: None,
        }
    }

    /// The panel's current paint frame: the composer line as typed, and the
    /// transcript rendered one row per entry, oldest first.
    #[must_use]
    pub fn view(&self) -> AiPanelView {
        AiPanelView::new(TITLE)
            .with_input(self.input.clone())
            .with_rows(self.transcript.iter().map(transcript_row).collect())
    }
}

impl Default for AiPanelState {
    fn default() -> Self {
        Self::new()
    }
}

/// The overlay's title, drawn into its top border.
const TITLE: &str = "AI Agent";

/// One transcript entry's row: the speaker as a plain-text prefix, then its
/// text. A single unstyled span, the same honest "nothing to preserve"
/// shape a picker candidate or a tree leaf paints in -- role-based styling
/// is a painter concern for the task that gives this row real content to
/// distinguish, not a reason to invent a span structure with nothing yet to
/// carry in it.
fn transcript_row(entry: &TranscriptEntry) -> Vec<Span> {
    let speaker = match entry.role {
        TranscriptRole::User => "You",
        TranscriptRole::Agent => "Agent",
    };
    vec![Span::plain(format!("{speaker}: {}", entry.text))]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_new_panel_is_empty() {
        let state = AiPanelState::new();
        assert_eq!(state.session_id, None);
        assert!(state.transcript.is_empty());
        assert_eq!(state.input, "");
        assert_eq!(state.pending_permission, None);
        assert_eq!(state.local_error, None);
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
        state.transcript.push(TranscriptEntry {
            id: "1".to_string(),
            role: TranscriptRole::User,
            text: "hi".to_string(),
        });
        state.transcript.push(TranscriptEntry {
            id: "2".to_string(),
            role: TranscriptRole::Agent,
            text: "hello".to_string(),
        });
        let view = state.view();
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.rows[0], vec![Span::plain("You: hi")]);
        assert_eq!(view.rows[1], vec![Span::plain("Agent: hello")]);
    }
}
