//! The pure state transition: `Msg` in, `Model` mutated, `Effect`s out.

use crate::model::{Focus, Model, MouseCapture, OverlayKind};
use crate::msg::{
    DeleteConfirmOutcome, Effect, EngineRequest, Key, MouseInput, Msg, ReplyValue, RpcCall,
};
use crate::native::ai_event::{AiCommand, PermissionOutcome};
use crate::native::diff::BufTextChangedEvent;
use crate::native::geometry::{Anchor, OverlayBox};
use crate::native::statusline::SegmentUpdate;
use crate::native::supervision::WedgeKind;

mod ai;
mod ai_fs;
mod review;
mod supervision;
mod ui_event;
mod watch;

use ai::{on_ai_event, open_ai_trust_prompt};
use review::review_key;
use supervision::{note_engine_liveness, note_supervision_choice};
use ui_event::apply_ui_event;
use watch::{on_checktime_reply, on_external_watch_degraded, on_external_writes_detected};

/// Converts a filesystem path to the UTF-8 string an `RpcCall` path field
/// carries, substituting the replacement character for any byte sequence
/// that is not valid UTF-8 rather than failing: nvim's own path arguments
/// are untyped strings, so a lossy round-trip here matches the contract
/// every wire path already accepts.
fn path_to_wire(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Applies one message to `model`, returning the effects the executor must
/// carry out. Never blocks and never performs I/O: every side effect crosses
/// the boundary as a returned [`Effect`] instead of being performed here.
#[must_use]
pub fn update(model: &mut Model, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Key(Key { notation }) => {
            // ahead of every other keypress rule: the busy modal is the
            // newest thing on screen, so a key naming one of its choices is
            // folded into the episode's bookkeeping here, and then routed
            // below exactly as it would have been with no modal open -- see
            // `note_supervision_choice` on which of those keys the routing
            // then reaches nvim with.
            //
            // While the engine owns the keyboard, which is the same
            // condition as "this key is about to reach nvim": an overlay
            // that takes focus is answering the key itself, and the
            // annunciator stacked over it must not read that answer as its
            // own -- one <Esc> at a picker under this modal would otherwise
            // close both, spending the episode's single offer on a
            // dismissal the user never made.
            //
            // Except for a connection that is gone, which is answered
            // wherever the stack sits. Its choices collide with nothing a
            // focused overlay answers (`a_focused_overlays_keys_never_collide_
            // with_a_dead_engines_choices`), and it offers no dismissal at
            // all, so there is no offer to spend by accident -- while a
            // modal painting keys it would refuse is an editor telling its
            // user the way out is a key that does nothing.
            let answers_anywhere = model
                .engine_busy()
                .is_some_and(|open| open.kind == WedgeKind::Dead);
            let mut effects = if answers_anywhere || model.focus() == Focus::Engine {
                note_supervision_choice(model, &notation)
            } else {
                Vec::new()
            };
            effects.extend(route_key(model, notation));
            effects
        }
        Msg::Paste(text) => match model.focus() {
            // never replayed as nvim_input keystrokes: one undo unit, no
            // mapping interference, matching nvim_paste's own contract
            Focus::Engine => vec![Effect::Rpc(RpcCall::Paste { text })],
            Focus::Native(_) => Vec::new(),
        },
        Msg::Mouse(input) => route_mouse(model, input),
        Msg::Redraw(events) => {
            let mut effects = Vec::new();
            for ev in events {
                effects.extend(apply_ui_event(model, ev));
            }
            effects
        }
        // loop plumbing tokens: the loop resolves the damage behind
        // RedrawReady, and the exit status behind EngineStopped, before
        // update() ever sees them. EngineStopped still arrives here when the
        // loop judged the stop a death rather than the session's own ending
        // (`SupervisionState::note_engine_stop`) -- the fold that acts on it
        // is the liveness reading a later pass takes, never this arm.
        // EngineReady is consumed even earlier, by startup's pre-attach
        // draining loop, before the steady-state loop this match belongs to
        // ever starts, so that arm is unreachable in practice but kept for
        // the same defensive-totality reason
        Msg::RedrawReady | Msg::EngineStopped { .. } | Msg::EngineReady => Vec::new(),
        // asked before this connection can say it has finished starting, and
        // deliberately: a startup error parks nvim ahead of its own
        // `VimEnter`, so a chain that waited for that event would never hear
        // about the recovery that failed. The reading is gated engine-side
        // and answers "nothing yet" until `VimEnter` has fired
        Msg::EngineAttached => vec![Effect::Rpc(RpcCall::ProbeSwapRecovery {
            generation: model.supervision.begin_swap_probe(),
        })],
        Msg::EngineDown(exit) => {
            model.running = false;
            vec![Effect::Quit {
                exit_code: exit.code.unwrap_or(1),
            }]
        }
        Msg::EngineRequest(EngineRequest::VimEnter { token }) => vec![
            Effect::Reply {
                token,
                value: ReplyValue::Nil,
            },
            // after the reply, never before it: nvim is blocked inside the
            // `rpcrequest` this answers, and a probe queued ahead of the
            // answer would be waiting on the engine that is waiting on view.
            // This is also the first moment the reading is final -- nvim
            // opens the files it was given, and replays their swap files,
            // before `VimEnter` fires
            Effect::Rpc(RpcCall::ProbeSwapRecovery {
                generation: model.supervision.renew_swap_probe(),
            }),
        ],
        // delegated, not answered here: the worker owns the reply (see
        // Effect::ClipboardRead/ClipboardWrite's docs), so this loop never
        // blocks on the system clipboard the way a direct Effect::Reply
        // would require reading it inline to produce
        Msg::EngineRequest(EngineRequest::ClipboardGet { token, register }) => {
            vec![Effect::ClipboardRead { token, register }]
        }
        Msg::EngineRequest(EngineRequest::ClipboardSet {
            token,
            register,
            lines,
            regtype,
        }) => vec![
            Effect::ClipboardWrite {
                token,
                register,
                lines: lines.clone(),
                regtype,
            },
            Effect::Osc52Copy {
                register,
                lines,
                regtype,
            },
        ],
        Msg::Resized { width, height } => {
            // an already-applied size is a no-op, not a repeat: the
            // frontend may fold a resize in ahead of this message to keep
            // the next frame's paint area current, and re-running the arm
            // would then dirty the model and re-issue TryResize for a
            // change that already happened
            if (model.term_width, model.term_height) == (width, height) {
                return Vec::new();
            }
            model.term_width = width;
            model.term_height = height;
            // the paint area is sourced from these fields, so the frame that
            // renders them is this frontend's own concern: `grid_target`
            // clamps, so a resize that leaves the grid unchanged draws no
            // engine redraw at all and would otherwise never repaint
            model.dirty = true;
            let (grid_width, grid_height) = model.grid_target();
            vec![Effect::Rpc(RpcCall::TryResize {
                width: grid_width,
                height: grid_height,
            })]
        }
        Msg::HlProbeReply { generation, fg, bg } => {
            // guards the write, not just the read: without this, a
            // reordered stale reply (an older generation's probe answered
            // after a newer one) would overwrite the newer reply's already-
            // correct confirmed state, permanently losing it since only one
            // slot is kept -- see HlTable::confirmed's doc comment
            if generation == model.engine.hl().probe_generation() {
                model
                    .engine
                    .confirm_hl_defaults(crate::hl::ProbedDefaults { generation, fg, bg });
                // the paint loop's `if model.dirty` gate is the only thing
                // that triggers a repaint; without this, a probe reply that
                // arrives after the frame it corrects has already painted
                // (the paint loop never awaits RPC, so this is the common
                // case) would sit applied-but-unpainted until some other,
                // unrelated event happens to mark dirty next
                model.dirty = true;
            }
            Vec::new()
        }
        Msg::HeartbeatReply { .. } => {
            // the acknowledgement itself is recorded by the runtime loop's
            // liveness watch on the way in, before this arm ever runs; the
            // model holds no read-side state for it to land in, and marking
            // the frame dirty for a reading that changed nothing visible
            // would repaint on every probe interval for the whole session
            Vec::new()
        }
        Msg::EngineLiveness {
            wedge,
            observed_for,
        } => note_engine_liveness(model, wedge, observed_for),
        Msg::FeatureInvoke { feature, verb } => {
            // `ai_enabled` gates ahead of `ai_trusted`: a feature that is
            // off has nothing to trust it for, so prompting first would ask
            // a question whose every answer is thrown away the moment the
            // disabled check runs anyway.
            if feature == "ai" && !model.ai_enabled {
                return notice_ai_disabled(model);
            }
            // Ahead of the trust gate below on purpose: dismissing a crash
            // banner launches no agent and asks no permission of its own,
            // so it needs no trust decision to reach -- routing it through
            // `open_ai_trust_prompt` first would show "trust this project
            // to launch an AI agent?" for an action that launches nothing.
            if feature == "ai" && verb == "dismiss" {
                if model.ai_panel_mut().local_error.take().is_some() {
                    model.dirty = true;
                }
                return Vec::new();
            }
            // `ai_trusted` is plain data the bin seeded (see `Model`'s own
            // doc on the field): the pure core decides the gate from it and
            // names nothing outside itself to do so. Checked ahead of every
            // other feature below, since an untrusted project must never
            // reach whatever the `ai` feature does next.
            if feature == "ai" && !model.ai_trusted {
                return open_ai_trust_prompt(model, verb);
            }
            if feature == "ai" && verb == "toggle" {
                return toggle_ai_panel(model);
            }
            if feature == "ai" && (verb == "open" || verb == "focus") {
                // an explicit user invoke, unlike a `PermissionRequested`
                // auto-open (`update::ai::on_ai_event`), is the one action
                // that claims the panel's keyboard focus -- see
                // `AiPanelState::focused`'s own doc. "open" and "focus" do
                // the identical thing (open if closed, then claim focus
                // either way): "focus" exists as the name a discoverability
                // hint can point at that still reads correctly when the
                // panel is already open (an agent's own auto-open leaves it
                // unentered), where "open" would read oddly.
                let effects = open_ai_panel(model);
                // `open_ai_panel` already dirties on a push; this only adds
                // a repaint for the case it does not cover -- an
                // already-open, not-yet-entered panel (auto-opened by a
                // permission request) taking focus for the first time.
                // Re-invoking on an already-entered panel is a true no-op:
                // nothing about the paint frame depends on `focused` beyond
                // whether it is set at all.
                if !model.ai_panel().focused {
                    model.dirty = true;
                }
                model.ai_panel_mut().focused = true;
                return effects;
            }
            if feature == "ai" && verb == "close" {
                // `close_ai_panel` itself clears `AiPanelState::focused`, at
                // the single authoritative closing point
                if model.close_ai_panel() {
                    model.dirty = true;
                }
                return Vec::new();
            }
            if feature == "picker" {
                if let Some(source) = picker_source_for_verb(&verb, &model.cwd) {
                    return open_picker(model, source);
                }
            }
            if feature == "tree" && verb == "toggle" {
                return toggle_tree_sidebar(model);
            }
            if feature == "notifications" && verb == "history" {
                return open_message_history(model);
            }
            // a bare `:View` (both tokens empty) is the discoverability
            // entry point: nothing was asked for, so nothing was invoked,
            // but nvim's own command-line completion for `:View` already
            // lists every registered feature/verb form (see
            // `nvim_api::register_mappings`), so reopening the command line
            // pre-seeded with the command name puts that completion one
            // `<Tab>` away inside the palette itself -- a strictly better
            // answer than a toast alone to a user wondering what to type.
            // Every other unmatched (feature, verb) pair -- a typo, a form
            // this build has registered no handler for -- still only gets
            // the notice below; reopening the cmdline for those would
            // replay whatever malformed thing was just typed.
            if feature.is_empty() && verb.is_empty() {
                model.dirty = true;
                let mut effects = model
                    .engine
                    .record_native_notice(feature_invoke_notice(&feature, &verb, false), false);
                effects.push(Effect::Rpc(RpcCall::Input {
                    notation: format!(":{} ", crate::native::mappings::COMMAND),
                }));
                return effects;
            }
            // no native feature has an overlay to open yet, and returning
            // nothing at all here is indistinguishable to a user from a key
            // that never registered: the entry point is answered with a
            // visible line saying it arrived and this build has nothing
            // behind it, through the same message surface every other
            // locally-originated notice uses.
            let known = crate::native::mappings::default_maps()
                .iter()
                .any(|spec| spec.feature == feature && spec.verb == verb);
            let notice = feature_invoke_notice(&feature, &verb, known);
            model.dirty = true;
            model.engine.record_native_notice(notice, false)
        }
        Msg::SwapRecovered {
            generation,
            count,
            reported,
            failure,
            empty,
        } => supervision::note_swap_recovery(model, generation, count, reported, failure, empty),
        Msg::MappingsClaimed { claimed } => {
            model.record_claimed_keys(claimed);
            Vec::new()
        }
        // no effect and no state of its own: the colors arrive through the
        // redraw stream and every frame re-derives its `Theme` from the
        // live highlight table regardless. Marking the model dirty is the
        // whole answer -- it guarantees the switch reaches the screen even
        // when nvim's own batch carries no cell damage the paint loop would
        // otherwise repaint for.
        Msg::ColorSchemeChanged { .. } => {
            model.dirty = true;
            Vec::new()
        }
        Msg::DiagnosticsChanged { errors, warnings } => {
            model
                .engine
                .statusline
                .apply(SegmentUpdate::Diagnostics { errors, warnings });
            model.dirty = true;
            Vec::new()
        }
        Msg::GitBranchChanged { branch } => {
            model
                .engine
                .statusline
                .apply(SegmentUpdate::GitBranch(branch));
            model.dirty = true;
            tree_git_refresh_effect(model)
        }
        Msg::BufferChanged { name, modified } => {
            model
                .engine
                .statusline
                .apply(SegmentUpdate::Buffer { name, modified });
            model.dirty = true;
            tree_git_refresh_effect(model)
        }
        // The key-dispatch-path arm: one event per keystroke in an attached
        // buffer, folded into the open review's hunks and nothing else. The
        // work is O(open hunks) and allocation-free for an edit outside
        // every anchor (see `native::diff::rebase`), so this holds the
        // O(edit size) contract `RpcCall::BufAttach` states rather than
        // adding a term in buffer size on top of it.
        Msg::BufTextChanged {
            buf,
            generation,
            firstline,
            lastline,
            linedata,
            changedtick,
            desynced,
        } => review::on_buf_text_changed(
            model,
            BufTextChangedEvent {
                buf,
                generation,
                firstline,
                lastline,
                linedata,
                changedtick,
                desynced,
            },
        ),
        Msg::BufDetached { buf, generation } => review::on_buf_detached(model, buf, generation),
        // One counter numbers every hidden-buffer resolve this crate issues
        // (see `Model::next_hidden_generation`), so exactly one of the two
        // owners below ever claims a given reply: the agent's filesystem
        // requests answer first and hand the message on untouched when the
        // generation is not one of theirs.
        Msg::HiddenBufferLoaded {
            generation,
            buf,
            changedtick,
            created: _,
        } => ai_fs::on_hidden_buffer_loaded(model, generation, buf, changedtick).unwrap_or_else(
            || review::on_hidden_buffer_loaded(model, generation, buf, changedtick),
        ),
        Msg::AiFsReadReply { request_id, result } => {
            ai_fs::on_read_reply(model, request_id, result)
        }
        Msg::AiFsWriteReply { request_id, result } => {
            ai_fs::on_write_reply(model, request_id, result)
        }
        Msg::ExternalWritesDetected { paths } => on_external_writes_detected(model, paths),
        Msg::ExternalWatchDegraded { reason } => on_external_watch_degraded(model, reason),
        // `request_id` is the engine's own correlation key, consumed by the
        // waiter table that produced `results`; by the time a reply is a
        // `Msg` there is no per-request state left in this crate for it to
        // name, and what a forced reload's reply must be told apart from
        // lives in `CheckTimeOutcome` rather than in a remembered id
        Msg::CheckTimeReply {
            request_id: _,
            results,
        } => on_checktime_reply(model, results),
        Msg::BufWriteRefused { buf, generation } => {
            review::on_buf_write_refused(model, buf, generation)
        }
        Msg::BufWriteApplied {
            buf,
            generation,
            changedtick,
        } => review::on_buf_write_applied(model, buf, generation, changedtick),
        Msg::PickerResults { generation, items } => {
            let Some(p) = model.picker_mut() else {
                return Vec::new();
            };
            p.apply_results(generation, items);
            let effects = picker_preview_request(p);
            model.dirty = true;
            effects
        }
        // not matched here: the engine's Lua reply lists every listed
        // buffer unconditionally, with no needle to filter by, so the
        // actual fuzzy match still has to happen in the matcher worker --
        // this arm's whole job is turning the raw reply into a corpus and
        // handing it to the worker as `resolved`, gated on the generation
        // still being the picker's own (see `Effect::PickerQuery`'s doc for
        // why `Source::Buffers` alone needs `resolved` at all)
        Msg::PickerBufferList { generation, names } => {
            let Some(p) = model.picker_mut() else {
                return Vec::new();
            };
            if p.generation() != generation {
                return Vec::new();
            }
            let needle = p.query().to_string();
            let items = names
                .into_iter()
                .map(|name| {
                    crate::native::picker::PickerItem::new(if name.is_empty() {
                        "[No Name]".to_string()
                    } else {
                        name
                    })
                })
                .collect();
            vec![Effect::PickerQuery {
                generation,
                needle,
                source: crate::native::picker::Source::Buffers,
                resolved: Some(items),
            }]
        }
        Msg::ToastExpired { id } => {
            // races the same entry being cleared, replaced (which stamps a
            // fresh id, so this id simply no longer matches anything), or
            // dismissed by a keypress in the meantime -- losing that race is
            // "already handled", not an error, so this filters by id rather
            // than asserting the entry is still present
            let before = model.engine.messages.entries.len();
            model.engine.messages.entries.retain(|e| e.id() != id);
            if model.engine.messages.entries.len() != before {
                model.dirty = true;
            }
            Vec::new()
        }
        // nvim owns all buffer text (see the crate's hard rule): a `loaded:
        // true` reply is applied straight to the preview pane, but `loaded:
        // false` means there is no buffer to read from at all, and the only
        // remaining source of truth is disk -- handed off to
        // `Effect::PickerPreviewFallback` rather than treated as "nothing to
        // preview", so a path with no open buffer still gets a preview.
        Msg::PickerPreviewReply {
            generation,
            path,
            loaded,
            lines,
        } => {
            let Some(p) = model.picker_mut() else {
                return Vec::new();
            };
            if p.preview_generation() != generation {
                return Vec::new();
            }
            if loaded {
                p.apply_preview(generation, lines);
                model.dirty = true;
                Vec::new()
            } else {
                vec![Effect::PickerPreviewFallback { generation, path }]
            }
        }
        Msg::PickerPreviewFile { generation, lines } => {
            let Some(p) = model.picker_mut() else {
                return Vec::new();
            };
            if p.preview_generation() != generation {
                return Vec::new();
            }
            p.apply_preview(generation, lines.unwrap_or_default());
            model.dirty = true;
            Vec::new()
        }
        Msg::TreeScanResult {
            generation,
            entries,
        } => {
            let Some(t) = model.tree_mut() else {
                return Vec::new();
            };
            t.apply_scan(generation, entries);
            model.dirty = true;
            Vec::new()
        }
        Msg::TreeGitResult {
            generation,
            status,
            timed_out,
        } => {
            let Some(t) = model.tree_mut() else {
                return Vec::new();
            };
            let reissue = t.apply_git(generation, status);
            model.dirty = true;
            let mut effects = if reissue {
                tree_git_refresh_effect(model)
            } else {
                Vec::new()
            };
            // apply_git above already cleared TreeState's in-flight flag
            // unconditionally, so a wedged git that hit its own deadline
            // never permanently suppresses a later refresh -- this notice
            // is purely informational, telling the user the decorations
            // they see may be stale rather than leaving them to wonder why
            // nothing updated.
            if timed_out {
                effects.extend(model.engine.record_native_notice(
                    "view: git status timed out; tree decorations may be stale".to_string(),
                    false,
                ));
            }
            effects
        }
        // a refused rename (`ok: false`) has nothing else to try -- see
        // `RpcCall::RenameFile`'s doc -- so it surfaces as a notice and
        // leaves the tree exactly as it was before the rename was issued;
        // a successful one requires an explicit rescan since
        // `nvim_buf_set_name` fires no autocmd the bridge could pick up on
        // its own (see docs/tree-rename-wire-capture.md). `generation` here
        // is not compared against the tree's own counters: it names the
        // rename request itself (see `Waiter::Rename`), not a scan or a
        // git refresh, so a successful reply always rescans unconditionally.
        Msg::TreeRenameReply { generation: _, ok } => {
            model.dirty = true;
            if !ok {
                return model.engine.record_native_notice(
                    "view: rename failed (destination exists?)".to_string(),
                    false,
                );
            }
            let Some(t) = model.tree_mut() else {
                return Vec::new();
            };
            let root = t.root().to_path_buf();
            let rescan_generation = t.request_rescan();
            vec![Effect::TreeScan {
                generation: rescan_generation,
                root,
            }]
        }
        // the reply is this prompt's definitive resolution, unlike the
        // ordinary confirm() dialogs `Msg::Key`'s lazy-dismiss guard exists
        // for (those have no reply channel of their own to hook into): this
        // pops the Prompt overlay itself rather than waiting for a keypress
        // that may never come before the tree needs to repaint the create.
        Msg::TreeCreatePromptReply { generation, name } => {
            dismiss_top_prompt(model);
            model.dirty = true;
            let Some(t) = model.tree_mut() else {
                return Vec::new();
            };
            if generation != t.generation() {
                return Vec::new();
            }
            let Some(name) = name.filter(|n| !n.is_empty()) else {
                return Vec::new();
            };
            // The typed answer is untrusted user input, not a path this
            // prompt's own UX ever offers a way to build safely: an
            // absolute answer (`/etc/passwd`) replaces `target_dir`
            // entirely under `Path::join`'s own semantics (the joined path
            // becomes whatever was typed, ignoring the base), and a `..`
            // component climbs out of the tree root the same way from a
            // relative one. This prompt's contract is "one leaf name beside
            // the selection" (the same contract a file manager's own
            // new-file action offers) -- nested creation was never a
            // feature it supports -- so a single `Component::Normal` is the
            // only shape accepted; anything else is refused with a visible
            // notice rather than silently normalized, since normalizing a
            // `..`-laden answer still risks landing somewhere the tree was
            // never rooted at.
            let is_single_plain_component = matches!(
                std::path::Path::new(&name).components().collect::<Vec<_>>()[..],
                [std::path::Component::Normal(_)]
            );
            if !is_single_plain_component {
                return model
                    .engine
                    .record_native_notice(format!("view: invalid file name {name:?}"), false);
            }
            // creates inside the selected directory, or alongside the
            // selected file (its parent), or at the tree's own root when
            // nothing is selected -- the same "beside what's under the
            // cursor" placement a file manager's own new-file action uses
            let target_dir = match t.selected_entry() {
                Some(entry) if entry.is_dir => t.root().join(&entry.path),
                Some(entry) => t
                    .root()
                    .join(&entry.path)
                    .parent()
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| t.root().to_path_buf()),
                None => t.root().to_path_buf(),
            };
            vec![Effect::TreeCreateFile {
                path: target_dir.join(name),
                generation: t.generation(),
            }]
        }
        Msg::TreeRenamePromptReply {
            generation,
            old_path,
            name,
        } => {
            dismiss_top_prompt(model);
            model.dirty = true;
            let Some(t) = model.tree_mut() else {
                return Vec::new();
            };
            if generation != t.generation() {
                return Vec::new();
            }
            let Some(new_name) = name.filter(|n| !n.is_empty()) else {
                return Vec::new();
            };
            // Same untrusted-typed-answer shape as `TreeCreatePromptReply`
            // above (see that arm's own comment) -- only more dangerous
            // here, since `RenameFile` goes over RPC straight to nvim
            // (`runtime.rs`'s effect executor), and unlike create's
            // `create_new(true)` a rename has no create-only guard: an
            // escaped destination that already exists is silently
            // overwritten rather than refused. The rename prompt's
            // contract is the same "one leaf name beside the original"
            // a file manager's own rename action offers, so it gets the
            // identical single-`Component::Normal` guard.
            let is_single_plain_component = matches!(
                std::path::Path::new(&new_name)
                    .components()
                    .collect::<Vec<_>>()[..],
                [std::path::Component::Normal(_)]
            );
            if !is_single_plain_component {
                return model
                    .engine
                    .record_native_notice(format!("view: invalid file name {new_name:?}"), false);
            }
            let old = std::path::PathBuf::from(&old_path);
            let Some(parent) = old.parent() else {
                return Vec::new();
            };
            let new_path = parent.join(new_name);
            vec![Effect::Rpc(RpcCall::RenameFile {
                old_path,
                new_path: path_to_wire(&new_path),
                generation: t.generation(),
            })]
        }
        Msg::TreeDeleteConfirmReply {
            generation,
            path,
            outcome,
        } => {
            dismiss_top_prompt(model);
            model.dirty = true;
            match outcome {
                DeleteConfirmOutcome::Declined => Vec::new(),
                DeleteConfirmOutcome::BufferOpen => model
                    .engine
                    .record_native_notice("view: buffer open -- close it first".to_string(), false),
                DeleteConfirmOutcome::Confirmed => {
                    let Some(t) = model.tree_mut() else {
                        return Vec::new();
                    };
                    if generation != t.generation() {
                        return Vec::new();
                    }
                    vec![Effect::TreeDeleteFile {
                        path: std::path::PathBuf::from(path),
                        generation: t.generation(),
                    }]
                }
            }
        }
        // mirrors `Msg::TreeRenameReply`'s own discard-generation, rescan-
        // on-success shape exactly: `generation` here names the create/
        // delete call itself, not a tree state to compare against, so a
        // successful reply always rescans unconditionally.
        Msg::TreeCreateFileResult { generation: _, ok } => {
            model.dirty = true;
            if !ok {
                return model.engine.record_native_notice(
                    "view: create failed (already exists?)".to_string(),
                    false,
                );
            }
            let Some(t) = model.tree_mut() else {
                return Vec::new();
            };
            let root = t.root().to_path_buf();
            let rescan_generation = t.request_rescan();
            vec![Effect::TreeScan {
                generation: rescan_generation,
                root,
            }]
        }
        Msg::TreeDeleteFileResult { generation: _, ok } => {
            model.dirty = true;
            if !ok {
                return model
                    .engine
                    .record_native_notice("view: delete failed".to_string(), false);
            }
            let Some(t) = model.tree_mut() else {
                return Vec::new();
            };
            let root = t.root().to_path_buf();
            let rescan_generation = t.request_rescan();
            vec![Effect::TreeScan {
                generation: rescan_generation,
                root,
            }]
        }
        // The agent vocabulary crosses into the loop here; `ai::on_ai_event`
        // folds what the panel renders and no-ops the rest, unconditionally
        // into `Model::ai_panel` (session state, not overlay state -- see
        // that field's doc): a chunk streamed while the sidebar is closed
        // still folds, and reopening finds it there. Written as its own arm
        // rather than folded into a wildcard on purpose: this match has
        // none, which is what makes a later `AiEvent` arm impossible to add
        // without every consumer of it being recompiled against the
        // addition.
        Msg::Ai(event) => on_ai_event(model, event),
        // A write that failed after an affirmative answer folds back to
        // `trusted: false` on the same terms a declined answer does (see
        // `Effect::AiTrustSet`'s own doc): either way the durable fact is
        // "not trusted", and this arm cannot tell the two apart from the
        // bool alone, so one notice covers both -- it names the way back in
        // rather than claiming to know why the gate did not open. An
        // affirmative answer instead completes the intent the gate
        // interrupted: `verb` is the pending `Msg::FeatureInvoke` the prompt
        // carried through `Effect::AiTrustSet` (see that effect's own doc),
        // re-dispatched here now that `model.ai_trusted` reads true -- a
        // user who types `:View ai` and answers Yes must see the `ai`
        // feature proceed in one flow, not a closed prompt with nothing
        // behind it that needs a second, undiscoverable invocation.
        Msg::AiTrustResolved { trusted, verb } => {
            model.ai_trusted = trusted;
            if trusted {
                update(
                    model,
                    Msg::FeatureInvoke {
                        feature: "ai".to_string(),
                        verb,
                    },
                )
            } else {
                model.dirty = true;
                model.engine.record_native_notice(
                    "view: AI agent access is not enabled for this project -- invoke :View ai again to be asked".to_string(),
                    false,
                )
            }
        }
    }
}

