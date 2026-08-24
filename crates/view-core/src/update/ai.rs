//! Folds one [`AiEvent`] into the session state every agent panel reads.

use crate::model::Model;
use crate::msg::Effect;
use crate::native::ai_event::{AiCommand, AiEvent, PermissionOutcome};
use crate::native::ai_panel::{DiffReviewState, PermissionPrompt, StandingAnswer};

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
    if answers_the_prompt(&event) && model.ai_panel_mut().transcript.agent_answered() {
        model.dirty = true;
    }
    match event {
        AiEvent::PermissionRequested {
            request_id,
            tool_call_id,
            title,
            tool_kind,
            options,
        } => {
            // A kind this session already answered for good is answered
            // here rather than asked again (see
            // `AiPanelState::standing_answers`). Ahead of the overlap
            // degrade below because it is not a degrade: this request gets
            // the answer the user already gave for its kind, whatever else
            // is on screen.
            if let Some((kind, answer)) = tool_kind
                .as_deref()
                .and_then(|kind| Some((kind, model.ai_panel().standing_answer(kind)?)))
            {
                if let Some(option) = PermissionPrompt::standing_option(&options, answer) {
                    let outcome = PermissionOutcome::Selected {
                        option_id: option.option_id.clone(),
                    };
                    let line = standing_answer_line(kind, answer);
                    // Both surfaces, because either one alone leaves an
                    // answer view gave on the user's behalf invisible: the
                    // transcript is the durable record of the conversation
                    // it was part of, and it is unread behind a closed
                    // panel -- which is exactly the state a standing answer
                    // makes comfortable to sit in.
                    let mut effects = if model.ai_panel_overlay_open() {
                        Vec::new()
                    } else {
                        model.engine.record_native_notice(line.clone(), false)
                    };
                    model.ai_panel_mut().transcript.append_or_extend(
                        None,
                        &line,
                        crate::native::ai_panel::TranscriptRole::Notice,
                    );
                    model.dirty = true;
                    effects.push(Effect::Ai(AiCommand::AnswerPermission {
                        request_id,
                        outcome,
                    }));
                    return effects;
                }
            }
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
                tool_kind,
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
            let transcript = &mut model.ai_panel_mut().transcript;
            if from_agent {
                transcript.append_or_extend(
                    message_id.as_deref(),
                    &text,
                    crate::native::ai_panel::TranscriptRole::Agent,
                );
            } else {
                // An adapter that replays the user's prompt is restating
                // what the panel already put on screen at submit time, so
                // this is the one fold that can decline a chunk (see
                // `Transcript::append_user_chunk`).
                transcript.append_user_chunk(message_id.as_deref(), &text);
            }
            model.dirty = true;
        }
        AiEvent::ToolCallUpdate {
            tool_call_id,
            title,
            status,
            content,
        } => {
            let transcript = &mut model.ai_panel_mut().transcript;
            transcript.upsert_tool_call(tool_call_id, title, status, content);
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
        // a turn ending is the same point past which nothing will ever
        // answer a still-open prompt as the driver's own
        // `cancel_open_permissions` settles on the wire (see that method's
        // doc); the panel side has no wire reply to send, but leaving
        // `pending_permission` set here would show a question the agent is
        // no longer waiting on an answer to
        AiEvent::TurnEnded { .. } => {
            let panel = model.ai_panel_mut();
            panel.transcript.end_turn();
            let had_pending = panel.pending_permission.take().is_some();
            let was_in_flight = std::mem::take(&mut panel.turn_in_flight);
            if had_pending || was_in_flight {
                model.dirty = true;
            }
        }
        // The session dying is the same point-of-no-return `TurnEnded`'s
        // own arm settles a pending permission at, plus its own persistent
        // surface: `AiPanelState::local_error`'s own doc states why this is
        // never a transient toast on its own -- a crashed long-running
        // session is easy to miss in four seconds -- so the only thing this
        // arm ever schedules a toast for is a session that never reached
        // `SessionReady` at all (`panel.session_id` still `None`). That is
        // the one case with no panel content yet for a user to notice the
        // banner sitting in: the agent panel may not even be open, and a
        // silent failure the first time someone tries the feature reads as
        // "nothing happened" rather than "it broke."
        AiEvent::SessionCrashed { message } => {
            let panel = model.ai_panel_mut();
            panel.transcript.end_turn();
            panel.pending_permission = None;
            panel.turn_in_flight = false;
            let never_became_ready = panel.session_id.is_none();
            // A queued proposal is unread by construction, and the session
            // that authored it is gone: opening it later would put the user
            // in front of a decision made on behalf of an agent that
            // crashed part way through its own work. It goes with the
            // session, and is discarded at the driver so the same diff
            // restated by a recovered session reaches the user again.
            // The review already on screen is not touched -- the user is
            // mid-decision on it, and every accept it can still honour is
            // written to nvim, which needs no agent at all.
            let abandoned = panel.pending_diff_next.take();
            panel.local_error = Some(message.clone());
            model.dirty = true;
            // Every filesystem request the dead session left in flight owes
            // a hold back. No answer goes with them: the session those
            // answers would cross into is gone, and the hidden buffers they
            // pin would otherwise outlive it for the rest of the run.
            let mut effects = super::ai_fs::on_session_ended(model);
            if let Some(queued) = abandoned {
                effects.extend(model.engine.record_native_notice(
                    format!(
                        "AI agent's queued changes to {} were dropped -- the session ended \
                         before that review opened",
                        queued.path.display()
                    ),
                    false,
                ));
                effects.push(Effect::Ai(AiCommand::DiscardProposal {
                    request_id: queued.request_id,
                }));
            }
            if never_became_ready {
                effects.extend(
                    model.engine.record_native_notice(
                        format!("AI agent failed to start: {message}"),
                        false,
                    ),
                );
            }
            return effects;
        }
        // The session's own recovery from whatever `local_error` recorded:
        // a session id only ever arrives once the handshake and
        // `session/new` both succeeded, which is a stronger signal that
        // the agent is working again than any timeout could be.
        AiEvent::SessionReady { session_id } => {
            let panel = model.ai_panel_mut();
            panel.session_id = Some(session_id);
            panel.local_error = None;
            // A standing answer answers questions on the user's behalf, so
            // it lasts exactly as long as the session it was given in --
            // including across a recovery, where the agent that was asked
            // is gone and the one that replaced it has never asked
            // anything.
            panel.clear_standing_answers();
            model.dirty = true;
        }
        // A proposal opens the panel's own diff review. Its hunks are
        // computed here, synchronously, from the whole-file pair the wire
        // carries (`docs/acp-v1-wire-capture.md`'s `Diff` pin has no hunk
        // list in it): the protocol hands over a before/after pair, so the
        // boundaries a user accepts one at a time are this crate's to
        // decide.
        //
        // The buffer the review writes into is resolved rather than
        // assumed: nvim owns buffer identity the same way it owns buffer
        // text, and the proposal names only a path.
        AiEvent::DiffProposed {
            request_id,
            path,
            old_text,
            new_text,
        } => {
            let hunks = crate::native::diff::hunk::diff(old_text.as_deref(), &new_text);
            if hunks.is_empty() {
                // Nothing to decide: the proposal's "after" is what the
                // file already holds, or differs from it only by a
                // trailing newline (see `hunk::diff`'s own doc for why
                // that is the same thing to nvim). Announced rather than
                // dropped in silence: the agent believes it changed the
                // file, and a review that never opened would otherwise
                // look like a lost proposal.
                return model.engine.record_native_notice(
                    format!(
                        "AI agent proposed no change to {} -- the file already matches",
                        path.display()
                    ),
                    false,
                );
            }
            let panel = model.ai_panel_mut();
            if panel.pending_diff.is_some() && panel.pending_diff_next.is_some() {
                // Both slots full. This is the one proposal that is
                // announced and dropped, and it is dropped at the driver
                // too so the agent restating it later reaches the user
                // instead of being deduplicated against a proposal nobody
                // ever saw.
                let notice = model.engine.record_native_notice(
                    format!(
                        "AI agent proposed changes to {} -- dropped, two reviews already waiting",
                        path.display()
                    ),
                    false,
                );
                let mut effects = notice;
                effects.push(Effect::Ai(AiCommand::DiscardProposal { request_id }));
                return effects;
            }
            let generation = model.next_hidden_generation();
            let review = DiffReviewState::new(request_id, path, generation, hunks);
            model.dirty = true;
            // A review the user is part way through is never replaced out
            // from under them: the arriving proposal waits in the queued
            // slot and opens itself the moment that review ends (see
            // `update::review::promote_queued`). Only the review that is
            // actually opening binds now -- a queued one resolves and
            // attaches when its turn comes, so nothing is subscribed to a
            // buffer whose review is not on screen.
            if model.ai_panel().pending_diff.is_some() {
                let path = review.path.clone();
                model.ai_panel_mut().pending_diff_next = Some(review);
                return model.engine.record_native_notice(
                    format!(
                        "AI agent proposed changes to {} -- queued behind the open review",
                        path.display()
                    ),
                    false,
                );
            }
            let effects = vec![review.bind_effect()];
            model.ai_panel_mut().pending_diff = Some(review);
            return effects;
        }
        AiEvent::FsReadRequested {
            request_id,
            path,
            line,
            limit,
        } => return super::ai_fs::on_read_requested(model, request_id, path, line, limit),
        AiEvent::FsWriteRequested {
            request_id,
            path,
            content,
        } => return super::ai_fs::on_write_requested(model, request_id, path, &content),
        AiEvent::ThoughtChunk { message_id, text } => {
            model.ai_panel_mut().transcript.append_or_extend(
                message_id.as_deref(),
                &text,
                crate::native::ai_panel::TranscriptRole::Thought,
            );
            model.dirty = true;
        }
    }
    Vec::new()
}

