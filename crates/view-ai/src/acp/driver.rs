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
    AiCommand, AiEvent, ContextBlock, Cost, FsError, PermissionOption, PlanEntry,
    PlanEntryPriority, PlanEntryStatus, StopReason, ToolCallStatus,
};

use crate::acp::fs::PendingReply;
use crate::acp::permission::{permission_option, permission_outcome};
use crate::acp::session::{ChildSlot, SessionShared};
use crate::acp::wire::{
    Incoming, JsonRpcCodec, JsonRpcError, JsonRpcMessage, RequestId, AUTH_REQUIRED, INTERNAL_ERROR,
    INVALID_PARAMS, METHOD_NOT_FOUND, REQUEST_CANCELLED, RESOURCE_NOT_FOUND,
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
                Some(Err(end)) => {
                    driver.cancel_open_permissions();
                    return Some(end);
                }
                // both halves are gone without either having said why, which
                // only happens if a task was dropped out from under the loop
                None => {
                    driver.cancel_open_permissions();
                    return Some(SessionEnd::ReaderEof);
                }
            },
            command = commands.recv() => match command {
                Some(command) => driver.on_command(command),
                None => return None,
            },
        }
    }
}

/// One tool call's last-known title and status, held by
/// [`Driver::tool_calls`] to fill in whatever a later `tool_call_update`
/// omits. Carries no `content`: the wire's own omission semantics for
/// result content are carried across the boundary as
/// [`AiEvent::ToolCallUpdate`]'s `content: Option<Vec<String>>` instead of
/// being flattened into a remembered copy here, so there is nothing about
/// a call's result for the driver to remember or bound.
#[derive(Clone)]
struct KnownToolCall {
    title: String,
    status: ToolCallStatus,
    /// The proposals already raised on this call, as
    /// `(request_id, fingerprint)`. A `tool_call_update` restates the whole
    /// `content` array, so without the fingerprint the same file
    /// modification would open a review on every update; a genuinely
    /// revised diff hashes differently and still proposes. The request id
    /// is what [`AiCommand::DiscardProposal`] names to drop an entry again,
    /// so a proposal the panel could not take stops being deduplicated
    /// against.
    proposed: Vec<(u64, u64)>,
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
    /// The last title and status seen per tool call -- never `content`
    /// (see [`KnownToolCall`]'s doc for why). A `tool_call_update` carries
    /// only what changed, while the event vocabulary carries a whole
    /// call's title and status, so the fields the agent omitted come from
    /// here rather than from a placeholder the panel would display. Never
    /// removed once a call reaches [`ToolCallStatus::Completed`] or
    /// [`ToolCallStatus::Failed`]: the capture doc permits (does not
    /// forbid) a further `tool_call_update` after a terminal status, and
    /// dropping the entry entirely would make that update fall back to an
    /// empty title and `Pending`, regressing a call the panel already
    /// rendered as finished.
    tool_calls: HashMap<String, KnownToolCall>,
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
                // both true, and the whole path behind them is what makes
                // that truthful: on_request dispatches these two methods,
                // every request is refused unless it falls inside the
                // session's own cwd, and the answer comes from the nvim
                // buffer rather than from disk -- so an agent reading a file
                // the user is editing sees the unsaved edits, which is
                // exactly what the capability promises and what an agent's
                // own direct-disk fallback cannot do
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
                // defensive: a conformant agent cannot refuse a prompt while
                // still holding a permission request on it open, but a turn
                // ending is exactly the point past which no reply would ever
                // be sent otherwise
                self.cancel_open_permissions();
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
                // defensive: see the same call in on_error_response's Prompt
                // arm -- a turn ending is the point past which no reply
                // would ever be sent otherwise
                self.cancel_open_permissions();
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
            "plan" => self.emit_plan(update),
            "usage_update" => self.emit_usage_update(update),
            // Deliberate no-ops, each one a surface the closed vocabulary
            // does not model: an agent's slash-command list, its mode, its
            // config options, and its session metadata. Decoded and dropped
            // rather than left to the catch-all below, so an addition to
            // the discriminant list is still distinguishable from a surface
            // that was weighed and left out.
            "available_commands_update"
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
            .or_else(|| known.map(|k| k.title.clone()))
            .unwrap_or_default();
        let status = update
            .get("status")
            .and_then(Value::as_str)
            .and_then(tool_call_status_from_wire)
            .or_else(|| known.map(|k| k.status))
            .unwrap_or(ToolCallStatus::Pending);
        // `content` carries the wire's own omission semantics across the
        // boundary as `Option`, rather than resolving "omitted" against a
        // remembered copy the way `title`/`status` do above: `None` here
        // means "this update said nothing about the result", and it is
        // left to `Transcript::upsert_tool_call` (the one place that still
        // holds the call's prior result) to leave that result untouched.
        // An explicit JSON `null` is omission too, not a present-but-empty
        // value -- the same distinction `emit_usage_update` draws for
        // `cost`.
        let content = update
            .get("content")
            .is_some_and(|c| !c.is_null())
            .then(|| tool_call_content(update));
        let mut proposed = known.map(|k| k.proposed.clone()).unwrap_or_default();
        self.shared.emit(AiEvent::ToolCallUpdate {
            tool_call_id: tool_call_id.to_string(),
            title: title.clone(),
            status,
            content,
        });
        // The diff items of the same `content` array, raised as proposals
        // the user decides on rather than left as the placeholder row
        // `tool_call_content_item` renders for them. The transcript entry
        // above is emitted first so the call the proposal belongs to is
        // already on screen when the review opens over it.
        for proposal in diff_proposals(update) {
            let fingerprint = fingerprint(&proposal);
            if proposed.iter().any(|(_, seen)| *seen == fingerprint) {
                continue;
            }
            let request_id = self.next_boundary_id();
            proposed.push((request_id, fingerprint));
            self.shared.emit(AiEvent::DiffProposed {
                request_id,
                path: proposal.path,
                old_text: proposal.old_text,
                new_text: proposal.new_text,
            });
        }
        self.tool_calls.insert(
            tool_call_id.to_string(),
            KnownToolCall {
                title,
                status,
                proposed,
            },
        );
    }

    fn emit_plan(&self, update: &Value) {
        let Some(entries) = update.get("entries").and_then(Value::as_array) else {
            return;
        };
        let entries = entries.iter().map(plan_entry).collect();
        self.shared.emit(AiEvent::PlanUpdated { entries });
    }

    fn emit_usage_update(&self, update: &Value) {
        let Some(used) = update.get("used").and_then(Value::as_u64) else {
            return;
        };
        let Some(size) = update.get("size").and_then(Value::as_u64) else {
            return;
        };
        let cost = update
            .get("cost")
            .filter(|c| !c.is_null())
            .and_then(cost_from_wire);
        self.shared.emit(AiEvent::UsageUpdated { used, size, cost });
    }

    fn on_request(&mut self, id: RequestId, method: &str, params: &Value) {
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

    fn on_permission_request(&mut self, id: RequestId, params: &Value) {
        let tool_call = params.get("toolCall");
        let tool_call_id = tool_call
            .and_then(|call| call.get("toolCallId"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // `ToolCallUpdate.title` is optional on the wire (only `toolCallId`
        // is required); the id is what the panel falls back to displaying
        // when an agent omits it
        let title = tool_call
            .and_then(|call| call.get("title"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
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
            title,
            options,
        });
    }

    /// Settles every `session/request_permission` this client is still
    /// holding a reply open for, each with the wire's own `cancelled`
    /// outcome. Called both by an explicit `AiCommand::Cancel` and by every
    /// path that ends the session (a crash, a turn ending on its own) so a
    /// reply the agent is blocked on is never left unanswered once nothing
    /// still intends to answer it.
    fn cancel_open_permissions(&mut self) {
        for wire_id in std::mem::take(&mut self.open_permissions).into_values() {
            let _ = self.out.send(JsonRpcMessage::response(
                wire_id,
                json!({ "outcome": { "outcome": "cancelled" } }),
            ));
        }
    }

    /// Answers `fs/read_text_file` from nvim's buffer truth, refusing any
    /// path outside the session's own cwd.
    fn on_fs_read(&mut self, id: RequestId, params: &Value) {
        let Some(path) = self.contained_path(&id, params) else {
            return;
        };
        let line = fs_window_field(params, "line");
        let limit = fs_window_field(params, "limit");
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
        self.shared.emit(AiEvent::FsReadRequested {
            request_id,
            path,
            line,
            limit,
        });
    }

    /// Answers `fs/write_text_file` through nvim's buffer, refusing any path
    /// outside the session's own cwd on the identical terms
    /// [`Self::on_fs_read`] refuses one.
    fn on_fs_write(&mut self, id: RequestId, params: &Value) {
        let Some(path) = self.contained_path(&id, params) else {
            return;
        };
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

    /// Resolves `params.path` against the session's cwd, answering the wire
    /// itself and returning `None` when the path is not inside it.
    ///
    /// The first thing either filesystem handler does, before a boundary id
    /// is allocated, before a reply is registered, and before anything is
    /// emitted: a refused request must leave no trace anywhere in the
    /// editor, or a path the user never consented to would still show up in
    /// the panel as a request that happened. Read and write run the exact
    /// same check -- a client that advertises safe file access must not let
    /// a read reach further than a write may.
    ///
    /// The refusal carries no `content` member, only a JSON-RPC error: the
    /// pinned `ReadTextFileResponse` requires `content`, so answering a
    /// refusal with an empty string would be indistinguishable from an empty
    /// file and would have the agent act on text the file does not hold.
    fn contained_path(&self, id: &RequestId, params: &Value) -> Option<std::path::PathBuf> {
        let path = std::path::PathBuf::from(
            params
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        // A synchronous walk on the task that drains the agent's stdout,
        // deliberately: it is what resolves symlinks, and a containment
        // check that ran anywhere else would decide against a path that had
        // already moved. The cost is bounded to `canonicalize(cwd)` plus
        // one `lstat` per component of the candidate that still exists --
        // no directory listing, no recursion, no I/O proportional to the
        // project. `spawn_blocking` would trade that bound for a scheduling
        // hop on every filesystem request and, worse, would put the gate
        // after the point where the handler has already committed to
        // answering.
        match crate::trust::path_is_contained(&self.cwd, &path) {
            Ok(Some(resolved)) => Some(resolved),
            // an unresolvable path is refused with the same code as one
            // that resolved outside: which of the two it was is a detail of
            // this client's filesystem that an agent must not be able to
            // probe by watching the error code change
            Ok(None) | Err(_) => {
                let _ = self.out.send(JsonRpcMessage::error_response(
                    id.clone(),
                    INVALID_PARAMS,
                    "view answers filesystem requests only for paths inside the session directory",
                ));
                None
            }
        }
    }

    /// Drops one raised proposal from the call that raised it, so the same
    /// diff restated later proposes again. Called for a proposal the panel
    /// refused to take: the user never saw it, so the session must not go
    /// on treating it as already shown.
    fn forget_proposal(&mut self, request_id: u64) {
        for known in self.tool_calls.values_mut() {
            known.proposed.retain(|(id, _)| *id != request_id);
        }
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
                // Correlated inside this crate against state that exists
                // before any session does, the same as the filesystem
                // answers above: deferring it would leave the discarded
                // proposal deduplicating restatements in the meantime,
                // which is the whole thing it exists to stop.
                AiCommand::DiscardProposal { request_id } => self.forget_proposal(request_id),
                other => self.deferred.push(other),
            }
            return;
        };
        match command {
            AiCommand::DiscardProposal { request_id } => self.forget_proposal(request_id),
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
                self.cancel_open_permissions();
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

/// Reads `line` or `limit` off a `fs/read_text_file` request.
///
/// Both are nullable in the pinned schema, and an agent that wants the whole
/// file may either omit the member or send an explicit `null` -- absent and
/// null therefore have to mean the same thing here, which is what
/// `Value::as_u64` on a missing key already gives. The schema's `minimum: 0`
/// makes a negative value out of contract, so one is treated as no window at
/// all rather than saturated into a window the agent never asked for.
fn fs_window_field(params: &Value, key: &str) -> Option<u32> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| u32::try_from(value).unwrap_or(u32::MAX))
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
                JsonRpcMessage::error_response(id.clone(), fs_code(&error), &fs_reason(&error))
            }
            // the sender was dropped without an answer: the editor is going
            // away, which for the agent is the same as its request being
            // cancelled
            Err(_) => JsonRpcMessage::error_response(id, REQUEST_CANCELLED, "cancelled"),
        };
        let _ = out.send(frame);
    });
}

