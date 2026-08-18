//! Test double for an ACP agent: answers `initialize`, `session/new`, and
//! `session/prompt` with the canned shapes `docs/acp-v1-wire-capture.md`
//! pins, and does nothing else unless a prompt asks it to.
//!
//! A compiled binary rather than a shell or python script, for the reason
//! the engine's own hang fixture is one: Windows `CreateProcess` cannot exec
//! a `#!` script, and an interpreter named on the command line is one more
//! thing that has to be installed on every host the gate runs on.
//!
//! Every request this fixture originates carries a **string** id, which the
//! schema allows and which the client must echo back verbatim. A client that
//! assumed numeric ids never matches its own pending request, so every
//! string-id leg below simply never finishes -- which is the regression
//! being nailed down.
//!
//! Prompt texts it treats as instructions, so one fixture covers every
//! transport case a test needs:
//!
//! - `stall` -- stop reading stdin until the file named by the fixture's
//!   first argument exists. Proves the client's own send path never waits on
//!   the agent, since a stalled reader fills the pipe.
//! - `die` -- exit immediately, mid-turn, with the request unanswered.
//! - `stream` -- emit one `session/update` of each chunk kind, then end the
//!   turn.
//! - `ask` -- send a `session/request_permission` request, then end the turn
//!   once it is answered.
//! - `read` -- send an `fs/read_text_file` request and report what came
//!   back as a message chunk.
//! - `write` -- send an `fs/write_text_file` request and report whether it
//!   was accepted or refused.
//! - `refuse` -- answer the prompt with a JSON-RPC error instead of a
//!   result.
//! - anything else -- end the turn straight away.

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut pending_prompt: Option<serde_json::Value> = None;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
            return;
        };
        let id = frame.get("id").cloned();
        let method = frame
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        if method.is_empty() {
            // an answer to something this fixture asked for, matched on the
            // string id it chose
            match id.as_ref().and_then(serde_json::Value::as_str) {
                Some("perm-1") => {
                    let chosen = frame["result"]["outcome"]["optionId"]
                        .as_str()
                        .unwrap_or("none")
                        .to_string();
                    chunk(
                        &mut stdout,
                        "agent_message_chunk",
                        &format!("chose {chosen}"),
                    );
                    end_prompt(&mut stdout, &mut pending_prompt);
                }
                Some("fs-read-1") => {
                    let content = frame["result"]["content"]
                        .as_str()
                        .unwrap_or("none")
                        .to_string();
                    chunk(
                        &mut stdout,
                        "agent_message_chunk",
                        &format!("read {content}"),
                    );
                    end_prompt(&mut stdout, &mut pending_prompt);
                }
                Some("fs-write-1") => {
                    let outcome = if frame.get("error").is_some() {
                        "refused"
                    } else {
                        "wrote"
                    };
                    chunk(&mut stdout, "agent_message_chunk", outcome);
                    end_prompt(&mut stdout, &mut pending_prompt);
                }
                _ => {}
            }
            continue;
        }

        let Some(id) = id else {
            // a notification; the only one this fixture is sent is
            // session/cancel, which needs no answer
            continue;
        };

        match method.as_str() {
            "initialize" => reply(
                &mut stdout,
                id,
                serde_json::json!({
                    "protocolVersion": protocol_version(),
                    "agentCapabilities": {},
                    "agentInfo": { "name": "stub", "title": "Stub", "version": "1.0.0" },
                    "authMethods": []
                }),
            ),
            "session/new" => reply(
                &mut stdout,
                id,
                serde_json::json!({ "sessionId": "sess_stub" }),
            ),
            "session/prompt" => {
                let text = frame["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                match text.as_str() {
                    "stall" => {
                        reply(
                            &mut stdout,
                            id,
                            serde_json::json!({ "stopReason": "end_turn" }),
                        );
                        stall();
                    }
                    "die" => std::process::exit(9),
                    "stream" => {
                        stream_chunks(&mut stdout);
                        reply(
                            &mut stdout,
                            id,
                            serde_json::json!({ "stopReason": "end_turn" }),
                        );
                    }
                    "ask" => {
                        pending_prompt = Some(id);
                        request(
                            &mut stdout,
                            serde_json::json!("perm-1"),
                            "session/request_permission",
                            serde_json::json!({
                                "sessionId": "sess_stub",
                                "toolCall": { "toolCallId": "call_001" },
                                "options": [
                                    { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                                    { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" }
                                ]
                            }),
                        );
                    }
                    "read" => {
                        pending_prompt = Some(id);
                        request(
                            &mut stdout,
                            serde_json::json!("fs-read-1"),
                            "fs/read_text_file",
                            serde_json::json!({
                                "sessionId": "sess_stub",
                                "path": "/stub/a.rs"
                            }),
                        );
                    }
                    "write" => {
                        pending_prompt = Some(id);
                        request(
                            &mut stdout,
                            serde_json::json!("fs-write-1"),
                            "fs/write_text_file",
                            serde_json::json!({
                                "sessionId": "sess_stub",
                                "path": "/stub/a.rs",
                                "content": "fn main() {}"
                            }),
                        );
                    }
                    "refuse" => error_reply(&mut stdout, id, "the agent refused the turn"),
                    _ => reply(
                        &mut stdout,
                        id,
                        serde_json::json!({ "stopReason": "end_turn" }),
                    ),
                }
            }
            _ => {}
        }
    }
}

