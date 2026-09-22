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

/// `Msg::FeatureInvoke { feature: "ui", verb: "gaps" }`: flips `[ui] gaps`
/// and reissues everything a look change owes -- the outer grid's own size,
/// then each window's inner size -- through [`super::look::set_look`], the
/// one place that sequence is assembled. The gap is the whole of what
/// moves: the panes stay exactly where they were, so the request
/// `pending_inner_request` computes for a slot that did not move is still
/// owed, because its guard is keyed on the look as well as the slot (see
/// that method's own doc).
pub(crate) fn toggle_gaps(model: &mut Model) -> Vec<Effect> {
    let look = crate::model::Look::new(model.look.panes, !model.look.gaps);
    super::look::set_look(model, look)
}

/// `Msg::FeatureInvoke { feature: "ui", verb: "cycle_surfaces" }`: advances
/// the shared three-position ring
/// (`config`, `windowed`, `overlay`) every surface answers to at once, and
/// carries whatever is open across the step it just took.
///
/// A surface's own state -- the tree's cursor row, the agent panel's
/// transcript, the notification stream's scroll position -- never moves for
/// this: [`retile_open_surface`] only ever changes where a surface already
/// open draws, through the same claim/release and `OpenNativeWindow`/
/// `CloseNativeWindow` pair every other placement change goes through, and
/// never the close that would drop the state riding under it. A surface
/// with nothing open just gets a new default for its next open, no effect
/// owed.
pub(crate) fn cycle_placements(model: &mut Model) -> Vec<Effect> {
    let mut effects = Vec::new();
    for (surface, target, changed) in model.surfaces.advance_ring() {
        if changed && surface_is_open(model, surface) {
            effects.append(&mut retile_open_surface(model, surface, target));
        }
    }
    model.dirty = true;
    effects
}

/// Whether `surface` has anything open right now, on whichever placement it
/// currently draws under -- what [`cycle_placements`] asks before it moves
/// anything, since a surface with nothing open owes no effect, only a new
/// default for its next open.
fn surface_is_open(model: &Model, surface: NativeSurface) -> bool {
    match surface {
        NativeSurface::Tree => model
            .overlays()
            .iter()
            .any(|overlay| matches!(overlay.kind, OverlayKind::Tree(_))),
        NativeSurface::Agent => model.ai_panel_overlay_open(),
        NativeSurface::Notifications => history(model).is_some(),
        // the palette's own overlay is nvim's cmdline, never a thing on
        // `model.overlays()` -- see `open_native_window`'s doc
        NativeSurface::Palette => model.engine.cmdline.is_some(),
    }
}

/// Moves `surface`'s already-open state to `target`'s placement, touching
/// only where it draws: the window claim and its wire pair, never the state
/// itself. [`surface_is_open`] is the caller's guard that there is
/// something here to move at all.
fn retile_open_surface(
    model: &mut Model,
    surface: NativeSurface,
    target: crate::native::geometry::SurfacePlacement,
) -> Vec<Effect> {
    use crate::native::geometry::SurfacePlacement;
    // The palette carries no window of nvim's own under either placement --
    // `view_surface::render` reads `palette_windowed_active()` straight off
    // the model on the next paint, so a ring step that changes its
    // placement needs no `OpenNativeWindow`/`CloseNativeWindow` pair to
    // move it, unlike the other three surfaces.
    if surface == NativeSurface::Palette {
        return Vec::new();
    }
    match target {
        SurfacePlacement::Windowed => {
            if model.engine.grids().native_window(surface).is_some() {
                return Vec::new();
            }
            // the ring carries a surface's state across a placement step
            // the user aimed at the whole set, never at this one surface,
            // so the keyboard stays where the step found it
            vec![Effect::Rpc(open_native_window(model, surface, false))]
        }
        SurfacePlacement::Overlay => {
            let Some(win) = model.engine.grids().native_window(surface) else {
                // still mid-open: there is no window yet to release, but
                // this step has already moved the surface off windowed, so
                // the open in flight is retired here rather than left to
                // land later and claim a window nothing wants any more
                // (see `native_window_opened`'s own stale-generation arm)
                model.surfaces.cancel_pending_open(surface);
                return Vec::new();
            };
            model.engine.grids_mut().release_native_window(win);
            vec![Effect::Rpc(RpcCall::CloseNativeWindow { win: win.0 })]
        }
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
    let prompt_is_topmost = matches!(
        model.overlays().last().map(|overlay| &overlay.kind),
        Some(OverlayKind::Prompt(_))
    );
    open_tree_state(model, prompt_is_topmost)
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
        open_tree_state(model, false)
    };
    effects.append(&mut vec![Effect::Rpc(open_native_window(
        model,
        NativeSurface::Tree,
        true,
    ))]);
    effects
}

