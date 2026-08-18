//! The session task: everything that happens between a spawned agent and
//! the closed event/command vocabulary the rest of the editor speaks.
//!
//! Three tasks, not one. A reader task turns the child's stdout into
//! frames, a writer task turns frames into the child's stdin, and the
//! driver between them holds the correlation state. The split is what
//! keeps a full pipe from deadlocking the session: the driver only ever
//! sends on unbounded channels, so it can always keep draining the agent's
//! output no matter how far behind the agent is on reading its input.
//!
//! Every method name and every enum string below is pinned in
//! `docs/acp-v1-wire-capture.md`; none is recalled.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};
use view_core::native::ai_event::{
    AiCommand, AiEvent, ContextBlock, FsError, PermissionOption, PermissionOptionKind,
    PermissionOutcome, StopReason, ToolCallStatus,
};

use crate::acp::fs::PendingReply;
use crate::acp::session::SessionShared;
use crate::acp::wire::{
    Incoming, JsonRpcCodec, JsonRpcMessage, INTERNAL_ERROR, METHOD_NOT_FOUND, REQUEST_CANCELLED,
};

/// The wire protocol version this client speaks, a bare integer.
const PROTOCOL_VERSION: i64 = 1;

/// What a request this client sent is waiting to hear back about.
#[derive(Debug, Clone, Copy)]
enum Outstanding {
    Initialize,
    NewSession,
    Prompt,
}

/// What ended the read side.
enum ReaderEnd {
    /// The agent closed its stdout.
    Eof,
    /// The stream failed or carried something that is not a frame.
    Failed(String),
}

/// Runs one session to completion. Returns once the agent's output stream
/// has ended, after reporting why.
pub(crate) async fn run_session(
    mut child: Child,
    codec: JsonRpcCodec<ChildStdout, ChildStdin>,
    mut commands: mpsc::UnboundedReceiver<AiCommand>,
    shared: Arc<SessionShared>,
    cwd: std::path::PathBuf,
) {
    let (mut reader, mut writer) = codec.split();
    let (frames_tx, mut frames) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();

    tokio::spawn(async move {
        loop {
            match reader.next_message().await {
                Ok(Some(frame)) => {
                    if frames_tx.send(Ok(frame)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = frames_tx.send(Err(ReaderEnd::Eof));
                    break;
                }
                Err(err) => {
                    let _ = frames_tx.send(Err(ReaderEnd::Failed(err.to_string())));
                    break;
                }
            }
        }
    });

    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if writer.write_message(&frame).await.is_err() {
                // the child's stdin is gone; the read side is about to
                // report the same death, and reporting it twice would put
                // two crash notices in front of the user for one event
                break;
            }
        }
    });

    let mut driver = Driver::new(Arc::clone(&shared), out_tx, cwd);
    driver.begin();

    let ending = loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(Ok(frame)) => driver.on_frame(frame),
                Some(Err(end)) => break end,
                None => break ReaderEnd::Eof,
            },
            command = commands.recv() => match command {
                Some(command) => driver.on_command(command),
                // the handle was dropped: the session is being torn down on
                // purpose, so nothing is reported and the child goes with
                // the task
                None => return,
            },
        }
    };

    let detail = match ending {
        ReaderEnd::Eof => match child.wait().await {
            Ok(status) => format!("the agent exited ({status})"),
            Err(err) => format!("the agent exited and could not be reaped: {err}"),
        },
        ReaderEnd::Failed(reason) => reason,
    };
    shared.emit(AiEvent::SessionCrashed { message: detail });
}

/// The correlation state one session accumulates.
struct Driver {
    shared: Arc<SessionShared>,
    out: mpsc::UnboundedSender<JsonRpcMessage>,
    /// The directory the agent was started in, and the root the session is
    /// created against.
    cwd: std::path::PathBuf,
    next_id: u64,
    outstanding: HashMap<u64, Outstanding>,
    session_id: Option<String>,
    /// Permission requests the agent is still waiting on an answer for.
    /// Kept so an answer naming an id the agent never asked about is
    /// dropped rather than written, and so a cancel can settle every one of
    /// them.
    open_permissions: HashSet<u64>,
    /// The last title and status seen per tool call. A `tool_call_update`
    /// carries only what changed, while the event vocabulary carries a
    /// whole call, so the fields the agent omitted come from here rather
    /// than from a placeholder the panel would display.
    tool_calls: HashMap<String, (String, ToolCallStatus)>,
    /// Commands that arrived before the session existed, replayed in order
    /// once it does. Dropping them instead would lose a prompt the user had
    /// already typed.
    deferred: Vec<AiCommand>,
}

