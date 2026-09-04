//! What happens to the agent adapter when the editor that owns it dies
//! without running a destructor.
//!
//! `AiSession`'s `Drop` signals the child on every teardown view chooses and
//! on none of the ones it does not: a `SIGKILL` leaves the adapter running
//! with no editor left to answer. An adapter still reading its stdin exits
//! on the pipe's EOF, so the only child that proves anything here is one
//! that has stopped reading -- which is what the stub's `stall` prompt makes
//! it, and what a real agent waiting on a long tool call is.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use view_ai::{AgentLaunch, AiSession};
use view_core::msg::Msg;
use view_core::native::ai_event::{AiCommand, AiEvent};

/// Set on the re-executed copy of this binary that plays the editor being
/// killed. Absent in an ordinary suite run, where [`the_intermediate_parent`]
/// is a no-op.
const INTERMEDIATE: &str = "VIEW_AI_ORPHAN_INTERMEDIATE";

/// Where the intermediate's adapter looks for its release: the case below
/// creates this file if the adapter outlives its owner, so a failing run
/// ends its own stray instead of leaving one parked on the host.
const RESUME: &str = "VIEW_AI_ORPHAN_RESUME";

/// The line the intermediate writes to stderr once its adapter is deaf.
const PID_MARKER: &str = "orphan-reaping agent pid ";

/// How long an event is given to arrive, before the host's load widens it.
const WAIT: Duration = Duration::from_secs(10);

/// How long the orphan is given to leave the process table once its owner
/// is gone.
#[cfg(target_os = "linux")]
const REAPED: Duration = Duration::from_secs(3);

/// The owner half of the pin, run in a re-executed copy of this binary so
/// there is a process to kill that is not the test harness itself.
///
/// Drives the stub past `stall`, which answers the turn and then stops
/// reading its standard input entirely: the reply arriving is what orders
/// the kill after the adapter has gone deaf, rather than a sleep hoping it
/// has.
#[test]
fn the_intermediate_parent() {
    use std::io::{Read, Write};

    if std::env::var_os(INTERMEDIATE).is_none() {
        return;
    }
    let resume = std::env::var(RESUME).unwrap();
    // the case's own scratch directory, reached through the file it passed:
    // the adapter's working directory must be one the case removes
    let cwd = std::path::Path::new(&resume)
        .parent()
        .expect("the resume file must sit in the case's scratch directory")
        .to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    let cfg = AgentLaunch::new(env!("CARGO_BIN_EXE_view-ai-stub-agent"), cwd)
        .with_args([resume.as_str()]);
    let session = AiSession::spawn(
        cfg,
        Box::new(move |msg| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(msg);
            }
        }),
    )
    .expect("the stub agent starts");
    assert!(matches!(
        next_event(&rx, "SessionReady"),
        AiEvent::SessionReady { .. }
    ));
    session.send(AiCommand::Prompt {
        text: "stall".to_string(),
        context: Vec::new(),
    });
    assert!(matches!(
        next_event(&rx, "TurnEnded"),
        AiEvent::TurnEnded { .. }
    ));
    let pid = session.pid().expect("the session still holds its adapter");
    let mut stderr = std::io::stderr();
    writeln!(stderr, "{PID_MARKER}{pid}").unwrap();
    stderr.flush().unwrap();
    let mut byte = [0_u8; 1];
    let _ = std::io::stdin().read(&mut byte);
}

/// The next event, or a failure naming what was waited for.
fn next_event(rx: &Receiver<Msg>, what: &str) -> AiEvent {
    match rx.recv_timeout(view_test_support::host_deadline(WAIT)) {
        Ok(Msg::Ai(event)) => event,
        Ok(other) => panic!("waiting for {what}, got {other:?}"),
        Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {what}"),
        Err(RecvTimeoutError::Disconnected) => panic!("session ended while waiting for {what}"),
    }
}

