//! The bin's own construction and driving of an [`AiSession`]: the piece
//! `view-ai` deliberately leaves outside its own boundary (it never
//! constructs a runtime for itself and never speaks for `[ai]` config), and
//! the piece `view-core` cannot own at all (it is pure, and provisioning
//! does real I/O).
//!
//! An [`AiWorker`] is a handle two things share: [`Executor::run`]'s
//! `Effect::Ai` arm, which forwards every command through [`AiWorker::dispatch`],
//! and the background thread `dispatch` spawns the first time it is ever
//! called with nothing running. Nothing here touches the loop thread beyond
//! a channel send and a mutex lock around a handful of enum-variant swaps,
//! both of which are the same non-blocking shape every other worker in this
//! crate already uses (`clipboard.rs`, `tree-scan` in `runtime.rs`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use view_ai::{AgentLaunch, AgentSpec, AiError, AiSession, ClaudeCodeAdapter, ProvisionError};
use view_core::msg::Msg;
use view_core::native::ai_event::{AiCommand, AiEvent};

use crate::runtime::spawn_or_log;
use crate::wake::LoopSender;

/// Where a project's one agent session stands.
///
/// `Spawning`'s `Vec<AiCommand>` is the only queue this module adds: once an
/// [`AiSession`] exists, its own `send` already queues onto an unbounded
/// channel the session task drains as soon as the protocol handshake lets
/// it, so nothing here duplicates that. What `AiSession::send` cannot help
/// with is the narrower window before that value exists at all --
/// provisioning and spawning run on a background thread precisely because
/// they can block on a download, so a command that arrives while that
/// thread is still working has nowhere else to wait.
enum AiSlot {
    /// Never asked for yet, or the last spawn attempt failed and left
    /// nothing running -- the next command tried again from here.
    Idle,
    /// A background thread is resolving the agent and spawning it.
    Spawning(Vec<AiCommand>),
    /// A live session, taking every future command directly.
    Ready(AiSession),
}

