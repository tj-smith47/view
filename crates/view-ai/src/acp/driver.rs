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
//! Unbounded is a deliberate trade with a real cost, not a default. A
//! bounded channel would make the driver's own sends either await (which
//! reintroduces the stall the split exists to remove, since the driver is
//! what drains the agent) or drop (which loses a user's prompt or an answer
//! the agent is blocked on). What unbounded buys instead is that a backlog
//! grows with the agent's unresponsiveness rather than wedging anything, and
//! what it costs is that a permanently unresponsive agent grows the backlog
//! until the process runs out of memory. Nothing here caps or reports the
//! depth, so an agent that accepts input and never answers is bounded only
//! by how long the user keeps typing at it.
//!
//! Both stream halves report their own ending. The reader's is obvious; the
//! writer's is not, and is the reason it has a channel of its own: an agent
//! that breaks its stdin while holding stdout open leaves the reader parked
//! on a stream that never ends, so nothing else would ever notice that
//! everything sent since is going nowhere.
//!
//! Every method name and every enum string below is pinned in
//! `docs/acp-v1-wire-capture.md`; none is recalled.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError};

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use view_core::native::ai_context::{DiagnosticEntry, DiagnosticSeverity, QuickfixEntry};
use view_core::native::ai_event::{
    AiCommand, AiEvent, ContextBlock, FsError, PermissionOption, PermissionOptionKind,
    PermissionOutcome, StopReason, ToolCallStatus,
};

use crate::acp::fs::PendingReply;
use crate::acp::session::{ChildSlot, SessionShared};
use crate::acp::wire::{
    Incoming, JsonRpcCodec, JsonRpcError, JsonRpcMessage, RequestId, AUTH_REQUIRED, INTERNAL_ERROR,
    METHOD_NOT_FOUND, REQUEST_CANCELLED,
};

/// The wire protocol version this client speaks, a bare integer.
const PROTOCOL_VERSION: i64 = 1;

/// What a request this client sent is waiting to hear back about.
#[derive(Debug, Clone, Copy)]
enum Outstanding {
    Initialize,
    Authenticate,
    NewSession,
    Prompt,
}

/// What ended the session, from whichever half noticed first.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// The agent closed its stdout.
    ReaderEof,
    /// The agent's output stream failed or carried something that is not a
    /// frame.
    ReaderFailed(String),
    /// The agent's input stream stopped accepting frames. Reported in its
    /// own right rather than left to the reader: an agent can break its
    /// stdin and hold its stdout open, and then nothing is ever delivered
    /// and nothing ever ends.
    WriterFailed(String),
}

/// Runs one session to completion, reporting why it ended.
pub(crate) async fn run_session(
    child: ChildSlot,
    codec: JsonRpcCodec<tokio::process::ChildStdout, tokio::process::ChildStdin>,
    commands: mpsc::UnboundedReceiver<AiCommand>,
    shared: Arc<SessionShared>,
    cwd: std::path::PathBuf,
    requires_auth: bool,
) {
    let Some(ending) = drive(codec, commands, Arc::clone(&shared), cwd, requires_auth).await else {
        // the handle was dropped: the session is being torn down on purpose,
        // and the handle's own `Drop` has already signalled the child
        return;
    };

    // The signal is sent while the lock is still held, and the child leaves
    // the slot only afterwards. That ordering is what makes the state where
    // neither teardown path signalled unreachable: the handle's `Drop` either
    // finds the child still in the slot and signals it itself, or blocks on
    // the lock until this path has signalled and taken it. Killing after
    // releasing the guard would leave exactly that gap, since a `Drop` landing
    // inside it sees an empty slot and a live child. Signalling on the way out
    // matters even for a reader that saw end-of-file, because an agent may
    // close its stdout and keep running.
    let mut child = {
        let mut slot = child.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(child) = slot.as_mut() {
            let _ = child.start_kill();
        }
        slot.take()
    };

    let detail = match ending {
        SessionEnd::ReaderEof => match child.as_mut() {
            Some(child) => match child.wait().await {
                Ok(status) => format!("the agent exited ({status})"),
                Err(err) => format!("the agent exited and could not be reaped: {err}"),
            },
            None => "the agent exited".to_string(),
        },
        SessionEnd::ReaderFailed(reason) => reason,
        SessionEnd::WriterFailed(reason) => {
            format!("the agent stopped accepting input: {reason}")
        }
    };
    shared.emit(AiEvent::SessionCrashed { message: detail });
}

