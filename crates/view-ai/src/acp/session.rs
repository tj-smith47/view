//! The agent session: one child process, one tokio runtime, and the two
//! calls the rest of the editor makes against it.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc;
use view_core::msg::Msg;
use view_core::native::ai_event::{AiCommand, AiEvent};

use crate::acp::driver::run_session;
use crate::acp::fs::PendingFsReplies;
use crate::acp::wire::JsonRpcCodec;
use crate::{AgentLaunch, AiError};

/// The agent child itself.
///
/// A std child on unix, spawned through [`view_proc`] so an editor killed
/// outright does not leave the agent running with nothing to answer to; the
/// parent-death signal is armed on a thread of that crate's own, which a
/// `tokio::process` spawn (forking inline on whichever thread called
/// `AiSession::spawn`) cannot be. Windows keeps the tokio child and stays on
/// the uncovered list, having no such signal either way.
#[cfg(unix)]
pub(crate) type AgentChild = std::process::Child;
#[cfg(not(unix))]
pub(crate) type AgentChild = tokio::process::Child;

/// The child's standard output, as the session's reader reads it.
#[cfg(unix)]
pub(crate) type AgentStdout = tokio::net::unix::pipe::Receiver;
#[cfg(not(unix))]
pub(crate) type AgentStdout = tokio::process::ChildStdout;

/// The child's standard input, as the session's writer writes it.
#[cfg(unix)]
pub(crate) type AgentStdin = tokio::net::unix::pipe::Sender;
#[cfg(not(unix))]
pub(crate) type AgentStdin = tokio::process::ChildStdin;

/// The agent child, reachable from both the handle and the session task.
///
/// Shared rather than owned by the task because the handle's `Drop` must be
/// able to signal the child itself, on the dropping thread, without waiting
/// for a task to be scheduled. The `Option` is what makes that safe once the
/// session has ended: whichever side takes the child out owns both the
/// signal and the wait that follows it, so the other can never signal a
/// process identifier the operating system has already recycled, and no
/// child is ever waited on twice.
pub(crate) type ChildSlot = Arc<Mutex<Option<AgentChild>>>;

/// Signals the agent to stop and returns without waiting on it.
///
/// One syscall on both platforms: this runs on the dropping thread, which at
/// editor teardown is the loop thread.
pub(crate) fn signal_stop(child: &mut AgentChild) {
    #[cfg(unix)]
    let _ = child.kill();
    #[cfg(not(unix))]
    let _ = child.start_kill();
}

/// Signals the child and collects it without blocking the caller.
///
/// The dropping thread is the editor's own loop thread, so the `waitpid`
/// that turns a signalled child into a collected one cannot happen there;
/// the session runtime cannot be asked either, since the drop shuts it down
/// without waiting for a task to run. A thread that outlives both does the
/// wait, and a `SIGKILL`ed child makes it a short one.
///
/// Unix only, because collection is: a `std::process::Child` stays in the
/// process table until someone waits on it. Windows has no zombie state --
/// killing the process and closing its handle is what releases the object,
/// which is what dropping the tokio child there already does.
#[cfg(unix)]
pub(crate) fn signal_and_collect(mut child: AgentChild) {
    signal_stop(&mut child);
    let _ = std::thread::Builder::new()
        .name(String::from("view-ai-reap"))
        .spawn(move || {
            let _ = child.wait();
        });
}

/// [`signal_and_collect`] where dropping the child's own handle is the
/// collection.
#[cfg(not(unix))]
pub(crate) fn signal_and_collect(mut child: AgentChild) {
    signal_stop(&mut child);
}