/// Routes one keypress to whatever currently owns the keyboard, after
/// [`note_supervision_choice`] has had its look at it.
///
/// Split out of [`update`]'s `Msg::Key` arm so that the supervision modal's
/// bookkeeping can run first without consuming the key: this is the routing
/// a keypress gets whether or not that modal is on screen, which is the
/// whole of what makes that modal free to answer.
fn route_key(model: &mut Model, notation: String) -> Vec<Effect> {
    // any keypress is "the user is reading again": gives a
    // transient (info-kind) toast a readable duration bounded by
    // real activity instead of a wall-clock timer the runtime
    // never delivers to `update`; runs regardless of focus, since
    // the semantic is user activity, not specifically engine input
    let cmdline_open = model.engine.cmdline.is_some();
    if model
        .engine
        .messages
        .dismiss_transient_on_keypress(cmdline_open)
    {
        model.dirty = true;
    }
    // a resolved prompt's overlay follows the same lazy-dismiss
    // timing as its underlying MessageEntry (see
    // dismiss_transient_on_keypress): nvim sends no msg_clear on
    // resolution, and cmdline_hide alone cannot tell "resolved"
    // apart from "about to re-arm" (both start with the identical
    // cmdline_hide + flush; the wire only disambiguates once a
    // later, separate redraw batch either does or doesn't bring a
    // new cmdline_show), so the overlay closes on the first
    // keypress observed after the cmdline has actually stayed
    // closed, exactly when the toast falls back to ordinary
    // transient rules
    // excludes the AI trust prompt and the external-write conflict prompt:
    // neither has a paired cmdline_show to have gone quiet, so
    // `cmdline_open` reads `false` for either from the moment it opens, and
    // this heuristic would otherwise pop it before the answer arm below
    // ever sees the keystroke meant to resolve it
    if !cmdline_open
        && matches!(
            model.focused_overlay_mut().map(|ov| &ov.kind),
            Some(OverlayKind::Prompt(p))
                if p.ai_trust_project_root().is_none()
                    && p.external_write_conflict_path().is_none()
        )
    {
        model.pop_focused_overlay();
        model.dirty = true;
    }
    // <Esc> closes a picker sitting directly on top of the stack.
    // Checked here, ahead of the focus match below, so a picker
    // buried under a still-open prompt (the stacking rule a modal
    // prompt keeps its focus, see `OverlayKind::Picker`'s doc) never
    // sees this: `focused_overlay_mut` names the prompt in that case,
    // not the picker, and the pattern below simply does not match.
    if notation == "<Esc>"
        && matches!(
            model.focused_overlay_mut().map(|ov| &ov.kind),
            Some(OverlayKind::Picker(_))
        )
    {
        model.pop_focused_overlay();
        // without this the closed picker stays on screen until some
        // unrelated event repaints: the paint loop's `if model.dirty`
        // gate is the only repaint trigger, and popping an overlay
        // produces no engine redraw to trip it
        model.dirty = true;
        // tells the matcher worker to drop its live Session so a
        // Files scan still walking a huge tree does not keep
        // running unobserved -- see Effect::PickerClose's doc; the
        // session-replacement path in the worker only fires on a
        // later query for a different source, which closing here
        // may never produce
        return vec![Effect::PickerClose];
    }
    match model.focus() {
        Focus::Engine => vec![Effect::Rpc(RpcCall::Input { notation })],
        Focus::Native(_) => match model.focused_overlay_mut().map(|ov| &mut ov.kind) {
            // an nvim-relayed prompt answers by feeding the engine a
            // keystroke -- the engine is blocked in its own input
            // loop, not on an RpcRequest, so this is the one Native
            // arm that still reaches RpcCall::Input. The AI trust
            // prompt is the one exception (see PromptState's Origin
            // doc): it resolves locally, so this returns
            // Effect::AiTrustSet instead of forwarding the key.
            Some(OverlayKind::Prompt(p)) => {
                if !p.accepts(&notation) {
                    return Vec::new();
                }
                if let Some(project_root) = p.ai_trust_project_root() {
                    let project_root = project_root.to_path_buf();
                    let verb = p.ai_trust_verb().unwrap_or_default().to_string();
                    let trusted = p.accepted_is_default(&notation);
                    model.pop_focused_overlay();
                    model.dirty = true;
                    return vec![Effect::AiTrustSet {
                        project_root,
                        trusted,
                        verb,
                    }];
                }
                // the external-write conflict prompt resolves locally too,
                // on the same terms the AI trust prompt does: "Reload" (the
                // bracketed default) re-drives `RpcCall::Checktime` with
                // `force: true` (see that field's own doc for why a bare
                // second checktime cannot re-decide what the first already
                // did), and "Keep local" issues nothing at all -- the
                // buffer's local edits are exactly what checktime's own
                // conflict branch already guaranteed it left untouched
                if let Some(path) = p.external_write_conflict_path() {
                    let path = path.to_path_buf();
                    let reload = p.accepted_is_default(&notation);
                    model.pop_focused_overlay();
                    model.dirty = true;
                    if !reload {
                        return Vec::new();
                    }
                    let request_id = model.next_checktime_request_id();
                    return vec![Effect::Rpc(RpcCall::Checktime {
                        request_id,
                        paths: vec![path_to_wire(&path)],
                        force: true,
                    })];
                }
                vec![Effect::Rpc(RpcCall::Input { notation })]
            }
            // every other key edits the query and re-asks the
            // matcher worker; edit_query itself decides what a
            // notation means (a plain char, <BS>, or a no-op it
            // still bumps the generation for), so this arm never
            // inspects notation itself.
            Some(OverlayKind::Picker(p)) => {
                let generation = p.edit_query(&notation);
                let needle = p.query().to_string();
                let source = p.source().clone();
                vec![Effect::PickerQuery {
                    generation,
                    needle,
                    source,
                    resolved: None,
                }]
            }
            // discards the payload deliberately: every branch below
            // reaches the tree through `model.tree_mut()` fresh
            // instead, since a bound `&mut TreeState` here would
            // keep `model` borrowed across the `model.pop_focused_overlay()`
            // and `model.close_tree()` calls the <CR>/<Esc> arms need
            Some(OverlayKind::Tree(_)) => match notation.as_str() {
                "<Esc>" => {
                    model.pop_focused_overlay();
                    model.dirty = true;
                    vec![Effect::TreeClose]
                }
                "<Down>" => {
                    if let Some(t) = model.tree_mut() {
                        t.move_selection(1);
                    }
                    model.dirty = true;
                    Vec::new()
                }
                "<Up>" => {
                    if let Some(t) = model.tree_mut() {
                        t.move_selection(-1);
                    }
                    model.dirty = true;
                    Vec::new()
                }
                // a directory toggles in place; a leaf's path is
                // opened through RPC (nvim owns the buffer this
                // creates) and the sidebar closes on the same
                // keypress, matching a picker selection's own
                // close-on-open behavior
                "<CR>" => {
                    let to_open = model.tree_mut().and_then(|t| {
                        let entry = t.selected_entry()?;
                        if entry.is_dir {
                            if let Some(idx) = t.view().selected {
                                t.toggle_expand(idx);
                            }
                            None
                        } else {
                            t.selected_path()
                        }
                    });
                    model.dirty = true;
                    match to_open {
                        Some(path) => {
                            model.pop_focused_overlay();
                            vec![Effect::Rpc(RpcCall::OpenFile {
                                path: path_to_wire(&path),
                            })]
                        }
                        None => Vec::new(),
                    }
                }
                // opens the blocked-engine Prompt overlay through
                // the entry's own RpcCall (`vim.fn.input` primed
                // with a `kind = "confirm"` `nvim_echo`, see
                // `RpcCall::TreeCreatePrompt`'s doc) rather than any
                // new local input state: the reply routes back as
                // `Msg::TreeCreatePromptReply` and resolves the
                // actual file write from there, once nvim has
                // answered. Any selection, including none at all
                // (an empty tree), can create -- `TreeCreatePromptReply`
                // resolves the target directory from whatever is
                // selected at reply time (see its arm below), since
                // nothing about the tree's selection can move while
                // this prompt holds focus.
                "a" => {
                    let Some(t) = model.tree_mut() else {
                        return Vec::new();
                    };
                    let generation = t.generation();
                    vec![Effect::Rpc(RpcCall::TreeCreatePrompt { generation })]
                }
                // renaming a directory has no backing effect --
                // `RpcCall::RenameFile` and the `Effect::Tree*File`
                // pair are file-only by their own doc contracts --
                // so a directory selection is a silent no-op here
                // rather than opening a prompt whose answer nothing
                // could act on.
                "r" => {
                    let Some(t) = model.tree_mut() else {
                        return Vec::new();
                    };
                    let Some(entry) = t.selected_entry() else {
                        return Vec::new();
                    };
                    if entry.is_dir {
                        return Vec::new();
                    }
                    let current_name = entry
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let Some(old_path) = t.selected_path() else {
                        return Vec::new();
                    };
                    let generation = t.generation();
                    vec![Effect::Rpc(RpcCall::TreeRenamePrompt {
                        generation,
                        old_path: path_to_wire(&old_path),
                        current_name,
                    })]
                }
                // same file-only restriction as "r", for the same
                // reason.
                "d" => {
                    let Some(t) = model.tree_mut() else {
                        return Vec::new();
                    };
                    let Some(entry) = t.selected_entry() else {
                        return Vec::new();
                    };
                    if entry.is_dir {
                        return Vec::new();
                    }
                    let Some(path) = t.selected_path() else {
                        return Vec::new();
                    };
                    let generation = t.generation();
                    vec![Effect::Rpc(RpcCall::TreeDeleteConfirm {
                        generation,
                        path: path_to_wire(&path),
                    })]
                }
                _ => Vec::new(),
            },
            // A pending permission request blocks the issuing agent's own
            // turn until answered; y/n/a/<Esc> reach it here because
            // `model.focus()` only ever names this overlay once the user
            // has deliberately entered it (`AiPanelState::focused`,
            // consulted by `Model::takes_focus_now`) -- never merely by
            // being open, and never by side effect of whatever mode the
            // engine happens to be in. With the panel not entered,
            // `model.focus()` is `Focus::Engine` instead, so every key --
            // including `y`/`n` as ordinary engine commands -- reaches
            // nvim through this same `match`'s `Focus::Engine` arm
            // untouched.
            Some(OverlayKind::Ai) => {
                if let Some(prompt) = model.ai_panel().pending_permission.clone() {
                    // <Esc> settles the request as `Cancelled` rather than
                    // any offered option -- the one answer that exists
                    // even when the agent offered only allow-kind options,
                    // so declining always has a key that means it
                    // regardless of what was offered.
                    if notation == "<Esc>" {
                        model.ai_panel_mut().pending_permission = None;
                        model.dirty = true;
                        return vec![Effect::Ai(AiCommand::AnswerPermission {
                            request_id: prompt.request_id,
                            outcome: PermissionOutcome::Cancelled,
                        })];
                    }
                    // An unmapped key is swallowed rather than forwarded,
                    // the same way an unmatched key on a confirm-class
                    // `PromptState` leaves the prompt open instead of
                    // falling through.
                    let mut chars = notation.chars();
                    let key = chars.next().filter(|_| chars.next().is_none());
                    let Some(option) = key.and_then(|c| prompt.option_for_key(c)).cloned() else {
                        return Vec::new();
                    };
                    model.ai_panel_mut().pending_permission = None;
                    model.dirty = true;
                    return vec![Effect::Ai(AiCommand::AnswerPermission {
                        request_id: prompt.request_id,
                        outcome: PermissionOutcome::Selected {
                            option_id: option.option_id,
                        },
                    })];
                }
                // A review owns the panel's printable keys while it is
                // open. Deliberately total for printables rather than a
                // few keys layered over the composer: `a` cannot mean
                // both "accept this hunk" and "type an a", and a review
                // is a decision the user opened the panel to make. Named
                // notations (`<Esc>` to un-enter, `<C-c>`, `<C-d>`) fall
                // through to the arm below, so every way out of the panel
                // still works from inside a review.
                if model.ai_panel().pending_diff.is_some() && !notation.starts_with('<') {
                    return review_key(model, &notation);
                }
                // Nothing pending: the panel's own composer keys. Every key
                // not named below is swallowed rather than leaked to nvim --
                // the whole point of having deliberately entered the panel
                // is that the engine does not see these keystrokes.
                if notation == "<Esc>" {
                    // Relinquishes the keyboard (clears `focused`) without
                    // closing the panel itself: it stays visible beside the
                    // buffer, the same non-modal presence it had before
                    // being entered, and the `close` verb remains the only
                    // thing that removes it from the stack.
                    model.ai_panel_mut().focused = false;
                    model.dirty = true;
                } else if notation == "<C-c>" {
                    // `<Esc>` is already taken by prompt-cancel/un-enter
                    // above, so cancelling an in-flight turn gets the
                    // vocabulary's other interrupt notation (see
                    // `supervision::INTERRUPT_NOTATION`, the same key for
                    // the same reason elsewhere in this codebase). Gated on
                    // `turn_in_flight` so cancelling with nothing running
                    // has no session to interrupt, rather than reaching
                    // `AiWorker::dispatch`'s own "no active session" surface
                    // for a key the panel itself could have refused instead.
                    if model.ai_panel().turn_in_flight {
                        return vec![Effect::Ai(AiCommand::Cancel)];
                    }
                } else if notation == "<C-d>" {
                    // The crash banner's reader is by construction inside an
                    // entered panel, where the composer consumes every
                    // printable -- so dismissal needs a named notation the
                    // composer excludes, beside `<C-c>` above. The `:View ai
                    // dismiss` verb remains the from-outside route to the
                    // same slot.
                    if model.ai_panel_mut().local_error.take().is_some() {
                        model.dirty = true;
                    }
                } else if notation == "<CR>" {
                    let panel = model.ai_panel_mut();
                    if !panel.input.trim().is_empty() {
                        let text = std::mem::take(&mut panel.input);
                        panel.turn_in_flight = true;
                        model.dirty = true;
                        // `view-core` cannot assemble this prompt's context
                        // itself: every block `view_ai::context::assemble`
                        // could turn into a `ContextBlock` is read over RPC,
                        // and this crate does neither I/O nor `view-ai` (see
                        // `scripts/audit-deps.sh`'s dependency direction).
                        // `Effect::AiPromptSubmit` carries only the text; the
                        // bin's executor performs the four reads, assembles
                        // the context, and only then hands the agent session
                        // the completed `AiCommand::Prompt` -- see that
                        // effect's own doc.
                        return vec![Effect::AiPromptSubmit { text }];
                    }
                } else if notation == "<BS>" {
                    if model.ai_panel_mut().input.pop().is_some() {
                        model.dirty = true;
                    }
                } else if notation == "<lt>" {
                    // nvim's own escape for a literal `<`, the one
                    // printable character that cannot arrive as itself
                    // (see `keys::encode_key`'s own doc).
                    model.ai_panel_mut().input.push('<');
                    model.dirty = true;
                } else if let Some(ch) = {
                    // Every other single character, including a literal
                    // space, arrives as itself rather than a named `<...>`
                    // notation -- the same convention the permission
                    // options above already key off of.
                    let mut chars = notation.chars();
                    chars.next().filter(|_| chars.next().is_none())
                } {
                    model.ai_panel_mut().input.push(ch);
                    model.dirty = true;
                }
                // Any other named key (`<Up>`, `<Tab>`, ...) has no
                // composer meaning yet and is swallowed the same way it
                // always was.
                Vec::new()
            }
            // the key belongs to the overlay on top of the stack,
            // and no other overlay kind carries a key handler yet,
            // so consuming it is the whole of that routing. <Esc>
            // closes exactly that one overlay, which is why it pops
            // rather than clearing: an overlay underneath it keeps
            // the keyboard.
            _ => {
                if notation == "<Esc>" {
                    model.pop_focused_overlay();
                    model.dirty = true;
                }
                Vec::new()
            }
        },
    }
}