/// The version this fixture answers the handshake with: 1 unless the second
/// argument names another, which is how the version-mismatch path is driven.
fn protocol_version() -> i64 {
    std::env::args()
        .nth(2)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(1)
}

fn end_prompt(stdout: &mut std::io::Stdout, pending: &mut Option<serde_json::Value>) {
    if let Some(prompt) = pending.take() {
        reply(
            stdout,
            prompt,
            serde_json::json!({ "stopReason": "end_turn" }),
        );
    }
}

/// Stops reading stdin until the file named by the first argument exists,
/// or forever when no argument was given.
fn stall() {
    let resume = std::env::args().nth(1).unwrap_or_default();
    loop {
        if !resume.is_empty() && std::path::Path::new(&resume).exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn stream_chunks(stdout: &mut std::io::Stdout) {
    chunk(stdout, "user_message_chunk", "you asked");
    chunk(stdout, "agent_thought_chunk", "thinking");
    chunk(stdout, "agent_message_chunk", "answering");
    send(
        stdout,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "sess_stub",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call_001",
                    "title": "Read a.rs",
                    "status": "pending"
                }
            }
        }),
    );
    send(
        stdout,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "sess_stub",
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call_001",
                    "status": "completed"
                }
            }
        }),
    );
    for ignored in [
        "plan",
        "usage_update",
        "available_commands_update",
        "current_mode_update",
        "config_option_update",
        "session_info_update",
    ] {
        send(
            stdout,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "sess_stub",
                    "update": { "sessionUpdate": ignored }
                }
            }),
        );
    }
}

fn chunk(stdout: &mut std::io::Stdout, discriminant: &str, text: &str) {
    send(
        stdout,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "sess_stub",
                "update": {
                    "sessionUpdate": discriminant,
                    "messageId": "msg_1",
                    "content": { "type": "text", "text": text }
                }
            }
        }),
    );
}

fn reply(stdout: &mut std::io::Stdout, id: serde_json::Value, result: serde_json::Value) {
    send(
        stdout,
        &serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    );
}

fn error_reply(stdout: &mut std::io::Stdout, id: serde_json::Value, message: &str) {
    send(
        stdout,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": message }
        }),
    );
}

fn request(
    stdout: &mut std::io::Stdout,
    id: serde_json::Value,
    method: &str,
    params: serde_json::Value,
) {
    send(
        stdout,
        &serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
    );
}

fn send(stdout: &mut std::io::Stdout, frame: &serde_json::Value) {
    let _ = writeln!(stdout, "{frame}");
    let _ = stdout.flush();
}
