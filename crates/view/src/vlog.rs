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
/// any other work, so a log line's `mono_ms` lines up with the shell-paint
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
    let mut f = file.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = writeln!(f, "{ms} {topic} {payload}");
}

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
        Msg::MappingsClaimed { claimed } => {
            log_with("native", || {
                let keys: Vec<String> = claimed
                    .iter()
                    .map(|c| format!("{}={}", c.lhs, c.had_user_mapping))
                    .collect();
                format!("claimed {}", keys.join(","))
            });
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

/// Milliseconds since the origin [`init`] was handed, which is the number
/// every line written here already carries as its own prefix.
///
/// For a caller holding one reading open until a later line can close it:
/// [`FeltLog`] stamps an input when it arrives and writes the line at the
/// flush that answers it, so the wait is a subtraction of two readings
/// taken from this one clock.
#[must_use]
pub fn mono_ms() -> u128 {
    START.get().map_or(0, |start| start.elapsed().as_millis())
}

/// Whether a sink is open, for a call site whose payload cannot be built
/// inside a [`log_with`] closure -- one that has to read the grid, or hold
/// state between two passes of the loop.
#[must_use]
pub fn capturing() -> bool {
    matches!(SINK.get(), Some(Some(_)))
}

/// One input received and not yet answered by a frame.
struct PendingInput {
    kind: &'static str,
    /// What the event carried beyond its size: a key's own notation, a
    /// mouse's button and action. Empty for a paste, whose text is the
    /// user's and whose size is the part a log can carry.
    detail: String,
    bytes: usize,
    received: u128,
}

/// How long past the first frame carrying the file's text the `highlight`
/// topic keeps reading the window grid. A bound rather than an
/// expectation: a session whose colours never change would otherwise scan
/// every window cell on every frame for the rest of its life.
const HIGHLIGHT_WATCH: std::time::Duration = std::time::Duration::from_secs(10);

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
#[derive(Default)]
pub struct FeltLog {
    pending: Vec<PendingInput>,
    palette_open: bool,
    palette_owes_paint: bool,
    /// The highlight ids the window's text carried on the frame it first
    /// appeared on, which every later frame is compared against. `None`
    /// until that frame.
    text_hls: Option<Vec<u64>>,
    /// The reading of [`mono_ms`] that frame was written at, which
    /// [`HIGHLIGHT_WATCH`] runs from.
    text_at: u128,
    highlight_closed: bool,
}

impl FeltLog {
    /// Holds one key, paste or mouse event until the next frame reaches the
    /// terminal.
    ///
    /// Read before the fold rather than after it, so the reading is when
    /// the event arrived and the fold's own cost sits inside the wait the
    /// line reports. Every other `Msg` is a deliberate no-op: nothing else
    /// is a thing the user did.
    pub fn note_input(&mut self, msg: &view_core::msg::Msg) {
        use view_core::msg::Msg;
        if !capturing() {
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
        self.pending.push(PendingInput {
            kind,
            detail,
            bytes,
            received: mono_ms(),
        });
    }

    /// Writes the `palette` topic's two open-side lines off the state the
    /// fold just produced.
    ///
    /// Read after the fold, because what decides whether the typed `:`
    /// reaches the palette rather than nvim's own one-line cmdline is
    /// state: the feature's own switch, and whether a prompt overlay
    /// already owns the same typed text ([`palette_shown`]).
    pub fn note_palette(&mut self, model: &view_core::model::Model) {
        if !capturing() {
            return;
        }
        let open = palette_shown(model);
        if open == self.palette_open {
            return;
        }
        self.palette_open = open;
        self.palette_owes_paint = open;
        log("palette", if open { "open requested" } else { "closed" });
    }

    /// Every line the frame that just reached the terminal closes.
    ///
    /// Read after the write rather than before it, so a wait reported here
    /// is a wait that ended: the render and the frame's own single write
    /// both sit inside it. The three `startup` milestones at the same call
    /// site are stamped before the render instead, so a reading taken
    /// across the two is one frame's paint apart.
    pub fn note_flush(&mut self, model: &view_core::model::Model) {
        if !capturing() {
            return;
        }
        let flushed = mono_ms();
        for input in self.pending.drain(..) {
            log(
                input.kind,
                &format!(
                    "bytes={}{} received={} waited={}",
                    input.bytes,
                    input.detail,
                    input.received,
                    flushed.saturating_sub(input.received)
                ),
            );
        }
        if self.palette_owes_paint && self.palette_open {
            self.palette_owes_paint = false;
            log("palette", "painted");
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
    fn note_highlight(&mut self, model: &view_core::model::Model, flushed: u128) {
        if self.highlight_closed {
            return;
        }
        let Some(ids) = window_text_hls(model) else {
            return;
        };
        let Some(base) = &self.text_hls else {
            log(
                "highlight",
                &format!("file text flushed hl-ids={}", ids.len()),
            );
            self.text_hls = Some(ids);
            self.text_at = flushed;
            return;
        };
        if ids.iter().any(|id| !base.contains(id)) {
            self.highlight_closed = true;
            log(
                "highlight",
                &format!(
                    "window text recoloured hl-ids={} was={} after={}",
                    ids.len(),
                    base.len(),
                    flushed.saturating_sub(self.text_at)
                ),
            );
        } else if flushed.saturating_sub(self.text_at) > HIGHLIGHT_WATCH.as_millis() {
            self.highlight_closed = true;
            log("highlight", "window text unchanged for the whole watch");
        }
    }
}

/// Whether the typed cmdline is what the palette is drawing.
///
/// The same three answers `view_surface::render` reads to decide it, and
/// read here rather than inferred from the keystroke: `:` typed into a
/// session whose palette is switched off draws nvim's own one-line cmdline,
/// and one typed while a prompt overlay holds the stack draws that
/// overlay's input line instead.
fn palette_shown(model: &view_core::model::Model) -> bool {
    use view_core::model::OverlayKind;
    model.engine.cmdline.is_some()
        && model.palette_enabled
        && !matches!(
            model.overlays().last().map(|open| &open.kind),
            Some(OverlayKind::Prompt(_))
        )
}

/// The highlight ids on every non-blank cell of the window holding the
/// file, or `None` while no window has drawn any text yet.
///
/// The windows nvim placed, never the global grid beside them: under
/// `ext_multigrid` that grid carries nvim's own message area, whose ids
/// move with every message and would read as the file being recoloured.
/// A session with no placed window has its file on the global grid and is
/// read there, which is the split
/// [`window_text_painted`](view_core::grid::GridRegistry::window_text_painted)
/// already makes.
///
/// Latency consequence: one pass over the window's cells per flush, and
/// only while the `highlight` topic still owes a line -- at most
/// [`HIGHLIGHT_WATCH`] past the frame the file appeared on, and never at
/// all without `VIEW_LOG`.
fn window_text_hls(model: &view_core::model::Model) -> Option<Vec<u64>> {
    use view_core::grid::registry::{GridId, PaneKind, GLOBAL_GRID};
    let grids = model.engine.grids();
    let mut placed: Vec<GridId> = grids
        .panes_in_z_order()
        .into_iter()
        .filter(|pane| matches!(pane.kind, PaneKind::Window) && pane.id != GLOBAL_GRID)
        .map(|pane| pane.id)
        .collect();
    if placed.is_empty() {
        placed.push(GLOBAL_GRID);
    }
    let mut ids: Vec<u64> = Vec::new();
    for id in placed {
        let Some(grid) = grids.grid(id) else {
            continue;
        };
        let (width, height) = grid.size();
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
    }
    (!ids.is_empty()).then_some(ids)
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
            window_text_hls(&model).is_none(),
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
            window_text_hls(&model).is_none(),
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
        let first = window_text_hls(&model).expect("the file has text now");
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
        let coloured = window_text_hls(&model).expect("the file still has text");
        assert!(
            coloured.iter().any(|id| !first.contains(id)),
            "the recolour has to carry an id the first frame did not: \
             {first:?} -> {coloured:?}"
        );
    }

    /// Every method of the recorder is the zero-overhead no-op without a
    /// sink, which is what lets the input stamp and the whole-grid read sit
    /// on the loop's own paint path at all.
    #[test]
    fn the_felt_recorder_holds_nothing_and_reads_nothing_with_no_sink_open() {
        let model = view_core::model::Model::new();
        let mut felt = FeltLog::default();
        felt.note_input(&view_core::msg::Msg::Key(view_core::msg::Key {
            notation: ":".to_string(),
        }));
        assert!(
            felt.pending.is_empty(),
            "an input held for a log nobody opened is an allocation per keystroke"
        );
        felt.note_palette(&model);
        felt.note_flush(&model);
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
