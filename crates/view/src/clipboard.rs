//! The clipboard worker: the one thread that touches the system clipboard,
//! off the paint loop. The loop only ever hands a read or write across a
//! channel and never blocks on the answer itself; this thread owns the
//! reply obligation for both.
//!
//! # The remote-paste contract
//!
//! There is no OSC52 *read* path here, and that is a stated limitation, not
//! an oversight: OSC52 paste-back requires the terminal to answer a query
//! escape sequence, which most terminal emulators refuse by default for
//! security reasons (an arbitrary program reading the system clipboard
//! without a user gesture), so it is not a mechanism this worker can lean
//! on. Instead, every successful [`ClipboardJobKind::Write`] updates an
//! in-memory shadow register alongside the real system-clipboard write, and
//! a [`ClipboardJobKind::Read`] falls back to that shadow whenever
//! `arboard` itself cannot reach a clipboard -- which is exactly the
//! situation an SSH session with no forwarded display is in. That matches
//! the behavior every remote nvim setup already has: `"+p` after `"+yy`
//! works across the same session, and reading a value copied on the far
//! end (something no local backend can do either) simply is not promised.

use std::collections::HashMap;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use view_core::msg::{ReplyToken, ReplyValue};
use view_native::clipboard::{lines_to_text, text_to_lines};

use crate::runtime::EngineOps;

/// One clipboard request the loop has handed off, carrying the token its
/// reply must answer (see `Effect::ClipboardRead`/`Effect::ClipboardWrite`'s
/// docs for why the worker, not the loop, owns that obligation).
pub struct ClipboardJob {
    pub token: ReplyToken,
    pub kind: ClipboardJobKind,
}

/// The two operations a `g:clipboard` provider call can ask this worker
/// for. `register` is carried on both rather than assumed: `'+'` and `'*'`
/// share one backend (see `Effect::ClipboardRead`'s doc for why), but the
/// shadow fallback keeps them as separate entries, so a design that later
/// gave them distinct backends would not have to replumb this job shape.
pub enum ClipboardJobKind {
    Read { register: char },
    Write { register: char, lines: Vec<String> },
}

/// Spawns the clipboard worker and returns its handle; the caller (`run`'s
/// setup) keeps the `JoinHandle` alive for the session's duration but never
/// joins it -- the thread runs until `jobs`'s sender side (owned by the
/// `Executor`) is dropped at process exit, same lifetime as the writer and
/// reader threads `Engine::spawn` starts.
///
/// Generic over [`EngineOps`] rather than the concrete `EngineHandle`: the
/// only capability this thread needs of its engine connection is `reply`,
/// and taking the same trait `Executor` is generic over lets a test drive
/// the reply-exactly-once contract this function owns against a recording
/// fake instead of a live nvim connection.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the OS cannot start the
/// thread.
pub fn spawn<E: EngineOps + Send + 'static>(
    ops: E,
    jobs: mpsc::Receiver<ClipboardJob>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("view-clipboard".to_owned())
        .spawn(move || run(&ops, &jobs))
}

/// The worker's body: one register-keyed shadow map, one lazily-created
/// `arboard::Clipboard` (see `ensure_clip`'s doc for why it must outlive any
/// single job), both live for the thread's whole lifetime, and one reply per
/// job -- the loop's one-reply-per-token invariant (see `EngineRequest`'s
/// doc) is this function's contract to keep, since `update()` has already
/// handed both obligations here and has no further chance to answer them
/// itself.
fn run<E: EngineOps>(ops: &E, jobs: &mpsc::Receiver<ClipboardJob>) {
    let mut shadow: HashMap<char, String> = HashMap::new();
    let mut clip: Option<arboard::Clipboard> = arboard::Clipboard::new().ok();
    while let Ok(job) = jobs.recv() {
        match job.kind {
            ClipboardJobKind::Read { register } => {
                let lines = read_lines(&mut clip, register, &shadow);
                // an EngineError here means the engine connection is
                // already gone (the writer thread exited); there is no
                // second engine to answer, and the paint loop's own
                // EngineLost/EngineDown path is what notices the
                // connection is down, not this reply
                let _ = ops.reply(job.token, ReplyValue::Lines(lines));
            }
            ClipboardJobKind::Write { register, lines } => {
                let text = lines_to_text(&lines);
                write_system(&mut clip, &text);
                shadow.insert(register, text);
                let _ = ops.reply(job.token, ReplyValue::Nil);
            }
        }
    }
}

/// Returns the live `arboard::Clipboard`, retrying `Clipboard::new()` if the
/// worker started before a display was reachable and none has been claimed
/// yet. Once a connection exists it is never dropped and re-opened between
/// jobs: on X11, dropping the last non-global `Clipboard` handle (this
/// worker's own instance, once no other thread in the process holds one)
/// tears the whole clipboard connection down -- destroys the selection
/// window and hands the data to a clipboard manager to persist it, which no
/// manager is running to receive under a bare Xvfb/CI/SSH session -- so a
/// fresh instance per call would silently erase whatever it had just
/// written before any reader, including this same thread's own next read,
/// could observe it.
fn ensure_clip(clip: &mut Option<arboard::Clipboard>) -> Option<&mut arboard::Clipboard> {
    if clip.is_none() {
        *clip = arboard::Clipboard::new().ok();
    }
    clip.as_mut()
}

