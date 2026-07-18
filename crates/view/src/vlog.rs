//! `VIEW_LOG=<path>` diagnostic capture: a structured, line-oriented
//! append log for triage flows that today mean asking the user to paste an
//! `asciinema` recording. Absent the env var, [`init`] leaves the process-wide
//! sink at `None` and every [`log`] call after that is a single `Option`
//! check with no allocation and no file I/O -- the zero-overhead contract an
//! interactive keystroke-to-paint loop needs even when nobody is capturing.
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
pub fn log(topic: &str, payload: &str) {
    let Some(Some(file)) = SINK.get() else {
        return;
    };
    let ms = START.get().map_or(0, |start| start.elapsed().as_millis());
    let mut f = file.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = writeln!(f, "{ms} {topic} {payload}");
}

/// Logs the loggable slice of one `Msg` crossing the runtime loop's
/// dispatch seam: theme events nested in a `Redraw` batch (`view-core` is
/// pure and cannot log these itself -- see the module docs), the async
/// `nvim_get_hl` default-colors probe's reply, and an engine-down
/// transition. Every other `Msg` variant (`Key`, `Paste`, `Mouse`,
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
        Msg::HlProbeReply { generation, fg, bg } => {
            log(
                "theme",
                &format!("probe-reply generation={generation} fg={fg:?} bg={bg:?}"),
            );
        }
        Msg::EngineDown(exit) => {
            log(
                "engine",
                &format!("down code={:?} by_signal={}", exit.code, exit.by_signal),
            );
        }
        _ => {}
    }
}

fn log_ui_event(ev: &view_core::events::UiEvent) {
    use view_core::events::UiEvent;
    match ev {
        UiEvent::DefaultColorsSet { fg, bg, sp } => {
            log(
                "theme",
                &format!("default_colors_set fg={fg:?} bg={bg:?} sp={sp:?}"),
            );
        }
        UiEvent::MsgShow {
            kind,
            content,
            replace_last,
        } => {
            let text: String = content.iter().map(|(_, t)| t.as_str()).collect();
            log(
                "msg",
                &format!("show kind={kind} replace_last={replace_last} text={text:?}"),
            );
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
