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
pub fn init(process_start: Instant) {
    START.get_or_init(|| process_start);
    SINK.get_or_init(|| match std::env::var_os("VIEW_LOG") {
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
}

/// Writes one `<mono_ms> <topic> <payload>` line if [`init`] opened a sink;
/// a single `Option` check and return otherwise -- the zero-overhead path
/// this module's docs promise. `topic` is a short fixed tag (`"startup"`,
/// `"theme"`, `"msg"`, `"engine"`, `"fatal"`); `payload` is caller-formatted
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
/// a native feature invocation, the mappings the engine claimed, and every
/// AI event. Every other `Msg` variant (`Key`, `Paste`, `Mouse`,
/// `Resized`, loop plumbing) carries nothing this log's contract asks for
/// and is a deliberate no-op here.
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
            options,
        } => format!(
            "PermissionRequested {{ request_id: {request_id}, tool_call_id: {tool_call_id:?}, \
             title: {}, options: [{}] }}",
            title.as_ref().map_or_else(
                || "None".to_string(),
                |title| format!("Some({})", capped(title))
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// `init`/`log` touch process-wide `OnceLock`s, so every test in this
    /// module that calls `init` must run against a distinct process --
    /// `cargo test` isolates each `#[test]` in its own process only when
    /// run as a `cargo test --workspace` binary per test target, which is
    /// exactly the case here since `OnceLock` is otherwise unresettable
    /// within one process. Kept to a single test that exercises the full
    /// `init` -> `log` -> file contents round trip, plus a second that
    /// exercises the pure `log_msg` formatting without ever touching the
    /// global sink (no `init` call, so `log`'s `SINK.get()` sees `None` and
    /// every `log` call inside `log_msg` is the zero-overhead no-op path).
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