/// The wire code one filesystem failure answers with.
///
/// An agent's control flow keys on the code, not on the prose beside it, so
/// a failure the agent can act on differently has to arrive with a
/// different code (`docs/acp-v1-wire-capture.md`, `fs/read_text_file`
/// case 6). `NotFound` is the pinned table's own "a given resource, such as
/// a file, was not found" -- an agent that reads it stops asking, where
/// `INTERNAL_ERROR` would tell it this client malfunctioned and invite it
/// to retry a call that can never succeed.
///
/// Everything else stays `INTERNAL_ERROR`, the table's
/// "implementation-defined server errors": a moved `changedtick` and an
/// unwritable target are both conditions on this client's side of the call,
/// and what separates them is the message. A refusal at the trust boundary
/// never reaches here at all -- it is answered `INVALID_PARAMS` before a
/// reply is ever registered, so its code cannot vary with whether the
/// refused path exists.
///
/// The wildcard is compelled by the type, not chosen: `FsError` is
/// `#[non_exhaustive]` and defined in another crate, so an exhaustive
/// match is impossible here. A new variant therefore lands on
/// `INTERNAL_ERROR` silently; `view_core`'s
/// `fs_error_variants_are_matched_without_a_wildcard` is the build break
/// that should bring its author back to this mapping.
fn fs_code(error: &FsError) -> i64 {
    match error {
        FsError::NotFound => RESOURCE_NOT_FOUND,
        _ => INTERNAL_ERROR,
    }
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

/// A tool call's whole `content` array, each item decoded to a display
/// string via [`tool_call_content_item`]. Empty (not dropped) when the
/// update carries no `content` array at all.
fn tool_call_content(update: &Value) -> Vec<String> {
    update
        .get("content")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(tool_call_content_item).collect())
        .unwrap_or_default()
}

/// One `ToolCallContent` item, decoded per `docs/acp-v1-wire-capture.md`'s
/// `Content`/`Terminal` pin. A `"content"`-typed item wrapping a `"text"`
/// `ContentBlock` becomes its own text; every other `ContentBlock` kind
/// (`image`/`audio`/`resource_link`/`resource`), and `ToolCallContent`'s own
/// `"diff"`/`"terminal"` variants, become `"[<kind> content]"` -- a labeled
/// placeholder rather than a dropped item, since a client that saw content
/// arrive and showed nothing for it looks like the call produced no output.
/// Never guesses at unpinned field names for the placeholder kinds: it only
/// ever reads the `type` discriminant already pinned for each of them.
fn tool_call_content_item(item: &Value) -> String {
    let Some(kind) = item.get("type").and_then(Value::as_str) else {
        return "[content]".to_string();
    };
    if kind != "content" {
        return format!("[{kind} content]");
    }
    let block = item.get("content");
    let block_kind = block.and_then(|c| c.get("type")).and_then(Value::as_str);
    if block_kind == Some("text") {
        if let Some(text) = block.and_then(|c| c.get("text")).and_then(Value::as_str) {
            return text.to_string();
        }
    }
    format!("[{} content]", block_kind.unwrap_or("content"))
}

