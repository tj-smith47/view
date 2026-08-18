//! The agent session: one child process, one tokio runtime, and the two
//! calls the rest of the editor makes against it.

use std::process::Stdio;
use std::sync::Arc;

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
pub(crate) struct SessionShared {
    emit: Box<dyn Fn(Msg) + Send + Sync>,
    pending: PendingFsReplies,
}

impl SessionShared {
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
/// Dropping it shuts the runtime down, which drops the session task and
/// with it the child handle; the child was spawned with `kill_on_drop`, so
/// the agent process goes away with the session rather than outliving the
/// editor.
pub struct AiSession {
    // dropped last is irrelevant here, but declared first for the reader:
    // this is the only tokio runtime the process owns
    runtime: tokio::runtime::Runtime,
    commands: mpsc::UnboundedSender<AiCommand>,
    shared: Arc<SessionShared>,
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
    /// construction. Nothing here runs on the loop thread, and the only
    /// thing the loop thread ever does with the result is a channel send.
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

        let task_shared = Arc::clone(&shared);
        let cwd = cfg.cwd;
        runtime.spawn(async move {
            run_session(
                child,
                JsonRpcCodec::new(stdout, stdin),
                command_rx,
                task_shared,
                cwd,
            )
            .await;
        });

        Ok(Self {
            runtime,
            commands,
            shared,
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
            .field("runtime", &self.runtime.handle())
            .field("closed", &self.commands.is_closed())
            .field("shared", &Arc::strong_count(&self.shared))
            .finish()
    }
}
