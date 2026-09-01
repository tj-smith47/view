//! Out-of-band write handling: the fs watcher's own detections, folded into
//! `RpcCall::Checktime` and the conflict prompt its reply raises.
//!
//! Two hops, mirroring `ai_fs.rs`'s own shape: a batch of detections becomes
//! one `RpcCall::Checktime` (`Msg::ExternalWritesDetected` here), and each
//! entry of the reply raises nothing, the conflict prompt, or a notice --
//! see `docs/checktime-wire-capture.md` for the outcomes this reads and
//! `CheckTimeOutcome` for why they are a closed vocabulary rather than
//! flags.
//!
//! One outcome takes a third hop. A path answering
//! `CheckTimeOutcome::FileGone` for the first time says nothing at all and
//! schedules `Effect::ReprobeExternalWrite` instead, which comes back as
//! `Msg::ConfirmExternalRemoval` for that one path a grace period later;
//! only the reply that second probe provokes may announce anything. That is
//! what keeps a save that unlinks its target before rewriting it --
//! nominated, legitimately, while the path is absent -- from flashing a
//! vanished file at the user over an ordinary write.
//!
//! A gone episode therefore has exactly three ends, and the state carrying
//! it is gone after any of them: the confirming reply arrives (announced or
//! not), some other answer for the path contradicts it, or a fresh
//! nomination of the same path starts the episode over. Nothing outlives
//! the episode that could let a later first answer pass for a confirmed
//! one -- what stops a path already announced from being probed again and
//! again is the notice standing on screen for it, which retires itself when
//! it expires or is withdrawn.

use std::path::PathBuf;

use crate::model::{Model, OverlayKind};
use crate::msg::{CheckTimeOutcome, Effect, RpcCall};
use crate::native::prompt::PromptState;

/// Starts the round trip for one batch of detections: mints a fresh
/// `request_id` and drives `RpcCall::Checktime` with `force: false`, the
/// watcher's own probe. Never inspects any path's content and never decides
/// silent-reload vs. conflict itself -- nvim's reply does that (see
/// `RpcCall::Checktime`'s own doc).
///
/// One call for the whole batch, never one per path: the cost this fold is
/// spending belongs to nvim's single-threaded main loop, and the chunk
/// resolves its buffer set once per call however many paths the call names.
pub(super) fn on_external_writes_detected(model: &mut Model, paths: Vec<PathBuf>) -> Vec<Effect> {
    if paths.is_empty() {
        return Vec::new();
    }
    let request_id = model.next_checktime_request_id();
    vec![Effect::Rpc(RpcCall::Checktime {
        request_id,
        paths: paths.iter().map(|p| super::path_to_wire(p)).collect(),
        force: false,
    })]
}

/// Starts the confirming second look at one path, a grace period after its
/// first `CheckTimeOutcome::FileGone` answer (see
/// `Effect::ReprobeExternalWrite`). Mints the `request_id` and records that
/// this probe's reply -- and no other -- may announce the removal.
///
/// One path, not a batch: this is the answer to a specific question about a
/// specific file, and a probe that carried other paths along would let one
/// of them be confirmed by a reply nobody asked for on its behalf.
pub(super) fn on_confirm_external_removal(model: &mut Model, path: PathBuf) -> Vec<Effect> {
    let request_id = model.next_checktime_request_id();
    model.expect_file_gone_confirmation(request_id, &path);
    vec![Effect::Rpc(RpcCall::Checktime {
        request_id,
        paths: vec![super::path_to_wire(&path)],
        force: false,
    })]
}