/// Pops the Prompt overlay if it is the topmost one -- the shared first
/// step every tree prompt-reply arm takes, since each of the three
/// (create/rename/delete) resolves the same way: a definitive async RPC
/// reply, not the lazy next-keypress dismissal `Msg::Key` uses for the
/// engine's own confirm() dialogs, which carry no reply of their own to
/// hook into.
fn dismiss_top_prompt(model: &mut Model) {
    if matches!(
        model.focused_overlay_mut().map(|ov| &ov.kind),
        Some(OverlayKind::Prompt(_))
    ) {
        model.pop_focused_overlay();
    }
}

/// The notice text for a `Msg::FeatureInvoke` this build has nothing behind:
/// `known` distinguishes a registered entry point with no handler yet
/// (echoes back exactly what was invoked) from one the registry has never
/// heard of (offers the forms that do work instead of naming a typo).
///
/// Only the first carries a `view: ` prefix. The usage line opens with the
/// ex-command's own name, so prefixing it stutters the product's name into
/// `view: :View needs ...`, and a line that starts `:View` already says
/// which tool is speaking.
fn feature_invoke_notice(feature: &str, verb: &str, known: bool) -> String {
    if known {
        format!("view: no handler for {feature} {verb} in this build")
    } else {
        crate::native::mappings::render_usage()
    }
}

