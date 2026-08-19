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
    /// `[ai]` names an `agent = [...]` with nothing in it -- refused
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

/// The step [`AiWorker::spawn_in_background`] calls through rather than
/// naming [`resolve_launch`] directly, so a test can replace it with a
/// fixture that genuinely blocks.
///
/// That indirection is what makes "provisioning never runs on the loop
/// thread" a falsifiable claim instead of one only true of today's
/// implementation: a resolver that sleeps for seconds, paired with an
/// assertion that `dispatch` still returns in milliseconds, fails the
/// moment the resolve-and-spawn step is inlined onto the calling thread --
/// a mutation a merely fast-failing fixture (a nonexistent program) cannot
/// distinguish from the real off-thread behavior it is supposed to prove.
type Resolver = Arc<dyn Fn(&AgentSpec, &Path) -> Result<AgentLaunch, SpawnFailure> + Send + Sync>;

/// The step [`AiWorker::spawn_in_background`] calls through rather than
/// naming `spawn_or_log` directly, returning whether the OS accepted the
/// worker thread. Exists so a test can prove the thread-spawn-refusal path
/// (see that method's own doc) without needing to genuinely exhaust OS
/// threads in a test process -- there is no portable way to force
/// `std::thread::Builder::spawn` to fail otherwise.
type Spawner = Arc<dyn Fn(Box<dyn FnOnce() + Send>) -> bool + Send + Sync>;

/// The default [`Spawner`]: a real OS thread via `spawn_or_log`.
fn default_spawner() -> Spawner {
    Arc::new(|f| spawn_or_log("ai-spawn", f))
}

/// Resolves `spec` to something [`AiSession::spawn`] can run, doing whatever
/// I/O that requires -- `ClaudeCodeAdapter::provisioned` may download a
/// pinned tarball. The default [`Resolver`]; called only from the
/// background thread [`AiWorker::spawn_in_background`] spawns, never from
/// the loop thread.
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
    resolver: Resolver,
    spawner: Spawner,
}

impl AiWorker {
    /// A worker for `agent_spec`, with no session running yet.
    pub(crate) fn new(agent_spec: AgentSpec, cwd: PathBuf, msg: LoopSender) -> Self {
        Self::with_seams(
            agent_spec,
            cwd,
            msg,
            Arc::new(resolve_launch),
            default_spawner(),
        )
    }

    fn with_seams(
        agent_spec: AgentSpec,
        cwd: PathBuf,
        msg: LoopSender,
        resolver: Resolver,
        spawner: Spawner,
    ) -> Self {
        Self {
            agent_spec,
            cwd,
            msg,
            slot: Arc::new(Mutex::new(AiSlot::Idle)),
            resolver,
            spawner,
        }
    }

    /// Same as [`Self::new`], with [`resolve_launch`] replaced by
    /// `resolver` -- the seam a test uses to prove the resolve-and-spawn
    /// step genuinely runs off the calling thread (see [`Resolver`]'s own
    /// doc).
    #[cfg(test)]
    fn new_with_resolver(
        agent_spec: AgentSpec,
        cwd: PathBuf,
        msg: LoopSender,
        resolver: Resolver,
    ) -> Self {
        Self::with_seams(agent_spec, cwd, msg, resolver, default_spawner())
    }

    /// Same as [`Self::new`], with the real OS-thread spawn replaced by
    /// `spawner` -- the seam a test uses to prove the thread-spawn-refusal
    /// path (see [`Spawner`]'s own doc).
    #[cfg(test)]
    fn new_with_spawner(
        agent_spec: AgentSpec,
        cwd: PathBuf,
        msg: LoopSender,
        spawner: Spawner,
    ) -> Self {
        Self::with_seams(agent_spec, cwd, msg, Arc::new(resolve_launch), spawner)
    }

    /// Hands `command` to the live session, buffers it for the one still
    /// being spawned, or starts a spawn and buffers it as the first command
    /// that spawn owes a reply to. Never blocks: the lock guards a handful
    /// of enum swaps and, at most, one unbounded-channel send
    /// ([`AiSession::send`]), none of which wait on the agent itself.
    ///
    /// Only [`AiCommand::Prompt`] against an [`AiSlot::Idle`] slot starts a
    /// spawn: every other command has nothing to answer it yet, so
    /// provisioning an agent only to hand it a `Cancel` (or a permission
    /// answer to a request it never made) would be work with no purpose,
    /// paid for by whichever command happened to arrive first. Those
    /// commands instead surface "no active session" through the same
    /// [`AiEvent::SessionCrashed`] path a genuine crash reports through
    /// (see that event's own doc), rather than a silent no-op the user has
    /// no way to notice.
    ///
    /// A [`AiSlot::Ready`] session whose channel has already closed is
    /// treated the same as [`AiSlot::Idle`]: the session task exits only
    /// after emitting its own crash event on every path (see
    /// [`AiSession::is_closed`]'s doc), so falling through here is what
    /// turns the next `Prompt` into a restart instead of a permanent silent
    /// sink for every command sent to a session that is already gone.
    pub(crate) fn dispatch(&self, command: AiCommand) {
        let mut slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(&*slot, AiSlot::Ready(session) if session.is_closed()) {
            *slot = AiSlot::Idle;
        }
        match &mut *slot {
            AiSlot::Ready(session) => session.send(command),
            AiSlot::Spawning(pending) => pending.push(command),
            AiSlot::Idle if matches!(command, AiCommand::Prompt { .. }) => {
                *slot = AiSlot::Spawning(vec![command]);
                drop(slot);
                self.spawn_in_background();
            }
            AiSlot::Idle => {
                drop(slot);
                let _ = self.msg.send(Msg::Ai(AiEvent::SessionCrashed {
                    message: "no active AI session for this command".to_string(),
                }));
            }
        }
    }