/// Folds one `Msg::CheckTimeReply`: every entry is dispositioned on its own,
/// since one call answers many paths and each can have landed differently.
///
/// [`CheckTimeOutcome::Conflict`] is the only outcome that opens the prompt.
/// A forced reload's own reply can never be one -- that is the whole reason
/// the outcome is a closed enum rather than the wire's `found`/`fired` pair,
/// which a forced reload answers identically to a fresh conflict and which
/// would therefore re-raise, indefinitely, the prompt the user just answered
/// with "Reload".
pub(super) fn on_checktime_reply(
    model: &mut Model,
    request_id: u64,
    results: Vec<(PathBuf, CheckTimeOutcome)>,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    for (path, outcome) in results {
        // any answer other than `FileGone` ends whatever gone episode the
        // path was in, both halves of it. `Conflict`, `HandledSilently`,
        // `Reloaded` and `ReloadFailed` are all answered below the chunk's
        // own stat, so each of them read the path as a file; `NoBuffer`
        // proves nothing about the file -- the chunk answers it above the
        // stat -- but it retires the notice all the same, whose second
        // clause describes a buffer that no longer exists to hold anything.
        //
        // So a notice asserting the path cannot be read has nothing true
        // left to say, and a confirming probe still in flight is asking a
        // question that has been answered: neither may survive into the
        // next episode, where the notice would suppress the second look the
        // next first answer owes and the probe's late reply would pass for
        // that second look itself
        if !matches!(outcome, CheckTimeOutcome::FileGone { .. }) {
            model.forget_file_gone_confirmation(&path);
            if model
                .engine
                .withdraw_native_notice(&file_notice_family(&path))
            {
                model.dirty = true;
            }
        }
        match outcome {
            CheckTimeOutcome::NoBuffer
            | CheckTimeOutcome::HandledSilently
            | CheckTimeOutcome::Reloaded => {}
            CheckTimeOutcome::Conflict => open_conflict_prompt(model, path),
            // the answer the user gave was destructive (discard the local
            // edits), so a reload that raised must say so rather than pass
            // for a completed discard. What it must NOT do is promise which
            // side survived: `:edit!` clears the buffer before it reads, so
            // every failure shape captured leaves either the external
            // content or an empty buffer, never the local edit
            // (`docs/checktime-wire-capture.md` case 7a)
            CheckTimeOutcome::ReloadFailed => {
                effects.extend(model.engine.record_native_notice(
                    format!(
                        "reloading {} did not finish -- check the buffer before saving over it",
                        path.display()
                    ),
                    false,
                ));
                model.dirty = true;
            }
            // nothing was read and nothing was lost: the chunk stats before
            // it reads precisely so this case reaches neither `:checktime`
            // nor `:edit!`, either of which against a pipe would never have
            // returned at all, and the latter of which against a missing
            // path would have succeeded and emptied the buffer.
            //
            // Two sentences because only one of them is true at a time. A
            // modified buffer is holding edits that now exist nowhere else,
            // which is the thing worth saying; an unmodified one is holding
            // what it last read off a path that no longer answers, and
            // claiming otherwise would promise the user edits they never
            // made
            CheckTimeOutcome::FileGone { modified } => {
                // one answer is not evidence yet. An unlink-then-rewrite
                // save (a generator clearing its output before writing it
                // again) is nominated while the path is genuinely absent,
                // so announcing the first answer would flash a vanished
                // file over an ordinary save and retract it a moment
                // later -- and a notice that is wrong for 50ms is still a
                // notice the user's eye caught. Only the reply the
                // confirming probe itself provoked settles it
                // (`Effect::ReprobeExternalWrite`), so a stray answer for a
                // path nobody is currently confirming cannot pass for one.
                //
                // A notice already standing for the path counts as settled
                // too, and has to: the episode it belongs to said its piece
                // already, so a watch that keeps nominating an absent path
                // once per window would otherwise buy a fresh probe, and a
                // fresh sleeping thread, for every window it stays gone
                let confirmed = model.take_file_gone_confirmation(request_id, &path)
                    || model.engine.has_native_notice(&file_notice_family(&path));
                if !confirmed {
                    effects.push(Effect::ReprobeExternalWrite { path });
                    continue;
                }
                // a conflict prompt opened by an earlier detection is still
                // offering "Reload and discard the local edits" against a
                // path that can no longer be read. Answering it would cost
                // the user their edits in exchange for nothing, so the
                // question is withdrawn and the notice below stands in its
                // place -- a prompt is worth keeping open only while one of
                // its answers is still true.
                //
                // Withdrawing is involuntary, which every other overlay
                // close is not: a key aimed at the prompt and pressed after
                // the withdrawal lands finds no overlay focused and reaches
                // nvim instead, where the prompt's own `r` is a normal-mode
                // character replace. The window is one paint wide and the
                // same one the cmdline-quiet dismissal already carries.
                //
                // The withdrawal waits for the confirmation the notice
                // waits for, so the stale question stays answerable for a
                // grace plus one round trip: the alternative is withdrawing
                // it on an answer taken mid-save, which would take a live
                // conflict off the screen for a file that never went
                // anywhere
                let withdrew = model.close_external_write_conflict_prompt(&path);
                let fate = if modified {
                    "and your buffer still holds your edits"
                } else {
                    "and the buffer still holds the content it last read"
                };
                let family = file_notice_family(&path);
                let raised = model.engine.record_native_notice_once(
                    &family,
                    format!(
                        "{family}no longer a readable file on disk -- nothing was \
                         reloaded, {fate}"
                    ),
                );
                // a native notice always routes transient, so recording one
                // always owes an expiry effect: an empty answer here is the
                // dedupe declining to say the same thing twice, and a
                // repaint for a screen nothing changed on would be spent
                // once per detection window for as long as the path stays
                // unreadable.
                //
                // Both, not either: a question leaving the screen and a line
                // arriving on it are each a change the user has to be shown,
                // and the paint each one owes belongs to it rather than to
                // the other happening to be there beside it
                let spoke = !raised.is_empty();
                effects.extend(raised);
                model.dirty |= withdrew || spoke;
            }
        }
    }
    effects
}

/// The family of every native notice about one path, which is what both the
/// notice and its later withdrawal are keyed on: a notice raised for a
/// modified buffer is still retracted by an answer that finds the path
/// readable again after the edits were saved elsewhere.
///
/// `view: `, the noun before the path and the word after it are not
/// decoration. Every native notice family shares the opening so that no two
/// of them can prefix each other -- withdrawal is `starts_with`, so a family
/// that is a prefix of another retracts the other's line -- and a bare path
/// opening the line would place a family under the user's control. The
/// trailing `is ` is what separates two paths one of which starts with the
/// other: without it the family for `/a` is a prefix of the family for
/// `/a b`, and taking the first notice down takes the second with it.
pub(super) fn file_notice_family(path: &std::path::Path) -> String {
    format!("view: file {} is ", path.display())
}

/// Folds one `Msg::ExternalWatchDegraded` into a notice the user actually
/// sees. Through `record_native_notice` rather than the AI panel's own
/// `local_error`, on purpose: `local_error` is cleared the moment
/// `AiEvent::SessionReady` arrives, and a watch that fails to register does
/// so *before* the agent finishes its handshake -- so the panel banner would
/// be wiped by the very event that means the session is working.
pub(super) fn on_external_watch_degraded(model: &mut Model, reason: String) -> Vec<Effect> {
    model.dirty = true;
    model.engine.record_native_notice(
        format!("out-of-band write detection is degraded: {reason}"),
        false,
    )
}