/// The session loop itself, over any stream pair rather than a child's
/// pipes: what ends a session is a property of the streams, and the one
/// ending that cannot be produced with a real child is the half-wedge this
/// separation exists to make testable.
///
/// `None` means the command handle was dropped, which is a deliberate
/// teardown and not something to report.
async fn drive<R, W>(
    codec: JsonRpcCodec<R, W>,
    mut commands: mpsc::UnboundedReceiver<AiCommand>,
    shared: Arc<SessionShared>,
    cwd: std::path::PathBuf,
    requires_auth: bool,
) -> Option<SessionEnd>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (mut reader, mut writer) = codec.split();
    let (ends_tx, mut frames) = mpsc::unbounded_channel();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();

    let reader_ends = ends_tx.clone();
    tokio::spawn(async move {
        loop {
            match reader.next_message().await {
                Ok(Some(frame)) => {
                    if reader_ends.send(Ok(frame)).is_err() {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = reader_ends.send(Err(SessionEnd::ReaderEof));
                    break;
                }
                Err(err) => {
                    let _ = reader_ends.send(Err(SessionEnd::ReaderFailed(err.to_string())));
                    break;
                }
            }
        }
    });

    tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if let Err(err) = writer.write_message(&frame).await {
                let _ = ends_tx.send(Err(SessionEnd::WriterFailed(err.to_string())));
                break;
            }
        }
    });

    let mut driver = Driver::new(shared, out_tx, cwd, requires_auth);
    driver.begin();

    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(Ok(frame)) => driver.on_frame(frame),
                Some(Err(end)) => return Some(end),
                // both halves are gone without either having said why, which
                // only happens if a task was dropped out from under the loop
                None => return Some(SessionEnd::ReaderEof),
            },
            command = commands.recv() => match command {
                Some(command) => driver.on_command(command),
                None => return None,
            },
        }
    }
}