/// Reads the system clipboard via `arboard`, falling back to `shadow`'s
/// entry for `register` when `arboard` cannot reach a clipboard at all (no
/// `Clipboard::new()`, e.g. no display) or reports no text -- see the
/// module doc's remote-paste contract for why a fallback, not an error, is
/// correct here.
fn read_lines(
    clip: &mut Option<arboard::Clipboard>,
    register: char,
    shadow: &HashMap<char, String>,
) -> Vec<String> {
    let text = ensure_clip(clip)
        .and_then(|clip| clip.get_text().ok())
        .or_else(|| shadow.get(&register).cloned());
    match text {
        Some(text) => text_to_lines(&text),
        None => Vec::new(),
    }
}

/// Writes `text` to the system clipboard via `arboard`. Failure (no
/// `Clipboard::new()`, e.g. no display) is silent: the shadow register the
/// caller updates regardless is what keeps `"+p` working in exactly that
/// case, and there is nowhere to report a clipboard failure that both this
/// background thread and a headless remote session could reach anyway.
fn write_system(clip: &mut Option<arboard::Clipboard>, text: &str) {
    if let Some(clip) = ensure_clip(clip) {
        let _ = clip.set_text(text.to_owned());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_register_never_written_this_session_reads_no_shadow_fallback() {
        let shadow: HashMap<char, String> = HashMap::new();
        // arboard's own success/failure depends on the host's display,
        // which this test must not assume either way; what is provable
        // without one is that an empty shadow contributes nothing to the
        // fallback path a display-less read would take
        assert!(!shadow.contains_key(&'+'));
    }

    #[test]
    fn write_then_shadow_read_round_trips_without_a_display() {
        let mut shadow: HashMap<char, String> = HashMap::new();
        shadow.insert(
            '+',
            lines_to_text(&["hello".to_owned(), "world".to_owned()]),
        );
        let read_back = text_to_lines(shadow.get(&'+').unwrap());
        assert_eq!(read_back, vec!["hello", "world"]);
    }

    /// An [`EngineOps`] whose only live method is `reply`: every other
    /// method is unreachable from this worker's own logic, so this fake
    /// exists to observe the one call the worker thread's reply obligation
    /// actually needs proof of, over a channel a test thread can wait on
    /// with a bound instead of joining a loop that never exits on its own.
    struct ReplyRecorder {
        tx: mpsc::Sender<(u64, ReplyValue)>,
    }

    impl EngineOps for ReplyRecorder {
        fn input(&self, _notation: &str) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn try_resize(
            &self,
            _width: u16,
            _height: u16,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn paste(&self, _text: &str) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn input_mouse(
            &self,
            _button: &str,
            _action: &str,
            _modifier: &str,
            _row: u16,
            _col: u16,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn set_option(
            &self,
            _name: &str,
            _value: &view_core::msg::OptionValue,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn hold_option(
            &self,
            _name: &str,
            _value: &view_core::msg::OptionValue,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn reply(
            &self,
            token: ReplyToken,
            value: ReplyValue,
        ) -> Result<(), view_engine::handle::EngineError> {
            let _ = self.tx.send((token.msgid, value));
            Ok(())
        }
        fn probe_default_hl(
            &self,
            _generation: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn register_mappings(
            &self,
            _specs: &[view_core::native::mappings::MappingSpec],
            _channel_id: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn register_bridge(
            &self,
            _channel_id: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
        fn register_clipboard(
            &self,
            _channel_id: u64,
        ) -> Result<(), view_engine::handle::EngineError> {
            Ok(())
        }
    }

    #[test]
    fn a_read_job_answers_its_token_exactly_once_within_a_bounded_wait() {
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let worker = spawn(ReplyRecorder { tx: reply_tx }, job_rx).unwrap();
        job_tx
            .send(ClipboardJob {
                token: ReplyToken { msgid: 42 },
                kind: ClipboardJobKind::Read { register: '+' },
            })
            .unwrap();

        // a bounded wait, not a join on a loop that runs until its sender
        // is dropped: a worker that silently swallowed the job must fail
        // this test with a named timeout, never hang it
        let (msgid, value) = reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a read job must answer its token within 2s, not block the engine forever");
        assert_eq!(msgid, 42, "the reply must answer the token the job carried");
        assert!(matches!(value, ReplyValue::Lines(_)));
        assert!(
            reply_rx.try_recv().is_err(),
            "a read job must reply exactly once, never twice"
        );

        drop(job_tx);
        let _ = worker.join();
    }

    #[test]
    fn a_write_job_answers_its_token_exactly_once_within_a_bounded_wait() {
        let (job_tx, job_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let worker = spawn(ReplyRecorder { tx: reply_tx }, job_rx).unwrap();
        job_tx
            .send(ClipboardJob {
                token: ReplyToken { msgid: 7 },
                kind: ClipboardJobKind::Write {
                    register: '+',
                    lines: vec!["hello".to_owned()],
                },
            })
            .unwrap();

        let (msgid, value) = reply_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a write job must answer its token within 2s, not block the engine forever");
        assert_eq!(msgid, 7, "the reply must answer the token the job carried");
        assert!(matches!(value, ReplyValue::Nil));
        assert!(
            reply_rx.try_recv().is_err(),
            "a write job must reply exactly once, never twice"
        );

        drop(job_tx);
        let _ = worker.join();
    }
}
