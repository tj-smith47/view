//! The transport's own falsifiable checks, driven against the stub agent
//! rather than a mocked codec: a real child process, real pipes, and real
//! newline framing are exactly the parts a unit test cannot stand in for.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use view_ai::{AgentLaunch, AiSession};
use view_core::msg::Msg;
use view_core::native::ai_event::{
    AiCommand, AiEvent, Cost, FsError, PermissionOutcome, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, StopReason, ToolCallStatus,
};

/// Generous enough that a loaded CI host does not flake, short enough that a
/// genuinely wedged session fails the run rather than hanging it.
const WAIT: Duration = Duration::from_secs(10);

fn session_with(args: &[&str]) -> (AiSession, Receiver<Msg>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    let cfg = AgentLaunch::new(
        env!("CARGO_BIN_EXE_view-ai-stub-agent"),
        std::env::temp_dir(),
    )
    .with_args(args.iter().copied());
    let session = AiSession::spawn(
        cfg,
        Box::new(move |msg| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(msg);
            }
        }),
    )
    .expect("the stub agent starts");
    (session, rx)
}

fn session() -> (AiSession, Receiver<Msg>) {
    session_with(&[])
}

/// Like [`session_with`], but with [`AgentLaunch::requiring_auth`] set: the
/// only difference between this and every other session in this file, and
/// the reason the auth-retry test needs its own constructor rather than
/// reusing `session_with`.
fn session_requiring_auth(args: &[&str]) -> (AiSession, Receiver<Msg>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    let cfg = AgentLaunch::new(
        env!("CARGO_BIN_EXE_view-ai-stub-agent"),
        std::env::temp_dir(),
    )
    .with_args(args.iter().copied())
    .requiring_auth();
    let session = AiSession::spawn(
        cfg,
        Box::new(move |msg| {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(msg);
            }
        }),
    )
    .expect("the stub agent starts");
    (session, rx)
}

/// The next event, or a failure naming what was waited for.
fn next_event(rx: &Receiver<Msg>, what: &str) -> AiEvent {
    match rx.recv_timeout(WAIT) {
        Ok(Msg::Ai(event)) => event,
        Ok(other) => panic!("waiting for {what}, got {other:?}"),
        Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for {what}"),
        Err(RecvTimeoutError::Disconnected) => panic!("session ended while waiting for {what}"),
    }
}

fn ready(rx: &Receiver<Msg>) -> String {
    match next_event(rx, "SessionReady") {
        AiEvent::SessionReady { session_id } => session_id,
        other => panic!("expected SessionReady, got {other:?}"),
    }
}

#[test]
fn the_handshake_reaches_session_ready() {
    let (_session, rx) = session();
    assert_eq!(ready(&rx), "sess_stub");
}

#[test]
fn send_returns_while_the_agent_is_not_reading() {
    let resume = std::env::temp_dir().join(format!("view-ai-resume-{}", std::process::id()));
    let _ = std::fs::remove_file(&resume);
    let (session, rx) = session_with(&[&resume.to_string_lossy()]);
    let session = Arc::new(session);
    ready(&rx);

    // the stub answers this one and then stops reading its stdin entirely
    session.send(AiCommand::Prompt {
        text: "stall".to_string(),
        context: Vec::new(),
    });
    assert!(matches!(
        next_event(&rx, "TurnEnded"),
        AiEvent::TurnEnded { .. }
    ));

    // far more than a pipe buffer holds. The assertion is completion, not
    // elapsed time: a genuinely blocking send never finishes at all against
    // an agent that is not reading, so the falsification is the same while
    // the bound stops being a wall-clock guess on a shared host.
    let sender = Arc::clone(&session);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let bulk = "x".repeat(4096);
        for _ in 0..512 {
            sender.send(AiCommand::Prompt {
                text: bulk.clone(),
                context: Vec::new(),
            });
        }
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(WAIT)
        .expect("512 sends complete against an agent that is not reading");

    // the queue was real, not discarded: once the agent reads again, the
    // turns it never saw arrive
    std::fs::write(&resume, "go").unwrap();
    assert!(matches!(
        next_event(&rx, "the first turn the agent had not read yet"),
        AiEvent::TurnEnded { .. }
    ));
    let _ = std::fs::remove_file(&resume);
}

#[test]
fn an_agent_speaking_another_protocol_version_is_refused_at_the_handshake() {
    let (_session, rx) = session_with(&["", "2"]);
    match next_event(&rx, "SessionCrashed") {
        AiEvent::SessionCrashed { message } => {
            assert!(
                message.contains("protocol version 2") && message.contains("view speaks 1"),
                "the refusal names both versions: {message}"
            );
        }
        other => panic!("expected SessionCrashed, got {other:?}"),
    }
}