/// Issues an `Effect::Rpc(RpcCall::PreviewBuffer)` for `state`'s current
/// selection, or no effect at all when there is nothing to preview (an
/// empty result set, or an unnamed `Buffers` scratch entry -- see
/// `PickerState::selected_path`'s doc). Shared by every arm that can move
/// the selection: today that is only `Msg::PickerResults` (no arrow-key/
/// Enter navigation exists yet), but the seam is named rather than inlined
/// so a future navigation arm reuses it instead of re-deriving the request.
fn picker_preview_request(state: &mut crate::native::picker::PickerState) -> Vec<Effect> {
    match state.refresh_preview() {
        Some((generation, path)) => vec![Effect::Rpc(RpcCall::PreviewBuffer { path, generation })],
        None => Vec::new(),
    }
}

/// Answers a `Msg::FeatureInvoke` naming `ai` while `model.ai_enabled` is
/// false: a native notice naming the exact config line that turns it back
/// on, and nothing else -- no prompt, no panel, no trust question, since a
/// disabled feature has nothing behind any of those to open.
fn notice_ai_disabled(model: &mut Model) -> Vec<Effect> {
    model.dirty = true;
    model.engine.record_native_notice(
        "view: the AI agent panel is off -- turn it on with ai.enabled = true in view.toml"
            .to_string(),
        false,
    )
}

