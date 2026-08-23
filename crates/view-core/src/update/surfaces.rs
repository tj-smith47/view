//! Opening, toggling and refreshing the surfaces that sit beside the
//! buffer -- the picker, the file tree sidebar, the agent panel and the
//! message-history overlay -- plus the effects each one's first frame
//! needs. One family, split out of `update` so the router keeps to
//! routing.

use crate::model::{Model, OverlayKind};
use crate::msg::{Effect, RpcCall};
use crate::native::geometry::{Anchor, OverlayBox};

/// Issues an `Effect::Rpc(RpcCall::PreviewBuffer)` for `state`'s current
/// selection, or no effect at all when there is nothing to preview (an
/// empty result set, or an unnamed `Buffers` scratch entry -- see
/// `PickerState::selected_path`'s doc). Shared by every arm that can move
/// the selection: today that is only `Msg::PickerResults` (no arrow-key/
/// Enter navigation exists yet), but the seam is named rather than inlined
/// so a future navigation arm reuses it instead of re-deriving the request.
pub(super) fn picker_preview_request(
    state: &mut crate::native::picker::PickerState,
) -> Vec<Effect> {
    match state.refresh_preview() {
        Some((generation, path)) => vec![Effect::Rpc(RpcCall::PreviewBuffer { path, generation })],
        None => Vec::new(),
    }
}

/// Answers a `Msg::FeatureInvoke` naming `ai` while `model.ai_enabled` is
/// false: a native notice naming the exact config line that turns it back
/// on, and nothing else -- no prompt, no panel, no trust question, since a
/// disabled feature has nothing behind any of those to open.
pub(super) fn notice_ai_disabled(model: &mut Model) -> Vec<Effect> {
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
pub(super) fn picker_source_for_verb(
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
pub(super) fn open_picker(model: &mut Model, source: crate::native::picker::Source) -> Vec<Effect> {
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
pub(super) fn tree_git_refresh_effect(model: &mut Model) -> Vec<Effect> {
    let Some(tree) = model.tree_mut() else {
        return Vec::new();
    };
    let root = tree.root().to_path_buf();
    match tree.request_git_refresh() {
        Some(generation) => vec![Effect::TreeGitScan { generation, root }],
        None => Vec::new(),
    }
}

pub(super) fn toggle_tree_sidebar(model: &mut Model) -> Vec<Effect> {
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
    let geometry = OverlayBox::new(model.tree_width_pct, 100).with_anchor(Anchor::Left);
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
pub(super) fn open_ai_panel(model: &mut Model) -> Vec<Effect> {
    if model.ai_panel_overlay_open() {
        return Vec::new();
    }
    let insert_beneath = model.overlays().last().is_some_and(|overlay| {
        Model::takes_focus(&overlay.kind) || matches!(overlay.kind, OverlayKind::EngineBusy(_))
    });
    let geometry = OverlayBox::new(model.ai_panel_width_pct, 100).with_anchor(Anchor::Right);
    if insert_beneath {
        model.insert_overlay_beneath_top(geometry, OverlayKind::Ai);
    } else {
        model.push_overlay(geometry, OverlayKind::Ai);
    }
    model.dirty = true;
    Vec::new()
}

/// One verb over three states: closed opens and enters, open-and-entered
/// closes, open-but-left re-enters. The middle state is the one the panel
/// alone has -- it is non-modal, so `<Esc>` un-enters it without closing it
/// (see `AiPanelState::focused`'s own doc), and a toggle that read that as
/// "open, therefore close" left the visible panel with no key back into it.
///
/// Closing never tears down a live session, for the same reason opening
/// never starts one (see [`open_ai_panel`]'s doc): a session already
/// running keeps running, unattended, exactly as `close_ai_panel`'s own doc
/// promises. Every direction here is an explicit user invoke, so it claims
/// or releases the panel's keyboard focus the same way the `open`/`close`
/// verbs do.
pub(super) fn toggle_ai_panel(model: &mut Model) -> Vec<Effect> {
    if model.ai_panel_overlay_open() && !model.ai_panel().focused {
        model.ai_panel_mut().focused = true;
        model.dirty = true;
        return Vec::new();
    }
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
pub(super) fn open_message_history(model: &mut Model) -> Vec<Effect> {
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
