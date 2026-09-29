//! The pure state transition: `Msg` in, `Model` mutated, `Effect`s out.

use crate::model::{Focus, Model, OverlayKind, Tier};
use crate::msg::{DeleteConfirmOutcome, Effect, EngineRequest, Key, Msg, RpcCall};
use crate::native::diff::BufTextChangedEvent;
use crate::native::geometry::NativeSurface;
use crate::native::supervision::WedgeKind;
use crate::native::toast::ToastMotion;
use crate::native::views::Span;

/// How long a session parks foreign startup messages before giving up on
/// hearing whether a channel it owns is held and letting them through.
///
/// The deadline rather than the schedule: the message area's own channel
/// is read in the first hundred milliseconds of an ordinary launch and the
/// hold resolves there. This bounds the launch where it is never read at
/// all -- a config that errors out before `VimEnter`, a plugin manager
/// that blocks on a network install -- so the hold cannot silently swallow
/// a message.
const STARTUP_HOLD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// When the last of a holder's startup complaints was sighted, measured
/// from launch on the heavy compat fixture (`unaccommodated`,
/// `VIEW_COMPAT_LOG`): the config re-runs its health check on a one-second
/// interval and raises the one about view holding `vim.notify` on the
/// fifth cycle, past any realistic first keystroke. The channel reading
/// that arms the grace landed within half a second of that same launch, so
/// nearly the whole grace was spent by the time the complaint arrived.
const COMPLAINT_RAISE_MEASURED: std::time::Duration = std::time::Duration::from_millis(4960);

/// The checker interval the measured config runs its health check on: the
/// unit a raise moves in when the launch it rides on stretches.
const HOLDER_CHECKER_CYCLE: std::time::Duration = std::time::Duration::from_secs(1);

/// How much later than measured a raise is allowed to land and still be
/// taken down. The two ends run on different clocks -- the grace is
/// anchored to view's own reading of the channel, the raise to how long
/// the config took to load and tick -- and config loading is what a slow
/// runner stretches, so the margin is a multiple of the raise rather than
/// a cycle or two added to it.
const COMPLAINT_RAISE_SLACK: u32 = 2;

/// How long after a held channel is found view still takes that holder's
/// complaints down, once the user has acted.
///
/// Derived from the measured raise rather than written down, so the margin
/// is on the record: twice [`COMPLAINT_RAISE_MEASURED`], which leaves about
/// five of the config's own checker cycles past the raise on the host it
/// was measured on. Long enough for a stretched launch, short enough that
/// a window the user opens minutes later is outside it; what bounds the
/// take-down inside it is the complaint signature, not the clock.
const COMPLAINT_GRACE: std::time::Duration =
    COMPLAINT_RAISE_MEASURED.saturating_mul(COMPLAINT_RAISE_SLACK);

// tied at compile time rather than by comment: a grace shrunk below the
// measured raise plus one checker cycle would leave the complaint standing
// on the host it was measured on, and fail as a remote-leg flake
const _: () = assert!(
    COMPLAINT_GRACE.as_millis()
        >= COMPLAINT_RAISE_MEASURED.as_millis() + HOLDER_CHECKER_CYCLE.as_millis(),
    "COMPLAINT_GRACE must outlast the measured raise by a checker cycle"
);

mod ai;
mod ai_fs;
mod bridge;
pub(crate) mod look;
mod mouse;
mod paste;
pub(super) mod review;
mod route;
mod supervision;
mod surface_conflict;
pub(crate) mod surfaces;
mod theme;
mod ui_event;
mod watch;