impl Driver {
    fn new(
        shared: Arc<SessionShared>,
        out: mpsc::UnboundedSender<JsonRpcMessage>,
        cwd: std::path::PathBuf,
    ) -> Self {
        Self {
            shared,
            out,
            cwd,
            next_id: 1,
            outstanding: HashMap::new(),
            session_id: None,
            open_permissions: HashSet::new(),
            tool_calls: HashMap::new(),
            deferred: Vec::new(),
        }
    }

    /// Opens the handshake.
    fn begin(&mut self) {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                // both true: an agent's file access is mediated through the
                // editor, which is the whole reason the fs events exist
                "fs": { "readTextFile": true, "writeTextFile": true },
                // false, and not a placeholder: the terminal methods are
                // unimplemented here, and claiming them would have the agent
                // wait forever on a call nothing answers
                "terminal": false
            },
            "clientInfo": {
                "name": "view",
                "title": "view",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        self.request("initialize", params, Outstanding::Initialize);
    }

    fn request(&mut self, method: &str, params: Value, kind: Outstanding) {
        let id = self.next_id;
        self.next_id += 1;
        self.outstanding.insert(id, kind);
        let _ = self.out.send(JsonRpcMessage::request(id, method, params));
    }

    fn on_frame(&mut self, frame: JsonRpcMessage) {
        match frame.classify() {
            Ok(Incoming::Response { id, outcome }) => self.on_response(id, outcome),
            Ok(Incoming::Notification { method, params }) => self.on_notification(&method, &params),
            Ok(Incoming::Request { id, method, params }) => self.on_request(id, &method, &params),
            Err(_) => {
                // a well-formed JSON object that is none of the three kinds
                // names no request to answer and no event to report, so
                // there is nothing to do with it but continue reading
            }
        }
    }

    fn on_response(&mut self, id: u64, outcome: Result<Value, crate::acp::wire::JsonRpcError>) {
        let Some(kind) = self.outstanding.remove(&id) else {
            return;
        };
        let result = match outcome {
            Ok(result) => result,
            Err(error) => {
                match kind {
                    // a refused handshake leaves no session to work in at
                    // all, which is the same dead end as a dead agent
                    Outstanding::Initialize | Outstanding::NewSession => {
                        self.shared.emit(AiEvent::SessionCrashed {
                            message: format!(
                                "the agent refused {}: {}",
                                method_of(kind),
                                error.message
                            ),
                        });
                    }
                    // a refused turn leaves the session usable, so it is
                    // reported as what it is: the agent's own words about
                    // the turn, and then the turn ending
                    Outstanding::Prompt => {
                        self.shared.emit(AiEvent::MessageChunk {
                            message_id: None,
                            text: error.message,
                            from_agent: true,
                        });
                        self.shared.emit(AiEvent::TurnEnded {
                            stop_reason: StopReason::Refusal,
                        });
                    }
                }
                return;
            }
        };
        match kind {
            Outstanding::Initialize => {
                // the same directory the agent process itself was started
                // in: a session created against a different root would have
                // the agent resolving relative paths one way and reading
                // files another
                let cwd = self.cwd.to_string_lossy().into_owned();
                self.request(
                    "session/new",
                    json!({ "cwd": cwd, "mcpServers": [] }),
                    Outstanding::NewSession,
                );
            }
            Outstanding::NewSession => {
                let Some(session_id) = result.get("sessionId").and_then(Value::as_str) else {
                    self.shared.emit(AiEvent::SessionCrashed {
                        message: "the agent created a session without an id".to_string(),
                    });
                    return;
                };
                self.session_id = Some(session_id.to_string());
                self.shared.emit(AiEvent::SessionReady {
                    session_id: session_id.to_string(),
                });
                for command in std::mem::take(&mut self.deferred) {
                    self.on_command(command);
                }
            }
            Outstanding::Prompt => {
                let stop_reason = result
                    .get("stopReason")
                    .and_then(Value::as_str)
                    .and_then(stop_reason_from_wire)
                    .unwrap_or(StopReason::EndTurn);
                self.shared.emit(AiEvent::TurnEnded { stop_reason });
            }
        }
    }

