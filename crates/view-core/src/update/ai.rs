//! Folds one [`AiEvent`] into the session state every agent panel reads.

use crate::model::Model;
use crate::msg::Effect;
use crate::native::ai_event::AiEvent;

/// Applies `event` to [`Model::ai_panel`].
///
/// Matched without a wildcard on purpose, the same reason the caller's own
/// `Msg::Ai` arm has none: a new `AiEvent` variant must recompile every
/// consumer of it, not silently fall through here. Every variant not yet
/// rendered (session lifecycle, permission requests, diff review) is its
/// own explicit no-op arm rather than absorbed by `_`, so wiring each one
/// later turns exactly one arm from a no-op into real behavior instead of
/// hunting for where a wildcard swallowed it.
///
/// Folds unconditionally: [`Model::ai_panel`] is session state, not overlay
/// state (see that field's own doc), so a chunk streamed while the sidebar
/// is closed still has somewhere to go, and reopening the sidebar finds it
/// already there rather than dropped mid-stream.
pub(super) fn on_ai_event(model: &mut Model, event: AiEvent) -> Vec<Effect> {
    match event {
        AiEvent::MessageChunk {
            message_id,
            text,
            from_agent,
        } => {
            model.ai_panel_mut().transcript.append_or_extend(
                message_id.as_deref(),
                &text,
                from_agent,
            );
            model.dirty = true;
        }
        AiEvent::ToolCallUpdate {
            tool_call_id,
            title,
            status,
            content,
        } => {
            model
                .ai_panel_mut()
                .transcript
                .upsert_tool_call(tool_call_id, title, status, content);
            model.dirty = true;
        }
        AiEvent::PlanUpdated { entries } => {
            model.ai_panel_mut().transcript.upsert_plan(entries);
            model.dirty = true;
        }
        AiEvent::UsageUpdated { used, size, cost } => {
            model.ai_panel_mut().usage =
                Some(crate::native::ai_panel::UsageStats { used, size, cost });
            model.dirty = true;
        }
        AiEvent::SessionReady { .. }
        | AiEvent::ThoughtChunk { .. }
        | AiEvent::PermissionRequested { .. }
        | AiEvent::DiffProposed { .. }
        | AiEvent::TurnEnded { .. }
        | AiEvent::SessionCrashed { .. }
        | AiEvent::FsReadRequested { .. }
        | AiEvent::FsWriteRequested { .. } => {}
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::model::OverlayKind;
    use crate::msg::Msg;
    use crate::native::ai_event::ToolCallStatus;
    use crate::native::geometry::OverlayBox;
    use crate::update::update;

    fn model_with_open_panel() -> Model {
        let mut model = Model::new();
        model.push_overlay(OverlayBox::new(30, 100), OverlayKind::Ai);
        model
    }

    #[test]
    fn a_message_chunk_folds_into_the_open_panels_transcript() {
        let mut model = model_with_open_panel();
        on_ai_event(
            &mut model,
            AiEvent::MessageChunk {
                message_id: Some("m1".to_string()),
                text: "hello".to_string(),
                from_agent: true,
            },
        );
        assert_eq!(model.ai_panel_mut().transcript.len(), 1);
        assert!(model.dirty);
    }

    #[test]
    fn a_message_chunk_with_no_panel_open_still_folds_into_the_session_state() {
        let mut model = Model::new();
        let effects = on_ai_event(
            &mut model,
            AiEvent::MessageChunk {
                message_id: None,
                text: "hello".to_string(),
                from_agent: true,
            },
        );
        assert!(effects.is_empty());
        assert_eq!(
            model.ai_panel_mut().transcript.len(),
            1,
            "the session survives the sidebar being closed: a chunk streamed \
             with no overlay open still has to land somewhere, so a later \
             reopen finds it rather than a gap in the transcript"
        );
    }

    /// Drives `update()` itself rather than `on_ai_event` directly, so the
    /// `Msg::Ai` dispatch arm in `update/mod.rs` is load-bearing: a mutation
    /// that no-ops that arm (return `Vec::new()` without calling
    /// `on_ai_event`) fails this test, where a test that only calls
    /// `on_ai_event` in isolation would not notice the dispatch was ever
    /// disconnected.
    #[test]
    fn dispatching_msg_ai_through_update_folds_a_message_chunk() {
        let mut model = model_with_open_panel();
        let effects = update(
            &mut model,
            Msg::Ai(AiEvent::MessageChunk {
                message_id: Some("m1".to_string()),
                text: "hello".to_string(),
                from_agent: true,
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(model.ai_panel_mut().transcript.len(), 1);
        assert!(model.dirty);
    }

    #[test]
    fn dispatching_msg_ai_through_update_folds_a_tool_call_update() {
        let mut model = model_with_open_panel();
        let effects = update(
            &mut model,
            Msg::Ai(AiEvent::ToolCallUpdate {
                tool_call_id: "call_1".to_string(),
                title: "Read file".to_string(),
                status: ToolCallStatus::Pending,
                content: None,
            }),
        );
        assert!(effects.is_empty());
        assert_eq!(model.ai_panel_mut().transcript.len(), 1);
        assert!(model.dirty);
    }

    fn ai_feature_invoke(verb: &str) -> Msg {
        Msg::FeatureInvoke {
            feature: "ai".to_string(),
            verb: verb.to_string(),
        }
    }

    /// Pins [`Model::ai_panel`]'s headline guarantee end to end, through the
    /// real `open`/`close` verbs rather than `push_overlay`/`model.ai_panel`
    /// reached directly: a chunk folded while the sidebar is open survives a
    /// close, a second chunk folded while it is closed still lands, and
    /// reopening finds both in one entry rather than a session `open_ai_panel`
    /// quietly re-seeded. A regression that re-seeds `model.ai_panel` on open
    /// (the exact defect [`Model::ai_panel`]'s doc promises cannot happen)
    /// would pass every other test in this file, since none of them ever
    /// closes and reopens -- this is the one that must fail for it.
    #[test]
    fn a_session_survives_the_sidebar_closing_and_reopening() {
        let mut model = Model::new();
        model.ai_trusted = true;
        let _ = update(&mut model, ai_feature_invoke("open"));

        let _ = update(
            &mut model,
            Msg::Ai(AiEvent::MessageChunk {
                message_id: Some("m1".to_string()),
                text: "before close".to_string(),
                from_agent: true,
            }),
        );

        let _ = update(&mut model, ai_feature_invoke("close"));
        assert!(
            !model.ai_panel_overlay_open(),
            "the close verb must actually hide the sidebar for this test to \
             prove anything about the closed case"
        );

        let _ = update(
            &mut model,
            Msg::Ai(AiEvent::MessageChunk {
                message_id: Some("m1".to_string()),
                text: ", while closed".to_string(),
                from_agent: true,
            }),
        );

        let _ = update(&mut model, ai_feature_invoke("open"));

        assert_eq!(
            model.ai_panel_mut().transcript.len(),
            1,
            "a session re-seeded on reopen would show a fresh, empty \
             transcript here instead of the one entry both chunks folded into"
        );
        let entry = model
            .ai_panel_mut()
            .transcript
            .iter()
            .next()
            .expect("one entry");
        assert_eq!(entry.text, "before close, while closed");
    }
}
