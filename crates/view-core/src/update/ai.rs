//! Folds one [`AiEvent`] into the session state every agent panel reads.

use crate::model::Model;
use crate::msg::Effect;
use crate::native::ai_event::{AiCommand, AiEvent, PermissionOutcome};
use crate::native::ai_panel::PermissionPrompt;

/// Applies `event` to [`Model::ai_panel`].
///
/// Matched without a wildcard on purpose, the same reason the caller's own
/// `Msg::Ai` arm has none: a new `AiEvent` variant must recompile every
/// consumer of it, not silently fall through here. Every variant not yet
/// rendered (session lifecycle, diff review) is its own explicit no-op arm
/// rather than absorbed by `_`, so wiring each one later turns exactly one
/// arm from a no-op into real behavior instead of hunting for where a
/// wildcard swallowed it.
///
/// Folds unconditionally: [`Model::ai_panel`] is session state, not overlay
/// state (see that field's own doc), so a chunk streamed while the sidebar
/// is closed still has somewhere to go, and reopening the sidebar finds it
/// already there rather than dropped mid-stream.
///
/// ## The permission-overlap degrade
///
/// [`crate::native::ai_panel::AiPanelState::pending_permission`]'s own doc
/// states the invariant [`AiEvent::PermissionRequested`]'s arm branches on:
/// ACP blocks the issuing agent's own turn on the reply, so a conformant
/// agent never has two outstanding requests on one session at once, and a
/// second arriving while the slot is still held is a protocol violation,
/// not a capacity problem a queue would solve.
///
/// The reply shape for that second request is a view-side policy choice,
/// not one the wire spec dictates: `docs/acp-v1-wire-capture.md`'s
/// "Permission-overlap reply legitimacy" section found the closest
/// analogous case (a whole prompt-turn cancellation) described two
/// different ways across the upstream ACP docs -- one page mandates a
/// `RequestPermissionOutcome` `"cancelled"` result, the other's own worked
/// example shows a raw JSON-RPC error instead -- and found no page that
/// addresses this overlap case at all. A normal `Cancelled` outcome is
/// legal under every reading of the schema (the schema's
/// `RequestPermissionResponse` defines only the success shape, and
/// `"cancelled"` is its own "not granted" spelling), so it is what answers
/// the second request, never a guessed-at raw error. The pending slot and
/// its prompt are left exactly as they were: this is an answer to a
/// different request, not an answer to the one the user is still looking
/// at.
pub(super) fn on_ai_event(model: &mut Model, event: AiEvent) -> Vec<Effect> {
    match event {
        AiEvent::PermissionRequested {
            request_id,
            tool_call_id,
            title,
            options,
        } => {
            if model.ai_panel().pending_permission.is_some() {
                return vec![Effect::Ai(AiCommand::AnswerPermission {
                    request_id,
                    outcome: PermissionOutcome::Cancelled,
                })];
            }
            model.ai_panel_mut().pending_permission = Some(PermissionPrompt::new(
                request_id,
                &tool_call_id,
                title,
                options,
            ));
            model.dirty = true;
            // a request arriving while the panel is closed must be visible
            // immediately without stealing focus, since the panel is
            // non-modal and must never redirect the engine's own
            // keystrokes on its own account -- so it opens through the
            // same insertion path the user's own `open`/`toggle` invoke
            // uses; `open_ai_panel` already never takes focus and no-ops
            // when already open, so there is nothing here that needs its
            // own focus-free variant
            return super::open_ai_panel(model);
        }
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
        // a turn ending or the session dying is the same point past which
        // nothing will ever answer a still-open prompt as the driver's own
        // `cancel_open_permissions` settles on the wire (see that method's
        // doc); the panel side has no wire reply to send, but leaving
        // `pending_permission` set here would show a question the agent is
        // no longer waiting on an answer to
        AiEvent::TurnEnded { .. } | AiEvent::SessionCrashed { .. } => {
            if model.ai_panel_mut().pending_permission.take().is_some() {
                model.dirty = true;
            }
        }
        AiEvent::SessionReady { .. }
        | AiEvent::ThoughtChunk { .. }
        | AiEvent::DiffProposed { .. }
        | AiEvent::FsReadRequested { .. }
        | AiEvent::FsWriteRequested { .. } => {}
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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

    fn allow_once(id: &str) -> crate::native::ai_event::PermissionOption {
        crate::native::ai_event::PermissionOption {
            option_id: id.to_string(),
            name: "Allow once".to_string(),
            kind: crate::native::ai_event::PermissionOptionKind::AllowOnce,
        }
    }

    fn permission_requested(request_id: u64, tool_call_id: &str, option_id: &str) -> Msg {
        Msg::Ai(AiEvent::PermissionRequested {
            request_id,
            tool_call_id: tool_call_id.to_string(),
            title: None,
            options: vec![allow_once(option_id)],
        })
    }

    /// Drives through `update()` with `Msg::Ai`, not `on_ai_event` directly,
    /// so the dispatch seam stays load-bearing -- see
    /// `dispatching_msg_ai_through_update_folds_a_message_chunk`'s doc for
    /// what a mutation that disconnects that arm would otherwise slip past.
    #[test]
    fn a_permission_request_becomes_the_pending_prompt_when_none_is_outstanding() {
        let mut model = Model::new();
        let effects = update(&mut model, permission_requested(1, "call_1", "allow-once"));
        assert!(effects.is_empty());
        assert!(model.dirty);
        let prompt = model
            .ai_panel()
            .pending_permission
            .as_ref()
            .expect("pending_permission is Some");
        assert_eq!(prompt.request_id, 1);
        assert_eq!(prompt.options, vec![allow_once("allow-once")]);
    }

    /// The single-slot degrade: a second `PermissionRequested` arriving
    /// while one is already pending is answered immediately with
    /// `Cancelled` -- never queued (the slot has no room), never a raw
    /// error (the wire capture found no basis for one here), and never
    /// touching the first request's still-open slot.
    #[test]
    fn a_second_permission_request_is_answered_cancelled_and_leaves_the_first_untouched() {
        let mut model = Model::new();
        let first_effects = update(&mut model, permission_requested(1, "call_1", "allow-once"));
        assert!(first_effects.is_empty());
        model.dirty = false;

        let second_effects = update(
            &mut model,
            permission_requested(2, "call_2", "allow-once-2"),
        );

        let [Effect::Ai(AiCommand::AnswerPermission {
            request_id: answered_id,
            outcome: PermissionOutcome::Cancelled,
        })] = second_effects.as_slice()
        else {
            panic!("expected one AnswerPermission{{outcome: Cancelled}}, got {second_effects:?}")
        };
        assert_eq!(
            *answered_id, 2,
            "the second request is answered directly, not folded into the panel"
        );
        assert!(
            !model.dirty,
            "answering the second request changes nothing the panel paints"
        );
        let prompt = model
            .ai_panel()
            .pending_permission
            .as_ref()
            .expect("the first prompt is still pending");
        assert_eq!(
            prompt.request_id, 1,
            "the first request's slot is untouched"
        );
        assert_eq!(prompt.options, vec![allow_once("allow-once")]);
    }

    /// The new ruled behavior: a request arriving while the panel is closed
    /// must open it so the prompt is visible, but never as though the user
    /// had asked for it -- `focused` stays false, so a keystroke right
    /// after this still reaches the engine untouched until the user's own
    /// `open`/`toggle` invoke claims the keyboard.
    #[test]
    fn a_permission_request_auto_opens_the_closed_panel_without_taking_focus() {
        let mut model = Model::new();
        assert!(!model.ai_panel_overlay_open());

        let _ = update(&mut model, permission_requested(1, "call_1", "allow-once"));

        assert!(
            model.ai_panel_overlay_open(),
            "the prompt must be visible immediately, not only folded into state"
        );
        assert!(
            !model.ai_panel().focused,
            "auto-open must never claim the keyboard on the user's behalf"
        );
        assert!(model.ai_panel().pending_permission.is_some());
    }

    /// A request arriving while the panel is already open must not disturb
    /// whatever focus it already holds -- `open_ai_panel`'s own no-op check
    /// covers the overlay, this covers that the auto-open call site never
    /// reaches past it to touch `focused` on its own account either.
    #[test]
    fn a_permission_request_with_the_panel_already_open_and_focused_leaves_focus_alone() {
        let mut model = Model::new();
        model.ai_trusted = true;
        let _ = update(&mut model, ai_feature_invoke("open"));
        assert!(model.ai_panel().focused);

        let _ = update(&mut model, permission_requested(1, "call_1", "allow-once"));

        assert!(model.ai_panel().focused);
    }

    /// Critical 3: a turn ending while a permission is still pending must
    /// clear it panel-side -- the driver's own `cancel_open_permissions`
    /// (see `view-ai`'s `driver.rs`) settles the wire reply, but the panel
    /// has its own copy of the question and must stop showing it once
    /// nothing is waiting on an answer to it.
    #[test]
    fn a_turn_ending_clears_a_still_pending_permission() {
        let mut model = Model::new();
        let _ = update(&mut model, permission_requested(1, "call_1", "allow-once"));
        model.dirty = false;

        let effects = update(
            &mut model,
            Msg::Ai(AiEvent::TurnEnded {
                stop_reason: crate::native::ai_event::StopReason::EndTurn,
            }),
        );

        assert!(effects.is_empty());
        assert_eq!(model.ai_panel().pending_permission, None);
        assert!(model.dirty);
    }

    /// The crash-side twin of `a_turn_ending_clears_a_still_pending_permission`.
    #[test]
    fn a_session_crash_clears_a_still_pending_permission() {
        let mut model = Model::new();
        let _ = update(&mut model, permission_requested(1, "call_1", "allow-once"));
        model.dirty = false;

        let effects = update(
            &mut model,
            Msg::Ai(AiEvent::SessionCrashed {
                message: "the agent exited".to_string(),
            }),
        );

        assert!(effects.is_empty());
        assert_eq!(model.ai_panel().pending_permission, None);
        assert!(model.dirty);
    }

    /// A turn ending with nothing pending must not manufacture a repaint:
    /// the common case (every turn that never needed a permission at all)
    /// would otherwise dirty the frame for no visible change.
    #[test]
    fn a_turn_ending_with_nothing_pending_does_not_dirty_the_model() {
        let mut model = Model::new();
        model.dirty = false;

        let _ = update(
            &mut model,
            Msg::Ai(AiEvent::TurnEnded {
                stop_reason: crate::native::ai_event::StopReason::EndTurn,
            }),
        );

        assert!(!model.dirty);
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