    fn on_notification(&mut self, method: &str, params: &Value) {
        if method != "session/update" {
            return;
        }
        let update = &params["update"];
        let Some(discriminant) = update.get("sessionUpdate").and_then(Value::as_str) else {
            return;
        };
        match discriminant {
            "agent_message_chunk" => self.emit_chunk(update, true),
            "user_message_chunk" => self.emit_chunk(update, false),
            "agent_thought_chunk" => {
                if let Some(text) = chunk_text(update) {
                    self.shared.emit(AiEvent::ThoughtChunk {
                        message_id: message_id(update),
                        text,
                    });
                }
            }
            "tool_call" | "tool_call_update" => self.emit_tool_call(update),
            // Deliberate no-ops, each one a surface the closed vocabulary
            // does not model: an agent's plan, its token accounting, its
            // slash-command list, its mode, its config options, and its
            // session metadata. Decoded and dropped rather than left to the
            // catch-all below, so an addition to the discriminant list is
            // still distinguishable from a surface that was weighed and
            // left out.
            "plan"
            | "usage_update"
            | "available_commands_update"
            | "current_mode_update"
            | "config_option_update"
            | "session_info_update" => {}
            _ => {}
        }
    }

    fn emit_chunk(&self, update: &Value, from_agent: bool) {
        if let Some(text) = chunk_text(update) {
            self.shared.emit(AiEvent::MessageChunk {
                message_id: message_id(update),
                text,
                from_agent,
            });
        }
    }

