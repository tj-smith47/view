//! Off-loop worker that performs a submitted prompt's four context reads
//! and hands the assembled command to the agent session -- and, since both
//! `Effect::Ai` and `Effect::AiPromptSubmit` queue onto this same worker's
//! one channel (see [`AiContextJob`]'s own doc), the single point every
//! command bound for the agent session passes through, in the order the
//! loop thread queued them.
//!
//! `Effect::AiPromptSubmit` carries only the prompt's text -- `view-core` is
//! pure and cannot itself issue RPC or depend on `view-ai` (see that
//! effect's own doc). The four reads
//! [`EngineOps`] exposes for this
//! (`read_current_buffer_text`, `read_cursor_context`,
//! `read_diagnostic_entries`, `read_quickfix_entries`) are synchronous,
//! bounded-timeout RPC requests, never fire-and-forget notifies -- issuing
//! them on the runtime loop thread would violate "the paint loop never
//! awaits RPC" the same way a blocking `nvim_eval` would. This worker is
//! the one place they run, off that thread, on the same "one dedicated
//! thread, one job channel" shape `clipboard.rs`'s worker already
//! establishes.
//!
//! Generic over [`EngineOps`] + `Clone` rather than the concrete
//! `EngineHandle`, mirroring `clipboard::spawn`'s own reasoning: a test can
//! drive this worker's read-then-assemble logic against a recording fake
//! instead of a live nvim connection.

use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};

use view_core::native::ai_context::EngineReadSnapshot;
use view_core::native::ai_event::AiCommand;

use crate::ai_worker::AiWorker;
use crate::engine_ops::EngineOps;

/// One job queued by [`crate::runtime::Executor::run`] onto this worker's
/// single channel. `Submit` and `Direct` share the one queue deliberately --
/// not two separate channels -- so a `Cancel` (`Effect::Ai`) issued right
/// after a `Submit` (`Effect::AiPromptSubmit`) can never overtake the prompt
/// it means to cancel: both funnel through the same `mpsc::Sender`, drained
/// in FIFO order by this worker's one thread, so "queued after" and
/// "dispatched to the session after" are the same relation by construction.
/// Before this type existed, `Effect::Ai` dispatched straight to
/// [`AiWorker::dispatch`] from the loop thread while `Effect::AiPromptSubmit`
/// queued here for the (slower, four-read) trip to the same call --
/// overtaking was possible because they were two independent paths racing
/// for the same destination. Total ordering is the deliberate point of
/// sharing one queue: the tradeoff is a `Direct` command (e.g. `Cancel`)
/// that happens to queue behind a slow `Submit`'s four reads waits for
/// them, bounded added latency this worker's own thread absorbs -- never
/// the loop thread, which only ever queues onto the channel and never
/// blocks on what this worker does with it, so this ordering cost never
/// reaches the paint path.
pub enum AiContextJob {
    /// A submitted prompt's raw text -- this worker reads the four context
    /// sources and assembles [`AiCommand::Prompt`] before dispatching.
    Submit { text: String },
    /// A command that needs no context assembly (`Cancel`, a permission
    /// answer, an already-assembled `Prompt`) -- dispatched to the session
    /// as-is, with no read in between.
    Direct(AiCommand),
}

/// The engine-ops connection this worker reads through, re-pointable across
/// an engine restart -- the same "the handle moves and the thread stays"
/// shape `crate::clipboard::ReplyRoute` established for the clipboard
/// worker (see that type's own doc), minus the epoch/token bookkeeping
/// neither reply obligation nor stale-reply detection applies to here: a
/// context read has no in-flight request a restart could answer with the
/// wrong connection's reply, only a live connection to read through or a
/// dead one that fails fast.
///
/// Without a rebind, a restarted session's context worker would go on
/// holding the FIRST engine's now-closed handle for the rest of the
/// session: every read after that point would fail immediately (a closed
/// connection, not a hang -- see [`read_snapshot`]'s own doc on why a
/// failed read only omits its block), so every prompt submitted after a
/// restart would silently carry empty context forever, with nothing on
/// screen to say why.
pub struct OpsRoute<E: EngineOps> {
    ops: Arc<Mutex<E>>,
}

impl<E: EngineOps> Clone for OpsRoute<E> {
    fn clone(&self) -> Self {
        Self {
            ops: Arc::clone(&self.ops),
        }
    }
}

