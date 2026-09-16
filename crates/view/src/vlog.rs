//! `VIEW_LOG=<path>` diagnostic capture: a structured, line-oriented
//! append log for triage flows that today mean asking the user to paste an
//! `asciinema` recording. Absent the env var, [`init`] leaves the process-wide
//! sink at `None` and every [`log`]/[`log_with`] call after that is a single
//! `Option` check with no allocation and no file I/O -- the zero-overhead
//! contract an interactive keystroke-to-paint loop needs even when nobody is
//! capturing. [`log_with`] is what makes that contract hold for a *computed*
//! payload (a `format!` call, a `Vec` collected into a `String`): its closure
//! runs only after the sink check, so a caller building a payload from
//! several fields never pays for that work on the no-`VIEW_LOG` path. Call
//! sites with an already-owned `&str` (no formatting needed either way) use
//! plain [`log`] instead.
//!
//! Deliberately lives in the bin crate, not `view-core`: `view-core` is pure
//! (no I/O, no env access -- see this repo's hard rules), so every log call
//! site here reads state at the runtime/main boundary, where the relevant
//! `Msg`/`UiEvent`/effect already crosses in the ordinary course of the
//! loop, rather than threading a logger parameter down into library code
//! that has no other reason to take one.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// Initializes the process-wide log sink from `VIEW_LOG`, once, using
/// `process_start` as the monotonic origin every logged timestamp is
/// relative to -- the same `Instant` `main.rs` already captures before doing
/// any other work, so a log line's stamp lines up with the shell-paint
/// latency this build already measures in debug builds.
///
/// A `VIEW_LOG` path that cannot be opened for append (bad permissions, a
/// missing parent directory) degrades to no logging rather than failing the
/// session: this is a diagnostics feature, never a reason an editor session
/// should refuse to start. Reported once to stderr so the miss is not
/// silent. Idempotent: a second call is a no-op ([`OnceLock`] contract), so
/// callers never need to guard against calling this more than once.
///
/// Hands `view-engine` the writer its own diagnostic lines go to
/// ([`view_engine::set_diagnostics`]) when -- and only when -- a sink was
/// opened, so the reader thread's account of a notification it could not
/// decode lands in this same file, and a session running without one
/// builds no such account at all.
pub fn init(process_start: Instant) {
    START.get_or_init(|| process_start);
    let sink = SINK.get_or_init(|| match std::env::var_os("VIEW_LOG") {
        None => None,
        Some(path) => match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => Some(Mutex::new(file)),
            Err(e) => {
                eprintln!(
                    "view: cannot open VIEW_LOG path {}: {e}, diagnostic logging disabled",
                    path.to_string_lossy()
                );
                None
            }
        },
    });
    // the engine's reader thread sees things no `Msg` carries -- a
    // notification its decoder refused -- and sits below this sink with no
    // way to reach it, so the crate that owns the sink hands it a writer
    // here. Its lines go under the topic every other line about the
    // connection uses.
    //
    // Only once there is something to write to: the writer is what tells
    // that crate a log is open, and handing it one that discards every
    // line would leave it formatting a payload per event for a session
    // nobody is capturing.
    if sink.is_some() {
        view_engine::set_diagnostics(|line| log("engine", line));
    }
}

/// The sink's own file duplicated, for a caller that needs a descriptor of
/// its own rather than a line on it -- `view-tui`'s stderr guard, which puts
/// fd 2 on this file for the life of the session so a library's diagnostic
/// becomes triage material instead of paint on the terminal.
///
/// A duplicate of the handle [`init`] opened, never a second open of the
/// same path: two independently opened appenders keep two offsets and tear
/// each other's lines. `None` when `VIEW_LOG` is unset, when its path could
/// not be opened, or when the duplicate itself fails -- every one of which
/// leaves the caller to pick its own fallback, since none of them is a
/// reason a session should refuse to start.
///
/// Unix-gated with its consumer: on Windows it is dead code, and
/// `-D warnings` makes that a build break.
#[cfg(unix)]
#[must_use]
pub fn sink_dup() -> Option<std::fs::File> {
    let Some(Some(file)) = SINK.get() else {
        return None;
    };
    let file = file.lock().unwrap_or_else(PoisonError::into_inner);
    file.try_clone().ok()
}

/// Writes one `<mono_ms> <topic> <payload>` line if [`init`] opened a sink;
/// a single `Option` check and return otherwise -- the zero-overhead path
/// this module's docs promise. `topic` is a short fixed tag (`"startup"`,
/// `"theme"`, `"msg"`, `"engine"`, `"layout"`, `"fatal"`); `payload` is caller-formatted
/// free text, never parsed back by this module.
///
/// For a `payload` that already exists as a `&str`/`&String` (nothing to
/// compute). A caller whose payload requires `format!` or a collect wants
/// [`log_with`] instead, so that work is skipped entirely when the sink is
/// absent.
pub fn log(topic: &str, payload: &str) {
    let Some(Some(file)) = SINK.get() else {
        return;
    };
    write_line(file, topic, payload);
}

/// Like [`log`], but `payload` is a closure run only once the sink check
/// above has confirmed a sink exists -- the shape that keeps a `format!`-
/// or collect-built payload out of the no-`VIEW_LOG` path entirely, rather
/// than building the `String` and then discarding it against an `Option`
/// check that already knew nobody would read it.
pub fn log_with(topic: &str, payload: impl FnOnce() -> String) {
    let Some(Some(file)) = SINK.get() else {
        return;
    };
    write_line(file, topic, &payload());
}

// Latency consequence: `dispatch`'s `log_msg` call runs this synchronously
// on the loop's own dispatch thread for every `Msg`, so with `VIEW_LOG` set
// each dispatch pays one blocking `writeln!` under this process-wide
// `Mutex`, serializing against every other logger call in flight. Not an
// RPC wait (the paint-loop-never-awaits-RPC rule this crate holds
// elsewhere is about the engine connection, not local file I/O), but a
// real per-message file write on the hot path nonetheless -- acceptable
// only because `VIEW_LOG` is opt-in and every call site above already
// short-circuits to a single `Option` check, costing nothing, when it is
// unset (see this module's own doc).
fn write_line(file: &Mutex<std::fs::File>, topic: &str, payload: &str) {
    let ms = START.get().map_or(0, |start| start.elapsed().as_millis());
    write_line_at(file, topic, ms, payload);
}

/// [`write_line`] for a payload whose own moment is not now: a line about
/// something another thread did, carrying the reading that thread took.
fn write_line_at(file: &Mutex<std::fs::File>, topic: &str, ms: u128, payload: &str) {
    let mut f = file.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = writeln!(f, "{ms} {topic} {payload}");
}

/// One `engine redraw` census line for a batch just drained from the
/// pump, stamped with `folded_at` -- the reading the reader thread took
/// when it folded that batch.
///
/// Which clock the line carries is what taking `folded_at` here is for:
/// written from this call instead, the stamp would be the loop's drain,
/// which is one pass or several after the engine spoke, so a window read
/// off these lines would be a window on view's own draining rather than on
/// the engine's traffic. `None` falls back to now, which is a drain the
/// pump dated nothing for (nothing had reached a `Flush`).
///
/// Two limits on reading it as the arrival. The reading is taken at the
/// top of the fold, which is after the reader thread has decoded the whole
/// batch into a `Vec` and after it has the damage lock, so a large batch's
/// own decode and any wait for a lock the loop holds sit inside the stamp
/// rather than before it -- on a 567-event batch that is the launch
/// attribution's own number. And a drain that reached two `Flush`es
/// carries both wire batches on one line, dated by the later fold: the
/// line says which it is, since the `flush=` count it prints is 1 for a
/// line standing for one batch.
///
/// An empty drain writes no line: it is a wakeup token for damage still
/// short of a `Flush`, so it is not a batch the engine sent.
pub fn log_redraw_census(events: &[view_core::events::UiEvent], folded_at: Option<Instant>) {
    if events.is_empty() {
        return;
    }
    let Some(Some(file)) = SINK.get() else {
        return;
    };
    let ms = folded_at.zip(START.get()).map_or_else(
        || START.get().map_or(0, |start| start.elapsed().as_millis()),
        |(at, start)| at.saturating_duration_since(*start).as_millis(),
    );
    FOLDED_MS.store(
        u64::try_from(ms).unwrap_or(u64::MAX),
        std::sync::atomic::Ordering::Relaxed,
    );
    write_line_at(file, "engine", ms, &redraw_census(events, &REDRAWS));
}

/// When the reader thread folded the batch the last census line stood for.
///
/// The `highlight` topic's own line is stamped at the paint, so the two
/// readings on one line separate an engine that sent a colour late from a
/// frontend that held one it already had.
static FOLDED_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Logs the loggable slice of one `Msg` crossing the runtime loop's
/// dispatch seam: theme events nested in a `Redraw` batch (`view-core` is
/// pure and cannot log these itself -- see the module docs), the async
/// `nvim_get_hl` default-colors probe's reply, an engine-down transition,
/// a native feature invocation, the mappings the engine claimed, what the
/// launch said before its UI existed, which sink its notices go to, and
/// every AI event, plus the toast stack's own clock (an expiry and each frame of
/// the motion it starts), which is what a read of this log answers "did
/// anything wake the loop while the editor was idle" from. Every other
/// `Msg` variant (`Key`, `Paste`, `Mouse`, `Resized`, loop plumbing)
/// carries nothing this log's contract asks for and is a deliberate no-op
/// here.
pub fn log_msg(msg: &view_core::msg::Msg) {
    use view_core::msg::Msg;
    match msg {
        // the batch's own census is written where it is drained
        // ([`log_redraw_census`]), which is the only place the fold's
        // reading is in hand
        Msg::Redraw(events) => {
            for ev in events {
                log_ui_event(ev);
            }
        }
        Msg::ColorSchemeChanged { name } => {
            log_with("theme", || format!("colorscheme name={name}"));
        }
        // the two lines read together answer why a resolved `[ui] theme` is
        // not the one painting: a session that resolved a name logs this or
        // the `ColorSchemeChanged` above it, and one that resolved none logs
        // neither
        Msg::ColorSchemeMissing { name } => {
            log_with("theme", || format!("colorscheme-missing name={name}"));
        }
        Msg::HlProbeReply { generation, fg, bg } => {
            log_with("theme", || {
                format!("probe-reply generation={generation} fg={fg:?} bg={bg:?}")
            });
        }
        Msg::EngineDown(exit) => {
            log_with("engine", || {
                format!("down code={:?} by_signal={}", exit.code, exit.by_signal)
            });
        }
        // the second `caps tier=` line a session can write, and the one that
        // records what it settled on: the line at startup states only what
        // the probe had heard before it handed the terminal over
        Msg::CapsUpgraded(caps) => {
            log_with("startup", || {
                format!(
                    "caps tier={:?} {} answered late",
                    caps.tier,
                    view_tui::tiers::resolved(caps)
                )
            });
        }
        Msg::FeatureInvoke { feature, verb } => {
            log_with("native", || format!("invoke feature={feature} verb={verb}"));
        }
        // The payload, not a kind label: an AI triage question is almost
        // always about one (which path a proposal named, which stop reason
        // ended a turn), and a log that recorded only which arm arrived
        // would answer none of them.
        Msg::Ai(event) => {
            log_with("ai", || ai_payload(event));
        }
        // the sighting, not the verdict: a conflict notice that failed to
        // appear is either a float the watcher never reported or a rect the
        // table declined, and only a line carrying the geometry as it
        // arrived can tell those apart
        Msg::FloatObserved(float) => {
            log_with("native", || {
                format!(
                    "float win={} row={} col={} {}x{} anchor={:?} zindex={} \
                     identity={:?} hidden={}",
                    float.win,
                    float.row,
                    float.col,
                    float.width,
                    float.height,
                    float.anchor,
                    float.zindex,
                    float.identity(),
                    float.hidden,
                )
            });
        }
        // the other half of an absorption, on the same terms: a palette
        // standing empty beside a plugin's menu is either a read whose
        // reply never came back or one that answered with the window still
        // visible, and only the reply's own fields tell those apart
        Msg::FloatRows {
            win,
            hidden,
            lines,
            selected,
        } => {
            log_with("native", || {
                format!(
                    "float-rows win={win} hidden={hidden} rows={} selected={selected:?}",
                    lines.len()
                )
            });
        }
        // the one `Msg` with a wall-clock cadence of its own, and so the
        // one whose lines answer "was the loop woken while nothing was
        // moving": a short burst per dismissal, silence between them
        Msg::AnimTick => log("toast", "anim-tick"),
        Msg::AnimDropped => log("toast", "anim-dropped"),
        Msg::ToastExpired { .. } => log("toast", "expired"),
        // the two halves of the takeover's one reply that a triage read
        // asks about: what the launch said before any UI existed (a
        // takeover that raised says so here, and nowhere else), and where
        // this session decided its own notices belong
        Msg::StartupMessages { text } => {
            log_with("native", || format!("startup-messages {}", capped(text)));
        }
        Msg::NotifySinkRead { foreign } => {
            log_with("native", || format!("notify-sink foreign={foreign}"));
        }
        // dated because the pass that sent it is the one nothing else in
        // the log records: a claimant asked at the takeover and a claimant
        // asked again after its own setup leave the same standing notice,
        // and only this line says which of them turned the surface loose
        Msg::ClaimantsHandedBack { modules } => {
            log_with("native", || format!("handed-back {}", modules.join(",")));
        }
        Msg::MappingsClaimed {
            claimed,
            colon_mapped,
        } => {
            log_with("native", || {
                let keys: Vec<String> = claimed
                    .iter()
                    .map(|c| format!("{}={}", c.lhs, c.had_user_mapping))
                    .collect();
                format!("claimed {} colon-mapped={colon_mapped}", keys.join(","))
            });
        }
        Msg::ColonMappingRead { mapped } => {
            log_with("native", || format!("colon-mapped={mapped}"));
        }
        _ => {}
    }
}

