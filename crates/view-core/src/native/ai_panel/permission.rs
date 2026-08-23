//! The panel's own projection of one outstanding `session/request_permission`.
//!
//! `PermissionOption`/`PermissionOptionKind` are `view-core`'s own closed
//! vocabulary (`crate::native::ai_event`), not an ACP or `view-ai` type, so
//! holding them here does not reopen the boundary this module's own doc
//! guards: the module that speaks the wire still owns every JSON shape and
//! every wire string, and stamps them onto this vocabulary before an event
//! ever reaches here.

use crate::native::ai_event::{PermissionOption, PermissionOptionKind};
use crate::native::views::{Span, StyleRole};

/// What a session-standing answer for one tool kind says: the two answers
/// that outlive their own question.
///
/// One enum with two arms rather than one store per direction, so "granted
/// and refused for the same kind" is unrepresentable and the user's latest
/// standing answer is simply the one that holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StandingAnswer {
    /// A later call of this kind is allowed without asking.
    Allow,
    /// A later call of this kind is refused without asking.
    Reject,
}

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
    /// The request's own `toolCall.kind`, verbatim from the wire (see
    /// [`crate::native::ai_event::AiEvent::PermissionRequested`]'s field
    /// doc): the scope an always-allow answer records its standing grant
    /// under, and `None` for an agent that named no kind -- a request that
    /// can be answered but never granted.
    pub tool_kind: Option<String>,
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
        tool_kind: Option<String>,
        options: Vec<PermissionOption>,
    ) -> Self {
        let name = title.unwrap_or_else(|| tool_call_id.to_string());
        Self {
            request_id,
            prompt: format!("Permission requested for {name}"),
            tool_kind,
            options,
        }
    }

    /// The offered option `key` selects: the digit keys, one per offered
    /// option in the order the agent sent them, `1` first.
    ///
    /// Digits rather than letters keyed to the option kinds, and
    /// unconditionally rather than only while a review is also open. A diff
    /// review owns `a` for "accept this hunk" (`super::review`), a
    /// permission prompt used to own `a` for "allow always", and both fill
    /// on the same agent edit with the permission consuming keys first --
    /// so the one key a user reaches for by muscle memory answered the
    /// invisible question with the most standing consequence of any answer
    /// offered. Digits belong to neither of the panel's other key owners, so
    /// the collision stops existing rather than being arbitrated by which
    /// state happens to be open; and a vocabulary that changed with
    /// invisible other state would be the same class of defect.
    ///
    /// Only the first nine options are reachable, which is what
    /// [`Self::render_rows`] paints a digit against -- there is no key
    /// beyond `9`, and a row labelled with a number nothing answers would
    /// promise one.
    #[must_use]
    pub fn option_for_key(&self, key: char) -> Option<&PermissionOption> {
        let index = usize::try_from(key.to_digit(10)?).ok()?.checked_sub(1)?;
        self.options.get(index)
    }

    /// The offered option a standing `answer` for this prompt's tool kind
    /// is given with, preferring the always-form the user originally chose
    /// and falling back to the once-form, or `None` from an agent that
    /// offered neither.
    ///
    /// Answering with the always-form again rather than downgrading to the
    /// once-form restates the user's own choice to every request: an adapter
    /// that does honour `allow_always`/`reject_always` then installs the rule
    /// it meant to, and the store simply never fires again.
    pub(crate) fn standing_option(
        options: &[PermissionOption],
        answer: StandingAnswer,
    ) -> Option<&PermissionOption> {
        let (always, once) = match answer {
            StandingAnswer::Allow => (
                PermissionOptionKind::AllowAlways,
                PermissionOptionKind::AllowOnce,
            ),
            StandingAnswer::Reject => (
                PermissionOptionKind::RejectAlways,
                PermissionOptionKind::RejectOnce,
            ),
        };
        options
            .iter()
            .find(|option| option.kind == always)
            .or_else(|| options.iter().find(|option| option.kind == once))
    }

    /// The prompt's own paint rows: the question, one row per offered option
    /// carrying the digit that answers it, its display name and its wire
    /// kind, and the hint row naming the way out.
    ///
    /// Every row carries a role of its own ([`StyleRole::AiPermissionAsk`]
    /// and friends), so an unanswered question does not paint as more
    /// transcript, and the answer with standing consequence does not paint
    /// as the answer without one.
    #[must_use]
    pub fn render_rows(&self) -> Vec<Vec<Span>> {
        let mut rows = vec![vec![Span::new(
            self.prompt.clone(),
            StyleRole::AiPermissionAsk,
        )]];
        rows.extend(self.options.iter().enumerate().map(|(index, option)| {
            // A tenth option and beyond has no digit to be answered with,
            // so it is not given one to press.
            let key = match index + 1 {
                key @ 1..=9 => key.to_string(),
                _ => UNREACHABLE_OPTION_MARK.to_string(),
            };
            vec![Span::new(
                format!(
                    "  {key} {} ({})",
                    option.name,
                    self.option_note(option.kind)
                ),
                option_role(option.kind),
            )]
        }));
        rows.push(vec![Span::new(KEY_HINT, StyleRole::AiPermissionAsk)]);
        rows
    }

    /// What the parenthesis after an option's own name says it does.
    ///
    /// The two always-answers say the consequence view will actually deliver
    /// -- every later call of this kind, until the session ends -- because
    /// that consequence is view's own doing (`super::AiPanelState`'s standing
    /// answers) and "Always Allow" alone names neither the scope nor the
    /// lifetime the user is agreeing to. Without a kind to scope it there is
    /// no standing answer to promise, so the row falls back to the wire
    /// spelling like every other option: the label never claims a grant that
    /// would not be recorded.
    fn option_note(&self, kind: PermissionOptionKind) -> String {
        match (kind, self.tool_kind.as_deref()) {
            (PermissionOptionKind::AllowAlways, Some(tool)) => {
                format!("all {tool} this session")
            }
            (PermissionOptionKind::RejectAlways, Some(tool)) => {
                format!("no {tool} this session")
            }
            _ => kind_label(kind).to_string(),
        }
    }
}

