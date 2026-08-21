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
//! - `tool-call` -- emit half a message, announce one tool call as
//!   non-terminal, hold both there until the resume file named by the
//!   fixture's first argument appears, then emit the rest of the message
//!   and complete the call. The pause is the whole point: it is what makes
//!   a partly-rendered turn observable, rather than frames that arrive
//!   together and are indistinguishable from a turn rendered at its end.
//! - `stream` -- emit one `session/update` of each chunk kind, then end the
//!   turn.
//! - `stream-forever` -- emit a numbered `agent_message_chunk` every
//!   [`SUSTAINED_INTERVAL`] and never answer the prompt, so the turn stays
//!   in flight for as long as the client keeps reading. Written for the
//!   bench rows that measure view with a live agent session present: a
//!   canned turn that ends is over long before a sampling run is, and a row
//!   that sampled after it ended would measure the session-absent path
//!   under a name that claims otherwise. The count of chunks written so far
//!   is kept in the file [`sustained_progress`] resolves -- the fifth
//!   argument, or [`SUSTAINED_PROGRESS_FILE`] beside the working directory
//!   (replaced by [`SUSTAINED_CEILING_SENTINEL`] if the loop ever reaches
//!   its own ceiling),
//!   which is how a driver checks the stream really did run for the whole
//!   sampling window rather than stopping after the first frame.
//! - `ask` -- send a `session/request_permission` request, then end the turn
//!   once it is answered.
//! - `ask-twice` -- send a second `session/request_permission` while the
//!   first is still unanswered, and report what came back for it as a
//!   message chunk. The overlap degrade is a client-side policy with no
//!   wire mandate behind it (`docs/acp-v1-wire-capture.md`), so what
//!   matters is that a real turn survives it: the first request stays open
//!   and the turn still ends on its answer.
//! - `propose` -- announce a tool call and complete it with a
//!   `ToolCallContent` `"diff"` item over `view-ai-stub-diff.txt` in this
//!   process's own working directory. Any suffix after the word (`propose2`)
//!   picks a different edit and a different tool call id, so a second
//!   proposal in the same session is a genuinely new one rather than a
//!   duplicate the driver deduplicates away.
//! - `read` -- send an `fs/read_text_file` request for a file inside this
//!   process's own working directory (which is the session's, and so the
//!   only directory the client answers for) and report what came back as a
//!   message chunk.
//! - `read-outside` -- the same request for a path nowhere near that
//!   directory, so the leg the client refuses can be driven over a real
//!   wire rather than only in a unit test. A refused read reports the
//!   error's `code` rather than its message.
//! - `write` -- send an `fs/write_text_file` request, also inside the
//!   working directory, and report whether it was accepted or refused.
//! - `refuse` -- answer the prompt with a JSON-RPC error instead of a
//!   result.
//! - anything else -- end the turn straight away.

use std::io::{BufRead, Write};

/// The wire's reserved code for `session/new` reporting that `authenticate`
/// must be called first, pinned in `docs/acp-v1-wire-capture.md`.
const AUTH_REQUIRED: i64 = -32000;

/// The file the `read`/`write` legs name.
///
/// Built from this process's own working directory rather than written as a
/// literal: the client answers filesystem requests only for paths inside
/// the session directory, and the session directory is exactly the one it
/// spawned this fixture in.
fn inside_cwd() -> String {
    named_inside_cwd("view-ai-stub-fs.txt")
}