/// The picker source `verb` names, or `None` when `verb` is not one of the
/// picker's own three entry points. `cwd` seeds `Source::Files`'s root: a
/// relative walk root would need `view-core` to ask the filesystem what
/// "here" means, which it cannot do (see [`Model::cwd`]'s doc), so the
/// caller resolves it once, at startup, and this just reads the result back.
fn picker_source_for_verb(
    verb: &str,
    cwd: &std::path::Path,
) -> Option<crate::native::picker::Source> {
    use crate::native::picker::Source;
    match verb {
        "files" => Some(Source::Files {
            root: cwd.to_path_buf(),
        }),
        "buffers" => Some(Source::Buffers),
        "grep" => Some(Source::LiveGrep {
            root: cwd.to_path_buf(),
        }),
        _ => None,
    }
}

/// Opens a new picker overlay over `source` and issues whatever first query
/// its corpus needs: an empty-needle `Effect::PickerQuery` for a source the
/// matcher worker walks itself, or `Effect::Rpc(RpcCall::ListBuffers)` for
/// `Source::Buffers`, whose corpus lives in the engine rather than on disk.
fn open_picker(model: &mut Model, source: crate::native::picker::Source) -> Vec<Effect> {
    let needs_buffer_list = matches!(source, crate::native::picker::Source::Buffers);
    let state = crate::native::picker::PickerState::open(source.clone());
    let generation = state.generation();
    // a blocked-engine Prompt must keep focus: a FeatureInvoke racing its
    // opening must not steal it out from under the answer nvim is still
    // waiting on, so this takes the stacking rule OverlayKind::Picker's doc
    // states for the reverse order (a Prompt arriving over an open picker)
    // and applies it here too, inserting beneath instead of on top
    let prompt_is_topmost = matches!(
        model.overlays().last().map(|overlay| &overlay.kind),
        Some(OverlayKind::Prompt(_))
    );
    if prompt_is_topmost {
        model.insert_overlay_beneath_top(OverlayBox::new(70, 60), OverlayKind::Picker(state));
    } else {
        model.push_overlay(OverlayBox::new(70, 60), OverlayKind::Picker(state));
    }
    model.dirty = true;
    if needs_buffer_list {
        vec![Effect::Rpc(RpcCall::ListBuffers { generation })]
    } else {
        vec![Effect::PickerQuery {
            generation,
            needle: String::new(),
            source,
            resolved: None,
        }]
    }
}