/// The agent panel's key while it takes a window of its own: leaving the
/// window is leaving the tile, the same as the tree's own `<Esc>` (see
/// [`tree_key`]'s doc), and every other key is the panel's ordinary
/// composer/permission handling.
///
/// A pending permission's own `<Esc>` (which cancels the request rather
/// than leaving) still reaches [`crate::update::ai::ai_panel_key`]
/// unchanged: only the plain "nothing else owns this key" `<Esc>` is
/// reinterpreted as a window command here.
pub(super) fn agent_pane_key(model: &mut Model, notation: &str) -> Vec<Effect> {
    if notation == "<Esc>" && model.ai_panel().pending_permission.is_none() {
        return vec![Effect::Rpc(RpcCall::FocusPreviousWindow)];
    }
    // A windowed agent panel's own `<C-w>` chord is held the same way the
    // tree's and the notification stream's are: the prefix waits here, and
    // reaches nvim together with its follower only once the follower is
    // known not to be the panel's own resize chord (see
    // `notifications_pane_key`).
    let armed_before = model.pending_chord.as_deref() == Some("<C-w>");
    let binding = take_binding(model, notation);
    if armed_before && !matches!(binding, Some(Resolved::Act(Action::Resize(_)))) {
        return vec![
            Effect::Rpc(RpcCall::Input {
                notation: "<C-w>".to_string(),
            }),
            Effect::Rpc(RpcCall::Input {
                notation: notation.to_string(),
            }),
        ];
    }
    super::ai::ai_panel_key(model, notation, binding)
}

/// The agent panel's key while it takes a window of its own, opened or
/// entered from anywhere else. Mirrors [`toggle_windowed_tree`]: the same
/// window is opened or entered by one message, so a doubled keypress cannot
/// open two windows.
pub(super) fn toggle_windowed_agent(model: &mut Model) -> Vec<Effect> {
    if model.focus() == Focus::Pane(NativeSurface::Agent) {
        return close_windowed_agent(model);
    }
    open_windowed_agent(model)
}

/// Opens the agent panel's window, or enters the one it already has.
/// `:View ai open`/`focus`'s own windowed path -- unlike
/// [`toggle_windowed_agent`], never closes what it finds, matching
/// [`open_ai_panel`]'s own floating contract.
pub(super) fn open_windowed_agent(model: &mut Model) -> Vec<Effect> {
    let mut effects = open_ai_panel(model);
    effects.push(Effect::Rpc(open_native_window(
        model,
        NativeSurface::Agent,
        true,
    )));
    effects
}

/// Closes the window the agent panel sits in. The session in
/// [`Model::ai_panel`] is untouched -- exactly what [`Model::close_ai_panel`]
/// already promises for the floating placement -- only the tile and its
/// claim go.
pub(super) fn close_windowed_agent(model: &mut Model) -> Vec<Effect> {
    let win = model.engine.grids().native_window(NativeSurface::Agent);
    if let Some(win) = win {
        model.engine.grids_mut().release_native_window(win);
    } else if model.surfaces.pending_open(NativeSurface::Agent) {
        // `:View ai close` reaches here whether or not the window it is
        // closing has been claimed yet -- unlike the toggle, which only
        // runs once focus proves it has (see `retile_open_surface`'s own
        // pending arm for why the open in flight has to be retired here)
        model.surfaces.cancel_pending_open(NativeSurface::Agent);
    }
    model.close_ai_panel();
    model.dirty = true;
    match win {
        Some(win) => vec![Effect::Rpc(RpcCall::CloseNativeWindow { win: win.0 })],
        None => Vec::new(),
    }
}