/// One `ToolCallContent` `"diff"` item, decoded per
/// `docs/acp-v1-wire-capture.md`'s pinned shape: `path` and `newText` are
/// required, `oldText` is nullable and absent for a file that does not
/// exist yet.
struct DiffProposal {
    path: std::path::PathBuf,
    old_text: Option<String>,
    new_text: String,
}

/// Whether a proposal's `path` is one the wire actually promised.
/// `docs/acp-v1-wire-capture.md`'s `Diff` schema documents `path` as "The
/// absolute file path being modified," and the spellings refused here are
/// the ones that resolve to a buffer nobody meant:
///
/// - not absolute -- resolved against nvim's cwd, which the user moves with
///   `:cd` and view's own process never observes
///   (`docs/hidden-buffer-wire-capture.md` case 19). A blank path is
///   refused by this same test (nothing blank is absolute) and is the worst
///   of the class: nvim resolves an empty name onto its own `[No Name]`
///   scratch buffer, which a review would attach to and write its hunks
///   into (case 17);
/// - ending in `/` or `\` -- a directory's spelling, which nvim binds to a
///   *second* buffer over the same file (case 18). Both characters count on
///   every platform: the engine and its Lua both refuse the pair
///   unconditionally, and this string crossed a process boundary from an
///   agent whose host need not be view's (case 21).
///
/// Refused here, at the boundary the agent's text crosses, rather than
/// normalized into something plausible: a proposal names a file the user is
/// about to accept edits into, and guessing which file an off-contract
/// spelling meant is exactly the guess that must not be made.
///
/// Deliberately not [`crate::trust::path_is_contained`], which every `fs/*`
/// request must pass: a proposal edits nothing on its own. It opens a review
/// naming the file on screen, and no byte reaches any buffer until the user
/// accepts a hunk, so containment is enforced by a person reading the path
/// rather than by the workspace root. An `fs/*` call has no such reader --
/// it is the agent acting directly -- which is why that path takes the
/// stricter test and this one does not.
fn usable_path(path: &&str) -> bool {
    !path.ends_with(['/', '\\']) && std::path::Path::new(path).is_absolute()
}

/// Every decodable `"diff"` item of one tool call's content, in wire
/// order. An item missing a required field, or naming a path the wire's own
/// absolute-path contract rules out (see [`usable_path`]), is skipped
/// rather than degraded to a placeholder the way rendered content is: a
/// proposal is something the user writes into a buffer, and there is no
/// partial version of it that is safe to offer.
fn diff_proposals(update: &Value) -> Vec<DiffProposal> {
    update
        .get("content")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(diff_proposal).collect())
        .unwrap_or_default()
}

fn diff_proposal(item: &Value) -> Option<DiffProposal> {
    if item.get("type").and_then(Value::as_str) != Some("diff") {
        return None;
    }
    let path = item
        .get("path")
        .and_then(Value::as_str)
        .filter(usable_path)?;
    let new_text = item.get("newText").and_then(Value::as_str)?;
    Some(DiffProposal {
        path: std::path::PathBuf::from(path),
        old_text: item
            .get("oldText")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        new_text: new_text.to_string(),
    })
}

/// Identifies a proposal by content rather than by the whole text it
/// carries: a tool call restating an unchanged diff must not open a second
/// review, and remembering the file bodies to compare them would hold a
/// copy of every proposed file for the life of the session.
fn fingerprint(proposal: &DiffProposal) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    proposal.path.hash(&mut hasher);
    proposal.old_text.hash(&mut hasher);
    proposal.new_text.hash(&mut hasher);
    hasher.finish()
}

/// One `PlanEntry`, degraded to a labeled placeholder rather than dropped
/// when a required field is missing or its `priority`/`status` string is
/// not one the wire pins -- the same "never drop, always render something"
/// policy [`tool_call_content_item`]'s doc states for tool-call content,
/// applied here so a task the agent believes it announced never silently
/// vanishes from the rendered plan. `priority`/`status` are closed,
/// wire-pinned enums with no "unknown" member by design (see their own
/// docs), so an undecodable value falls back to the lowest-commitment
/// member of each (`Low`/`Pending`) rather than fabricate a variant the
/// wire never sends; the placeholder text says so instead of rendering as
/// if the fallback were what the agent reported.
fn plan_entry(raw: &Value) -> PlanEntry {
    let content = raw.get("content").and_then(Value::as_str);
    let priority = raw
        .get("priority")
        .and_then(Value::as_str)
        .and_then(|wire| match wire {
            "high" => Some(PlanEntryPriority::High),
            "medium" => Some(PlanEntryPriority::Medium),
            "low" => Some(PlanEntryPriority::Low),
            _ => None,
        });
    let status = raw
        .get("status")
        .and_then(Value::as_str)
        .and_then(|wire| match wire {
            "pending" => Some(PlanEntryStatus::Pending),
            "in_progress" => Some(PlanEntryStatus::InProgress),
            "completed" => Some(PlanEntryStatus::Completed),
            _ => None,
        });
    let readable = content.is_some() && priority.is_some() && status.is_some();
    let content = match (content, readable) {
        (Some(text), true) => text.to_string(),
        (Some(text), false) => format!("{text} [unreadable plan entry]"),
        (None, _) => "[unreadable plan entry]".to_string(),
    };
    PlanEntry {
        content,
        priority: priority.unwrap_or(PlanEntryPriority::Low),
        status: status.unwrap_or(PlanEntryStatus::Pending),
    }
}

/// A real currency code is three to eight characters; this is headroom,
/// not a format assumption.
const CURRENCY_CAP: usize = 16;

/// A `UsageUpdate`'s `cost` member, already filtered non-null by the
/// caller. `None` when `amount` or `currency` is missing -- an unreadable
/// cost is left out of the stats rather than shown as free.
///
/// `currency` is capped here, at decode, rather than at either place it is
/// later read: `UsageStats::render` runs once per painted frame while the
/// panel is open, and the vlog catch-all logs it verbatim. Capping once at
/// the wire boundary bounds both.
fn cost_from_wire(raw: &Value) -> Option<Cost> {
    let currency = raw.get("currency").and_then(Value::as_str)?;
    Some(Cost {
        amount: raw.get("amount").and_then(Value::as_f64)?,
        currency: currency.chars().take(CURRENCY_CAP).collect(),
    })
}