/// The falsifiable check for the retry-after-auth path: the stub fails its
/// first `session/new` with the wire's `auth_required` error, which only a
/// client that calls `authenticate` and retries `session/new` can ever get
/// past. `SessionReady` arriving at all is the proof, since nothing else in
/// this fixture's flow can produce it.
#[test]
fn a_session_new_auth_required_error_is_answered_with_authenticate_then_retried() {
    let (_session, rx) = session_requiring_auth(&["", "", "", "auth"]);
    assert_eq!(ready(&rx), "sess_stub");
}

#[test]
fn an_agent_that_dies_mid_turn_is_reported_not_swallowed() {
    let (session, rx) = session();
    ready(&rx);
    session.send(AiCommand::Prompt {
        text: "die".to_string(),
        context: Vec::new(),
    });
    match next_event(&rx, "SessionCrashed") {
        AiEvent::SessionCrashed { message } => {
            assert!(
                message.contains("exited"),
                "the crash names what happened: {message}"
            );
        }
        other => panic!("expected SessionCrashed, got {other:?}"),
    }
}

/// The falsifiable half of "never stalls paint" this crate owns: from the
/// command that kills the stub child to `SessionCrashed` reaching the
/// caller's channel is process exit detection (the child is already dead,
/// so `Child::wait` resolves at once) plus a channel send, neither of which
/// waits on anything external -- unlike `next_event`'s own `WAIT`, a ceiling
/// generous enough to absorb CI scheduling noise on every other test in this
/// file, this bound is the claim itself: a regression that made the crash
/// path block (on a lock, a retry, a second I/O round trip) would still pass
/// under `WAIT` and would only be caught here. 2 seconds is the same
/// watchdog width `view-engine`'s own burst test
/// (`ten_thousand_undrained_folds_produce_at_most_one_channel_token`) uses
/// for an identical class of claim, not a number tuned to this run.
#[test]
fn killing_the_agent_mid_turn_reports_the_crash_within_a_tight_bound() {
    let (session, rx) = session();
    ready(&rx);
    let started = Instant::now();
    session.send(AiCommand::Prompt {
        text: "die".to_string(),
        context: Vec::new(),
    });
    let event = next_event(&rx, "SessionCrashed");
    let elapsed = started.elapsed();
    assert!(
        matches!(event, AiEvent::SessionCrashed { .. }),
        "expected SessionCrashed, got {event:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "a crash mid-turn must reach the caller promptly, not stall the loop \
         that is waiting to paint it: took {elapsed:?}"
    );
}

#[test]
fn every_streamed_chunk_kind_reaches_its_own_arm() {
    let (session, rx) = session();
    ready(&rx);
    session.send(AiCommand::Prompt {
        text: "stream".to_string(),
        context: Vec::new(),
    });

    let mut seen = Vec::new();
    loop {
        match next_event(&rx, "the streamed turn") {
            AiEvent::TurnEnded { .. } => break,
            event => seen.push(event),
        }
    }

    assert_eq!(
        seen,
        vec![
            AiEvent::MessageChunk {
                message_id: Some("msg_1".to_string()),
                text: "you asked".to_string(),
                from_agent: false,
            },
            AiEvent::ThoughtChunk {
                message_id: Some("msg_1".to_string()),
                text: "thinking".to_string(),
            },
            AiEvent::MessageChunk {
                message_id: Some("msg_1".to_string()),
                text: "answering".to_string(),
                from_agent: true,
            },
            AiEvent::ToolCallUpdate {
                tool_call_id: "call_001".to_string(),
                title: "Read a.rs".to_string(),
                status: ToolCallStatus::Pending,
                content: None,
            },
            // the title comes from the announcement, which this update
            // omitted; the content array mixes a text item (decoded) with
            // an image and a terminal item (each a labeled placeholder)
            AiEvent::ToolCallUpdate {
                tool_call_id: "call_001".to_string(),
                title: "Read a.rs".to_string(),
                status: ToolCallStatus::Completed,
                content: Some(vec![
                    "fn main() {}".to_string(),
                    "[image content]".to_string(),
                    "[terminal content]".to_string(),
                ]),
            },
            AiEvent::PlanUpdated {
                entries: vec![PlanEntry {
                    content: "Read the file".to_string(),
                    priority: PlanEntryPriority::High,
                    status: PlanEntryStatus::InProgress,
                }],
            },
            AiEvent::UsageUpdated {
                used: 100,
                size: 1000,
                cost: Some(Cost {
                    amount: 0.05,
                    currency: "USD".to_string(),
                }),
            },
            // the four discriminants with no arm produced nothing
        ]
    );
}

