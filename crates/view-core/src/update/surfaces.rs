//! Opening, toggling and refreshing the surfaces that sit beside the
//! buffer -- the picker, the file tree sidebar, the agent panel and the
//! message-history overlay -- plus the effects each one's first frame
//! needs, the keys the history overlay answers, and the notices a verb
//! raises instead of opening anything. One family, split out of `update`
//! so the router keeps to routing.

use crate::model::{Focus, Model, OverlayKind};
use crate::msg::{Effect, RegisterType, RpcCall, WinSplit};
use crate::native::geometry::{Anchor, NativeSurface, OverlayBox};
use crate::native::keys::{Action, Resolved};
use crate::native::palette::MessageHistoryState;

use super::{path_to_wire, take_binding};

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
        "view: the AI agent panel is off. Turn it on with ai.enabled = true in view.toml"
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
/// already open -- the toggle semantic `<leader>e` carries over from the
/// file-tree plugins a switching user arrives with. Reachable only while
/// the engine holds focus (`Msg::FeatureInvoke` is nvim's own `rpcnotify`,
/// which a native overlay's focus would intercept before it ever reaches
/// nvim's mapping, see
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
    if model.tree_is_windowed() {
        return toggle_windowed_tree(model);
    }
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

/// The tree's key while it takes a window of its own: closed from inside
/// it, opened or entered from anywhere else.
///
/// Entering a standing window is the same call as opening one, because the
/// engine answers a surface it has a live window for by going to it. One
/// message, so a key pressed twice quickly cannot leave two windows.
fn toggle_windowed_tree(model: &mut Model) -> Vec<Effect> {
    if model.focus() == Focus::Pane(NativeSurface::Tree) {
        return close_windowed_tree(model);
    }
    let mut effects = if model.tree_mut().is_some() {
        Vec::new()
    } else {
        open_tree_state(model)
    };
    effects.append(&mut vec![Effect::Rpc(open_native_window(
        model,
        NativeSurface::Tree,
    ))]);
    effects
}

/// Closes the window the tree sits in and drops its state.
fn close_windowed_tree(model: &mut Model) -> Vec<Effect> {
    let win = model
        .engine
        .grids()
        .native_window(NativeSurface::Tree)
        .map(|handle| handle.0);
    let closed = model.close_tree();
    model.dirty = true;
    let mut effects = Vec::new();
    if closed {
        effects.append(&mut vec![Effect::TreeClose]);
    }
    if let Some(win) = win {
        effects.append(&mut vec![Effect::Rpc(RpcCall::CloseNativeWindow { win })]);
    }
    effects
}

/// The call that opens `surface`'s window, or enters the one it already
/// has, at the anchor and size this session resolved for it.
fn open_native_window(model: &mut Model, surface: NativeSurface) -> RpcCall {
    let layout = model.surfaces.layout(surface);
    RpcCall::OpenNativeWindow {
        surface,
        split: WinSplit::for_anchor(layout.anchor).unwrap_or(WinSplit::Left),
        size: layout.size,
        generation: model.surfaces.next_generation(),
    }
}

/// The tree's own state on the overlay stack, with the scans its first
/// frame needs. Shared by both placements: the state is the same either
/// way, and only what draws it differs.
fn open_tree_state(model: &mut Model) -> Vec<Effect> {
    let mut state = crate::native::tree::TreeState::open(model.cwd.clone());
    let scan_generation = state.generation();
    let git_generation = state.request_git_refresh();
    let geometry = OverlayBox::new(model.tree_width_pct, 100).with_anchor(Anchor::Left);
    model.push_overlay(geometry, OverlayKind::Tree(state));
    model.dirty = true;
    let mut effects = vec![Effect::TreeScan {
        generation: scan_generation,
        root: model.cwd.clone(),
    }];
    if let Some(generation) = git_generation {
        effects.append(&mut vec![Effect::TreeGitScan {
            generation,
            root: model.cwd.clone(),
        }]);
    }
    effects
}