    fn emit_tool_call(&mut self, update: &Value) {
        let Some(tool_call_id) = update.get("toolCallId").and_then(Value::as_str) else {
            return;
        };
        let known = self.tool_calls.get(tool_call_id);
        let title = update
            .get("title")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| known.map(|(title, _)| title.clone()))
            .unwrap_or_default();
        let status = update
            .get("status")
            .and_then(Value::as_str)
            .and_then(tool_call_status_from_wire)
            .or_else(|| known.map(|(_, status)| *status))
            .unwrap_or(ToolCallStatus::Pending);
        self.tool_calls
            .insert(tool_call_id.to_string(), (title.clone(), status));
        self.shared.emit(AiEvent::ToolCallUpdate {
            tool_call_id: tool_call_id.to_string(),
            title,
            status,
        });
    }

    fn on_request(&mut self, id: u64, method: &str, params: &Value) {
        match method {
            "session/request_permission" => self.on_permission_request(id, params),
            "fs/read_text_file" => self.on_fs_read(id, params),
            "fs/write_text_file" => self.on_fs_write(id, params),
            _ => {
                let _ = self.out.send(JsonRpcMessage::error_response(
                    id,
                    METHOD_NOT_FOUND,
                    &format!("view does not implement {method}"),
                ));
            }
        }
    }

    fn on_permission_request(&mut self, id: u64, params: &Value) {
        let tool_call_id = params["toolCall"]
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let options: Vec<PermissionOption> = params
            .get("options")
            .and_then(Value::as_array)
            .map(|options| options.iter().filter_map(permission_option).collect())
            .unwrap_or_default();
        if options.is_empty() {
            // a request offering nothing this client can present is one no
            // user can answer, so it is settled with the one outcome that
            // names no option rather than left to hang
            let _ = self.out.send(JsonRpcMessage::response(
                id,
                json!({ "outcome": { "outcome": "cancelled" } }),
            ));
            return;
        }
        self.open_permissions.insert(id);
        self.shared.emit(AiEvent::PermissionRequested {
            request_id: id,
            tool_call_id,
            options,
        });
    }

    fn on_fs_read(&mut self, id: u64, params: &Value) {
        let path = std::path::PathBuf::from(
            params
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let (tx, rx) = oneshot::channel();
        let request_id = self.shared.pending().register(PendingReply::Read(tx));
        spawn_fs_reply(
            self.out.clone(),
            id,
            rx,
            |content| json!({ "content": content }),
        );
        self.shared
            .emit(AiEvent::FsReadRequested { request_id, path });
    }

    fn on_fs_write(&mut self, id: u64, params: &Value) {
        let path = std::path::PathBuf::from(
            params
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let (tx, rx) = oneshot::channel();
        let request_id = self.shared.pending().register(PendingReply::Write(tx));
        spawn_fs_reply(self.out.clone(), id, rx, |()| json!({}));
        self.shared.emit(AiEvent::FsWriteRequested {
            request_id,
            path,
            content,
        });
    }

    fn on_command(&mut self, command: AiCommand) {
        let Some(session_id) = self.session_id.clone() else {
            // filesystem answers are correlated inside this crate and need
            // no session id, so they are never deferred; everything else
            // addresses a session that does not exist yet
            match command {
                AiCommand::FsReadReply { .. } | AiCommand::FsWriteReply { .. } => {
                    self.answer_fs(command);
                }
                other => self.deferred.push(other),
            }
            return;
        };
        match command {
            AiCommand::Prompt { text, context } => {
                let mut blocks = vec![json!({ "type": "text", "text": text })];
                blocks.extend(context.iter().filter_map(context_block));
                self.request(
                    "session/prompt",
                    json!({ "sessionId": session_id, "prompt": blocks }),
                    Outstanding::Prompt,
                );
            }
            AiCommand::AnswerPermission {
                request_id,
                outcome,
            } => {
                if self.open_permissions.remove(&request_id) {
                    let _ = self.out.send(JsonRpcMessage::response(
                        request_id,
                        json!({ "outcome": permission_outcome(&outcome) }),
                    ));
                }
            }
            AiCommand::Cancel => {
                let _ = self.out.send(JsonRpcMessage::notification(
                    "session/cancel",
                    json!({ "sessionId": session_id }),
                ));
                // the transport requires every pending permission request to
                // be answered with the cancelled outcome once a turn is
                // cancelled; an unanswered one would leave the agent waiting
                // on a turn that is already over
                for request_id in std::mem::take(&mut self.open_permissions) {
                    let _ = self.out.send(JsonRpcMessage::response(
                        request_id,
                        json!({ "outcome": { "outcome": "cancelled" } }),
                    ));
                }
            }
            AiCommand::FsReadReply { .. } | AiCommand::FsWriteReply { .. } => {
                self.answer_fs(command);
            }
            // the vocabulary is declared open: a command with no wire
            // mapping here is dropped rather than guessed at, and the
            // compiler stops nothing, which is why every arm above is
            // spelled out rather than folded into this one
            _ => {}
        }
    }

    fn answer_fs(&self, command: AiCommand) {
        match command {
            AiCommand::FsReadReply { request_id, result } => {
                if let Some(PendingReply::Read(sender)) = self.shared.pending().take(request_id) {
                    let _ = sender.send(result);
                }
            }
            AiCommand::FsWriteReply { request_id, result } => {
                if let Some(PendingReply::Write(sender)) = self.shared.pending().take(request_id) {
                    let _ = sender.send(result);
                }
            }
            _ => {}
        }
    }
}

/// Awaits one filesystem answer and writes the agent's reply, off the
/// driver's own path: the driver must keep draining the agent's output
/// while the editor decides, and an answer that never comes must not stop
/// it doing so.
fn spawn_fs_reply<T, F>(
    out: mpsc::UnboundedSender<JsonRpcMessage>,
    id: u64,
    rx: oneshot::Receiver<Result<T, FsError>>,
    to_result: F,
) where
    T: Send + 'static,
    F: FnOnce(T) -> Value + Send + 'static,
{
    tokio::spawn(async move {
        let frame = match rx.await {
            Ok(Ok(value)) => JsonRpcMessage::response(id, to_result(value)),
            Ok(Err(error)) => {
                JsonRpcMessage::error_response(id, INTERNAL_ERROR, &fs_reason(&error))
            }
            // the sender was dropped without an answer: the editor is going
            // away, which for the agent is the same as its request being
            // cancelled
            Err(_) => JsonRpcMessage::error_response(id, REQUEST_CANCELLED, "cancelled"),
        };
        let _ = out.send(frame);
    });
}

fn fs_reason(error: &FsError) -> String {
    match error {
        FsError::NotFound => "no such file".to_string(),
        FsError::PermissionDenied => "permission denied".to_string(),
        FsError::Other { message } => message.clone(),
        // the vocabulary is declared open, so a reason with no wording here
        // still has to reach the agent as a refusal rather than as silence
        _ => "the request could not be answered".to_string(),
    }
}

fn method_of(kind: Outstanding) -> &'static str {
    match kind {
        Outstanding::Initialize => "initialize",
        Outstanding::NewSession => "session/new",
        Outstanding::Prompt => "session/prompt",
    }
}