/// Everything [`resolve_launch`] can fail with, folded into one type so
/// [`AiWorker::spawn_in_background`] has a single error to report through
/// [`AiEvent::SessionCrashed`] regardless of which stage failed.
#[derive(Debug, thiserror::Error)]
enum SpawnFailure {
    /// `[ai]` names an adapter id this build has no [`ClaudeCodeAdapter`]
    /// (or other) provisioner for.
    #[error("unknown AI agent id \"{0}\" (this build only knows \"claude-code\")")]
    UnknownAdapter(String),
    /// `[ai]` names a `command = [...]` with nothing in it -- refused
    /// already by `AiConfig`'s own parse, so reaching this is a
    /// same-process config/spawn disagreement rather than a user error, and
    /// is reported the same way any other spawn failure is rather than
    /// unwrapped.
    #[error("the configured agent command has no program to run")]
    EmptyCommand,
    /// Downloading, verifying, or extracting the pinned adapter failed.
    #[error("could not provision the AI agent: {0}")]
    Provision(#[from] ProvisionError),
    /// The resolved command could not be started as a child process.
    #[error("{0}")]
    Session(#[from] AiError),
}

/// Resolves `spec` to something [`AiSession::spawn`] can run, doing whatever
/// I/O that requires -- `ClaudeCodeAdapter::provisioned` may download a
/// pinned tarball. Called only from the background thread
/// [`AiWorker::spawn_in_background`] spawns, never from the loop thread.
fn resolve_launch(spec: &AgentSpec, cwd: &Path) -> Result<AgentLaunch, SpawnFailure> {
    match spec {
        AgentSpec::Id(id) if id == "claude-code" => {
            let adapter = ClaudeCodeAdapter::provisioned()?;
            Ok(AgentLaunch::from_adapter(&adapter, cwd))
        }
        AgentSpec::Id(id) => Err(SpawnFailure::UnknownAdapter(id.clone())),
        AgentSpec::Command(argv) => {
            let (program, args) = argv.split_first().ok_or(SpawnFailure::EmptyCommand)?;
            Ok(AgentLaunch::new(program.clone(), cwd).with_args(args.iter().cloned()))
        }
    }
}

/// The bin's handle on one project's agent session.
///
/// Cheap to clone (an `Arc` and two owned small values): a restarted engine
/// gets a fresh [`crate::runtime::Executor`] wired to the same worker, so a
/// session already running (or already being spawned) survives an engine
/// restart exactly the way the clipboard and picker workers do -- see
/// `LoopChannels`'s own doc.
#[derive(Clone)]
pub(crate) struct AiWorker {
    agent_spec: AgentSpec,
    cwd: PathBuf,
    msg: LoopSender,
    slot: Arc<Mutex<AiSlot>>,
}

impl AiWorker {
    /// A worker for `agent_spec`, with no session running yet.
    pub(crate) fn new(agent_spec: AgentSpec, cwd: PathBuf, msg: LoopSender) -> Self {
        Self {
            agent_spec,
            cwd,
            msg,
            slot: Arc::new(Mutex::new(AiSlot::Idle)),
        }
    }

    /// Hands `command` to the live session, buffers it for the one still
    /// being spawned, or starts a spawn and buffers it as the first command
    /// that spawn owes a reply to. Never blocks: the lock guards a handful
    /// of enum swaps and, at most, one unbounded-channel send
    /// ([`AiSession::send`]), none of which wait on the agent itself.
    pub(crate) fn dispatch(&self, command: AiCommand) {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        match &mut *slot {
            AiSlot::Ready(session) => session.send(command),
            AiSlot::Spawning(pending) => pending.push(command),
            AiSlot::Idle => {
                *slot = AiSlot::Spawning(vec![command]);
                drop(slot);
                self.spawn_in_background();
            }
        }
    }

    /// Runs [`resolve_launch`] and [`AiSession::spawn`] on their own thread,
    /// since provisioning may download an adapter and neither may ever run
    /// on the loop thread (see `AiSession::spawn`'s own latency doc, carried
    /// here for the resolve step ahead of it).
    ///
    /// On success, every command buffered while this ran is sent to the new
    /// session, in order, before the slot is published as `Ready` -- so
    /// nothing dispatched during the spawn is lost, and nothing sent after
    /// it publishes can overtake what arrived first.
    ///
    /// On failure, the slot returns to `Idle` (the next command tries again
    /// rather than being permanently refused by one bad attempt), and the
    /// failure is reported as [`AiEvent::SessionCrashed`] -- the same event
    /// a session that started and later died reports, which already reads
    /// as both a panel-local error and a toast when no session ever became
    /// ready (see `update::ai::on_ai_event`). The commands buffered for a
    /// failed spawn are dropped along with it: there is no session left to
    /// hand them to, and the crash event is what tells the caller they were
    /// never carried out.
    fn spawn_in_background(&self) {
        let agent_spec = self.agent_spec.clone();
        let cwd = self.cwd.clone();
        let slot = Arc::clone(&self.slot);
        let msg = self.msg.clone();
        spawn_or_log("ai-spawn", move || {
            let emit_tx = msg.clone();
            let result = resolve_launch(&agent_spec, &cwd).and_then(|launch| {
                let emit_tx = msg.clone();
                AiSession::spawn(
                    launch,
                    Box::new(move |event| {
                        let _ = emit_tx.send(event);
                    }),
                )
                .map_err(SpawnFailure::from)
            });
            let mut guard = slot.lock().unwrap_or_else(PoisonError::into_inner);
            let pending = match std::mem::replace(&mut *guard, AiSlot::Idle) {
                AiSlot::Spawning(pending) => pending,
                // only this thread ever moves the slot out of `Spawning`,
                // and only one spawn runs per `Idle` -> `Spawning`
                // transition, so every other shape here is unreachable;
                // still handled rather than assumed, with nothing lost, in
                // case a future caller changes that invariant
                other => {
                    *guard = other;
                    Vec::new()
                }
            };
            match result {
                Ok(session) => {
                    for command in pending {
                        session.send(command);
                    }
                    *guard = AiSlot::Ready(session);
                }
                Err(err) => {
                    drop(guard);
                    let _ = emit_tx.send(Msg::Ai(AiEvent::SessionCrashed {
                        message: format!("AI agent failed to start: {err}"),
                    }));
                }
            }
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// An `AgentSpec::Command` naming a program that does not exist:
    /// resolving it never touches the network, so this fixture spawns
    /// promptly and predictably fails, the same shape a genuinely offline
    /// provisioning attempt or a missing `claude-code` binary would leave
    /// the worker in.
    fn missing_program_spec() -> AgentSpec {
        AgentSpec::Command(vec![
            "view-ai-worker-test-nonexistent-program-xyz".to_string()
        ])
    }

    fn worker_with(spec: AgentSpec) -> (AiWorker, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::sync_channel(8);
        let worker = AiWorker::new(spec, PathBuf::from("."), LoopSender::new(tx));
        (worker, rx)
    }

    /// The falsifiable half of "the paint loop never awaits RPC" extended to
    /// agent traffic: `dispatch` returns to its caller immediately, on the
    /// calling thread, even for the very first command this worker has ever
    /// seen -- the one that has to start a spawn from nothing. A regression
    /// that resolved the agent or spawned the child inline, on this thread,
    /// would turn this call into the exact stall the contract forbids; nothing
    /// downstream of `dispatch` would ever observe the difference except a
    /// hang here.
    #[test]
    fn dispatching_the_first_command_returns_before_the_spawn_finishes() {
        let (worker, rx) = worker_with(missing_program_spec());

        let started = Instant::now();
        worker.dispatch(AiCommand::Prompt {
            text: "hello".to_string(),
            context: Vec::new(),
        });
        let dispatch_elapsed = started.elapsed();

        assert!(
            dispatch_elapsed < Duration::from_millis(50),
            "dispatch must return before a background spawn resolves, took {dispatch_elapsed:?}"
        );

        // the background thread's own failure is measured separately, below
        // -- this assertion only pins that `dispatch` itself never becomes
        // the wait
        let _ = rx.recv_timeout(Duration::from_secs(5));
    }

    /// A spawn that cannot start (an absent program, standing in for a
    /// missing agent binary or a failed provisioning download) never
    /// vanishes: it is reported as a crash event, and the slot returns to
    /// `Idle` so the next command gets a fresh attempt rather than being
    /// permanently refused by one bad one.
    #[test]
    fn a_spawn_failure_is_reported_as_a_session_crashed_event_and_the_slot_recovers() {
        let (worker, rx) = worker_with(missing_program_spec());

        worker.dispatch(AiCommand::Prompt {
            text: "hello".to_string(),
            context: Vec::new(),
        });

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a spawn failure must report SessionCrashed, not silently vanish");
        match msg {
            Msg::Ai(AiEvent::SessionCrashed { message }) => {
                assert!(
                    message.contains("AI agent failed to start"),
                    "message should name that startup failed, got {message:?}"
                );
            }
            other => panic!("expected SessionCrashed, got {other:?}"),
        }

        let slot = worker.slot.lock().unwrap();
        assert!(
            matches!(&*slot, AiSlot::Idle),
            "a failed spawn must leave the slot ready to retry, not stuck"
        );
    }

    /// A command naming an unknown adapter id fails through the same
    /// `SessionCrashed` path as a provisioning or spawn failure, rather than
    /// panicking or hanging on a `ClaudeCodeAdapter` this build never
    /// intended to run.
    #[test]
    fn an_unknown_adapter_id_reports_session_crashed() {
        let (worker, rx) = worker_with(AgentSpec::Id("not-a-real-adapter".to_string()));

        worker.dispatch(AiCommand::Cancel);

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("an unknown adapter id must still report SessionCrashed");
        assert!(
            matches!(&msg, Msg::Ai(AiEvent::SessionCrashed { message }) if message.contains("unknown AI agent id")),
            "expected an unknown-adapter SessionCrashed, got {msg:?}"
        );
    }
}