/// Opens (or, for a second conflict on the same still-open path, replaces
/// in place) the confirm-class conflict prompt, on the identical
/// dedupe-by-lookup and focus-stacking terms `update/mod.rs`'s
/// `open_ai_trust_prompt` uses for its own locally-resolving prompt: a
/// blocked-engine `Prompt` may already have taken focus above this one, and
/// inserting beneath it (rather than displacing it) never steals an answer
/// nvim is still waiting on.
fn open_conflict_prompt(model: &mut Model, path: PathBuf) {
    let message = format!(
        "{} changed outside view while the buffer had local edits. Reload \
         and discard the local edits, or keep them and ignore the external \
         change?",
        path.display()
    );
    let state = PromptState::external_write_conflict_prompt(path.clone(), message);
    if let Some(p) = model.external_write_conflict_prompt_mut(&path) {
        *p = state;
        model.resize_prompt_overlays();
        model.dirty = true;
        return;
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::msg::Msg;
    use crate::update::update;

    /// Collapses every run of whitespace to one space, so a sentence the
    /// docs wrap across lines still compares equal to the one line of Rust
    /// that produces it.
    fn flatten(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn external_writes(paths: &[&str]) -> Msg {
        Msg::ExternalWritesDetected {
            paths: paths.iter().map(PathBuf::from).collect(),
        }
    }

    fn checktime_reply(request_id: u64, results: &[(&str, CheckTimeOutcome)]) -> Msg {
        Msg::CheckTimeReply {
            request_id,
            results: results
                .iter()
                .map(|(p, o)| (PathBuf::from(*p), *o))
                .collect(),
        }
    }

    /// The whole announcing sequence for an unreadable path, shell hop
    /// included: a first answer that only asks for the confirming re-probe,
    /// the grace's own re-nomination, and the reply to the probe that
    /// re-nomination drove -- the one answer allowed to speak. Hands back
    /// that last reply's own effects.
    ///
    /// `id` is the first answer's; the confirming probe mints its own, which
    /// is exactly why it can be told apart from any other answer for the
    /// same path.
    fn confirm_gone(model: &mut Model, id: u64, path: &str, modified: bool) -> Vec<Effect> {
        let gone = CheckTimeOutcome::FileGone { modified };
        let asked = update(model, checktime_reply(id, &[(path, gone)]));
        assert!(
            matches!(asked.as_slice(), [Effect::ReprobeExternalWrite { .. }]),
            "the first answer confirms rather than speaks, got {asked:?}"
        );
        let confirming = reprobe(model, path);
        update(model, checktime_reply(confirming, &[(path, gone)]))
    }

    /// Performs the shell's half of the confirmation -- the grace elapsing
    /// and the second look being asked for -- and hands back the
    /// `request_id` of the probe it drove, since only the reply wearing that
    /// id may confirm anything.
    fn reprobe(model: &mut Model, path: &str) -> u64 {
        let effects = update(
            model,
            Msg::ConfirmExternalRemoval {
                path: PathBuf::from(path),
            },
        );
        match effects.as_slice() {
            [Effect::Rpc(RpcCall::Checktime {
                request_id,
                paths,
                force: false,
            })] => {
                assert_eq!(paths, &vec![path.to_string()], "the probe names its path");
                *request_id
            }
            other => panic!("expected one unforced probe of the path, got {other:?}"),
        }
    }

    /// The watcher's own detection always drives one `RpcCall::Checktime`
    /// with `force: false` -- the falsifiable half of "the watcher never
    /// decides itself, it only ever drives nvim's own check."
    #[test]
    fn an_external_write_drives_one_unforced_checktime() {
        let mut model = Model::new();
        let effects = update(&mut model, external_writes(&["/proj/src/lib.rs"]));
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::Rpc(RpcCall::Checktime {
                paths,
                force,
                request_id,
            }) => {
                assert_eq!(paths, &vec!["/proj/src/lib.rs".to_string()]);
                assert!(!force);
                assert!(*request_id > 0);
            }
            other => panic!("expected Effect::Rpc(RpcCall::Checktime), got {other:?}"),
        }
    }

    /// A batch of detections costs ONE call, not one per path: the cost
    /// being bounded here is nvim's own main loop, which re-resolves every
    /// loaded buffer per call. A regression that folded each path into its
    /// own `RpcCall` would multiply that cost by the size of whatever an
    /// agent's shell tool just rewrote.
    #[test]
    fn a_batch_of_detections_drives_exactly_one_call() {
        let mut model = Model::new();
        let effects = update(
            &mut model,
            external_writes(&["/proj/a.rs", "/proj/b.rs", "/proj/c.rs"]),
        );
        assert_eq!(effects.len(), 1, "one batch must cost one probe");
        match &effects[0] {
            Effect::Rpc(RpcCall::Checktime { paths, .. }) => assert_eq!(paths.len(), 3),
            other => panic!("expected one batched Checktime, got {other:?}"),
        }
    }

    /// An empty batch never reaches the wire at all.
    #[test]
    fn an_empty_batch_drives_nothing() {
        let mut model = Model::new();
        assert!(update(&mut model, external_writes(&[])).is_empty());
    }

    /// `NoBuffer` -- no buffer for the path -- raises nothing: no overlay,
    /// no effect. This is case 1 of the falsifiable check: "an external
    /// write to a file with no loaded buffer produces no UI."
    #[test]
    fn no_buffer_found_raises_no_ui() {
        let mut model = Model::new();
        let effects = update(
            &mut model,
            checktime_reply(1, &[("/proj/gone.rs", CheckTimeOutcome::NoBuffer)]),
        );
        assert!(effects.is_empty());
        assert!(!model.ai_panel_overlay_open());
        assert_eq!(model.overlays().len(), 0);
    }

    /// `HandledSilently` -- nvim reloaded an unmodified buffer, or the
    /// write was view's own -- also raises nothing: this is the
    /// mutation-killing case that distinguishes the required outcome gate
    /// from a weaker "the buffer's file changed" one, which would wrongly
    /// pop a prompt for every self-write.
    #[test]
    fn a_silently_handled_change_raises_no_ui() {
        let mut model = Model::new();
        let effects = update(
            &mut model,
            checktime_reply(
                1,
                &[("/proj/src/lib.rs", CheckTimeOutcome::HandledSilently)],
            ),
        );
        assert!(effects.is_empty());
        assert_eq!(model.overlays().len(), 0);
    }

    /// `Conflict` -- the genuine conflict -- opens the confirm-class
    /// prompt, and only that outcome does: this is the mutation-killing
    /// positive alongside the negatives around it.
    #[test]
    fn a_conflict_opens_the_conflict_prompt() {
        let mut model = Model::new();
        let effects = update(
            &mut model,
            checktime_reply(1, &[("/proj/src/lib.rs", CheckTimeOutcome::Conflict)]),
        );
        assert!(effects.is_empty());
        assert_eq!(model.overlays().len(), 1);
        let prompt = model
            .external_write_conflict_prompt_mut(std::path::Path::new("/proj/src/lib.rs"))
            .expect("the conflict prompt must be open for this exact path");
        assert!(prompt.accepts("<CR>"));
    }

    /// Answering "Reload" ends the conflict, once. The forced reload the
    /// answer issues comes back as its own reply, and that reply must not
    /// re-open the prompt the user just closed -- otherwise "Reload"
    /// reloads the file and immediately asks the same question again about
    /// local edits the buffer no longer has, and the only answer that ever
    /// escapes is the one the user rejected.
    #[test]
    fn answering_reload_does_not_reopen_the_prompt() {
        let mut model = Model::new();
        let _ = update(
            &mut model,
            checktime_reply(1, &[("/proj/src/lib.rs", CheckTimeOutcome::Conflict)]),
        );
        assert_eq!(model.overlays().len(), 1);

        let effects = update(
            &mut model,
            Msg::Key(crate::msg::Key {
                notation: "<CR>".to_string(),
            }),
        );
        let forced = effects
            .iter()
            .find_map(|e| match e {
                Effect::Rpc(RpcCall::Checktime {
                    request_id,
                    paths,
                    force: true,
                }) => Some((*request_id, paths.clone())),
                _ => None,
            })
            .expect("answering Reload must issue the forced reload");
        assert_eq!(forced.1, vec!["/proj/src/lib.rs".to_string()]);
        assert_eq!(model.overlays().len(), 0, "the answer closes the prompt");

        let effects = update(
            &mut model,
            checktime_reply(
                forced.0,
                &[("/proj/src/lib.rs", CheckTimeOutcome::Reloaded)],
            ),
        );
        assert!(effects.is_empty());
        assert_eq!(
            model.overlays().len(),
            0,
            "the forced reload's own reply must not re-raise the conflict it answered"
        );
    }

    /// A forced reload that raised is reported: the user asked for their
    /// local edits to be discarded, that did not happen, and silence would
    /// leave them believing it did.
    #[test]
    fn a_failed_forced_reload_is_reported() {
        let mut model = Model::new();
        let before = model.engine.messages.entries.len();
        let effects = update(
            &mut model,
            checktime_reply(1, &[("/proj/src/lib.rs", CheckTimeOutcome::ReloadFailed)]),
        );
        assert!(
            model.engine.messages.entries.len() > before,
            "a failed reload must leave the user a notice, got effects {effects:?}"
        );
        assert_eq!(model.overlays().len(), 0);
    }

    /// A degraded watch says so. Through the notice path, so it survives
    /// the `SessionReady` that clears the AI panel's own crash banner.
    /// The path is not a readable file: nothing was read, so the user is
    /// owed a notice rather than `Reloaded`'s silence, and no prompt -- the
    /// question the prompt would ask has no answer left to offer.
    ///
    /// Both sentences, because only one of them is ever true and picking the
    /// wrong one is a lie about the user's own data: an unmodified buffer
    /// holds no edits to still be holding. Each is asserted against
    /// `docs/ai.md`, which quotes both verbatim -- a literal here alone
    /// would let an editor update the two sides together and leave the page
    /// promising something view no longer says.
    #[test]
    fn a_vanished_file_is_reported_by_what_the_buffer_is_holding() {
        let ai_md = flatten(include_str!("../../../../docs/ai.md"));
        for (modified, tail) in [
            (true, "and your buffer still holds your edits"),
            (false, "and the buffer still holds the content it last read"),
        ] {
            let mut model = Model::new();
            let before = model.engine.messages.entries.len();
            let effects = confirm_gone(&mut model, 2, "/proj/src/lib.rs", modified);
            assert!(
                model.engine.messages.entries.len() > before,
                "a vanished file must leave the user a notice, got effects {effects:?}"
            );
            let entry = model.engine.messages.entries.last().expect("a notice");
            let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
            assert_eq!(
                text,
                format!(
                    "view: file /proj/src/lib.rs is no longer a readable file \
                     on disk -- nothing was reloaded, {tail}"
                ),
                "the notice docs/ai.md quotes for modified={modified} must say exactly this"
            );
            let quoted = text.replace("/proj/src/lib.rs", "/home/you/project/src/lib.rs");
            assert!(
                ai_md.contains(&flatten(&quoted)),
                "docs/ai.md quotes this notice verbatim, and no longer contains: {quoted}"
            );
            assert_eq!(model.overlays().len(), 0);
        }
    }

    /// A path that keeps answering `FileGone` -- a pipe being written to
    /// raises a detection per coalesce window, every one of them unreadable
    /// -- says it once. The prompt this arm replaced was idempotent by
    /// construction (`open_conflict_prompt` assigns in place), and a notice
    /// that stacks a byte-identical copy per window would be a regression
    /// against it: the pile buries whatever stood above it and evicts real
    /// history out of the ring behind `:messages`.
    ///
    /// A *different* path's own notice still lands, which is what keeps
    /// this from silencing a second file that vanished right after the
    /// first.
    #[test]
    fn one_path_answering_gone_over_and_over_leaves_one_notice() {
        let mut model = Model::new();
        let before = model.engine.messages.entries.len();
        let _ = confirm_gone(&mut model, 1, "/proj/src/lib.rs", false);
        for id in 3..=4 {
            let _ = update(
                &mut model,
                checktime_reply(
                    id,
                    &[(
                        "/proj/src/lib.rs",
                        CheckTimeOutcome::FileGone { modified: false },
                    )],
                ),
            );
        }
        assert_eq!(
            model.engine.messages.entries.len(),
            before + 1,
            "four detections of one unreadable path are one thing to say: {:?}",
            model.engine.messages.entries
        );

        let _ = confirm_gone(&mut model, 5, "/proj/src/other.rs", false);
        assert_eq!(
            model.engine.messages.entries.len(),
            before + 2,
            "a second file that vanished is a second thing to say: {:?}",
            model.engine.messages.entries
        );
    }

    /// Anything at all landing between two detections of the same
    /// unreadable path must not restore the pile the collapse exists to
    /// stop. An ordinary nvim message is the normal case, not the exotic
    /// one: the user is typing and nvim is talking while a pipe is being
    /// written to, so a collapse that only looked at the tail would hold
    /// only for as long as nothing else spoke.
    #[test]
    fn a_message_arriving_between_two_detections_does_not_restack_the_notice() {
        let mut model = Model::new();
        let before = model.engine.messages.entries.len();
        let gone = || {
            checktime_reply(
                1,
                &[(
                    "/proj/src/lib.rs",
                    CheckTimeOutcome::FileGone { modified: false },
                )],
            )
        };
        let _ = confirm_gone(&mut model, 1, "/proj/src/lib.rs", false);
        let _ = model
            .engine
            .messages
            .resolve_startup_hold(crate::native::toast::HoldOutcome::Release);
        let _ = update(
            &mut model,
            Msg::Redraw(vec![crate::events::UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "written 1 line".into())],
                replace_last: false,
            }]),
        );
        let _ = update(&mut model, gone());
        assert_eq!(
            model.engine.messages.entries.len(),
            before + 2,
            "the notice and the message nvim showed, not a second copy of the notice: {:?}",
            model.engine.messages.entries
        );
    }

    /// Two unreadable paths alternating (`A`, `B`, `A`) is the other
    /// interleaving, and the answer is the same: `A` is still standing, so
    /// it is not said again.
    #[test]
    fn a_second_path_between_two_detections_does_not_restack_the_first() {
        let mut model = Model::new();
        let before = model.engine.messages.entries.len();
        let _ = confirm_gone(&mut model, 1, "/proj/src/lib.rs", false);
        let _ = confirm_gone(&mut model, 2, "/proj/src/other.rs", false);
        for (id, path) in [
            (5, "/proj/src/lib.rs"),
            (6, "/proj/src/other.rs"),
            (7, "/proj/src/lib.rs"),
        ] {
            let _ = update(
                &mut model,
                checktime_reply(
                    id,
                    &[(path, CheckTimeOutcome::FileGone { modified: false })],
                ),
            );
        }
        assert_eq!(
            model.engine.messages.entries.len(),
            before + 2,
            "two unreadable paths are two things to say, however they interleave: {:?}",
            model.engine.messages.entries
        );
    }

    /// The first answer of `FileGone` for a path says nothing and asks for
    /// a second stat instead. An unlink-then-rewrite save is nominated
    /// while the path is genuinely absent, so this answer is as true and as
    /// meaningless as the same answer during a `rm`; what tells the two
    /// apart is whether the path is still unreadable a moment later.
    #[test]
    fn a_first_answer_of_gone_asks_for_a_re_probe_instead_of_speaking() {
        let mut model = Model::new();
        let before = model.engine.messages.entries.len();
        let effects = update(
            &mut model,
            checktime_reply(
                1,
                &[(
                    "/proj/src/lib.rs",
                    CheckTimeOutcome::FileGone { modified: true },
                )],
            ),
        );
        match effects.as_slice() {
            [Effect::ReprobeExternalWrite { path }] => {
                assert_eq!(path, std::path::Path::new("/proj/src/lib.rs"));
            }
            other => panic!("expected one re-probe of the path, got {other:?}"),
        }
        assert_eq!(
            model.engine.messages.entries.len(),
            before,
            "nothing is said until the re-probe answers: {:?}",
            model.engine.messages.entries
        );
        assert!(
            !model.dirty,
            "an answer that changed nothing on screen must not cost a paint"
        );
    }

    /// The save this whole confirmation exists for: the path is back by the
    /// time the re-probe stats it, and the user is never told anything at
    /// all -- not a notice, and not a notice retracted a moment later,
    /// which is still a notice their eye caught.
    #[test]
    fn a_path_readable_by_the_re_probe_is_never_announced_at_all() {
        let mut model = Model::new();
        let before = model.engine.messages.entries.len();
        let _ = update(
            &mut model,
            checktime_reply(
                1,
                &[(
                    "/proj/src/lib.rs",
                    CheckTimeOutcome::FileGone { modified: false },
                )],
            ),
        );
        let effects = update(
            &mut model,
            checktime_reply(
                2,
                &[("/proj/src/lib.rs", CheckTimeOutcome::HandledSilently)],
            ),
        );
        assert!(effects.is_empty(), "an ordinary save owes nothing");
        assert_eq!(
            model.engine.messages.entries.len(),
            before,
            "a save that put the file back said nothing to retract: {:?}",
            model.engine.messages.entries
        );
        assert!(!model.dirty, "nothing reached the screen to be repainted");
    }

    /// A path that answers something else has its gone record dropped, so
    /// the next unreadable answer it gives is confirmed from the start
    /// again. Every answer other than `FileGone` counts, whichever arm
    /// below handles it: each one either read the path as a file or found
    /// no buffer left for the notice to describe.
    #[test]
    fn an_answer_that_is_not_gone_makes_the_next_one_start_over() {
        for answered in [
            CheckTimeOutcome::HandledSilently,
            CheckTimeOutcome::Reloaded,
            CheckTimeOutcome::NoBuffer,
            CheckTimeOutcome::Conflict,
            CheckTimeOutcome::ReloadFailed,
        ] {
            let mut model = Model::new();
            let _ = confirm_gone(&mut model, 1, "/proj/src/lib.rs", false);
            let _ = update(
                &mut model,
                checktime_reply(3, &[("/proj/src/lib.rs", answered)]),
            );
            let before = model.engine.messages.entries.len();
            let effects = update(
                &mut model,
                checktime_reply(
                    4,
                    &[(
                        "/proj/src/lib.rs",
                        CheckTimeOutcome::FileGone { modified: false },
                    )],
                ),
            );
            assert!(
                matches!(effects.as_slice(), [Effect::ReprobeExternalWrite { .. }]),
                "{answered:?} ended the gone episode, so the next one is unconfirmed again, got {effects:?}"
            );
            assert_eq!(
                model.engine.messages.entries.len(),
                before,
                "and an unconfirmed answer says nothing: {:?}",
                model.engine.messages.entries
            );
        }
    }

    /// The episode ends when it has spoken, and the next one starts from
    /// nothing. A notice is transient: it leaves the screen on its own, and
    /// the file can come back with nothing watching it (the watch runs only
    /// while an agent session does). If anything survived that, the next
    /// unlink-then-rewrite save would be announced on its first answer and
    /// retracted a moment later -- the flash the confirmation exists to
    /// prevent, back through the side door.
    #[test]
    fn a_finished_episode_leaves_nothing_that_could_announce_the_next_one() {
        let mut model = Model::new();
        let raised = confirm_gone(&mut model, 1, "/proj/src/gen.rs", false);
        let expiry = raised
            .iter()
            .find_map(|e| match e {
                Effect::ScheduleToastExpiry { id, .. } => Some(*id),
                _ => None,
            })
            .expect("the notice the episode raised owes an expiry");
        assert_eq!(model.engine.messages.entries.len(), 1, "it spoke once");

        // the notice ages out, and the file comes back unobserved: nothing
        // ever answers for the path again until the next save unlinks it
        let _ = update(&mut model, Msg::ToastExpired { id: expiry });
        assert!(
            model.engine.messages.entries.is_empty(),
            "the notice left the screen: {:?}",
            model.engine.messages.entries
        );

        let effects = update(
            &mut model,
            checktime_reply(
                9,
                &[(
                    "/proj/src/gen.rs",
                    CheckTimeOutcome::FileGone { modified: false },
                )],
            ),
        );
        assert!(
            matches!(effects.as_slice(), [Effect::ReprobeExternalWrite { .. }]),
            "the next episode owes its own second look, got {effects:?}"
        );
        assert!(
            model.engine.messages.entries.is_empty(),
            "and says nothing until that look answers: {:?}",
            model.engine.messages.entries
        );
    }

    /// One path, one fact, two wordings. A `modified` flip between two
    /// confirmed answers -- the user typed while the path stayed absent --
    /// rewords the standing notice instead of stacking a second line
    /// beside it, because the older wording is not merely redundant: after
    /// the keystroke, "still holds the content it last read" is false, and
    /// a stale line contradicting the true one is worse than a repeat.
    #[test]
    fn a_modified_flip_rewords_the_standing_notice_instead_of_stacking_it() {
        let mut model = Model::new();
        let _ = confirm_gone(&mut model, 1, "/proj/src/gen.rs", false);
        assert_eq!(model.engine.messages.entries.len(), 1, "it spoke once");

        let _ = update(
            &mut model,
            checktime_reply(
                2,
                &[(
                    "/proj/src/gen.rs",
                    CheckTimeOutcome::FileGone { modified: true },
                )],
            ),
        );
        assert_eq!(
            model.engine.messages.entries.len(),
            1,
            "one line, not a stack: {:?}",
            model.engine.messages.entries
        );
        let line = &model.engine.messages.entries[0].content[0].1;
        assert!(
            line.ends_with("your buffer still holds your edits"),
            "the line wears the current wording: {line}"
        );
    }

    /// Only the answer the confirming probe itself provoked may confirm.
    /// Any other reply naming the path -- a watcher batch that happened to
    /// nominate it while the probe was in flight -- is a first answer like
    /// any other, because it was taken at a moment nobody chose.
    #[test]
    fn an_answer_the_confirming_probe_did_not_provoke_confirms_nothing() {
        let mut model = Model::new();
        let gone = CheckTimeOutcome::FileGone { modified: false };
        let _ = update(
            &mut model,
            checktime_reply(1, &[("/proj/src/lib.rs", gone)]),
        );
        let confirming = reprobe(&mut model, "/proj/src/lib.rs");

        let effects = update(
            &mut model,
            checktime_reply(confirming + 7, &[("/proj/src/lib.rs", gone)]),
        );
        assert!(
            matches!(effects.as_slice(), [Effect::ReprobeExternalWrite { .. }]),
            "a reply nobody asked for is a first answer, got {effects:?}"
        );
        assert!(
            model.engine.messages.entries.is_empty(),
            "and says nothing: {:?}",
            model.engine.messages.entries
        );

        let effects = update(
            &mut model,
            checktime_reply(confirming, &[("/proj/src/lib.rs", gone)]),
        );
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleToastExpiry { .. })));
        assert_eq!(
            model.engine.messages.entries.len(),
            1,
            "the probe's own answer is what speaks: {:?}",
            model.engine.messages.entries
        );
    }

    /// One probe in flight per path, and the last nomination owns it: a
    /// second look asked for while an earlier one is still out replaces it
    /// rather than joining it. That is what makes "one entry per path" the
    /// record's real bound rather than a hopeful one, and it keeps the
    /// superseded probe's late reply from announcing on behalf of an
    /// episode that has already moved on.
    #[test]
    fn a_second_look_asked_for_twice_leaves_only_the_later_probe_able_to_confirm() {
        let mut model = Model::new();
        let gone = CheckTimeOutcome::FileGone { modified: false };
        let _ = update(
            &mut model,
            checktime_reply(1, &[("/proj/src/lib.rs", gone)]),
        );
        let superseded = reprobe(&mut model, "/proj/src/lib.rs");
        let current = reprobe(&mut model, "/proj/src/lib.rs");

        let _ = update(
            &mut model,
            checktime_reply(superseded, &[("/proj/src/lib.rs", gone)]),
        );
        assert!(
            model.engine.messages.entries.is_empty(),
            "the replaced probe's answer speaks for nobody: {:?}",
            model.engine.messages.entries
        );
        let _ = update(
            &mut model,
            checktime_reply(current, &[("/proj/src/lib.rs", gone)]),
        );
        assert_eq!(
            model.engine.messages.entries.len(),
            1,
            "the probe that is still outstanding is the one that answers"
        );
    }

    /// A conflict reads the path as a file (it is answered below the
    /// chunk's own stat), so a notice saying the path cannot be read is
    /// false the moment the conflict arrives -- and left standing it would
    /// also silence the second look the next genuine removal owes.
    #[test]
    fn a_conflict_takes_down_the_notice_it_contradicts() {
        let mut model = Model::new();
        let _ = confirm_gone(&mut model, 1, "/proj/src/lib.rs", true);
        assert_eq!(model.engine.messages.entries.len(), 1);

        let _ = update(
            &mut model,
            checktime_reply(4, &[("/proj/src/lib.rs", CheckTimeOutcome::Conflict)]),
        );
        assert!(
            model.engine.messages.entries.is_empty(),
            "the file is there to conflict over: {:?}",
            model.engine.messages.entries
        );
        assert_eq!(model.overlays().len(), 1, "and the question is open");
    }

    /// A notice asserts the path cannot be read *now*, so an answer that
    /// reads it takes the notice down rather than leaving it to age out:
    /// a save that unlinks before it rewrites can be nominated between its
    /// two halves, and the user must not be told a file is gone while it
    /// is back.
    #[test]
    fn a_path_readable_again_takes_its_notice_down() {
        for back in [
            CheckTimeOutcome::HandledSilently,
            CheckTimeOutcome::Reloaded,
            CheckTimeOutcome::NoBuffer,
        ] {
            let mut model = Model::new();
            let before = model.engine.messages.entries.len();
            let _ = confirm_gone(&mut model, 1, "/proj/src/lib.rs", true);
            assert_eq!(model.engine.messages.entries.len(), before + 1);
            model.dirty = false;
            let _ = update(
                &mut model,
                checktime_reply(3, &[("/proj/src/lib.rs", back)]),
            );
            assert_eq!(
                model.engine.messages.entries.len(),
                before,
                "{back:?} leaves the notice nothing true left to assert: {:?}",
                model.engine.messages.entries
            );
            assert!(model.dirty, "taking a line off the screen needs a repaint");
        }
    }

    /// A different path's notice is left alone by the one that came back:
    /// the withdrawal is keyed on the path, never on "a notice of this
    /// shape is standing".
    #[test]
    fn a_path_readable_again_leaves_another_paths_notice_standing() {
        let mut model = Model::new();
        let before = model.engine.messages.entries.len();
        let _ = confirm_gone(&mut model, 1, "/proj/src/other.rs", false);
        let _ = update(
            &mut model,
            checktime_reply(
                3,
                &[("/proj/src/lib.rs", CheckTimeOutcome::HandledSilently)],
            ),
        );
        assert_eq!(
            model.engine.messages.entries.len(),
            before + 1,
            "only the path that came back loses its notice: {:?}",
            model.engine.messages.entries
        );
    }

    /// A suppressed notice changed nothing on screen, so it must not cost
    /// a paint: with a pipe under the watch this arm runs once per coalesce
    /// window for as long as the writing lasts, and paint cost is a
    /// contract here.
    #[test]
    fn a_suppressed_notice_does_not_ask_for_a_repaint() {
        let mut model = Model::new();
        let _ = confirm_gone(&mut model, 1, "/proj/src/lib.rs", false);
        model.dirty = false;
        let effects = update(
            &mut model,
            checktime_reply(
                3,
                &[(
                    "/proj/src/lib.rs",
                    CheckTimeOutcome::FileGone { modified: false },
                )],
            ),
        );
        assert!(effects.is_empty(), "nothing was said, so nothing is owed");
        assert!(
            !model.dirty,
            "the screen already shows the only thing this had to say"
        );
    }

    /// The watcher's own probe answers `FileGone` too, and it must land on
    /// the notice rather than the prompt: a path nvim cannot read has no
    /// reload to offer, so a prompt would ask a question with one answer.
    #[test]
    fn an_unforced_probe_of_a_vanished_file_notices_rather_than_prompts() {
        let mut model = Model::new();
        let before = model.engine.messages.entries.len();
        let _ = confirm_gone(&mut model, 1, "/proj/src/lib.rs", false);
        assert!(
            model.engine.messages.entries.len() > before,
            "a probe that found no readable file must not be silent"
        );
        assert_eq!(
            model.overlays().len(),
            0,
            "there is nothing to reload, so there is nothing to ask"
        );
    }

    /// The prompt is opened by one detection and outlived by the next. A
    /// user reading "Reload and discard the local edits, or keep them?"
    /// while an agent deletes the file underneath them must not be left
    /// holding a question whose expensive answer now buys nothing -- and
    /// must not have to answer it to find that out.
    #[test]
    fn a_conflict_prompt_is_withdrawn_when_its_path_stops_being_readable() {
        let mut model = Model::new();
        let _ = update(
            &mut model,
            checktime_reply(1, &[("/proj/src/lib.rs", CheckTimeOutcome::Conflict)]),
        );
        assert_eq!(model.overlays().len(), 1, "the conflict prompt opened");

        let before = model.engine.messages.entries.len();
        let _ = confirm_gone(&mut model, 2, "/proj/src/lib.rs", true);
        assert_eq!(
            model.overlays().len(),
            0,
            "the question has no true answer left, so it must be withdrawn"
        );
        assert!(
            model.engine.messages.entries.len() > before,
            "withdrawing the prompt must not also take away the explanation"
        );
    }

    /// One reply must never close another file's still-open conflict. The
    /// paths are separate questions and only one of them went stale.
    #[test]
    fn a_vanished_path_leaves_another_files_conflict_prompt_open() {
        let mut model = Model::new();
        let _ = update(
            &mut model,
            checktime_reply(1, &[("/proj/src/other.rs", CheckTimeOutcome::Conflict)]),
        );
        assert_eq!(model.overlays().len(), 1);

        let _ = confirm_gone(&mut model, 2, "/proj/src/lib.rs", true);
        assert_eq!(
            model.overlays().len(),
            1,
            "another file's conflict is a different question, still answerable"
        );
        assert!(model
            .external_write_conflict_prompt_mut(std::path::Path::new("/proj/src/other.rs"))
            .is_some());
    }

    #[test]
    fn a_degraded_watch_is_reported() {
        let mut model = Model::new();
        let before = model.engine.messages.entries.len();
        let _ = update(
            &mut model,
            Msg::ExternalWatchDegraded {
                reason: "the platform's watch limit was reached".to_string(),
            },
        );
        assert!(
            model.engine.messages.entries.len() > before,
            "a degraded watch must not be silent"
        );
    }

    /// One reply answering several paths dispositions each on its own: a
    /// batch is not all-or-nothing, and the conflict in it must still reach
    /// the user even though the paths around it had nothing to show.
    #[test]
    fn each_entry_of_a_batched_reply_is_dispositioned_on_its_own() {
        let mut model = Model::new();
        let _ = update(
            &mut model,
            checktime_reply(
                1,
                &[
                    ("/proj/a.rs", CheckTimeOutcome::NoBuffer),
                    ("/proj/b.rs", CheckTimeOutcome::HandledSilently),
                    ("/proj/c.rs", CheckTimeOutcome::Conflict),
                ],
            ),
        );
        assert_eq!(model.overlays().len(), 1);
        assert!(model
            .external_write_conflict_prompt_mut(std::path::Path::new("/proj/c.rs"))
            .is_some());
    }

    /// An untrusted project is never watched, asserted at the only place
    /// this crate can assert it: no `Effect::Ai` leaves an untrusted
    /// project, and the watch starts nowhere else (the bin's own
    /// `AiWorker` starts one only from an `AiCommand::Prompt` it receives,
    /// which is what this gate withholds). Its counterpart at the watcher
    /// level lives in `ai_worker.rs`.
    #[test]
    fn an_untrusted_project_dispatches_nothing_a_watch_could_start_from() {
        let mut model = Model::new();
        model.ai_trusted = false;
        let effects = update(
            &mut model,
            Msg::FeatureInvoke {
                feature: "ai".to_string(),
                verb: "toggle".to_string(),
            },
        );
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::Ai(_))),
            "an untrusted project must reach no agent command at all, got {effects:?}"
        );
    }

    /// A second conflict reply for the SAME path replaces the open prompt
    /// in place rather than stacking a duplicate -- the same dedupe
    /// `open_ai_trust_prompt` gives its own single global prompt, applied
    /// here per-path.
    #[test]
    fn a_second_conflict_for_the_same_path_replaces_rather_than_stacks() {
        let mut model = Model::new();
        let _ = update(
            &mut model,
            checktime_reply(1, &[("/proj/src/lib.rs", CheckTimeOutcome::Conflict)]),
        );
        let _ = update(
            &mut model,
            checktime_reply(2, &[("/proj/src/lib.rs", CheckTimeOutcome::Conflict)]),
        );
        assert_eq!(model.overlays().len(), 1);
    }

    /// A conflict for a DIFFERENT path opens its own, second prompt: unlike
    /// the single global AI trust prompt, two files can each have their own
    /// pending conflict at once.
    #[test]
    fn conflicts_for_different_paths_each_get_their_own_prompt() {
        let mut model = Model::new();
        let _ = update(
            &mut model,
            checktime_reply(
                1,
                &[
                    ("/proj/a.rs", CheckTimeOutcome::Conflict),
                    ("/proj/b.rs", CheckTimeOutcome::Conflict),
                ],
            ),
        );
        assert_eq!(model.overlays().len(), 2);
    }
}
