//! The panel's own projection of one outstanding `session/request_permission`.
//!
//! `PermissionOption`/`PermissionOptionKind` are `view-core`'s own closed
//! vocabulary (`crate::native::ai_event`), not an ACP or `view-ai` type, so
//! holding them here does not reopen the boundary this module's own doc
//! guards: the module that speaks the wire still owns every JSON shape and
//! every wire string, and stamps them onto this vocabulary before an event
//! ever reaches here.

use crate::native::ai_event::{PermissionOption, PermissionOptionKind};
use crate::native::views::Span;

/// One outstanding permission request the agent is blocked on: which
/// boundary id answering it must cite, a display question, and the options
/// the agent itself offered.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPrompt {
    /// Correlates an answer back to this request; echoed verbatim into
    /// `AiCommand::AnswerPermission`.
    pub request_id: u64,
    pub prompt: String,
    /// The agent's own offered choices, in the order it sent them. Never a
    /// view-side default: an option the agent did not present is
    /// unrepresentable here, the same invariant
    /// [`crate::native::ai_event::AiEvent::PermissionRequested`]'s own doc
    /// states for the event this prompt is built from.
    pub options: Vec<PermissionOption>,
}

impl PermissionPrompt {
    /// Builds the prompt an [`crate::native::ai_event::AiEvent::PermissionRequested`]
    /// folds into [`super::AiPanelState::pending_permission`]. Names `title`
    /// when the agent sent one, falling back to `tool_call_id`: the wire's
    /// `RequestPermissionRequest.toolCall` member is a `ToolCallUpdate`, not
    /// a `ToolCall` (`docs/acp-v1-wire-capture.md`'s
    /// `## RequestPermissionRequest` section dumps both `$defs` verbatim),
    /// and `ToolCallUpdate` requires only `toolCallId` -- `title` is
    /// nullable/optional there -- so an agent that omits it still gets a
    /// question naming the call it asks about, never a blank.
    #[must_use]
    pub fn new(
        request_id: u64,
        tool_call_id: &str,
        title: Option<String>,
        options: Vec<PermissionOption>,
    ) -> Self {
        let name = title.unwrap_or_else(|| tool_call_id.to_string());
        Self {
            request_id,
            prompt: format!("Permission requested for {name}"),
            options,
        }
    }

    /// The offered option `key` selects, or `None` if the agent offered no
    /// option of that kind. `y`/`a`/`n` are the panel's own accelerators for
    /// the three everyday answers -- allow once, allow always, reject --
    /// matching `docs/acp-v1-wire-capture.md`'s pinned `PermissionOptionKind`
    /// spellings: `y` -> `allow_once`, `a` -> `allow_always`, `n` ->
    /// `reject_once`, falling back to `reject_always` when the agent offered
    /// no one-time reject. Case-insensitive, the same convention nvim's own
    /// confirm-class prompts use (see `native::prompt::PromptState::accepts`).
    #[must_use]
    pub fn option_for_key(&self, key: char) -> Option<&PermissionOption> {
        match key.to_ascii_lowercase() {
            'y' => self.option_of_kind(PermissionOptionKind::AllowOnce),
            'a' => self.option_of_kind(PermissionOptionKind::AllowAlways),
            'n' => self
                .option_of_kind(PermissionOptionKind::RejectOnce)
                .or_else(|| self.option_of_kind(PermissionOptionKind::RejectAlways)),
            _ => None,
        }
    }

    fn option_of_kind(&self, kind: PermissionOptionKind) -> Option<&PermissionOption> {
        self.options.iter().find(|option| option.kind == kind)
    }

    /// The prompt's own paint rows: the question, then one row per offered
    /// option naming both its display name and its wire kind, so the panel
    /// shows what pressing `y`/`a`/`n` will actually answer rather than a
    /// name alone that leaves the kind to guesswork.
    #[must_use]
    pub fn render_rows(&self) -> Vec<Vec<Span>> {
        let mut rows = vec![vec![Span::plain(self.prompt.clone())]];
        rows.extend(self.options.iter().map(|option| {
            vec![Span::plain(format!(
                "  {} ({})",
                option.name,
                kind_label(option.kind)
            ))]
        }));
        rows
    }
}

/// The wire spelling for `kind`, per `docs/acp-v1-wire-capture.md`'s pinned
/// `PermissionOptionKind` strings -- shown verbatim rather than a
/// view-invented label, so what the panel prints for a kind always matches
/// what the wire itself called it.
fn kind_label(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption {
            option_id: id.to_string(),
            name: id.to_string(),
            kind,
        }
    }

    #[test]
    fn the_prompt_names_the_tool_call_it_answers_for_with_no_title() {
        let prompt = PermissionPrompt::new(1, "call_1", None, vec![]);
        assert_eq!(prompt.request_id, 1);
        assert!(prompt.prompt.contains("call_1"));
    }

    #[test]
    fn the_prompt_prefers_the_human_readable_title_when_the_agent_sent_one() {
        let prompt =
            PermissionPrompt::new(1, "call_1", Some("Delete config.yaml".to_string()), vec![]);
        assert!(prompt.prompt.contains("Delete config.yaml"));
        assert!(!prompt.prompt.contains("call_1"));
    }

    #[test]
    fn y_a_n_map_to_the_pinned_allow_and_one_time_reject_kinds() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
            None,
            vec![
                option("allow-once", PermissionOptionKind::AllowOnce),
                option("allow-always", PermissionOptionKind::AllowAlways),
                option("reject-once", PermissionOptionKind::RejectOnce),
            ],
        );
        assert_eq!(prompt.option_for_key('y').unwrap().option_id, "allow-once");
        assert_eq!(prompt.option_for_key('Y').unwrap().option_id, "allow-once");
        assert_eq!(
            prompt.option_for_key('a').unwrap().option_id,
            "allow-always"
        );
        assert_eq!(prompt.option_for_key('n').unwrap().option_id, "reject-once");
    }

    #[test]
    fn n_falls_back_to_reject_always_when_no_one_time_reject_is_offered() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
            None,
            vec![option("reject-always", PermissionOptionKind::RejectAlways)],
        );
        assert_eq!(
            prompt.option_for_key('n').unwrap().option_id,
            "reject-always"
        );
    }

    #[test]
    fn a_key_with_no_matching_offered_kind_answers_none() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
            None,
            vec![option("allow-once", PermissionOptionKind::AllowOnce)],
        );
        assert!(prompt.option_for_key('a').is_none());
        assert!(prompt.option_for_key('n').is_none());
        assert!(prompt.option_for_key('z').is_none());
    }

    #[test]
    fn render_rows_names_the_question_and_every_options_kind() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
            Some("Delete config.yaml".to_string()),
            vec![
                option("allow-once", PermissionOptionKind::AllowOnce),
                option("reject-once", PermissionOptionKind::RejectOnce),
            ],
        );
        let rows = prompt.render_rows();
        assert_eq!(rows.len(), 3, "the question plus one row per option");
        assert_eq!(
            rows[0],
            vec![Span::plain("Permission requested for Delete config.yaml")]
        );
        assert_eq!(
            rows[1],
            vec![Span::plain("  allow-once (allow_once)".to_string())]
        );
        assert_eq!(
            rows[2],
            vec![Span::plain("  reject-once (reject_once)".to_string())]
        );
    }
}