/// Reaps a signalled agent, off the runtime's own worker.
///
/// The unix wait blocks, and the session runtime has a single worker driving
/// the agent's stdio; blocking it would stall the reader and the writer with
/// it.
pub(crate) async fn reap(child: AgentChild) -> std::io::Result<std::process::ExitStatus> {
    #[cfg(unix)]
    {
        let mut child = child;
        tokio::task::spawn_blocking(move || child.wait())
            .await
            .map_err(std::io::Error::other)?
    }
    #[cfg(not(unix))]
    {
        let mut child = child;
        child.wait().await
    }
}

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
        if let Some(child) = slot.take() {
            signal_and_collect(child);
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
        // signalled, and nothing is left holding it -- the reaper this drop
        // started does the unix wait, and on Windows dropping the child's
        // handle is the whole of the collection. Neither waits here.
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
    pub fn spawn(cfg: AgentLaunch, emit: Box<dyn Fn(Msg) + Send + Sync>) -> Result<Self, AiError> {
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

        let mut child = spawn_agent(&cfg)?;
        // adopting a descriptor registers it with the reactor, which only
        // exists inside the runtime context
        let (stdout, stdin) = {
            let _guard = runtime.enter();
            agent_pipes(&mut child)?
        };

        let shared = Arc::new(SessionShared {
            emit,
            pending: PendingFsReplies::default(),
        });
        let (commands, command_rx) = mpsc::unbounded_channel();

        let child: ChildSlot = Arc::new(Mutex::new(Some(child)));

        let task_shared = Arc::clone(&shared);
        let task_child = Arc::clone(&child);
        let cwd = cfg.cwd;
        let requires_auth = cfg.requires_auth;
        runtime.spawn(async move {
            run_session(
                task_child,
                JsonRpcCodec::new(stdout, stdin),
                command_rx,
                task_shared,
                cwd,
                requires_auth,
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

    /// Whether the session task has already ended -- the receiving half of
    /// [`send`](Self::send)'s channel has been dropped.
    ///
    /// A closed session is a dead one: its task exits only after emitting
    /// [`AiEvent::SessionCrashed`] on every path (the agent's own exit, a
    /// decode failure, a fatal write), so a caller that observes `true` here
    /// already has an explanation on the wire and needs this only to decide
    /// whether a fresh session must replace this one before the next command
    /// can go anywhere.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.commands.is_closed()
    }

    /// The child process's OS process id, if this session still holds it.
    ///
    /// `None` once the session task has taken the child out of
    /// [`ChildSlot`] to reap it, matching [`Drop`]'s own "nothing left to
    /// signal" state. Exists for the one thing nothing else exposes: proof,
    /// from outside the session task, that a later observation is still the
    /// *same* child rather than a second one quietly spawned in its place.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(agent_pid)
    }
}

/// Starts the agent with its stdio piped and its stderr discarded.
///
/// The transport says an agent MAY log to stderr and a client MAY ignore it;
/// inheriting it would paint agent logs over the alternate screen, so it is
/// discarded until there is a panel to route it into.
fn spawn_agent(cfg: &AgentLaunch) -> Result<AgentChild, AiError> {
    let failed = |source| AiError::Spawn {
        command: cfg.command.clone(),
        source,
    };
    #[cfg(unix)]
    {
        let mut command = std::process::Command::new(&cfg.command);
        command
            .args(&cfg.args)
            .current_dir(&cfg.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        view_proc::spawn_tied_to_this_process(command).map_err(failed)
    }
    #[cfg(not(unix))]
    {
        tokio::process::Command::new(&cfg.command)
            .args(&cfg.args)
            .current_dir(&cfg.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(failed)
    }
}

/// Takes the child's two pipes in the form the session's codec reads and
/// writes them.
///
/// Must be called inside the session runtime's context: adopting a
/// descriptor registers it with that runtime's reactor.
fn agent_pipes(child: &mut AgentChild) -> Result<(AgentStdout, AgentStdin), AiError> {
    let stdout = child.stdout.take().ok_or(AiError::ChildPipeMissing)?;
    let stdin = child.stdin.take().ok_or(AiError::ChildPipeMissing)?;
    #[cfg(unix)]
    {
        use std::os::fd::OwnedFd;

        let adopt = |source| AiError::ChildPipeAdoption { source };
        Ok((
            AgentStdout::from_owned_fd(OwnedFd::from(stdout)).map_err(adopt)?,
            AgentStdin::from_owned_fd(OwnedFd::from(stdin)).map_err(adopt)?,
        ))
    }
    #[cfg(not(unix))]
    {
        Ok((stdout, stdin))
    }
}

/// The child's process id, which only the unix child answers unconditionally
/// -- a tokio child that has already been reaped has none to give.
fn agent_pid(child: &AgentChild) -> Option<u32> {
    #[cfg(unix)]
    {
        Some(child.id())
    }
    #[cfg(not(unix))]
    {
        child.id()
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

/// The seam between a specific agent's own launch details and the ACP
/// client this crate drives: what to execute, how to name it in
/// diagnostics, and whether it enforces authentication. `AiSession::spawn`
/// drives an adapter's config internally through
/// [`AgentLaunch::from_adapter`](crate::AgentLaunch::from_adapter); no ACP wire
/// type is reachable through this trait, so a consumer choosing which agent
/// to run never sees this crate's JSON-RPC shapes.
pub trait AgentAdapter: Send + Sync {
    /// The executable and the arguments it is invoked with, in order.
    fn command(&self) -> (&str, &[String]);
    /// A short, stable name for this agent, used only in diagnostics.
    fn id(&self) -> &str;
    /// Whether `session/new` failing with the wire's `auth_required` error
    /// must be answered with `authenticate` rather than treated as a
    /// terminal refusal.
    fn requires_auth(&self) -> bool;
}

/// The adapter for the reference agent binary this build ships with, whose
/// ACP endpoint sits behind an account login.
///
/// [`ClaudeCodeAdapter::new`] takes `pinned_version`/`binary_path` as given,
/// for a caller that already has both (a test, or a hand-configured
/// command). [`ClaudeCodeAdapter::provisioned`] is the real path: it
/// resolves both from `provision::ensure_adapter`'s own verified,
/// extracted output instead of a hand-typed value that could drift from
/// what was actually provisioned.
pub struct ClaudeCodeAdapter {
    pinned_version: String,
    binary_path: PathBuf,
    args: Vec<String>,
}

impl ClaudeCodeAdapter {
    /// An adapter for the executable at `binary_path`, pinned to
    /// `pinned_version`, with no arguments.
    #[must_use]
    pub fn new(pinned_version: impl Into<String>, binary_path: PathBuf) -> Self {
        Self {
            pinned_version: pinned_version.into(),
            binary_path,
            args: Vec::new(),
        }
    }

    /// The real `claude-code` adapter, provisioned end to end: resolves
    /// `node` from `PATH` (the pinned release is an npm package, run under
    /// Node.js, not a standalone executable), ensures the pinned tarball is
    /// downloaded, verified, and extracted via
    /// [`crate::ensure_adapter`], and returns an adapter whose
    /// [`AgentAdapter::command`] is `node <extracted entry script>` --
    /// something [`AiSession::spawn`] can actually launch, not a path to
    /// the tarball itself. The node check runs before the download: an
    /// adapter this build cannot run has no reason to fetch anything first.
    ///
    /// # Errors
    ///
    /// Returns [`crate::ProvisionError::NodeNotFound`] if no `node` is on
    /// `PATH`, and any other [`crate::ProvisionError`] variant
    /// [`crate::ensure_adapter`] can return.
    pub fn provisioned() -> Result<Self, crate::ProvisionError> {
        let node = crate::provision::resolve_node()?;
        let entry_path = crate::ensure_adapter("claude-code")?;
        let pinned_version = crate::pinned_version("claude-code").unwrap_or("unknown");
        Ok(Self {
            pinned_version: pinned_version.to_string(),
            binary_path: node,
            args: vec![entry_path.to_string_lossy().into_owned()],
        })
    }

    /// The version this adapter was constructed against, independent of
    /// [`AgentAdapter::id`], which names the agent kind rather than the
    /// pinned build.
    #[must_use]
    pub fn pinned_version(&self) -> &str {
        &self.pinned_version
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    fn command(&self) -> (&str, &[String]) {
        (self.binary_path.to_str().unwrap_or_default(), &self.args)
    }

    fn id(&self) -> &str {
        "claude-code"
    }

    fn requires_auth(&self) -> bool {
        // this agent's ACP endpoint sits behind an account login; a session
        // created without authenticating first is not a degraded session,
        // it is one that was never going to work
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn an_adapter_reports_its_own_command_id_and_auth_requirement() {
        let adapter = ClaudeCodeAdapter::new("1.2.3", PathBuf::from("/usr/bin/claude-code-acp"));
        assert_eq!(adapter.pinned_version(), "1.2.3");
        assert_eq!(adapter.id(), "claude-code");
        assert!(adapter.requires_auth());
        let (command, args) = adapter.command();
        assert_eq!(command, "/usr/bin/claude-code-acp");
        assert!(args.is_empty());
    }

    /// The disconfirm the falsifiable check names, driven through the
    /// adapter path rather than a raw `AgentLaunch`: a config built from an
    /// adapter whose binary does not exist must fail `spawn` synchronously,
    /// before any tokio task starts, exactly like a raw one does.
    #[test]
    fn a_nonexistent_adapter_binary_fails_spawn_synchronously_not_a_hang() {
        let adapter = ClaudeCodeAdapter::new(
            "0.0.0",
            PathBuf::from("/no/such/path/view-ai-adapter-binary-does-not-exist"),
        );
        let cfg = AgentLaunch::from_adapter(&adapter, std::env::temp_dir());
        let err =
            AiSession::spawn(cfg, Box::new(|_| {})).expect_err("a missing agent cannot start");
        assert!(
            matches!(err, AiError::Spawn { .. }),
            "expected AiError::Spawn, got {err:?}"
        );
    }
}
