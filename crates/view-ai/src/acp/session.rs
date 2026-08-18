//! The agent session: one child process, one tokio runtime, and the two
//! calls the rest of the editor makes against it.

use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::process::Child;
use tokio::sync::mpsc;
use view_core::msg::Msg;
use view_core::native::ai_event::{AiCommand, AiEvent};

use crate::acp::driver::run_session;
use crate::acp::fs::PendingFsReplies;
use crate::acp::wire::JsonRpcCodec;
use crate::{AiConfig, AiError};

/// The state the session task and the handle both reach: emitting an event
/// into the caller's loop, and the correlation map for agent-initiated
/// filesystem requests.
///
/// Separate from [`AiSession`] because the handle owns the tokio runtime
/// and so cannot itself be shared into a task running on it, while both
/// sides genuinely need these two things.
/// The agent child, reachable from both the handle and the session task.
///
/// Shared rather than owned by the task because the handle's `Drop` must be
/// able to signal the child itself, on the dropping thread, without waiting
/// for a task to be scheduled. The `Option` is what makes that safe once the
/// session has ended: the task takes the child out before reaping it, so the
/// handle can never signal a process identifier the operating system has
/// already recycled.
pub(crate) type ChildSlot = Arc<Mutex<Option<Child>>>;

pub(crate) struct SessionShared {
    emit: Box<dyn Fn(Msg) + Send + Sync>,
    pending: PendingFsReplies,
}

impl SessionShared {
    /// Builds the shared half on its own, without a child or a runtime, so
    /// the session loop can be driven over ordinary streams.
    #[cfg(test)]
    pub(crate) fn detached(emit: Box<dyn Fn(Msg) + Send + Sync>) -> Self {
        Self {
            emit,
            pending: PendingFsReplies::default(),
        }
    }

    /// Wraps `event` in [`Msg::Ai`] and hands it to the stored emit
    /// closure.
    ///
    /// Takes an [`AiEvent`], not a `Msg`: nothing on this side of the
    /// boundary has business naming any other `Msg` arm, and wrapping in
    /// one place keeps that true by construction. Never blocks -- the
    /// closure is a channel send, and this runs on a runtime worker
    /// thread, never the paint thread.
    pub(crate) fn emit(&self, event: AiEvent) {
        (self.emit)(Msg::Ai(event));
    }

    /// The correlation map for outstanding agent-initiated filesystem
    /// requests.
    pub(crate) fn pending(&self) -> &PendingFsReplies {
        &self.pending
    }
}

/// A running agent session.
///
/// Dropping it tears the session down without waiting on it: see the `Drop`
/// impl below for why that is the whole point.
pub struct AiSession {
    /// `Option` only so `Drop` can take the runtime out and hand it to a
    /// non-blocking shutdown; it is `Some` for the whole life of the value.
    runtime: Option<tokio::runtime::Runtime>,
    commands: mpsc::UnboundedSender<AiCommand>,
    shared: Arc<SessionShared>,
    child: ChildSlot,
}