/// What an nvim-side close of a windowed surface's window owes the model:
/// the claim goes, the surface's state goes, and its scan worker is told.
///
/// nvim closes such a window on `:q` inside it, `<C-w>c`, or `:only` from
/// another window, and view hears nothing but `win_close` and
/// `grid_destroy` for it. Left unhandled, the tree's state stayed on the
/// overlay stack invisible, its scan worker kept walking, and the next
/// `<leader>e` reopened the window on a listing as old as the first open.
/// The agent panel's session in [`Model::ai_panel`] is never torn down here,
/// for the same reason `close_ai_panel` never tears it down: the sidebar's
/// visibility and the session's lifetime are independent by design.
pub(super) fn native_window_closed(
    model: &mut Model,
    surface: NativeSurface,
    win: crate::events::WinHandle,
) -> Vec<Effect> {
    model.engine.grids_mut().release_native_window(win);
    model.dirty = true;
    // a window closed before its own `win_pos` ever placed it (open,
    // then closed again within the same round) would otherwise leave
    // `pending_open` stuck true with nothing left to clear it
    model.surfaces.clear_pending(surface);
    match surface {
        NativeSurface::Tree if model.close_tree() => vec![Effect::TreeClose],
        NativeSurface::Agent => {
            model.close_ai_panel();
            Vec::new()
        }
        NativeSurface::Notifications => {
            model.close_message_history();
            Vec::new()
        }
        // the palette's window is a paint target for state nvim owns
        // (`Model::engine.cmdline`), never a thing view opened or closed on
        // the user's behalf -- releasing the claim above is the whole of
        // what a lost window owes it, and the next keystroke's own
        // `CmdlineShow` reopens a tile if the placement still wants one
        NativeSurface::Palette | NativeSurface::Tree => Vec::new(),
    }
}

/// What a window view opened for a surface holding something else owes the
/// model: everything an nvim-side close owes it, minus the close.
///
/// nvim gives the window to whatever asked for it -- `:edit` typed in the
/// tree, a quickfix jump, a plugin autocommand -- and sends no event view
/// can read as "that window is no longer yours". The claim stays, the pane
/// keeps painting the surface's rows over the file the person is now
/// reading, and only the next toggle takes them off. The Lua that notices
/// the buffer arrive hands the window's look back and reports it here.
pub(super) fn native_window_taken(model: &mut Model, surface: NativeSurface) -> Vec<Effect> {
    let Some(win) = model.engine.grids().native_window(surface) else {
        return Vec::new();
    };
    native_window_closed(model, surface, win)
}

/// Carries a resized sidebar's stepped share to every other windowed
/// surface pinned to the same edge, in the model alone -- no RPC of its
/// own.
///
/// Two windowed surfaces sharing an anchor are stacked one above the other
/// inside the same nvim column (`split = "below"` opens the second inside
/// the first's own window, per the open chunk's stacking rule), so
/// `nvim_win_set_width`/`nvim_win_set_height` on either one already resizes
/// the whole column nvim's side; what would otherwise drift is the two
/// surfaces' own `layout.size`, each written only by its own resize key.
/// Left unsynced, a ring step or a later resize of the sibling reads its
/// stale share and asks nvim for a size the column is not actually at.
///
/// The sibling's own loose copy of its share (`tree_width_pct` or
/// `ai_panel_width_pct` -- whichever `NativeSurface` it is) is written back
/// too, the same field its own resize key steps: left at the pre-sync
/// value, a later overlay resize of the sibling would read that stale
/// number and step from it instead of from the share this sync just gave
/// it, silently discarding the carry the moment the sibling floats.
fn sync_stacked_siblings(model: &mut Model, resized: NativeSurface, anchor: Anchor, size: u16) {
    for surface in NativeSurface::ALL {
        if surface == resized || !model.surfaces.windowed(surface) {
            continue;
        }
        let sibling = model.surfaces.layout(surface);
        if sibling.anchor != anchor {
            continue;
        }
        model.surfaces.set_layout(
            surface,
            crate::native::geometry::SurfaceLayout::new(sibling.placement, sibling.anchor, size),
        );
        match surface {
            NativeSurface::Tree => model.tree_width_pct = size,
            NativeSurface::Agent => model.ai_panel_width_pct = size,
            NativeSurface::Notifications | NativeSurface::Palette => {}
        }
    }
}