/// What view says, in the transcript and in the toast behind a closed panel,
/// when it answers a request from a standing answer rather than asking.
///
/// Whether `event` is the agent putting its own content in front of the
/// user -- which is what stands the submitted prompt's spinner back down
/// (see [`crate::native::ai_panel::Transcript::agent_answered`]).
///
/// Asked ahead of the fold rather than repeated inside the arms that
/// qualify, so no arm's own early `return` can skip it, and enumerated
/// rather than defaulted to "any event": the agent working invisibly is not
/// the agent answering. A filesystem request it makes of view, a usage
/// update, or a session going ready puts nothing on screen, and stopping the
/// spinner for one of those would take the only sign of life off the panel
/// with nothing replacing it. A `from_agent: false` chunk is the adapter
/// restating the user's own prompt, which is not the agent speaking either.
///
/// `TurnEnded` and `SessionCrashed` are absent because they stop it through
/// [`crate::native::ai_panel::Transcript::end_turn`] instead, which stops
/// every marker rather than only this one.
fn answers_the_prompt(event: &AiEvent) -> bool {
    matches!(
        event,
        AiEvent::MessageChunk {
            from_agent: true,
            ..
        } | AiEvent::ThoughtChunk { .. }
            | AiEvent::ToolCallUpdate { .. }
            | AiEvent::PlanUpdated { .. }
            | AiEvent::PermissionRequested { .. }
            | AiEvent::DiffProposed { .. }
    )
}