/// How much of a free-text field reaches the log.
///
/// Latency consequence: `log_msg` runs on the loop's dispatch thread under
/// the process-wide logger mutex (see [`write_line`]), so an unbounded
/// `Debug` of an `AiEvent` would put a whole proposed file -- or a whole
/// agent write -- through `format!` and `writeln!` there, turning one agent
/// edit to a large file into a megabyte-scale write between two frames.
/// Every payload-bearing arm below is capped at this, which holds the cost
/// of an AI dispatch with `VIEW_LOG` set to the same small constant every
/// other arm already pays. With `VIEW_LOG` unset nothing is formatted at
/// all: the closure `log_with` takes never runs.
///
/// It is also what keeps the log handable: the module doc describes this
/// file as the thing a user is asked to attach to a bug report, and a full
/// `Debug` would put the whole conversation, the model's reasoning and the
/// contents of every file it touched into it.
const PAYLOAD_CAP: usize = 120;

/// How many redraw batches one sink has written a census line for, which is
/// what numbers them, and how many events they carried between them.
///
/// Read by the `first content frame` startup line as well as written here,
/// so that column and the census lines standing above it in the log are
/// one count rather than two that can disagree -- the loop's own counter
/// saw neither the cutover's drain nor the recovery path's, both of which
/// write a line here.
///
/// The count belongs to the sink rather than to the process, because a test
/// driving a census of its own would otherwise number its lines between a
/// sibling's: [`REDRAWS`] is the live session's one sink, and each test
/// holds its own and starts at 1.
#[derive(Default)]
struct RedrawCounts {
    batches: std::sync::atomic::AtomicU64,
    events: std::sync::atomic::AtomicU64,
}

static REDRAWS: RedrawCounts = RedrawCounts {
    batches: std::sync::atomic::AtomicU64::new(0),
    events: std::sync::atomic::AtomicU64::new(0),
};

/// Batches this process has drained a census line for and the events in
/// them, for a caller reporting what the engine had sent by some moment.
pub fn redraws_drained() -> (u64, u64) {
    use std::sync::atomic::Ordering;
    (
        REDRAWS.batches.load(Ordering::Relaxed),
        REDRAWS.events.load(Ordering::Relaxed),
    )
}

