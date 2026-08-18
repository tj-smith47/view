//! Test double for an ACP agent: answers `initialize`, `session/new`, and
//! `session/prompt` with the canned shapes `docs/acp-v1-wire-capture.md`
//! pins, and does nothing else unless a prompt asks it to.
//!
//! A compiled binary rather than a shell or python script, for the reason
//! the engine's own hang fixture is one: Windows `CreateProcess` cannot exec
//! a `#!` script, and an interpreter named on the command line is one more
//! thing that has to be installed on every host the gate runs on.
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
    let mut permission_id: Option<u64> = None;
    let mut pending_prompt: Option<u64> = None;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
            return;
        };
        let id = frame.get("id").and_then(serde_json::Value::as_u64);
        let method = frame
            .get("method")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();

        match (method.as_str(), id) {
            ("initialize", Some(id)) => reply(
                &mut stdout,
                id,
                serde_json::json!({
                    "protocolVersion": 1,
                    "agentCapabilities": {},
                    "agentInfo": { "name": "stub", "title": "Stub", "version": "1.0.0" },
                    "authMethods": []
                }),
            ),
            ("session/new", Some(id)) => reply(
                &mut stdout,
                id,
                serde_json::json!({ "sessionId": "sess_stub" }),
            ),
            ("session/prompt", Some(id)) => {
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
                        permission_id = Some(900);
                        pending_prompt = Some(id);
                        request(
                            &mut stdout,
                            900,
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
                            901,
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
                            902,
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
            // an answer to something this fixture asked for
            (_, Some(id)) if Some(id) == permission_id => {
                let chosen = frame["result"]["outcome"]["optionId"]
                    .as_str()
                    .unwrap_or("none")
                    .to_string();
                chunk(
                    &mut stdout,
                    "agent_message_chunk",
                    &format!("chose {chosen}"),
                );
                if let Some(prompt) = pending_prompt.take() {
                    reply(
                        &mut stdout,
                        prompt,
                        serde_json::json!({ "stopReason": "end_turn" }),
                    );
                }
                permission_id = None;
            }
            (_, Some(902)) => {
                let outcome = if frame.get("error").is_some() {
                    "refused"
                } else {
                    "wrote"
                };
                chunk(&mut stdout, "agent_message_chunk", outcome);
                if let Some(prompt) = pending_prompt.take() {
                    reply(
                        &mut stdout,
                        prompt,
                        serde_json::json!({ "stopReason": "end_turn" }),
                    );
                }
            }
            (_, Some(901)) => {
                let content = frame["result"]["content"]
                    .as_str()
                    .unwrap_or("none")
                    .to_string();
                chunk(
                    &mut stdout,
                    "agent_message_chunk",
                    &format!("read {content}"),
                );
                if let Some(prompt) = pending_prompt.take() {
                    reply(
                        &mut stdout,
                        prompt,
                        serde_json::json!({ "stopReason": "end_turn" }),
                    );
                }
            }
            _ => {}
        }
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

fn reply(stdout: &mut std::io::Stdout, id: u64, result: serde_json::Value) {
    send(
        stdout,
        &serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
    );
}

fn error_reply(stdout: &mut std::io::Stdout, id: u64, message: &str) {
    send(
        stdout,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": message }
        }),
    );
}

fn request(stdout: &mut std::io::Stdout, id: u64, method: &str, params: serde_json::Value) {
    send(
        stdout,
        &serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
    );
}

fn send(stdout: &mut std::io::Stdout, frame: &serde_json::Value) {
    let _ = writeln!(stdout, "{frame}");
    let _ = stdout.flush();
}