/// A deaf adapter dies with the editor that owns it, even when that editor
/// dies by `SIGKILL` and runs no destructor at all.
///
/// Linux only: `PR_SET_PDEATHSIG` is what covers this, and no equivalent is
/// armed on the other two platforms (see `view_proc::spawn_tied_to_this_process`).
#[test]
fn a_deaf_adapter_dies_with_an_owner_that_was_killed_outright() {
    #[cfg(not(target_os = "linux"))]
    {
        view_test_support::announce_skip(
            "a_deaf_adapter_dies_with_an_owner_that_was_killed_outright",
            "no parent-death signal is armed off Linux",
        );
    }
    #[cfg(target_os = "linux")]
    {
        use std::io::BufRead;

        let dir = view_test_support::ScratchDir::new("ai-orphan-reaping").unwrap();
        let resume = dir.join("release-the-stall");
        let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["the_intermediate_parent", "--exact", "--nocapture"])
            .env(INTERMEDIATE, "1")
            .env(RESUME, &resume)
            // held open for the whole case: the intermediate parks on a read of
            // this pipe, so the only thing that ends it is the kill below
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("re-exec this test binary as the intermediate owner");
        let reader = std::io::BufReader::new(owner.stderr.take().unwrap());
        let agent = reader
            .lines()
            .map_while(Result::ok)
            .find_map(|line| {
                line.strip_prefix(PID_MARKER)
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
            })
            .unwrap_or_else(|| {
                // the intermediate parks on a pipe this process holds, so it
                // would leave once the panic drops it; killed here anyway, so a
                // case failing before its own kill does not depend on the drop
                // order for that
                let _ = owner.kill();
                let _ = owner.wait();
                panic!("the intermediate owner must report the pid of the adapter it stalled")
            });
        assert!(
            live(agent),
            "the adapter was already gone before its owner was killed, so nothing below is \
         evidence about the kill"
        );

        owner.kill().expect("kill the intermediate owner outright");
        owner.wait().expect("reap the intermediate owner");

        let deadline = Instant::now() + view_test_support::host_deadline(REAPED);
        while live(agent) {
            if Instant::now() >= deadline {
                // releases the stall so the stray ends itself: it reads its
                // stdin again, finds the owner's end of the pipe closed, and
                // exits. Killing it here would be this test signalling a
                // process it can no longer prove it owns. Waited out before the
                // panic, because the scratch directory holding the release is
                // removed by its own guard on the way out.
                std::fs::write(&resume, b"").unwrap();
                let released = Instant::now() + view_test_support::host_deadline(REAPED);
                while live(agent) && Instant::now() < released {
                    std::thread::sleep(Duration::from_millis(10));
                }
                panic!(
                    "the adapter {agent} outlived the owner that spawned it: an agent that has \
                 stopped reading its stdin sees no closed pipe, so without a parent-death \
                 signal it runs until something kills it"
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// A session on the committed stub agent, ready to take a prompt, with the
/// channel its events arrive on.
#[cfg(target_os = "linux")]
fn stub_session(dir: &view_test_support::ScratchDir) -> (AiSession, Receiver<Msg>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    let cfg = AgentLaunch::new(
        env!("CARGO_BIN_EXE_view-ai-stub-agent"),
        dir.path().to_path_buf(),
    );
    let session = AiSession::spawn(
        cfg,
        Box::new(move |msg| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(msg);
            }
        }),
    )
    .expect("the stub agent starts");
    assert!(matches!(
        next_event(&rx, "SessionReady"),
        AiEvent::SessionReady { .. }
    ));
    (session, rx)
}

/// Waits for `pid` to leave the process table entirely, reporting what it
/// was doing when the wait ran out.
///
/// A signalled child that nobody waits on stays in the table as a zombie for
/// the life of the process that spawned it, which is what this distinguishes:
/// the entry is gone once someone has collected it.
#[cfg(target_os = "linux")]
fn await_collection(pid: u32) -> Result<(), String> {
    let deadline = Instant::now() + view_test_support::host_deadline(REAPED);
    while live(pid) {
        if Instant::now() >= deadline {
            let state = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')
                        .and_then(|(_, rest)| rest.split_whitespace().next().map(String::from))
                })
                .unwrap_or_else(|| String::from("unreadable"));
            return Err(state);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

/// An adapter that exits on its own is collected, not left a zombie.
///
/// Linux only: the process state this reads is `/proc`'s.
#[test]
fn a_crashed_adapter_is_collected_rather_than_left_behind() {
    #[cfg(not(target_os = "linux"))]
    {
        view_test_support::announce_skip(
            "a_crashed_adapter_is_collected_rather_than_left_behind",
            "the process state this reads is /proc's",
        );
    }
    #[cfg(target_os = "linux")]
    {
        let dir = view_test_support::ScratchDir::new("ai-crash-reaping").unwrap();
        let (session, rx) = stub_session(&dir);
        let agent = session.pid().expect("the session must hold its adapter");
        session.send(AiCommand::Prompt {
            text: "die".to_string(),
            context: Vec::new(),
        });
        assert!(matches!(
            next_event(&rx, "SessionCrashed"),
            AiEvent::SessionCrashed { .. }
        ));
        if let Err(state) = await_collection(agent) {
            panic!(
                "the adapter {agent} that exited was never waited on (process state {state}), so \
                 an editor that restarts agents accumulates one entry per crash"
            );
        }
    }
}

/// Dropping a session collects the adapter it signals, without the editor's
/// own thread waiting for it.
///
/// Linux only, as above.
#[test]
fn a_dropped_session_collects_the_adapter_it_signalled() {
    #[cfg(not(target_os = "linux"))]
    {
        view_test_support::announce_skip(
            "a_dropped_session_collects_the_adapter_it_signalled",
            "the process state this reads is /proc's",
        );
    }
    #[cfg(target_os = "linux")]
    {
        let dir = view_test_support::ScratchDir::new("ai-drop-reaping").unwrap();
        let (session, _rx) = stub_session(&dir);
        let agent = session.pid().expect("the session must hold its adapter");
        drop(session);
        if let Err(state) = await_collection(agent) {
            panic!(
                "the adapter {agent} was signalled and never waited on (process state {state}), \
                 so every panel close leaves an entry behind"
            );
        }
    }
}

/// Whether the operating system still holds a process-table entry for `pid`.
#[cfg(target_os = "linux")]
fn live(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}
