//! An editor stand-in that spawns an agent session, waits until the agent is
//! demonstrably serving, drops the session handle, and returns from `main`
//! immediately.
//!
//! A separate process because the question it answers cannot be asked from
//! inside the process that owns the session: "does the agent outlive the
//! editor" is only observable once the editor is gone, and a test that drops
//! a handle in its own address space keeps a live tokio runtime around that
//! would clean up after it regardless.
//!
//! Arguments, both required and positional: the stub agent binary to run,
//! and the path the stub holds its liveness lock on.

use std::time::Duration;

use view_ai::{AgentLaunch, AiSession};

/// Long enough that a loaded host still gets the agent up, short enough that
/// a wedged handshake fails the harness rather than hanging the gate.
const READY: Duration = Duration::from_secs(10);

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(agent), Some(lock_path)) = (args.next(), args.next()) else {
        eprintln!("usage: drop_harness <agent-binary> <liveness-lock-path>");
        std::process::exit(2);
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let cfg = AgentLaunch::new(agent, std::env::temp_dir()).with_args([
        // no stall-release file and the default protocol version: this
        // harness only needs the agent to answer the handshake
        String::new(),
        "1".to_string(),
        lock_path,
    ]);
    let session = match AiSession::spawn(
        cfg,
        Box::new(move |msg| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(msg);
            }
        }),
    ) {
        Ok(session) => session,
        Err(err) => {
            eprintln!("the agent did not start: {err}");
            std::process::exit(3);
        }
    };

    // An event proves the agent answered, which proves it is past the point
    // where it takes the lock. Dropping before that would leave the test
    // watching a lock nobody ever held.
    if rx.recv_timeout(READY).is_err() {
        eprintln!("the agent never answered the handshake");
        std::process::exit(4);
    }

    drop(session);

    // Exiting rather than returning is the point of the harness: an editor
    // that tears a session down on its way out gives a runtime thread no
    // time to finish anything the drop only started, so a kill that is not
    // synchronous with the drop is a kill that never happens.
    std::process::exit(0);
}