/// Opens the file tree sidebar over `model.cwd`, or closes it if one is
/// already open -- the toggle semantic `<leader>e` carries over from
/// neo-tree's own binding. Reachable only while the engine holds focus
/// (`Msg::FeatureInvoke` is nvim's own `rpcnotify`, which a native overlay's
/// focus would intercept before it ever reaches nvim's mapping, see
/// `Msg::Key`'s `Focus::Native` arm), so an already-open tree can only be
/// found here in the corner case of a stray re-invocation; the ordinary
/// close path is `<Esc>` from inside the tree's own key arm.
/// Reissues `Effect::TreeGitScan` for the open tree, if one is, on a
/// bridge write/focus callback -- see `TreeState`'s own doc on why a git
/// refresh is timed off these callbacks rather than the scan, and why the
/// two carry independent generations. A no-op (empty effect list) when no
/// tree is open, which is the common case: these callbacks fire on every
/// buffer write and focus change regardless of the sidebar's state -- and
/// also when a refresh is already in flight, since `TreeState` coalesces
/// this request into it rather than spawning a second concurrent scan (see
/// `TreeState::request_git_refresh`).
fn tree_git_refresh_effect(model: &mut Model) -> Vec<Effect> {
    let Some(tree) = model.tree_mut() else {
        return Vec::new();
    };
    let root = tree.root().to_path_buf();
    match tree.request_git_refresh() {
        Some(generation) => vec![Effect::TreeGitScan { generation, root }],
        None => Vec::new(),
    }
}