use ai::{on_ai_event, open_ai_trust_prompt};
use paste::{paste_into_agent_composer, paste_into_focused_surface};
use route::{
    ai_scroll_for, dismiss_top_prompt, picker_query, reaches_past_a_panel_owner,
    scroll_ai_transcript,
};
use supervision::{note_engine_liveness, note_supervision_choice, restarts_at_standing_wedge};
pub use surface_conflict::FLOAT_SCAN_THROTTLE;
use surfaces::{
    message_history_key, notice_ai_disabled, notice_clipboard_unavailable, open_ai_panel,
    open_picker, picker_preview_request, picker_source_for_verb, toggle_ai_panel,
    toggle_notifications_stream, toggle_tree_sidebar, tree_git_refresh_effect,
};
use theme::{on_colorscheme_missing, on_vim_enter};
use ui_event::apply_ui_event;
use watch::{
    on_checktime_reply, on_confirm_external_removal, on_external_watch_degraded,
    on_external_writes_detected,
};

/// Converts a filesystem path to the UTF-8 string an `RpcCall` path field
/// carries, substituting the replacement character for any byte sequence
/// that is not valid UTF-8 rather than failing: nvim's own path arguments
/// are untyped strings, so a lossy round-trip here matches the contract
/// every wire path already accepts.
fn path_to_wire(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Closes, in the model, every surface view had open in a window of the
/// engine being replaced, returning what each close owes the executor.
///
/// The replacement has none of those windows, so a surface left reading
/// open would hold `Focus::Pane`, its ring position and its open flag for a
/// window nobody can reach. Runs before the grids are forgotten, since the
/// claims are the only record of which surfaces were windowed. Each closed
/// surface opens again at the replacement's `VimEnter`, the way a floating
/// one stays open across the restart.
#[must_use]
pub fn forget_native_windows(model: &mut Model) -> Vec<Effect> {
    // runs outside `update`, so its focus comparison never sees the
    // surfaces this closes
    model.submit_hold.take_sequence();
    surfaces::forget_native_windows(model)
}

/// Adds what a launch handed to view (the features it draws, the user keys
/// it maps) to the launch's one notice, returning what raising it owes the
/// executor.
///
/// Each item carries its first-run record key, and one the record already
/// holds for this config ([`Model::seed_announced`]) is left out. The
/// caller writes the record.
#[must_use]
pub fn tell_taken_over(
    model: &mut Model,
    taken: Vec<(String, crate::native::surfaces::Taken)>,
) -> Vec<Effect> {
    surface_conflict::on_taken_over(model, taken)
}

/// Applies one message to `model`, returning the effects the executor must
/// carry out. Never blocks and never performs I/O: every side effect crosses
/// the boundary as a returned [`Effect`] instead of being performed here.
#[must_use]
pub fn update(model: &mut Model, msg: Msg) -> Vec<Effect> {
    let Some(msg) = model.submit_hold.hold(msg) else {
        return Vec::new();
    };
    let releases = model.submit_hold.releases(&msg);
    let mut effects = update_one(model, msg);
    // replayed after the command has run, so the focus it set routes them.
    // A replayed `:View` submit arms a fresh hold, which the rest are then
    // kept behind in order
    if releases {
        for held in model.submit_hold.take_held() {
            effects.extend(update(model, held));
        }
    }
    effects
}

fn update_one(model: &mut Model, msg: Msg) -> Vec<Effect> {
    // any input during an animation completes it on that frame (the spec's
    // interruptible rule): the model is already in the state the motion is
    // interpolating toward, so jumping to the last frame paints that state
    // and nothing waits on choreography the user has moved past. A key that
    // itself dismisses a notice still starts that notice's own motion at the
    // tail below -- that is the next motion, not this one continuing.
    if matches!(msg, Msg::Key(_) | Msg::Mouse(_) | Msg::Paste(_)) {
        if let Some(motion) = model.toast_motion.as_mut() {
            model.dirty |= motion.complete();
        }
        // one definition of "the user has acted": the same three messages
        // that finish a motion close the startup conflict window, so a
        // click before any key cannot leave view holding a licence to take
        // a window down
        model.surface_conflicts.note_user_acted();
        // and the conflict notice's own way down for a user who is typing
        // rather than reaching for `<Esc>`: only once it has stood the
        // window a transient one gets, so the key pressed while it is still
        // being read leaves it alone.
        //
        // Every input that reaches the editor, which is what excludes the
        // one the busy modal eats: a key answering that modal is not a
        // user reading past a notice behind it, and taking both with one
        // keystroke spends an answer they made on a dismissal they did not
        // (`route_key`'s own exclusion, for the same reason).
        if model.engine_busy().is_none() {
            model.dirty |= model.engine.messages.dismiss_read_sticky();
        }
    }
    // both taken ahead of the message: the count is what tells a notice
    // that left the stack from one nvim replaced in place, and the slot is
    // where the stack the user was looking at was actually drawing the
    // notice that is about to leave
    let entries_before = model.engine.messages.entries.len();
    let armed_before = model.armed_toast_slot();
    let chrome_before = model.chrome_rows();
    let focus_before = model.focus();
    let mut effects = dispatch(model, msg);
    // held keys belong to the surface they were typed on: a click, a
    // `:View` command, an agent's review or a window nvim closed can move
    // the keyboard with no key of the user's, and a sequence left standing
    // would be completed by the next key typed after focus comes back
    if model.focus() != focus_before {
        model.submit_hold.take_sequence();
    }
    // the top row follows facts a dozen arms move (a tabpage, the buffer
    // list, the agent's state, `showtabline`), so the one comparison lives
    // here: a row that appears or leaves resizes the grid once, and a look
    // flip has already asked for that resize itself
    if model.chrome_rows() != chrome_before {
        model.dirty = true;
        let resized = effects
            .iter()
            .any(|effect| matches!(effect, Effect::Rpc(RpcCall::TryResize { .. })));
        if !resized {
            let (width, height) = model.grid_target();
            effects.push(Effect::Rpc(RpcCall::TryResize { width, height }));
        }
    }
    // a docked float moves the part of each tile nvim's text is shown in,
    // so a tile it closes asks nvim to lay its text out there
    if model.follow_the_docks() {
        model.dirty = true;
        effects.append(&mut look::request_all(model));
    }
    let departed = model
        .engine
        .messages
        .departed_toast(entries_before, armed_before);
    // the windowed notification stream is a live view of the same ring the
    // floating stack reads (`Model::refresh_message_history`, below), so a
    // reader's attention already sitting in its pane is the one signal that
    // beats an idle timeout: entering it holds the stack open, leaving it
    // drops that hold, independent of any standing manual pause
    // (`Messages::set_pane_held`'s own doc). Idempotent, so this costs one
    // enum comparison on every fold that is not itself a focus transition.
    model.engine.messages.set_pane_held(matches!(
        model.focus(),
        Focus::Pane(NativeSurface::Notifications)
    ));
    // the toast stack's dismissal timer belongs to its top slot, and the
    // ways an entry leaves that slot are spread across a dozen arms below
    // -- an expiry, a keypress, a deliberate sticky dismissal, an
    // `msg_clear`, a startup hold releasing what it parked. Arming here
    // instead of at each of them is what makes "the promoted entry's timer
    // starts now" true on every one of those paths rather than on the ones
    // somebody remembered; `arm_top_slot` answers `None` when the slot has
    // not changed hands, which is nearly every message.
    effects.extend(model.engine.messages.arm_top_slot());
    effects.extend(start_toast_exit(model, departed));
    // the history overlay draws under the toast stack, so a notice raised
    // while it is open is neither on screen nor in the list unless the list
    // follows the ring
    model.dirty |= model.refresh_message_history();
    // the end the stack holds moves only here, and each notice is wrapped
    // here once for the column's width so a frame reads the wrap
    model.place_notices();
    effects
}

/// Starts the stack's exit motion for a notice that just left, and asks for
/// the first frame's wakeup.
///
/// The gate is the tier and nothing else: below `Tier::Full` there is no
/// interpolation at all, so no motion is started, no tick is ever scheduled,
/// and the stack paints the state it is already in. The slot timers and the
/// pause key are untouched either way -- motion is presentation, timing is
/// behavior.
fn start_toast_exit(model: &mut Model, departed: Option<(Vec<Vec<Span>>, usize)>) -> Vec<Effect> {
    let Some((lines, slot)) = departed else {
        return Vec::new();
    };
    if model.caps.tier != Tier::Full {
        return Vec::new();
    }
    // one wakeup chain at a time: a second dismissal landing while the
    // first is still playing replaces the motion and is driven by the chain
    // already running, because two chains ticking one clock advance it twice
    // per interval and the stack arrives in half the time it was given
    let already_ticking = model.toast_motion.is_some();
    let width = model.notice_column().rect.2;
    model.toast_motion = Some(ToastMotion::exit_right(lines, slot, width));
    model.dirty = true;
    if already_ticking {
        return Vec::new();
    }
    vec![Effect::ScheduleAnimTick {
        after: crate::native::toast::MOTION_STEP,
    }]
}

fn dispatch(model: &mut Model, msg: Msg) -> Vec<Effect> {
    // A chord prefix belongs to the sidebar it was armed in, and focus can
    // leave one with no key involved at all -- a review landing hands the
    // keyboard back to the buffer it is drawn in. Anything that reaches a
    // sidebar again is itself a message dispatched while focus is still
    // elsewhere, so dropping the prefix here is what keeps a `>` typed into
    // a prompt minutes later from spending itself on a width instead. The
    // `is_some` short-circuit keeps every keystroke that never armed one --
    // which is nearly all of them -- to a single null check.
    if model.pending_chord.is_some()
        && !matches!(
            model.focused_overlay().map(|overlay| &overlay.kind),
            Some(OverlayKind::Ai | OverlayKind::Tree(_))
        )
        // a windowed surface holds no overlay for the check above to find
        // (`focused_overlay` answers only `Focus::Native`), so a chord
        // armed inside a windowed tree, agent panel or stream needs its
        // own clause or `<C-w>>`/`<C-w><` could never resolve there at all
        && !matches!(
            model.focus(),
            Focus::Pane(NativeSurface::Tree | NativeSurface::Agent | NativeSurface::Notifications)
        )
    {
        model.pending_chord = None;
    }
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
            // read before the fold below, which closes the modal on a
            // dismissal: asked afterwards, every key that answered a modal
            // reads as one that arrived with none open
            let busy = model.engine_busy();
            // the restart key at a standing wedge is a request to view, and
            // no overlay answers it
            let answers_anywhere = busy.is_some_and(|open| open.kind == WedgeKind::Dead)
                || restarts_at_standing_wedge(model, &notation);
            let modal_was_open = busy.is_some();
            let mut effects = if answers_anywhere || model.focus() == Focus::Engine {
                note_supervision_choice(model, &notation)
            } else {
                Vec::new()
            };
            effects.extend(route::route_unescaped(model, notation, modal_was_open));
            effects
        }
        Msg::Paste(text) => match model.focus() {
            // never replayed as nvim_input keystrokes: one undo unit, no
            // mapping interference, matching nvim_paste's own contract.
            Focus::Engine => {
                vec![Effect::Rpc(RpcCall::Paste { text })]
            }
            // the windowed agent panel's overlay never claims focus (see
            // `Model::draws_as_overlay`'s doc), so `focused_overlay_mut`
            // would find nothing here -- the composer is reached directly
            // instead, the same text-delivery rule `paste_into_focused_surface`
            // applies to the floating panel
            Focus::Pane(NativeSurface::Agent) => paste_into_agent_composer(model, &text),
            // a surface answers a paste the same way in either placement.
            // The cursor a windowed surface leaves in nvim sits in the
            // scratch buffer its own window shows, so an `nvim_paste` here
            // reaches no buffer a person is editing.
            Focus::Pane(_) | Focus::Native(_) => paste_into_focused_surface(model, &text),
        },
        Msg::Mouse(input) => mouse::route(model, input),
        Msg::Redraw(events) => {
            let flushed = matches!(events.last(), Some(crate::events::UiEvent::Flush));
            let mut effects = Vec::new();
            for ev in events {
                effects.extend(apply_ui_event(model, ev));
            }
            model.engine.note_batch(flushed);
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
        // the history alone, never a toast: these lines were raised before
        // this session had a UI, and the notice that promises them is the
        // history's own (`EngineModel::seed_startup_history`)
        Msg::StartupMessages { text } => {
            model.dirty |= model.engine.seed_startup_history(&text);
            Vec::new()
        }
        // the window-local hold's own report, raised on the session's
        // window events rather than on the redraw path
        Msg::ChannelHeld { channel, holder } => {
            surface_conflict::on_channel_held(model, &channel, &holder)
        }
        // marks dirty unconditionally: the reading decides which entries
        // the stack paints at all, so a frame drawn before it and one drawn
        // after are different frames whichever way it lands
        Msg::NotifySinkRead { foreign } => {
            let settled = model.engine.messages.set_foreign_notifier(foreign);
            model.dirty = true;
            let mut effects: Vec<Effect> = settled
                .into_iter()
                .map(|text| Effect::Rpc(RpcCall::Notify { text }))
                .collect();
            effects.extend(surface_conflict::on_notify_sink_read(model, foreign));
            effects
        }
        // whichever of the two attach paths arrives first performs it, and
        // the other finds it done (see `Model::takes_attach`)
        Msg::AttachDeadline => model.takes_attach().map(Effect::Rpc).into_iter().collect(),
        // asked before this connection can say it has finished starting, and
        // deliberately: a startup error parks nvim ahead of its own
        // `VimEnter`, so a chain that waited for that event would never hear
        // about the recovery that failed. The reading is gated engine-side
        // and answers "nothing yet" until `VimEnter` has fired
        Msg::EngineAttached => vec![
            Effect::Rpc(RpcCall::ProbeSwapRecovery {
                generation: model.supervision.begin_swap_probe(),
            }),
            // armed from the attach rather than from the connection, which
            // exists a config's whole sourcing earlier: the hold is what
            // withholds a holder's startup messages from the screen, and
            // there is no screen to withhold them from until this arrives
            Effect::ScheduleStartupHold {
                after: STARTUP_HOLD_DEADLINE,
                generation: model.surface_conflicts.engine_generation(),
            },
        ],
        Msg::EngineDown(exit) => {
            model.running = false;
            vec![Effect::Quit {
                exit_code: exit.code.unwrap_or(1),
            }]
        }
        // `128 + signal` is the status a shell reports for a process a
        // signal ended, and reporting anything else would make view the one
        // command in a pipeline whose death cannot be read the usual way
        Msg::Terminated { signal } => {
            model.running = false;
            vec![Effect::Quit {
                exit_code: 128 + signal,
            }]
        }
        Msg::EngineRequest(EngineRequest::VimEnter { token, .. }) => on_vim_enter(model, token),
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
                token: Some(token),
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
            // the held slots fit the size the screen had
            model.engine.release_held_layout();
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
        Msg::EscapeTimeout(_) => {
            // the reader consumes this before the fold ever sees it; the arm
            // exists because a platform without a byte-level reader still
            // has to answer the variant, and the answer is a still frame
            Vec::new()
        }
        Msg::CapsUpgraded(caps) => {
            // an upgrade is only ever sent for capabilities that actually
            // changed, and the frame already on screen was painted at the
            // ones it replaces -- borders, palette and the synchronized
            // bracket all read off these
            model.caps = caps;
            model.dirty = true;
            Vec::new()
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
        Msg::AccentProbeReply {
            generation,
            function_fg,
            statement_fg,
        } => look::accent_reply(model, generation, function_fg, statement_fg),
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
            // A bare `:View <feature>` (no verb) means "just open it":
            // resolved to the feature's own first `default_maps()` entry
            // ahead of every gate below, so a trust prompt this triggers
            // (`open_ai_trust_prompt`) carries the resolved verb into its
            // pending re-dispatch instead of the empty one that used to
            // land back here as "needs a feature and a verb".
            let verb = if verb.is_empty() && !feature.is_empty() {
                crate::native::mappings::default_maps()
                    .iter()
                    .find(|spec| spec.feature == feature)
                    .map_or(verb, |spec| spec.verb.to_string())
            } else {
                verb
            };
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
                // a windowed panel has no "entered" state of its own to
                // claim (see `Model::takes_focus_now`'s `agent_windowed`
                // arm) -- entering its window is what nvim's own cursor
                // move already does, so "open" and "focus" both reduce to
                // opening or entering the tile
                if model.agent_is_windowed() {
                    return surfaces::open_windowed_agent(model);
                }
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
                if model.agent_is_windowed() {
                    return surfaces::close_windowed_agent(model);
                }
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
                return toggle_notifications_stream(model);
            }
            if feature == "palette" && verb == "open" {
                // opens the cmdline exactly as a typed `:` would; nvim's own
                // `CmdlineShow` answer (`UiEvent::CmdlineShow`) is what
                // marks the model dirty and opens the windowed tile, the
                // same path a hand-typed `:` already takes
                return vec![Effect::Rpc(RpcCall::Input {
                    notation: ":".to_string(),
                })];
            }
            if feature == "ui" && verb == "gaps" {
                return surfaces::toggle_gaps(model);
            }
            if feature == "ui" && verb == "cycle_surfaces" {
                return surfaces::cycle_placements(model);
            }
            if feature == "notifications" && verb == "pause" {
                // no notice of its own: raising one would push an entry onto
                // the very stack the key is freezing, and the top box's mark
                // is the feedback. The tail's `arm_top_slot` is what re-arms
                // the slot on the way back out.
                model.engine.messages.toggle_pause();
                model.dirty = true;
                return Vec::new();
            }
            if feature == "notifications" && verb == "dismiss" {
                // no notice of its own, on `pause`'s own terms above: the
                // entry leaving the stack is the feedback.
                if model.engine.messages.dismiss_newest() {
                    model.dirty = true;
                }
                return Vec::new();
            }
            if feature == "window" {
                match verb.as_str() {
                    "new" => return surfaces::window_new(model),
                    "zoom" => return surfaces::window_zoom(model),
                    "fit" => return surfaces::window_fit(model),
                    "flip" => return surfaces::window_flip(model),
                    "float" => return surfaces::window_float(model),
                    _ => {
                        if let Some(destination) = verb
                            .strip_prefix("to_tabpage_")
                            .and_then(|n| n.parse().ok())
                        {
                            return surfaces::window_to_tabpage(model, destination);
                        }
                    }
                }
            }
            // The open review's own vocabulary, arriving from the
            // buffer-local mappings `RpcCall::ReviewShow` installs on the
            // file under review (and from `:View review <verb>` typed by
            // hand, which is the always-available way to answer a review
            // whose maps did not install). No trust gate: a review only
            // exists because a proposal the user has already seen raised
            // one, and deciding it launches nothing.
            if feature == "review" {
                return review::review_verb(model, &verb);
            }
            // the one form whose first token is not a feature id: the look
            // is a session-wide setting with no surface of its own, so it
            // has no registry row for the dispatch above to match
            if feature == "ui" {
                return look::invoke(model, &verb);
            }
            // `keys` names the session's whole key vocabulary and no
            // registry feature: this pure crate only records the flip on
            // the model, since restoring the previous registration and
            // reissuing the new one is engine I/O only `NativeSession`
            // (view/src/native.rs, outside this crate) can perform.
            if feature == "keys" {
                return keys_invoke(model, &verb);
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
                let notation = format!(":{} ", crate::native::mappings::COMMAND);
                // the reopened line is folded as if typed, so the keys
                // typed on it and behind its `<CR>` are read the same way
                let mut folded = Vec::new();
                for key in notation.chars() {
                    folded.extend(crate::native::submit_hold::fold_engine_key(
                        model,
                        &key.to_string(),
                    ));
                }
                effects.push(Effect::Rpc(RpcCall::Input { notation }));
                effects.extend(folded);
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
        reading @ Msg::SwapRecovered { .. } => supervision::note_swap_recovery(model, reading),
        Msg::MappingsClaimed {
            claimed,
            colon_mapped,
            ..
        } => {
            model.submit_hold.learn_invoke_keys(&claimed);
            model.record_claimed_keys(claimed);
            model.record_colon_mapped(colon_mapped);
            Vec::new()
        }
        Msg::ColonMappingRead { mapped } => {
            model.record_colon_mapped(mapped);
            Vec::new()
        }
        Msg::UserMappingsRead {
            keys,
            timeoutlen,
            cmdline,
        } => {
            model.submit_hold.learn_user_keys(&keys, timeoutlen);
            model.submit_hold.learn_cmdline_maps(&cmdline);
            Vec::new()
        }
        Msg::SequenceExpired { generation } => route::expire_sequence(model, generation),
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
        Msg::ColorSchemeMissing { name } => on_colorscheme_missing(model, &name),
        Msg::DiagnosticsChanged { errors, warnings } => {
            bridge::on_diagnostics(model, errors, warnings)
        }
        Msg::GitBranchChanged { branch } => bridge::on_git_branch(model, branch),
        Msg::BufferChanged {
            name,
            modified,
            filetype,
        } => bridge::on_buffer(model, name, modified, filetype),
        Msg::WindowStatus { win, status } => bridge::on_window_status(model, win, status),
        Msg::BufferList { buffers } => bridge::on_buffer_list(model, buffers),
        Msg::ShowTablineChanged { value } => bridge::on_showtabline(model, value),
        Msg::MinPaneSizeChanged { width, height } => bridge::on_min_pane_size(model, width, height),
        Msg::FloatObserved(float) => surface_conflict::observe_float(model, &float),
        Msg::FloatSweep => surface_conflict::sweep_floats(model),
        Msg::FloatRows { win, lines } => surface_conflict::on_float_rows(model, win, lines),
        Msg::ComplaintGraceExpired { generation } => {
            model.surface_conflicts.end_complaint_grace(generation);
            Vec::new()
        }
        Msg::StartupHoldExpired { generation } => {
            model.dirty |= model.expire_startup_hold(generation);
            Vec::new()
        }
        Msg::LayoutHoldExpired { generation } => {
            if generation == model.surface_conflicts.engine_generation() {
                model.engine.release_held_layout();
                model.dirty = true;
            }
            Vec::new()
        }
        // the input hold belongs to the binary's native session, which
        // reads this message at the same dispatch
        Msg::ChordHoldExpired { .. } => Vec::new(),
        // `update` releases the hold around this dispatch
        Msg::SubmitHoldExpired { .. } => Vec::new(),
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
        Msg::ConfirmExternalRemoval { path } => on_confirm_external_removal(model, path),
        Msg::ExternalWatchDegraded { reason } => on_external_watch_degraded(model, reason),
        Msg::ClipboardUnavailable => notice_clipboard_unavailable(model),
        // Through the same notice channel `on_external_watch_degraded`
        // uses, and for the same reason it does rather than the panel's own
        // banner: the panel may not even be open when a session starts, and
        // this is the one thing the user needs told before a wait nothing
        // else on screen explains.
        Msg::AiProvisioning { detail } => {
            model.dirty = true;
            model.engine.record_native_notice(detail, false)
        }
        // `request_id` reaches the fold because one probe is not like the
        // others: the confirming second look at a vanished path
        // (`Msg::ConfirmExternalRemoval`) records the id it minted, and only
        // the reply wearing that id may announce the removal. Every other
        // disposition still comes out of `CheckTimeOutcome` alone
        Msg::CheckTimeReply {
            request_id,
            results,
        } => on_checktime_reply(model, request_id, results),
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
        Msg::NativeWindowOpened {
            generation,
            surface,
            win,
        } => surfaces::native_window_opened(model, generation, surface, win),
        Msg::NativeWindowOpenFailed {
            generation,
            surface,
        } => surfaces::native_window_open_failed(model, generation, surface),
        Msg::NativeWindowTaken { surface } => surfaces::native_window_taken(model, surface),
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
            // only the top slot is ever armed, so an expiry naming anything
            // else is a stale one: the entry it was armed for has since been
            // cleared, replaced (which stamps a fresh id), or dismissed by a
            // keypress, and the timer thread was already in flight when that
            // happened. Losing that race is "already handled", not an error
            // -- but obeying it would retire whatever now sits at that id,
            // an entry that has not had its own turn at the front yet
            //
            // `Effect::ScheduleToastExpiry` has no cancellation either, so
            // the timer armed before the pause key was pressed is still
            // asleep and still lands: obeying it would retire the exact
            // notice the user paused in order to read.
            if !model.engine.messages.paused() && model.engine.messages.top_slot() == Some(id) {
                model.engine.messages.entries.retain(|e| e.id() != id);
                model.dirty = true;
            }
            // a sticky notice holds no slot, so an expiry naming one is not
            // stale: it is the timer its own record armed, and what it says
            // is that the line has now stood long enough to have been read.
            // Nothing leaves the screen for it, so nothing repaints
            model.engine.messages.note_stood_its_window(id);
            Vec::new()
        }
        // one frame of the stack's exit motion, and the only place a
        // further tick is ever asked for: a tick arriving with no motion
        // live -- the interrupt rule dropped it, or this is the wakeup that
        // followed the last frame -- ends the chain instead of re-arming it,
        // which is what stops an idle editor from holding a timer thread
        Msg::AnimTick => {
            let Some(motion) = model.toast_motion.as_mut() else {
                return Vec::new();
            };
            if motion.advance() {
                model.dirty = true;
                return vec![Effect::ScheduleAnimTick {
                    after: crate::native::toast::MOTION_STEP,
                }];
            }
            // no repaint on the way out: the frame already on screen is the
            // settled stack, and every toast box compares unequal once the
            // motion stops renumbering the slots, so asking for this frame
            // recomposites the whole toast area into something nobody can
            // tell from what it replaced
            model.toast_motion = None;
            Vec::new()
        }
        // the chain died before its motion did, so this is the retirement
        // the last tick would have done: the model is already in its final
        // state, and dropping the interpolation paints that state rather
        // than leaving the stack held at whatever frame it reached
        Msg::AnimDropped => {
            model.dirty |= model.toast_motion.take().is_some();
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
                    .record_native_notice("view: buffer open. Close it first".to_string(), false),
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
                    "view: AI agent access is not enabled for this project. Invoke :View ai again to be asked".to_string(),
                    false,
                )
            }
        }
    }
}

/// `:View keys profile [desktop|editor|auto]`: records the flip on the
/// model, with no report and no RPC of its own -- `NativeSession::follow_up`
/// (`view/src/native.rs`) watches this field through `Stage::ProfileFlip`
/// and is where the actual restore-and-reissue happens. A bare
/// `:View keys profile` sets [`Model::key_profile_report_requested`]
/// alone, since it asks what is live and changes nothing, and
/// `NativeSession` is what knows the profile and modifier to report. An
/// unrecognized name changes nothing, the same "typo does nothing" answer
/// `look::invoke` gives an unrecognized `ui panes` argument.
fn keys_invoke(model: &mut Model, verb: &str) -> Vec<Effect> {
    let mut words = verb.split_whitespace();
    if words.next() != Some("profile") {
        return Vec::new();
    }
    let profile = match words.next() {
        Some("desktop") => Some(crate::native::chords::KeyProfile::Desktop),
        Some("editor") => Some(crate::native::chords::KeyProfile::Editor),
        Some("auto") => None,
        None => {
            model.key_profile_report_requested = true;
            return Vec::new();
        }
        _ => return Vec::new(),
    };
    model.key_profile_override = profile;
    Vec::new()
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

#[cfg(test)]
mod tests;