/// The correlation state one session accumulates.
struct Driver {
    shared: Arc<SessionShared>,
    out: mpsc::UnboundedSender<JsonRpcMessage>,
    /// The directory the agent was started in, and the root the session is
    /// created against.
    cwd: std::path::PathBuf,
    /// The next id for a request this client sends. Numeric because view
    /// chooses its own shape for the ids it originates; an agent's own ids
    /// keep whatever shape the agent gave them.
    next_wire_id: i64,
    /// The one id space that crosses the boundary. Every `request_id` on an
    /// `AiEvent` is drawn from here -- permission requests and filesystem
    /// requests alike -- so a consumer holding two of them can compare or
    /// key on them without knowing which kind it has. An agent's own wire
    /// ids never leave this file.
    next_boundary_id: u64,
    outstanding: HashMap<RequestId, Outstanding>,
    /// Whether the agent enforces `authenticate` before `session/new`
    /// succeeds, from [`AgentLaunch::requiring_auth`](crate::AgentLaunch::requiring_auth)
    /// (or an [`AgentAdapter`](crate::acp::session::AgentAdapter) that set
    /// it). Gates the retry-after-`auth_required` path so an agent that
    /// merely advertises optional methods is never made to authenticate
    /// against its own wishes.
    requires_auth: bool,
    /// The method ids `initialize` advertised in `authMethods`, in order.
    /// Empty means the agent offered none, which makes the retry path a
    /// no-op regardless of `requires_auth`.
    auth_methods: Vec<String>,
    /// Set once `authenticate` has been sent, so a `session/new` that fails
    /// with `auth_required` a second time is reported rather than retried
    /// forever against an agent whose rejection has nothing to do with
    /// authentication.
    auth_attempted: bool,
    session_id: Option<String>,
    /// Permission requests the agent is still waiting on an answer for,
    /// from the boundary id the event carried to the wire id the answer
    /// must be addressed to. Kept so an answer naming an id the agent never
    /// asked about is dropped rather than written, and so a cancel can
    /// settle every one of them.
    open_permissions: HashMap<u64, RequestId>,
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
        requires_auth: bool,
    ) -> Self {
        Self {
            shared,
            out,
            cwd,
            next_wire_id: 1,
            next_boundary_id: 1,
            outstanding: HashMap::new(),
            requires_auth,
            auth_methods: Vec::new(),
            auth_attempted: false,
            session_id: None,
            open_permissions: HashMap::new(),
            tool_calls: HashMap::new(),
            deferred: Vec::new(),
        }
    }

    /// Opens the handshake.
    fn begin(&mut self) {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                // both false: this transport already answers fs/read_text_file
                // and fs/write_text_file at the wire level, but nothing yet
                // routes the resulting event through a trust prompt or
                // nvim's buffer truth, so a well-behaved agent that trusted
                // a true flag here could wait on an answer no other part of
                // the running editor provides yet. Advertising false is
                // truthful about the whole path, not just this file's part
                // of it, and a fallback to the agent's own direct-disk
                // access is the same "capability absent" behavior every ACP
                // agent already has to support
                "fs": { "readTextFile": false, "writeTextFile": false },
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
        let id = RequestId::Num(self.next_wire_id);
        self.next_wire_id = self.next_wire_id.saturating_add(1);
        self.outstanding.insert(id.clone(), kind);
        let _ = self.out.send(JsonRpcMessage::request(id, method, params));
    }

    /// Sends `session/new`, from a fresh handshake or after `authenticate`
    /// has just succeeded -- both paths request the same session, so both
    /// go through the one place that builds it.
    fn begin_session_new(&mut self) {
        // the same directory the agent process itself was started in: a
        // session created against a different root would have the agent
        // resolving relative paths one way and reading files another
        let cwd = self.cwd.to_string_lossy().into_owned();
        self.request(
            "session/new",
            json!({ "cwd": cwd, "mcpServers": [] }),
            Outstanding::NewSession,
        );
    }

    /// Handles a JSON-RPC error answer to a request this client sent.
    ///
    /// `session/new` failing with `auth_required` is the one case that is
    /// not terminal: the wire's own signal to call `authenticate` and try
    /// again, gated on the adapter actually requiring it so an agent that
    /// merely advertises optional methods is never made to authenticate
    /// against its own wishes, and on `auth_attempted` so a `session/new`
    /// that still fails after a successful `authenticate` is reported
    /// rather than retried forever.
    fn on_error_response(&mut self, kind: Outstanding, error: JsonRpcError) {
        if matches!(kind, Outstanding::NewSession)
            && error.code == AUTH_REQUIRED
            && self.requires_auth
            && !self.auth_attempted
        {
            if let Some(method_id) = self.auth_methods.first().cloned() {
                self.auth_attempted = true;
                self.request(
                    "authenticate",
                    json!({ "methodId": method_id }),
                    Outstanding::Authenticate,
                );
                return;
            }
        }
        match kind {
            // a refused handshake, authentication, or session creation
            // leaves no session to work in at all, which is the same dead
            // end as a dead agent
            Outstanding::Initialize | Outstanding::Authenticate | Outstanding::NewSession => {
                self.shared.emit(AiEvent::SessionCrashed {
                    message: format!("the agent refused {}: {}", method_of(kind), error.message),
                });
            }
            // a refused turn leaves the session usable, so it is reported
            // as what it is: the agent's own words about the turn, and
            // then the turn ending
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
    }

    /// The next id to put on an event crossing into the closed vocabulary.
    fn next_boundary_id(&mut self) -> u64 {
        let id = self.next_boundary_id;
        self.next_boundary_id = self.next_boundary_id.saturating_add(1);
        id
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

    fn on_response(&mut self, id: RequestId, outcome: Result<Value, JsonRpcError>) {
        let Some(kind) = self.outstanding.remove(&id) else {
            return;
        };
        let result = match outcome {
            Ok(result) => result,
            Err(error) => {
                self.on_error_response(kind, error);
                return;
            }
        };
        match kind {
            Outstanding::Initialize => {
                // the version is only bumped for breaking changes, so an
                // agent answering with anything else is saying that
                // everything after this handshake is a different protocol.
                // Speaking v1 at it anyway would turn one legible refusal
                // into a stream of decode failures blamed on the agent.
                let agreed = result.get("protocolVersion").and_then(Value::as_i64);
                if agreed != Some(PROTOCOL_VERSION) {
                    let spoken = match agreed {
                        Some(version) => version.to_string(),
                        None => "no version at all".to_string(),
                    };
                    self.shared.emit(AiEvent::SessionCrashed {
                        message: format!(
                            "the agent speaks protocol version {spoken}, view speaks {PROTOCOL_VERSION}"
                        ),
                    });
                    return;
                }
                self.auth_methods = result
                    .get("authMethods")
                    .and_then(Value::as_array)
                    .map(|methods| {
                        methods
                            .iter()
                            .filter_map(|m| m.get("id").and_then(Value::as_str))
                            .map(ToString::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                self.begin_session_new();
            }
            Outstanding::Authenticate => self.begin_session_new(),
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

    fn on_request(&mut self, id: RequestId, method: &str, params: &Value) {
        match method {
            "session/request_permission" => self.on_permission_request(id, params),
            // `fs/read_text_file` and `fs/write_text_file` fall to the
            // METHOD_NOT_FOUND arm below on purpose: `begin`'s advertised
            // `clientCapabilities.fs` is false, and a client that dispatched
            // these anyway would answer a call it told the agent it cannot
            // handle. A conforming agent never sends them while the
            // capability is false; the arm exists for the one that does.
            _ => {
                let _ = self.out.send(JsonRpcMessage::error_response(
                    id,
                    METHOD_NOT_FOUND,
                    &format!("view does not implement {method}"),
                ));
            }
        }
    }

    fn on_permission_request(&mut self, id: RequestId, params: &Value) {
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
        // the agent's own wire id stays here; what crosses is a view id
        // from the one counter every boundary id comes from
        let request_id = self.next_boundary_id();
        self.open_permissions.insert(request_id, id);
        self.shared.emit(AiEvent::PermissionRequested {
            request_id,
            tool_call_id,
            options,
        });
    }

    /// The `fs/read_text_file` handler body, kept live and directly tested
    /// even though `on_request` no longer dispatches to it: the wire route
    /// is closed while `clientCapabilities.fs.readTextFile` is false, but
    /// the body itself -- registering the reply and emitting the event --
    /// is exactly what the dispatch arm re-attaches to once that capability
    /// is advertised true again, and re-deriving it at that point would be
    /// the one place a hand-written duplicate could silently drift.
    // this allow is the only thing keeping the whole retained fs reply path
    // compiling: PendingFsReplies::register, spawn_fs_reply, fs_reason, and
    // the REQUEST_CANCELLED/INTERNAL_ERROR wire constants are reachable from
    // nowhere else, so rustc's dead-code walk needs this call site treated
    // as live to see any of them as used
    #[allow(dead_code)]
    fn on_fs_read(&mut self, id: RequestId, params: &Value) {
        let path = std::path::PathBuf::from(
            params
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        let (tx, rx) = oneshot::channel();
        let request_id = self.next_boundary_id();
        self.shared
            .pending()
            .register(request_id, PendingReply::Read(tx));
        spawn_fs_reply(
            self.out.clone(),
            id,
            rx,
            |content| json!({ "content": content }),
        );
        self.shared
            .emit(AiEvent::FsReadRequested { request_id, path });
    }

    /// The `fs/write_text_file` handler body -- see [`Self::on_fs_read`]'s
    /// doc comment for why it stays live and directly tested with no
    /// dispatch arm reaching it.
    // this allow keeps the write leg of the same retained fs reply path
    // compiling: PendingFsReplies::register (the Write variant),
    // spawn_fs_reply, fs_reason, and the REQUEST_CANCELLED/INTERNAL_ERROR
    // wire constants are the same shared machinery on_fs_read's allow keeps
    // live for the read leg, reachable from nowhere but these two call sites
    #[allow(dead_code)]
    fn on_fs_write(&mut self, id: RequestId, params: &Value) {
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
        let request_id = self.next_boundary_id();
        self.shared
            .pending()
            .register(request_id, PendingReply::Write(tx));
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
                if let Some(wire_id) = self.open_permissions.remove(&request_id) {
                    let _ = self.out.send(JsonRpcMessage::response(
                        wire_id,
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
                for wire_id in std::mem::take(&mut self.open_permissions).into_values() {
                    let _ = self.out.send(JsonRpcMessage::response(
                        wire_id,
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
    id: RequestId,
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
                JsonRpcMessage::error_response(id.clone(), INTERNAL_ERROR, &fs_reason(&error))
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
        Outstanding::Authenticate => "authenticate",
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
///
/// The engine-read blocks (`CurrentBuffer` through `QuickfixList`) all
/// lower to `"text"`: the ACP schema's own `resource`/`image`/`audio`
/// content kinds are not pinned anywhere in this crate's wire capture, and
/// guessing their field shapes would risk a payload the agent's decoder
/// rejects outright, which is worse than the plainer text rendering below.
fn context_block(block: &ContextBlock) -> Option<Value> {
    match block {
        ContextBlock::Text { text } => Some(json!({ "type": "text", "text": text })),
        ContextBlock::ResourceLink { uri, name } => {
            Some(json!({ "type": "resource_link", "uri": uri, "name": name }))
        }
        ContextBlock::CurrentBuffer { path, text } => Some(json!({
            "type": "text",
            "text": format!("Current buffer: {}\n\n{text}", path.display()),
        })),
        ContextBlock::Selection { text, range } => Some(json!({
            "type": "text",
            "text": format!("Selected lines {}-{}:\n\n{text}", range.0, range.1),
        })),
        ContextBlock::Cursor { line, col } => Some(json!({
            "type": "text",
            "text": format!("Cursor at line {line}, column {col}"),
        })),
        // An entry-less list renders as a bare header the agent would read
        // as content; `assemble` never produces one, but this function takes
        // blocks from any producer and owes the same blank-message refusal.
        ContextBlock::Diagnostics { entries } if entries.is_empty() => None,
        ContextBlock::Diagnostics { entries } => {
            Some(json!({ "type": "text", "text": diagnostics_text(entries) }))
        }
        ContextBlock::QuickfixList { entries } if entries.is_empty() => None,
        ContextBlock::QuickfixList { entries } => {
            Some(json!({ "type": "text", "text": quickfix_text(entries) }))
        }
        _ => None,
    }
}

/// One line per entry: `line:col [severity] message`.
fn diagnostics_text(entries: &[DiagnosticEntry]) -> String {
    let mut out = String::from("Diagnostics:");
    for entry in entries {
        out.push_str(&format!(
            "\n{}:{} [{}] {}",
            entry.line,
            entry.col,
            severity_label(entry.severity),
            entry.message
        ));
    }
    out
}

/// Closed match over [`DiagnosticSeverity`]'s own four levels -- adding a
/// fifth there fails this compilation rather than this label silently
/// falling through to a misreported existing one.
fn severity_label(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::Error => "error",
        DiagnosticSeverity::Warning => "warning",
        DiagnosticSeverity::Info => "info",
        DiagnosticSeverity::Hint => "hint",
    }
}

/// One line per entry: `path:line:col text`.
fn quickfix_text(entries: &[QuickfixEntry]) -> String {
    let mut out = String::from("Quickfix list:");
    for entry in entries {
        out.push_str(&format!(
            "\n{}:{}:{} {}",
            entry.path.display(),
            entry.line,
            entry.col,
            entry.text
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::pin::Pin;
    use std::task::{Context, Poll};

    use view_core::native::ai_context::{
        CurrentBufferRead, CursorRead, EngineReadSnapshot, QuickfixEntry as QfEntry, SelectionRead,
    };

    use super::*;

    /// `context::assemble`'s output reaches `session/prompt`'s wire content
    /// array with every engine-read block kind mapped to something an agent
    /// can read, none of them silently dropped by `context_block`'s
    /// fallback arm.
    #[test]
    fn assembled_context_serializes_every_block_kind() {
        let reads = EngineReadSnapshot::default()
            .with_current_buffer(CurrentBufferRead::new(
                std::path::PathBuf::from("/tmp/a.rs"),
                "fn main() {}".to_string(),
            ))
            .with_selection(SelectionRead::new("main".to_string(), (0, 0)))
            .with_cursor(CursorRead::new(0, 3))
            .with_diagnostics(vec![DiagnosticEntry::new(
                1,
                2,
                DiagnosticSeverity::Error,
                "unresolved import".to_string(),
            )])
            .with_quickfix(vec![QfEntry::new(
                std::path::PathBuf::from("/tmp/a.rs"),
                5,
                0,
                "TODO".to_string(),
            )]);

        let context = crate::context::assemble(&reads);
        assert_eq!(context.len(), 5);

        let blocks: Vec<Value> = context.iter().filter_map(context_block).collect();
        assert_eq!(blocks.len(), 5, "no engine-read block should be dropped");
        for block in &blocks {
            assert_eq!(block["type"], "text");
        }
        assert!(blocks[3]["text"]
            .as_str()
            .expect("diagnostics block carries text")
            .contains("[error] unresolved import"));
    }

    /// Full-text equality over the rendered attachments: a garbled label,
    /// separator, or field order fails here, not in an agent's lap.
    #[test]
    fn diagnostic_and_quickfix_renderings_pin_the_exact_line_format() {
        let diagnostics = ContextBlock::Diagnostics {
            entries: vec![
                DiagnosticEntry::new(3, 1, DiagnosticSeverity::Warning, "unused var".to_string()),
                DiagnosticEntry::new(9, 4, DiagnosticSeverity::Hint, "inline this".to_string()),
                DiagnosticEntry::new(2, 0, DiagnosticSeverity::Info, "note".to_string()),
            ],
        };
        let block = context_block(&diagnostics).expect("non-empty diagnostics lower to text");
        assert_eq!(
            block["text"],
            "Diagnostics:\n3:1 [warning] unused var\n9:4 [hint] inline this\n2:0 [info] note"
        );

        let quickfix = ContextBlock::QuickfixList {
            entries: vec![QfEntry::new(
                std::path::PathBuf::from("/tmp/a.rs"),
                5,
                0,
                "TODO".to_string(),
            )],
        };
        let block = context_block(&quickfix).expect("non-empty quickfix lowers to text");
        assert_eq!(block["text"], "Quickfix list:\n/tmp/a.rs:5:0 TODO");
    }

    #[test]
    fn entry_less_diagnostic_and_quickfix_blocks_lower_to_no_content() {
        assert!(context_block(&ContextBlock::Diagnostics { entries: vec![] }).is_none());
        assert!(context_block(&ContextBlock::QuickfixList { entries: vec![] }).is_none());
    }

    /// A stdin that is already broken: every write fails, which is what an
    /// agent that closed its input looks like from this side.
    struct BrokenStdin;

    impl AsyncWrite for BrokenStdin {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the agent closed its stdin",
            )))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// The half-wedge: the agent's input is gone while its output is still
    /// open and silent, so the reader has nothing to notice and never will.
    /// Without the writer reporting for itself this loop runs forever and
    /// every prompt after it vanishes into a channel nobody drains.
    #[tokio::test]
    async fn a_broken_stdin_is_reported_even_though_the_reader_never_ends() {
        // held open for the whole test: dropping this end would EOF the
        // reader and let the reader-side path report instead
        let (_agent_stdout, client_side) = tokio::io::duplex(4096);
        let (_commands_tx, commands) = mpsc::unbounded_channel();
        let shared = Arc::new(SessionShared::detached(Box::new(|_| {})));

        let ending = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(
                JsonRpcCodec::new(client_side, BrokenStdin),
                commands,
                shared,
                std::env::temp_dir(),
                false,
            ),
        )
        .await
        .expect("the session reports a dead writer instead of running forever");

        let Some(SessionEnd::WriterFailed(reason)) = ending else {
            panic!("expected WriterFailed, got {ending:?}")
        };
        assert!(
            reason.contains("stdin"),
            "the reason names what broke: {reason}"
        );
    }

    /// The tripwire a later handler-landing change must flip, not delete:
    /// until a trust-gated `fs/*` handler exists end to end, the agent must
    /// be told these capabilities are absent, never advertised ahead of the
    /// handler that would back them.
    #[test]
    fn the_outgoing_initialize_advertises_fs_capabilities_as_false() {
        let shared = Arc::new(SessionShared::detached(Box::new(|_| {})));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let mut driver = Driver::new(shared, out_tx, std::env::temp_dir(), false);

        driver.begin();

        let frame = out_rx.try_recv().expect("initialize was sent");
        assert_eq!(frame.method.as_deref(), Some("initialize"));
        let params = frame.params.expect("initialize carries params");
        // the capture doc pins this as the JSON integer 1, not the string
        // "1" -- a string round-trips through every assertion in this suite
        // that only checks the decoded value, so the wire type is the part
        // worth pinning
        assert_eq!(params["protocolVersion"], 1);
        assert_eq!(params["clientCapabilities"]["fs"]["readTextFile"], false);
        assert_eq!(params["clientCapabilities"]["fs"]["writeTextFile"], false);
        assert_eq!(params["clientCapabilities"]["terminal"], false);
        assert_eq!(params["clientInfo"]["name"], "view");
        assert_eq!(params["clientInfo"]["version"], env!("CARGO_PKG_VERSION"));
    }

    /// Deleting the `return` after the version-mismatch `SessionCrashed`
    /// emit would let a client still speak `session/new` to an agent it just
    /// told the caller it refuses to talk to. Draining `out_rx` for a second
    /// frame is what a first-event-only assertion (checking only the emitted
    /// `AiEvent`) cannot catch.
    #[test]
    fn a_version_mismatch_never_reaches_session_new() {
        let shared = Arc::new(SessionShared::detached(Box::new(|_| {})));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let mut driver = Driver::new(shared, out_tx, std::env::temp_dir(), false);

        driver.begin();
        let _initialize = out_rx.try_recv().expect("initialize was sent");

        driver.on_response(RequestId::Num(1), Ok(json!({ "protocolVersion": 2 })));

        assert!(
            out_rx.try_recv().is_err(),
            "no further request after a version refusal"
        );
    }

    fn detached_driver(requires_auth: bool) -> (Driver, mpsc::UnboundedReceiver<JsonRpcMessage>) {
        let shared = Arc::new(SessionShared::detached(Box::new(|_| {})));
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        (
            Driver::new(shared, out_tx, std::env::temp_dir(), requires_auth),
            out_rx,
        )
    }

    /// Drives a real `begin()`/`initialize`-response round trip rather than
    /// inventing a wire id: `next_wire_id` is single-counter state shared
    /// with every other request the driver sends, so a hand-picked id can
    /// silently collide with the one `begin_session_new` assigns next and
    /// mask exactly the ordering bug these tests exist to catch.
    fn initialize_with_auth_methods(
        driver: &mut Driver,
        out_rx: &mut mpsc::UnboundedReceiver<JsonRpcMessage>,
        methods: &[&str],
    ) {
        driver.begin();
        let initialize = out_rx.try_recv().expect("initialize was sent");
        let ids: Vec<Value> = methods
            .iter()
            .map(|id| json!({ "id": id, "name": id }))
            .collect();
        driver.on_response(
            initialize.id.expect("initialize carries an id"),
            Ok(json!({ "protocolVersion": PROTOCOL_VERSION, "authMethods": ids })),
        );
    }

    fn auth_required_error() -> JsonRpcError {
        JsonRpcError {
            code: AUTH_REQUIRED,
            message: "authentication required".to_string(),
            data: None,
        }
    }

    /// `requires_auth: false` must never call `authenticate`, even against
    /// an agent that offers methods and refuses `session/new` with the
    /// wire's own `auth_required` code.
    #[test]
    fn auth_required_is_not_retried_when_the_adapter_does_not_require_it() {
        let (mut driver, mut out_rx) = detached_driver(false);
        initialize_with_auth_methods(&mut driver, &mut out_rx, &["stub-login"]);
        let session_new = out_rx.try_recv().expect("session/new was sent");

        driver.on_response(
            session_new.id.expect("session/new carries an id"),
            Err(auth_required_error()),
        );

        assert!(
            out_rx.try_recv().is_err(),
            "no authenticate frame without requires_auth"
        );
    }

    /// A `session/new` that still fails with `auth_required` after a
    /// successful `authenticate` must not be retried a second time --
    /// exactly one `authenticate` frame, ever, per session.
    #[test]
    fn a_second_auth_required_after_authenticating_is_not_retried_again() {
        let (mut driver, mut out_rx) = detached_driver(true);
        initialize_with_auth_methods(&mut driver, &mut out_rx, &["stub-login"]);
        let first_session_new = out_rx.try_recv().expect("session/new was sent");

        driver.on_response(
            first_session_new.id.expect("session/new carries an id"),
            Err(auth_required_error()),
        );
        let authenticate = out_rx.try_recv().expect("authenticate was sent");
        assert_eq!(authenticate.method.as_deref(), Some("authenticate"));

        driver.on_response(
            authenticate.id.expect("authenticate carries an id"),
            Ok(json!({})),
        );
        let second_session_new = out_rx.try_recv().expect("session/new was retried");

        driver.on_response(
            second_session_new.id.expect("session/new carries an id"),
            Err(auth_required_error()),
        );

        assert!(
            out_rx.try_recv().is_err(),
            "no second authenticate frame after one has already been sent"
        );
    }

    /// An agent that fails `session/new` with `auth_required` but offered no
    /// `authMethods` at all gives the client nothing to authenticate with,
    /// so the retry path must not fire.
    #[test]
    fn auth_required_with_no_advertised_methods_is_not_retried() {
        let (mut driver, mut out_rx) = detached_driver(true);
        initialize_with_auth_methods(&mut driver, &mut out_rx, &[]);
        let session_new = out_rx.try_recv().expect("session/new was sent");

        driver.on_response(
            session_new.id.expect("session/new carries an id"),
            Err(auth_required_error()),
        );

        assert!(
            out_rx.try_recv().is_err(),
            "no authenticate frame with no methods to authenticate with"
        );
    }

    /// The wire route is closed while the `fs` capability is advertised
    /// false, so both methods fall to the same `METHOD_NOT_FOUND` arm every
    /// other unimplemented method does.
    #[test]
    fn fs_read_and_write_answer_method_not_found_over_the_wire() {
        let (mut driver, mut out_rx) = detached_driver(false);

        driver.on_request(
            RequestId::Str("read-1".to_string()),
            "fs/read_text_file",
            &json!({ "path": "/stub/a.rs" }),
        );
        driver.on_request(
            RequestId::Str("write-1".to_string()),
            "fs/write_text_file",
            &json!({ "path": "/stub/a.rs", "content": "fn main() {}" }),
        );

        for expected_id in ["read-1", "write-1"] {
            let frame = out_rx.try_recv().expect("an error response was sent");
            assert_eq!(frame.id, Some(RequestId::Str(expected_id.to_string())));
            let error = frame.error.expect("the response carries an error");
            assert_eq!(error.code, METHOD_NOT_FOUND);
        }
    }

    /// The `fs/read_text_file` handler body, driven directly rather than
    /// through `on_request`: proves `on_fs_read` still registers a reply and
    /// answers the wire correctly, independent of whether any dispatch arm
    /// currently reaches it.
    #[tokio::test]
    async fn on_fs_read_registers_a_reply_and_answers_the_wire_when_it_arrives() {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(SessionShared::detached(Box::new(move |msg| {
            let _ = events_tx.send(msg);
        })));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let mut driver = Driver::new(shared, out_tx, std::env::temp_dir(), false);

        driver.on_fs_read(
            RequestId::Str("read-1".to_string()),
            &json!({ "path": "/stub/a.rs" }),
        );

        let view_core::msg::Msg::Ai(AiEvent::FsReadRequested { request_id, path }) =
            events_rx.recv().expect("FsReadRequested was emitted")
        else {
            panic!("expected FsReadRequested")
        };
        assert_eq!(path, std::path::PathBuf::from("/stub/a.rs"));

        driver.on_command(AiCommand::FsReadReply {
            request_id,
            result: Ok("fn main() {}".to_string()),
        });

        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
            .await
            .expect("the reply was written")
            .expect("the channel stayed open");
        assert_eq!(frame.id, Some(RequestId::Str("read-1".to_string())));
        assert_eq!(frame.result, Some(json!({ "content": "fn main() {}" })));
    }

    /// The `fs/write_text_file` handler body's failure leg: a refused write
    /// must answer the wire with an error, not silently drop the request.
    #[tokio::test]
    async fn on_fs_write_registers_a_reply_and_answers_a_refusal_over_the_wire() {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(SessionShared::detached(Box::new(move |msg| {
            let _ = events_tx.send(msg);
        })));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let mut driver = Driver::new(shared, out_tx, std::env::temp_dir(), false);

        driver.on_fs_write(
            RequestId::Str("write-1".to_string()),
            &json!({ "path": "/stub/a.rs", "content": "fn main() {}" }),
        );

        let view_core::msg::Msg::Ai(AiEvent::FsWriteRequested {
            request_id,
            path,
            content,
        }) = events_rx.recv().expect("FsWriteRequested was emitted")
        else {
            panic!("expected FsWriteRequested")
        };
        assert_eq!(path, std::path::PathBuf::from("/stub/a.rs"));
        assert_eq!(content, "fn main() {}");

        driver.on_command(AiCommand::FsWriteReply {
            request_id,
            result: Err(FsError::PermissionDenied),
        });

        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
            .await
            .expect("the reply was written")
            .expect("the channel stayed open");
        assert_eq!(frame.id, Some(RequestId::Str("write-1".to_string())));
        let error = frame.error.expect("a refused write answers with an error");
        assert_eq!(error.code, INTERNAL_ERROR);
        assert_eq!(error.message, fs_reason(&FsError::PermissionDenied));
    }
}