fn toggle_tree_sidebar(model: &mut Model) -> Vec<Effect> {
    if model.close_tree() {
        model.dirty = true;
        return vec![Effect::TreeClose];
    }
    let mut state = crate::native::tree::TreeState::open(model.cwd.clone());
    let scan_generation = state.generation();
    // a freshly opened `TreeState` has never had a refresh in flight, so
    // this always allocates rather than coalescing -- the `Option` is
    // still handled rather than assumed, so a future change to `open`'s
    // initial state cannot silently turn this into a missing git scan
    let git_generation = state.request_git_refresh();
    let prompt_is_topmost = matches!(
        model.overlays().last().map(|overlay| &overlay.kind),
        Some(OverlayKind::Prompt(_))
    );
    let geometry = OverlayBox::new(30, 100).with_anchor(Anchor::Left);
    if prompt_is_topmost {
        model.insert_overlay_beneath_top(geometry, OverlayKind::Tree(state));
    } else {
        model.push_overlay(geometry, OverlayKind::Tree(state));
    }
    model.dirty = true;
    let mut effects = vec![Effect::TreeScan {
        generation: scan_generation,
        root: model.cwd.clone(),
    }];
    if let Some(generation) = git_generation {
        effects.push(Effect::TreeGitScan {
            generation,
            root: model.cwd.clone(),
        });
    }
    effects
}

