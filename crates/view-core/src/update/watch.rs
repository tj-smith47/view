//! Out-of-band write handling: the fs watcher's own detections, folded into
//! `RpcCall::Checktime` and the conflict prompt its `fired` reply raises.
//!
//! Two hops, mirroring `ai_fs.rs`'s own shape: a detection becomes a
//! `RpcCall::Checktime` (`Msg::ExternalWriteDetected` here), and the reply
//! either raises nothing (`found: false`, or `found: true` with nvim's own
//! silent handling) or opens the conflict prompt (`fired: true`) -- see
//! `docs/checktime-wire-capture.md` for the three-way split this reads.

use std::path::PathBuf;

use crate::model::{Model, OverlayKind};
use crate::msg::{Effect, RpcCall};
use crate::native::geometry::OverlayBox;
use crate::native::prompt::PromptState;

/// Starts the round trip for one `Msg::ExternalWriteDetected`: mints a
/// fresh `request_id` and drives `RpcCall::Checktime` with `force: false`,
/// the watcher's own probe. Never inspects `path`'s content and never
/// decides silent-reload vs. conflict itself -- nvim's reply does that (see
/// `RpcCall::Checktime`'s own doc).
pub(super) fn on_external_write_detected(model: &mut Model, path: PathBuf) -> Vec<Effect> {
    let request_id = model.next_checktime_request_id();
    vec![Effect::Rpc(RpcCall::Checktime {
        request_id,
        path: super::path_to_wire(&path),
        force: false,
    })]
}

/// Folds one `Msg::CheckTimeReply`: `found && fired` is the only shape that
/// raises anything (see `Msg::CheckTimeReply`'s own doc for why the other
/// three combinations are all "nothing to show" -- no buffer, a silent
/// reload nvim already performed, or a self-write no-op).
pub(super) fn on_checktime_reply(
    model: &mut Model,
    path: PathBuf,
    found: bool,
    fired: bool,
) -> Vec<Effect> {
    if !(found && fired) {
        return Vec::new();
    }
    open_conflict_prompt(model, path)
}

/// Opens (or, for a second conflict on the same still-open path, replaces
/// in place) the confirm-class conflict prompt, on the identical
/// dedupe-by-lookup and focus-stacking terms `update/mod.rs`'s
/// `open_ai_trust_prompt` uses for its own locally-resolving prompt: a
/// blocked-engine `Prompt` may already have taken focus above this one, and
/// inserting beneath it (rather than displacing it) never steals an answer
/// nvim is still waiting on.
fn open_conflict_prompt(model: &mut Model, path: PathBuf) -> Vec<Effect> {
    let message = format!(
        "{} changed outside view while the buffer had local edits. Reload \
         and discard the local edits, or keep them and ignore the external \
         change?",
        path.display()
    );
    let state = PromptState::external_write_conflict_prompt(path.clone(), message);
    if let Some(p) = model.external_write_conflict_prompt_mut(&path) {
        *p = state;
        model.dirty = true;
        return Vec::new();
    }
    match model.focused_overlay_mut().map(|ov| &ov.kind) {
        Some(OverlayKind::Prompt(_)) => {
            model.insert_overlay_beneath_top(OverlayBox::new(60, 40), OverlayKind::Prompt(state));
        }
        _ => {
            model.push_overlay(OverlayBox::new(60, 40), OverlayKind::Prompt(state));
        }
    }
    model.dirty = true;
    Vec::new()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::msg::Msg;
    use crate::update::update;

    fn external_write(path: &str) -> Msg {
        Msg::ExternalWriteDetected {
            path: PathBuf::from(path),
        }
    }

    fn checktime_reply(request_id: u64, path: &str, found: bool, fired: bool) -> Msg {
        Msg::CheckTimeReply {
            request_id,
            path: PathBuf::from(path),
            found,
            fired,
        }
    }

    /// The watcher's own detection always drives one `RpcCall::Checktime`
    /// with `force: false` -- the falsifiable half of "the watcher never
    /// decides itself, it only ever drives nvim's own check."
    #[test]
    fn an_external_write_drives_one_unforced_checktime() {
        let mut model = Model::new();
        let effects = update(&mut model, external_write("/proj/src/lib.rs"));
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::Rpc(RpcCall::Checktime {
                path,
                force,
                request_id,
            }) => {
                assert_eq!(path, "/proj/src/lib.rs");
                assert!(!force);
                assert!(*request_id > 0);
            }
            other => panic!("expected Effect::Rpc(RpcCall::Checktime), got {other:?}"),
        }
    }

    /// `found: false` -- no buffer for the path -- raises nothing: no
    /// overlay, no effect. This is case 1 of the falsifiable check: "an
    /// external write to a file with no loaded buffer produces no UI."
    #[test]
    fn no_buffer_found_raises_no_ui() {
        let mut model = Model::new();
        let effects = update(
            &mut model,
            checktime_reply(1, "/proj/gone.rs", false, false),
        );
        assert!(effects.is_empty());
        assert!(!model.ai_panel_overlay_open());
        assert_eq!(model.overlays().len(), 0);
    }

    /// `found: true, fired: false` -- nvim silently handled it (a real
    /// reload of an unmodified buffer, or a self-write no-op) -- also
    /// raises nothing: this is the mutation-killing case that distinguishes
    /// the required `found && fired` gate from a weaker `found` alone,
    /// which would wrongly pop a prompt for every self-write.
    #[test]
    fn found_but_not_fired_raises_no_ui() {
        let mut model = Model::new();
        let effects = update(
            &mut model,
            checktime_reply(1, "/proj/src/lib.rs", true, false),
        );
        assert!(effects.is_empty());
        assert_eq!(model.overlays().len(), 0);
    }

    /// `found: true, fired: true` -- the genuine conflict -- opens the
    /// confirm-class prompt, and only that combination does: this is the
    /// mutation-killing positive alongside the two negatives above.
    #[test]
    fn found_and_fired_opens_the_conflict_prompt() {
        let mut model = Model::new();
        let effects = update(
            &mut model,
            checktime_reply(1, "/proj/src/lib.rs", true, true),
        );
        assert!(effects.is_empty());
        assert_eq!(model.overlays().len(), 1);
        let path = model
            .external_write_conflict_prompt_mut(std::path::Path::new("/proj/src/lib.rs"))
            .expect("the conflict prompt must be open for this exact path");
        assert!(path.accepts("<CR>"));
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
            checktime_reply(1, "/proj/src/lib.rs", true, true),
        );
        let _ = update(
            &mut model,
            checktime_reply(2, "/proj/src/lib.rs", true, true),
        );
        assert_eq!(model.overlays().len(), 1);
    }

    /// A conflict for a DIFFERENT path opens its own, second prompt: unlike
    /// the single global AI trust prompt, two files can each have their own
    /// pending conflict at once.
    #[test]
    fn conflicts_for_different_paths_each_get_their_own_prompt() {
        let mut model = Model::new();
        let _ = update(&mut model, checktime_reply(1, "/proj/a.rs", true, true));
        let _ = update(&mut model, checktime_reply(2, "/proj/b.rs", true, true));
        assert_eq!(model.overlays().len(), 2);
    }
}