/// Every request the stub originates carries a string id, which the schema
/// allows. A client that echoed back an id of its own choosing, or failed to
/// decode one at all, never completes any of these round trips.
#[test]
fn a_permission_request_crosses_out_and_its_answer_crosses_back() {
    let (session, rx) = session();
    ready(&rx);
    session.send(AiCommand::Prompt {
        text: "ask".to_string(),
        context: Vec::new(),
    });

    let AiEvent::PermissionRequested {
        request_id,
        tool_call_id,
        title: _,
        options,
    } = next_event(&rx, "PermissionRequested")
    else {
        panic!("expected PermissionRequested")
    };
    assert_eq!(tool_call_id, "call_001");
    assert_eq!(options.len(), 2);

    session.send(AiCommand::AnswerPermission {
        request_id,
        outcome: PermissionOutcome::Selected {
            option_id: "allow-once".to_string(),
        },
    });

    assert_eq!(
        next_event(&rx, "the agent's acknowledgement"),
        AiEvent::MessageChunk {
            message_id: Some("msg_1".to_string()),
            text: "chose allow-once".to_string(),
            from_agent: true,
        }
    );
}

/// The file the stub's `read`/`write` legs name, as this side spells it.
fn stub_fs_path() -> std::path::PathBuf {
    std::fs::canonicalize(std::env::temp_dir())
        .expect("canonicalize the session directory")
        .join("view-ai-stub-fs.txt")
}

/// An agent's `fs/read_text_file` crosses out as an event and its answer
/// crosses back as the agent's own reply -- over a real child process and
/// real pipes, which is the only place the advertised capability, the
/// dispatch arm, and the reply correlation are all exercised at once.
#[test]
fn an_agent_read_crosses_out_and_its_answer_crosses_back() {
    let (session, rx) = session();
    ready(&rx);
    session.send(AiCommand::Prompt {
        text: "read".to_string(),
        context: Vec::new(),
    });

    let AiEvent::FsReadRequested {
        request_id,
        path,
        line,
        limit,
    } = next_event(&rx, "FsReadRequested")
    else {
        panic!("expected FsReadRequested")
    };
    assert_eq!(path, stub_fs_path());
    assert_eq!((line, limit), (None, None));

    session.send(AiCommand::FsReadReply {
        request_id,
        result: Ok("from the buffer\n".to_string()),
    });

    assert_eq!(
        next_event(&rx, "the agent's report of what it read"),
        AiEvent::MessageChunk {
            message_id: Some("msg_1".to_string()),
            text: "read from the buffer\n".to_string(),
            from_agent: true,
        }
    );
}

/// The write leg of the same round trip.
#[test]
fn an_agent_write_crosses_out_and_its_answer_crosses_back() {
    let (session, rx) = session();
    ready(&rx);
    session.send(AiCommand::Prompt {
        text: "write".to_string(),
        context: Vec::new(),
    });

    let AiEvent::FsWriteRequested {
        request_id,
        path,
        content,
    } = next_event(&rx, "FsWriteRequested")
    else {
        panic!("expected FsWriteRequested")
    };
    assert_eq!(path, stub_fs_path());
    assert_eq!(content, "fn main() {}");

    session.send(AiCommand::FsWriteReply {
        request_id,
        result: Ok(()),
    });

    assert_eq!(
        next_event(&rx, "the agent's report of the write"),
        AiEvent::MessageChunk {
            message_id: Some("msg_1".to_string()),
            text: "wrote".to_string(),
            from_agent: true,
        }
    );
}

/// A read of a path outside the session directory is refused on the wire
/// and never becomes an event at all: the next thing this side sees is the
/// agent reporting the refusal, not a request it was asked to answer.
#[test]
fn an_agent_read_outside_the_session_directory_never_crosses_at_all() {
    let (session, rx) = session();
    ready(&rx);
    session.send(AiCommand::Prompt {
        text: "read-outside".to_string(),
        context: Vec::new(),
    });

    assert_eq!(
        next_event(&rx, "the agent's report of the refusal"),
        AiEvent::MessageChunk {
            message_id: Some("msg_1".to_string()),
            text: "read refused -32602".to_string(),
            from_agent: true,
        },
        "a refused path must be answered by the transport itself, never \
         raised as a request the editor is asked to serve"
    );
}

/// A path inside the session directory that names nothing readable answers
/// the wire's own "resource not found," not "internal error." The
/// distinction is the agent's, not this client's: `-32002` tells it to stop
/// asking, while `-32603` reports a client malfunction and invites it to
/// retry a call that can never succeed.
#[test]
fn a_read_that_found_nothing_answers_the_wires_resource_not_found_code() {
    let (session, rx) = session();
    ready(&rx);
    session.send(AiCommand::Prompt {
        text: "read".to_string(),
        context: Vec::new(),
    });

    let AiEvent::FsReadRequested { request_id, .. } = next_event(&rx, "FsReadRequested") else {
        panic!("expected FsReadRequested")
    };
    session.send(AiCommand::FsReadReply {
        request_id,
        result: Err(FsError::NotFound),
    });

    assert_eq!(
        next_event(&rx, "the agent's report of the failure"),
        AiEvent::MessageChunk {
            message_id: Some("msg_1".to_string()),
            text: "read refused -32002".to_string(),
            from_agent: true,
        }
    );
}

