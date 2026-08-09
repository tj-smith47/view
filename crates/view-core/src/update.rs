//! The pure state transition: `Msg` in, `Model` mutated, `Effect`s out.

use crate::events::{clamp_dim, saturate_u16, UiEvent};
use crate::grid::GridOp;
use crate::hl::HlAttr;
use crate::model::{
    CmdlineState, Focus, Model, MouseCapture, OverlayKind, PopupmenuState, TablineState,
};
use crate::msg::{
    DeleteConfirmOutcome, Effect, EngineRequest, Key, MouseInput, Msg, ReplyValue, RpcCall,
};
use crate::native::geometry::{Anchor, OverlayBox};
use crate::native::prompt::PromptState;
use crate::native::statusline::SegmentUpdate;

/// Applies one message to `model`, returning the effects the executor must
/// carry out. Never blocks and never performs I/O: every side effect crosses
/// the boundary as a returned [`Effect`] instead of being performed here.
#[must_use]
pub fn update(model: &mut Model, msg: Msg) -> Vec<Effect> {
    match msg {
        Msg::Key(Key { notation }) => {
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
            if !cmdline_open
                && matches!(
                    model.top_overlay_mut().map(|ov| &ov.kind),
                    Some(OverlayKind::Prompt(_))
                )
            {
                model.pop_overlay();
                model.dirty = true;
            }
            // <Esc> closes a picker sitting directly on top of the stack.
            // Checked here, ahead of the focus match below, so a picker
            // buried under a still-open prompt (the stacking rule a modal
            // prompt keeps its focus, see `OverlayKind::Picker`'s doc) never
            // sees this: `top_overlay_mut` names the prompt in that case,
            // not the picker, and the pattern below simply does not match.
            if notation == "<Esc>"
                && matches!(
                    model.top_overlay_mut().map(|ov| &ov.kind),
                    Some(OverlayKind::Picker(_))
                )
            {
                model.pop_overlay();
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
                Focus::Native(_) => match model.top_overlay_mut().map(|ov| &mut ov.kind) {
                    // the prompt overlay answers by feeding the engine a
                    // keystroke -- the engine is blocked in its own input
                    // loop, not on an RpcRequest, so this is the one Native
                    // arm that still reaches RpcCall::Input
                    Some(OverlayKind::Prompt(p)) => {
                        if p.accepts(&notation) {
                            vec![Effect::Rpc(RpcCall::Input { notation })]
                        } else {
                            Vec::new()
                        }
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
                    // keep `model` borrowed across the `model.pop_overlay()`
                    // and `model.close_tree()` calls the <CR>/<Esc> arms need
                    Some(OverlayKind::Tree(_)) => match notation.as_str() {
                        "<Esc>" => {
                            model.pop_overlay();
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
                                    model.pop_overlay();
                                    vec![Effect::Rpc(RpcCall::OpenFile {
                                        path: path.to_string_lossy().into_owned(),
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
                                old_path: old_path.to_string_lossy().into_owned(),
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
                                path: path.to_string_lossy().into_owned(),
                            })]
                        }
                        _ => Vec::new(),
                    },
                    // the key belongs to the overlay on top of the stack,
                    // and no other overlay kind carries a key handler yet,
                    // so consuming it is the whole of that routing. <Esc>
                    // closes exactly that one overlay, which is why it pops
                    // rather than clearing: an overlay underneath it keeps
                    // the keyboard.
                    _ => {
                        if notation == "<Esc>" {
                            model.pop_overlay();
                            model.dirty = true;
                        }
                        Vec::new()
                    }
                },
            }
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
        // loop plumbing tokens: the loop resolves these into Redraw/EngineDown
        // before update() ever sees them, so both arms are no-ops here;
        // EngineReady is consumed even earlier, by startup's pre-attach
        // draining loop, before the steady-state loop this match belongs to
        // ever starts, so this arm is unreachable in practice but kept for
        // the same defensive-totality reason
        Msg::RedrawReady | Msg::EngineStopped(_) | Msg::EngineReady => Vec::new(),
        Msg::EngineDown(exit) => {
            model.running = false;
            vec![Effect::Quit {
                exit_code: exit.code.unwrap_or(1),
            }]
        }
        Msg::EngineRequest(EngineRequest::VimEnter { token }) => vec![Effect::Reply {
            token,
            value: ReplyValue::Nil,
        }],
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
        Msg::FeatureInvoke { feature, verb } => {
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
        Msg::TreeGitResult { generation, status } => {
            let Some(t) = model.tree_mut() else {
                return Vec::new();
            };
            t.apply_git(generation, status);
            model.dirty = true;
            Vec::new()
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
            let old = std::path::PathBuf::from(&old_path);
            let Some(parent) = old.parent() else {
                return Vec::new();
            };
            let new_path = parent.join(new_name);
            vec![Effect::Rpc(RpcCall::RenameFile {
                old_path,
                new_path: new_path.to_string_lossy().into_owned(),
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
        model.top_overlay_mut().map(|ov| &ov.kind),
        Some(OverlayKind::Prompt(_))
    ) {
        model.pop_overlay();
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
/// buffer write and focus change regardless of the sidebar's state.
fn tree_git_refresh_effect(model: &mut Model) -> Vec<Effect> {
    let Some(tree) = model.tree_mut() else {
        return Vec::new();
    };
    let root = tree.root().to_path_buf();
    let generation = tree.request_git_refresh();
    vec![Effect::TreeGitScan { generation, root }]
}

fn toggle_tree_sidebar(model: &mut Model) -> Vec<Effect> {
    if model.close_tree() {
        model.dirty = true;
        return vec![Effect::TreeClose];
    }
    let mut state = crate::native::tree::TreeState::open(model.cwd.clone());
    let scan_generation = state.generation();
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
    vec![
        Effect::TreeScan {
            generation: scan_generation,
            root: model.cwd.clone(),
        },
        Effect::TreeGitScan {
            generation: git_generation,
            root: model.cwd.clone(),
        },
    ]
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

/// Applies one decoded redraw sub-event to `model`, returning any effects
/// it produces. Only [`UiEvent::TablineUpdate`] can produce one: crossing
/// the 1-tab chrome-reservation boundary (either direction) changes the
/// grid target size, which the loop's executor must forward to the engine
/// as a `TryResize` the same way a terminal resize does.
fn apply_ui_event(model: &mut Model, ev: UiEvent) -> Vec<Effect> {
    match ev {
        UiEvent::GridResize { width, height, .. } => {
            // clamp untrusted wire dimensions: a desynced or malformed
            // grid_resize must not allocate unboundedly, and a plain `as
            // u16` cast would silently truncate 65536 to 0
            model.engine.apply_grid(GridOp::Resize {
                width: clamp_dim(width),
                height: clamp_dim(height),
            });
            Vec::new()
        }
        UiEvent::GridLine {
            row,
            col_start,
            cells,
            ..
        } => {
            model.engine.apply_grid(GridOp::PutLine {
                row: saturate_u16(row),
                col_start: saturate_u16(col_start),
                cells: cells
                    .into_iter()
                    .map(|c| (c.text, c.hl_id, c.repeat))
                    .collect(),
            });
            Vec::new()
        }
        UiEvent::GridCursorGoto { row, col, .. } => {
            model.engine.apply_grid(GridOp::CursorGoto {
                row: saturate_u16(row),
                col: saturate_u16(col),
            });
            Vec::new()
        }
        UiEvent::GridScroll {
            top,
            bot,
            left,
            right,
            rows,
            ..
        } => {
            model.engine.apply_grid(GridOp::Scroll {
                top: saturate_u16(top),
                bot: saturate_u16(bot),
                left: saturate_u16(left),
                right: saturate_u16(right),
                rows: i32::try_from(rows).unwrap_or(if rows > 0 { i32::MAX } else { i32::MIN }),
            });
            Vec::new()
        }
        UiEvent::GridClear { .. } => {
            model.engine.apply_grid(GridOp::Clear);
            Vec::new()
        }
        UiEvent::HlAttrDefine {
            id,
            fg,
            bg,
            bold,
            italic,
            underline,
            reverse,
        } => {
            model.engine.define_hl_attr(
                id,
                HlAttr {
                    fg,
                    bg,
                    bold,
                    italic,
                    underline,
                    reverse,
                },
            );
            Vec::new()
        }
        UiEvent::DefaultColorsSet { fg, bg, .. } => {
            // the generation comes back from the write that opened it, so
            // the emitted probe carries the exact generation these colors
            // own; a stale reply for an older generation is dropped by the
            // Msg::HlProbeReply arm above, and Theme::from_hl falls back to
            // the (possibly still wire-ambiguous) raw values until a
            // matching reply lands
            let generation = model.engine.set_hl_default_colors(fg, bg);
            vec![Effect::Rpc(RpcCall::GetDefaultHl { generation })]
        }
        UiEvent::HlGroupSet { name, hl_id } => {
            model.engine.set_hl_group(name, hl_id);
            Vec::new()
        }
        UiEvent::Flush => {
            model.dirty = true;
            // idempotent past the first Flush: see Model::content_painted's
            // doc comment for why this never resets
            model.content_painted = true;
            // records that one full paint cycle has happened, so a
            // transient toast pushed in this same batch is guaranteed at
            // least one visible frame before dismiss_transient_on_keypress
            // can drop it (see Messages::note_flush)
            model.engine.messages.note_flush();
            Vec::new()
        }
        UiEvent::ModeInfoSet {
            cursor_style_enabled,
            modes,
        } => {
            model.engine.mode.cursor_style_enabled = cursor_style_enabled;
            model.engine.mode.modes = modes;
            Vec::new()
        }
        UiEvent::ModeChange { mode, mode_idx } => {
            model.engine.mode.current = mode;
            model.engine.mode.current_idx = mode_idx;
            Vec::new()
        }
        UiEvent::CmdlineShow {
            content,
            pos,
            firstc,
            prompt,
            indent,
            level,
        } => {
            let cmdline = CmdlineState {
                content,
                pos,
                firstc,
                prompt,
                indent,
                level,
            };
            // covers both the prompt's first arrival and every re-arm after
            // an unmatched key: the two are wire-identical, so re-learning
            // unconditionally on every CmdlineShow is simpler than trying
            // to tell them apart
            if let Some(OverlayKind::Prompt(p)) = model.top_overlay_mut().map(|ov| &mut ov.kind) {
                p.learn_cmdline(&cmdline);
            }
            model.engine.cmdline = Some(cmdline);
            Vec::new()
        }
        UiEvent::CmdlinePos { pos, level } => {
            if let Some(cmdline) = &mut model.engine.cmdline {
                cmdline.pos = pos;
                cmdline.level = level;
            }
            Vec::new()
        }
        UiEvent::CmdlineHide => {
            model.engine.cmdline = None;
            Vec::new()
        }
        UiEvent::MsgShow {
            kind,
            content,
            replace_last,
        } => {
            // classification, push, history, and expiry-scheduling all
            // happen inside record_message -- the wire event owns no copy
            // of that sequence, only the three fields the choke point needs
            let effects = model.engine.record_message(kind, content, replace_last);
            // a confirm-class msg_show while a Prompt overlay is already
            // open replaces its state wholesale rather than being skipped:
            // a genuine re-arm of the SAME question (an unmatched key)
            // never sends a second msg_show at all (see
            // docs/prompt-overlay-wire-capture.md, section 1), so every
            // msg_show this branch ever sees while a Prompt overlay is open
            // names a distinct new question -- routine when nvim's own Lua
            // resolves one confirm and immediately raises another with no
            // intervening keystroke, as plugin bootstraps do. Replacing
            // both the message and the (still-Pending) answer together
            // keeps them from a prior question's leftovers, so the paired
            // CmdlineShow that follows learns choices for the same
            // question this state's message now names.
            let prompt_state = model
                .engine
                .messages
                .entries
                .last()
                .and_then(PromptState::from_entry);
            if let Some(state) = prompt_state {
                match model.top_overlay_mut().map(|ov| &mut ov.kind) {
                    Some(OverlayKind::Prompt(p)) => *p = state,
                    _ => {
                        model.push_overlay(OverlayBox::new(60, 40), OverlayKind::Prompt(state));
                    }
                }
            }
            effects
        }
        UiEvent::MsgClear => {
            model.engine.messages.clear();
            Vec::new()
        }
        UiEvent::MsgShowmode { content } => {
            let text: String = content.iter().map(|(_, t)| t.as_str()).collect();
            model.engine.statusline.apply(SegmentUpdate::Mode(text));
            model.dirty = true;
            Vec::new()
        }
        UiEvent::MsgShowcmd { content } => {
            let text: String = content.iter().map(|(_, t)| t.as_str()).collect();
            model.engine.statusline.apply(SegmentUpdate::Showcmd(text));
            model.dirty = true;
            Vec::new()
        }
        UiEvent::MsgRuler { content } => {
            let text: String = content.iter().map(|(_, t)| t.as_str()).collect();
            model.engine.statusline.apply(SegmentUpdate::Ruler(text));
            model.dirty = true;
            Vec::new()
        }
        UiEvent::TablineUpdate { current, tabs } => {
            let before = model.chrome_rows();
            model.engine.tabline = Some(TablineState { current, tabs });
            let after = model.chrome_rows();
            if before == after {
                Vec::new()
            } else {
                let (grid_width, grid_height) = model.grid_target();
                vec![Effect::Rpc(RpcCall::TryResize {
                    width: grid_width,
                    height: grid_height,
                })]
            }
        }
        UiEvent::PopupmenuShow {
            items,
            selected,
            row,
            col,
            grid,
        } => {
            model.engine.popupmenu = Some(PopupmenuState {
                items,
                selected,
                row,
                col,
                grid,
            });
            Vec::new()
        }
        UiEvent::PopupmenuSelect { selected } => {
            if let Some(pm) = &mut model.engine.popupmenu {
                pm.selected = selected;
            }
            Vec::new()
        }
        UiEvent::PopupmenuHide => {
            model.engine.popupmenu = None;
            Vec::new()
        }
        UiEvent::MouseOn => {
            model.engine.mouse_on = true;
            Vec::new()
        }
        UiEvent::MouseOff => {
            model.engine.mouse_on = false;
            Vec::new()
        }
        UiEvent::Unknown { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::events::UiEvent;
    use crate::model::{OverlayId, OverlayKind};
    use crate::msg::{ExitInfo, RegisterType, ReplyToken};
    use crate::native::geometry::OverlayBox;

    fn model() -> Model {
        Model::new()
    }

    /// A model on an 80x24 terminal, so an overlay's share of it resolves to
    /// a rect with cells both inside and outside to click on.
    fn full_screen_model() -> Model {
        Model::with_term_size(80, 24)
    }

    /// A real `OverlayKind::Prompt`, built through the same `MsgShow` path
    /// production uses. Stands in for "some open overlay" in tests that
    /// only exercise the overlay stack's own behavior -- routing,
    /// geometry, focus, stacking -- and do not care about prompt
    /// semantics: `Prompt` is the only concrete overlay kind there is.
    fn some_overlay_kind() -> OverlayKind {
        let mut throwaway = Model::new();
        let _ = update(
            &mut throwaway,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "confirm".into(),
                content: vec![(0, "test".into())],
                replace_last: false,
            }]),
        );
        throwaway
            .pop_overlay()
            .expect("MsgShow opens a Prompt overlay")
            .kind
    }

    /// Opens a real, live confirm-style Prompt overlay: `learn_cmdline`
    /// puts it in `Answer::Choices` and `model.engine.cmdline` is set the
    /// same way a real `cmdline_show` would, together the exact end state
    /// nvim leaves a `confirm()` dialog in once both halves of its paired
    /// `msg_show`/`cmdline_show` batch have arrived (see
    /// docs/prompt-overlay-wire-capture.md section 1). `MsgShow` alone
    /// leaves `cmdline` at `None`, a state a live dialog is never actually
    /// in: the lazy-dismiss check at the top of `Msg::Key` treats a Prompt
    /// overlay open with the cmdline closed as already resolved and
    /// awaiting its dismissal keystroke, which is correct for that case
    /// and wrong to build by construction here. This stays a raw stack
    /// push rather than replaying both events through `update`, so that
    /// stacking several of these still creates distinct overlays: a
    /// second `MsgShow` while one is already open replaces it in place
    /// instead of pushing (see the `MsgShow` handler above). Covers rows
    /// 6..18 and columns 20..60 of an 80x24 terminal.
    fn open_overlay(model: &mut Model) -> OverlayId {
        let cmdline = CmdlineState {
            content: vec![],
            pos: 0,
            firstc: String::new(),
            prompt: "[Y]es, (N)o: ".into(),
            indent: 0,
            level: 1,
        };
        let mut kind = some_overlay_kind();
        let OverlayKind::Prompt(p) = &mut kind else {
            unreachable!("some_overlay_kind always returns OverlayKind::Prompt")
        };
        p.learn_cmdline(&cmdline);
        model.engine.cmdline = Some(cmdline);
        model.push_overlay(OverlayBox::new(50, 50), kind)
    }

    /// The ids on the stack, bottom first.
    fn stack_ids(model: &Model) -> Vec<u64> {
        model.overlays().iter().map(|o| o.id.0).collect()
    }

    fn mouse(action: &str, row: u16, col: u16) -> Msg {
        Msg::Mouse(MouseInput {
            button: "left".into(),
            action: action.into(),
            modifier: String::new(),
            row,
            col,
        })
    }

    fn click(row: u16, col: u16) -> Msg {
        mouse("press", row, col)
    }

    #[test]
    fn redraw_batch_applies_to_grid_and_sets_dirty_only_on_flush() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::GridResize {
                    grid: 1,
                    width: 10,
                    height: 3,
                },
                UiEvent::GridLine {
                    grid: 1,
                    row: 0,
                    col_start: 0,
                    cells: vec![crate::events::GridCell {
                        text: "h".into(),
                        hl_id: 0,
                        repeat: 1,
                    }],
                },
            ]),
        );
        assert!(effects.is_empty());
        assert!(!m.dirty, "no Flush yet: must not request paint");
        let effects = update(&mut m, Msg::Redraw(vec![UiEvent::Flush]));
        assert!(effects.is_empty());
        assert!(m.dirty);
        assert_eq!(m.engine.grid().row_text(0).trim_end(), "h");
    }

    #[test]
    fn hl_group_set_records_the_name_to_hl_id_mapping() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::HlGroupSet {
                name: "StatusLine".to_string(),
                hl_id: 41,
            }]),
        );
        assert!(effects.is_empty());
        assert_eq!(m.engine.hl().group("StatusLine"), Some(41));
    }

    #[test]
    fn hl_group_set_overwrites_a_prior_mapping_for_the_same_name() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::HlGroupSet {
                name: "StatusLine".to_string(),
                hl_id: 1,
            }]),
        );
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::HlGroupSet {
                name: "StatusLine".to_string(),
                hl_id: 168,
            }]),
        );
        assert_eq!(m.engine.hl().group("StatusLine"), Some(168));
    }

    /// The `hl_attr_define` counterpart to the mapping test above: a
    /// redefinition that changes exactly one attribute must land, for every
    /// attribute a resolved style reads.
    ///
    /// `HlTable::define_attr` drops a redefinition it judges identical to
    /// what it already holds, so any field missing from `HlAttr`'s equality
    /// turns that field's redefinition into a silent no-op: the stored
    /// attributes keep the old value, every cell holding the id keeps
    /// painting the old style, and nothing anywhere reports it. A state
    /// assertion rather than a clipped-versus-full paint comparison,
    /// because those composite both sides from one model and so read the
    /// same dropped mutation twice, agreeing with each other about the
    /// wrong screen.
    #[test]
    fn an_hl_attr_redefinition_lands_for_every_field_a_resolved_style_reads() {
        type AttrProbe = fn(&HlAttr) -> bool;
        type StyleProbe = fn(&crate::theme::ResolvedStyle) -> bool;
        const ID: u64 = 7;
        let base = UiEvent::HlAttrDefine {
            id: ID,
            fg: Some(0x111111),
            bg: Some(0x222222),
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        };
        // each case redefines the baseline with exactly one field moved, so
        // a field dropped from the equality can only be caught by its own
        // case and never masked by another field changing alongside it
        let cases: [(&str, UiEvent, AttrProbe, StyleProbe); 6] = [
            (
                "fg",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x999999),
                    bg: Some(0x222222),
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                |a| a.fg == Some(0x999999),
                |s| s.fg == Some(0x999999),
            ),
            (
                "bg",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x111111),
                    bg: Some(0x888888),
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                |a| a.bg == Some(0x888888),
                |s| s.bg == Some(0x888888),
            ),
            (
                "bold",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x111111),
                    bg: Some(0x222222),
                    bold: true,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                |a| a.bold,
                |s| s.bold,
            ),
            (
                "italic",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x111111),
                    bg: Some(0x222222),
                    bold: false,
                    italic: true,
                    underline: false,
                    reverse: false,
                },
                |a| a.italic,
                |s| s.italic,
            ),
            (
                "underline",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x111111),
                    bg: Some(0x222222),
                    bold: false,
                    italic: false,
                    underline: true,
                    reverse: false,
                },
                |a| a.underline,
                |s| s.underline,
            ),
            (
                "reverse",
                UiEvent::HlAttrDefine {
                    id: ID,
                    fg: Some(0x111111),
                    bg: Some(0x222222),
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: true,
                },
                |a| a.reverse,
                |s| s.reverse,
            ),
        ];

        for (field, redefine, attr_probe, style_probe) in cases {
            let mut m = model();
            let _ = update(&mut m, Msg::Redraw(vec![base.clone()]));
            let style_before =
                crate::theme::Theme::from_hl(m.engine.hl()).style_for(ID, m.engine.hl());
            // the baseline definition is itself a change, so its damage is
            // drained before the redefinition under test runs
            let _ = m.take_paint_damage();

            let _ = update(&mut m, Msg::Redraw(vec![redefine]));

            let stored = m.engine.hl().attr(ID).expect("the id stays defined");
            assert!(
                attr_probe(&stored),
                "{field}: the redefinition never reached the stored attributes ({stored:?})"
            );
            let style_after =
                crate::theme::Theme::from_hl(m.engine.hl()).style_for(ID, m.engine.hl());
            assert!(
                style_probe(&style_after),
                "{field}: the redefinition never reached the resolved style ({style_after:?})"
            );
            assert_ne!(
                style_before, style_after,
                "{field}: every field here changes the style cells paint with"
            );
            assert!(
                m.take_paint_damage().full,
                "{field}: a landed redefinition restyles every cell holding the id"
            );
        }
    }

    #[test]
    fn key_in_engine_focus_becomes_rpc_input_effect() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: "<C-x>".into(),
            }),
        );
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::Input { notation })] if notation == "<C-x>"
        ));
    }

    #[test]
    fn a_keypress_dismisses_an_already_flushed_transient_toast_and_marks_dirty() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::MsgShow {
                    kind: "echomsg".into(),
                    content: vec![(0, "info".into())],
                    replace_last: false,
                },
                UiEvent::Flush,
            ]),
        );
        assert_eq!(m.engine.messages.entries.len(), 1);
        m.dirty = false;

        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "l".into(),
            }),
        );
        assert!(
            m.engine.messages.entries.is_empty(),
            "a transient toast that already survived one Flush must be dismissed on the next keypress"
        );
        assert!(
            m.dirty,
            "dismissing a visible toast must mark the model dirty for a repaint"
        );
    }

    #[test]
    fn a_keypress_never_dismisses_a_persistent_toast() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::MsgShow {
                    kind: "echoerr".into(),
                    content: vec![(0, "boom".into())],
                    replace_last: false,
                },
                UiEvent::Flush,
            ]),
        );

        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "l".into(),
            }),
        );
        assert_eq!(
            m.engine.messages.entries.len(),
            1,
            "an error/warn-kind toast must persist across a keypress"
        );
    }

    #[test]
    fn a_keypress_does_not_dismiss_a_transient_toast_shown_in_the_same_batch_pre_flush() {
        // no Flush yet: the toast has not necessarily been painted even
        // once, so it must survive this keypress and only be dismissed on
        // the one after -- guarantees at least one visible frame
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "info".into())],
                replace_last: false,
            }]),
        );

        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "l".into(),
            }),
        );
        assert_eq!(m.engine.messages.entries.len(), 1);
    }

    // routing table: focus x input-kind -> effect. Pins the seam native
    // overlays build on, using a test-only Focus::Native(OverlayId)
    // placeholder in the absence of a real overlay implementation.

    #[test]
    fn paste_in_engine_focus_becomes_rpc_paste_effect() {
        let mut m = model();
        let effects = update(&mut m, Msg::Paste("hello\nworld".into()));
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::Paste { text })] if text == "hello\nworld"
        ));
    }

    #[test]
    fn mouse_in_engine_focus_becomes_rpc_input_mouse_effect() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Mouse(MouseInput {
                button: "left".into(),
                action: "press".into(),
                modifier: String::new(),
                row: 5,
                col: 10,
            }),
        );
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::InputMouse {
                button,
                action,
                modifier,
                row: 5,
                col: 10,
            })] if button == "left" && action == "press" && modifier.is_empty()
        ));
    }

    #[test]
    fn mouse_row_is_offset_by_reserved_chrome_rows_before_reaching_the_engine() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        // opening a second tab reserves one chrome row for the tabline
        // (Model::chrome_rows), so a click on terminal row 3 must land on
        // grid row 2, not grid row 3.
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        let effects = update(
            &mut m,
            Msg::Mouse(MouseInput {
                button: "left".into(),
                action: "press".into(),
                modifier: String::new(),
                row: 3,
                col: 0,
            }),
        );
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::InputMouse { row: 2, .. })]
        ));
    }

    #[test]
    fn mouse_click_on_a_reserved_chrome_row_is_dropped_not_forwarded() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        let effects = update(
            &mut m,
            Msg::Mouse(MouseInput {
                button: "left".into(),
                action: "press".into(),
                modifier: String::new(),
                row: 0,
                col: 0,
            }),
        );
        assert!(
            effects.is_empty(),
            "click on the tabline row must not reach the engine grid"
        );
    }

    #[test]
    fn key_in_native_focus_is_consumed_and_esc_forwards_without_closing() {
        // a live choice prompt's Esc resolves/aborts nvim's own blocking
        // confirm() call, so it must reach the engine rather than pop
        // locally: a local-only pop would desync view's overlay from
        // nvim's still-blocked prompt, exactly the silent-hang class this
        // overlay exists to prevent (docs/prompt-overlay-wire-capture.md
        // section 2). The overlay only closes later, via the lazy-dismiss
        // keypress that follows nvim's own cmdline_hide -- covered
        // separately below.
        let mut m = model();
        let id = open_overlay(&mut m);
        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: "x".into(),
            }),
        );
        assert!(
            effects.is_empty(),
            "native focus consumes keys that name none of the prompt's choices"
        );
        assert_eq!(
            m.focus(),
            Focus::Native(id),
            "a key naming none of the choices must not close the overlay"
        );

        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: "<Esc>".into(),
            }),
        );
        assert!(
            matches!(
                &effects[..],
                [Effect::Rpc(RpcCall::Input { notation })] if notation == "<Esc>"
            ),
            "Esc on a resolved choice prompt must reach the still-blocked engine: {effects:?}"
        );
        assert_eq!(
            m.focus(),
            Focus::Native(id),
            "forwarding Esc to the engine does not itself close the overlay"
        );
        assert_eq!(m.overlays().len(), 1, "Esc alone pops nothing");

        // nvim resolves its own confirm() call and hides the cmdline; the
        // overlay's lazy-dismiss timing then closes it on the next key,
        // exactly like the underlying toast (see the `Msg::Key` handler's
        // own doc comment)
        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::CmdlineHide]));
        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: "j".into(),
            }),
        );
        assert!(
            matches!(
                &effects[..],
                [Effect::Rpc(RpcCall::Input { notation })] if notation == "j"
            ),
            "closing the overlay hands focus back to the engine within the \
             same dispatch, so the dismissing keystroke also reaches it, \
             the same double duty a keypress already does for dismissing \
             a transient toast: {effects:?}"
        );
        assert_eq!(
            m.focus(),
            Focus::Engine,
            "closing the last overlay returns focus to the engine"
        );
        assert!(m.overlays().is_empty());
    }

    #[test]
    fn any_sequence_of_pushes_pops_and_escapes_leaves_focus_on_the_stack_top() {
        // a seeded xorshift rather than a property-test crate: view-core
        // carries no dev-dependency beyond criterion, and the invariant lives
        // in a state space (stack depth crossed with input kind) small enough
        // that a fixed seed walks all of it. The expected focus comes from a
        // shadow stack rebuilt from the operations alone, so the assertion
        // cannot restate the implementation it checks.
        let mut rng = 0x2545_f491_4f6c_dd1d_u64;
        let mut roll = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        for _ in 0..2_000 {
            let mut m = full_screen_model();
            let mut shadow: Vec<u64> = Vec::new();
            let mut ever_issued: Vec<u64> = Vec::new();

            for _ in 0..(roll() % 24) {
                let depth_before = shadow.len();
                match roll() % 7 {
                    0 => {
                        let id = open_overlay(&mut m).0;
                        assert!(
                            !ever_issued.contains(&id),
                            "an overlay id must never be reissued: {id}"
                        );
                        ever_issued.push(id);
                        shadow.push(id);
                    }
                    1 => {
                        m.pop_overlay();
                        shadow.pop();
                    }
                    2 => {
                        let effects = update(
                            &mut m,
                            Msg::Key(Key {
                                notation: "<Esc>".into(),
                            }),
                        );
                        // Esc always reaches the engine now: with no
                        // overlay open it is plain input, and with a
                        // resolved choice prompt on top (every overlay
                        // this generator opens is one) forwarding it IS
                        // the prompt's resolution keystroke -- nvim owns
                        // closing the overlay later, via the lazy-dismiss
                        // keypress that follows its own cmdline_hide,
                        // which this generator never sends. Esc alone
                        // never pops, so the shadow stack does not either.
                        assert!(
                            matches!(
                                &effects[..],
                                [Effect::Rpc(RpcCall::Input { notation })] if notation == "<Esc>"
                            ),
                            "Esc must always reach the engine: {effects:?}"
                        );
                    }
                    3 => {
                        let effects = update(
                            &mut m,
                            Msg::Key(Key {
                                notation: "j".into(),
                            }),
                        );
                        assert_eq!(
                            effects.is_empty(),
                            depth_before > 0,
                            "an ordinary key reaches the engine only with no overlay open"
                        );
                    }
                    4 => {
                        let effects = update(&mut m, click(0, 0));
                        assert!(
                            matches!(&effects[..], [Effect::Rpc(RpcCall::InputMouse { .. })]),
                            "the terminal corner is outside every overlay opened here"
                        );
                    }
                    5 => {
                        // the middle of the terminal is inside every overlay
                        // this generator opens, and outside all of them when
                        // the stack is empty
                        let effects = update(&mut m, click(12, 40));
                        assert_eq!(
                            effects.is_empty(),
                            depth_before > 0,
                            "a click on a covered cell belongs to the overlay covering it"
                        );
                        assert_eq!(
                            m.overlay_at(12, 40).map(|id| id.0),
                            shadow.last().copied(),
                            "the cell is claimed by the topmost overlay over it"
                        );
                    }
                    _ => {
                        let effects = update(&mut m, Msg::Paste("p".into()));
                        assert_eq!(effects.is_empty(), depth_before > 0);
                    }
                }

                assert_eq!(
                    stack_ids(&m),
                    shadow,
                    "the stack must hold exactly what the operations opened"
                );
                let expected = match shadow.last() {
                    Some(id) => Focus::Native(OverlayId(*id)),
                    None => Focus::Engine,
                };
                assert_eq!(m.focus(), expected, "focus must name the topmost overlay");
                assert!(
                    shadow.len() + 1 >= depth_before,
                    "no single operation may close more than one overlay"
                );
            }
        }
    }

    #[test]
    fn focus_is_the_top_of_the_stack_and_the_engine_when_it_is_empty() {
        let mut m = model();
        assert_eq!(m.focus(), Focus::Engine, "no overlays means engine focus");
        let lower = open_overlay(&mut m);
        assert_eq!(m.focus(), Focus::Native(lower));
        let upper = open_overlay(&mut m);
        assert_eq!(
            m.focus(),
            Focus::Native(upper),
            "the overlay opened last is the one on top"
        );
        m.pop_overlay();
        assert_eq!(m.focus(), Focus::Native(lower));
    }

    #[test]
    fn every_open_overlay_holds_an_id_no_other_one_holds() {
        let mut m = model();
        let ids: Vec<OverlayId> = (0..8).map(|_| open_overlay(&mut m)).collect();
        let mut seen = ids.clone();
        seen.sort_unstable_by_key(|id| id.0);
        seen.dedup_by_key(|id| id.0);
        assert_eq!(seen.len(), ids.len(), "the model must not reissue an id");

        // an id is retired with its overlay rather than recycled: a token
        // held past a close names nothing instead of a later overlay
        let closed = m.pop_overlay().map(|o| o.id);
        let reopened = open_overlay(&mut m);
        assert_ne!(Some(reopened), closed);
        assert_eq!(stack_ids(&m).len(), 8);
    }

    #[test]
    fn esc_on_the_top_of_a_stack_forwards_without_touching_what_is_beneath() {
        // a live choice prompt's Esc forwards to the engine rather than
        // popping (see key_in_native_focus_is_consumed_and_esc_forwards_
        // without_closing): stacking a second one behind the top proves
        // that forwarding leaves the whole stack, not just the top
        // overlay, untouched.
        let mut m = model();
        let picker = open_overlay(&mut m);
        let prompt = open_overlay(&mut m);
        assert_ne!(picker, prompt);

        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: "<Esc>".into(),
            }),
        );
        assert!(
            matches!(
                &effects[..],
                [Effect::Rpc(RpcCall::Input { notation })] if notation == "<Esc>"
            ),
            "Esc on the top choice prompt must reach the engine: {effects:?}"
        );
        assert_eq!(
            m.focus(),
            Focus::Native(prompt),
            "forwarding Esc to the engine does not close the top overlay"
        );
        assert_eq!(
            m.overlays().len(),
            2,
            "Esc alone pops nothing off the stack"
        );
    }

    #[test]
    fn paste_in_native_focus_is_consumed_not_forwarded() {
        let mut m = model();
        open_overlay(&mut m);
        assert!(update(&mut m, Msg::Paste("x".into())).is_empty());
    }

    #[test]
    fn a_click_inside_an_overlay_is_claimed_by_it_and_never_reaches_the_engine() {
        let mut m = full_screen_model();
        let id = open_overlay(&mut m);
        // the overlay covers the middle half of an 80x24 terminal: rows
        // 6..18, columns 20..60
        let effects = update(&mut m, click(12, 40));
        assert!(
            effects.is_empty(),
            "a click on a cell the overlay covers belongs to the overlay"
        );
        assert_eq!(m.overlay_at(12, 40), Some(id));
    }

    #[test]
    fn a_click_outside_an_open_overlay_still_moves_the_engine_cursor() {
        let mut m = full_screen_model();
        open_overlay(&mut m);
        let effects = update(&mut m, click(2, 3));
        assert!(
            matches!(
                &effects[..],
                [Effect::Rpc(RpcCall::InputMouse { row: 2, col: 3, .. })]
            ),
            "grid still visible beside an overlay keeps taking clicks: {effects:?}"
        );
        assert_eq!(m.overlay_at(2, 3), None);
    }

    #[test]
    fn a_click_lands_on_the_topmost_overlay_covering_it_not_the_one_with_focus() {
        let mut m = full_screen_model();
        // a wide overlay with a narrower one stacked on top of it
        let lower = m.push_overlay(OverlayBox::new(100, 100), some_overlay_kind());
        let upper = m.push_overlay(OverlayBox::new(50, 50), some_overlay_kind());
        assert_eq!(
            m.overlay_at(12, 40),
            Some(upper),
            "where both cover the cell, the top one claims it"
        );
        assert_eq!(
            m.overlay_at(0, 0),
            Some(lower),
            "outside the top overlay, the one beneath still claims its own cells"
        );
    }

    #[test]
    fn a_left_anchored_overlay_claims_the_column_it_is_painted_in() {
        use crate::native::geometry::Anchor;
        let mut m = full_screen_model();
        let sidebar = m.push_overlay(
            OverlayBox::new(30, 100).with_anchor(Anchor::Left),
            some_overlay_kind(),
        );
        assert_eq!(
            m.overlay_at(0, 0),
            Some(sidebar),
            "a sidebar owns the terminal's first column, not a centered band"
        );
        assert!(update(&mut m, click(0, 0)).is_empty());
        assert_eq!(m.overlay_at(0, 40), None, "and nothing past its own width");
        assert!(matches!(
            &update(&mut m, click(0, 40))[..],
            [Effect::Rpc(RpcCall::InputMouse { .. })]
        ));
    }

    // gesture routing: a drag belongs to the surface that took its press for
    // the whole of its life, however far the pointer travels

    #[test]
    fn a_drag_off_an_overlay_stays_with_the_overlay_through_its_release() {
        let mut m = full_screen_model();
        open_overlay(&mut m);
        assert!(update(&mut m, mouse("press", 12, 40)).is_empty());
        assert_eq!(m.mouse_capture(), Some(MouseCapture::Overlay(OverlayId(1))));

        assert!(
            update(&mut m, mouse("drag", 2, 3)).is_empty(),
            "a drag that left the overlay must not start moving the engine cursor"
        );
        assert!(
            update(&mut m, mouse("release", 2, 3)).is_empty(),
            "the engine must not receive a release for a press it never saw"
        );
        assert_eq!(m.mouse_capture(), None, "release ends the gesture");
    }

    #[test]
    fn a_drag_onto_an_overlay_keeps_delivering_to_the_engine_until_release() {
        let mut m = full_screen_model();
        open_overlay(&mut m);
        assert!(matches!(
            &update(&mut m, mouse("press", 2, 3))[..],
            [Effect::Rpc(RpcCall::InputMouse { row: 2, col: 3, .. })]
        ));
        assert_eq!(m.mouse_capture(), Some(MouseCapture::Engine));

        // without capture the engine would be left mid-selection here, with
        // a press it never got to finish
        assert!(
            matches!(
                &update(&mut m, mouse("drag", 12, 40))[..],
                [Effect::Rpc(RpcCall::InputMouse {
                    row: 12,
                    col: 40,
                    ..
                })]
            ),
            "a drag that crossed onto the overlay still belongs to the engine"
        );
        assert!(matches!(
            &update(&mut m, mouse("release", 12, 40))[..],
            [Effect::Rpc(RpcCall::InputMouse { .. })]
        ));
        assert_eq!(m.mouse_capture(), None);
    }

    #[test]
    fn closing_an_overlay_mid_gesture_releases_the_capture_it_held() {
        // closes the overlay by a direct pop rather than Esc: a live
        // choice prompt's Esc forwards to the engine instead of popping
        // (see esc_on_the_top_of_a_stack_forwards_without_touching_what_
        // is_beneath), which is a separate concern from the one this
        // test checks -- that a gesture capture does not outlive the
        // overlay that held it, however the overlay came to close.
        let mut m = full_screen_model();
        open_overlay(&mut m);
        let _ = update(&mut m, mouse("press", 12, 40));
        assert!(m.mouse_capture().is_some());

        m.pop_overlay();
        assert_eq!(
            m.mouse_capture(),
            None,
            "a gesture must not stay captured by an overlay that is gone"
        );
        assert!(
            matches!(
                &update(&mut m, mouse("release", 2, 3))[..],
                [Effect::Rpc(RpcCall::InputMouse { .. })]
            ),
            "with the overlay closed the gesture falls back to position routing"
        );
    }

    #[test]
    fn the_wheel_routes_by_position_and_leaves_a_gesture_in_flight_alone() {
        let mut m = full_screen_model();
        open_overlay(&mut m);
        let _ = update(&mut m, mouse("press", 2, 3));
        assert_eq!(m.mouse_capture(), Some(MouseCapture::Engine));

        let wheel = Msg::Mouse(MouseInput {
            button: "wheel".into(),
            action: "up".into(),
            modifier: String::new(),
            row: 12,
            col: 40,
        });
        assert!(
            update(&mut m, wheel).is_empty(),
            "a wheel over the overlay scrolls the overlay, not the buffer under it"
        );
        assert_eq!(
            m.mouse_capture(),
            Some(MouseCapture::Engine),
            "a wheel carries no gesture and must not steal one in flight"
        );
    }

    #[test]
    fn a_release_with_no_press_behind_it_falls_back_to_position() {
        let mut m = full_screen_model();
        open_overlay(&mut m);
        assert_eq!(m.mouse_capture(), None);
        assert!(matches!(
            &update(&mut m, mouse("release", 2, 3))[..],
            [Effect::Rpc(RpcCall::InputMouse { .. })]
        ));
        assert!(
            update(&mut m, mouse("drag", 12, 40)).is_empty(),
            "an unclaimed drag over the overlay belongs to the overlay"
        );
    }

    #[test]
    fn mouse_on_off_events_set_the_model_flag_the_terminal_reads_for_capture() {
        let mut m = model();
        assert!(!m.engine.mouse_on, "mouse capture must default off");
        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::MouseOn]));
        assert!(m.engine.mouse_on);
        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::MouseOff]));
        assert!(!m.engine.mouse_on);
    }

    #[test]
    fn engine_down_maps_signal_and_code_to_exit_effects() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::EngineDown(ExitInfo {
                code: Some(5),
                by_signal: false,
            }),
        );
        assert!(matches!(&effects[..], [Effect::Quit { exit_code: 5 }]));
        assert!(!m.running);
    }

    #[test]
    fn engine_down_without_code_exits_one_and_signal_code_passes_through() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::EngineDown(ExitInfo {
                code: None,
                by_signal: false,
            }),
        );
        assert!(matches!(&effects[..], [Effect::Quit { exit_code: 1 }]));
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::EngineDown(ExitInfo {
                code: Some(137),
                by_signal: true,
            }),
        );
        assert!(matches!(&effects[..], [Effect::Quit { exit_code: 137 }]));
    }

    #[test]
    fn loop_tokens_are_noops_and_engine_request_always_replies() {
        let mut m = model();
        assert!(update(&mut m, Msg::RedrawReady).is_empty());
        assert!(update(&mut m, Msg::EngineStopped(None)).is_empty());
        let effects = update(
            &mut m,
            Msg::EngineRequest(EngineRequest::VimEnter {
                token: ReplyToken { msgid: 9 },
            }),
        );
        assert!(matches!(
            &effects[..],
            [Effect::Reply {
                token: ReplyToken { msgid: 9 },
                value: ReplyValue::Nil
            }]
        ));
    }

    /// The worker owns the actual reply (see `Effect::ClipboardRead`'s
    /// doc); this arm's whole job is routing the token and register
    /// through unmodified.
    #[test]
    fn clipboard_get_produces_a_clipboard_read_effect_carrying_the_token_and_register() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::EngineRequest(EngineRequest::ClipboardGet {
                token: ReplyToken { msgid: 11 },
                register: '+',
            }),
        );
        assert!(matches!(
            &effects[..],
            [Effect::ClipboardRead {
                token: ReplyToken { msgid: 11 },
                register: '+'
            }]
        ));
    }

    /// One arm, two effects: the local write and the OSC52 escape are
    /// companions (see `Effect::Osc52Copy`'s doc), not a branch on whether
    /// a display is present, and both must carry the same `lines` and
    /// `regtype` the copy itself carried -- deleting either effect from
    /// this arm would fail nothing else in the suite.
    #[test]
    fn clipboard_set_produces_a_write_and_an_osc52_copy_effect_from_one_arm() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::EngineRequest(EngineRequest::ClipboardSet {
                token: ReplyToken { msgid: 12 },
                register: '*',
                lines: vec!["a".to_string(), "b".to_string()],
                regtype: RegisterType::Linewise,
            }),
        );
        assert!(matches!(
            &effects[..],
            [
                Effect::ClipboardWrite {
                    token: ReplyToken { msgid: 12 },
                    register: '*',
                    lines: ref w_lines,
                    regtype: RegisterType::Linewise,
                },
                Effect::Osc52Copy {
                    register: '*',
                    lines: ref o_lines,
                    regtype: RegisterType::Linewise,
                }
            ] if w_lines == &vec!["a".to_string(), "b".to_string()]
                && o_lines == &vec!["a".to_string(), "b".to_string()]
        ));
    }

    #[test]
    fn resize_marks_the_model_dirty_so_the_new_area_is_actually_painted() {
        let mut m = model();
        m.dirty = false;
        let _ = update(
            &mut m,
            Msg::Resized {
                width: 120,
                height: 40,
            },
        );
        assert!(
            m.dirty,
            "a resize changes the paint area, so the frontend must repaint on its own \
             rather than waiting for an engine redraw that a clamped grid_target may never produce"
        );
    }

    #[test]
    fn resize_produces_try_resize_effect() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Resized {
                width: 120,
                height: 40,
            },
        );
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::TryResize {
                width: 120,
                height: 40
            })]
        ));
    }

    // fixtures below are shaped from crates/view-engine/src/ui_events.rs's
    // decode test fixtures, which are themselves copied from the live
    // capture at ~/.claude/tmp/capture-*.log (see that module's tests for
    // the raw wire fixtures); these tests exercise state application only, not
    // decoding.

    #[test]
    fn mode_events_update_state_without_dirty() {
        use crate::events::ModeInfo;
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::ModeInfoSet {
                    cursor_style_enabled: true,
                    modes: vec![
                        ModeInfo {
                            name: "normal".into(),
                            cursor_shape: "block".into(),
                            ..ModeInfo::default()
                        },
                        ModeInfo {
                            name: "insert".into(),
                            cursor_shape: "vertical".into(),
                            cell_percentage: 25,
                            ..ModeInfo::default()
                        },
                    ],
                },
                UiEvent::ModeChange {
                    mode: "insert".into(),
                    mode_idx: 1,
                },
            ]),
        );
        assert!(effects.is_empty());
        assert!(!m.dirty, "mode events alone must not request paint");
        assert!(m.engine.mode.cursor_style_enabled);
        assert_eq!(m.engine.mode.current, "insert");
        assert_eq!(
            m.engine
                .mode
                .active_cursor()
                .map(|c| c.cursor_shape.as_str()),
            Some("vertical")
        );
    }

    #[test]
    fn mode_state_active_cursor_is_none_before_mode_info_set() {
        let m = model();
        assert!(m.engine.mode.active_cursor().is_none());
    }

    #[test]
    fn cmdline_show_pos_hide_set_and_clear_state() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlineShow {
                content: vec![(0, "q".into())],
                pos: 1,
                firstc: ":".into(),
                prompt: "".into(),
                indent: 0,
                level: 1,
            }]),
        );
        assert!(!m.dirty);
        let cmdline = m.engine.cmdline.as_ref().expect("cmdline must be set");
        assert_eq!(cmdline.content, vec![(0, "q".to_string())]);
        assert_eq!(cmdline.firstc, ":");

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlinePos { pos: 2, level: 1 }]),
        );
        assert_eq!(m.engine.cmdline.as_ref().unwrap().pos, 2);

        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::CmdlineHide]));
        assert!(m.engine.cmdline.is_none());
    }

    #[test]
    fn a_confirm_questions_toast_lives_exactly_as_long_as_the_prompt_does() {
        // the event order is nvim 0.12.4's own, observed over a real RPC
        // session driving `confirm('Save changes?', "&Yes\n&No")`: the
        // question arrives once as msg_show, the answer line separately as
        // cmdline_show, and a key answering none of the choices re-arms the
        // prompt with cmdline_hide + cmdline_show and NO second msg_show.
        let mut m = model();
        let prompt = || UiEvent::CmdlineShow {
            content: vec![],
            pos: 0,
            firstc: String::new(),
            prompt: "[Y]es, (N)o: ".into(),
            indent: 0,
            level: 1,
        };
        let question = || UiEvent::MsgShow {
            kind: "confirm".into(),
            content: vec![(0, "Save changes?".into())],
            replace_last: false,
        };
        let shown = |m: &Model| {
            m.engine
                .messages
                .visible_lines(4)
                .into_iter()
                .map(|spans| spans.into_iter().map(|s| s.text).collect::<String>())
                .collect::<Vec<_>>()
        };

        let _ = update(&mut m, Msg::Redraw(vec![question(), UiEvent::Flush]));
        let _ = update(&mut m, Msg::Redraw(vec![prompt(), UiEvent::Flush]));
        assert_eq!(shown(&m), vec!["Save changes?"]);

        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "q".into(),
            }),
        );
        assert_eq!(
            shown(&m),
            vec!["Save changes?"],
            "a key the prompt refuses must not take the question away with it"
        );

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlineHide, UiEvent::Flush]),
        );
        let _ = update(&mut m, Msg::Redraw(vec![prompt(), UiEvent::Flush]));
        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "y".into(),
            }),
        );
        assert_eq!(shown(&m), vec!["Save changes?"]);

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlineHide, UiEvent::Flush]),
        );
        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "j".into(),
            }),
        );
        assert!(
            shown(&m).is_empty(),
            "with the prompt closed the question is ordinary transient text"
        );
    }

    #[test]
    fn a_second_distinct_confirm_replaces_the_first_with_no_intervening_keystroke() {
        // nvim's own Lua can resolve one confirm and immediately raise a
        // second, distinct one before the overlay ever sees a keystroke --
        // routine in plugin bootstraps. A genuine re-arm of the SAME
        // question never sends a second msg_show at all (see
        // docs/prompt-overlay-wire-capture.md, section 1), so a msg_show
        // arriving while a Prompt overlay is already open always names a
        // distinct question, never the open one.
        let mut m = model();
        let msg_show = |text: &str| UiEvent::MsgShow {
            kind: "confirm".into(),
            content: vec![(0, text.into())],
            replace_last: false,
        };
        let cmdline_show = |prompt: &str| UiEvent::CmdlineShow {
            content: vec![],
            pos: 0,
            firstc: String::new(),
            prompt: prompt.into(),
            indent: 0,
            level: 1,
        };
        let prompt_view = |m: &Model| match m.overlays().last().map(|ov| &ov.kind) {
            Some(OverlayKind::Prompt(p)) => Some(p.view()),
            _ => None,
        };

        let _ = update(
            &mut m,
            Msg::Redraw(vec![msg_show("Save changes?"), UiEvent::Flush]),
        );
        let _ = update(
            &mut m,
            Msg::Redraw(vec![cmdline_show("[Y]es, (N)o: "), UiEvent::Flush]),
        );
        let first = prompt_view(&m).expect("a prompt overlay must open on the first confirm");
        assert_eq!(first.message, "Save changes?");
        assert_eq!(first.choices, vec!["Yes".to_string(), "No".to_string()]);

        // #1 resolves and #2 arrives in the same redraw batch, with no
        // Msg::Key between them -- the shape a Lua-driven bootstrap sends.
        let _ = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::CmdlineHide,
                msg_show("Discard changes?"),
                cmdline_show("[D]iscard, (C)ancel: "),
                UiEvent::Flush,
            ]),
        );

        let second =
            prompt_view(&m).expect("the overlay must stay open, now answering the new question");
        assert_eq!(
            second.message, "Discard changes?",
            "the overlay must show #2's message, not #1's stale one"
        );
        assert_eq!(
            second.choices,
            vec!["Discard".to_string(), "Cancel".to_string()],
            "and #2's choices, coherent with #2's message"
        );
    }

    #[test]
    fn cmdline_pos_without_prior_show_is_a_noop() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlinePos { pos: 3, level: 1 }]),
        );
        assert!(m.engine.cmdline.is_none());
    }

    #[test]
    fn msg_show_appends_and_replace_last_overwrites() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(73, "first".into())],
                replace_last: false,
            }]),
        );
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(73, "second".into())],
                replace_last: false,
            }]),
        );
        assert_eq!(m.engine.messages.entries.len(), 2);

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(73, "replaced".into())],
                replace_last: true,
            }]),
        );
        assert_eq!(m.engine.messages.entries.len(), 2);
        assert_eq!(
            m.engine.messages.entries.last().unwrap().content,
            vec![(73, "replaced".to_string())]
        );

        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::MsgClear]));
        assert!(m.engine.messages.entries.is_empty());
    }

    #[test]
    fn tabline_update_sets_current_and_tabs() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(2),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "[No Name]".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "[No Name]".into(),
                    },
                ],
            }]),
        );
        assert!(!m.dirty);
        let tabline = m.engine.tabline.as_ref().expect("tabline must be set");
        assert_eq!(tabline.current, TabHandle(2));
        assert_eq!(tabline.tabs.len(), 2);
    }

    #[test]
    fn popupmenu_show_select_hide_set_and_clear_state() {
        use crate::events::PmItem;
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::PopupmenuShow {
                items: vec![PmItem {
                    word: "foobar".into(),
                    ..PmItem::default()
                }],
                selected: 0,
                row: 0,
                col: 11,
                grid: 1,
            }]),
        );
        assert_eq!(m.engine.popupmenu.as_ref().unwrap().selected, 0);

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::PopupmenuSelect { selected: 1 }]),
        );
        assert_eq!(m.engine.popupmenu.as_ref().unwrap().selected, 1);

        let _ = update(&mut m, Msg::Redraw(vec![UiEvent::PopupmenuHide]));
        assert!(m.engine.popupmenu.is_none());
    }

    #[test]
    fn popupmenu_select_without_prior_show_is_a_noop() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::PopupmenuSelect { selected: 0 }]),
        );
        assert!(m.engine.popupmenu.is_none());
    }

    #[test]
    fn resize_target_shrinks_by_chrome_rows_once_more_than_one_tab_is_open() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        let effects = update(
            &mut m,
            Msg::Resized {
                width: 80,
                height: 24,
            },
        );
        assert!(matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::TryResize {
                width: 80,
                height: 23
            })]
        ));
    }

    #[test]
    fn tabline_crossing_the_one_tab_boundary_round_trips_the_reserved_row() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Resized {
                width: 80,
                height: 24,
            },
        );

        let opened = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        assert!(
            matches!(
                &opened[..],
                [Effect::Rpc(RpcCall::TryResize {
                    width: 80,
                    height: 23
                })]
            ),
            "opening a second tab must reserve the tabline row: {opened:?}"
        );

        let closed = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![TabEntry {
                    tab: TabHandle(1),
                    name: "a".into(),
                }],
            }]),
        );
        assert!(
            matches!(
                &closed[..],
                [Effect::Rpc(RpcCall::TryResize {
                    width: 80,
                    height: 24
                })]
            ),
            "closing back to one tab must release the reserved row: {closed:?}"
        );
    }

    #[test]
    fn tabline_update_within_the_same_tab_count_bucket_emits_no_resize() {
        use crate::events::{TabEntry, TabHandle};
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        // renaming a tab (still 2 tabs) never crosses the boundary
        let effects = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::TablineUpdate {
                current: TabHandle(1),
                tabs: vec![
                    TabEntry {
                        tab: TabHandle(1),
                        name: "a-renamed".into(),
                    },
                    TabEntry {
                        tab: TabHandle(2),
                        name: "b".into(),
                    },
                ],
            }]),
        );
        assert!(effects.is_empty());
    }

    /// A style change has no rows of its own: the highlight table sits
    /// behind every cell's resolved style, so any change to it can restyle
    /// the whole screen while no grid cell's text moves. Damage clipped to
    /// grid rows alone would repaint whatever row happened to change and
    /// leave the rest of the screen in the previous colors.
    #[test]
    fn a_highlight_change_with_no_grid_change_damages_the_whole_frame() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::GridResize {
                grid: 1,
                width: 8,
                height: 4,
            }]),
        );
        let _ = m.take_paint_damage();

        for ev in [
            UiEvent::DefaultColorsSet {
                fg: Some(0xF8F8F2),
                bg: Some(0x282A36),
                sp: None,
            },
            UiEvent::HlAttrDefine {
                id: 3,
                fg: Some(0xFF0000),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
            UiEvent::HlGroupSet {
                name: "StatusLine".to_string(),
                hl_id: 3,
            },
        ] {
            let _ = update(&mut m, Msg::Redraw(vec![ev.clone()]));
            assert!(
                m.take_paint_damage().full,
                "{ev:?} must damage every cell, not none of them"
            );
        }

        let generation = m.engine.hl().probe_generation();
        let _ = update(
            &mut m,
            Msg::HlProbeReply {
                generation,
                fg: Some(0xF8F8F2),
                bg: Some(0x282A36),
            },
        );
        assert!(
            m.take_paint_damage().full,
            "an accepted probe reply re-derives the theme and must damage every cell"
        );
    }

    /// The one write in the highlight table that marks changed without
    /// comparing values first, and the state that makes the difference
    /// visible: nvim resends the same default colors, so neither colour
    /// moves, but the probe generation those colours carry moves anyway and
    /// strands a `confirmed` reply that was matching an instant earlier.
    /// The derived theme changes with no value in the write itself
    /// changing, so a value comparison here would leave the whole screen
    /// painted in a theme the model no longer holds.
    ///
    /// Reaching this live needs `nvim_get_hl`'s answer to differ from the
    /// `default_colors_set` that opened the same generation, so the state
    /// is built from an explicit event sequence rather than left to a
    /// live path to happen upon.
    #[test]
    fn resending_identical_default_colors_still_damages_the_frame() {
        let mut m = model();
        let resend = UiEvent::DefaultColorsSet {
            fg: Some(0xFFFFFF),
            bg: Some(0),
            sp: None,
        };
        let _ = update(&mut m, Msg::Redraw(vec![resend.clone()]));
        let generation = m.engine.hl().probe_generation();
        // the reply disambiguates the wire-ambiguous zero and reports a fg
        // the wire never carried, so the theme it derives is distinguishable
        // from the one the raw wire values derive on their own
        let _ = update(
            &mut m,
            Msg::HlProbeReply {
                generation,
                fg: Some(0xF8F8F2),
                bg: Some(0x123456),
            },
        );
        let before = crate::theme::Theme::from_hl(m.engine.hl());
        let _ = m.take_paint_damage();

        let _ = update(&mut m, Msg::Redraw(vec![resend]));

        assert_eq!(
            (m.engine.hl().default_fg(), m.engine.hl().default_bg()),
            (Some(0xFFFFFF), Some(0)),
            "the resent colours must be identical, or this proves nothing"
        );
        let after = crate::theme::Theme::from_hl(m.engine.hl());
        assert_ne!(
            before, after,
            "the bumped generation strands the matching reply and re-derives the theme"
        );
        assert!(
            m.take_paint_damage().full,
            "a re-derived theme restyles every cell, however little the write itself changed"
        );
    }

    /// Installing a whole highlight table is a style change like any other:
    /// every painted cell resolves through the table, so the frame after
    /// the swap must repaint all of them even though no grid row moved.
    /// The installed table is drained clean first, so the only thing that
    /// can supply the damage is the install itself.
    #[test]
    fn installing_a_whole_highlight_table_damages_the_frame() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::GridResize {
                grid: 1,
                width: 8,
                height: 4,
            }]),
        );
        let _ = m.take_paint_damage();

        let mut replacement = crate::hl::HlTable::new();
        replacement.define_attr(
            3,
            HlAttr {
                fg: Some(0x00FF00),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
        );
        assert!(
            replacement.take_dirty(),
            "the replacement must arrive carrying no damage of its own"
        );

        m.engine.replace_hl(replacement);

        assert_eq!(
            m.engine.hl().attr(3).map(|a| a.fg),
            Some(Some(0x00FF00)),
            "the replacement must actually be installed"
        );
        assert!(
            m.take_paint_damage().full,
            "a swapped table restyles every cell on screen at once"
        );
    }

    /// A first-ever definition of an `hl_id` damages the whole frame the
    /// same way a redefinition does, and the pessimism is deliberate: the
    /// highlight table holds no record of which ids the painted cells
    /// reference, so it cannot tell a definition that restyles nothing from
    /// one that restyles cells already on screen resolving that id through
    /// the `Normal` fallback. Skipping the mark for an id the table has not
    /// seen would trade one whole-frame composite for a silently stale
    /// screen wherever nvim's own define-before-use ordering does not hold,
    /// which the table cannot check, against a damage contract that
    /// over-reports rather than under-reports.
    #[test]
    fn a_first_definition_of_a_previously_unknown_hl_id_still_damages_the_frame() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::GridResize {
                grid: 1,
                width: 8,
                height: 4,
            }]),
        );
        let _ = m.take_paint_damage();
        assert!(
            m.engine.hl().attr(12).is_none(),
            "the id must be one the table has never held"
        );

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::HlAttrDefine {
                id: 12,
                fg: Some(0x00FF00),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            }]),
        );

        assert!(
            m.take_paint_damage().full,
            "a first definition may still restyle cells already holding the id"
        );
    }

    /// The other direction: a definition that changes nothing must not cost
    /// a repaint. The value comparison this pins is defensive rather than
    /// load-bearing -- the pinned engine was measured never to resend an
    /// unchanged definition (see [`HlTable::define_attr`]) -- but an engine
    /// that did would turn every resend into a whole-frame repaint, giving
    /// back the frames damage clipping exists to save.
    #[test]
    fn a_highlight_definition_that_changes_nothing_produces_no_damage() {
        let mut m = model();
        let define = UiEvent::HlAttrDefine {
            id: 3,
            fg: Some(0xFF0000),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        };
        let group = UiEvent::HlGroupSet {
            name: "StatusLine".to_string(),
            hl_id: 3,
        };
        let _ = update(&mut m, Msg::Redraw(vec![define.clone(), group.clone()]));
        let _ = m.take_paint_damage();

        let _ = update(&mut m, Msg::Redraw(vec![define, group]));
        let damage = m.take_paint_damage();
        assert!(!damage.full, "an identical redefinition is not a repaint");
        assert!(damage.rows.is_empty(), "and touches no grid row either");
    }

    #[test]
    fn default_colors_set_bumps_generation_and_emits_a_matching_probe_effect() {
        let mut m = model();
        assert_eq!(m.engine.hl().probe_generation(), 0);
        let effects = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0xFFFFFF),
                bg: Some(0),
                sp: None,
            }]),
        );
        assert_eq!(m.engine.hl().probe_generation(), 1);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Rpc(RpcCall::GetDefaultHl { generation: 1 })]
        ));
    }

    #[test]
    fn a_second_default_colors_set_bumps_generation_again() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: None,
                bg: Some(0),
                sp: None,
            }]),
        );
        let effects = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: None,
                bg: Some(0x282a36),
                sp: None,
            }]),
        );
        assert_eq!(m.engine.hl().probe_generation(), 2);
        assert!(matches!(
            effects.as_slice(),
            [Effect::Rpc(RpcCall::GetDefaultHl { generation: 2 })]
        ));
    }

    #[test]
    fn hl_probe_reply_matching_the_current_generation_is_accepted_and_marks_dirty() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0x111111),
                bg: Some(0),
                sp: None,
            }]),
        );
        m.dirty = false;
        let effects = update(
            &mut m,
            Msg::HlProbeReply {
                generation: 1,
                fg: Some(0x111111),
                bg: None,
            },
        );
        assert!(effects.is_empty());
        assert!(m.dirty, "an accepted probe reply must trigger a repaint");
        let confirmed = m.engine.hl().confirmed().expect("reply must be recorded");
        assert_eq!(confirmed.generation, 1);
        assert_eq!(confirmed.fg, Some(0x111111));
        assert_eq!(confirmed.bg, None);
    }

    /// The write-time half of the generation guard: a reply for a
    /// superseded generation must never overwrite a newer (or absent)
    /// confirmed value, even though nothing else has happened between the
    /// two `DefaultColorsSet`s to make the stale reply's arrival implausible
    /// on a real connection (out-of-order delivery is not assumed to be
    /// impossible, only rare).
    #[test]
    fn hl_probe_reply_for_a_stale_generation_is_dropped_without_marking_dirty() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: None,
                bg: Some(0),
                sp: None,
            }]),
        );
        // a second DefaultColorsSet bumps the generation to 2 before the
        // generation-1 probe's reply ever arrives
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: None,
                bg: Some(0x282a36),
                sp: None,
            }]),
        );
        m.dirty = false;
        let effects = update(
            &mut m,
            Msg::HlProbeReply {
                generation: 1,
                fg: None,
                bg: None,
            },
        );
        assert!(effects.is_empty());
        assert!(
            !m.dirty,
            "a stale-generation reply must not trigger a repaint"
        );
        assert!(
            m.engine.hl().confirmed().is_none(),
            "a stale-generation reply must not be recorded"
        );
    }

    /// A colorscheme switch mid-session: `Normal` goes from confirmed
    /// transparent to a wire-ambiguous zero that will turn out to be
    /// genuinely black, once its own probe replies. Between the switch's
    /// `DefaultColorsSet` and that reply, `Theme::from_hl` must keep
    /// reading the *old* confirmed value (still transparent) rather than
    /// painting the new, not-yet-disambiguated wire zero -- only once the
    /// matching reply lands does the theme converge on black.
    #[test]
    fn colorscheme_switch_to_ambiguous_zero_holds_the_prior_confirmed_value_until_its_own_probe_replies(
    ) {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0xF8F8F2),
                bg: Some(0),
                sp: None,
            }]),
        );
        let _ = update(
            &mut m,
            Msg::HlProbeReply {
                generation: 1,
                fg: Some(0xF8F8F2),
                bg: None,
            },
        );
        assert_eq!(crate::theme::Theme::from_hl(m.engine.hl()).bg, None);

        // the colorscheme switch: a second DefaultColorsSet, still an
        // ambiguous wire zero, bumps the generation and starts a fresh probe
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0xFFFFFF),
                bg: Some(0),
                sp: None,
            }]),
        );
        assert_eq!(
            crate::theme::Theme::from_hl(m.engine.hl()).bg,
            None,
            "must keep the prior confirmed transparent value while the new probe is in flight"
        );

        let _ = update(
            &mut m,
            Msg::HlProbeReply {
                generation: 2,
                fg: Some(0xFFFFFF),
                bg: Some(0),
            },
        );
        assert_eq!(
            crate::theme::Theme::from_hl(m.engine.hl()).bg,
            Some(0),
            "must converge on the new theme's genuinely-black bg once its probe replies"
        );
    }

    #[test]
    fn an_unregistered_invoke_says_so_rather_than_going_quiet() {
        // every registered picker verb (files/buffers/grep) now has a real
        // handler, so a feature+verb the registry has never heard of is the
        // only way left to exercise this "must not go quiet" contract
        // end-to-end; the sibling test below locks the wording for the
        // still-real "registered but unhandled" branch directly
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "wat".to_string(),
                verb: "huh".to_string(),
            },
        );
        assert!(
            matches!(effects.as_slice(), [Effect::ScheduleToastExpiry { .. }]),
            "an unrecognized invoke must not talk to the engine, only schedule \
             its own notice's expiry through the same choke point every other \
             locally-synthesized notice uses: {effects:?}"
        );
        let entry = m
            .engine
            .messages
            .entries
            .last()
            .expect("the invoke must reach the user through the message surface");
        let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
        assert!(
            text.contains("picker files"),
            "an unrecognized invoke must fall back to the usage line, not go quiet, \
             got {text:?}"
        );
        assert!(
            text.starts_with(":View "),
            "the usage line opens with the ex-command itself, never a `view: ` \
             prefix stuttering into `view: :View ...`, got {text:?}"
        );
        assert!(m.dirty, "the notice must be painted without another event");
        assert!(
            m.overlays().is_empty(),
            "no overlay exists to open yet: an invoke must not push one"
        );
    }

    #[test]
    fn a_registered_verb_with_no_handler_yet_would_name_what_was_invoked() {
        // no registry entry is currently unhandled (picker answers all
        // three of its own), so this exercises `feature_invoke_notice`
        // directly rather than through a real, presently-unreachable
        // registry gap; it stands ready for the next feature that lands a
        // registry row before its own update() handler
        let text = feature_invoke_notice("tree", "open", true);
        assert_eq!(text, "view: no handler for tree open in this build");
    }

    #[test]
    fn a_feature_invoke_while_a_blocked_prompt_is_topmost_does_not_steal_focus() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "confirm".into(),
                content: vec![(0, "test".into())],
                replace_last: false,
            }]),
        );
        let prompt_id = match m.focus() {
            Focus::Native(id) => id,
            Focus::Engine => unreachable!("MsgShow must open a Prompt overlay"),
        };
        assert!(matches!(
            m.overlays().last().map(|o| &o.kind),
            Some(OverlayKind::Prompt(_))
        ));

        let effects = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "files".to_string(),
            },
        );
        assert!(
            matches!(effects.as_slice(), [Effect::PickerQuery { .. }]),
            "opening beneath the prompt must still issue the picker's first \
             query: {effects:?}"
        );
        assert_eq!(
            m.focus(),
            Focus::Native(prompt_id),
            "a picker opening while a blocked prompt is topmost must not \
             steal its focus"
        );
        assert_eq!(
            m.overlays().len(),
            2,
            "the picker must open beneath the prompt, not replace it"
        );
        assert!(
            matches!(m.overlays()[0].kind, OverlayKind::Picker(_)),
            "the picker must sit beneath the prompt on the stack: {:?}",
            m.overlays()
        );
        assert!(
            matches!(m.overlays()[1].kind, OverlayKind::Prompt(_)),
            "the prompt must remain topmost: {:?}",
            m.overlays()
        );
        assert!(
            m.picker_mut().is_some(),
            "the picker must still be reachable so a streamed reply can \
             reach it even while the prompt holds focus"
        );
    }

    #[test]
    fn a_prompt_opening_over_an_open_picker_takes_focus_and_returns_it_on_resolve() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "files".to_string(),
            },
        );
        let picker_id = match m.focus() {
            Focus::Native(id) => id,
            Focus::Engine => unreachable!("FeatureInvoke picker files must open a Picker overlay"),
        };

        // the same real msg_show + cmdline_show pairing
        // a_confirm_questions_toast_lives_exactly_as_long_as_the_prompt_does
        // captured against the pinned engine
        let _ = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::MsgShow {
                    kind: "confirm".into(),
                    content: vec![(0, "Save changes?".into())],
                    replace_last: false,
                },
                UiEvent::Flush,
            ]),
        );
        let _ = update(
            &mut m,
            Msg::Redraw(vec![
                UiEvent::CmdlineShow {
                    content: vec![],
                    pos: 0,
                    firstc: String::new(),
                    prompt: "[Y]es, (N)o: ".into(),
                    indent: 0,
                    level: 1,
                },
                UiEvent::Flush,
            ]),
        );

        let prompt_id = match m.focus() {
            Focus::Native(id) => id,
            Focus::Engine => unreachable!("MsgShow must open a Prompt overlay over the picker"),
        };
        assert_ne!(
            prompt_id, picker_id,
            "the prompt must be a distinct, new overlay"
        );
        assert_eq!(
            m.overlays().len(),
            2,
            "the prompt must stack over the picker, not replace it"
        );
        assert!(
            matches!(m.overlays()[0].kind, OverlayKind::Picker(_)),
            "the picker must remain underneath the prompt: {:?}",
            m.overlays()
        );

        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::CmdlineHide, UiEvent::Flush]),
        );
        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "y".into(),
            }),
        );

        assert_eq!(
            m.focus(),
            Focus::Native(picker_id),
            "resolving the prompt must return focus to the picker underneath it"
        );
        assert_eq!(
            m.overlays().len(),
            1,
            "the resolved prompt must be gone from the stack"
        );
    }

    /// The real close path a user takes to abandon a picker: `<Esc>` routed
    /// through `update()`, not a direct `model.pop_overlay()` call, must
    /// emit `Effect::PickerClose` so the matcher worker drops its live
    /// `Session` and stops any `Files` scan still walking a huge tree (see
    /// `Effect::PickerClose`'s own doc). Disabling the emission at the
    /// `Msg::Key` `<Esc>`-on-Picker arm makes this fail by name.
    #[test]
    fn esc_on_an_open_picker_emits_picker_close() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "files".to_string(),
            },
        );
        assert!(
            matches!(
                m.overlays().last().map(|o| &o.kind),
                Some(OverlayKind::Picker(_))
            ),
            "FeatureInvoke picker/files must open a Picker overlay on top"
        );

        // cleared so the dirty assertion below can only be satisfied by the
        // close arm itself, not by the open that preceded it
        m.dirty = false;
        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: "<Esc>".into(),
            }),
        );

        assert!(m.overlays().is_empty(), "Esc must close the picker overlay");
        assert!(
            matches!(effects.as_slice(), [Effect::PickerClose]),
            "closing the picker via Esc must emit exactly Effect::PickerClose: \
             {effects:?}"
        );
        // the paint loop repaints only when dirty, and a pop produces no
        // engine redraw: without this the closed picker stays on screen
        // until some unrelated event repaints (observed as a bench desync
        // whose failure screen still showed the popped overlay)
        assert!(m.dirty, "closing the picker must mark the model dirty");
    }

    /// A candidate landing in `Msg::PickerResults` must itself trigger a
    /// preview request for the now-selected row -- there is no separate
    /// navigation message yet, so `PickerResults` is the only place a
    /// selection is ever established. Disabling `picker_preview_request`'s
    /// call in that arm makes this fail by name.
    #[test]
    fn picker_results_issues_a_preview_request_for_the_selected_candidate() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "files".to_string(),
            },
        );
        let generation = m.picker_mut().expect("picker must be open").generation();

        let effects = update(
            &mut m,
            Msg::PickerResults {
                generation,
                items: vec![crate::native::picker::PickerItem::new("a.rs")],
            },
        );

        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Rpc(RpcCall::PreviewBuffer { path, .. })] if path.ends_with("a.rs")
            ),
            "a fresh result set must issue exactly one PreviewBuffer request for \
             the selected candidate: {effects:?}"
        );
    }

    /// A `LiveGrep` query commonly streams several result batches while the
    /// scan is still running, and a later batch reordering rows without
    /// changing the *selected* candidate must not re-issue a preview
    /// request for a path already current or in flight -- two successive
    /// `PickerResults` batches selecting the same first row must together
    /// issue exactly one `PreviewBuffer` request, not two. Reverting
    /// `PickerState::refresh_preview`'s dedupe check makes this fail by
    /// name (it would then see two).
    #[test]
    fn two_result_batches_with_the_same_selection_issue_one_preview_request() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "files".to_string(),
            },
        );
        let generation = m.picker_mut().expect("picker must be open").generation();

        let first = update(
            &mut m,
            Msg::PickerResults {
                generation,
                items: vec![
                    crate::native::picker::PickerItem::new("a.rs"),
                    crate::native::picker::PickerItem::new("b.rs"),
                ],
            },
        );
        assert!(
            matches!(
                first.as_slice(),
                [Effect::Rpc(RpcCall::PreviewBuffer { path, .. })] if path.ends_with("a.rs")
            ),
            "the first batch must issue the usual single preview request: {first:?}"
        );

        // a second, streamed batch: still selecting the first row (`a.rs`),
        // just a longer result list -- the selection itself never changed
        let second = update(
            &mut m,
            Msg::PickerResults {
                generation,
                items: vec![
                    crate::native::picker::PickerItem::new("a.rs"),
                    crate::native::picker::PickerItem::new("b.rs"),
                    crate::native::picker::PickerItem::new("c.rs"),
                ],
            },
        );
        assert!(
            second.is_empty(),
            "a second batch that keeps the same selection must issue no new \
             preview request: {second:?}"
        );
    }

    /// The falsifiable check `PickerState::apply_preview`'s own doc names,
    /// driven through `update()`: a reply tagged with a preview generation
    /// this session has since superseded (a newer selection issued its own
    /// preview request before the old one answered) must be dropped, not
    /// merged -- a naive always-apply handler passes every other preview
    /// test and only this one catches it.
    #[test]
    fn a_preview_reply_for_a_stale_generation_is_dropped() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "files".to_string(),
            },
        );
        let generation = m.picker_mut().expect("picker must be open").generation();
        let _ = update(
            &mut m,
            Msg::PickerResults {
                generation,
                items: vec![crate::native::picker::PickerItem::new("a.rs")],
            },
        );
        let stale_generation = m
            .picker_mut()
            .expect("picker must be open")
            .preview_generation();

        // a second result set whose first row now names a *different*
        // candidate (`b.rs`, not `a.rs`) -- the dedupe `refresh_preview`
        // applies (see its own doc) only skips a request for a path
        // already current, so a genuinely new selection still allocates a
        // fresh preview generation, superseding the one just captured
        let _ = update(
            &mut m,
            Msg::PickerResults {
                generation,
                items: vec![crate::native::picker::PickerItem::new("b.rs")],
            },
        );

        let _ = update(
            &mut m,
            Msg::PickerPreviewReply {
                generation: stale_generation,
                path: "/tmp/a.rs".to_string(),
                loaded: true,
                lines: vec!["stale content".to_string()],
            },
        );

        assert!(
            m.picker_mut()
                .expect("picker must still be open")
                .view()
                .preview
                .is_empty(),
            "a reply tagged with a superseded preview generation must never reach \
             the pane"
        );
    }

    /// `loaded: true` is applied straight to the preview pane, with no disk
    /// fallback issued -- the RPC answer already carries nvim's own
    /// authoritative content, modified-but-unsaved or not (see
    /// `docs/picker-preview-wire-capture.md` case 3).
    #[test]
    fn a_loaded_preview_reply_applies_its_lines_and_issues_no_fallback() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "files".to_string(),
            },
        );
        let generation = m.picker_mut().expect("picker must be open").generation();
        let _ = update(
            &mut m,
            Msg::PickerResults {
                generation,
                items: vec![crate::native::picker::PickerItem::new("a.rs")],
            },
        );
        let preview_generation = m
            .picker_mut()
            .expect("picker must be open")
            .preview_generation();

        let effects = update(
            &mut m,
            Msg::PickerPreviewReply {
                generation: preview_generation,
                path: "/tmp/a.rs".to_string(),
                loaded: true,
                lines: vec![
                    "modified line one".to_string(),
                    "modified line two".to_string(),
                ],
            },
        );

        assert!(
            effects.is_empty(),
            "a loaded reply must not also issue a disk-fallback effect: {effects:?}"
        );
        assert_eq!(
            m.picker_mut()
                .expect("picker must still be open")
                .view()
                .preview,
            vec![
                "modified line one".to_string(),
                "modified line two".to_string()
            ]
        );
    }

    /// `loaded: false` means nvim has no buffer open for the candidate; the
    /// only remaining source of truth is disk, handed off to
    /// `Effect::PickerPreviewFallback` rather than treated as "nothing to
    /// preview" (see `update`'s `Msg::PickerPreviewReply` arm doc).
    #[test]
    fn an_unloaded_preview_reply_issues_a_disk_fallback_effect() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "files".to_string(),
            },
        );
        let generation = m.picker_mut().expect("picker must be open").generation();
        let _ = update(
            &mut m,
            Msg::PickerResults {
                generation,
                items: vec![crate::native::picker::PickerItem::new("a.rs")],
            },
        );
        let preview_generation = m
            .picker_mut()
            .expect("picker must be open")
            .preview_generation();

        let effects = update(
            &mut m,
            Msg::PickerPreviewReply {
                generation: preview_generation,
                path: "/tmp/a.rs".to_string(),
                loaded: false,
                lines: Vec::new(),
            },
        );

        assert!(
            matches!(
                effects.as_slice(),
                [Effect::PickerPreviewFallback { generation, path }]
                    if *generation == preview_generation && path == "/tmp/a.rs"
            ),
            "loaded: false must hand off to the disk-fallback effect, echoing the \
             same generation and path: {effects:?}"
        );
    }

    /// `Msg::PickerPreviewFile` is the disk-fallback read's own answer,
    /// gated on the same preview generation as an RPC reply -- a stale
    /// fallback landing after a newer selection's own request is issued
    /// must be dropped on the identical terms.
    #[test]
    fn a_picker_preview_file_reply_applies_disk_fallback_lines() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "files".to_string(),
            },
        );
        let generation = m.picker_mut().expect("picker must be open").generation();
        let _ = update(
            &mut m,
            Msg::PickerResults {
                generation,
                items: vec![crate::native::picker::PickerItem::new("a.rs")],
            },
        );
        let preview_generation = m
            .picker_mut()
            .expect("picker must be open")
            .preview_generation();

        let effects = update(
            &mut m,
            Msg::PickerPreviewFile {
                generation: preview_generation,
                lines: Some(vec!["disk line one".to_string()]),
            },
        );

        assert!(effects.is_empty());
        assert_eq!(
            m.picker_mut()
                .expect("picker must still be open")
                .view()
                .preview,
            vec!["disk line one".to_string()]
        );
    }

    #[test]
    fn a_bare_view_command_is_answered_with_what_it_could_have_asked_for() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: String::new(),
                verb: String::new(),
            },
        );
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::ScheduleToastExpiry { .. }, Effect::Rpc(RpcCall::Input { notation })]
                    if notation == ":View "
            ),
            "a bare :View's usage notice must schedule its own expiry the same way \
             every other locally-synthesized notice does, and reopen the cmdline \
             pre-seeded with the command name so its own completion is one <Tab> \
             away inside the palette: {effects:?}"
        );
        let entry = m
            .engine
            .messages
            .entries
            .last()
            .expect("a bare :View must still reach the user");
        let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
        assert!(
            !text.contains("  "),
            "a bare invocation must not render its two empty tokens as a gap, got {text:?}"
        );
        for form in crate::native::mappings::invocations() {
            assert!(
                text.contains(&form),
                "the usage line must offer {form}, got {text:?}"
            );
        }
        assert!(m.dirty);
    }

    #[test]
    fn a_bare_view_command_reopens_the_cmdline_seeded_with_the_command_name() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: String::new(),
                verb: String::new(),
            },
        );
        assert!(
            effects.iter().any(
                |e| matches!(e, Effect::Rpc(RpcCall::Input { notation }) if notation == ":View ")
            ),
            "a bare :View must reopen the cmdline pre-seeded with its own command \
             name, so :View<Tab> completion is one keystroke away: {effects:?}"
        );
    }

    #[test]
    fn an_unmatched_named_invocation_gets_only_the_notice_not_a_cmdline_reopen() {
        // only the truly bare (feature="", verb="") case is discoverability;
        // a typo'd or not-yet-handled named form (e.g. a stale default map)
        // must not replay itself back into the cmdline, since there is
        // nothing about it a reopened `:View ` prompt would fix
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "nonexistent".to_string(),
            },
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Rpc(RpcCall::Input { .. }))),
            "an unmatched named invocation must not reopen the cmdline: {effects:?}"
        );
    }

    #[test]
    fn a_native_invoke_notice_is_wired_through_the_same_choke_point_as_a_wire_toast() {
        // a native notice must flow through the same classify/expire/history
        // path a wire toast does: it must schedule the identical
        // ScheduleToastExpiry effect and land in ToastHistory that a
        // wire-decoded UiEvent::MsgShow transient toast gets, or an idle
        // editor keeps the notice on screen forever and it's invisible to
        // a future :messages view
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: String::new(),
                verb: String::new(),
            },
        );
        let entry = m
            .engine
            .messages
            .entries
            .last()
            .expect("the invoke notice must reach the message surface");
        assert!(
            matches!(
                &effects[..],
                [Effect::ScheduleToastExpiry { id, after }, Effect::Rpc(RpcCall::Input { .. })]
                    if *id == entry.id() && *after == crate::native::toast::TRANSIENT_TOAST_TIMEOUT
            ),
            "expected a ScheduleToastExpiry for {:?} after {:?}, followed by the bare \
             invocation's cmdline-reopen effect, got {effects:?}",
            entry.id(),
            crate::native::toast::TRANSIENT_TOAST_TIMEOUT
        );
        let recorded = m
            .engine
            .toast_history
            .entries()
            .next()
            .expect("the invoke notice must land in scrollback history, not just on screen");
        assert_eq!(
            recorded.id(),
            entry.id(),
            "history must record the same entry that's on screen, not a stale one"
        );
    }

    #[test]
    fn a_verb_this_build_does_not_answer_is_told_the_ones_it_does() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "nonesuch".to_string(),
            },
        );
        let entry = m.engine.messages.entries.last().expect("a notice");
        let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
        assert!(
            text.contains("picker files"),
            "an unanswerable verb must be told the forms that work, got {text:?}"
        );
    }

    #[test]
    fn claimed_keys_are_recorded_for_the_handover_report() {
        use crate::native::mappings::MappingClaim;
        let mut m = model();
        let claimed = vec![MappingClaim {
            feature: "picker".to_string(),
            lhs: "<leader>ff".to_string(),
            had_user_mapping: true,
        }];
        let effects = update(
            &mut m,
            Msg::MappingsClaimed {
                claimed: claimed.clone(),
            },
        );
        assert!(effects.is_empty(), "{effects:?}");
        assert_eq!(
            m.claimed_keys(),
            claimed.as_slice(),
            "the report has no other source for what the keys took"
        );
    }

    #[test]
    fn tree_toggle_opens_a_sidebar_and_issues_both_the_scan_and_the_git_scan() {
        let mut m = model();
        let effects = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "tree".to_string(),
                verb: "toggle".to_string(),
            },
        );
        assert!(
            matches!(
                m.overlays().last().map(|o| &o.kind),
                Some(OverlayKind::Tree(_))
            ),
            "toggling tree must open a Tree overlay: {:?}",
            m.overlays()
        );
        let (mut saw_scan, mut saw_git_scan) = (false, false);
        for effect in &effects {
            match effect {
                Effect::TreeScan { .. } => saw_scan = true,
                Effect::TreeGitScan { .. } => saw_git_scan = true,
                _ => {}
            }
        }
        assert!(
            saw_scan,
            "opening a tree must issue its filesystem scan: {effects:?}"
        );
        assert!(
            saw_git_scan,
            "opening a tree must issue its git-status refresh too: {effects:?}"
        );
    }

    #[test]
    fn tree_toggle_again_closes_the_sidebar_and_cancels_its_scan_worker() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "tree".to_string(),
                verb: "toggle".to_string(),
            },
        );
        let effects = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "tree".to_string(),
                verb: "toggle".to_string(),
            },
        );
        assert!(
            m.overlays().is_empty(),
            "a second toggle must close the tree it just opened: {:?}",
            m.overlays()
        );
        // `Effect::TreeClose` flips whatever scan the executor still has
        // running (see `Executor::tree_scan_cancel`'s own doc): closing
        // issues exactly this one effect, not none, so a huge tree closed
        // mid-scan does not leave its worker thread walking unobserved.
        assert!(
            matches!(effects.as_slice(), [Effect::TreeClose]),
            "closing must cancel the scan worker via Effect::TreeClose: {effects:?}"
        );
    }

    #[test]
    fn tree_toggle_while_a_blocked_prompt_is_topmost_opens_beneath_it_without_stealing_focus() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "confirm".into(),
                content: vec![(0, "test".into())],
                replace_last: false,
            }]),
        );
        let prompt_id = match m.focus() {
            Focus::Native(id) => id,
            Focus::Engine => unreachable!("MsgShow must open a Prompt overlay"),
        };
        assert!(matches!(
            m.overlays().last().map(|o| &o.kind),
            Some(OverlayKind::Prompt(_))
        ));

        let effects = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "tree".to_string(),
                verb: "toggle".to_string(),
            },
        );
        let (mut saw_scan, mut saw_git_scan) = (false, false);
        for effect in &effects {
            match effect {
                Effect::TreeScan { .. } => saw_scan = true,
                Effect::TreeGitScan { .. } => saw_git_scan = true,
                _ => {}
            }
        }
        assert!(
            saw_scan && saw_git_scan,
            "opening beneath the prompt must still issue both scans: {effects:?}"
        );
        assert_eq!(
            m.focus(),
            Focus::Native(prompt_id),
            "a tree opening while a blocked prompt is topmost must not \
             steal its focus"
        );
        assert_eq!(
            m.overlays().len(),
            2,
            "the tree must open beneath the prompt, not replace it"
        );
        assert!(
            matches!(m.overlays()[0].kind, OverlayKind::Tree(_)),
            "the tree must sit beneath the prompt on the stack: {:?}",
            m.overlays()
        );
        assert!(
            matches!(m.overlays()[1].kind, OverlayKind::Prompt(_)),
            "the prompt must remain topmost: {:?}",
            m.overlays()
        );
        assert!(
            m.tree_mut().is_some(),
            "the tree must still be reachable so a streamed scan reply can \
             reach it even while the prompt holds focus"
        );
    }

    #[test]
    fn close_tree_releases_its_own_mouse_capture_but_not_a_different_overlays() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "tree".to_string(),
                verb: "toggle".to_string(),
            },
        );
        let tree_id = match m.overlays().last() {
            Some(overlay) if matches!(overlay.kind, OverlayKind::Tree(_)) => overlay.id,
            other => unreachable!("toggle must have opened a Tree overlay: {other:?}"),
        };
        m.capture_mouse(MouseCapture::Overlay(tree_id));
        assert_eq!(m.mouse_capture(), Some(MouseCapture::Overlay(tree_id)));

        assert!(m.close_tree(), "the open tree must be found and closed");
        assert_eq!(
            m.mouse_capture(),
            None,
            "closing the tree that owns the in-flight gesture must release \
             it, or a drag whose target just disappeared would keep \
             routing to an overlay id nothing on the stack holds anymore"
        );

        // a capture belonging to some other, unrelated overlay id must
        // survive a tree close that never held it
        let unrelated_id = OverlayId(9999);
        m.capture_mouse(MouseCapture::Overlay(unrelated_id));
        assert!(
            !m.close_tree(),
            "no tree is open at this point, so this must report nothing \
             was found to close"
        );
        assert_eq!(
            m.mouse_capture(),
            Some(MouseCapture::Overlay(unrelated_id)),
            "close_tree must never release a capture it does not own"
        );
    }

    /// The falsifiable check for the bridge write/focus wiring this
    /// module's `tree_git_refresh_effect` exists to satisfy: with a tree
    /// open, a `BufferChanged` (a write callback) must reissue
    /// `Effect::TreeGitScan` against a fresh generation, not merely the
    /// once-at-open refresh `toggle_tree_sidebar` already issues.
    #[test]
    fn a_buffer_write_callback_with_a_tree_open_reissues_the_git_scan() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "tree".to_string(),
                verb: "toggle".to_string(),
            },
        );
        let opened_at = m
            .tree_mut()
            .expect("tree must be open after toggling it")
            .git_generation();

        let effects = update(
            &mut m,
            Msg::BufferChanged {
                name: "a.txt".to_string(),
                modified: false,
            },
        );
        let generation = match effects.as_slice() {
            [Effect::TreeGitScan { generation, .. }] => *generation,
            other => panic!(
                "a buffer-write callback with a tree open must reissue exactly                  one TreeGitScan, got {other:?}"
            ),
        };
        assert_ne!(
            generation, opened_at,
            "the reissued refresh must carry a fresh generation, not the              one the tree opened with"
        );
        assert_eq!(
            m.tree_mut().expect("tree still open").git_generation(),
            generation,
            "TreeState's own git_generation must track the effect it just issued"
        );
    }

    /// Same proof as the write-callback test above, for the focus-side
    /// trigger (`GitBranchChanged`, fired on `BufEnter`/`DirChanged`/
    /// `FocusGained`).
    #[test]
    fn a_focus_callback_with_a_tree_open_reissues_the_git_scan() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "tree".to_string(),
                verb: "toggle".to_string(),
            },
        );
        let effects = update(
            &mut m,
            Msg::GitBranchChanged {
                branch: "main".to_string(),
            },
        );
        assert!(
            matches!(effects.as_slice(), [Effect::TreeGitScan { .. }]),
            "a focus callback with a tree open must reissue exactly one              TreeGitScan: {effects:?}"
        );
    }

    /// The other half of the falsifiable check: with no tree open at all,
    /// the same callbacks that must trigger a refresh while one is open
    /// must issue no tree effect whatsoever -- every buffer write and
    /// focus change in an ordinary editing session hits these arms, so a
    /// no-tree no-op is the common case, not a corner one.
    #[test]
    fn bridge_callbacks_with_no_tree_open_issue_no_tree_effect() {
        let mut m = model();
        let write_effects = update(
            &mut m,
            Msg::BufferChanged {
                name: "a.txt".to_string(),
                modified: true,
            },
        );
        assert!(
            !write_effects
                .iter()
                .any(|e| matches!(e, Effect::TreeGitScan { .. })),
            "no tree is open, so a write callback must issue no tree              effect: {write_effects:?}"
        );
        let focus_effects = update(
            &mut m,
            Msg::GitBranchChanged {
                branch: "main".to_string(),
            },
        );
        assert!(
            !focus_effects
                .iter()
                .any(|e| matches!(e, Effect::TreeGitScan { .. })),
            "no tree is open, so a focus callback must issue no tree              effect: {focus_effects:?}"
        );
    }

    #[test]
    fn notifications_history_invoke_opens_a_message_history_overlay_and_marks_dirty() {
        let mut m = model();
        m.dirty = false;
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "notifications".to_string(),
                verb: "history".to_string(),
            },
        );
        assert!(
            matches!(
                m.overlays().last().map(|o| &o.kind),
                Some(OverlayKind::MessageHistory(_))
            ),
            "invoking notifications/history must open a MessageHistory overlay: {:?}",
            m.overlays()
        );
        assert!(
            m.dirty,
            "opening the message history must mark the model dirty for a repaint"
        );
    }

    /// The generic `Focus::Native` `<Esc>` fallback pops whatever overlay is
    /// on top without a per-kind arm of its own -- MessageHistory is the
    /// first real inhabitant of that path. A pop that does not mark
    /// `dirty` leaves the paint loop's `if model.dirty` gate (see
    /// `view`'s runtime loop) never repainting the frame the overlay just
    /// vacated, so the stale frame stays on screen until some unrelated
    /// event happens to set `dirty` again.
    #[test]
    fn esc_through_the_generic_native_fallback_closes_message_history_and_marks_dirty() {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::FeatureInvoke {
                feature: "notifications".to_string(),
                verb: "history".to_string(),
            },
        );
        assert!(matches!(
            m.overlays().last().map(|o| &o.kind),
            Some(OverlayKind::MessageHistory(_))
        ));
        m.dirty = false;

        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: "<Esc>".into(),
            }),
        );
        assert!(
            m.overlays().is_empty(),
            "<Esc> through the generic fallback must close the MessageHistory overlay: {:?}",
            m.overlays()
        );
        assert!(
            m.dirty,
            "closing the overlay must mark the model dirty for a repaint"
        );
    }
}