impl Drop for AiSession {
    fn drop(&mut self) {
        // Signal the child here, synchronously, before anything else.
        // `shutdown_background` returns before a single task has been
        // dropped, so leaving the kill to `Child`'s own `kill_on_drop` would
        // race whatever the dropping thread does next: at editor teardown
        // `main` can return and the process can exit before a runtime thread
        // ever reaches that drop, and the agent would go on running with no
        // editor left to answer. `start_kill` sends the signal and returns
        // without waiting on it, so the guarantee costs one syscall.
        //
        // A poisoned lock is stepped over rather than propagated: a panicked
        // task must not be the reason a child process survives.
        let mut slot = self.child.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(child) = slot.as_mut() {
            let _ = child.start_kill();
        }
        drop(slot);

        // `Runtime`'s own `Drop` shuts down synchronously on the dropping
        // thread, and the dropping thread here is the loop thread. That is
        // the one call this type would make that is not a channel send, and
        // a session is dropped for reasons other than editor teardown --
        // restarting an agent, closing the panel -- where a stall would land
        // in the middle of a frame. `shutdown_background` returns at once
        // and lets the runtime's own threads wind themselves down.
        //
        // What is guaranteed after this returns: the child has been
        // signalled. What is not: that it has been reaped. Collection is
        // left to the runtime's reaper if its threads outlive the drop, and
        // to the operating system otherwise.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl AiSession {
    /// Spawns the agent subprocess and the session task that drives it
    /// inside a tokio runtime owned entirely by this call.
    ///
    /// `emit` forwards a decoded message into the caller's loop channel. It
    /// runs on a runtime worker thread, never the paint thread, and must
    /// never block, which a plain unbounded channel send satisfies. The
    /// closure shape is what keeps the dependency one-way: the caller wraps
    /// its own concrete loop sender without this crate ever naming that
    /// type.
    ///
    /// Latency consequence: zero on the paint and key-dispatch paths by
    /// construction. Nothing here runs on the loop thread; the only things
    /// the loop thread ever does with the result are a channel send
    /// ([`send`](Self::send)) and, in `Drop`, one non-blocking kill syscall
    /// followed by a non-blocking runtime shutdown.
    ///
    /// `emit` must not block, and the requirement is stronger than it looks:
    /// the runtime has one worker thread, so an `emit` that blocks stalls
    /// the whole session -- the reader, the writer, and every outstanding
    /// filesystem answer -- not just the task that called it.
    ///
    /// # Errors
    ///
    /// [`AiError::Runtime`] if the runtime cannot be built,
    /// [`AiError::Spawn`] if the agent command cannot be started, and
    /// [`AiError::ChildPipeMissing`] if the started child has no stdin or
    /// stdout. That last one is taken rather than unwrapped: a library has
    /// no panic budget, and a child spawned without the pipes it was
    /// configured for is a bug the caller must see as an error value.
    pub fn spawn(cfg: AiConfig, emit: Box<dyn Fn(Msg) + Send + Sync>) -> Result<Self, AiError> {
        // a single worker thread: this runtime drives one child's stdio and
        // a handful of correlation tasks, none of them CPU-bound, so extra
        // workers would add threads to the editor's own footprint for no
        // parallelism that exists to be had
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_io()
            .thread_name("view-ai")
            .build()
            .map_err(AiError::Runtime)?;

        let mut child = {
            // spawning registers the child with the reactor, which only
            // exists inside the runtime context
            let _guard = runtime.enter();
            tokio::process::Command::new(&cfg.command)
                .args(&cfg.args)
                .current_dir(&cfg.cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                // the transport says an agent MAY log to stderr and a client
                // MAY ignore it; inheriting it would paint agent logs over
                // the alternate screen, so it is discarded until there is a
                // panel to route it into
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|source| AiError::Spawn {
                    command: cfg.command.clone(),
                    source,
                })?
        };

        let stdout = child.stdout.take().ok_or(AiError::ChildPipeMissing)?;
        let stdin = child.stdin.take().ok_or(AiError::ChildPipeMissing)?;

        let shared = Arc::new(SessionShared {
            emit,
            pending: PendingFsReplies::default(),
        });
        let (commands, command_rx) = mpsc::unbounded_channel();

        let child: ChildSlot = Arc::new(Mutex::new(Some(child)));

        let task_shared = Arc::clone(&shared);
        let task_child = Arc::clone(&child);
        let cwd = cfg.cwd;
        runtime.spawn(async move {
            run_session(
                task_child,
                JsonRpcCodec::new(stdout, stdin),
                command_rx,
                task_shared,
                cwd,
            )
            .await;
        });

        Ok(Self {
            runtime: Some(runtime),
            commands,
            shared,
            child,
        })
    }

    /// Queues `command` for the agent and returns immediately.
    ///
    /// Never awaits and never touches the child: the command crosses an
    /// unbounded channel to the session task, which owns every write. That
    /// extends "the paint loop never awaits RPC" to agent traffic -- a
    /// wedged or slow agent cannot stall the caller, because the caller
    /// never waits on it. A send after the session task has ended is
    /// dropped: there is nothing left to carry it, and the crash that
    /// ended the task has already been reported as an event.
    pub fn send(&self, command: AiCommand) {
        let _ = self.commands.send(command);
    }
}

impl std::fmt::Debug for AiSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // hand-written because the emit closure has no Debug, and a session
        // whose handle cannot be printed at all is worse than one that
        // prints its liveness
        f.debug_struct("AiSession")
            .field(
                "runtime",
                &self.runtime.as_ref().map(tokio::runtime::Runtime::handle),
            )
            .field("closed", &self.commands.is_closed())
            .field("shared", &Arc::strong_count(&self.shared))
            .finish()
    }
}
