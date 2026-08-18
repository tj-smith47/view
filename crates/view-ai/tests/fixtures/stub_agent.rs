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
//! Arguments, all optional and all positional: the file whose appearance
//! releases a stalled reader, the protocol version to answer `initialize`
//! with, the path of a file to hold an exclusive lock on for as long as
//! this process lives, and the literal `auth` to fail the first
//! `session/new` with the wire's `auth_required` error and answer
//! `authenticate` before letting a retried `session/new` succeed.
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

/// The wire's reserved code for `session/new` reporting that `authenticate`
/// must be called first, pinned in `docs/acp-v1-wire-capture.md`.
const AUTH_REQUIRED: i64 = -32000;

fn main() {
    // Taken before a single frame is served, so a client that has seen this
    // agent answer anything has also seen it take the lock.
    let liveness = liveness_lock();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut pending_prompt: Option<serde_json::Value> = None;
    // counts this fixture's own session/new attempts, so auth_mode()'s
    // one-time auth_required answer cannot fire more than once regardless
    // of how many times the client retries
    let mut session_new_attempts: u32 = 0;

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(&line) else {
            break;
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
                    "authMethods": auth_methods()
                }),
            ),
            "authenticate" => reply(&mut stdout, id, serde_json::json!({})),
            "session/new" => {
                session_new_attempts += 1;
                if auth_mode() && session_new_attempts == 1 {
                    error_reply(&mut stdout, id, AUTH_REQUIRED, "authentication required");
                } else {
                    reply(
                        &mut stdout,
                        id,
                        serde_json::json!({ "sessionId": "sess_stub" }),
                    );
                }
            }
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
                    "refuse" => {
                        error_reply(&mut stdout, id, -32603, "the agent refused the turn");
                    }
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

    outlive_the_client(liveness.as_ref());
}

/// Holds an exclusive lock on the file named by the fixture's third
/// argument, for as long as this process lives.
///
/// The lock is the only portable way a test can ask "is that process still
/// alive" about a process it does not own: every operating system releases
/// a file lock when the holder dies, and none of them lets a second holder
/// take it first.
fn liveness_lock() -> Option<std::fs::File> {
    let path = std::env::args().nth(3).filter(|arg| !arg.is_empty())?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    file.lock().ok()?;
    Some(file)
}

/// Keeps running after stdin closes, so that a client which failed to signal
/// this process leaves an observably live one behind.
///
/// A real agent is under no obligation to exit when its client disappears,
/// and one that does exit would hide exactly the bug this fixture is here to
/// expose. The wait is bounded so a fixture never outlives the machine's
/// patience if the signal it is waiting for never comes.
fn outlive_the_client(liveness: Option<&std::fs::File>) {
    if liveness.is_none() {
        return;
    }
    std::thread::sleep(std::time::Duration::from_secs(60));
}

/// The version this fixture answers the handshake with: 1 unless the second
/// argument names another, which is how the version-mismatch path is driven.
fn protocol_version() -> i64 {
    std::env::args()
        .nth(2)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(1)
}

/// Whether this fixture's fourth argument requests the auth-required retry
/// leg: the first `session/new` fails with the wire's `auth_required` code,
/// and every attempt after a successful `authenticate` succeeds.
fn auth_mode() -> bool {
    std::env::args().nth(4).as_deref() == Some("auth")
}

/// The `authMethods` this fixture advertises in its `initialize` response:
/// one method whenever `auth_mode` is set, so the client has an id to pass
/// back to `authenticate`, and none otherwise, matching every other test's
/// expectation that no auth flow is offered.
fn auth_methods() -> serde_json::Value {
    if auth_mode() {
        serde_json::json!([{ "id": "stub-login", "name": "Stub login" }])
    } else {
        serde_json::json!([])
    }
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

fn error_reply(stdout: &mut std::io::Stdout, id: serde_json::Value, code: i64, message: &str) {
    send(
        stdout,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
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
