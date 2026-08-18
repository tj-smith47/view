//! The panel's own projection of one outstanding `session/request_permission`.
//!
//! `PermissionOption`/`PermissionOptionKind` are `view-core`'s own closed
//! vocabulary (`crate::native::ai_event`), not an ACP or `view-ai` type, so
//! holding them here does not reopen the boundary this module's own doc
//! guards: the module that speaks the wire still owns every JSON shape and
//! every wire string, and stamps them onto this vocabulary before an event
//! ever reaches here.

use crate::native::ai_event::{PermissionOption, PermissionOptionKind};

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
    /// folds into [`super::AiPanelState::pending_permission`]. `tool_call_id`
    /// is the only description the wire's own worked example guarantees
    /// (`docs/acp-v1-wire-capture.md`'s `RequestPermissionParams` pin --
    /// `toolCall` carries only `toolCallId` in that example, no title), so
    /// it is what the question text names.
    #[must_use]
    pub fn new(request_id: u64, tool_call_id: &str, options: Vec<PermissionOption>) -> Self {
        Self {
            request_id,
            prompt: format!("Permission requested for {tool_call_id}"),
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
    fn the_prompt_names_the_tool_call_it_answers_for() {
        let prompt = PermissionPrompt::new(1, "call_1", vec![]);
        assert_eq!(prompt.request_id, 1);
        assert!(prompt.prompt.contains("call_1"));
    }

    #[test]
    fn y_a_n_map_to_the_pinned_allow_and_one_time_reject_kinds() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
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
            vec![option("allow-once", PermissionOptionKind::AllowOnce)],
        );
        assert!(prompt.option_for_key('a').is_none());
        assert!(prompt.option_for_key('n').is_none());
        assert!(prompt.option_for_key('z').is_none());
    }
}