/// Opens the agent panel, anchored flush right like the tree sidebar is
/// flush left (see [`OverlayKind::Ai`]'s doc). A no-op when the panel is
/// already open: unlike `toggle`, `open` never closes what it finds.
///
/// Inserted beneath the topmost overlay when that overlay takes focus or is
/// the busy annunciator, rather than pushed on top of it: `Ai` has no key
/// path of its own (see [`Model::takes_focus`]'s doc on why), so it must
/// never sit over something that can still act on a keystroke, and must
/// never bury the one warning that has to stay visible while the engine is
/// unresponsive. Every other topmost overlay is exactly as blind to input
/// as `Ai` itself, so stacking on top of it costs nothing.
///
/// Opening never itself starts (or stops) an agent session: the session's
/// own lifecycle is independent of the overlay's, driven instead by the
/// first non-empty `<CR>` a user submits through the panel once entered
/// (see the `Some(OverlayKind::Ai)` arm of `route_key`) and, once bound,
/// kept alive by `crate::ai_worker::AiWorker` regardless of whether this
/// overlay is open, closed, or has never been opened at all -- see
/// [`Model::ai_panel_overlay_open`]'s own doc for that split. This function
/// only pushes or hides the sidebar overlay that renders the session state
/// already sitting in [`Model::ai_panel`], see [`OverlayKind::Ai`]'s doc.
fn open_ai_panel(model: &mut Model) -> Vec<Effect> {
    if model.ai_panel_overlay_open() {
        return Vec::new();
    }
    let insert_beneath = model.overlays().last().is_some_and(|overlay| {
        Model::takes_focus(&overlay.kind) || matches!(overlay.kind, OverlayKind::EngineBusy(_))
    });
    let geometry = OverlayBox::new(30, 100).with_anchor(Anchor::Right);
    if insert_beneath {
        model.insert_overlay_beneath_top(geometry, OverlayKind::Ai);
    } else {
        model.push_overlay(geometry, OverlayKind::Ai);
    }
    model.dirty = true;
    Vec::new()
}

/// Opens the agent panel if it is closed, closes it if it is open. Closing
/// never tears down a live session, for the same reason opening never
/// starts one (see [`open_ai_panel`]'s doc): a session already running
/// keeps running, unattended, exactly as `close_ai_panel`'s own doc
/// promises. Either direction is an explicit user invoke, so it claims or
/// releases the panel's keyboard focus the same way the `open`/`close`
/// verbs do -- see `AiPanelState::focused`'s own doc.
fn toggle_ai_panel(model: &mut Model) -> Vec<Effect> {
    // `close_ai_panel` itself clears `AiPanelState::focused`, at the single
    // authoritative closing point
    if model.close_ai_panel() {
        model.dirty = true;
        return Vec::new();
    }
    let effects = open_ai_panel(model);
    model.ai_panel_mut().focused = true;
    effects
}

/// Opens the message-history overlay over a snapshot of `ToastHistory`,
/// centered like a picker. `<leader>fm`/`:View notifications history` is
/// its only entry point (see `mappings::DEFAULT_MAPS`); there is nothing to
/// toggle the way the tree sidebar has, since re-invoking it while one is
/// already open would only ever want a fresher snapshot, not a close --
/// the same "closing is `<Esc>`'s job, not the invoking key's" split every
/// other centered overlay here already follows.
fn open_message_history(model: &mut Model) -> Vec<Effect> {
    let state = crate::native::palette::MessageHistoryState::snapshot(&model.engine.toast_history);
    let prompt_is_topmost = matches!(
        model.overlays().last().map(|overlay| &overlay.kind),
        Some(OverlayKind::Prompt(_))
    );
    if prompt_is_topmost {
        model.insert_overlay_beneath_top(
            OverlayBox::new(70, 60),
            OverlayKind::MessageHistory(state),
        );
    } else {
        model.push_overlay(OverlayBox::new(70, 60), OverlayKind::MessageHistory(state));
    }
    model.dirty = true;
    Vec::new()
}

/// Routes one mouse event to the surface that owns it: the overlay under
/// the pointer, or the engine when no overlay covers that cell.
///
/// A press claims the gesture, and the `drag`s and the `release` that
/// follow go wherever the press went, however far the pointer travels.
/// Routing every event by its own position instead would truncate any drag
/// crossing an overlay edge: the engine would see a press with no release
/// and stay stuck mid-selection, or a release for a press it never saw.
/// `wheel` and `move` carry no gesture, so they always route by position
/// and leave an in-flight capture alone.
fn route_mouse(model: &mut Model, input: MouseInput) -> Vec<Effect> {
    let owner = match input.action.as_str() {
        "press" => {
            let owner = position_owner(model, &input);
            model.capture_mouse(owner);
            owner
        }
        "drag" | "release" => {
            // a gesture whose press was never seen (input started mid-drag)
            // has no owner to honor, so it falls back to position
            let owner = model
                .mouse_capture()
                .unwrap_or_else(|| position_owner(model, &input));
            if input.action == "release" {
                model.release_mouse();
            }
            owner
        }
        _ => position_owner(model, &input),
    };
    match owner {
        // no overlay carries a mouse handler, so an overlay claiming the
        // event is the whole of that routing
        MouseCapture::Overlay(_) => Vec::new(),
        MouseCapture::Engine => mouse_effect(model, input),
    }
}

/// Which surface the pointer is over: the topmost overlay covering the
/// cell, else the engine grid.
fn position_owner(model: &Model, input: &MouseInput) -> MouseCapture {
    match model.overlay_at(input.row, input.col) {
        Some(id) => MouseCapture::Overlay(id),
        None => MouseCapture::Engine,
    }
}

/// Maps one terminal mouse event to an `RpcCall::InputMouse` effect,
/// translating `input.row` from raw terminal cell coordinates into engine
/// grid coordinates by subtracting [`Model::chrome_rows`]. A row that lands
/// inside the reserved chrome (the tabline) belongs to that chrome, not the
/// grid; no native chrome click handling exists yet, so such a click is
/// dropped rather than forwarded at a wrapped-around row.
fn mouse_effect(model: &Model, input: MouseInput) -> Vec<Effect> {
    let chrome = model.chrome_rows();
    if input.row < chrome {
        return Vec::new();
    }
    vec![Effect::Rpc(RpcCall::InputMouse {
        button: input.button,
        action: input.action,
        modifier: input.modifier,
        row: input.row - chrome,
        col: input.col,
    })]
}

#[cfg(test)]
mod tests;