/// The absolute path of `name` inside this process's working directory,
/// which is the session directory the client answers for.
fn named_inside_cwd(name: &str) -> String {
    std::env::current_dir()
        .unwrap_or_default()
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// The file the `propose` leg offers edits to.
const DIFF_FILE: &str = "view-ai-stub-diff.txt";

/// What each `propose` suffix claims the file holds, and what it offers to
/// make of it: the bare word touches the middle line of the seeded file,
/// any suffix touches the last line of what accepting the first one leaves
/// behind. The client diffs `old` against `new` to derive the hunks it
/// offers and anchors them in the buffer, so a proposal whose `old` is not
/// what the buffer actually holds gets a review of stale hunks -- which is
/// why the second one states the first one's result rather than the seed.
fn diff_texts(suffix: &str) -> (&'static str, &'static str) {
    if suffix.is_empty() {
        ("alpha\nbeta\ngamma\n", "alpha\nBETA\ngamma\n")
    } else {
        ("alpha\nBETA\ngamma\n", "alpha\nBETA\nGAMMA\n")
    }
}

/// What a `session/request_permission` answer actually said, in one word:
/// the chosen `optionId` for the wire's `"selected"` variant, the bare
/// outcome string for `"cancelled"`, and the error code for a reply that
/// was no outcome at all.
///
/// Reported this precisely because a client's reply body is not otherwise
/// observable from outside the two processes -- this fixture is the only
/// witness to what it received, and "none" for every shape it could not
/// read would make a malformed reply indistinguishable from a correct
/// cancellation.
fn outcome_label(frame: &serde_json::Value) -> String {
    let outcome = &frame["result"]["outcome"];
    outcome["optionId"]
        .as_str()
        .or_else(|| outcome["outcome"].as_str())
        .map_or_else(
            || format!("error {}", frame["error"]["code"]),
            str::to_string,
        )
}

fn main() {
    // Taken before a single frame is served, so a client that has seen this
    // agent answer anything has also seen it take the lock.
    let liveness = liveness_lock();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut pending_prompt: Option<serde_json::Value> = None;
    // Set only once this fixture has actually received `authenticate`, so
    // `session/new` still refuses a client that retries without
    // authenticating first -- an attempt counter would let any second
    // attempt through regardless of whether authentication happened.
    let mut authenticated = false;
    // Set on `session/cancel` and consulted the next time a pending prompt
    // is resolved: the transport requires this fixture's own reply to
    // `session/prompt` to carry `stopReason: "cancelled"` once the client
    // cancelled the turn, not the `"end_turn"` every other path answers
    // with, so a client that only settled the permission's own outcome and
    // never actually notified `session/cancel` is distinguishable from one
    // that did both.
    let mut cancelled = false;

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
                    chunk(
                        &mut stdout,
                        "agent_message_chunk",
                        &format!("chose {}", outcome_label(&frame)),
                    );
                    end_prompt(&mut stdout, &mut pending_prompt, stop_reason_for(cancelled));
                    cancelled = false;
                }
                Some("perm-2") => {
                    // reported rather than ended on: the first request is
                    // still open, and a turn that ended here would hide
                    // whether the overlap disturbed it
                    chunk(
                        &mut stdout,
                        "agent_message_chunk",
                        &format!("overlap {}", outcome_label(&frame)),
                    );
                }
                Some("fs-read-1") => {
                    let content = if let Some(error) = frame.get("error") {
                        // the code, not the prose: an agent's control flow
                        // keys on it, so it is the part worth reporting
                        // back over a real wire
                        format!("refused {}", error["code"])
                    } else {
                        frame["result"]["content"]
                            .as_str()
                            .unwrap_or("none")
                            .to_string()
                    };
                    chunk(
                        &mut stdout,
                        "agent_message_chunk",
                        &format!("read {content}"),
                    );
                    end_prompt(&mut stdout, &mut pending_prompt, stop_reason_for(cancelled));
                    cancelled = false;
                }
                Some("fs-write-1") => {
                    let outcome = if frame.get("error").is_some() {
                        "refused"
                    } else {
                        "wrote"
                    };
                    chunk(&mut stdout, "agent_message_chunk", outcome);
                    end_prompt(&mut stdout, &mut pending_prompt, stop_reason_for(cancelled));
                    cancelled = false;
                }
                _ => {}
            }
            continue;
        }

        let Some(id) = id else {
            // a notification; the only one this fixture is sent is
            // session/cancel, which flips the flag `end_prompt`'s stop
            // reason is read from the next time a pending prompt resolves
            if method == "session/cancel" {
                cancelled = true;
            }
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
            "authenticate" => {
                let method_id = frame["params"]["methodId"].as_str().unwrap_or_default();
                if method_id == "stub-login" {
                    authenticated = true;
                    reply(&mut stdout, id, serde_json::json!({}));
                } else {
                    error_reply(
                        &mut stdout,
                        id,
                        -32602,
                        "unexpected methodId in authenticate",
                    );
                }
            }
            "session/new" => {
                if auth_mode() && !authenticated {
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
                    "stream-forever" => stream_sustained(&mut stdout),
                    "stream" => {
                        stream_chunks(&mut stdout);
                        reply(
                            &mut stdout,
                            id,
                            serde_json::json!({ "stopReason": "end_turn" }),
                        );
                    }
                    "tool-call" => {
                        // The two halves are separated by the resume file
                        // rather than sent together, so the non-terminal
                        // status is on screen long enough to be read: a
                        // transcript keeps one row per call, and a terminal
                        // update written in the same breath overwrites the
                        // status nobody got to see.
                        chunk(&mut stdout, "agent_message_chunk", "streaming");
                        tool_call_status(&mut stdout, "in_progress", None);
                        stall();
                        chunk(&mut stdout, "agent_message_chunk", " and done");
                        tool_call_status(&mut stdout, "completed", Some("read 3 lines"));
                        reply(
                            &mut stdout,
                            id,
                            serde_json::json!({ "stopReason": "end_turn" }),
                        );
                    }
                    "ask" => {
                        pending_prompt = Some(id);
                        ask_permission(&mut stdout, "perm-1", "call_001");
                    }
                    "ask-twice" => {
                        pending_prompt = Some(id);
                        ask_permission(&mut stdout, "perm-1", "call_001");
                        // no wait in between: the overlap under test is a
                        // second request arriving while the first is still
                        // unanswered, which only happens if this one is
                        // written before any answer could have come back
                        ask_permission(&mut stdout, "perm-2", "call_002");
                    }
                    "read" => {
                        pending_prompt = Some(id);
                        request(
                            &mut stdout,
                            serde_json::json!("fs-read-1"),
                            "fs/read_text_file",
                            serde_json::json!({
                                "sessionId": "sess_stub",
                                "path": inside_cwd()
                            }),
                        );
                    }
                    "read-outside" => {
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
                                "path": inside_cwd(),
                                "content": "fn main() {}"
                            }),
                        );
                    }
                    "refuse" => {
                        error_reply(&mut stdout, id, -32603, "the agent refused the turn");
                    }
                    proposal if proposal.starts_with("propose") => {
                        propose_diff(&mut stdout, &proposal["propose".len()..]);
                        reply(
                            &mut stdout,
                            id,
                            serde_json::json!({ "stopReason": "end_turn" }),
                        );
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

fn end_prompt(
    stdout: &mut std::io::Stdout,
    pending: &mut Option<serde_json::Value>,
    stop_reason: &str,
) {
    if let Some(prompt) = pending.take() {
        reply(
            stdout,
            prompt,
            serde_json::json!({ "stopReason": stop_reason }),
        );
    }
}

/// The wire's own two spellings this fixture ever answers a resolved
/// prompt with: `"cancelled"` once `session/cancel` arrived since the
/// prompt was issued, `"end_turn"` otherwise.
fn stop_reason_for(cancelled: bool) -> &'static str {
    if cancelled {
        "cancelled"
    } else {
        "end_turn"
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

/// Spacing between `stream-forever`'s chunks. Fixed rather than argued
/// over the wire so two runs of a measurement that holds a session live
/// face the same agent, and slow enough that the stream is a live session
/// in the background rather than a flood the row would end up measuring.
const SUSTAINED_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// Ceiling on how long `stream-forever` keeps streaming. The loop's real
/// end is the client going away, which the write below sees; this is what
/// stops a fixture whose client died without closing the pipe from
/// outliving the run that spawned it.
const SUSTAINED_CEILING: std::time::Duration = std::time::Duration::from_secs(900);

/// Where `stream-forever` records how many chunks it has written, when the
/// client did not name a path with the fifth argument.
const SUSTAINED_PROGRESS_FILE: &str = "view-ai-stub-stream-progress.txt";

/// The path `stream-forever` records its count in: the fifth argument when
/// the client named one, and [`SUSTAINED_PROGRESS_FILE`] in this process's
/// working directory otherwise.
///
/// The argument exists because the working directory is also the session
/// directory a client may be watching for external writes, and a count
/// rewritten every [`SUSTAINED_INTERVAL`] inside it is a write the client
/// answers -- traffic a measurement holding this stream live would be
/// reading as its own subject.
fn sustained_progress() -> String {
    std::env::args()
        .nth(5)
        .filter(|arg| !arg.is_empty())
        .unwrap_or_else(|| named_inside_cwd(SUSTAINED_PROGRESS_FILE))
}

/// What `stream-forever` leaves in that file when it stops on the ceiling
/// rather than because the client went away, so a reader watching the
/// count can tell "the stream ended by itself" from "the stream is still
/// going" instead of reading a stalled number as either.
const SUSTAINED_CEILING_SENTINEL: &str = "ceiling";

/// Streams numbered chunks until the client stops reading, recording the
/// count as it goes. Never replies to the prompt: the turn it belongs to
/// is meant to still be in flight when the caller stops watching.
fn stream_sustained(stdout: &mut std::io::Stdout) {
    let progress = sustained_progress();
    let start = std::time::Instant::now();
    let mut written: u64 = 0;
    while start.elapsed() < SUSTAINED_CEILING {
        written += 1;
        let frame = chunk_frame("agent_message_chunk", &format!("chunk {written} "));
        if writeln!(stdout, "{frame}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return;
        }
        let _ = std::fs::write(&progress, written.to_string());
        std::thread::sleep(SUSTAINED_INTERVAL);
    }
    let _ = std::fs::write(&progress, SUSTAINED_CEILING_SENTINEL);
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
                    "status": "completed",
                    "content": [
                        { "type": "content", "content": { "type": "text", "text": "fn main() {}" } },
                        { "type": "content", "content": { "type": "image" } },
                        { "type": "terminal", "terminalId": "term_1" }
                    ]
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
                    "sessionUpdate": "plan",
                    "entries": [
                        { "content": "Read the file", "priority": "high", "status": "in_progress" }
                    ]
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
                    "sessionUpdate": "usage_update",
                    "used": 100,
                    "size": 1000,
                    "cost": { "amount": 0.05, "currency": "USD" }
                }
            }
        }),
    );
    for ignored in [
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

/// One tool call at `status`, announced under a title the first frame
/// establishes and every later one inherits (`docs/acp-v1-wire-capture.md`:
/// `ToolCallUpdate` requires only `toolCallId`).
fn tool_call_status(stdout: &mut std::io::Stdout, status: &str, result: Option<&str>) {
    let discriminant = if result.is_none() {
        "tool_call"
    } else {
        "tool_call_update"
    };
    let mut update = serde_json::json!({
        "sessionUpdate": discriminant,
        "toolCallId": "call_probe",
        "status": status
    });
    if let Some(result) = result {
        update["content"] = serde_json::json!([
            { "type": "content", "content": { "type": "text", "text": result } }
        ]);
    } else {
        update["title"] = serde_json::json!("Probe the file");
    }
    send(
        stdout,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": { "sessionId": "sess_stub", "update": update }
        }),
    );
}

