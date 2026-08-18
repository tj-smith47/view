//! The transport's own falsifiable checks, driven against the stub agent
//! rather than a mocked codec: a real child process, real pipes, and real
//! newline framing are exactly the parts a unit test cannot stand in for.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use view_ai::{AiConfig, AiSession};
use view_core::msg::Msg;
use view_core::native::ai_event::{
    AiCommand, AiEvent, FsError, PermissionOutcome, StopReason, ToolCallStatus,
};

/// Generous enough that a loaded CI host does not flake, short enough that a
/// genuinely wedged session fails the run rather than hanging it.
const WAIT: Duration = Duration::from_secs(10);

fn session_with(args: &[&str]) -> (AiSession, Receiver<Msg>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let tx = Mutex::new(tx);
    let cfg = AiConfig::new(
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
            },
            // the title comes from the announcement, which this update
            // omitted: the six discriminants with no arm produced nothing
            AiEvent::ToolCallUpdate {
                tool_call_id: "call_001".to_string(),
                title: "Read a.rs".to_string(),
                status: ToolCallStatus::Completed,
            },
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

#[test]
fn an_agent_file_read_crosses_out_and_its_reply_crosses_back() {
    let (session, rx) = session();
    ready(&rx);
    session.send(AiCommand::Prompt {
        text: "read".to_string(),
        context: Vec::new(),
    });

    let AiEvent::FsReadRequested { request_id, path } = next_event(&rx, "FsReadRequested") else {
        panic!("expected FsReadRequested")
    };
    assert_eq!(path, std::path::PathBuf::from("/stub/a.rs"));

    session.send(AiCommand::FsReadReply {
        request_id,
        result: Ok("fn main() {}".to_string()),
    });

    assert_eq!(
        next_event(&rx, "the agent's echo of the read"),
        AiEvent::MessageChunk {
            message_id: Some("msg_1".to_string()),
            text: "read fn main() {}".to_string(),
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
fn an_agent_file_write_crosses_out_and_its_failure_crosses_back() {
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
    assert_eq!(path, std::path::PathBuf::from("/stub/a.rs"));
    assert_eq!(content, "fn main() {}");

    session.send(AiCommand::FsWriteReply {
        request_id,
        result: Err(FsError::PermissionDenied),
    });

    assert_eq!(
        next_event(&rx, "the agent's report of the refusal"),
        AiEvent::MessageChunk {
            message_id: Some("msg_1".to_string()),
            text: "refused".to_string(),
            from_agent: true,
        }
    );
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

    // the stub reports which option it was told about; a cancelled outcome
    // names none, which is what the transport is required to answer with
    assert_eq!(
        next_event(&rx, "the agent's reading of the cancelled outcome"),
        AiEvent::MessageChunk {
            message_id: Some("msg_1".to_string()),
            text: "chose none".to_string(),
            from_agent: true,
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
    let cfg = AiConfig::new("view-ai-no-such-agent-on-any-path", std::env::temp_dir());
    let err = AiSession::spawn(cfg, Box::new(|_| {})).expect_err("a missing agent cannot start");
    assert!(
        err.to_string()
            .contains("view-ai-no-such-agent-on-any-path"),
        "the error names the command: {err}"
    );
}
