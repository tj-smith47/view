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

use view_ai::{
    AgentLaunch, AgentSpec, AiError, AiSession, ClaudeCodeAdapter, ProvisionError, WatchHandle,
};
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

/// The one slot an [`AiWorker`] and every clone of it share for whichever
/// out-of-band write watch is currently running.
///
/// `generation` and `stopped` are what make start-versus-stop ordering
/// irrelevant instead of load-bearing. `AiSession::spawn` returns before
/// its own async event loop has necessarily run, so a child that exits as
/// fast as `true` can have the crash-forwarding closure ask for a stop
/// before the spawning thread has published anything at all -- and a stop
/// that arrives first must still be honoured, or the watch outlives the
/// session it belonged to with nothing left to stop it. Recording the stop
/// against the generation it was meant for gives that answer by
/// construction, on any interleaving, with no ordering constraint left on
/// the caller.
#[derive(Default)]
struct WatchSlot {
    /// The live watch for [`Self::generation`], once one has been published
    /// and not yet stopped.
    handle: Option<WatchHandle>,
    /// Which spawn attempt this slot currently belongs to. A stop or a
    /// publish naming an older generation is a late message from a session
    /// that is already gone, and is ignored rather than allowed to tear
    /// down its successor's watch.
    generation: u64,
    /// Whether [`Self::generation`]'s watch has already been asked to stop.
    stopped: bool,
}

type Watch = Arc<Mutex<WatchSlot>>;

fn lock(watch: &Watch) -> std::sync::MutexGuard<'_, WatchSlot> {
    watch.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Claims the slot for a new spawn attempt, stopping whatever the previous
/// one left running, and answers the generation every later [`start_watch`]
/// and [`stop_watch`] call for that attempt must name.
fn begin_watch(watch: &Watch) -> u64 {
    let mut slot = lock(watch);
    if let Some(handle) = slot.handle.take() {
        handle.stop();
    }
    slot.generation += 1;
    slot.stopped = false;
    slot.generation
}

/// Starts the out-of-band write watcher over `cwd` and publishes it into
/// `watch` for `generation`.
///
/// A watch published into a slot that has already been stopped -- or that
/// a newer spawn attempt has already claimed -- is torn down here and now
/// rather than left running: that is the whole point of the generation, and
/// it is why nothing outside this function has to care whether a start
/// races its own session's crash.
///
/// Starting is best-effort in that a failure never fails the spawn: the
/// watcher is the safety net for a write path ACP itself cannot see or
/// route (an agent's own shell tool), not a precondition for the writes ACP
/// *can* route, which work with no watch running at all. It is not silent,
/// though -- `docs/ai.md` tells the user detection is on, so a session
/// running without it says so through `Msg::ExternalWatchDegraded`.
fn start_watch(watch: &Watch, generation: u64, cwd: &Path, msg: &LoopSender) {
    let emit = msg.clone();
    let handle = match view_ai::spawn_watch(cwd, move |m| {
        let _ = emit.send(m);
    }) {
        Ok(handle) => handle,
        Err(err) => {
            let _ = msg.send(Msg::ExternalWatchDegraded {
                reason: format!("{err}"),
            });
            return;
        }
    };
    let mut slot = lock(watch);
    if slot.generation != generation || slot.stopped {
        drop(slot);
        handle.stop();
        return;
    }
    slot.handle = Some(handle);
}