/// One `session/request_permission` with the two options every permission
/// leg here offers, under the request id the answer is matched back on.
fn ask_permission(stdout: &mut std::io::Stdout, request_id: &str, tool_call_id: &str) {
    request(
        stdout,
        serde_json::json!(request_id),
        "session/request_permission",
        serde_json::json!({
            "sessionId": "sess_stub",
            "toolCall": { "toolCallId": tool_call_id },
            "options": [
                { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" }
            ]
        }),
    );
}

/// A tool call announced and then completed carrying one `"diff"` content
/// item, which is how an edit is offered for review: the announcement is a
/// separate frame so the non-terminal status is on screen before the
/// terminal one replaces it, rather than the call appearing already
/// finished.
fn propose_diff(stdout: &mut std::io::Stdout, suffix: &str) {
    let tool_call_id = format!("edit_{}", if suffix.is_empty() { "1" } else { suffix });
    send(
        stdout,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "sess_stub",
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": tool_call_id,
                    "title": "Edit view-ai-stub-diff.txt",
                    "status": "in_progress"
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
                    "toolCallId": tool_call_id,
                    "status": "completed",
                    "content": [{
                        "type": "diff",
                        "path": named_inside_cwd(DIFF_FILE),
                        "oldText": diff_texts(suffix).0,
                        "newText": diff_texts(suffix).1
                    }]
                }
            }
        }),
    );
}

fn chunk(stdout: &mut std::io::Stdout, discriminant: &str, text: &str) {
    send(stdout, &chunk_frame(discriminant, text));
}

/// One chunk frame, built apart from writing it so the sustained stream can
/// write it through a path that reports the broken pipe `send` swallows.
fn chunk_frame(discriminant: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
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
    })
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