#[test]
fn a_command_sent_before_the_session_exists_is_replayed_not_dropped() {
    let (session, rx) = session();
    // queued in the same breath as the spawn, ahead of the handshake the
    // agent has not answered yet
    session.send(AiCommand::Prompt {
        text: "hello".to_string(),
        context: Vec::new(),
    });
    assert_eq!(ready(&rx), "sess_stub");
    assert!(matches!(
        next_event(&rx, "TurnEnded"),
        AiEvent::TurnEnded { .. }
    ));
}

#[test]
fn a_cancel_settles_every_permission_request_the_agent_is_holding() {
    let (session, rx) = session();
    ready(&rx);
    session.send(AiCommand::Prompt {
        text: "ask".to_string(),
        context: Vec::new(),
    });
    assert!(matches!(
        next_event(&rx, "PermissionRequested"),
        AiEvent::PermissionRequested { .. }
    ));

    session.send(AiCommand::Cancel);

    // the stub reports the outcome it was actually handed, which for this
    // path is the wire's bare `"cancelled"` and no option id at all -- a
    // reply the agent could not read at all would name its error code here
    // instead, so the two are never confused for one another
    assert_eq!(
        next_event(&rx, "the agent's reading of the cancelled outcome"),
        AiEvent::MessageChunk {
            message_id: Some("msg_1".to_string()),
            text: "chose cancelled".to_string(),
            from_agent: true,
        }
    );

    // the other half of the contract: the agent's own reply to the
    // original session/prompt call carries stopReason "cancelled", not
    // "end_turn" -- proof that the client's session/cancel notification
    // itself reached the agent, not just the permission's own cancelled
    // outcome
    assert_eq!(
        next_event(&rx, "TurnEnded"),
        AiEvent::TurnEnded {
            stop_reason: StopReason::Cancelled
        }
    );
}

#[test]
fn a_refused_turn_ends_the_turn_rather_than_killing_the_session() {
    let (session, rx) = session();
    ready(&rx);
    session.send(AiCommand::Prompt {
        text: "refuse".to_string(),
        context: Vec::new(),
    });

    assert_eq!(
        next_event(&rx, "the agent's reason"),
        AiEvent::MessageChunk {
            message_id: None,
            text: "the agent refused the turn".to_string(),
            from_agent: true,
        }
    );
    assert_eq!(
        next_event(&rx, "TurnEnded"),
        AiEvent::TurnEnded {
            stop_reason: StopReason::Refusal
        }
    );

    // still usable: the refusal was a turn ending, not a death
    session.send(AiCommand::Prompt {
        text: "hello".to_string(),
        context: Vec::new(),
    });
    assert_eq!(
        next_event(&rx, "the next turn"),
        AiEvent::TurnEnded {
            stop_reason: StopReason::EndTurn
        }
    );
}

#[test]
fn a_missing_agent_is_an_error_value_not_a_panic() {
    let cfg = AgentLaunch::new("view-ai-no-such-agent-on-any-path", std::env::temp_dir());
    let err = AiSession::spawn(cfg, Box::new(|_| {})).expect_err("a missing agent cannot start");
    assert!(
        err.to_string()
            .contains("view-ai-no-such-agent-on-any-path"),
        "the error names the command: {err}"
    );
}

#[test]
fn a_dropped_session_signals_its_agent_before_the_editor_process_is_gone() {
    let lock_path = std::env::temp_dir().join(format!(
        "view-ai-liveness-{}-{}.lock",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_file(&lock_path);

    // The editor stand-in spawns the agent, waits for it to answer, drops the
    // session, and returns from main at once. Anything the agent's own death
    // depends on happening later than that has already lost the race.
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_view-ai-drop-harness"))
        .arg(env!("CARGO_BIN_EXE_view-ai-stub-agent"))
        .arg(&lock_path)
        .status()
        .expect("the drop harness runs");
    assert!(
        status.success(),
        "the drop harness exited cleanly: {status}"
    );

    // The agent holds an exclusive lock on this file for as long as it lives
    // and keeps running after its client's pipes close, so taking the lock is
    // exactly the claim "the agent is no longer running".
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&lock_path)
        .expect("the agent created its liveness file");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(file.lock().is_ok());
    });

    let outcome = rx.recv_timeout(WAIT);
    let _ = std::fs::remove_file(&lock_path);
    match outcome {
        Ok(true) => {}
        Ok(false) => panic!("the liveness lock could not be taken at all"),
        Err(_) => panic!(
            "the agent outlived the editor process that spawned it: its liveness lock is still held"
        ),
    }
}
