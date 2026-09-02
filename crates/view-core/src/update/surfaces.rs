//! Opening, toggling and refreshing the surfaces that sit beside the
//! buffer -- the picker, the file tree sidebar, the agent panel and the
//! message-history overlay -- plus the effects each one's first frame
//! needs, the keys the history overlay answers, and the notices a verb
//! raises instead of opening anything. One family, split out of `update`
//! so the router keeps to routing.

use crate::model::{Model, OverlayKind};
use crate::msg::{Effect, RegisterType, RpcCall};
use crate::native::geometry::{Anchor, OverlayBox};
use crate::native::palette::MessageHistoryState;

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
    // entered decides the direction, and only then does anything close:
    // `close_ai_panel` itself clears `AiPanelState::focused`, at the single
    // authoritative closing point, so a `true` here always names a panel
    // the user is actually in
    if model.ai_panel().focused && model.close_ai_panel() {
        model.dirty = true;
        return Vec::new();
    }
    // a no-op on the panel already open, which is what leaves re-entry with
    // nothing to do but claim the keyboard below
    let effects = open_ai_panel(model);
    model.ai_panel_mut().focused = true;
    model.dirty = true;
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
    let state = MessageHistoryState::snapshot(&model.engine.toast_history);
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

/// The rows the framed history overlay spends on everything that is not a
/// history entry: its two borders, its (empty) query line and the rule
/// under it, which `view_surface::overlay`'s `rows` and `palette_body`
/// between them always draw.
///
/// Named here so a page key moves the selection by exactly what the frame
/// last showed. Mechanism honesty: this crate cannot depend on the one that
/// paints it, so nothing ties the two numbers together mechanically --
/// `view_surface::overlay`'s `a_framed_palette_spends_four_rows_on_chrome`
/// pins the painter's half against this same four, and its doc names this
/// constant.
const HISTORY_CHROME_ROWS: u16 = 4;

/// One keypress aimed at the open message-history overlay.
///
/// `<Esc>` never reaches here (see the caller's guard): closing an overlay
/// is the router's shared fallback, not a key of this overlay's own.
pub(super) fn message_history_key(model: &mut Model, notation: &str) -> Vec<Effect> {
    // the two keys that reach past the overlay answer first, so neither is
    // holding a borrow of it while it touches the message log beside it
    match notation {
        "y" => return copy_selection(history(model).and_then(MessageHistoryState::selected_text)),
        // A dismissal retracts a standing notice; it never edits the
        // history, which is the record of what was said and stays true
        // whether or not the line is still up. An entry with no family --
        // every wire message -- therefore takes nothing down.
        "d" => {
            let family = history(model)
                .and_then(MessageHistoryState::selected_family)
                .map(str::to_owned);
            if let Some(family) = family {
                model.dirty |= model.engine.withdraw_native_notice(&family);
            }
            return Vec::new();
        }
        _ => {}
    }
    let page = history_page(model);
    let Some(state) = history_mut(model) else {
        return Vec::new();
    };
    let moved = match notation {
        "j" => state.move_selection(1),
        "k" => state.move_selection(-1),
        "<C-d>" => state.move_selection(page),
        "<C-u>" => state.move_selection(-page),
        "gg" => state.select(0),
        "G" => state.select(usize::MAX),
        _ => false,
    };
    model.dirty |= moved;
    Vec::new()
}

/// The half-page `<C-d>`/`<C-u>` move, derived from the open overlay's own
/// painted height rather than a fixed number, so a page is half of what the
/// user can actually see. Floored at one: a frame with no room for entries
/// at all still moves the selection rather than swallowing the key.
fn history_page(model: &Model) -> isize {
    let rows = model.focused_overlay().map_or(0, |overlay| {
        model
            .overlay_rect(overlay)
            .height
            .saturating_sub(HISTORY_CHROME_ROWS)
    });
    isize::try_from(rows.div_ceil(2))
        .unwrap_or(isize::MAX)
        .max(1)
}

/// The open history overlay's state, or `None` when the focused overlay is
/// something else -- which the caller's own match has already ruled out,
/// and which this answers without panicking anyway.
fn history(model: &Model) -> Option<&MessageHistoryState> {
    match model.focused_overlay().map(|overlay| &overlay.kind) {
        Some(OverlayKind::MessageHistory(state)) => Some(state),
        _ => None,
    }
}

/// [`history`], for the keys that move the selection.
fn history_mut(model: &mut Model) -> Option<&mut MessageHistoryState> {
    match model.focused_overlay_mut().map(|overlay| &mut overlay.kind) {
        Some(OverlayKind::MessageHistory(state)) => Some(state),
        _ => None,
    }
}

/// Copies the selected entry's line, through the identical pair of effects
/// an engine-initiated `"+y` produces (`EngineRequest::ClipboardSet`'s own
/// arm): the local system-clipboard write and the OSC 52 escape, never one
/// or the other. That pairing is the whole reason this key is worth having
/// over an SSH session -- the escape reaches the terminal the user is
/// actually sitting at, and the local write serves the session that has a
/// display of its own.
///
/// `token: None` because nvim asked for nothing here (see
/// [`Effect::ClipboardWrite`]), and `Charwise` because a copied line is a
/// line, not a linewise register: `lines_to_text` appends no newline to it,
/// which is what keeps a pasted path a path.
fn copy_selection(text: Option<String>) -> Vec<Effect> {
    let Some(text) = text else {
        return Vec::new();
    };
    let lines = vec![text];
    vec![
        Effect::ClipboardWrite {
            token: None,
            register: COPY_REGISTER,
            lines: lines.clone(),
            regtype: RegisterType::Charwise,
        },
        Effect::Osc52Copy {
            register: COPY_REGISTER,
            lines,
            regtype: RegisterType::Charwise,
        },
    ]
}

/// The register the history's own copy lands in: `'+'`, the system
/// clipboard, which is where a user pasting into another program looks.
const COPY_REGISTER: char = '+';

/// The family every notice about an unreachable system clipboard is
/// recorded under. Pinned as a constant because
/// `surface_conflict`'s own collision walk ranges over it beside every
/// other family in the crate.
pub(super) const CLIPBOARD_NOTICE_FAMILY: &str = "view: no system clipboard ";

/// What the user is told, once, when a copy could not reach a system
/// clipboard. Opens with its own family, as `is_standing_native_notice`'s
/// `starts_with` withdrawal requires, and says where the copy did go rather
/// than only where it did not.
const CLIPBOARD_UNAVAILABLE: &str = "view: no system clipboard is reachable; copies went to \
                                     view's own registers and to OSC 52.";

/// Answers [`crate::msg::Msg::ClipboardUnavailable`]: the once-per-family
/// notice, and nothing else -- the copy itself already succeeded into the
/// worker's shadow register and onto the terminal.
pub(super) fn notice_clipboard_unavailable(model: &mut Model) -> Vec<Effect> {
    let effects = model
        .engine
        .record_native_notice_once(CLIPBOARD_NOTICE_FAMILY, CLIPBOARD_UNAVAILABLE.to_string());
    // an empty answer is the dedupe declining to say the same thing twice,
    // and a repaint for a screen nothing changed on is what a second copy
    // would otherwise cost
    model.dirty |= !effects.is_empty();
    effects
}