/// What the hint row under a prompt's options says. The digits are on the
/// rows themselves; this names the answer that is on no row, since `<Esc>`
/// settles the request as cancelled and exists even when every option
/// offered was an allow.
pub(crate) const KEY_HINT: &str = "press a number, <Esc> cancels";

/// What stands where a digit would for an option past the ninth, which no
/// key reaches.
const UNREACHABLE_OPTION_MARK: char = '-';

/// The role one option row paints in: the answers a user can tell apart at
/// a glance are allow-once, allow-always and refuse, so those are the three
/// colors -- an always-allow reading as an ordinary allow is exactly how a
/// standing grant gets handed out by accident.
fn option_role(kind: PermissionOptionKind) -> StyleRole {
    match kind {
        PermissionOptionKind::AllowOnce => StyleRole::AiPermissionAllow,
        PermissionOptionKind::AllowAlways => StyleRole::AiPermissionAlways,
        PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => {
            StyleRole::AiPermissionReject
        }
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
        let prompt = PermissionPrompt::new(1, "call_1", None, None, vec![]);
        assert_eq!(prompt.request_id, 1);
        assert!(prompt.prompt.contains("call_1"));
    }

    #[test]
    fn the_prompt_prefers_the_human_readable_title_when_the_agent_sent_one() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
            Some("Delete config.yaml".to_string()),
            None,
            vec![],
        );
        assert!(prompt.prompt.contains("Delete config.yaml"));
        assert!(!prompt.prompt.contains("call_1"));
    }

    #[test]
    fn each_digit_selects_the_option_at_that_position_in_the_agents_own_order() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
            None,
            None,
            vec![
                option("reject-once", PermissionOptionKind::RejectOnce),
                option("allow-once", PermissionOptionKind::AllowOnce),
                option("allow-always", PermissionOptionKind::AllowAlways),
            ],
        );
        assert_eq!(prompt.option_for_key('1').unwrap().option_id, "reject-once");
        assert_eq!(prompt.option_for_key('2').unwrap().option_id, "allow-once");
        assert_eq!(
            prompt.option_for_key('3').unwrap().option_id,
            "allow-always"
        );
    }

    /// The keys the panel's other owner holds are exactly the ones this
    /// prompt must not answer: `a` used to mean allow-always here and means
    /// accept-hunk in a review, and both pend together on one agent edit.
    #[test]
    fn no_letter_and_no_digit_past_the_offered_options_answers_anything() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
            None,
            None,
            vec![option("allow-once", PermissionOptionKind::AllowOnce)],
        );
        for key in ['a', 'A', 'y', 'n', 'x', 'q', '0', '2', '9'] {
            assert!(
                prompt.option_for_key(key).is_none(),
                "{key} must answer nothing"
            );
        }
    }

    #[test]
    fn a_standing_answer_is_given_with_the_always_option_of_its_own_direction() {
        let options = vec![
            option("allow-once", PermissionOptionKind::AllowOnce),
            option("allow-always", PermissionOptionKind::AllowAlways),
            option("reject-once", PermissionOptionKind::RejectOnce),
            option("reject-always", PermissionOptionKind::RejectAlways),
        ];
        assert_eq!(
            PermissionPrompt::standing_option(&options, StandingAnswer::Allow)
                .unwrap()
                .option_id,
            "allow-always"
        );
        assert_eq!(
            PermissionPrompt::standing_option(&options, StandingAnswer::Reject)
                .unwrap()
                .option_id,
            "reject-always"
        );
    }

    #[test]
    fn a_standing_answer_falls_back_to_the_once_option_and_never_crosses_direction() {
        let once_only = vec![
            option("reject-once", PermissionOptionKind::RejectOnce),
            option("allow-once", PermissionOptionKind::AllowOnce),
        ];
        assert_eq!(
            PermissionPrompt::standing_option(&once_only, StandingAnswer::Allow)
                .unwrap()
                .option_id,
            "allow-once"
        );
        assert_eq!(
            PermissionPrompt::standing_option(&once_only, StandingAnswer::Reject)
                .unwrap()
                .option_id,
            "reject-once"
        );
        let no_allow = vec![option("reject-always", PermissionOptionKind::RejectAlways)];
        assert!(PermissionPrompt::standing_option(&no_allow, StandingAnswer::Allow).is_none());
        let no_reject = vec![option("allow-always", PermissionOptionKind::AllowAlways)];
        assert!(PermissionPrompt::standing_option(&no_reject, StandingAnswer::Reject).is_none());
    }

    /// The two answers that outlive their question must say so on the row
    /// that offers them: "Always Allow" alone names neither what it covers
    /// nor how long it lasts.
    #[test]
    fn the_always_rows_name_the_kind_and_the_session_they_stand_for() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
            None,
            Some("edit".to_string()),
            vec![
                option("Always Allow", PermissionOptionKind::AllowAlways),
                option("Always Reject", PermissionOptionKind::RejectAlways),
                option("Allow once", PermissionOptionKind::AllowOnce),
            ],
        );
        let rows = prompt.render_rows();
        assert_eq!(rows[1][0].text, "  1 Always Allow (all edit this session)");
        assert_eq!(rows[2][0].text, "  2 Always Reject (no edit this session)");
        assert_eq!(
            rows[3][0].text, "  3 Allow once (allow_once)",
            "an answer with no standing consequence keeps naming its wire kind"
        );
    }

    /// No kind means no standing answer can be recorded, so the row must not
    /// promise one.
    #[test]
    fn an_always_row_with_no_tool_kind_promises_nothing_beyond_its_wire_spelling() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
            None,
            None,
            vec![option("Always Allow", PermissionOptionKind::AllowAlways)],
        );
        assert_eq!(
            prompt.render_rows()[1][0].text,
            "  1 Always Allow (allow_always)"
        );
    }

    #[test]
    fn render_rows_labels_every_option_with_its_digit_its_kind_and_its_own_role() {
        let prompt = PermissionPrompt::new(
            1,
            "call_1",
            Some("Delete config.yaml".to_string()),
            None,
            vec![
                option("Allow once", PermissionOptionKind::AllowOnce),
                option("Always Allow", PermissionOptionKind::AllowAlways),
                option("Deny", PermissionOptionKind::RejectOnce),
            ],
        );
        let rows = prompt.render_rows();
        assert_eq!(rows.len(), 5, "the question, three options, the hint");
        assert_eq!(
            rows[0],
            vec![Span::new(
                "Permission requested for Delete config.yaml",
                StyleRole::AiPermissionAsk
            )]
        );
        assert_eq!(
            rows[1],
            vec![Span::new(
                "  1 Allow once (allow_once)",
                StyleRole::AiPermissionAllow
            )]
        );
        assert_eq!(
            rows[2],
            vec![Span::new(
                "  2 Always Allow (allow_always)",
                StyleRole::AiPermissionAlways
            )],
            "the answer with standing consequence paints as its own thing"
        );
        assert_eq!(
            rows[3],
            vec![Span::new(
                "  3 Deny (reject_once)",
                StyleRole::AiPermissionReject
            )]
        );
        assert_eq!(
            rows[4],
            vec![Span::new(KEY_HINT, StyleRole::AiPermissionAsk)]
        );
    }

    /// A row promising a key nothing answers is the defect this prompt
    /// exists to fix, one option further along.
    #[test]
    fn an_option_past_the_ninth_is_painted_without_a_digit_it_cannot_be_answered_by() {
        let options: Vec<PermissionOption> = (1..=10)
            .map(|n| option(&format!("opt-{n}"), PermissionOptionKind::AllowOnce))
            .collect();
        let prompt = PermissionPrompt::new(1, "call_1", None, None, options);
        let rows = prompt.render_rows();
        assert_eq!(rows[9][0].text, "  9 opt-9 (allow_once)");
        assert_eq!(rows[10][0].text, "  - opt-10 (allow_once)");
        assert!(prompt.option_for_key('9').is_some());
    }
}