/// One attached context block as a wire content block, or `None` when the
/// vocabulary has grown a kind with no wire mapping here: an unmappable
/// attachment is left out of the turn rather than sent as empty text the
/// agent would read as a blank message.
///
/// Every block lowers to `"text"`: the ACP schema's own
/// `resource`/`resource_link`/`image`/`audio`
/// content kinds are not pinned anywhere in this crate's wire capture, and
/// guessing their field shapes would risk a payload the agent's decoder
/// rejects outright, which is worse than the plainer text rendering below.
fn context_block(block: &ContextBlock) -> Option<Value> {
    match block {
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
    use view_core::native::ai_event::PermissionOutcome;

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

    /// The same physical buffer position -- line 5, column 3 -- rendered
    /// through `Cursor`, `Diagnostics`, and `QuickfixList` must show the
    /// identical numbers in all three. `EngineReadSnapshot`'s three position
    /// sources are normalized onto one shared 1-indexed convention upstream,
    /// in `view-engine`'s own reply decoders (see that type's doc); this
    /// layer forwards whatever numbers it receives verbatim and must never
    /// itself add or subtract an index, so a physical position an agent
    /// reads from a cursor read, a diagnostic, and a quickfix entry all
    /// resolve to the same place rather than three positions offset from
    /// each other.
    #[test]
    fn cursor_diagnostic_and_quickfix_render_the_same_physical_position_identically() {
        let cursor_block = context_block(&ContextBlock::Cursor { line: 5, col: 3 })
            .expect("cursor lowers to text");
        assert_eq!(cursor_block["text"], "Cursor at line 5, column 3");

        let diagnostics = ContextBlock::Diagnostics {
            entries: vec![DiagnosticEntry::new(
                5,
                3,
                DiagnosticSeverity::Error,
                "boom".to_string(),
            )],
        };
        let diagnostics_block =
            context_block(&diagnostics).expect("non-empty diagnostics lower to text");
        assert_eq!(diagnostics_block["text"], "Diagnostics:\n5:3 [error] boom");

        let quickfix = ContextBlock::QuickfixList {
            entries: vec![QfEntry::new(
                std::path::PathBuf::from("/tmp/a.rs"),
                5,
                3,
                "boom".to_string(),
            )],
        };
        let quickfix_block = context_block(&quickfix).expect("non-empty quickfix lowers to text");
        assert_eq!(quickfix_block["text"], "Quickfix list:\n/tmp/a.rs:5:3 boom");
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

    /// `cancel_open_permissions` at both crash-return points in `drive`'s
    /// select loop was previously covered only by calling the method
    /// directly, which cannot tell apart a call that is load-bearing there
    /// from one that would compile and pass even if both call sites were
    /// deleted. This drives the real `drive()` select loop over two duplex
    /// pairs (one per direction, mirroring a child's independent stdin and
    /// stdout pipes), with a stand-in agent answering the handshake and
    /// opening a permission request before going silent, and asserts the
    /// cancelled reply is read back off the actual wire.
    #[tokio::test]
    async fn a_crash_while_a_permission_is_open_settles_it_cancelled_on_the_wire() {
        // agent -> client and client -> agent are independent pipes, exactly
        // like a real child's separate stdout and stdin: dropping the whole
        // `agent_out` end below must EOF only the client's reader, leaving
        // `client_out` free to still carry the cancelled reply
        let (agent_out, client_in) = tokio::io::duplex(4096);
        let (client_out, agent_in) = tokio::io::duplex(4096);

        let (mut agent_reader, mut agent_writer) = JsonRpcCodec::new(agent_in, agent_out).split();
        let (commands_tx, commands) = mpsc::unbounded_channel();
        let shared = Arc::new(SessionShared::detached(Box::new(|_| {})));

        let session = tokio::spawn(drive(
            JsonRpcCodec::new(client_in, client_out),
            commands,
            shared,
            std::env::temp_dir(),
            false,
        ));

        let initialize = agent_reader
            .next_message()
            .await
            .expect("read initialize")
            .expect("initialize arrives");
        let initialize_id = initialize.id.expect("initialize carries an id");
        agent_writer
            .write_message(&JsonRpcMessage::response(
                initialize_id,
                json!({ "protocolVersion": PROTOCOL_VERSION }),
            ))
            .await
            .expect("answer initialize");

        let new_session = agent_reader
            .next_message()
            .await
            .expect("read session/new")
            .expect("session/new arrives");
        let new_session_id = new_session.id.expect("session/new carries an id");
        agent_writer
            .write_message(&JsonRpcMessage::response(
                new_session_id,
                json!({ "sessionId": "sess_stand_in" }),
            ))
            .await
            .expect("answer session/new");

        // a permission request is normally raised mid-turn; sending the
        // prompt keeps this test's shape matching that reality even though
        // `on_permission_request` itself would process the request either way
        commands_tx
            .send(AiCommand::Prompt {
                text: "irrelevant".to_string(),
                context: Vec::new(),
            })
            .expect("the drive task is still alive");
        let _prompt = agent_reader
            .next_message()
            .await
            .expect("read session/prompt")
            .expect("session/prompt arrives");

        agent_writer
            .write_message(&JsonRpcMessage::request(
                RequestId::Str("perm-1".to_string()),
                "session/request_permission",
                json!({
                    "sessionId": "sess_stand_in",
                    "toolCall": { "toolCallId": "call_001" },
                    "options": [
                        { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                    ],
                }),
            ))
            .await
            .expect("send session/request_permission");

        // the crash: dropping the whole `agent_writer` (and its `agent_out`
        // stream) EOFs the client's reader without touching `client_out`,
        // the pipe the cancelled reply still needs to travel on
        drop(agent_writer);

        let ending = tokio::time::timeout(std::time::Duration::from_secs(10), session)
            .await
            .expect("drive() reports the crash instead of hanging")
            .expect("the drive task did not panic");
        assert!(
            matches!(ending, Some(SessionEnd::ReaderEof)),
            "expected ReaderEof, got {ending:?}"
        );

        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            agent_reader.next_message(),
        )
        .await
        .expect("the cancelled reply arrives instead of hanging")
        .expect("read the cancelled reply")
        .expect("the cancelled reply is a real frame, not eof");
        assert_eq!(reply.id, Some(RequestId::Str("perm-1".to_string())));
        assert_eq!(
            reply.result,
            Some(json!({ "outcome": { "outcome": "cancelled" } })),
            "the still-open permission request is settled cancelled on the real wire, \
             not only inside driver state"
        );
    }

    /// The capability advertisement and the handlers behind it must move
    /// together: this flipped to `true` in the same commit that gave
    /// `on_request` its two `fs/*` arms, and it must never read `true` while
    /// a `fs/*` request would go unanswered, nor `false` while the handlers
    /// stand -- either way the agent is told something the client does not
    /// do.
    #[test]
    fn the_outgoing_initialize_advertises_fs_capabilities_as_true() {
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
        assert_eq!(params["clientCapabilities"]["fs"]["readTextFile"], true);
        assert_eq!(params["clientCapabilities"]["fs"]["writeTextFile"], true);
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

    /// The next event the driver emitted, or a failure naming what was
    /// waited for.
    ///
    /// Bounded, never a bare `recv`: the sender lives as long as the
    /// `Driver` the test holds, so a driver that stopped emitting blocks
    /// forever rather than answering `RecvError`. A guard whose deletion
    /// wedges the suite for its whole timeout reports nothing at all --
    /// this turns that into a named failure.
    fn next_emitted(
        rx: &std::sync::mpsc::Receiver<view_core::msg::Msg>,
        what: &str,
    ) -> view_core::msg::Msg {
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(msg) => msg,
            Err(err) => panic!("{what}: {err}"),
        }
    }

    /// A scratch directory to stand in for a session's cwd, on the same
    /// terms `trust.rs`'s own tests use one: `path_is_contained`
    /// canonicalizes, so a test needs a root that really exists on disk and
    /// a canonical form to compare the emitted path against.
    fn fs_scratch_dir(nonce: &str) -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tmp")
            .join(format!("view-ai-driver-fs-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch cwd");
        std::fs::canonicalize(&dir).expect("canonicalize scratch cwd")
    }

    fn fs_driver(
        cwd: std::path::PathBuf,
    ) -> (
        Driver,
        std::sync::mpsc::Receiver<view_core::msg::Msg>,
        mpsc::UnboundedReceiver<JsonRpcMessage>,
    ) {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(SessionShared::detached(Box::new(move |msg| {
            let _ = events_tx.send(msg);
        })));
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        (Driver::new(shared, out_tx, cwd, false), events_rx, out_rx)
    }

    /// Both methods reach their handlers through `on_request` itself, not
    /// only when a test calls the handler body directly: the dispatch arms
    /// and the `true` capability flags are one claim, and a dispatch that
    /// fell back to `METHOD_NOT_FOUND` would make the advertisement a lie.
    #[tokio::test]
    async fn fs_read_and_write_dispatch_to_their_handlers() {
        let cwd = fs_scratch_dir("dispatch");
        let file = cwd.join("a.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write scratch file");
        let (mut driver, events_rx, mut out_rx) = fs_driver(cwd);

        driver.on_request(
            RequestId::Str("read-1".to_string()),
            "fs/read_text_file",
            &json!({ "path": file }),
        );
        driver.on_request(
            RequestId::Str("write-1".to_string()),
            "fs/write_text_file",
            &json!({ "path": file, "content": "fn main() {}" }),
        );

        // bounded, never a bare `recv`: a dispatch arm that stopped
        // reaching its handler emits nothing at all, and this test's job is
        // to report that, not to hang the suite waiting for it
        assert!(
            matches!(
                next_emitted(&events_rx, "FsReadRequested"),
                view_core::msg::Msg::Ai(AiEvent::FsReadRequested { .. })
            ),
            "fs/read_text_file dispatched to on_fs_read"
        );
        assert!(
            matches!(
                events_rx.recv_timeout(std::time::Duration::from_secs(5)),
                Ok(view_core::msg::Msg::Ai(AiEvent::FsWriteRequested { .. }))
            ),
            "fs/write_text_file dispatched to on_fs_write"
        );
        assert!(
            out_rx.try_recv().is_err(),
            "a dispatched request is answered by the editor later, never \
             refused on the spot"
        );
    }

    /// The `fs/read_text_file` handler: it registers a reply keyed by the
    /// boundary id it emits, and the editor's answer to that id is what
    /// reaches the wire.
    #[tokio::test]
    async fn on_fs_read_registers_a_reply_and_answers_the_wire_when_it_arrives() {
        let cwd = fs_scratch_dir("read-answer");
        let file = cwd.join("a.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write scratch file");
        let (mut driver, events_rx, mut out_rx) = fs_driver(cwd);

        driver.on_fs_read(
            RequestId::Str("read-1".to_string()),
            &json!({ "path": file, "line": 3, "limit": 7 }),
        );

        let view_core::msg::Msg::Ai(AiEvent::FsReadRequested {
            request_id,
            path,
            line,
            limit,
        }) = next_emitted(&events_rx, "FsReadRequested was emitted")
        else {
            panic!("expected FsReadRequested")
        };
        assert_eq!(path, file);
        assert_eq!((line, limit), (Some(3), Some(7)));

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

    /// `line` and `limit` are nullable in the pinned schema, and an agent
    /// asking for the whole file may omit them or send an explicit `null` --
    /// both must read as "no window" rather than one of them becoming a
    /// window of zero lines.
    #[tokio::test]
    async fn an_absent_or_null_read_window_asks_for_the_whole_file() {
        let cwd = fs_scratch_dir("read-window");
        let file = cwd.join("a.rs");
        std::fs::write(&file, "fn main() {}\n").expect("write scratch file");
        let (mut driver, events_rx, _out_rx) = fs_driver(cwd);

        driver.on_fs_read(RequestId::Num(1), &json!({ "path": file }));
        driver.on_fs_read(
            RequestId::Num(2),
            &json!({ "path": file, "line": Value::Null, "limit": Value::Null }),
        );

        for expected in ["an omitted window", "an explicit null window"] {
            let view_core::msg::Msg::Ai(AiEvent::FsReadRequested { line, limit, .. }) =
                next_emitted(&events_rx, "FsReadRequested was emitted")
            else {
                panic!("expected FsReadRequested")
            };
            assert_eq!((line, limit), (None, None), "{expected}");
        }
    }

    /// The `fs/write_text_file` handler's failure leg: a refused write must
    /// answer the wire with an error, not silently drop the request.
    #[tokio::test]
    async fn on_fs_write_registers_a_reply_and_answers_a_refusal_over_the_wire() {
        let cwd = fs_scratch_dir("write-refusal");
        let file = cwd.join("a.rs");
        let (mut driver, events_rx, mut out_rx) = fs_driver(cwd);

        driver.on_fs_write(
            RequestId::Str("write-1".to_string()),
            &json!({ "path": file, "content": "fn main() {}" }),
        );

        let view_core::msg::Msg::Ai(AiEvent::FsWriteRequested {
            request_id,
            path,
            content,
        }) = next_emitted(&events_rx, "FsWriteRequested was emitted")
        else {
            panic!("expected FsWriteRequested")
        };
        // the file does not exist yet: ACP requires the client to create it,
        // so a write to a not-yet-existing path inside the cwd must pass the
        // gate rather than be refused as unresolvable
        assert_eq!(path, file);
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

    /// The trust boundary, on both legs and against both escapes a
    /// containment check exists for. The refusal has to be total: no event
    /// crosses into the editor (nothing loads the buffer, nothing shows in
    /// the panel), and the answer carries an error with no `content` member
    /// -- an empty-string `content` would be indistinguishable from an empty
    /// file and would have the agent act on text that is not there.
    #[test]
    fn a_path_outside_the_session_directory_is_refused_on_both_legs() {
        let cwd = fs_scratch_dir("outside");
        let outside = fs_scratch_dir("outside-target").join("secret.txt");
        std::fs::write(&outside, "secret\n").expect("write outside file");
        let link = cwd.join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).expect("plant symlink");
        #[cfg(not(unix))]
        std::fs::copy(&outside, &link).expect("stand in for a symlink");
        let dot_dot = cwd.join("..").join("escape.txt");
        let (mut driver, events_rx, mut out_rx) = fs_driver(cwd);

        let mut refusals = 0;
        for escape in [&dot_dot, &link] {
            #[cfg(not(unix))]
            if escape == &link {
                continue;
            }
            driver.on_request(
                RequestId::Str(format!("read-{refusals}")),
                "fs/read_text_file",
                &json!({ "path": escape }),
            );
            driver.on_request(
                RequestId::Str(format!("write-{refusals}")),
                "fs/write_text_file",
                &json!({ "path": escape, "content": "overwritten" }),
            );
            refusals += 2;
        }

        for _ in 0..refusals {
            let frame = out_rx.try_recv().expect("the refusal was answered");
            let error = frame.error.expect("the refusal carries an error");
            assert_eq!(error.code, INVALID_PARAMS);
            assert!(
                frame.result.is_none(),
                "a refusal answers with an error only, never a result an \
                 agent could read `content` out of"
            );
        }
        assert!(
            events_rx.try_recv().is_err(),
            "a refused request emits nothing into the editor"
        );
        assert_eq!(
            std::fs::read_to_string(&outside).expect("the outside file survives"),
            "secret\n"
        );
    }

    /// The whole mapping in one place. `Other` is the arm the wire tests
    /// never reach -- a moved `changedtick` and an unwritable target both
    /// arrive as it -- so without this its code is asserted nowhere and
    /// could be changed to anything with a green suite.
    #[test]
    fn every_fs_error_maps_to_the_code_the_pinned_table_names_for_it() {
        let table = [
            (FsError::NotFound, RESOURCE_NOT_FOUND),
            (FsError::PermissionDenied, INTERNAL_ERROR),
            (
                FsError::Other {
                    message: "changedtick moved".to_owned(),
                },
                INTERNAL_ERROR,
            ),
        ];
        for (error, code) in table {
            assert_eq!(fs_code(&error), code, "{error:?} maps to the wrong code");
        }
    }

    fn session_update(update: Value) -> Value {
        json!({ "sessionId": "s1", "update": update })
    }

    /// A tool call's title and status must never regress once rendered:
    /// a further `tool_call_update` after a terminal status (the capture
    /// doc permits, does not forbid, one) would read `known` as absent if
    /// the entry were dropped, and fall back to an empty title and
    /// `Pending`, undoing the completed call the panel already rendered.
    /// `content` is asserted `None` here, not resolved against a
    /// remembered copy: the driver never remembers a call's content (see
    /// [`KnownToolCall`]'s doc) -- leaving an omitted result untouched is
    /// [`view_core::native::ai_panel::Transcript::upsert_tool_call`]'s
    /// job, pinned on that side by
    /// `upsert_tool_call_with_no_content_leaves_the_rendered_result_rows_intact`.
    #[test]
    fn a_tool_call_update_after_a_terminal_status_never_regresses_the_entry() {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(SessionShared::detached(Box::new(move |msg| {
            let _ = events_tx.send(msg);
        })));
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let mut driver = Driver::new(shared, out_tx, std::env::temp_dir(), false);

        driver.on_notification(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "c1",
                "title": "Run tests",
                "status": "completed",
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "42 passed" } },
                ],
            })),
        );
        let view_core::msg::Msg::Ai(AiEvent::ToolCallUpdate {
            status,
            title,
            content,
            ..
        }) = next_emitted(&events_rx, "the terminal update was emitted")
        else {
            panic!("expected ToolCallUpdate")
        };
        assert_eq!(status, ToolCallStatus::Completed);
        assert_eq!(title, "Run tests");
        assert_eq!(content, Some(vec!["42 passed".to_string()]));

        // Names a property `ToolCallUpdate` carries but `content` does not
        // -- the exact shape a post-terminal update the wire permits (a
        // late `rawOutput` correction) actually takes.
        driver.on_notification(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "c1",
                "rawOutput": { "exitCode": 0 },
            })),
        );
        let view_core::msg::Msg::Ai(AiEvent::ToolCallUpdate {
            status,
            title,
            content,
            ..
        }) = next_emitted(&events_rx, "the late update was emitted")
        else {
            panic!("expected ToolCallUpdate")
        };
        assert_eq!(
            status,
            ToolCallStatus::Completed,
            "status must still read completed, not fall back to pending"
        );
        assert_eq!(
            title, "Run tests",
            "title must still be retained, not fall back to empty"
        );
        assert_eq!(
            content, None,
            "content must read as omitted, not as an empty result that \
             erases the 42 passed row already on screen"
        );
    }

    /// An explicit JSON `null` for `content` is omission, the same as the
    /// field being absent: both emit `content: None`, never `Some(vec![])`.
    #[test]
    fn a_null_content_field_emits_omission_not_an_empty_result() {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(SessionShared::detached(Box::new(move |msg| {
            let _ = events_tx.send(msg);
        })));
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let mut driver = Driver::new(shared, out_tx, std::env::temp_dir(), false);

        driver.on_notification(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "c1",
                "title": "Run tests",
                "status": "in_progress",
                "content": [
                    { "type": "content", "content": { "type": "text", "text": "partial" } },
                ],
            })),
        );
        next_emitted(&events_rx, "the first update was emitted");

        driver.on_notification(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "c1",
                "content": Value::Null,
            })),
        );
        let view_core::msg::Msg::Ai(AiEvent::ToolCallUpdate { content, .. }) =
            next_emitted(&events_rx, "the second update was emitted")
        else {
            panic!("expected ToolCallUpdate")
        };
        assert_eq!(
            content, None,
            "an explicit null must read exactly as an omitted field does"
        );
    }

    /// A driver wired to a channel, the shape every emission test below
    /// starts from.
    fn diff_driver() -> (Driver, std::sync::mpsc::Receiver<view_core::msg::Msg>) {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(SessionShared::detached(Box::new(move |msg| {
            let _ = events_tx.send(msg);
        })));
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        (
            Driver::new(shared, out_tx, std::env::temp_dir(), false),
            events_rx,
        )
    }

    fn diff_update(tool_call_id: &str, content: Value) -> Value {
        session_update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": tool_call_id,
            "title": "Edit main.rs",
            "status": "in_progress",
            "content": content,
        }))
    }

    /// An absolute path in this platform's own spelling: `diff_proposal`
    /// refuses anything the wire's absolute-path contract rules out, and a
    /// leading `/` is *not* absolute on Windows (no drive prefix), so a
    /// hard-coded POSIX path would silently turn every proposal test below
    /// into a refusal test there.
    fn abs(tail: &str) -> String {
        if cfg!(windows) {
            format!("C:\\work\\{tail}")
        } else {
            format!("/work/{tail}")
        }
    }

    /// Every `AiEvent::DiffProposed` a receiver holds, in emission order.
    fn proposals(
        events_rx: &std::sync::mpsc::Receiver<view_core::msg::Msg>,
    ) -> Vec<(std::path::PathBuf, Option<String>, String)> {
        events_rx
            .try_iter()
            .filter_map(|msg| match msg {
                view_core::msg::Msg::Ai(AiEvent::DiffProposed {
                    path,
                    old_text,
                    new_text,
                    ..
                }) => Some((path, old_text, new_text)),
                _ => None,
            })
            .collect()
    }

    /// A `"diff"` content item is a proposal the user decides on, decoded
    /// per the wire capture's pinned shape -- not the `[diff content]`
    /// placeholder the transcript row renders for it.
    #[test]
    fn a_diff_content_item_is_raised_as_a_proposal() {
        let (mut driver, events_rx) = diff_driver();

        driver.on_notification(
            "session/update",
            &diff_update(
                "c1",
                json!([{
                    "type": "diff",
                    "path": abs("main.rs"),
                    "oldText": "fn main() {}\n",
                    "newText": "fn main() { run() }\n",
                }]),
            ),
        );

        let first = next_emitted(&events_rx, "the tool call was emitted");
        assert!(
            matches!(
                first,
                view_core::msg::Msg::Ai(AiEvent::ToolCallUpdate { .. })
            ),
            "the call the proposal belongs to reaches the transcript first: {first:?}"
        );
        assert_eq!(
            proposals(&events_rx),
            vec![(
                std::path::PathBuf::from(abs("main.rs")),
                Some("fn main() {}\n".to_string()),
                "fn main() { run() }\n".to_string(),
            )]
        );
    }

    /// `oldText` is absent for a file that does not exist yet -- the one
    /// case with nothing to diff against, which the consumer reads as a
    /// whole-file add.
    #[test]
    fn a_new_file_proposal_carries_no_old_text() {
        let (mut driver, events_rx) = diff_driver();

        driver.on_notification(
            "session/update",
            &diff_update(
                "c1",
                json!([{ "type": "diff", "path": abs("new.rs"), "newText": "fn new() {}\n" }]),
            ),
        );

        assert_eq!(
            proposals(&events_rx),
            vec![(
                std::path::PathBuf::from(abs("new.rs")),
                None,
                "fn new() {}\n".to_string(),
            )]
        );
    }

    /// A `tool_call_update` restates the whole content array. The same
    /// diff restated is the same proposal, and re-proposing it would open
    /// a second review over one the user is part way through; a revised
    /// `newText` is a different proposal and still arrives.
    #[test]
    fn a_restated_diff_does_not_re_propose_but_a_revised_one_does() {
        let (mut driver, events_rx) = diff_driver();
        let first = json!([{
            "type": "diff",
            "path": abs("main.rs"),
            "oldText": "a\n",
            "newText": "b\n",
        }]);

        driver.on_notification("session/update", &diff_update("c1", first.clone()));
        assert_eq!(proposals(&events_rx).len(), 1);

        driver.on_notification("session/update", &diff_update("c1", first));
        assert!(
            proposals(&events_rx).is_empty(),
            "the identical diff was proposed twice"
        );

        driver.on_notification(
            "session/update",
            &diff_update(
                "c1",
                json!([{
                    "type": "diff",
                    "path": abs("main.rs"),
                    "oldText": "a\n",
                    "newText": "c\n",
                }]),
            ),
        );
        assert_eq!(
            proposals(&events_rx).len(),
            1,
            "a revised proposal is a new decision and must reach the user"
        );
    }

    /// A proposal the panel could not take is forgotten here, so the same
    /// diff restated on a later `tool_call_update` proposes again. Without
    /// the clearing it would stay deduplicated forever against a proposal
    /// the user never saw.
    #[test]
    fn a_discarded_proposal_is_proposed_again_when_the_agent_restates_it() {
        let (mut driver, events_rx) = diff_driver();
        let content = json!([{
            "type": "diff",
            "path": abs("main.rs"),
            "oldText": "a\n",
            "newText": "b\n",
        }]);

        driver.on_notification("session/update", &diff_update("c1", content.clone()));
        let raised = proposals(&events_rx);
        assert_eq!(raised.len(), 1);
        let request_id = 1;

        driver.on_notification("session/update", &diff_update("c1", content.clone()));
        assert!(
            proposals(&events_rx).is_empty(),
            "still deduplicated while the proposal stands"
        );

        driver.on_command(AiCommand::DiscardProposal { request_id });
        driver.on_notification("session/update", &diff_update("c1", content));

        assert_eq!(
            proposals(&events_rx).len(),
            1,
            "a discarded proposal must be raisable again"
        );
    }

    /// An item missing a required field is skipped rather than degraded to
    /// a placeholder the way rendered content is: a proposal is bytes
    /// written into a buffer, and there is no partial version of it that
    /// is safe to offer.
    #[test]
    fn a_diff_item_missing_a_required_field_is_not_proposed() {
        let (mut driver, events_rx) = diff_driver();

        driver.on_notification(
            "session/update",
            &diff_update(
                "c1",
                json!([
                    { "type": "diff", "path": abs("main.rs") },
                    { "type": "diff", "newText": "orphan\n" },
                    { "type": "content", "content": { "type": "text", "text": "not a diff" } },
                ]),
            ),
        );

        assert!(proposals(&events_rx).is_empty());
    }

    /// A path off the wire's own absolute-path contract is skipped for the
    /// same reason a missing field is: each of these three spellings binds
    /// a review to a buffer nobody named -- nvim's `[No Name]` scratch
    /// buffer, a file under whatever directory nvim's cwd happens to be, or
    /// a second buffer over the same file -- and there is no way to guess
    /// which file the agent meant.
    #[test]
    fn a_diff_item_whose_path_is_not_absolute_is_not_proposed() {
        let (mut driver, events_rx) = diff_driver();

        driver.on_notification(
            "session/update",
            &diff_update(
                "c1",
                json!([
                    { "type": "diff", "path": "", "newText": "into [No Name]\n" },
                    { "type": "diff", "path": "   ", "newText": "into [No Name]\n" },
                    { "type": "diff", "path": "main.rs", "newText": "against nvim's cwd\n" },
                    { "type": "diff", "path": "./main.rs", "newText": "against nvim's cwd\n" },
                    { "type": "diff", "path": format!("{}/", abs("main.rs")), "newText": "a second buffer\n" },
                    { "type": "diff", "path": format!("{}\\", abs("main.rs")), "newText": "a second buffer\n" },
                ]),
            ),
        );

        assert!(
            proposals(&events_rx).is_empty(),
            "every off-contract spelling must be dropped at the boundary"
        );
    }

    /// The refusal above must not swallow the ordinary case it sits in
    /// front of: an absolute path with a `.` component, or one that does
    /// not exist yet, is exactly what a proposal normally carries.
    #[test]
    fn an_absolute_path_is_still_proposed_alongside_refused_ones() {
        let (mut driver, events_rx) = diff_driver();

        driver.on_notification(
            "session/update",
            &diff_update(
                "c1",
                json!([
                    { "type": "diff", "path": "", "newText": "dropped\n" },
                    { "type": "diff", "path": abs("./new.rs"), "newText": "kept\n" },
                ]),
            ),
        );

        assert_eq!(
            proposals(&events_rx),
            vec![(
                std::path::PathBuf::from(abs("./new.rs")),
                None,
                "kept\n".to_string(),
            )]
        );
    }

    /// `plan_entry` renders a complete entry unchanged.
    #[test]
    fn plan_entry_decodes_a_complete_task() {
        let entry = plan_entry(&json!({
            "content": "Write tests",
            "priority": "high",
            "status": "in_progress",
        }));
        assert_eq!(entry.content, "Write tests");
        assert_eq!(entry.priority, PlanEntryPriority::High);
        assert_eq!(entry.status, PlanEntryStatus::InProgress);
    }

    /// A task with an unpinned `priority`/`status` string, or a missing
    /// `content`, is rendered as a labeled placeholder rather than dropped
    /// from the plan -- a plan update is a wholesale replace, so a
    /// silently dropped task would vanish from the agent's own announced
    /// plan without a trace, not just from one update.
    #[test]
    fn plan_entry_degrades_an_undecodable_task_instead_of_dropping_it() {
        let bad_priority = plan_entry(&json!({
            "content": "Ship it",
            "priority": "urgent",
            "status": "pending",
        }));
        assert!(bad_priority.content.contains("Ship it"));
        assert!(bad_priority.content.contains("[unreadable plan entry]"));
        assert_eq!(bad_priority.priority, PlanEntryPriority::Low);
        assert_eq!(bad_priority.status, PlanEntryStatus::Pending);

        let no_content = plan_entry(&json!({
            "priority": "low",
            "status": "pending",
        }));
        assert_eq!(no_content.content, "[unreadable plan entry]");
    }

    /// The plan-level entry point never shrinks the entry count: an
    /// undecodable task among readable ones is degraded in place, not
    /// silently filtered out from under the others.
    #[test]
    fn emit_plan_keeps_every_entry_including_undecodable_ones() {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(SessionShared::detached(Box::new(move |msg| {
            let _ = events_tx.send(msg);
        })));
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let mut driver = Driver::new(shared, out_tx, std::env::temp_dir(), false);

        driver.on_notification(
            "session/update",
            &session_update(json!({
                "sessionUpdate": "plan",
                "entries": [
                    { "content": "Write tests", "priority": "high", "status": "pending" },
                    { "content": "Ship it", "priority": "urgent", "status": "pending" },
                ],
            })),
        );
        let view_core::msg::Msg::Ai(AiEvent::PlanUpdated { entries }) =
            next_emitted(&events_rx, "PlanUpdated was emitted")
        else {
            panic!("expected PlanUpdated")
        };
        assert_eq!(entries.len(), 2, "the undecodable entry must not vanish");
    }

    /// A ready driver: `begin()` through a successful `session/new`, with
    /// both intervening frames drained from `out_rx` and the `SessionReady`
    /// event drained from `events_rx`, so a permission test starts from a
    /// clean slate with `session_id` set -- `on_command`'s `AnswerPermission`
    /// arm defers to `self.deferred` rather than answering the wire when no
    /// session exists yet (see that arm's own doc), which a real agent can
    /// never trigger for this method since it has no session to issue
    /// `session/request_permission` on before `session/new` succeeds.
    fn driver_with_ready_session() -> (
        Driver,
        mpsc::UnboundedReceiver<JsonRpcMessage>,
        std::sync::mpsc::Receiver<view_core::msg::Msg>,
    ) {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let shared = Arc::new(SessionShared::detached(Box::new(move |msg| {
            let _ = events_tx.send(msg);
        })));
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let mut driver = Driver::new(shared, out_tx, std::env::temp_dir(), false);

        driver.begin();
        let initialize = out_rx.try_recv().expect("initialize was sent");
        driver.on_response(
            initialize.id.expect("initialize carries an id"),
            Ok(json!({ "protocolVersion": PROTOCOL_VERSION, "authMethods": [] })),
        );
        let session_new = out_rx.try_recv().expect("session/new was sent");
        driver.on_response(
            session_new.id.expect("session/new carries an id"),
            Ok(json!({ "sessionId": "s1" })),
        );
        let view_core::msg::Msg::Ai(AiEvent::SessionReady { .. }) =
            next_emitted(&events_rx, "SessionReady was emitted")
        else {
            panic!("expected SessionReady")
        };

        (driver, out_rx, events_rx)
    }

    fn permission_request_params() -> Value {
        json!({
            "sessionId": "s1",
            "toolCall": { "toolCallId": "call_1" },
            "options": [
                { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" },
            ],
        })
    }

    /// The whole point of `session/request_permission`: this is a JSON-RPC
    /// REQUEST that blocks the agent's own turn, so nothing may answer it
    /// on the wire until the user's own answer arrives as
    /// `AiCommand::AnswerPermission` -- an auto-reply here would let the
    /// agent's tool call proceed on a decision the user never made.
    #[test]
    fn a_permission_request_holds_the_reply_open_until_answered() {
        let (mut driver, mut out_rx, events_rx) = driver_with_ready_session();

        driver.on_request(
            RequestId::Str("perm-1".to_string()),
            "session/request_permission",
            &permission_request_params(),
        );

        assert!(
            out_rx.try_recv().is_err(),
            "no reply is sent before the user answers"
        );

        let view_core::msg::Msg::Ai(AiEvent::PermissionRequested {
            request_id,
            tool_call_id,
            title,
            options,
        }) = next_emitted(&events_rx, "PermissionRequested was emitted")
        else {
            panic!("expected PermissionRequested")
        };
        assert_eq!(tool_call_id, "call_1");
        assert_eq!(title, None, "the wire params carried no title member");
        assert_eq!(options.len(), 2);

        driver.on_command(AiCommand::AnswerPermission {
            request_id,
            outcome: PermissionOutcome::Selected {
                option_id: "allow-once".to_string(),
            },
        });

        let frame = out_rx.try_recv().expect("the reply was written");
        assert_eq!(frame.id, Some(RequestId::Str("perm-1".to_string())));
        assert_eq!(
            frame.result,
            Some(json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }))
        );
    }

    /// The driver's own correlation map (`open_permissions`) holds
    /// arbitrarily many outstanding requests, each keyed by its own
    /// `request_id` -- the single-outstanding-request policy is
    /// `view-core`'s `AiPanelState::pending_permission` slot, not a
    /// constraint this map enforces. Two requests answered out of order
    /// still each reach the correct wire id: proves `request_id` keys
    /// the map rather than the map assuming one slot.
    #[test]
    fn two_outstanding_requests_are_answered_independently_by_request_id() {
        let (mut driver, mut out_rx, events_rx) = driver_with_ready_session();

        driver.on_request(
            RequestId::Str("perm-1".to_string()),
            "session/request_permission",
            &permission_request_params(),
        );
        let view_core::msg::Msg::Ai(AiEvent::PermissionRequested {
            request_id: first, ..
        }) = next_emitted(&events_rx, "first PermissionRequested")
        else {
            panic!("expected PermissionRequested")
        };

        driver.on_request(
            RequestId::Str("perm-2".to_string()),
            "session/request_permission",
            &permission_request_params(),
        );
        let view_core::msg::Msg::Ai(AiEvent::PermissionRequested {
            request_id: second, ..
        }) = next_emitted(&events_rx, "second PermissionRequested")
        else {
            panic!("expected PermissionRequested")
        };
        assert_ne!(first, second, "each request gets its own boundary id");

        // answered in reverse order, on purpose: proves the map is keyed by
        // request_id rather than assuming FIFO or a single slot
        driver.on_command(AiCommand::AnswerPermission {
            request_id: second,
            outcome: PermissionOutcome::Cancelled,
        });
        let second_frame = out_rx.try_recv().expect("the second reply was written");
        assert_eq!(second_frame.id, Some(RequestId::Str("perm-2".to_string())));
        assert_eq!(
            second_frame.result,
            Some(json!({ "outcome": { "outcome": "cancelled" } }))
        );

        driver.on_command(AiCommand::AnswerPermission {
            request_id: first,
            outcome: PermissionOutcome::Selected {
                option_id: "allow-once".to_string(),
            },
        });
        let first_frame = out_rx.try_recv().expect("the first reply was written");
        assert_eq!(first_frame.id, Some(RequestId::Str("perm-1".to_string())));
        assert_eq!(
            first_frame.result,
            Some(json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } }))
        );
    }

    /// `ToolCallUpdate.title` is optional on the wire; when an agent does
    /// send it, the client's job is to carry it rather than fall back to
    /// the id.
    #[test]
    fn a_permission_request_carrying_a_title_carries_it_across() {
        let (mut driver, _out_rx, events_rx) = driver_with_ready_session();

        driver.on_request(
            RequestId::Str("perm-1".to_string()),
            "session/request_permission",
            &json!({
                "sessionId": "s1",
                "toolCall": { "toolCallId": "call_1", "title": "Delete config.yaml" },
                "options": [
                    { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                ],
            }),
        );

        let view_core::msg::Msg::Ai(AiEvent::PermissionRequested { title, .. }) =
            next_emitted(&events_rx, "PermissionRequested was emitted")
        else {
            panic!("expected PermissionRequested")
        };
        assert_eq!(title, Some("Delete config.yaml".to_string()));
    }

    /// A turn ending -- successfully or by refusal -- is the point past
    /// which nothing will ever answer a still-open permission reply, so the
    /// driver settles it itself rather than leaving the agent's own
    /// `session/request_permission` call blocked forever.
    #[test]
    fn a_turn_ending_settles_any_still_open_permission_as_cancelled() {
        let (mut driver, mut out_rx, events_rx) = driver_with_ready_session();

        driver.on_command(AiCommand::Prompt {
            text: "do the thing".to_string(),
            context: vec![],
        });
        let prompt_request = out_rx.try_recv().expect("session/prompt was sent");

        driver.on_request(
            RequestId::Str("perm-1".to_string()),
            "session/request_permission",
            &permission_request_params(),
        );
        let view_core::msg::Msg::Ai(AiEvent::PermissionRequested { .. }) =
            next_emitted(&events_rx, "PermissionRequested was emitted")
        else {
            panic!("expected PermissionRequested")
        };

        driver.on_response(
            prompt_request.id.expect("session/prompt carries an id"),
            Ok(json!({ "stopReason": "end_turn" })),
        );

        let cancelled = out_rx
            .try_recv()
            .expect("the still-open permission reply was settled");
        assert_eq!(cancelled.id, Some(RequestId::Str("perm-1".to_string())));
        assert_eq!(
            cancelled.result,
            Some(json!({ "outcome": { "outcome": "cancelled" } }))
        );
        let view_core::msg::Msg::Ai(AiEvent::TurnEnded { .. }) =
            next_emitted(&events_rx, "TurnEnded was emitted")
        else {
            panic!("expected TurnEnded")
        };
    }

    /// The driver's own crash path -- the reader stream failing or ending
    /// unexpectedly -- is the same kind of point of no return as a turn
    /// ending: covered directly here via [`Driver::cancel_open_permissions`]
    /// rather than through `drive`'s task-spawning setup, since that is
    /// where `drive`'s own crash-return arms call it.
    #[test]
    fn cancel_open_permissions_settles_every_outstanding_request() {
        let (mut driver, mut out_rx, events_rx) = driver_with_ready_session();

        driver.on_request(
            RequestId::Str("perm-1".to_string()),
            "session/request_permission",
            &permission_request_params(),
        );
        let view_core::msg::Msg::Ai(AiEvent::PermissionRequested { .. }) =
            next_emitted(&events_rx, "PermissionRequested was emitted")
        else {
            panic!("expected PermissionRequested")
        };

        driver.cancel_open_permissions();

        let cancelled = out_rx.try_recv().expect("the open reply was settled");
        assert_eq!(cancelled.id, Some(RequestId::Str("perm-1".to_string())));
        assert_eq!(
            cancelled.result,
            Some(json!({ "outcome": { "outcome": "cancelled" } }))
        );
        assert!(
            driver.open_permissions.is_empty(),
            "the correlation map is drained, not just replied to"
        );
    }

    /// A currency code is the agent's to choose, and both readers of
    /// `Cost.currency` -- the vlog catch-all and `UsageStats::render`,
    /// which runs once per painted frame -- take it on faith that it is
    /// short. Capped at decode, an oversized wire value never reaches
    /// either.
    #[test]
    fn an_oversized_currency_is_capped_at_decode() {
        let huge = "x".repeat(200_000);
        let cost = cost_from_wire(&json!({ "amount": 1.5, "currency": huge })).unwrap();
        assert_eq!(cost.currency.chars().count(), CURRENCY_CAP);
    }
}