impl<E: EngineOps + Clone> OpsRoute<E> {
    /// A route reading through `ops`.
    pub fn new(ops: E) -> Self {
        Self {
            ops: Arc::new(Mutex::new(ops)),
        }
    }

    /// Points every later read at `ops` instead, for the engine that has
    /// replaced the one this route was built on.
    pub fn rebind(&self, ops: E) {
        match self.ops.lock() {
            Ok(mut held) => *held = ops,
            // a poisoned route means the loop thread panicked mid-swap, and
            // the process is already coming down; this worker's reads fail
            // the same way they would against a closed connection
            Err(_) => crate::vlog::log("ai-context", "ops route poisoned; not rebound"),
        }
    }

    /// The connection this route currently names, cloned out from behind
    /// the lock rather than held across a read: a context read is several
    /// RPC round trips (four, one per source), and holding the lock for all
    /// of them would block a restart's `rebind` behind whichever read is
    /// slowest, turning a bounded-timeout read into an unbounded wait for
    /// the loop thread.
    fn current(&self) -> E {
        self.ops
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Spawns the context worker and returns its handle; the caller (`run`'s
/// setup) keeps the `JoinHandle` alive for the session's duration but never
/// joins it, the same lifetime `clipboard::spawn`'s own doc states for its
/// worker.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the OS cannot start the
/// thread.
pub fn spawn<E: EngineOps + Clone + Send + 'static>(
    route: OpsRoute<E>,
    ai: AiWorker,
    jobs: mpsc::Receiver<AiContextJob>,
) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("view-ai-context".to_owned())
        .spawn(move || run(&route, &ai, &jobs))
}

fn run<E: EngineOps + Clone>(
    route: &OpsRoute<E>,
    ai: &AiWorker,
    jobs: &mpsc::Receiver<AiContextJob>,
) {
    while let Ok(job) = jobs.recv() {
        match job {
            AiContextJob::Submit { text } => {
                let ops = route.current();
                ai.dispatch(build_prompt(&ops, text));
            }
            AiContextJob::Direct(command) => ai.dispatch(command),
        }
    }
}

/// Performs the four context reads through `ops` and assembles the
/// resulting [`AiCommand::Prompt`] -- the exact seam a live-engine test
/// drives directly (no worker thread, no agent session needed) to prove a
/// submitted prompt's context contains the blocks a real buffer/cursor
/// produce.
pub(crate) fn build_prompt<E: EngineOps>(ops: &E, text: String) -> AiCommand {
    let snapshot = read_snapshot(ops);
    let context = view_ai::assemble(&snapshot);
    AiCommand::Prompt { text, context }
}

/// Performs the four context reads, folding each into
/// [`EngineReadSnapshot`]. A read that errors -- a closed connection, a
/// stale-after-restart handle, a timed-out request -- omits its own field
/// rather than failing the whole snapshot: `EngineReadSnapshot`'s own doc
/// draws no distinction between "this read failed" and "there was nothing
/// here," and neither does this function.
fn read_snapshot<E: EngineOps>(ops: &E) -> EngineReadSnapshot {
    let mut snapshot = EngineReadSnapshot::default();
    if let Ok(buffer) = ops.read_current_buffer_text() {
        snapshot = snapshot.with_current_buffer(buffer);
    }
    if let Ok((cursor, selection)) = ops.read_cursor_context() {
        snapshot = snapshot.with_cursor(cursor);
        if let Some(selection) = selection {
            snapshot = snapshot.with_selection(selection);
        }
    }
    if let Ok(diagnostics) = ops.read_diagnostic_entries() {
        snapshot = snapshot.with_diagnostics(diagnostics);
    }
    if let Ok(quickfix) = ops.read_quickfix_entries() {
        snapshot = snapshot.with_quickfix(quickfix);
    }
    snapshot
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use std::rc::Rc;

    use super::*;
    use crate::engine_ops::FakeOps;

    /// `build_prompt` folds every successful read into the assembled
    /// context and carries `text` through unchanged -- `FakeOps`'s default
    /// reads report an empty buffer, no selection, a cursor at (0, 0), and
    /// no diagnostics/quickfix entries, so `assemble` produces only the
    /// `Cursor` block (the one block `assemble` emits even for an
    /// all-zero/absent read -- see its own doc).
    #[test]
    fn build_prompt_carries_text_through_and_assembles_available_context() {
        let ops = FakeOps::default();

        let command = build_prompt(&ops, "hello".to_string());

        match command {
            AiCommand::Prompt { text, context } => {
                assert_eq!(text, "hello");
                assert_eq!(
                    context,
                    vec![view_core::native::ai_event::ContextBlock::Cursor { line: 0, col: 0 }]
                );
            }
            other => panic!("expected AiCommand::Prompt, got {other:?}"),
        }
    }

    /// A read that fails omits its own block without failing the others --
    /// `fail_next` makes every `FakeOps` call fail, so this proves the
    /// all-failed case degrades to an empty context rather than panicking
    /// or propagating an error nothing here has anywhere to send.
    #[test]
    fn build_prompt_degrades_to_empty_context_when_every_read_fails() {
        let ops = FakeOps::default();
        *ops.fail_next.borrow_mut() = true;

        let command = build_prompt(&ops, "hi".to_string());

        match command {
            AiCommand::Prompt { text, context } => {
                assert_eq!(text, "hi");
                assert!(context.is_empty());
            }
            other => panic!("expected AiCommand::Prompt, got {other:?}"),
        }
    }

    /// `OpsRoute::rebind` genuinely re-points later reads: two `FakeOps`,
    /// each shared through its own `Rc` (`EngineOps`'s `Rc<T>` blanket, so
    /// the clone `OpsRoute` holds still logs into the same `calls` this
    /// test reads back), record into their own logs -- so a read issued
    /// after a rebind must land in the replacement's log, never gain a
    /// second entry in the original's -- the exact property a restarted
    /// session's context worker depends on to stop reading a dead engine.
    #[test]
    fn rebind_points_later_reads_at_the_new_ops() {
        let first = Rc::new(FakeOps::default());
        let route = OpsRoute::new(Rc::clone(&first));
        let _ = build_prompt(&route.current(), "before".to_string());
        assert_eq!(
            first
                .calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("read_current_buffer_text"))
                .count(),
            1,
            "the pre-rebind read must land on the original ops"
        );

        let replacement = Rc::new(FakeOps::default());
        route.rebind(Rc::clone(&replacement));
        let _ = build_prompt(&route.current(), "after".to_string());

        assert_eq!(
            replacement
                .calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("read_current_buffer_text"))
                .count(),
            1,
            "the post-rebind read must land on the replacement ops"
        );
        assert_eq!(
            first
                .calls
                .borrow()
                .iter()
                .filter(|c| c.starts_with("read_current_buffer_text"))
                .count(),
            1,
            "the post-rebind read must not also land on the original ops"
        );
    }

    /// The end-to-end falsifiable check the ruled-in scope states directly:
    /// a submitted prompt against a live engine with a real buffer/cursor
    /// produces `AiCommand::Prompt` whose context contains the
    /// current-buffer and cursor blocks -- `build_prompt` driven straight
    /// against a real `EngineHandle` (`EngineOps` is implemented for it,
    /// per `engine_ops.rs`), the same "drive the seam directly, no worker
    /// thread, no agent session" approach this file's other `build_prompt`
    /// tests already use, just with a live connection standing in for
    /// `FakeOps`. No `[ai]` config, no `AiWorker`, no engine restart --
    /// this only proves the read-then-assemble half of the pipeline, which
    /// is the half `view-core` cannot reach on its own.
    #[test]
    fn build_prompt_against_a_live_engine_carries_real_buffer_and_cursor_context() {
        let engine =
            view_engine::process::Engine::spawn(view_engine::process::EngineConfig::isolated())
                .expect("spawn engine");
        engine.handle.ui_attach(80, 24).expect("attach ui");
        engine
            .handle
            .command("call setline(1, ['hello world', 'second line'])")
            .expect("seed buffer content");
        engine
            .handle
            .input("gg0ll")
            .expect("move the cursor off (0, 0)");

        let command = build_prompt(&engine.handle, "explain this".to_string());

        match command {
            AiCommand::Prompt { text, context } => {
                assert_eq!(text, "explain this");
                assert!(
                    context.iter().any(|block| matches!(
                        block,
                        view_core::native::ai_event::ContextBlock::CurrentBuffer { text, .. }
                            if text == "hello world\nsecond line"
                    )),
                    "expected a CurrentBuffer block carrying the seeded content: {context:?}"
                );
                assert!(
                    context.iter().any(|block| matches!(
                        block,
                        view_core::native::ai_event::ContextBlock::Cursor { line: 1, col: 3 }
                    )),
                    "expected a Cursor block at the moved-to position: {context:?}"
                );
            }
            other => panic!("expected AiCommand::Prompt, got {other:?}"),
        }
    }
}