fn message_id(update: &Value) -> Option<String> {
    update
        .get("messageId")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

/// The text of a streamed chunk, or `None` when the chunk carries content
/// this vocabulary has no arm for: an image, audio, or an embedded
/// resource. Rendering those as empty text would put a blank line in the
/// transcript where content actually arrived.
fn chunk_text(update: &Value) -> Option<String> {
    let content = update.get("content")?;
    if content.get("type").and_then(Value::as_str)? != "text" {
        return None;
    }
    content
        .get("text")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn stop_reason_from_wire(wire: &str) -> Option<StopReason> {
    match wire {
        "end_turn" => Some(StopReason::EndTurn),
        "max_tokens" => Some(StopReason::MaxTokens),
        "max_turn_requests" => Some(StopReason::MaxTurnRequests),
        "refusal" => Some(StopReason::Refusal),
        "cancelled" => Some(StopReason::Cancelled),
        _ => None,
    }
}

fn tool_call_status_from_wire(wire: &str) -> Option<ToolCallStatus> {
    match wire {
        "pending" => Some(ToolCallStatus::Pending),
        "in_progress" => Some(ToolCallStatus::InProgress),
        "completed" => Some(ToolCallStatus::Completed),
        "failed" => Some(ToolCallStatus::Failed),
        _ => None,
    }
}

/// One offered permission option, or `None` when its kind is not one of the
/// four the protocol defines: an option whose nature cannot be read is one
/// the panel cannot present honestly, and guessing between allow and reject
/// is the one mistake with a cost.
fn permission_option(raw: &Value) -> Option<PermissionOption> {
    let kind = match raw.get("kind").and_then(Value::as_str)? {
        "allow_once" => PermissionOptionKind::AllowOnce,
        "allow_always" => PermissionOptionKind::AllowAlways,
        "reject_once" => PermissionOptionKind::RejectOnce,
        "reject_always" => PermissionOptionKind::RejectAlways,
        _ => return None,
    };
    Some(PermissionOption {
        option_id: raw.get("optionId").and_then(Value::as_str)?.to_string(),
        name: raw.get("name").and_then(Value::as_str)?.to_string(),
        kind,
    })
}

fn permission_outcome(outcome: &PermissionOutcome) -> Value {
    match outcome {
        PermissionOutcome::Cancelled => json!({ "outcome": "cancelled" }),
        PermissionOutcome::Selected { option_id } => {
            json!({ "outcome": "selected", "optionId": option_id })
        }
    }
}

/// One attached context block as a wire content block, or `None` when the
/// vocabulary has grown a kind with no wire mapping here: an unmappable
/// attachment is left out of the turn rather than sent as empty text the
/// agent would read as a blank message.
fn context_block(block: &ContextBlock) -> Option<Value> {
    match block {
        ContextBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
        ContextBlock::ResourceLink { uri, name } => {
            Some(json!({ "type": "resource_link", "uri": uri, "name": name }))
        }
        _ => None,
    }
}
