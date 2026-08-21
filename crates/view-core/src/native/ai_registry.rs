//! The doctor-facing agent-session lifecycle record: session id, agent
//! identity, state, last activity, and pending edit count, for a future
//! doctor consumer to report about the AI session an agent panel can hold.
//!
//! Deliberately not a [`FeatureDesc`](super::registry::FeatureDesc) entry:
//! `[ai]` is a sibling top-level table, not a `[native]` key, and
//! `FeatureDesc::id`'s own doc comment hard-codes the `[native]`-table-key
//! contract this struct must not inherit.

use super::ai_panel::AiPanelState;

/// Where one agent session sits in its lifecycle, as the doctor sees it.
/// Not the same axis as `enabled`: a disabled feature and a never-started
/// session both read `NotStarted` here, and the doctor row's own `enabled`
/// field is what tells them apart.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// No session has ever been started, and trust has not been granted.
    NotStarted,
    /// Trust has been granted but no session is currently live.
    Trusted,
    /// A session id is bound; the agent is running.
    Active,
    /// The panel's own crash surface is set: the session ended abnormally.
    Crashed,
}

/// One agent session's lifecycle as the doctor reports it: session id
/// (carried inside `session_state`'s `Active` case via the panel, not
/// duplicated here), agent identity, state, last activity, and pending
/// edit count -- the exact fields the charter's own doctor contract names.
#[must_use]
#[derive(Debug, Clone, PartialEq)]
pub struct AiStatus {
    pub enabled: bool,
    pub agent_id: String,
    pub session_state: SessionState,
    pub last_activity: Option<std::time::SystemTime>,
    pub pending_edit_count: usize,
}

impl AiStatus {
    /// Derived from panel state plus the three scalars the caller already
    /// holds. Takes plain data, never a config type: `AiConfig` lives in
    /// `view-ai`, which this crate may not depend on
    /// (`scripts/audit-deps.sh`), so the caller extracts `enabled`,
    /// `agent_id`, and `trusted` and hands them over rather than a
    /// `&AiConfig` this function could not even name.
    ///
    /// `session_state` is resolved in this order, top to bottom, and stops
    /// at the first match:
    /// 1. `panel.local_error.is_some()` -> `Crashed`, regardless of
    ///    `trusted` or `session_id` -- a crashed session is crashed even if
    ///    it was trusted and had a live id a moment ago.
    /// 2. `panel.session_id.is_some()` -> `Active`.
    /// 3. `trusted` (no session yet) -> `Trusted`.
    /// 4. otherwise -> `NotStarted`.
    ///
    /// `enabled` never enters that resolution: it is config-enabled, not
    /// session-started, the same pair `Model::ai_enabled` and
    /// `Model::ai_trusted` are kept apart for -- conflating them would make
    /// a disabled-but-previously-trusted project read as `Trusted` here.
    /// It only ever surfaces on `AiStatus::enabled` for the doctor to
    /// render next to the state, never folded into deriving it.
    ///
    /// `pending_edit_count` counts the hunks still awaiting a decision, in
    /// the open review and in the one queued behind it. Derived from the
    /// reviews themselves rather than a parallel list kept beside them: a
    /// second record of what the user has and has not acted on is a second
    /// thing to keep true, and the one that is never read is the one that
    /// silently stops being true.
    ///
    /// `last_activity` is always `None`: nothing in `AiPanelState` carries
    /// a timestamp yet, so there is nothing honest to derive it from until
    /// a task threads one through.
    pub fn derive(panel: &AiPanelState, enabled: bool, agent_id: &str, trusted: bool) -> Self {
        let session_state = if panel.local_error.is_some() {
            SessionState::Crashed
        } else if panel.session_id.is_some() {
            SessionState::Active
        } else if trusted {
            SessionState::Trusted
        } else {
            SessionState::NotStarted
        };
        let pending_edit_count = [&panel.pending_diff, &panel.pending_diff_next]
            .into_iter()
            .flatten()
            .map(|review| {
                review
                    .hunks
                    .iter()
                    .filter(|hunk| hunk.status.is_open())
                    .count()
            })
            .sum();
        Self {
            enabled,
            agent_id: agent_id.to_string(),
            session_state,
            last_activity: None,
            pending_edit_count,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::super::ai_panel::{AiPanelState, DiffReviewState};
    use super::*;

    /// One review of `open` undecided hunks, plus `decided` the user has
    /// already answered.
    fn review(open: usize, decided: usize) -> DiffReviewState {
        let hunk = || {
            crate::native::diff::hunk::Hunk::new(
                (0, 1),
                vec!["new".to_string()],
                0,
                vec!["old".to_string()],
            )
        };
        let mut hunks: Vec<_> = std::iter::repeat_with(hunk).take(open + decided).collect();
        for hunk in hunks.iter_mut().take(decided) {
            hunk.status = crate::native::diff::hunk::HunkStatus::Rejected;
        }
        DiffReviewState::new(1, std::path::PathBuf::from("/p/a.rs"), 1, hunks)
    }

    #[test]
    fn pending_edit_count_counts_every_hunk_still_owed_a_decision() {
        let mut panel = AiPanelState::new();
        panel.pending_diff = Some(review(2, 1));
        panel.pending_diff_next = Some(review(3, 0));

        let status = AiStatus::derive(&panel, true, "claude-code", false);
        assert_eq!(
            status.pending_edit_count, 5,
            "the queued review's hunks are owed a decision too"
        );
        assert!(status.enabled);
        assert_eq!(status.agent_id, "claude-code");
    }

    #[test]
    fn a_session_with_no_review_owes_no_decisions() {
        let status = AiStatus::derive(&AiPanelState::new(), true, "claude-code", false);
        assert_eq!(status.pending_edit_count, 0);
    }

    #[test]
    fn disabled_config_passes_through_as_disabled() {
        let panel = AiPanelState::new();
        let status = AiStatus::derive(&panel, false, "custom-agent", false);
        assert!(!status.enabled);
        assert_eq!(status.agent_id, "custom-agent");
    }

    #[test]
    fn a_local_error_derives_crashed() {
        let mut panel = AiPanelState::new();
        panel.session_id = Some("s1".to_string());
        panel.local_error = Some("agent process exited".to_string());
        let status = AiStatus::derive(&panel, true, "claude-code", true);
        assert_eq!(status.session_state, SessionState::Crashed);
    }

    #[test]
    fn enabled_alone_does_not_derive_active() {
        let panel = AiPanelState::new();
        let status = AiStatus::derive(&panel, true, "claude-code", false);
        assert_eq!(status.session_state, SessionState::NotStarted);
    }

    #[test]
    fn trust_with_no_session_derives_trusted() {
        let panel = AiPanelState::new();
        let status = AiStatus::derive(&panel, true, "claude-code", true);
        assert_eq!(status.session_state, SessionState::Trusted);
    }

    #[test]
    fn a_bound_session_derives_active() {
        let mut panel = AiPanelState::new();
        panel.session_id = Some("s1".to_string());
        let status = AiStatus::derive(&panel, true, "claude-code", false);
        assert_eq!(status.session_state, SessionState::Active);
    }
}