/// One wording for both surfaces so the toast and the record cannot drift,
/// and it names the kind: "view answered for you" is only auditable if the
/// user can tell which of their standing answers did it.
fn standing_answer_line(tool_kind: &str, answer: StandingAnswer) -> String {
    match answer {
        StandingAnswer::Allow => format!("auto-allowed {tool_kind} (standing answer)"),
        StandingAnswer::Reject => format!("auto-refused {tool_kind} (standing answer)"),
    }
}

/// Opens the per-project AI trust confirm as the topmost overlay, the first
/// time a session's `Msg::FeatureInvoke` names the `ai` feature with
/// `model.ai_trusted` still false. `verb` is that `FeatureInvoke`'s own
/// field, carried into the prompt's `Origin` so an affirmative answer can
/// re-dispatch it (see [`crate::msg::Msg::AiTrustResolved`]'s arm).
///
/// A second `ai` invocation before the first prompt is answered replaces
/// its state in place rather than stacking a second one -- looked up by
/// kind via [`Model::ai_trust_prompt_mut`], wherever it sits in the stack,
/// not only when it is focused: a blocked-engine `Prompt` can have taken
/// focus above it in the meantime (the same "keeps its focus instead"
/// stacking rule `open_picker`'s own doc states, since a stray
/// `FeatureInvoke` racing nvim's own confirm block must not steal the
/// answer nvim is still waiting on), and a lookup keyed to focus alone
/// would miss the trust prompt sitting underneath it and stack a duplicate.
/// Only when no trust prompt exists anywhere in the stack yet does this
/// fall to the focus-based insert-beneath/push-new choice.
pub(super) fn open_ai_trust_prompt(model: &mut Model, verb: String) -> Vec<Effect> {
    use crate::model::OverlayKind;

    let message = format!(
        "Trust {} to launch an AI agent? Agents can read and write files in this project.",
        model.cwd.display()
    );
    let state =
        crate::native::prompt::PromptState::ai_trust_prompt(model.cwd.clone(), verb, message);
    if let Some(p) = model.ai_trust_prompt_mut() {
        *p = state;
        model.resize_prompt_overlays();
        model.dirty = true;
        return Vec::new();
    }
    match model.focused_overlay_mut().map(|ov| &ov.kind) {
        Some(OverlayKind::Prompt(_)) => {
            model.insert_overlay_beneath_top(state.overlay_box(), OverlayKind::Prompt(state));
        }
        _ => {
            model.push_overlay(state.overlay_box(), OverlayKind::Prompt(state));
        }
    }
    model.dirty = true;
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

    /// Reasoning was dropped on the floor here for the life of the panel,
    /// which showed as dead air on any agent that thinks before it answers
    /// -- and it is the panel's rendered row that has to prove otherwise,
    /// since a fold into the transcript under the wrong voice is exactly
    /// the failure a length check would call a pass.
    #[test]
    fn a_thought_chunk_reaches_the_panel_in_its_own_voice() {
        let mut model = model_with_open_panel();
        on_ai_event(
            &mut model,
            AiEvent::ThoughtChunk {
                message_id: Some("m1".to_string()),
                text: "weighing it".to_string(),
            },
        );
        assert!(model.dirty);
        assert_eq!(
            model.ai_panel().view(64, 60).rows,
            vec![vec![
                crate::native::views::Span::new(
                    "\u{25e6} ",
                    crate::native::views::StyleRole::AiThought
                ),
                crate::native::views::Span::new(
                    "weighing it",
                    crate::native::views::StyleRole::AiThought
                ),
            ]]
        );
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

    /// An open panel with a prompt just submitted -- the spinner's own
    /// state, armed at submit and looking for whatever stops it.
    fn model_awaiting_the_agent() -> Model {
        let mut model = model_with_open_panel();
        model.ai_panel_mut().turn_in_flight = true;
        model
            .ai_panel_mut()
            .transcript
            .echo_user_prompt("fix the retry policy");
        assert!(model.ai_panel().transcript.is_spinning());
        model.dirty = false;
        model
    }

    /// Every event that puts the agent's own content in front of the user
    /// stands the submitted prompt's marker back down, whichever arm it
    /// takes and whether or not that arm returns early. Enumerated as a
    /// table rather than one test per variant because the property is the
    /// same one for all of them, and a variant the gate forgets is a
    /// spinner that runs behind a panel already full of answers.
    #[test]
    fn the_agents_first_event_stops_the_submitted_prompts_spinner() {
        for event in [
            AiEvent::MessageChunk {
                message_id: Some("m1".to_string()),
                text: "on it".to_string(),
                from_agent: true,
            },
            AiEvent::ThoughtChunk {
                message_id: Some("m1".to_string()),
                text: "weighing it".to_string(),
            },
            AiEvent::ToolCallUpdate {
                tool_call_id: "call_1".to_string(),
                title: "Read file".to_string(),
                status: ToolCallStatus::Pending,
                content: None,
            },
            AiEvent::PlanUpdated {
                entries: Vec::new(),
            },
            AiEvent::PermissionRequested {
                request_id: 1,
                tool_call_id: "call_1".to_string(),
                title: None,
                tool_kind: Some("edit".to_string()),
                options: vec![allow_once("allow-once")],
            },
            AiEvent::DiffProposed {
                request_id: 1,
                path: std::path::PathBuf::from("src/lib.rs"),
                old_text: Some("before\n".to_string()),
                new_text: "after\n".to_string(),
            },
            // The one qualifying arm that returns before setting `dirty`
            // itself (its proposal is a no-op against the file on disk), so
            // the repaint that shows the marker standing down has to come
            // from the gate.
            AiEvent::DiffProposed {
                request_id: 2,
                path: std::path::PathBuf::from("src/lib.rs"),
                old_text: Some("unchanged\n".to_string()),
                new_text: "unchanged\n".to_string(),
            },
            AiEvent::TurnEnded {
                stop_reason: crate::native::ai_event::StopReason::EndTurn,
            },
            AiEvent::SessionCrashed {
                message: "agent exited".to_string(),
            },
        ] {
            let mut model = model_awaiting_the_agent();
            let label = format!("{event:?}");
            let _ = update(&mut model, Msg::Ai(event));
            assert!(
                !model.ai_panel().transcript.is_spinning(),
                "{label} left the prompt's marker spinning"
            );
            assert!(
                model.dirty,
                "{label} stopped the marker without asking for the repaint that shows it"
            );
        }
    }

    /// The other half of the matrix. An agent reading a file, spending
    /// tokens, or coming back up has put nothing on screen, and a spinner
    /// stopped there takes the only sign of life off the panel with nothing
    /// replacing it. Nor is an adapter replaying the user's own prompt the
    /// agent speaking.
    #[test]
    fn an_event_that_shows_the_user_nothing_leaves_the_spinner_running() {
        for event in [
            AiEvent::MessageChunk {
                message_id: Some("wire-1".to_string()),
                text: "fix the retry".to_string(),
                from_agent: false,
            },
            AiEvent::UsageUpdated {
                used: 10,
                size: 100,
                cost: None,
            },
            AiEvent::SessionReady {
                session_id: "s1".to_string(),
            },
            AiEvent::FsReadRequested {
                request_id: 1,
                path: std::path::PathBuf::from("src/lib.rs"),
                line: None,
                limit: None,
            },
        ] {
            let mut model = model_awaiting_the_agent();
            let label = format!("{event:?}");
            let _ = update(&mut model, Msg::Ai(event));
            assert!(
                model.ai_panel().transcript.is_spinning(),
                "{label} stopped a spinner nothing had replaced"
            );
        }
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
            tool_kind: Some("edit".to_string()),
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

    /// A grant answers for the session it was given in and no other. A
    /// recovered or replaced session is a new agent with new work, and the
    /// user has answered nothing for it.
    #[test]
    fn a_session_becoming_ready_drops_every_standing_grant() {
        let mut model = Model::new();
        model
            .ai_panel_mut()
            .record_standing_answer("edit".to_string(), StandingAnswer::Allow);

        let _ = update(
            &mut model,
            Msg::Ai(AiEvent::SessionReady {
                session_id: "sess_2".to_string(),
            }),
        );

        assert!(model.ai_panel().standing_answer("edit").is_none());
        let effects = update(&mut model, permission_requested(1, "call_1", "allow-once"));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Ai(AiCommand::AnswerPermission { .. }))),
            "a dropped grant must leave the question being asked: {effects:?}"
        );
        assert!(model.ai_panel().pending_permission.is_some());
    }

    /// An agent that named no `toolCall.kind` can be answered but never
    /// grants anything: there is nothing to scope a later auto-answer to,
    /// and "every tool" is not what answering one question means.
    #[test]
    fn a_request_naming_no_tool_kind_is_never_answered_by_a_grant() {
        let mut model = Model::new();
        model
            .ai_panel_mut()
            .record_standing_answer("edit".to_string(), StandingAnswer::Allow);

        let effects = update(
            &mut model,
            Msg::Ai(AiEvent::PermissionRequested {
                request_id: 1,
                tool_call_id: "call_1".to_string(),
                title: None,
                tool_kind: None,
                options: vec![allow_once("allow-once")],
            }),
        );

        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Ai(AiCommand::AnswerPermission { .. }))),
            "an unscoped request must be asked about: {effects:?}"
        );
        assert!(model.ai_panel().pending_permission.is_some());
    }

    /// A granted kind whose request offers no allow at all is asked about
    /// rather than answered: a grant says "allow this kind", and there is
    /// nothing here to allow with.
    #[test]
    fn a_granted_kind_offering_only_rejects_still_asks() {
        let mut model = Model::new();
        model
            .ai_panel_mut()
            .record_standing_answer("edit".to_string(), StandingAnswer::Allow);

        let effects = update(
            &mut model,
            Msg::Ai(AiEvent::PermissionRequested {
                request_id: 1,
                tool_call_id: "call_1".to_string(),
                title: None,
                tool_kind: Some("edit".to_string()),
                options: vec![crate::native::ai_event::PermissionOption {
                    option_id: "reject-once".to_string(),
                    name: "Reject".to_string(),
                    kind: crate::native::ai_event::PermissionOptionKind::RejectOnce,
                }],
            }),
        );

        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Ai(AiCommand::AnswerPermission { .. }))),
            "nothing here allows anything: {effects:?}"
        );
        assert!(model.ai_panel().pending_permission.is_some());
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

    /// A turn ending while a permission is still pending must clear it
    /// panel-side -- the driver's own `cancel_open_permissions`
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

    /// `turn_in_flight` -- the flag the `<C-c>` cancel key gates on -- must
    /// clear the same place `pending_permission` does: a turn that ended
    /// leaves nothing for a cancel to interrupt, and a flag left set would
    /// wrongly let `<C-c>` reach a session with no turn running.
    #[test]
    fn a_turn_ending_clears_turn_in_flight() {
        let mut model = Model::new();
        model.ai_panel_mut().turn_in_flight = true;
        model.dirty = false;

        let effects = update(
            &mut model,
            Msg::Ai(AiEvent::TurnEnded {
                stop_reason: crate::native::ai_event::StopReason::EndTurn,
            }),
        );

        assert!(effects.is_empty());
        assert!(!model.ai_panel().turn_in_flight);
        assert!(model.dirty);
    }

    /// The crash-side twin: a session that dies mid-turn reports
    /// `SessionCrashed` rather than `TurnEnded`, and the panel must not be
    /// left thinking a turn is still in flight for a session that no
    /// longer exists to answer a cancel.
    #[test]
    fn a_session_crash_clears_turn_in_flight() {
        let mut model = Model::new();
        model.ai_panel_mut().turn_in_flight = true;

        let _ = update(
            &mut model,
            Msg::Ai(AiEvent::SessionCrashed {
                message: "the agent exited".to_string(),
            }),
        );

        assert!(!model.ai_panel().turn_in_flight);
    }

    fn session_ready(session_id: &str) -> Msg {
        Msg::Ai(AiEvent::SessionReady {
            session_id: session_id.to_string(),
        })
    }

    /// The crash-side twin of `a_turn_ending_clears_a_still_pending_permission`.
    /// Crashed after `SessionReady`, so this is the "was working, then
    /// died" case: panel-local only, never a toast (see the production
    /// arm's own doc for why).
    #[test]
    fn a_session_crash_clears_a_still_pending_permission() {
        let mut model = Model::new();
        let _ = update(&mut model, session_ready("s1"));
        let _ = update(&mut model, permission_requested(1, "call_1", "allow-once"));
        model.dirty = false;

        let effects = update(
            &mut model,
            Msg::Ai(AiEvent::SessionCrashed {
                message: "the agent exited".to_string(),
            }),
        );

        assert!(
            effects.is_empty(),
            "an already-active session's own crash is panel-local only: {effects:?}"
        );
        assert_eq!(model.ai_panel().pending_permission, None);
        assert!(model.dirty);
    }

    /// The wait a first run pays is announced where a user sees it with the
    /// panel closed -- a notice, not the panel's own banner, which is why
    /// this asserts against the message log rather than `local_error`.
    #[test]
    fn a_first_run_provisioning_wait_is_announced() {
        let mut model = Model::new();
        let before = model.engine.messages.entries.len();
        let _ = crate::update::update(
            &mut model,
            Msg::AiProvisioning {
                detail: "provisioning the AI agent claude-code 0.69.0".to_string(),
            },
        );
        assert!(
            model.engine.messages.entries.len() > before,
            "a multi-minute first-run wait must not be silent"
        );
        assert!(model.dirty);
    }

    /// The falsifiable half of the crash-surfacing contract: `local_error`
    /// must actually hold the agent's own message, not just clear the
    /// permission slot -- a doctor row or a panel render reading `None`
    /// here would show a healthy session for one that just died.
    #[test]
    fn a_session_crash_sets_the_panels_local_error_to_the_agents_own_message() {
        let mut model = Model::new();
        let _ = update(&mut model, session_ready("s1"));

        let _ = update(
            &mut model,
            Msg::Ai(AiEvent::SessionCrashed {
                message: "the agent exited (signal: 9)".to_string(),
            }),
        );

        assert_eq!(
            model.ai_panel().local_error,
            Some("the agent exited (signal: 9)".to_string())
        );
    }

    /// The other half of "the runtime loop's next paint stays within
    /// budget" once a crash reaches this fold: `on_ai_event`'s
    /// `SessionCrashed` arm touches no lock, no I/O, and nothing beyond a
    /// handful of `Option`/`Vec` writes on `model`, so the wall clock is
    /// measured here rather than assumed, the same way `view-ai`'s own
    /// `killing_the_agent_mid_turn_reports_the_crash_within_a_tight_bound`
    /// measures the crash reaching the channel in the first place; the two
    /// together cover the whole path from a dead child process to a painted
    /// banner. 1 millisecond is generously wide for pure in-memory work and
    /// tight enough that a fold that started blocking on something would
    /// fail it.
    #[test]
    fn folding_a_session_crash_into_local_error_completes_within_a_tight_bound() {
        let mut model = Model::new();
        let _ = update(&mut model, session_ready("s1"));

        let started = std::time::Instant::now();
        let _ = update(
            &mut model,
            Msg::Ai(AiEvent::SessionCrashed {
                message: "the agent exited (signal: 9)".to_string(),
            }),
        );
        let elapsed = started.elapsed();

        assert!(model.ai_panel().local_error.is_some());
        assert!(
            elapsed < std::time::Duration::from_millis(1),
            "folding a crash into local_error must not stall the paint that \
             follows it: took {elapsed:?}"
        );
    }

    /// The one case `SessionCrashed`'s own arm additionally toasts: a
    /// session that never reached `SessionReady` at all, where the panel
    /// may not even be open for `local_error`'s own banner to be visible
    /// in -- a failed first attempt at the feature must not read as
    /// "nothing happened."
    #[test]
    fn a_session_crash_before_the_session_ever_became_ready_also_toasts() {
        let mut model = Model::new();

        let effects = update(
            &mut model,
            Msg::Ai(AiEvent::SessionCrashed {
                message: "could not provision the claude-code agent".to_string(),
            }),
        );

        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::ScheduleToastExpiry { .. })),
            "a spawn failure must schedule a toast, got {effects:?}"
        );
        let entry = model.engine.messages.entries.last().expect("a notice");
        let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
        assert!(
            text.contains("could not provision the claude-code agent"),
            "the toast must name what the agent reported: {text:?}"
        );
        assert_eq!(
            model.ai_panel().local_error,
            Some("could not provision the claude-code agent".to_string()),
            "the persistent banner is still owed alongside the toast"
        );
    }

    /// The recovery half: a fresh `SessionReady` must clear whatever
    /// `local_error` a previous crash left behind, or a doctor row / panel
    /// render would keep reporting a dead session the agent has since
    /// replaced.
    #[test]
    fn a_session_ready_binds_the_session_id_and_clears_any_previous_local_error() {
        let mut model = Model::new();
        let _ = update(
            &mut model,
            Msg::Ai(AiEvent::SessionCrashed {
                message: "the agent exited".to_string(),
            }),
        );
        assert!(model.ai_panel().local_error.is_some());
        model.dirty = false;

        let effects = update(&mut model, session_ready("s2"));

        assert!(effects.is_empty());
        assert_eq!(model.ai_panel().session_id, Some("s2".to_string()));
        assert_eq!(model.ai_panel().local_error, None);
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

    /// The explicit dismiss action `local_error`'s own doc promises exists
    /// independent of the next `SessionReady` clearing it automatically: a
    /// user who has read the crash banner and does not intend to retry can
    /// clear it directly, reachable whether or not the panel is entered.
    #[test]
    fn the_dismiss_verb_clears_a_set_local_error() {
        let mut model = Model::new();
        model.ai_panel_mut().local_error = Some("the agent exited".to_string());
        model.dirty = false;

        let effects = update(&mut model, ai_feature_invoke("dismiss"));

        assert!(effects.is_empty());
        assert_eq!(model.ai_panel().local_error, None);
        assert!(model.dirty);
    }

    /// Dismissing with nothing set is a true no-op: no effect, and no
    /// spurious repaint for a banner that was never on screen.
    #[test]
    fn the_dismiss_verb_with_no_local_error_is_a_noop() {
        let mut model = Model::new();
        model.dirty = false;

        let effects = update(&mut model, ai_feature_invoke("dismiss"));

        assert!(effects.is_empty());
        assert_eq!(model.ai_panel().local_error, None);
        assert!(!model.dirty);
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