/// Carries the share the resize keys just stepped to the window the tree
/// sits in, in the cells it works out to against the grid nvim lays its
/// windows in.
///
/// The stepped share is written back into the layout whether or not the
/// tree is windowed right now: `layout.size` is what a later ring step
/// reads to open the window at, so a width stepped while the tree floats
/// must already be there when that step arrives, not just in
/// `tree_width_pct`'s own copy (which `resize_tree` has already re-widthed
/// the open float from). Only the live `SetWindowSize` RPC is a windowed
/// surface's own.
fn resize_windowed_tree(model: &mut Model) -> Vec<Effect> {
    let layout = model.surfaces.layout(NativeSurface::Tree);
    let stepped = model.tree_width_pct;
    model.surfaces.set_layout(
        NativeSurface::Tree,
        crate::native::geometry::SurfaceLayout::new(layout.placement, layout.anchor, stepped),
    );
    sync_stacked_siblings(model, NativeSurface::Tree, layout.anchor, stepped);
    if !model.tree_is_windowed() {
        return Vec::new();
    }
    let Some(win) = model.engine.grids().native_window(NativeSurface::Tree) else {
        return Vec::new();
    };
    let columns = model.engine.grids().global().size().0;
    let cells = crate::native::geometry::share(columns, stepped).max(1);
    vec![Effect::Rpc(RpcCall::SetWindowSize {
        win: win.0,
        width: Some(cells),
        height: None,
    })]
}

/// [`resize_windowed_tree`], for the agent panel: carries the share
/// `resize_ai_panel` just stepped to the layout, windowed or not, and to
/// the window it sits in when it is. `pub(super)` rather than private: the
/// resize key lives on `update::ai::ai_panel_key`'s own composer match,
/// not here, the same split `tree_key`'s own module keeps for the sidebar.
pub(super) fn resize_windowed_agent(model: &mut Model) -> Vec<Effect> {
    let layout = model.surfaces.layout(NativeSurface::Agent);
    let stepped = model.ai_panel_width_pct;
    model.surfaces.set_layout(
        NativeSurface::Agent,
        crate::native::geometry::SurfaceLayout::new(layout.placement, layout.anchor, stepped),
    );
    sync_stacked_siblings(model, NativeSurface::Agent, layout.anchor, stepped);
    if !model.agent_is_windowed() {
        return Vec::new();
    }
    let Some(win) = model.engine.grids().native_window(NativeSurface::Agent) else {
        return Vec::new();
    };
    let columns = model.engine.grids().global().size().0;
    let cells = crate::native::geometry::share(columns, stepped).max(1);
    vec![Effect::Rpc(RpcCall::SetWindowSize {
        win: win.0,
        width: Some(cells),
        height: None,
    })]
}