    /// The live child's OS process id, if the slot is `Ready` and its
    /// session still holds one -- `None` for `Idle`, `Spawning`, or a
    /// `Ready` session whose child has already been reaped. Exists only for
    /// the restart-survival test in `recovery.rs`: proof, from outside the
    /// worker, that a session observed before some event and one observed
    /// after it are the same live child, not one that quietly restarted in
    /// between.
    #[cfg(test)]
    pub(crate) fn ready_pid_for_test(&self) -> Option<u32> {
        let slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        match &*slot {
            AiSlot::Ready(session) => session.pid(),
            AiSlot::Idle | AiSlot::Spawning(_) => None,
        }
    }

    /// Whether `self` and `other` share the same underlying spawn state --
    /// true only for clones of the same worker, never for two independently
    /// constructed ones that merely look alike. What a restart's re-wiring
    /// must preserve (see `LoopChannels::executor`'s own doc): a mutation
    /// that constructed a fresh `AiWorker` there instead of cloning the
    /// shared one compiles and would pass every test that never asks this
    /// question.
    #[cfg(test)]
    pub(crate) fn is_same_worker_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.slot, &other.slot)
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
    ///
    /// If the OS itself refuses the worker thread (`self.spawner` returns
    /// `false`), nothing will ever drain the `Spawning` slot `dispatch`
    /// already published, so this resets it to `Idle` and reports
    /// `SessionCrashed` right here, on the calling thread -- unlike
    /// `Effect::TreeGitScan`'s own same-condition handling (`runtime.rs`),
    /// which leaves its state permanently frozen because nothing there
    /// depends on retrying; an AI turn a user is waiting on must not wedge
    /// silently the same way.
    fn spawn_in_background(&self) {
        let agent_spec = self.agent_spec.clone();
        let cwd = self.cwd.clone();
        let slot = Arc::clone(&self.slot);
        let msg = self.msg.clone();
        let resolver = Arc::clone(&self.resolver);
        let started = (self.spawner)(Box::new(move || {
            let emit_tx = msg.clone();
            let result = resolver(&agent_spec, &cwd).and_then(|launch| {
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
        }));
        if !started {
            let mut guard = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
            *guard = AiSlot::Idle;
            drop(guard);
            let _ = self.msg.send(Msg::Ai(AiEvent::SessionCrashed {
                message: "AI agent failed to start: could not create a worker thread".to_string(),
            }));
        }
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

    /// Polls `cond` until it is true or `timeout` elapses, returning
    /// whichever comes first. Used only to wait for a background thread's
    /// state to settle before making the next assertion depend on it --
    /// the alternative, asserting straight off a channel receive, does not
    /// guarantee the mutex-guarded slot itself has been published yet (see
    /// callers for the specific race).
    fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            if cond() {
                return true;
            }
            if start.elapsed() >= timeout {
                return cond();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
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

        worker.dispatch(AiCommand::Prompt {
            text: "hello".to_string(),
            context: Vec::new(),
        });

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("an unknown adapter id must still report SessionCrashed");
        assert!(
            matches!(&msg, Msg::Ai(AiEvent::SessionCrashed { message }) if message.contains("unknown AI agent id")),
            "expected an unknown-adapter SessionCrashed, got {msg:?}"
        );
    }

    /// Only `Prompt` may start a spawn against an `Idle` slot: `Cancel`
    /// against a slot with nothing running has no session to cancel, so it
    /// must surface "no active session" rather than provisioning an agent
    /// only to hand it a command that was never waiting on one. The
    /// message distinguishes this from a genuine spawn failure (which would
    /// say "AI agent failed to start") -- provoking one here would mean the
    /// bug this test guards against (spawning for a non-`Prompt` command)
    /// happened.
    #[test]
    fn a_non_prompt_command_against_an_idle_slot_never_spawns() {
        let (worker, rx) = worker_with(missing_program_spec());

        worker.dispatch(AiCommand::Cancel);

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("an idle Cancel must still report something visible");
        match msg {
            Msg::Ai(AiEvent::SessionCrashed { message }) => {
                assert_eq!(
                    message, "no active AI session for this command",
                    "an idle Cancel must not read as a spawn failure"
                );
            }
            other => panic!("expected SessionCrashed, got {other:?}"),
        }

        let slot = worker.slot.lock().unwrap();
        assert!(
            matches!(&*slot, AiSlot::Idle),
            "a non-Prompt command against Idle must never start a spawn"
        );
    }

    /// A `Ready` session whose channel has already closed -- the session
    /// task exited and reported its own crash -- is not a permanent sink
    /// for every command sent to it afterward: the next `Prompt` restarts
    /// rather than silently vanishing into a channel nothing drains
    /// anymore.
    #[test]
    fn a_dead_ready_session_is_replaced_by_the_next_prompt() {
        let (worker, rx) = worker_with(AgentSpec::Command(vec!["true".to_string()]));

        worker.dispatch(AiCommand::Prompt {
            text: "first".to_string(),
            context: Vec::new(),
        });

        // `true` exits immediately without ever completing the ACP
        // handshake, so the session task ends on end-of-file and reports
        // its own crash -- this is what leaves the slot `Ready` with a
        // closed channel rather than ever reaching `SessionReady`.
        let first = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first session must report its own end");
        assert!(
            matches!(&first, Msg::Ai(AiEvent::SessionCrashed { .. })),
            "expected the first session to crash, got {first:?}"
        );

        // the crash message and the spawn thread's own `*guard =
        // Ready(session)` race independently (they run on different
        // threads with no ordering between them), so receiving the
        // message alone does not guarantee the guard has been published
        // yet -- wait for the slot itself to settle before dispatching
        // again.
        let settled = wait_until(
            || {
                let slot = worker.slot.lock().unwrap();
                matches!(&*slot, AiSlot::Ready(session) if session.is_closed())
            },
            Duration::from_secs(5),
        );
        assert!(
            settled,
            "the slot never settled into a dead Ready state after the crash"
        );

        worker.dispatch(AiCommand::Prompt {
            text: "second".to_string(),
            context: Vec::new(),
        });

        let second = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a Prompt against a dead session must restart, not vanish");
        assert!(
            matches!(&second, Msg::Ai(AiEvent::SessionCrashed { .. })),
            "expected the restarted session to also report its own end, got {second:?}"
        );
    }

    /// The OS refusing the spawn worker thread must not leave the slot
    /// wedged in `Spawning` forever: `dispatch` drives the real
    /// `spawn_in_background` through an injected [`Spawner`] that always
    /// refuses, and the slot must come back to `Idle` with the refusal
    /// reported the same way any other spawn failure is.
    #[test]
    fn a_worker_thread_spawn_refusal_resets_the_slot_and_reports_a_crash() {
        let (tx, rx) = mpsc::sync_channel(8);
        let never_spawns: Spawner = Arc::new(|_f: Box<dyn FnOnce() + Send>| false);
        let worker = AiWorker::new_with_spawner(
            missing_program_spec(),
            PathBuf::from("."),
            LoopSender::new(tx),
            never_spawns,
        );

        worker.dispatch(AiCommand::Prompt {
            text: "hello".to_string(),
            context: Vec::new(),
        });

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a thread-spawn refusal must still report SessionCrashed");
        assert!(
            matches!(&msg, Msg::Ai(AiEvent::SessionCrashed { message }) if message.contains("worker thread")),
            "expected a worker-thread SessionCrashed, got {msg:?}"
        );
        let slot = worker.slot.lock().unwrap();
        assert!(
            matches!(&*slot, AiSlot::Idle),
            "a thread-spawn refusal must leave the slot ready to retry, not stuck in Spawning"
        );
    }

    /// The resolve-and-spawn step genuinely runs off the calling thread:
    /// `dispatch` returns in milliseconds even when the resolver itself
    /// blocks for seconds, which an inlined resolve+spawn (the mutation
    /// this guards against) cannot survive -- a merely fast-failing
    /// resolver, like `missing_program_spec`'s, cannot tell inline
    /// execution from backgrounded execution apart, since both return
    /// almost instantly either way.
    #[test]
    fn dispatch_returns_before_a_genuinely_slow_resolver_finishes() {
        let (tx, rx) = mpsc::sync_channel(8);
        let resolver: Resolver = Arc::new(|_spec: &AgentSpec, _cwd: &Path| {
            std::thread::sleep(Duration::from_secs(2));
            Err(SpawnFailure::EmptyCommand)
        });
        let worker = AiWorker::new_with_resolver(
            missing_program_spec(),
            PathBuf::from("."),
            LoopSender::new(tx),
            resolver,
        );

        let started = Instant::now();
        worker.dispatch(AiCommand::Prompt {
            text: "hello".to_string(),
            context: Vec::new(),
        });
        let dispatch_elapsed = started.elapsed();

        assert!(
            dispatch_elapsed < Duration::from_millis(50),
            "dispatch must return before a genuinely slow resolver finishes, took {dispatch_elapsed:?}"
        );

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the slow resolver's eventual failure must still be reported");
        assert!(
            matches!(&msg, Msg::Ai(AiEvent::SessionCrashed { .. })),
            "expected SessionCrashed once the slow resolver finally returns, got {msg:?}"
        );
    }
}