/// Binds the window nvim opened to the surface view asked for it, which is
/// what makes the next `win_pos` for that handle place a pane view paints.
///
/// A reply for a generation older than the one the last open carried is
/// dropped: the surface has been closed and reopened since the call, and
/// the handle it names belongs to a window that is already gone.
pub(super) fn native_window_opened(
    model: &mut Model,
    generation: u64,
    surface: NativeSurface,
    win: crate::events::WinHandle,
) -> Vec<Effect> {
    if generation != model.surfaces.generation() {
        return Vec::new();
    }
    model.engine.grids_mut().claim_native_window(win, surface);
    model.dirty = true;
    Vec::new()
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

/// Every key this overlay answers, with what the docs page says about it.
///
/// The one list: [`message_history_key`] matches these notations and
/// `crate::native::mappings::render_history_table` renders them, so a key
/// added to the overlay and not to the page fails
/// `the_keys_page_renders_the_history_overlay_keys_this_build_answers`,
/// and one that is documented but dead fails
/// `every_documented_history_key_answers_a_real_keystroke`.
///
/// `gg` is two `g` presses; every other entry is one key event.
///
/// Test-only, like the table renderer it feeds: the page carries the
/// rendered rows and the walk presses them, and nothing in a running
/// session reads this list.
/// Every key the file tree answers, whether it is floating over the buffer
/// or sitting in a window of its own.
///
/// Shared by both placements so a key cannot mean one thing in a float and
/// another in a tile. The payload of the overlay match that reaches it here
/// is discarded deliberately: every branch below reaches the tree through
/// `model.tree_mut()` fresh instead, since a bound `&mut TreeState` would
/// keep `model` borrowed across the `pop_focused_overlay` and `close_tree`
/// calls the `<CR>` and `<Esc>` arms need.
pub(super) fn tree_key(model: &mut Model, notation: &str) -> Vec<Effect> {
    // Ahead of the tree's own keys and resolved through the one
    // shared set, so neither sidebar can drift onto a key the
    // other does not answer (see [`take_binding`]).
    match take_binding(model, notation) {
        Some(Resolved::Act(Action::Resize(direction))) => {
            if model.resize_tree(direction.widens()) {
                model.dirty = true;
            }
            return Vec::new();
        }
        // The composer's line break is the agent panel's alone,
        // and the tree answers it the way it answers any key no
        // binding of its own names.
        Some(Resolved::Act(Action::ComposerNewline)) => {}
        // The chord's first key waits here rather than moving
        // the selection or closing the sidebar; the follower
        // that completes nothing falls straight through to the
        // arms below on its own next pass.
        Some(Resolved::Pending) => return Vec::new(),
        None => {}
    }
    match notation {
        // leaving a windowed tree is leaving its window, and the tile
        // stands: nvim's own layout is what put it there, and a key that
        // dissolved a window the user split for themselves would be view
        // undoing a window command
        "<Esc>" if model.tree_is_windowed() => {
            vec![Effect::Rpc(RpcCall::FocusPreviousWindow)]
        }
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
                    let open = Effect::Rpc(RpcCall::OpenFile {
                        path: path_to_wire(&path),
                    });
                    // the cursor sits in the tree's own window, and `:edit`
                    // opens in the window it runs in, so the file would
                    // land inside the sidebar
                    if model.tree_is_windowed() {
                        return vec![Effect::Rpc(RpcCall::FocusPreviousWindow), open];
                    }
                    model.pop_focused_overlay();
                    vec![open]
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
    }
}

#[cfg(test)]
pub(crate) const HISTORY_KEYS: &[(&str, &str)] = &[
    ("j", "select the next entry"),
    ("k", "select the previous entry"),
    ("<C-d>", "select half a screen further down"),
    ("<C-u>", "select half a screen further up"),
    ("gg", "select the newest entry"),
    ("G", "select the oldest entry"),
    (
        "y",
        "copy the selected entry verbatim, to the system clipboard and over OSC 52",
    ),
    (
        "d",
        "take down the standing notice the selected entry belongs to",
    ),
];

/// One keypress aimed at the open message-history overlay.
///
/// `<Esc>` never reaches here (see the caller's guard): closing an overlay
/// is the router's shared fallback, not a key of this overlay's own.
///
/// `d` retires a notice view raised about a condition it observed, one
/// family at a time, which is what it is for: the input rule takes the
/// whole standing set once each line has stood its reading window
/// (`Messages::dismiss_read_sticky`), and this is how a user takes down the
/// one line they are done with. nvim's own sticky errors carry no family,
/// so `d` no-ops on them and `<Esc>` is their way out.
pub(super) fn message_history_key(model: &mut Model, notation: &str) -> Vec<Effect> {
    // `gg` reaches the router as two `g` events -- `encode_key` emits one
    // notation per key event -- and `dispatch` drops the shared chord
    // prefix for every overlay but the sidebars, so the first half is held
    // on the overlay's own state. Taken here, before the key is answered,
    // so any other key spends it.
    let armed = history_mut(model).is_some_and(MessageHistoryState::take_g);
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
        "g" if armed => state.select(0),
        "g" => {
            state.arm_g();
            false
        }
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