/// [`resize_windowed_tree`]/[`resize_windowed_agent`], for the notification
/// stream and ticker: steps `layout.size` one notch and carries it to the
/// window nvim already opened for it.
///
/// The tree and the agent panel each keep a second copy of their share
/// (`tree_width_pct`/`ai_panel_width_pct`) because the same number also
/// sizes their floating placement's `OverlayBox`. The stream's floating
/// placement never reads `layout.size` at all -- `open_message_history`
/// opens the history overlay at a fixed `OverlayBox::new(70, 60)`, the same
/// box on every open -- so `model.surfaces`'s own layout is the only copy
/// of this surface's share there is, and this steps it directly rather than
/// stepping a second field and copying it across the way the sidebars do.
///
/// The axis follows the anchor: a left or right tile resizes in columns,
/// top or bottom in rows, per [`crate::native::geometry::SurfaceLayout`]'s
/// own doc for what `size` percent means at each edge.
pub(super) fn resize_windowed_stream(model: &mut Model, widen: bool) -> Vec<Effect> {
    if !model.notifications_is_windowed() {
        return Vec::new();
    }
    let layout = model.surfaces.layout(NativeSurface::Notifications);
    let stepped = crate::native::geometry::step_panel_width(layout.size, widen);
    if stepped == layout.size {
        return Vec::new();
    }
    model.surfaces.set_layout(
        NativeSurface::Notifications,
        crate::native::geometry::SurfaceLayout::new(layout.placement, layout.anchor, stepped),
    );
    sync_stacked_siblings(model, NativeSurface::Notifications, layout.anchor, stepped);
    let Some(win) = model
        .engine
        .grids()
        .native_window(NativeSurface::Notifications)
    else {
        return Vec::new();
    };
    let (columns, rows) = model.engine.grids().global().size();
    let vertical = WinSplit::for_anchor(layout.anchor).is_vertical();
    let cells =
        crate::native::geometry::share(if vertical { columns } else { rows }, stepped).max(1);
    vec![Effect::Rpc(RpcCall::SetWindowSize {
        win: win.0,
        width: vertical.then_some(cells),
        height: (!vertical).then_some(cells),
    })]
}

/// Closes the window the tree sits in and drops its state.
///
/// The claim goes here rather than on the `win_close` the close produces,
/// because nvim can refuse the close and then send nothing: `:only` from
/// inside the tree leaves its window the last one, and `nvim_win_close`
/// answers E444. Waiting for an event that never comes left the handle
/// claimed for the rest of the session, and the next window nvim gave that
/// number read as a surface of view's own. Releasing a handle whose window
/// survives costs one repaint as an ordinary window; the release the
/// `win_close` path does is a `retain` and stays a no-op.
fn close_windowed_tree(model: &mut Model) -> Vec<Effect> {
    let win = model.engine.grids().native_window(NativeSurface::Tree);
    if let Some(win) = win {
        model.engine.grids_mut().release_native_window(win);
    } else if model.surfaces.pending_open(NativeSurface::Tree) {
        // a close reached before the open it is closing was ever claimed
        // retires that open here (see `retile_open_surface`'s own pending
        // arm for why), rather than leaving it to land and claim a window
        // this close already said it did not want
        model.surfaces.cancel_pending_open(NativeSurface::Tree);
    }
    let closed = model.close_tree();
    model.dirty = true;
    let mut effects = Vec::new();
    if closed {
        effects.append(&mut vec![Effect::TreeClose]);
    }
    if let Some(win) = win {
        effects.append(&mut vec![Effect::Rpc(RpcCall::CloseNativeWindow {
            win: win.0,
        })]);
    }
    effects
}

/// The call that opens `surface`'s window, or enters the one it already
/// has, at the anchor and size this session resolved for it.
///
/// Never called for [`NativeSurface::Palette`]: a windowed palette
/// carries no window of nvim's own, so [`retile_open_surface`] returns
/// before reaching here for it and nothing else asks this function to open
/// one.
///
/// `enter` is `true` for a call a user's own toggle key made -- they asked
/// to go there -- and `false` for a ring step carrying an already-open
/// surface to `windowed`, which is not a place the keyboard should move to.
pub(super) fn open_native_window(
    model: &mut Model,
    surface: NativeSurface,
    enter: bool,
) -> RpcCall {
    let layout = model.surfaces.layout(surface);
    RpcCall::OpenNativeWindow {
        surface,
        split: WinSplit::for_anchor(layout.anchor),
        size: layout.size,
        generation: model.surfaces.next_generation(surface),
        enter,
    }
}