/// Stops `generation`'s out-of-band write watch, whether or not it has been
/// published yet. Idempotent, and a no-op for a generation the slot has
/// already moved past.
fn stop_watch(watch: &Watch, generation: u64) {
    let mut slot = lock(watch);
    if slot.generation != generation {
        return;
    }
    slot.stopped = true;
    if let Some(handle) = slot.handle.take() {
        handle.stop();
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
    /// The out-of-band write watcher for whichever session is currently
    /// `Ready`. Started as a spawn begins and stopped the moment that
    /// session's own `SessionCrashed` is observed -- see
    /// [`Self::spawn_in_background`]'s own doc for why that is eager, not
    /// the lazy `Ready`-to-`Idle` demotion [`Self::dispatch`] does for the
    /// slot itself, and [`WatchSlot`]'s for why the two never race.
    watch: Watch,
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
            watch: Watch::default(),
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
    #[cfg(all(test, unix))]
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
    #[cfg(all(test, unix))]
    pub(crate) fn is_same_worker_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.slot, &other.slot) && Arc::ptr_eq(&self.watch, &other.watch)
    }

    /// Whether an out-of-band write watch is currently published for this
    /// worker. The falsifiable handle on the watcher's own lifetime:
    /// "untrusted project, no watch", "a live session watches", "a crashed
    /// session does not", and "a restarted engine keeps the one it had" are
    /// all one question asked from outside the worker.
    #[cfg(test)]
    pub(crate) fn watch_is_running(&self) -> bool {
        lock(&self.watch).handle.is_some()
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
    ///
    /// ## The watcher's own lifetime
    ///
    /// A successful resolve also starts [`view_ai::spawn_watch`] over `cwd`
    /// -- already the trusted project root, since [`AiCommand::Prompt`]
    /// cannot reach this worker until `view-core`'s own trust gate has
    /// granted it (`update/mod.rs`'s `open_ai_trust_prompt` arm) -- so no
    /// separate trust check belongs here. Which side of `AiSession::spawn`
    /// it starts on is deliberately not load-bearing: the generation
    /// [`begin_watch`] claims on the dispatching thread is what makes a
    /// stop that arrives before the start still win (see [`WatchSlot`]),
    /// so a child that exits as fast as `true` cannot leave a watch running
    /// past the session it belonged to on any interleaving.
    ///
    /// The watch is stopped eagerly -- inside the per-event closure the
    /// instant that session's own `SessionCrashed` is observed, and again
    /// on this thread if resolving succeeded but the spawn itself failed --
    /// rather than left to [`Self::dispatch`]'s lazy `Ready`-to-`Idle`
    /// demotion: that demotion only runs on the *next* command, which may
    /// be arbitrarily far in the future (or never), and a watcher left
    /// running past its session's death would go on driving `checktime`
    /// (and popping conflict prompts) for writes no agent is left to have
    /// made.
    ///
    /// Registering the tree is not on this path at all: `spawn_watch`
    /// returns as soon as the backend and its thread exist and walks the
    /// project on that thread (see `view_ai::watch::spawn`), so the user's
    /// first prompt never waits on it.
    fn spawn_in_background(&self) {
        let agent_spec = self.agent_spec.clone();
        let cwd = self.cwd.clone();
        let slot = Arc::clone(&self.slot);
        let watch = Arc::clone(&self.watch);
        let watch_generation = begin_watch(&watch);
        let msg = self.msg.clone();
        let resolver = Arc::clone(&self.resolver);
        let started = (self.spawner)(Box::new(move || {
            let emit_tx = msg.clone();
            let watch_for_events = Arc::clone(&watch);
            let watch_for_start = Arc::clone(&watch);
            let msg_for_start = msg.clone();
            let cwd_for_start = cwd.clone();
            let result = resolver(&agent_spec, &cwd).and_then(|launch| {
                let emit_tx = msg.clone();
                start_watch(
                    &watch_for_start,
                    watch_generation,
                    &cwd_for_start,
                    &msg_for_start,
                );
                AiSession::spawn(
                    launch,
                    Box::new(move |event| {
                        if matches!(&event, Msg::Ai(AiEvent::SessionCrashed { .. })) {
                            stop_watch(&watch_for_events, watch_generation);
                        }
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
                    // idempotent whether or not the failure happened
                    // before or after the `start_watch` call above (a
                    // resolver failure never reaches it at all; a spawn
                    // failure after a successful resolve did)
                    stop_watch(&watch, watch_generation);
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

    /// A real, empty directory for a watch to run over -- `spawn_watch`
    /// needs a genuinely readable path, unlike every other test's `"."`
    /// stand-in (which happens to work too, but would leave a live watcher
    /// on this crate's own source tree for the duration of the test run).
    fn watch_tempdir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "view-ai-worker-watch-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A live session (one that never crashes) keeps its watch running --
    /// the positive half of the watcher's lifetime contract. `sleep 5`
    /// neither reads its stdin nor writes anything to stdout, so it never
    /// completes the ACP handshake, never triggers a decode failure the way
    /// a pass-through fixture like `cat` would (echoing the session's own
    /// handshake bytes back as a malformed reply), and does not exit for
    /// the whole assertion window -- unlike every other fixture in this
    /// module, which is deliberately chosen to fail fast.
    #[test]
    fn a_ready_session_keeps_its_watch_running() {
        let dir = watch_tempdir("ready");
        let (tx, rx) = mpsc::sync_channel(8);
        let worker = AiWorker::new(
            AgentSpec::Command(vec!["sleep".to_string(), "5".to_string()]),
            dir.clone(),
            LoopSender::new(tx),
        );

        worker.dispatch(AiCommand::Prompt {
            text: "hello".to_string(),
            context: Vec::new(),
        });

        let settled = wait_until(
            || matches!(&*worker.slot.lock().unwrap(), AiSlot::Ready(session) if !session.is_closed()),
            Duration::from_secs(3),
        );
        assert!(settled, "the session never reached a live Ready state");
        assert!(
            worker.watch_is_running(),
            "a live session's watch must be running"
        );

        drop(worker);
        drop(rx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A running watch survives an engine restart. The restart re-wires a
    /// fresh executor to a CLONE of the shared worker (`recovery.rs`
    /// asserts that reuse directly), so the property that matters here is
    /// that a clone observes the same live watch -- a worker rebuilt from
    /// scratch instead would leave the surviving agent's own shell writes
    /// unnoticed for the rest of the run, with nothing anywhere saying so.
    #[test]
    fn a_clone_of_a_running_worker_shares_its_live_watch() {
        let dir = watch_tempdir("restart");
        let (tx, rx) = mpsc::sync_channel(8);
        let worker = AiWorker::new(
            AgentSpec::Command(vec!["sleep".to_string(), "5".to_string()]),
            dir.clone(),
            LoopSender::new(tx),
        );

        worker.dispatch(AiCommand::Prompt {
            text: "hello".to_string(),
            context: Vec::new(),
        });
        assert!(
            wait_until(|| worker.watch_is_running(), Duration::from_secs(3)),
            "the live session never started its watch"
        );

        let survivor = worker.clone();
        assert!(survivor.is_same_worker_as(&worker));
        assert!(
            survivor.watch_is_running(),
            "a restart's re-wired worker must keep the watch its session already had"
        );

        drop(worker);
        drop(survivor);
        drop(rx);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stop that arrives BEFORE its own start still wins. This is the
    /// whole TOCTOU property, asked synchronously: no child process, no
    /// channel, no interleaving to be lucky about. The previous shape of
    /// this guard read the slot at the instant a crash message arrived and
    /// passed under both orderings, which is the "timing-shaped assertion
    /// that fails open" class -- it protected nothing.
    #[test]
    fn a_watch_stopped_before_it_starts_never_ends_up_running() {
        let dir = watch_tempdir("stop-first");
        let (tx, _rx) = mpsc::sync_channel(8);
        let msg = LoopSender::new(tx);
        let watch = Watch::default();

        let generation = begin_watch(&watch);
        stop_watch(&watch, generation);
        start_watch(&watch, generation, &dir, &msg);

        assert!(
            lock(&watch).handle.is_none(),
            "a watch published into an already-stopped slot must be torn down, not left running"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A late stop from a session that is already gone never tears down its
    /// successor's watch: the generation is what tells the two apart, and
    /// without it a slow crash-forwarding closure silently disables
    /// detection for the session the user just started.
    #[test]
    fn a_stale_generations_stop_leaves_the_current_watch_alone() {
        let dir = watch_tempdir("stale-stop");
        let (tx, _rx) = mpsc::sync_channel(8);
        let msg = LoopSender::new(tx);
        let watch = Watch::default();

        let first = begin_watch(&watch);
        let second = begin_watch(&watch);
        start_watch(&watch, second, &dir, &msg);
        stop_watch(&watch, first);

        assert!(
            lock(&watch).handle.is_some(),
            "a dead session's stop must not reach the watch of the one that replaced it"
        );
        stop_watch(&watch, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No watch without a session, and therefore none without trust: the
    /// only command that ever starts one is `Prompt`, which `view-core`'s
    /// own trust gate is what lets reach this worker at all. Asked here at
    /// the watcher level rather than inferred from the call chain, so a
    /// future caller that dispatched some other command into a spawn would
    /// fail this instead of quietly watching an untrusted root.
    #[test]
    fn a_command_that_starts_no_session_starts_no_watch() {
        let dir = watch_tempdir("untrusted");
        let (tx, rx) = mpsc::sync_channel(8);
        let worker = AiWorker::new(missing_program_spec(), dir.clone(), LoopSender::new(tx));

        assert!(!worker.watch_is_running(), "an idle worker watches nothing");
        worker.dispatch(AiCommand::Cancel);
        let _ = rx.recv_timeout(Duration::from_secs(5));

        assert!(
            !worker.watch_is_running(),
            "a command that starts no session must never start a watch"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The out-of-band write watcher must never outlive its own session's
    /// crash: this is the falsifiable half of
    /// [`AiWorker::spawn_in_background`]'s own "## The watcher's own
    /// lifetime" doc. `true` exits before its own watch can finish
    /// starting, so the crash-forwarding closure's `stop_watch` runs while
    /// nothing is published yet -- the interleaving the generation slot
    /// exists for, and the one a plain "take the handle out" teardown would
    /// let a later `start_watch` walk straight past.
    #[test]
    fn a_watch_never_outlives_its_own_crashed_session() {
        let dir = watch_tempdir("crashed");
        let (tx, rx) = mpsc::sync_channel(8);
        let worker = AiWorker::new(
            AgentSpec::Command(vec!["true".to_string()]),
            dir.clone(),
            LoopSender::new(tx),
        );

        worker.dispatch(AiCommand::Prompt {
            text: "hello".to_string(),
            context: Vec::new(),
        });

        let msg = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a fast-exiting child must still report its own crash");
        assert!(
            matches!(&msg, Msg::Ai(AiEvent::SessionCrashed { .. })),
            "expected SessionCrashed, got {msg:?}"
        );

        // the crash-forwarding closure calls `stop_watch` strictly before
        // it sends this event (sequential code, same closure), so by the
        // time the message above has been received, `watch` is already
        // settled -- no `wait_until` needed
        assert!(
            !worker.watch_is_running(),
            "the watch must not outlive its own session's crash"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