/// One `engine redraw` line for a batch the engine sent: how many events it
/// carried and how many of each kind.
///
/// Counted rather than listed, because `grid_line` alone runs to hundreds
/// per batch and the `layout` topic omits it for that reason. What this
/// answers that no other topic can: whether a window the log shows no
/// `layout` or `msg` line in is a window the engine said nothing at all in,
/// or one where it sent nothing but cells. An absence of lines in those two
/// topics is not an absence of engine traffic, and two attributions were
/// argued from that reading.
///
/// The line's own stamp is the reader thread's fold rather than the drain
/// that produced this call: [`log_redraw_census`] is its one writer and
/// carries that reading.
fn redraw_census(events: &[view_core::events::UiEvent], counts: &RedrawCounts) -> String {
    use std::borrow::Cow;
    use view_core::events::UiEvent;
    let batch = counts
        .batches
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1;
    counts.events.fetch_add(
        u64::try_from(events.len()).unwrap_or(u64::MAX),
        std::sync::atomic::Ordering::Relaxed,
    );
    let mut kinds: Vec<(Cow<'_, str>, usize)> = Vec::new();
    for ev in events {
        // an event this tree's decoder has no arm for still carries the name
        // nvim sent, and a census counting it as `unknown` names nothing: 28
        // of the 567 events in the batch a launch attribution rests on were
        // written under that label
        let kind = match ev {
            UiEvent::Unknown { name } => Cow::Owned(format!("unknown({})", capped(name))),
            _ => Cow::Borrowed(event_kind(ev)),
        };
        match kinds.iter_mut().find(|(name, _)| *name == kind) {
            Some((_, count)) => *count += 1,
            None => kinds.push((kind, 1)),
        }
    }
    let census = kinds
        .iter()
        .map(|(name, count)| format!("{name}={count}"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "redraw batch={batch} events={} kinds={census}",
        events.len()
    )
}

/// The wire name of one event, so a census line reads in the vocabulary
/// nvim's own `ui.txt` and this tree's decoder both use.
fn event_kind(ev: &view_core::events::UiEvent) -> &'static str {
    use view_core::events::UiEvent;
    match ev {
        UiEvent::GridResize { .. } => "grid_resize",
        UiEvent::GridLine { .. } => "grid_line",
        UiEvent::GridCursorGoto { .. } => "grid_cursor_goto",
        UiEvent::GridScroll { .. } => "grid_scroll",
        UiEvent::GridClear { .. } => "grid_clear",
        UiEvent::GridDestroy { .. } => "grid_destroy",
        UiEvent::WinPos { .. } => "win_pos",
        UiEvent::WinFloatPos { .. } => "win_float_pos",
        UiEvent::WinExternalPos { .. } => "win_external_pos",
        UiEvent::WinHide { .. } => "win_hide",
        UiEvent::WinClose { .. } => "win_close",
        UiEvent::MsgSetPos { .. } => "msg_set_pos",
        UiEvent::WinViewport { .. } => "win_viewport",
        UiEvent::WinViewportMargins { .. } => "win_viewport_margins",
        UiEvent::HlAttrDefine { .. } => "hl_attr_define",
        UiEvent::DefaultColorsSet { .. } => "default_colors_set",
        UiEvent::HlGroupSet { .. } => "hl_group_set",
        UiEvent::Flush => "flush",
        UiEvent::ModeInfoSet { .. } => "mode_info_set",
        UiEvent::ModeChange { .. } => "mode_change",
        UiEvent::CmdlineShow { .. } => "cmdline_show",
        UiEvent::CmdlinePos { .. } => "cmdline_pos",
        UiEvent::CmdlineHide => "cmdline_hide",
        UiEvent::MsgShow { .. } => "msg_show",
        UiEvent::MsgClear => "msg_clear",
        UiEvent::MsgShowmode { .. } => "msg_showmode",
        UiEvent::MsgShowcmd { .. } => "msg_showcmd",
        UiEvent::MsgRuler { .. } => "msg_ruler",
        UiEvent::TablineUpdate { .. } => "tabline_update",
        UiEvent::PopupmenuShow { .. } => "popupmenu_show",
        UiEvent::PopupmenuSelect { .. } => "popupmenu_select",
        UiEvent::PopupmenuHide => "popupmenu_hide",
        UiEvent::MouseOn => "mouse_on",
        UiEvent::MouseOff => "mouse_off",
        UiEvent::UiSend { .. } => "ui_send",
        UiEvent::Unknown { .. } => "unknown",
    }
}

/// `text` capped at [`PAYLOAD_CAP`], with what was dropped counted rather
/// than silently lost -- a truncation that did not say so reads as a short
/// message, which is a different bug report.
fn capped(text: &str) -> String {
    let kept: String = text.chars().take(PAYLOAD_CAP).collect();
    if kept.len() == text.len() {
        return format!("{kept:?}");
    }
    format!("{kept:?}+{}B", text.len() - kept.len())
}

/// One `AiEvent` rendered for the log: ids, paths, statuses and stop
/// reasons in full, free text capped. Shaped like the `Debug` it replaces
/// -- same field names, and `MessageChunk` alone reordered so that a
/// capped `text` never sits between the two fields a reader (or a grep)
/// identifies a chunk by -- so a reader of an older log is not learning a
/// second format.
fn ai_payload(event: &view_core::native::ai_event::AiEvent) -> String {
    use view_core::native::ai_event::AiEvent;
    match event {
        AiEvent::MessageChunk {
            message_id,
            text,
            from_agent,
        } => format!(
            "MessageChunk {{ message_id: {message_id:?}, from_agent: {from_agent}, \
             text: {} }}",
            capped(text)
        ),
        AiEvent::ThoughtChunk { message_id, text } => format!(
            "ThoughtChunk {{ message_id: {message_id:?}, text: {} }}",
            capped(text)
        ),
        // Both carry a string the agent chose: a crash message is whatever
        // its `error.message` said, and a session id is opaque to this
        // process. Neither is bounded by anything on this side of the wire.
        AiEvent::SessionReady { session_id } => {
            format!("SessionReady {{ session_id: {} }}", capped(session_id))
        }
        AiEvent::SessionCrashed { message } => {
            format!("SessionCrashed {{ message: {} }}", capped(message))
        }
        AiEvent::ToolCallUpdate {
            tool_call_id,
            title,
            status,
            content,
        } => format!(
            "ToolCallUpdate {{ tool_call_id: {tool_call_id:?}, title: {}, status: {status:?}, \
             content: {} }}",
            capped(title),
            content.as_ref().map_or_else(
                || "None".to_string(),
                |items| format!("{} item(s)", items.len())
            )
        ),
        AiEvent::PermissionRequested {
            request_id,
            tool_call_id,
            title,
            tool_kind,
            options,
        } => format!(
            "PermissionRequested {{ request_id: {request_id}, tool_call_id: {tool_call_id:?}, \
             title: {}, tool_kind: {}, options: [{}] }}",
            title.as_ref().map_or_else(
                || "None".to_string(),
                |title| format!("Some({})", capped(title))
            ),
            tool_kind.as_ref().map_or_else(
                || "None".to_string(),
                |kind| format!("Some({})", capped(kind))
            ),
            // The count is bounded too: option_id and name arrive from the
            // agent, and so does how many options there are.
            options
                .iter()
                .take(8)
                .map(|o| format!(
                    "{{ option_id: {}, name: {}, kind: {:?} }}",
                    capped(&o.option_id),
                    capped(&o.name),
                    o.kind
                ))
                .chain((options.len() > 8).then(|| format!("+{} more", options.len() - 8)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        AiEvent::PlanUpdated { entries } => {
            format!("PlanUpdated {{ entries: {} item(s) }}", entries.len())
        }
        AiEvent::DiffProposed {
            request_id,
            path,
            old_text,
            new_text,
        } => format!(
            "DiffProposed {{ request_id: {request_id}, path: {path:?}, old_bytes: {}, \
             new_bytes: {} }}",
            old_text.as_ref().map_or(0, String::len),
            new_text.len()
        ),
        AiEvent::FsWriteRequested {
            request_id,
            path,
            content,
        } => format!(
            "FsWriteRequested {{ request_id: {request_id}, path: {path:?}, bytes: {} }}",
            content.len()
        ),
        // Everything else is bounded by its own shape -- a session id, a
        // stop reason, a request id and a path, an exit message, a usage
        // count -- and reads better as the `Debug` the wire's own
        // vocabulary spells.
        bounded => format!("{bounded:?}"),
    }
}

/// One outbound `AiCommand` rendered for the log, the answering half of
/// [`ai_payload`] and capped the same way.
///
/// Every `ai …` line used to be inbound, so a log could show five identical
/// permission requests and say nothing at all about what was answered to
/// the four before the fifth -- which is exactly the question a session
/// that kept asking leaves behind
/// (`.superpowers/sdd/2026-08-21-dogfood-fixes/task-21-rootcause.md`). The
/// `option_id` is what closes it: read against the `PermissionRequested`
/// line already in the log, which prints every option's id and kind, it
/// says which option the answer named without a new field on the wire type.
pub fn ai_command_payload(command: &view_core::native::ai_event::AiCommand) -> String {
    use view_core::native::ai_event::{AiCommand, PermissionOutcome};
    match command {
        AiCommand::AnswerPermission {
            request_id,
            outcome,
        } => {
            let outcome = match outcome {
                PermissionOutcome::Cancelled => "Cancelled".to_string(),
                PermissionOutcome::Selected { option_id } => {
                    format!("Selected {{ option_id: {} }}", capped(option_id))
                }
            };
            format!("AnswerPermission {{ request_id: {request_id}, outcome: {outcome} }}")
        }
        // The reply's own content is a whole file the agent asked for; its
        // size is the part a log can carry (see `PAYLOAD_CAP`).
        AiCommand::FsReadReply { request_id, result } => format!(
            "FsReadReply {{ request_id: {request_id}, result: {} }}",
            result.as_ref().map_or_else(
                |error| format!("Err({})", fs_error_payload(error)),
                |content| format!("Ok({} bytes)", content.len())
            )
        ),
        AiCommand::FsWriteReply { request_id, result } => format!(
            "FsWriteReply {{ request_id: {request_id}, result: {} }}",
            result.as_ref().map_or_else(
                |error| format!("Err({})", fs_error_payload(error)),
                |()| { "Ok".to_string() }
            )
        ),
        AiCommand::Prompt { text, context } => format!(
            "Prompt {{ text: {}, context: {} block(s) }}",
            capped(text),
            context.len()
        ),
        // Cancel and DiscardProposal carry an id and an outcome word at
        // most.
        bounded => format!("{bounded:?}"),
    }
}

/// One [`view_core::native::ai_event::FsError`] rendered for the log.
///
/// `Other` is the arm that needs one: it carries whatever nvim said, which
/// includes a path the agent chose and, on some failures, the line it tried
/// to write -- unbounded text reaching the log through an error is the same
/// leak as unbounded text reaching it through a reply.
fn fs_error_payload(error: &view_core::native::ai_event::FsError) -> String {
    use view_core::native::ai_event::FsError;
    match error {
        FsError::Other { message } => format!("Other {{ message: {} }}", capped(message)),
        // NotFound and PermissionDenied are their own whole spelling.
        bounded => format!("{bounded:?}"),
    }
}

/// The other outbound AI edge: a prompt handed to the context worker, which
/// assembles the `AiCommand::Prompt` off this thread. Capped like every
/// other free-text payload here -- what a triage read needs from it is that
/// a submission happened at all, and roughly what it said.
#[must_use]
pub fn ai_prompt_submit_payload(text: &str) -> String {
    format!("AiPromptSubmit {{ text: {} }}", capped(text))
}

/// Whether `ev` is one of the box-moving events the `layout` topic
/// records.
///
/// These are the events that move, size or remove a box on screen without
/// naming a single cell, so the question a stale cell raises -- which pane
/// owned it, and on which frame that stopped being true -- is answerable
/// from nothing else. `grid_line` is deliberately not among them: one line
/// per cell run would be the log's whole volume and would answer a
/// different question.
fn is_layout_event(ev: &view_core::events::UiEvent) -> bool {
    use view_core::events::UiEvent;
    matches!(
        ev,
        UiEvent::GridResize { .. }
            | UiEvent::GridClear { .. }
            | UiEvent::GridDestroy { .. }
            | UiEvent::WinPos { .. }
            | UiEvent::WinFloatPos { .. }
            | UiEvent::WinHide { .. }
            | UiEvent::WinClose { .. }
            | UiEvent::WinViewportMargins { .. }
    )
}

/// The box-moving events of one `Msg`, kept aside so [`log_layout`] can
/// write them out beside the fold's own answer.
///
/// Empty whenever no sink is open, so a run without `VIEW_LOG` clones
/// nothing and allocates nothing. This is where the deferral [`log_with`]
/// gives an ordinary call site lives for this topic: the payload cannot be
/// built until `update` has run, so the sink check moves ahead of the
/// clone instead of ahead of the `format!`.
#[must_use]
pub fn layout_events(msg: &view_core::msg::Msg) -> Vec<view_core::events::UiEvent> {
    let (Some(Some(_)), view_core::msg::Msg::Redraw(events)) = (SINK.get(), msg) else {
        return Vec::new();
    };
    events
        .iter()
        .filter(|ev| is_layout_event(ev))
        .cloned()
        .collect()
}

/// One `layout` line for `ev`, or `None` for an event the topic does not
/// record.
///
/// A float's line carries `withheld` -- view's own hold, distinct from the
/// `hidden` nvim announces -- read off `model` after the fold, because a
/// withheld float paints no cell and a reader working out which pane owned
/// one has to be able to rule it out.
fn layout_payload(
    model: &view_core::model::Model,
    ev: &view_core::events::UiEvent,
) -> Option<String> {
    use view_core::events::UiEvent;
    Some(match ev {
        UiEvent::GridResize {
            grid,
            width,
            height,
        } => format!("grid_resize grid={grid} width={width} height={height}"),
        UiEvent::GridClear { grid } => format!("grid_clear grid={grid}"),
        UiEvent::GridDestroy { grid } => format!("grid_destroy grid={grid}"),
        UiEvent::WinPos {
            grid,
            win,
            startrow,
            startcol,
            width,
            height,
        } => format!(
            "win_pos grid={grid} win={} startrow={startrow} startcol={startcol} \
             width={width} height={height}",
            win.0
        ),
        UiEvent::WinFloatPos {
            grid,
            win,
            anchor_grid,
            zindex,
            compindex,
            screen_row,
            screen_col,
        } => format!(
            "win_float_pos grid={grid} win={} anchor_grid={anchor_grid} zindex={zindex} \
             compindex={compindex} screen_row={screen_row} screen_col={screen_col} \
             withheld={}",
            win.0,
            model
                .engine
                .grids()
                .float_withheld(view_core::grid::registry::GridId(*grid))
        ),
        UiEvent::WinHide { grid } => format!("win_hide grid={grid}"),
        UiEvent::WinClose { grid } => format!("win_close grid={grid}"),
        UiEvent::WinViewportMargins {
            grid,
            win,
            top,
            bottom,
            left,
            right,
        } => format!(
            "win_viewport_margins grid={grid} win={} top={top} bottom={bottom} \
             left={left} right={right}",
            win.0
        ),
        _ => return None,
    })
}

/// One `layout` line per event [`layout_events`] kept, written after
/// `update` folded the batch.
pub fn log_layout(model: &view_core::model::Model, events: &[view_core::events::UiEvent]) {
    for ev in events {
        if let Some(payload) = layout_payload(model, ev) {
            log("layout", &payload);
        }
    }
}

fn log_ui_event(ev: &view_core::events::UiEvent) {
    use view_core::events::UiEvent;
    match ev {
        UiEvent::DefaultColorsSet { fg, bg, sp } => {
            log_with("theme", || {
                format!("default_colors_set fg={fg:?} bg={bg:?} sp={sp:?}")
            });
        }
        UiEvent::MsgShow {
            kind,
            content,
            replace_last,
        } => {
            // the `content` collect used to run unconditionally ahead of the
            // sink check; folding it into the closure means a no-`VIEW_LOG`
            // run never allocates the joined text at all
            log_with("msg", || {
                let text: String = content.iter().map(|(_, t)| t.as_str()).collect();
                format!("show kind={kind} replace_last={replace_last} text={text:?}")
            });
        }
        UiEvent::MsgClear => log("msg", "clear"),
        _ => {}
    }
}

/// Microseconds since the origin [`init`] was handed, whose millisecond
/// is the number every line written here already carries as its prefix.
///
/// Microseconds rather than the prefix's own unit because a keystroke
/// answered inside one millisecond reads 0 or 1 there, and that is the
/// whole span the input path is budgeted in. For a caller holding one
/// reading open until a later line can close it: [`FeltLog`] stamps an
/// input when it arrives and writes the line at the flush that answers
/// it, so the wait is a subtraction of two readings taken from this one
/// clock.
#[must_use]
pub fn mono_us() -> u128 {
    START.get().map_or(0, |start| start.elapsed().as_micros())
}

/// Whether a sink is open, for a call site whose payload cannot be built
/// inside a [`log_with`] closure -- one that has to read the grid, or hold
/// state between two passes of the loop.
#[must_use]
pub fn capturing() -> bool {
    matches!(SINK.get(), Some(Some(_)))
}

/// Microseconds from the origin at the takeover batch's first write, which
/// every `takeover` line is measured from.
///
/// Stored per batch rather than once: a replacement engine answers on its
/// own channel and is handed the takeover again, and that batch is a
/// different one.
static TAKEOVER_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Opens the takeover batch's reading, immediately before its first call
/// goes out.
pub fn takeover_opened() {
    if capturing() {
        TAKEOVER_US.store(
            u64::try_from(mono_us()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

/// One `takeover` line: which call of that batch went out, or was
/// answered, and how long after the batch's first write.
///
/// nvim is blocked inside `VimEnter` for the whole of this batch, so the
/// lines together say which of its calls that block is spent in -- the
/// reading `--startuptime`'s single `VimEnter autocommands` figure cannot
/// give.
pub fn log_takeover(call: &str) {
    log_with("takeover", || {
        let since = u64::try_from(mono_us())
            .unwrap_or(u64::MAX)
            .saturating_sub(TAKEOVER_US.load(std::sync::atomic::Ordering::Relaxed));
        format!("{call} us={since}")
    });
}

/// What the `takeover` topic calls one effect of that batch.
#[must_use]
pub fn takeover_call(eff: &view_core::msg::Effect) -> String {
    use view_core::msg::{Effect, RpcCall};
    match eff {
        Effect::Rpc(RpcCall::Takeover { steps }) => format!("takeover steps={}", steps.len()),
        Effect::Rpc(RpcCall::UiAttach { surfaces, .. }) => {
            format!("ui_attach ext={}", surfaces.len())
        }
        Effect::Rpc(RpcCall::ClaimStdoutTty) => "claim_stdout_tty".to_string(),
        Effect::Rpc(_) => "rpc".to_string(),
        Effect::Reply { .. } => "vim_enter reply".to_string(),
        _ => "effect".to_string(),
    }
}

/// What a dispatched message is to a keystroke still waiting for the
/// screen, read off the message before the fold consumes it.
///
/// The three things that dirty view's screen, which is what deciding
/// whether a frame answered a key comes down to: the user's own input, a
/// batch the engine sent, and everything else -- a timer, a notice, a reply
/// on a chain of view's own.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Input,
    EngineBatch {
        /// Whether this batch carried a `cmdline_show`, which is the only
        /// thing that tells a guess nvim answered from one it refuted. The
        /// settled model cannot: a batch carrying both the show and the
        /// hide ends with no command line and no guess, exactly as a
        /// withdrawal does.
        answered_cmdline: bool,
    },
    Other,
}

impl Dispatch {
    /// Which of the three a message is.
    #[must_use]
    pub fn of(msg: &view_core::msg::Msg) -> Self {
        use view_core::msg::Msg;
        match msg {
            Msg::Key(_) | Msg::Paste(_) | Msg::Mouse(_) => Self::Input,
            // an empty batch is the drain of a wakeup token for damage that
            // has not reached a `Flush` yet: it folds nothing, paints
            // nothing, and answers no key. Read as a batch it closed every
            // waiting input on whatever frame came next
            Msg::Redraw(events) if !events.is_empty() => Self::EngineBatch {
                answered_cmdline: events
                    .iter()
                    .any(|ev| matches!(ev, view_core::events::UiEvent::CmdlineShow { .. })),
            },
            _ => Self::Other,
        }
    }
}

/// One input received and not yet answered by a frame.
struct PendingInput {
    kind: &'static str,
    /// What the event carried beyond its size: a key's own notation, a
    /// mouse's button and action. Empty for a paste, whose text is the
    /// user's and whose size is the part a log can carry.
    detail: String,
    bytes: usize,
    /// [`mono_us`] at the moment the event reached the loop. Microseconds
    /// because the gap it opens is routinely shorter than the millisecond
    /// every line is stamped in.
    received_us: u128,
    /// Whether the screen was already owed a frame when this input was
    /// dispatched, so the paint that follows cannot be read as this
    /// input's own work.
    dirty_before: bool,
    /// Whether this input's own dispatch dirtied the screen: a key view
    /// answers itself -- a line typed into the palette, a picker, a modal
    /// -- is answered by the frame that paints that surface.
    own_paint: bool,
    /// Whether an engine batch dispatched after this input reached the
    /// engine has been folded, which is what answers a key view forwards.
    engine_answered: bool,
    /// Whether the engine had a batch staged when this input was
    /// dispatched. That batch was folded before the engine saw the key, so
    /// the first one after it is passed over rather than read as its
    /// answer.
    staged_batch: bool,
}

/// How long past the first frame carrying the file's text the `highlight`
/// topic keeps reading the window grid. A bound rather than an
/// expectation: a session whose colours never change would otherwise scan
/// every window cell on every frame for the rest of its life.
const HIGHLIGHT_WATCH: std::time::Duration = std::time::Duration::from_secs(10);

/// How long an input is held waiting for the frame that answers it before
/// it is written out as answered by none.
///
/// The hold has to outlast a burst: `:qa!` is four keys the user types
/// before the engine says anything about the first, and every one of them
/// is waiting on the same answer. What it may not outlast is the gap to an
/// unrelated frame, which would be reported as that keystroke's wait, so
/// the bound is well past any wait a person would sit through and well
/// short of an idle stretch.
const PENDING_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// The three waits a user reports as lag, each closed by the frame that
/// ends it: a keystroke's own wait for the screen, the command palette
/// opening, and the syntax colours arriving on the file opened at launch.
///
/// A recording of those three moments is what a `script -O` capture and
/// the topics before this one could not give: the capture holds no input
/// timing at all, and no topic recorded a key, the palette or a highlight.
/// Every method below returns on [`capturing`] first, so a session with no
/// `VIEW_LOG` allocates nothing here, holds no input, and never reads a
/// cell.
///
/// A key is closed by the first frame whose bytes reached the terminal
/// *and* whose cause is that key's own dispatch -- either the fold of the
/// key itself dirtied the screen, or an engine batch folded after the key
/// reached the engine did. A timer, a notice and a batch the engine had
/// already staged close nothing: on a login-shaped config a `:` was
/// credited with a float about something else that had opened before the
/// palette was even requested, and the reading was that float's own wait
/// rather than the one the wire showed for the palette.
#[derive(Default)]
pub struct FeltLog {
    pending: Vec<PendingInput>,
    palette_open: bool,
    palette_owes_paint: bool,
    /// Whether the open the last `palette` line reported was a guess, so
    /// the `cmdline_show` answering it is written up as the reconcile it
    /// is rather than passed over as "still open".
    palette_speculated: bool,
    /// Whether the batch now being reported on answered the guess, set from
    /// the batch's own `cmdline_show` and spent by the `palette` line it
    /// decides.
    palette_answered: bool,
    /// The highlight ids the window's text carried on the frame it first
    /// appeared on, which every later frame is compared against. `None`
    /// until that frame.
    text_hls: Option<Vec<u64>>,
    /// The one window grid the topic reads, pinned at the frame the file's
    /// text first appeared on. `None` until then.
    text_grid: Option<view_core::grid::registry::GridId>,
    /// The reading of the millisecond clock that frame was written at, which
    /// [`HIGHLIGHT_WATCH`] runs from.
    text_at: u128,
    /// Whether any frame has already reported colours the text arrived
    /// without, which is what the watch's own closing line answers.
    recoloured: bool,
    highlight_closed: bool,
    /// Where the lines go when a test drives the wiring rather than the
    /// rule functions. [`SINK`] is a process-wide `OnceLock` and a sibling
    /// test asserts what a session with none open does, so a test that
    /// needs a sink open cannot use that one.
    #[cfg(test)]
    captured: Option<std::sync::Arc<Mutex<Vec<String>>>>,
}

impl FeltLog {
    /// Whether this recorder writes anything -- the process-wide sink, or a
    /// test's own.
    fn capturing(&self) -> bool {
        #[cfg(test)]
        if self.captured.is_some() {
            return true;
        }
        capturing()
    }

    /// A recorder writing into a `Vec` of its own, for a test driving the
    /// wiring -- `note_input` through `note_dispatched` to `note_pass` --
    /// rather than the rule functions each of those calls.
    #[cfg(test)]
    fn recording() -> (Self, std::sync::Arc<Mutex<Vec<String>>>) {
        let lines = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut felt = Self::default();
        felt.captured = Some(std::sync::Arc::clone(&lines));
        (felt, lines)
    }

    /// One line, to whichever sink this recorder holds.
    fn emit(&self, topic: &str, payload: &str) {
        #[cfg(test)]
        if let Some(lines) = &self.captured {
            lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(format!("{topic} {payload}"));
            return;
        }
        log(topic, payload);
    }

    /// Holds one key, paste or mouse event until the next frame reaches the
    /// terminal.
    ///
    /// Read before the fold rather than after it, so the reading is when
    /// the event arrived and the fold's own cost sits inside the wait the
    /// line reports. Every other `Msg` is a deliberate no-op: nothing else
    /// is a thing the user did.
    pub fn note_input(
        &mut self,
        msg: &view_core::msg::Msg,
        model: &view_core::model::Model,
        staged: impl FnOnce() -> bool,
    ) {
        use view_core::msg::Msg;
        if !self.capturing() {
            return;
        }
        let (kind, detail, bytes) = match msg {
            Msg::Key(key) => (
                "key",
                format!(" notation={:?}", key.notation),
                key.notation.len(),
            ),
            Msg::Paste(text) => ("paste", String::new(), text.len()),
            Msg::Mouse(mouse) => (
                "mouse",
                format!(
                    " button={} action={} modifier={:?}",
                    mouse.button, mouse.action, mouse.modifier
                ),
                mouse.button.len() + mouse.action.len() + mouse.modifier.len(),
            ),
            _ => return,
        };
        let received_us = mono_us();
        // the arrival of the next input closes nothing of its own: a burst
        // is several keys waiting on one answer, and each is closed by the
        // frame that answers it
        self.close_expired(received_us);
        self.pending.push(PendingInput {
            kind,
            detail,
            bytes,
            received_us,
            dirty_before: model.dirty,
            own_paint: false,
            engine_answered: false,
            staged_batch: staged(),
        });
    }

    /// Writes the line for every input this frame is the answer to, and
    /// holds the rest: a key the frame's own cause has nothing to do with
    /// is still waiting, and is closed by the frame that does answer it or
    /// by [`close_unanswered`](Self::close_unanswered).
    fn close_answered(&mut self, flushed_us: u128) {
        let mut closed: Vec<(&'static str, String)> = Vec::new();
        self.pending.retain(|input| {
            if !input.own_paint && !input.engine_answered {
                return true;
            }
            closed.push((
                input.kind,
                format!(
                    "bytes={}{} received={} waited_us={}",
                    input.bytes,
                    input.detail,
                    input.received_us / 1000,
                    flushed_us.saturating_sub(input.received_us)
                ),
            ));
            false
        });
        for (topic, payload) in closed {
            self.emit(topic, &payload);
        }
    }

    /// Writes out every input that has now waited longer than any answer
    /// could be, so the next frame -- which can be minutes away and about
    /// something else -- is never reported as that keystroke's wait.
    fn close_expired(&mut self, now_us: u128) {
        let deadline = PENDING_DEADLINE.as_micros();
        let expired = self
            .pending
            .iter()
            .position(|input| now_us.saturating_sub(input.received_us) < deadline)
            .unwrap_or(self.pending.len());
        if expired == 0 {
            return;
        }
        // `pending` is in arrival order, so everything before the first
        // input still inside the deadline is past it
        let held = self.pending.split_off(expired);
        self.close_unanswered();
        self.pending = held;
    }

    /// Writes the line for every input the loop answered with no frame.
    ///
    /// Read from [`close_expired`](Self::close_expired) and when the loop
    /// ends, and from nowhere else: a key view forwards to the engine is
    /// answered by the frame the engine's own redraw produces, which is one
    /// pass later or several and can be several keys later still.
    fn close_unanswered(&mut self) {
        for input in std::mem::take(&mut self.pending) {
            self.emit(input.kind, &unanswered(&input));
        }
    }

    /// Writes out every input whose own fold dirtied the screen and whose
    /// render then found nothing to write.
    ///
    /// The pass that was its answer put nothing in front of the user, so the
    /// key is answered by no frame -- held instead, its `own_paint` mark
    /// still set, it is closed by whatever flushes next (a toast tick, a
    /// notice) with that frame's stamp. An input an engine batch has also
    /// marked keeps waiting: the frame that batch drives is still owed.
    fn close_unpainted(&mut self) {
        let mut closed: Vec<(&'static str, String)> = Vec::new();
        self.pending.retain(|input| {
            if !input.own_paint || input.engine_answered {
                return true;
            }
            closed.push((input.kind, unanswered(input)));
            false
        });
        for (topic, payload) in closed {
            self.emit(topic, &payload);
        }
    }

    /// What the message just dispatched owes the topics: the `palette`
    /// topic's two open-side lines, and which pending input the next frame
    /// answers.
    ///
    /// Read after the fold, because both readings are of the state the fold
    /// produced. What decides whether the typed `:` reaches the palette
    /// rather than nvim's own one-line cmdline is state: the feature's own
    /// switch, and whether a prompt overlay already owns the same typed
    /// text ([`palette_shown`]). And what decides whether the next frame is
    /// an answer to a key is what dirtied the screen -- the key's own fold,
    /// or a batch the engine sent after it.
    pub fn note_dispatched(&mut self, was: Dispatch, model: &view_core::model::Model) {
        if !self.capturing() {
            return;
        }
        match was {
            Dispatch::Input => {
                if let Some(input) = self.pending.last_mut() {
                    input.own_paint = model.dirty && !input.dirty_before;
                }
            }
            Dispatch::EngineBatch { answered_cmdline } => {
                self.palette_answered |= answered_cmdline;
                self.note_engine_batch();
            }
            // a timer, a notice or a reply on a chain of view's own: the
            // frame it draws answers whatever asked for it rather than a
            // key the user is waiting on
            Dispatch::Other => {}
        }
        self.note_palette(model, false);
    }

    /// The `palette` topic's own transitions, written wherever they happen.
    ///
    /// Five lines rather than two, because a reader comparing the log
    /// against the wire has to be able to tell a guess from an answer. A
    /// `:` view put the palette up for before the engine had answered is an
    /// open with no `cmdline_show` behind it (`open requested speculated`);
    /// the `cmdline_show` that answers one changes nothing on screen and
    /// would otherwise be written up as nothing at all (`reconciled`); a
    /// guess the backstop took back comes off on a pass no message
    /// dispatched, which is what `off_the_clock` names (`closed expired`);
    /// and a guess the evidence took back -- a cursor move on the grid the
    /// `:` was typed on, a mode change out of the gate's modes -- comes off
    /// at a dispatch like a real close does, so it says which it was
    /// (`closed withdrawn`). What parts those two is whether the batch just
    /// dispatched carried the `cmdline_show`: the settled model cannot say,
    /// because a `:` and an `<Esc>` typed inside one flush leave no command
    /// line and no guess, which is a withdrawal's own end state.
    fn note_palette(&mut self, model: &view_core::model::Model, off_the_clock: bool) {
        // the flag describes the batch this call is reporting on, so it is
        // spent here whether or not a line comes of it
        let answered = std::mem::take(&mut self.palette_answered);
        let open = palette_shown(model);
        let speculated = model.engine.cmdline_speculated.is_some();
        if open == self.palette_open && speculated == self.palette_speculated {
            return;
        }
        if !open && !self.palette_open {
            // a guess made while a prompt overlay held the typed text was
            // never a palette on screen, so neither is its withdrawal
            self.palette_speculated = speculated;
            return;
        }
        let line = match (self.palette_open, open) {
            (_, false) if off_the_clock => "closed expired",
            (_, false) if self.palette_speculated && !answered => "closed withdrawn",
            (_, false) => "closed",
            (true, true) => "reconciled",
            (false, true) if speculated => "open requested speculated",
            (false, true) => "open requested",
        };
        if open != self.palette_open {
            self.palette_open = open;
            self.palette_owes_paint = open;
        }
        self.palette_speculated = speculated;
        self.emit("palette", line);
    }

    /// One engine batch, against every input still waiting for a frame.
    ///
    /// The first batch after an input the engine had already staged one for
    /// is passed over: it was folded before the engine had the key, so what
    /// it paints is not that key's answer.
    fn note_engine_batch(&mut self) {
        for input in &mut self.pending {
            if input.staged_batch {
                input.staged_batch = false;
            } else {
                input.engine_answered = true;
            }
        }
    }

    /// Every line the pass that just ended closes.
    ///
    /// `flushed` is whether that pass's frame reached the terminal --
    /// `Term::draw_surface`'s own reading, not `model.dirty`: a pass that
    /// rendered and found nothing to write put nothing in front of the
    /// user, and a key closed by one is written up with a wait that ended
    /// on a screen nobody saw change. Read after the write rather than
    /// before it, so a wait reported here is a wait that ended: the render
    /// and the frame's own single write both sit inside it. The three
    /// `startup` milestones at the same call site are stamped before the
    /// render instead, so a reading taken across the two is one frame's
    /// paint apart.
    pub fn note_pass(&mut self, model: &view_core::model::Model, flushed: bool) {
        if !self.capturing() {
            return;
        }
        if !flushed {
            self.close_unpainted();
            return;
        }
        let flushed_us = mono_us();
        let flushed = flushed_us / 1000;
        self.close_answered(flushed_us);
        self.close_expired(flushed_us);
        // after the dispatches of this pass and before the paint line: what
        // the palette lost between the last message and here is the age
        // bound's doing, since `expire_speculation` is the only thing the
        // loop runs in that gap
        self.note_palette(model, true);
        if self.palette_owes_paint && self.palette_open {
            self.palette_owes_paint = false;
            self.emit("palette", "painted");
        }
        self.note_highlight(model, flushed);
    }

    /// The `highlight` topic's two lines: the frame the file's text first
    /// reached the terminal on, and the first later frame whose window text
    /// carries a highlight id that frame did not.
    ///
    /// Growth in the set rather than the presence of a non-default id: a
    /// config with `'number'` on draws its gutter in `LineNr` from the very
    /// first frame, so a reading of "any id but the default" answers the
    /// moment the file appeared and never the moment it was coloured. Each
    /// line carries how many ids the frame held, so a file that arrived
    /// already coloured says so on the first line and writes no second one.
    ///
    /// Every growth, not the first alone: on a login-shaped config the
    /// colours arrive in waves, and a topic that closed on the first one
    /// dated the earliest pass and left the row's own final colours -- which
    /// can be most of a second behind it -- recorded nowhere. Each line
    /// names the ids that arrived and the foreground each resolves to, which
    /// is what says whose pass it was.
    ///
    /// `arrived=` on a recolour line is when the reader thread folded the
    /// batch that carried those ids ([`FOLDED_MS`]), against the line's own
    /// stamp at the paint: read together they say whether a colour the user
    /// waited for was sent late or held.
    fn note_highlight(&mut self, model: &view_core::model::Model, flushed: u128) {
        if self.highlight_closed {
            return;
        }
        let Some((grid, ids)) = window_text_hls(model, self.text_grid) else {
            // a pinned grid the registry no longer holds is gone for good
            // (`:only`, a window close, a layout the config rebuilds), so
            // every later frame would read nothing and the topic would write
            // neither a further wave nor its closing line with nothing saying
            // why. A grid that is merely blank -- `window_text_hls` answers
            // `None` for one with no non-blank cell, which a `grid_clear` from
            // a `:redraw!` or a colorscheme reload leaves for a frame -- is
            // still the window this watch is on, and closing on it would end
            // the watch on the exact moment it exists to record
            let gone = self
                .text_grid
                .filter(|pinned| model.engine.grids().grid(*pinned).is_none());
            if let Some(pinned) = gone {
                self.highlight_closed = true;
                self.emit(
                    "highlight",
                    &format!(
                        "window gone grid={} after={}",
                        pinned.0,
                        flushed.saturating_sub(self.text_at)
                    ),
                );
            }
            return;
        };
        let Some(base) = &self.text_hls else {
            self.emit(
                "highlight",
                &format!("file text flushed grid={} hl-ids={}", grid.0, ids.len()),
            );
            self.text_hls = Some(ids);
            self.text_grid = Some(grid);
            self.text_at = flushed;
            return;
        };
        let added: Vec<u64> = ids
            .iter()
            .copied()
            .filter(|id| !base.contains(id))
            .collect();
        if !added.is_empty() {
            let was = base.len();
            let since = flushed.saturating_sub(self.text_at);
            let colours = added
                .iter()
                .map(|id| {
                    let fg = model
                        .engine
                        .hl()
                        .attr(*id)
                        .and_then(|attr| attr.fg)
                        .map_or_else(|| "none".to_string(), |fg| format!("{fg:06x}"));
                    format!("{id}/fg={fg}")
                })
                .collect::<Vec<_>>()
                .join(",");
            self.emit(
                "highlight",
                &format!(
                    "window text recoloured hl-ids={} was={was} after={since} \
                     added={colours} arrived={}",
                    ids.len(),
                    FOLDED_MS.load(std::sync::atomic::Ordering::Relaxed),
                ),
            );
            self.text_hls = Some(ids);
            self.recoloured = true;
        } else if flushed.saturating_sub(self.text_at) > HIGHLIGHT_WATCH.as_millis() {
            self.highlight_closed = true;
            if !self.recoloured {
                self.emit("highlight", "window text unchanged for the whole watch");
            }
        }
    }
}

impl Drop for FeltLog {
    /// Closes whatever the last pass left open, so an input the session
    /// ended on is a line in the log rather than a reading nothing wrote.
    fn drop(&mut self) {
        if !self.capturing() {
            return;
        }
        self.close_unanswered();
    }
}

/// One input's line for a frame that never came: the same fields a closed
/// input writes, with `flush=none` where its wait would be.
fn unanswered(input: &PendingInput) -> String {
    format!(
        "bytes={}{} received={} flush=none",
        input.bytes,
        input.detail,
        input.received_us / 1000
    )
}

/// Whether the typed cmdline is what the palette is drawing, a `:` view has
/// speculated a command line for included.
///
/// The same answers `view_surface::render` reads to decide it, and
/// read here rather than inferred from the keystroke: `:` typed into a
/// session whose palette is switched off draws nvim's own one-line cmdline,
/// and one typed while a prompt overlay holds the stack draws that
/// overlay's input line instead.
fn palette_shown(model: &view_core::model::Model) -> bool {
    use view_core::model::OverlayKind;
    (model.engine.cmdline.is_some() || model.engine.cmdline_speculated.is_some())
        && model.palette_enabled
        && !matches!(
            model.overlays().last().map(|open| &open.kind),
            Some(OverlayKind::Prompt(_))
        )
}

/// The one window showing the file, and the highlight ids on every
/// non-blank cell of it -- or `None` while that window has drawn no text.
///
/// `pinned` is the grid a previous frame already answered with, and it is
/// read back unconditionally: the topic compares one window against
/// itself, so a split or a tree pane changing colour is not the file
/// being recoloured.
///
/// With nothing pinned yet the window is the one nvim has the cursor in,
/// which at launch is the window holding the file named on the command
/// line -- nvim opens that file in the current window and leaves the
/// cursor there. A cursor in something other than a placed window (a
/// float, a message grid) falls back to the first placed window carrying
/// text, and a session with no placed window at all has its file on the
/// global grid and is read there, which is the split
/// [`window_text_painted`](view_core::grid::GridRegistry::window_text_painted)
/// already makes.
///
/// Latency consequence: one pass over that one window's cells per flush,
/// and only while the `highlight` topic still owes a line -- at most
/// [`HIGHLIGHT_WATCH`] past the frame the file appeared on, and never at
/// all without `VIEW_LOG`.
fn window_text_hls(
    model: &view_core::model::Model,
    pinned: Option<view_core::grid::registry::GridId>,
) -> Option<(view_core::grid::registry::GridId, Vec<u64>)> {
    use view_core::grid::registry::{GridId, PaneKind, GLOBAL_GRID};
    let grids = model.engine.grids();
    let placed: Vec<GridId> = grids
        .panes_in_z_order()
        .into_iter()
        .filter(|pane| matches!(pane.kind, PaneKind::Window) && pane.id != GLOBAL_GRID)
        .map(|pane| pane.id)
        .collect();
    let read_ids = |id: GridId| -> Option<Vec<u64>> {
        let grid = grids.grid(id)?;
        let (width, height) = grid.size();
        let mut ids: Vec<u64> = Vec::new();
        for row in 0..height {
            for col in 0..width {
                let Some(cell) = grid.cell(row, col) else {
                    continue;
                };
                if cell.text.trim().is_empty() {
                    continue;
                }
                if !ids.contains(&cell.hl_id) {
                    ids.push(cell.hl_id);
                }
            }
        }
        (!ids.is_empty()).then_some(ids)
    };
    if let Some(id) = pinned {
        return read_ids(id).map(|ids| (id, ids));
    }
    let cursor = grids.cursor_grid().filter(|id| placed.contains(id));
    if let Some(id) = cursor {
        return read_ids(id).map(|ids| (id, ids));
    }
    for id in placed {
        if let Some(ids) = read_ids(id) {
            return Some((id, ids));
        }
    }
    read_ids(GLOBAL_GRID).map(|ids| (GLOBAL_GRID, ids))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// `init`/`log` touch process-wide `OnceLock`s, so every test in this
    /// module that calls `init` must run against a distinct process --
    /// `cargo test` isolates each `#[test]` in its own process only when
    /// run as a `cargo test --workspace` binary per test target, which is
    /// exactly the case here since `OnceLock` is otherwise unresettable
    /// within one process. Kept to one `init` call in the whole module --
    /// the test below asserting what a session with no log hands the
    /// engine -- beside tests that exercise the pure `log_msg` formatting
    /// without ever touching the global sink (no `init` call, so `log`'s
    /// `SINK.get()` sees `None` and every `log` call inside `log_msg` is
    /// the zero-overhead no-op path).
    #[test]
    fn log_msg_with_no_sink_initialized_is_the_zero_overhead_no_op() {
        // no init() call: SINK is whatever an earlier test in this same
        // process may have left it at (None, since no other test in this
        // module calls init with a real path), so this only asserts the
        // call does not panic and produces no observable side effect this
        // test can detect -- the actual file-writing path is covered by
        // the end-to-end smoke test the fix-wave report documents (a real
        // `VIEW_LOG=<path>` run against the built binary), since `OnceLock`
        // makes a same-process double-init test meaningless.
        log_msg(&view_core::msg::Msg::Redraw(vec![
            view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(1),
                bg: None,
                sp: None,
            },
        ]));
    }

    /// The writer handed to `view-engine` is that crate's whole answer to
    /// "is anything capturing this", so a session that opened no log has
    /// to leave it uninstalled: installed anyway, the engine formats a
    /// payload per undecodable event and hands it to a writer that drops
    /// it.
    ///
    /// Reads the environment and never writes it: `init` resolves
    /// `VIEW_LOG` itself, and a test that unset the variable would race
    /// every other test in this binary that reads one.
    #[test]
    fn a_session_that_opened_no_log_installs_no_engine_diagnostics() {
        assert!(
            std::env::var_os("VIEW_LOG").is_none(),
            "this test is about the session that captures nothing, so it \
             has to run without VIEW_LOG set"
        );

        init(Instant::now());

        assert!(
            !view_engine::diagnostics_installed(),
            "a writer installed here is a formatted payload per event for \
             a log nobody opened"
        );
    }

    /// Every event the `layout` topic keeps has a line to write, and every
    /// event it does not keep has none.
    ///
    /// The two halves are separate functions -- one filters the batch
    /// ahead of the fold, the other formats after it -- so a member added
    /// to one and not the other would keep an event that logs nothing, or
    /// format one that never arrives. This walks the whole topic against a
    /// batch carrying one of each, plus the two events nearest to it that
    /// it must not claim: `grid_line`, which the topic excludes by volume,
    /// and an unrecognized wire name that is not the margins event.
    #[test]
    fn every_layout_event_the_topic_keeps_writes_exactly_one_line() {
        use view_core::events::{UiEvent, WinHandle};

        let members = vec![
            UiEvent::GridResize {
                grid: 2,
                width: 30,
                height: 80,
            },
            UiEvent::GridClear { grid: 2 },
            UiEvent::GridDestroy { grid: 3 },
            UiEvent::WinPos {
                grid: 2,
                win: WinHandle(1000),
                startrow: 0,
                startcol: 0,
                width: 30,
                height: 80,
            },
            UiEvent::WinFloatPos {
                grid: 4,
                win: WinHandle(1001),
                anchor_grid: 1,
                zindex: 50,
                compindex: 0,
                screen_row: 2,
                screen_col: 166,
            },
            UiEvent::WinHide { grid: 5 },
            UiEvent::WinClose { grid: 3 },
            UiEvent::WinViewportMargins {
                grid: 6,
                win: WinHandle(1002),
                top: 1,
                bottom: 0,
                left: 0,
                right: 0,
            },
        ];
        let outsiders = vec![
            UiEvent::GridLine {
                grid: 2,
                row: 0,
                col_start: 0,
                cells: Vec::new(),
            },
            UiEvent::Unknown {
                name: "set_title".to_string(),
            },
        ];

        let model = view_core::model::Model::new();
        for ev in &members {
            assert!(
                is_layout_event(ev),
                "the topic must keep {ev:?} for the fold to have anything to write"
            );
            let payload =
                layout_payload(&model, ev).expect("a kept event must have a line to write");
            assert_eq!(
                payload.lines().count(),
                1,
                "one line per event, not {payload:?}"
            );
            assert!(
                payload.contains("grid="),
                "a line naming no grid answers nothing the topic exists for: \
                 {payload:?}"
            );
        }
        for ev in &outsiders {
            assert!(!is_layout_event(ev), "the topic must not keep {ev:?}");
            assert!(
                layout_payload(&model, ev).is_none(),
                "a line written for an event the topic never keeps: {ev:?}"
            );
        }
    }

    /// A float's line answers whether view is holding it off the screen,
    /// not only where nvim put it: a withheld float paints no cell, so a
    /// reader working out which pane owned one has to be able to rule it
    /// out.
    #[test]
    fn a_floats_layout_line_carries_views_own_hold() {
        use view_core::events::{UiEvent, WinHandle};
        use view_core::grid::registry::{GridEvent, GridId};

        let float = UiEvent::WinFloatPos {
            grid: 4,
            win: WinHandle(1001),
            anchor_grid: 1,
            zindex: 50,
            compindex: 0,
            screen_row: 2,
            screen_col: 166,
        };
        let mut model = view_core::model::Model::new();
        model.engine.apply_grid_event(GridEvent::Float {
            grid: GridId(4),
            anchor_grid: GridId(1),
            screen_row: 2,
            screen_col: 166,
            zindex: 50,
            compindex: 0,
        });
        let shown = layout_payload(&model, &float).unwrap();
        assert!(shown.contains("withheld=false"), "{shown}");

        assert!(model.engine.withhold_float(GridId(4), true));
        let held = layout_payload(&model, &float).unwrap();
        assert!(held.contains("withheld=true"), "{held}");
    }

    /// The payloads that have no bound of their own never reach the log at
    /// full size, and say how much they dropped. `log_msg` runs on the
    /// dispatch thread under a process-wide mutex, so a proposal carrying a
    /// large file would otherwise be a megabyte-scale write between two
    /// frames -- and a log written to be handed over would carry the file's
    /// whole contents with it.
    #[test]
    fn an_unbounded_ai_payload_never_reaches_the_log_at_full_size() {
        use view_core::native::ai_event::AiEvent;

        let huge = "x".repeat(200_000);
        let proposal = ai_payload(&AiEvent::DiffProposed {
            request_id: 1,
            path: std::path::PathBuf::from("/tmp/big.rs"),
            old_text: None,
            new_text: huge.clone(),
        });
        assert!(
            proposal.len() < PAYLOAD_CAP * 4 && proposal.contains("new_bytes: 200000"),
            "a proposal must log its size, not its text: {proposal}"
        );

        let write = ai_payload(&AiEvent::FsWriteRequested {
            request_id: 2,
            path: std::path::PathBuf::from("/tmp/big.rs"),
            content: huge.clone(),
        });
        assert!(
            write.len() < PAYLOAD_CAP * 4 && write.contains("bytes: 200000"),
            "an agent write must log its size, not its content: {write}"
        );

        let chunk = ai_payload(&AiEvent::MessageChunk {
            message_id: Some("m1".to_string()),
            text: huge.clone(),
            from_agent: true,
        });
        assert!(
            chunk.len() < PAYLOAD_CAP * 4 && chunk.contains("+199880B"),
            "a chunk must be capped and count what it dropped: {chunk}"
        );

        let crashed = ai_payload(&AiEvent::SessionCrashed {
            message: huge.clone(),
        });
        assert!(
            crashed.len() < PAYLOAD_CAP * 4 && crashed.contains("+199880B"),
            "a crash message is the agent's own error text and is capped \
             like the rest of it: {crashed}"
        );

        let ready = ai_payload(&AiEvent::SessionReady {
            session_id: huge.clone(),
        });
        assert!(
            ready.len() < PAYLOAD_CAP * 4 && ready.contains("+199880B"),
            "a session id is opaque to this process and bounded by nothing \
             on this side of the wire: {ready}"
        );

        let permission = ai_payload(&AiEvent::PermissionRequested {
            request_id: 3,
            tool_call_id: "call_1".to_string(),
            title: Some(huge.clone()),
            tool_kind: Some(huge.clone()),
            options: Vec::new(),
        });
        assert!(
            permission.len() < PAYLOAD_CAP * 4 && permission.contains("+199880B"),
            "a permission title must be capped like every other title: {permission}"
        );

        // Every dimension of the options list is the agent's to choose:
        // the id, the name, and how many there are.
        let overloaded = ai_payload(&AiEvent::PermissionRequested {
            request_id: 4,
            tool_call_id: "call_2".to_string(),
            title: None,
            tool_kind: None,
            options: vec![
                view_core::native::ai_event::PermissionOption {
                    option_id: huge.clone(),
                    name: huge.clone(),
                    kind: view_core::native::ai_event::PermissionOptionKind::AllowOnce,
                };
                40
            ],
        });
        assert!(
            overloaded.len() < PAYLOAD_CAP * 40 && overloaded.contains("+32 more"),
            "options must be capped in id, name, and count: {} bytes",
            overloaded.len()
        );

        // The wire replaces the whole plan on every update, so its size is
        // the model's to choose, not the shape's.
        let plan = ai_payload(&AiEvent::PlanUpdated {
            entries: vec![
                view_core::native::ai_event::PlanEntry {
                    content: huge,
                    priority: view_core::native::ai_event::PlanEntryPriority::High,
                    status: view_core::native::ai_event::PlanEntryStatus::Pending,
                };
                4
            ],
        });
        assert_eq!(plan, "PlanUpdated { entries: 4 item(s) }");

        // ... and a payload that is already bounded is logged whole, or the
        // cap would be answering a triage question with a truncation.
        let ended = ai_payload(&AiEvent::TurnEnded {
            stop_reason: view_core::native::ai_event::StopReason::Cancelled,
        });
        assert_eq!(ended, "TurnEnded { stop_reason: Cancelled }");
    }

    /// The breadcrumb the frozen-session forensics did not have: five
    /// identical requests in the log and nothing saying what was answered
    /// to any of them. The `option_id` is what a reader matches against the
    /// `PermissionRequested` line above it.
    #[test]
    fn an_answered_permission_logs_the_request_it_answers_and_the_option_it_named() {
        use view_core::native::ai_event::{AiCommand, PermissionOutcome};

        let selected = ai_command_payload(&AiCommand::AnswerPermission {
            request_id: 7,
            outcome: PermissionOutcome::Selected {
                option_id: "allow_always".to_string(),
            },
        });
        assert_eq!(
            selected,
            "AnswerPermission { request_id: 7, outcome: Selected { option_id: \"allow_always\" } }"
        );
        assert_eq!(
            ai_command_payload(&AiCommand::AnswerPermission {
                request_id: 8,
                outcome: PermissionOutcome::Cancelled,
            }),
            "AnswerPermission { request_id: 8, outcome: Cancelled }"
        );
    }

    /// An outbound command carrying free text or a whole file is bounded
    /// the same way every inbound payload is -- a read reply is a file the
    /// agent asked for, and the log is what a user is asked to attach to a
    /// bug report.
    #[test]
    fn an_unbounded_outbound_payload_never_reaches_the_log_at_full_size() {
        use view_core::native::ai_event::AiCommand;

        let huge = "x".repeat(200_000);
        let read = ai_command_payload(&AiCommand::FsReadReply {
            request_id: 1,
            result: Ok(huge.clone()),
        });
        assert_eq!(
            read,
            "FsReadReply { request_id: 1, result: Ok(200000 bytes) }"
        );

        let prompt = ai_command_payload(&AiCommand::Prompt {
            text: huge.clone(),
            context: Vec::new(),
        });
        assert!(
            prompt.len() < PAYLOAD_CAP * 4 && prompt.contains("+199880B"),
            "a submitted prompt must be capped like any other free text: {prompt}"
        );
        let submitted = ai_prompt_submit_payload(&huge);
        assert!(
            submitted.len() < PAYLOAD_CAP * 4 && submitted.contains("+199880B"),
            "and so must the prompt handed to the context worker: {submitted}"
        );

        // The error arms carry nvim's own wording, which quotes the path
        // and the text the agent handed it -- a reply that failed must not
        // be the way the whole file reaches the log.
        let read_error = ai_command_payload(&AiCommand::FsReadReply {
            request_id: 2,
            result: Err(view_core::native::ai_event::FsError::Other {
                message: huge.clone(),
            }),
        });
        assert!(
            read_error.len() < PAYLOAD_CAP * 4 && read_error.contains("+199880B"),
            "a failed read must be capped like any other free text: {read_error}"
        );
        let write_error = ai_command_payload(&AiCommand::FsWriteReply {
            request_id: 3,
            result: Err(view_core::native::ai_event::FsError::Other { message: huge }),
        });
        assert!(
            write_error.len() < PAYLOAD_CAP * 4 && write_error.contains("+199880B"),
            "and so must a failed write: {write_error}"
        );
        assert_eq!(
            ai_command_payload(&AiCommand::FsWriteReply {
                request_id: 4,
                result: Ok(()),
            }),
            "FsWriteReply { request_id: 4, result: Ok }"
        );

        assert_eq!(ai_command_payload(&AiCommand::Cancel), "Cancel");
    }

    /// The palette's own three answers, read off the model rather than off
    /// the keystroke: the same `:` reaches nvim's one-line cmdline in a
    /// session whose palette is switched off, and a prompt overlay's input
    /// line while one holds the stack. A line saying "open requested" for
    /// either of those names a surface nobody drew.
    #[test]
    fn the_palette_line_is_owed_only_where_the_palette_is_what_draws_the_cmdline() {
        use view_core::events::UiEvent;

        let mut model = view_core::model::Model::new();
        model.palette_enabled = true;
        assert!(
            !palette_shown(&model),
            "no cmdline is open, so nothing routed anywhere"
        );

        // through the fold, because that is the only thing that builds a
        // `CmdlineState` -- the same `cmdline_show` the typed `:` comes back as
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::Redraw(vec![UiEvent::CmdlineShow {
                content: vec![(0, String::new())],
                pos: 0,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            }]),
        );
        assert!(palette_shown(&model), "the palette is what draws this");

        model.palette_enabled = false;
        assert!(
            !palette_shown(&model),
            "a switched-off palette leaves nvim's own cmdline drawing it"
        );
    }

    /// The speculated open owes its own word. A `:` view put the palette up
    /// for before the engine answered is an open line with no `cmdline_show`
    /// behind it, and the reader comparing this log against the wire has
    /// nothing else to tell the two apart by.
    #[test]
    fn a_speculated_open_says_so_and_the_shown_cmdline_does_not() {
        use view_core::events::UiEvent;
        use view_core::native::speculate::{CmdlineSpeculation, SpecStamp};

        let mut model = view_core::model::Model::new();
        model.palette_enabled = true;
        model.engine.cmdline_speculated = Some(CmdlineSpeculation {
            since: SpecStamp::new(std::time::Duration::ZERO),
            grid: view_core::grid::registry::GLOBAL_GRID,
        });
        assert!(
            palette_shown(&model),
            "the palette is on screen although the engine has said nothing"
        );

        let (mut felt, lines) = FeltLog::recording();
        felt.note_dispatched(Dispatch::Input, &model);
        let written = lines.lock().unwrap().clone();
        assert!(
            written
                .iter()
                .any(|l| l == "palette open requested speculated"),
            "the speculated open must name itself: {written:?}"
        );

        let mut shown = view_core::model::Model::new();
        shown.palette_enabled = true;
        let _ = view_core::update::update(
            &mut shown,
            view_core::msg::Msg::Redraw(vec![UiEvent::CmdlineShow {
                content: vec![(0, String::new())],
                pos: 0,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            }]),
        );
        let (mut felt, lines) = FeltLog::recording();
        felt.note_dispatched(engine_batch(false), &shown);
        let written = lines.lock().unwrap().clone();
        assert!(
            written.iter().any(|l| l == "palette open requested"),
            "an engine-shown cmdline claims no speculation: {written:?}"
        );
    }

    /// The two lines the guess owes beside its open: the `cmdline_show`
    /// that answers it, which changes nothing on screen and would otherwise
    /// go unwritten, and the age bound taking it back on a pass no message
    /// dispatched.
    #[test]
    fn a_guess_writes_its_reconcile_and_its_expiry() {
        use view_core::native::speculate::{CmdlineSpeculation, SpecStamp};

        let speculating = || {
            let mut model = view_core::model::Model::new();
            model.palette_enabled = true;
            model.engine.cmdline_speculated = Some(CmdlineSpeculation {
                since: SpecStamp::new(std::time::Duration::ZERO),
                grid: view_core::grid::registry::GLOBAL_GRID,
            });
            model
        };

        let mut model = speculating();
        let (mut felt, lines) = FeltLog::recording();
        felt.note_dispatched(Dispatch::Input, &model);
        model.engine.cmdline_speculated = None;
        model.engine.cmdline = Some(view_core::model::CmdlineState::bare_colon());
        felt.note_dispatched(engine_batch(false), &model);
        let written = lines.lock().unwrap().clone();
        assert!(
            written.iter().any(|l| l == "palette reconciled"),
            "the answer to a guess is the line the topic was extended for: {written:?}"
        );

        let mut model = speculating();
        let (mut felt, lines) = FeltLog::recording();
        felt.note_dispatched(Dispatch::Input, &model);
        model.engine.cmdline_speculated = None;
        felt.note_pass(&model, true);
        let written = lines.lock().unwrap().clone();
        assert!(
            written.iter().any(|l| l == "palette closed expired"),
            "a guess withdrawn on a bare pass leaves the topic reading open: {written:?}"
        );
    }

    /// The evidence path's own word. A guess a cursor move or a mode change
    /// took back comes off at a dispatch, exactly where a real command line
    /// closing does, and a reader comparing the log against the wire has
    /// nothing else to tell a wrong guess from a finished command.
    #[test]
    fn a_guess_the_evidence_took_back_says_withdrawn_and_a_real_close_does_not() {
        use view_core::native::speculate::{CmdlineSpeculation, SpecStamp};

        let mut model = view_core::model::Model::new();
        model.palette_enabled = true;
        model.engine.cmdline_speculated = Some(CmdlineSpeculation {
            since: SpecStamp::new(std::time::Duration::ZERO),
            grid: view_core::grid::registry::GLOBAL_GRID,
        });
        let (mut felt, lines) = FeltLog::recording();
        felt.note_dispatched(Dispatch::Input, &model);
        model.engine.cmdline_speculated = None;
        felt.note_dispatched(engine_batch(false), &model);
        let written = lines.lock().unwrap().clone();
        assert!(
            written.iter().any(|l| l == "palette closed withdrawn"),
            "the wrong guess the evidence caught must name itself: {written:?}"
        );

        let mut model = view_core::model::Model::new();
        model.palette_enabled = true;
        model.engine.cmdline = Some(view_core::model::CmdlineState::bare_colon());
        let (mut felt, lines) = FeltLog::recording();
        felt.note_dispatched(engine_batch(false), &model);
        model.engine.cmdline = None;
        felt.note_dispatched(engine_batch(false), &model);
        let written = lines.lock().unwrap().clone();
        assert!(
            written.iter().any(|l| l == "palette closed"),
            "a command line nvim opened and closed is no guess: {written:?}"
        );
    }

    /// The batch that opens and closes a command line in one flush. `:`
    /// then `<Esc>` inside a single nvim flush -- likeliest on the slow link
    /// the speculated palette is sized for -- settles to no command line and
    /// no guess, which is a withdrawal's own end state, so the batch's own
    /// `cmdline_show` is the only thing that says nvim really opened one.
    #[test]
    fn a_show_and_a_hide_in_one_batch_is_a_close_and_not_a_withdrawal() {
        use view_core::events::UiEvent;
        use view_core::msg::Msg;
        use view_core::native::speculate::{CmdlineSpeculation, SpecStamp};

        let mut model = view_core::model::Model::new();
        model.palette_enabled = true;
        model.engine.cmdline_speculated = Some(CmdlineSpeculation {
            since: SpecStamp::new(std::time::Duration::ZERO),
            grid: view_core::grid::registry::GLOBAL_GRID,
        });
        let (mut felt, lines) = FeltLog::recording();
        felt.note_dispatched(Dispatch::Input, &model);

        // what the fold leaves behind for such a batch: the show installed
        // the real line and withdrew the guess, the hide took the line away
        model.engine.cmdline_speculated = None;
        let batch = Msg::Redraw(vec![
            UiEvent::CmdlineShow {
                content: Vec::new(),
                pos: 0,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            },
            UiEvent::CmdlineHide,
        ]);
        felt.note_dispatched(Dispatch::of(&batch), &model);

        let written = lines.lock().unwrap().clone();
        assert!(
            written.iter().any(|l| l == "palette closed"),
            "a command line nvim really opened is no guess, whatever one batch carried: {written:?}"
        );
        assert!(
            !written.iter().any(|l| l == "palette closed withdrawn"),
            "the guess was answered, not refuted: {written:?}"
        );
    }

    /// The window that holds the file, and not the global grid beside it:
    /// under `ext_multigrid` that grid carries nvim's message area, whose
    /// ids move with every message and would read as the file being
    /// recoloured.
    #[test]
    fn the_highlight_reading_takes_the_placed_window_and_not_the_grid_beside_it() {
        use view_core::grid::registry::{GridEvent, GridId, GLOBAL_GRID};
        use view_core::grid::GridOp;

        let mut model = view_core::model::Model::new();
        assert!(
            window_text_hls(&model, None).is_none(),
            "a session that has drawn nothing has no reading to take"
        );

        let window = GridId(2);
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::Resize {
                width: 8,
                height: 2,
            },
        });
        model.engine.apply_grid_event(GridEvent::Window {
            grid: window,
            startrow: 0,
            startcol: 0,
        });
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: GLOBAL_GRID,
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("E".to_string(), 77, 1)],
            },
        });
        assert!(
            window_text_hls(&model, None).is_none(),
            "a message on the grid beside the window is not the file appearing"
        );

        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("1".to_string(), 9, 1), ("f".to_string(), 0, 2)],
            },
        });
        let (found, first) = window_text_hls(&model, None).expect("the file has text now");
        assert_eq!(found, window, "the window with the file is the one read");
        assert_eq!(first.len(), 2, "the gutter's id and the text's: {first:?}");
        assert!(
            !first.contains(&77),
            "the grid beside the window reached the reading: {first:?}"
        );

        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::PutLine {
                row: 0,
                col_start: 1,
                cells: vec![("f".to_string(), 42, 2)],
            },
        });
        let (_, coloured) = window_text_hls(&model, Some(window)).expect("the file still has text");
        assert!(
            coloured.iter().any(|id| !first.contains(id)),
            "the recolour has to carry an id the first frame did not: \
             {first:?} -> {coloured:?}"
        );

        let beside = GridId(3);
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: beside,
            op: GridOp::Resize {
                width: 4,
                height: 1,
            },
        });
        model.engine.apply_grid_event(GridEvent::Window {
            grid: beside,
            startrow: 0,
            startcol: 8,
        });
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: beside,
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("t".to_string(), 512, 1)],
            },
        });
        let (_, still) =
            window_text_hls(&model, Some(window)).expect("the pinned window still has text");
        assert!(
            !still.contains(&512),
            "a second window's colours reached a reading pinned to the file's: {still:?}"
        );
    }

    /// One engine batch as the loop classifies it, saying whether it
    /// carried the `cmdline_show` that answers a guess.
    fn engine_batch(answered_cmdline: bool) -> Dispatch {
        Dispatch::EngineBatch { answered_cmdline }
    }

    /// One key waiting for the screen, as the loop holds it.
    fn waiting(staged_batch: bool) -> PendingInput {
        PendingInput {
            kind: "key",
            detail: String::new(),
            bytes: 1,
            received_us: 5_000,
            dirty_before: false,
            own_paint: false,
            engine_answered: false,
            staged_batch,
        }
    }

    /// Colours that arrive in waves are each reported, because a topic that
    /// closed on the first wave dated the earliest pass and left the row's
    /// own final colours -- most of a second behind it on the config a lag
    /// was reported on -- recorded nowhere.
    #[test]
    fn every_wave_of_colours_is_reported_and_not_just_the_first() {
        use view_core::grid::registry::{GridEvent, GridId};
        use view_core::grid::GridOp;

        let window = GridId(2);
        let mut model = view_core::model::Model::new();
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::Resize {
                width: 8,
                height: 2,
            },
        });
        model.engine.apply_grid_event(GridEvent::Window {
            grid: window,
            startrow: 0,
            startcol: 0,
        });
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("f".to_string(), 9, 2)],
            },
        });
        let mut felt = FeltLog::default();
        felt.note_highlight(&model, 100);
        for (at, id) in [(150_u128, 42_u64), (500, 77)] {
            model.engine.apply_grid_event(GridEvent::Cells {
                grid: window,
                op: GridOp::PutLine {
                    row: 0,
                    col_start: 0,
                    cells: vec![("f".to_string(), id, 2)],
                },
            });
            felt.note_highlight(&model, at);
            assert!(
                !felt.highlight_closed,
                "the watch closed on the wave at {at} and the ones after it \
                 are recorded nowhere"
            );
            assert!(
                felt.text_hls.as_ref().is_some_and(|ids| ids.contains(&id)),
                "the wave that arrived is not the set the next one is read \
                 against"
            );
        }
        felt.note_highlight(&model, 500 + HIGHLIGHT_WATCH.as_millis() + 1);
        assert!(
            felt.highlight_closed,
            "the watch never ends and the window is scanned for the rest of \
             the session"
        );
    }

    /// A pinned window nvim destroys (`:only`, a window close, a layout the
    /// config rebuilds) says so once and closes the topic, rather than
    /// reading `None` on every later frame and writing neither a further
    /// wave nor its closing line.
    #[test]
    fn a_pinned_window_that_went_away_says_so_and_closes_the_topic() {
        use view_core::grid::registry::{GridEvent, GridId};
        use view_core::grid::GridOp;

        let window = GridId(2);
        let mut model = view_core::model::Model::new();
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::Resize {
                width: 4,
                height: 1,
            },
        });
        model.engine.apply_grid_event(GridEvent::Window {
            grid: window,
            startrow: 0,
            startcol: 0,
        });
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("f".to_string(), 9, 1)],
            },
        });
        let (mut felt, lines) = FeltLog::recording();
        felt.note_highlight(&model, 100);
        assert_eq!(felt.text_grid, Some(window), "the window was never pinned");

        model
            .engine
            .apply_grid_event(GridEvent::Destroy { grid: window });
        felt.note_highlight(&model, 400);
        assert!(
            felt.highlight_closed,
            "the topic goes quiet for the rest of the session with nothing \
             saying why"
        );
        let written = lines.lock().unwrap().clone();
        assert!(
            written
                .iter()
                .any(|l| l == "highlight window gone grid=2 after=300"),
            "the window going away was not written out: {written:?}"
        );
    }

    /// A window that is merely blank -- cleared by a `:redraw!` or a
    /// colorscheme reload, and holding no non-blank cell for a frame -- is
    /// still the window this watch is on. Reported as gone, it ended the
    /// watch on the exact moment the topic exists to record.
    #[test]
    fn a_pinned_window_that_went_blank_keeps_the_watch_open() {
        use view_core::grid::registry::{GridEvent, GridId};
        use view_core::grid::GridOp;

        let window = GridId(2);
        let mut model = view_core::model::Model::new();
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::Resize {
                width: 4,
                height: 1,
            },
        });
        model.engine.apply_grid_event(GridEvent::Window {
            grid: window,
            startrow: 0,
            startcol: 0,
        });
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("f".to_string(), 9, 1)],
            },
        });
        let (mut felt, lines) = FeltLog::recording();
        felt.note_highlight(&model, 100);
        assert_eq!(felt.text_grid, Some(window), "the window was never pinned");

        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::Clear,
        });
        felt.note_highlight(&model, 200);
        assert!(
            !felt.highlight_closed,
            "a blank window closed the watch: {:?}",
            lines.lock().unwrap()
        );

        // the colours the watch is for, arriving on the frame after the clear
        model.engine.apply_grid_event(GridEvent::Cells {
            grid: window,
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("f".to_string(), 12, 1)],
            },
        });
        felt.note_highlight(&model, 300);
        let written = lines.lock().unwrap().clone();
        assert!(
            written.iter().any(|l| l.starts_with(
                "highlight window text recoloured hl-ids=1 was=1 after=200 added=12/"
            )),
            "the wave after the clear was never recorded: {written:?}"
        );
    }

    /// An event this tree's decoder has no arm for is counted under the name
    /// nvim sent it as: 28 of the 567 events in the batch a launch
    /// attribution rests on were counted as `unknown`, which names nothing.
    #[test]
    fn an_unknown_event_is_counted_under_the_name_the_wire_sent() {
        use view_core::events::UiEvent;
        let counts = RedrawCounts::default();
        let census = redraw_census(
            &[
                UiEvent::Unknown {
                    name: "chdir".to_string(),
                },
                UiEvent::Unknown {
                    name: "chdir".to_string(),
                },
                UiEvent::Unknown {
                    name: "suspend".to_string(),
                },
                UiEvent::Unknown {
                    name: "z".repeat(PAYLOAD_CAP + 40),
                },
                UiEvent::Flush,
            ],
            &counts,
        );
        let long = format!("unknown({})=1", capped(&"z".repeat(PAYLOAD_CAP + 40)));
        assert!(
            census.ends_with(&format!(
                "events=5 kinds=unknown(\"chdir\")=2,unknown(\"suspend\")=1,{long},flush=1"
            )),
            "the census throws away the names the wire carried, or writes one \
             the wire chose the length of: {census}"
        );
        assert!(
            long.contains("+40B"),
            "a name off the wire reaches the log uncapped: {long}"
        );
    }

    /// A batch's census counts its cells rather than listing them, which is
    /// what makes "the engine sent nothing" readable at all: the `layout`
    /// topic omits `grid_line` by volume, so an absence of lines there is not
    /// an absence of engine traffic.
    #[test]
    fn a_redraw_census_counts_its_cells_rather_than_listing_them() {
        use view_core::events::UiEvent;
        let counts = RedrawCounts::default();
        let line = |row| UiEvent::GridLine {
            grid: 2,
            row,
            col_start: 0,
            cells: vec![view_core::events::GridCell {
                text: "x".to_string(),
                hl_id: 0,
                repeat: 1,
            }],
        };
        let census = redraw_census(&[line(0), line(1), UiEvent::Flush, line(2)], &counts);
        assert!(
            census.ends_with("events=4 kinds=grid_line=3,flush=1"),
            "the census does not count what the batch carried: {census}"
        );
        let next = redraw_census(&[UiEvent::Flush], &counts);
        let number = |c: &str| {
            c.split_whitespace()
                .find_map(|w| w.strip_prefix("batch="))
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap()
        };
        assert_eq!(
            number(&next),
            number(&census) + 1,
            "consecutive batches are not consecutively numbered: {census} / {next}"
        );
    }

    /// An input the loop answered with no frame is written out and
    /// released, so the next frame -- which can be seconds away and about
    /// something else -- cannot be reported as the wait that keystroke had.
    #[test]
    fn a_key_no_frame_answered_is_closed_and_not_left_for_a_later_flush() {
        let mut felt = FeltLog::default();
        felt.pending.push(waiting(false));
        felt.close_unanswered();
        assert!(
            felt.pending.is_empty(),
            "a key held past the pass that answered nothing is stamped by \
             whatever flushes next"
        );
    }

    /// A frame nothing caused on the key's behalf leaves it waiting.
    ///
    /// The shipped reading this refuses: on a login-shaped config the `:`
    /// was closed by a float that opened well before the palette the user
    /// was waiting for reached the screen.
    #[test]
    fn a_frame_the_key_did_not_cause_leaves_it_waiting() {
        let mut felt = FeltLog::default();
        felt.pending.push(waiting(false));
        felt.close_answered(50_000);
        assert_eq!(
            felt.pending.len(),
            1,
            "a frame with no cause of the key's own closed it anyway"
        );
        felt.note_engine_batch();
        felt.close_answered(64_000);
        assert!(
            felt.pending.is_empty(),
            "the batch the engine sent after the key answered it and the \
             frame it drove left the key open"
        );
    }

    /// A batch the engine had already staged when the key was dispatched
    /// was folded before the engine saw that key, so the frame it drives is
    /// not the key's answer; the next batch is.
    #[test]
    fn a_batch_staged_before_the_key_closes_nothing() {
        let mut felt = FeltLog::default();
        felt.pending.push(waiting(true));
        felt.note_engine_batch();
        felt.close_answered(20_000);
        assert_eq!(
            felt.pending.len(),
            1,
            "a batch the engine staged before the key was written closed it"
        );
        felt.note_engine_batch();
        felt.close_answered(64_000);
        assert!(
            felt.pending.is_empty(),
            "the first batch folded after the key never closed it"
        );
    }

    /// A key view answers itself -- the palette's own typed line, a picker,
    /// a modal -- is closed by the frame that paints that surface, which is
    /// the one the key's own fold dirtied the screen for.
    ///
    /// Driven through `note_input`/`note_dispatched`/`note_pass` rather than
    /// through the rule functions they call: a test that assigns `own_paint`
    /// itself stays green with the `Dispatch::Input` arm deleted.
    #[test]
    fn a_key_view_answers_itself_is_closed_by_its_own_frame() {
        let mut model = view_core::model::Model::new();
        let (mut felt, lines) = FeltLog::recording();
        let key = view_core::msg::Msg::Key(view_core::msg::Key {
            notation: "x".to_string(),
        });
        felt.note_input(&key, &model, || false);
        // what the key's own fold left behind, which is the state
        // `note_dispatched` reads
        model.dirty = true;
        felt.note_dispatched(Dispatch::of(&key), &model);
        felt.note_pass(&model, true);
        assert!(
            felt.pending.is_empty(),
            "a key whose own fold drew the frame was left for a later one"
        );
        let written = lines.lock().unwrap().join("\n");
        assert!(
            written.contains("key bytes=1 notation=\"x\"") && written.contains("waited_us="),
            "the key was closed without its own line: {written}"
        );
    }

    /// The four keys of `:qa!` reach the loop before the engine answers the
    /// first of them, and each is closed by the frame that answered it.
    ///
    /// The shipped reading this refuses: the arrival of the next input
    /// closed whatever was still waiting, so 20 of 20 keys in the published
    /// recording were written `flush=none` and the frame that painted the
    /// palette was credited to the `<CR>` typed after it.
    #[test]
    fn a_burst_of_keys_is_closed_by_the_one_frame_that_answered_it() {
        use view_core::events::UiEvent;
        use view_core::msg::{Key, Msg};

        let mut model = view_core::model::Model::new();
        model.palette_enabled = true;
        let (mut felt, lines) = FeltLog::recording();

        let dispatch = |felt: &mut FeltLog, model: &mut view_core::model::Model, msg: Msg| {
            felt.note_input(&msg, model, || false);
            let was = Dispatch::of(&msg);
            let _ = view_core::update::update(model, msg);
            felt.note_dispatched(was, model);
        };

        for notation in [":", "q", "a", "!"] {
            dispatch(
                &mut felt,
                &mut model,
                Msg::Key(Key {
                    notation: notation.to_string(),
                }),
            );
        }
        assert_eq!(
            felt.pending.len(),
            4,
            "a key still waiting for its answer was closed by the next key"
        );
        assert!(
            lines.lock().unwrap().is_empty(),
            "a burst wrote lines before anything answered it: {:?}",
            lines.lock().unwrap()
        );

        // the engine's answer to the `:`, and the frame that paints it
        dispatch(
            &mut felt,
            &mut model,
            Msg::Redraw(vec![
                UiEvent::CmdlineShow {
                    content: vec![(0, "qa!".to_string())],
                    pos: 3,
                    firstc: ":".to_string(),
                    prompt: String::new(),
                    indent: 0,
                    level: 1,
                },
                UiEvent::Flush,
            ]),
        );
        felt.note_pass(&model, true);
        model.dirty = false;
        let written = lines.lock().unwrap().clone();
        assert_eq!(
            written.iter().filter(|l| l.starts_with("key ")).count(),
            4,
            "the burst was not closed by the frame that answered it: {written:?}"
        );
        assert!(
            written.iter().all(|l| !l.contains("flush=none")),
            "a key the palette frame answered was written up as answered by \
             nothing: {written:?}"
        );
        assert!(
            written.iter().any(|l| l == "palette painted"),
            "the frame that closed the burst is not the palette's: {written:?}"
        );

        // the `<CR>`, typed after that frame, is closed by its own
        dispatch(
            &mut felt,
            &mut model,
            Msg::Key(Key {
                notation: "<CR>".to_string(),
            }),
        );
        felt.note_pass(&model, true);
        assert_eq!(
            felt.pending.len(),
            1,
            "the `<CR>` was closed by a frame with no cause of its own"
        );
        dispatch(
            &mut felt,
            &mut model,
            Msg::Redraw(vec![UiEvent::CmdlineHide, UiEvent::Flush]),
        );
        felt.note_pass(&model, true);
        assert!(
            felt.pending.is_empty(),
            "the batch the engine sent for the `<CR>` never closed it"
        );
        let closing = lines.lock().unwrap().clone();
        assert!(
            closing
                .iter()
                .any(|l| l.starts_with("key bytes=4 notation=\"<CR>\"") && l.contains("waited_us=")),
            "the `<CR>` is not closed by the frame that answered it: {closing:?}"
        );
    }

    /// An empty batch is the drain of a wakeup token for damage that has not
    /// reached a `Flush`: it folds nothing and answers no key.
    ///
    /// Read as an answer, it set `engine_answered` on every waiting input and
    /// the next frame -- a timer, a notice, anything -- closed the key with
    /// that frame's stamp.
    #[test]
    fn an_empty_engine_batch_answers_nothing() {
        use view_core::msg::Msg;
        assert!(matches!(
            Dispatch::of(&Msg::Redraw(Vec::new())),
            Dispatch::Other
        ));
        assert!(matches!(
            Dispatch::of(&Msg::Redraw(vec![view_core::events::UiEvent::Flush])),
            Dispatch::EngineBatch {
                answered_cmdline: false
            }
        ));

        let model = view_core::model::Model::new();
        let (mut felt, lines) = FeltLog::recording();
        felt.pending.push(waiting(false));
        felt.note_dispatched(Dispatch::of(&Msg::Redraw(Vec::new())), &model);
        felt.note_pass(&model, true);
        assert_eq!(
            felt.pending.len(),
            1,
            "an empty batch closed a key: {:?}",
            lines.lock().unwrap()
        );
    }

    /// A pass whose render found nothing to write is not an answer: the key
    /// its own fold dirtied the screen for is written out as answered by no
    /// frame, rather than held for whatever flushes next -- a toast tick, a
    /// notice -- and stamped with that.
    #[test]
    fn a_key_whose_own_pass_wrote_nothing_is_closed_by_that_pass() {
        let model = view_core::model::Model::new();
        let (mut felt, lines) = FeltLog::recording();
        let mut input = waiting(false);
        input.own_paint = true;
        felt.pending.push(input);
        felt.note_pass(&model, false);
        assert!(
            felt.pending.is_empty(),
            "the key is still waiting for a frame its own pass never wrote"
        );
        let written = lines.lock().unwrap().clone();
        assert_eq!(
            written,
            vec!["key bytes=1 received=5 flush=none".to_string()],
            "the pass that wrote nothing did not close the key it answered"
        );

        // an input an engine batch has marked is waiting on that batch's own
        // frame, which this pass is not
        let (mut felt, lines) = FeltLog::recording();
        let mut input = waiting(false);
        input.own_paint = true;
        input.engine_answered = true;
        felt.pending.push(input);
        felt.note_pass(&model, false);
        assert_eq!(
            felt.pending.len(),
            1,
            "a key the engine still owes a frame was closed: {:?}",
            lines.lock().unwrap()
        );
    }

    /// A key no frame ever answers is written out at the deadline, so the
    /// frame that follows -- minutes later and about something else -- is
    /// never reported as that keystroke's wait.
    #[test]
    fn a_key_nothing_answers_closes_at_the_deadline_and_not_before() {
        let (mut felt, lines) = FeltLog::recording();
        felt.pending.push(waiting(false));
        felt.close_expired(5_000 + PENDING_DEADLINE.as_micros() - 1);
        assert_eq!(
            felt.pending.len(),
            1,
            "a key still inside the deadline was closed early"
        );
        felt.close_expired(5_000 + PENDING_DEADLINE.as_micros());
        assert!(
            felt.pending.is_empty(),
            "a key past the deadline is stamped by whatever flushes next"
        );
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains("flush=none")),
            "the expired key was released without a line: {:?}",
            lines.lock().unwrap()
        );
    }

    /// The deadline reads arrival order: a burst whose oldest key has
    /// expired still holds the keys typed inside the window.
    #[test]
    fn an_expiring_burst_holds_the_keys_still_inside_the_deadline() {
        let (mut felt, _lines) = FeltLog::recording();
        felt.pending.push(waiting(false));
        let mut late = waiting(false);
        late.received_us = 5_000 + PENDING_DEADLINE.as_micros();
        felt.pending.push(late);
        felt.close_expired(5_000 + PENDING_DEADLINE.as_micros() + 1);
        assert_eq!(
            felt.pending.len(),
            1,
            "a key typed inside the deadline went out with the one past it"
        );
    }

    /// Every method of the recorder is the zero-overhead no-op without a
    /// sink, which is what lets the input stamp and the whole-grid read sit
    /// on the loop's own paint path at all.
    #[test]
    fn the_felt_recorder_holds_nothing_and_reads_nothing_with_no_sink_open() {
        let model = view_core::model::Model::new();
        let mut felt = FeltLog::default();
        let key = view_core::msg::Msg::Key(view_core::msg::Key {
            notation: ":".to_string(),
        });
        let asked = std::cell::Cell::new(false);
        felt.note_input(&key, &model, || {
            asked.set(true);
            false
        });
        assert!(
            !asked.get(),
            "the engine's damage lock was taken for a log nobody opened"
        );
        assert!(
            felt.pending.is_empty(),
            "an input held for a log nobody opened is an allocation per keystroke"
        );
        felt.note_dispatched(Dispatch::of(&key), &model);
        felt.note_pass(&model, true);
        assert!(
            felt.text_hls.is_none(),
            "the grid was read for a log nobody opened"
        );
    }

    #[test]
    fn log_ui_event_recognizes_every_loggable_variant_without_panicking() {
        log_ui_event(&view_core::events::UiEvent::MsgShow {
            kind: "echoerr".to_string(),
            content: vec![(0, "boom".to_string())],
            replace_last: false,
        });
        log_ui_event(&view_core::events::UiEvent::MsgClear);
        log_ui_event(&view_core::events::UiEvent::DefaultColorsSet {
            fg: Some(0xFFFFFF),
            bg: Some(0x000000),
            sp: None,
        });
        // an event this log has no contract for (e.g. a grid op) must stay
        // a no-op rather than panicking or growing a match arm it has
        // nothing to report for
        log_ui_event(&view_core::events::UiEvent::GridClear { grid: 1 });
    }
}