/// The tree's own state on the overlay stack, with the scans its first
/// frame needs. Shared by both placements: the state is the same either
/// way, and only what draws it differs.
///
/// The box takes the anchor `[ui.surfaces.tree]` names, and its share from
/// `tree_width_pct`, which that table's `size` seeds and the resize keys
/// step. `beneath_top` puts the tree under the overlay already on top,
/// which is what a tree opened while a prompt is blocking owes that prompt.
fn open_tree_state(model: &mut Model, beneath_top: bool) -> Vec<Effect> {
    let mut state = crate::native::tree::TreeState::open(model.cwd.clone());
    let scan_generation = state.generation();
    // a freshly opened `TreeState` has never had a refresh in flight, so
    // this always allocates rather than coalescing -- the `Option` is
    // still handled rather than assumed, so a future change to `open`'s
    // initial state cannot silently turn this into a missing git scan
    let git_generation = state.request_git_refresh();
    // the tree takes a side, never a band: both config layers accept only
    // `left` and `right` for it, so the box is a share of the width at full
    // height whichever side the anchor names
    let anchor = model.surfaces.layout(NativeSurface::Tree).anchor;
    let geometry = OverlayBox::new(model.tree_width_pct, 100).with_anchor(anchor);
    if beneath_top {
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
/// A reply for a generation older than the one `surface`'s own last open
/// carried names a window a close or a retile has since moved the surface
/// past ([`crate::native::placement::SurfaceState::cancel_pending_open`]):
/// nothing claims it, and closing it here is the only place left that
/// still knows which handle to name, or it would sit open in nvim with
/// nothing on view's side ever pointing at it again. Read off `surface`'s
/// own counter, never a counter every surface shares -- a ring step that
/// opens two surfaces in one fold issues two calls before either reply
/// lands, and a shared counter would answer only the second (see
/// [`crate::native::placement::SurfaceState::generation`]'s doc).
///
/// An open onto an edge a windowed sibling already holds sets the shared
/// nvim column's width to this surface's own size exactly as the open
/// chunk's stacking branch does (`nvim_win_set_width` on either window in
/// the column resizes both), so this reply carries the same sync a resize
/// key carries: [`sync_stacked_siblings`] runs here too, or the sibling's
/// `layout.size`/`tree_width_pct`/`ai_panel_width_pct` reads a share the
/// column is no longer at until its own resize key happens to run.
pub(super) fn native_window_opened(
    model: &mut Model,
    generation: u64,
    surface: NativeSurface,
    win: crate::events::WinHandle,
) -> Vec<Effect> {
    if generation != model.surfaces.generation(surface) {
        return vec![Effect::Rpc(RpcCall::CloseNativeWindow { win: win.0 })];
    }
    // a re-enter of a window the open chunk's `is_ours(live)` branch found
    // already open hands back that same handle instead of opening a new
    // one, and nvim never fires a fresh `win_pos` for a window whose
    // position has not moved -- read before the claim below overwrites it,
    // since that is what tells this reply apart from a genuine new open
    let reentered = model.engine.grids().native_window(surface) == Some(win);
    // `pending_open` stays true past this claim -- it is what
    // `native_window()` will answer once the `win_pos` this claim is
    // waiting on places it, and that is a separate redraw event, not
    // this reply. Clearing it here reopens the same gap the flag exists
    // to close: a keystroke landing between this claim and that `win_pos`
    // would read "no window yet" and "nothing pending" and open a second
    // one. `ui_event::WinPos`'s handler clears it once the placement the
    // guard is actually waiting for has happened -- except on a re-enter,
    // where that event is never coming and this is the only place left
    // that can still tell `pending_open` the wait is over.
    model.engine.grids_mut().claim_native_window(win, surface);
    let layout = model.surfaces.layout(surface);
    sync_stacked_siblings(model, surface, layout.anchor, layout.size);
    if reentered {
        model.surfaces.clear_pending(surface);
    }
    model.dirty = true;
    Vec::new()
}

/// An `OpenNativeWindow` call answered with an error rather than a window
/// handle: nothing to claim, but `pending_open` still has to clear, or a
/// guard gated on it refuses every later open of `surface` for the rest of
/// the session.
pub(super) fn native_window_open_failed(
    model: &mut Model,
    generation: u64,
    surface: NativeSurface,
) -> Vec<Effect> {
    if generation == model.surfaces.generation(surface) {
        model.surfaces.clear_pending(surface);
    }
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
    // `Anchor::Right` was hard-coded here, so `[ui.surfaces.agent]
    // anchor` had no effect on the overlay placement the vast majority of
    // sessions actually run -- only a windowed open read it. The overlay
    // now opens at whichever edge the surfaces table (or its default)
    // names, the same anchor a windowed open already resolves.
    let geometry = OverlayBox::new(model.ai_panel_width_pct, 100)
        .with_anchor(model.surfaces.layout(NativeSurface::Agent).anchor);
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
    if model.agent_is_windowed() {
        return toggle_windowed_agent(model);
    }
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

/// The windowed notification stream's own key handling, while its pane
/// holds the cursor (`Focus::Pane(NativeSurface::Notifications)`).
///
/// `<Esc>` leaves the tile for the previous window, the same as the tree's
/// and the agent panel's own ([`agent_pane_key`]'s own `<Esc>` arm), checked
/// first for the same reason: past it, the resize keys are resolved through
/// the same shared [`take_binding`] the sidebars use, so
/// `<S-Right>`/`<S-Left>` (or a rebound chord) resize the stream's own tile
/// exactly as they resize the tree's; every other resolution falls through
/// unchanged. Past that, [`history_mut`] reaches the same
/// `MessageHistoryState` a float would, so [`message_history_key`]'s own
/// dispatch answers every key here exactly as it does for the floating
/// overlay -- the pane is a placement, not a different feature.
pub(super) fn notifications_pane_key(model: &mut Model, notation: &str) -> Vec<Effect> {
    if notation == "<Esc>" {
        return vec![Effect::Rpc(RpcCall::FocusPreviousWindow)];
    }
    // `<C-w>` opens a real nvim window-command prefix, and this pane's own
    // resize chord (`<C-w>>`/`<C-w><`) is only two of the followers nvim
    // itself answers (`w`, `s`, `q`, ...). The prefix is held here, not
    // forwarded on arming: nvim must never be told about a `<C-w>` whose
    // follower turns out to be this build's own resize chord, so the
    // prefix and its follower reach nvim together, and only once the
    // follower is known to belong to nvim rather than this pane.
    let armed_before = model.pending_chord.as_deref() == Some("<C-w>");
    match take_binding(model, notation) {
        Some(Resolved::Act(Action::Resize(direction))) => {
            return resize_windowed_stream(model, direction.widens());
        }
        Some(Resolved::Pending) => return Vec::new(),
        _ => {}
    }
    if armed_before {
        return vec![
            Effect::Rpc(RpcCall::Input {
                notation: "<C-w>".to_string(),
            }),
            Effect::Rpc(RpcCall::Input {
                notation: notation.to_string(),
            }),
        ];
    }
    message_history_key(model, notation)
}

/// `<leader>fm`/`:View notifications history`: opens the floating history
/// overlay under the float placement, [`toggle_tree_sidebar`]'s own split.
/// Under the windowed placement, opens the stream's own tile when none is
/// claimed and closes it when one is, mirroring
/// [`toggle_windowed_tree`]/[`toggle_windowed_agent`].
pub(super) fn toggle_notifications_stream(model: &mut Model) -> Vec<Effect> {
    if !model.notifications_is_windowed() {
        if model.close_message_history() {
            model.dirty = true;
            return Vec::new();
        }
        return open_message_history(model);
    }
    if model.focus() == Focus::Pane(NativeSurface::Notifications) {
        return close_windowed_notifications(model);
    }
    open_windowed_notifications(model)
}

/// Opens the notification stream's window, claiming a pane the same way
/// [`open_windowed_agent`] does for the agent panel: the state already
/// lives on the overlay stack once open (kept live every fold by
/// `Model::refresh_message_history`), so opening a window is nothing more
/// than seating that state, if it is not already there, plus the claim.
fn open_windowed_notifications(model: &mut Model) -> Vec<Effect> {
    if history(model).is_none() {
        let state = MessageHistoryState::snapshot(&model.engine.toast_history);
        model.push_overlay(OverlayBox::new(70, 60), OverlayKind::MessageHistory(state));
    }
    vec![Effect::Rpc(open_native_window(
        model,
        NativeSurface::Notifications,
        true,
    ))]
}

/// Closes the window the notification stream sits in and drops its state,
/// mirroring [`close_windowed_agent`].
fn close_windowed_notifications(model: &mut Model) -> Vec<Effect> {
    let win = model
        .engine
        .grids()
        .native_window(NativeSurface::Notifications);
    if let Some(win) = win {
        model.engine.grids_mut().release_native_window(win);
    } else if model.surfaces.pending_open(NativeSurface::Notifications) {
        // a close reached before the open it is closing was ever claimed
        // retires that open here (see `retile_open_surface`'s own pending
        // arm for why), rather than leaving it to land and claim a window
        // this close already said it did not want
        model
            .surfaces
            .cancel_pending_open(NativeSurface::Notifications);
    }
    model.close_message_history();
    model.dirty = true;
    match win {
        Some(win) => vec![Effect::Rpc(RpcCall::CloseNativeWindow { win: win.0 })],
        None => Vec::new(),
    }
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
    // A windowed tree's own `<C-w>` chord is held the same way the
    // notification stream's is (see `notifications_pane_key`): the prefix
    // waits here rather than reaching nvim on arming, so a follower this
    // build resolves as its own resize never leaves nvim mid-chord.
    let armed_before = model.tree_is_windowed() && model.pending_chord.as_deref() == Some("<C-w>");
    // Ahead of the tree's own keys and resolved through the one
    // shared set, so neither sidebar can drift onto a key the
    // other does not answer (see [`take_binding`]).
    match take_binding(model, notation) {
        Some(Resolved::Act(Action::Resize(direction))) => {
            if !model.resize_tree(direction.widens()) {
                return Vec::new();
            }
            model.dirty = true;
            return resize_windowed_tree(model);
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
    if armed_before {
        return vec![
            Effect::Rpc(RpcCall::Input {
                notation: "<C-w>".to_string(),
            }),
            Effect::Rpc(RpcCall::Input {
                notation: notation.to_string(),
            }),
        ];
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
        // opened through RPC, since nvim owns the buffer
        // this creates. A floating sidebar closes on the
        // same keypress, matching a picker selection's own
        // close-on-open behavior, and a windowed one stays,
        // the way a tiled sidebar does everywhere else
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
    let rows = if model.notifications_is_windowed() {
        model
            .engine
            .grids()
            .native_window_size(NativeSurface::Notifications)
            .map_or(0, |(_, height)| height)
    } else {
        model.focused_overlay().map_or(0, |overlay| {
            model
                .overlay_rect(overlay)
                .height
                .saturating_sub(HISTORY_CHROME_ROWS)
        })
    };
    isize::try_from(rows.div_ceil(2))
        .unwrap_or(isize::MAX)
        .max(1)
}

/// The open history overlay's state, wherever it sits: on top of the
/// float's own stack question ([`Model::focused_overlay`]) while it draws
/// as one, or scanned directly off [`Model::overlays`] once it is windowed
/// -- [`Model::focused_overlay`] skips a windowed `MessageHistory` on
/// purpose (see `model/focus.rs`'s `takes_focus_now`), since its keys route
/// through `Focus::Pane(Notifications)` instead of the overlay stack's own
/// dispatch.
fn history(model: &Model) -> Option<&MessageHistoryState> {
    model
        .overlays()
        .iter()
        .find_map(|overlay| match &overlay.kind {
            OverlayKind::MessageHistory(state) => Some(state),
            _ => None,
        })
}

/// [`history`], for the keys that move the selection.
fn history_mut(model: &mut Model) -> Option<&mut MessageHistoryState> {
    model
        .overlays_mut()
        .iter_mut()
        .find_map(|overlay| match &mut overlay.kind {
            OverlayKind::MessageHistory(state) => Some(state),
            _ => None,
        })
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
