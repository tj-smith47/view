//! The closed vocabulary the agent boundary crosses on. [`AiEvent`] is
//! everything an agent can tell this session; [`AiCommand`] is everything
//! this session can ask of an agent.
//!
//! Pure data, on the same terms as [`RpcCall`](crate::msg::RpcCall): no
//! serde derive, no JSON type, no agent-protocol crate type appears here,
//! so `view-core` stays free of a wire format and an unencodable exchange
//! is unrepresentable. The crate that speaks the protocol maps each
//! variant onto its wire shape, and the wire strings each variant answers
//! to are pinned in `docs/acp-v1-wire-capture.md` rather than recalled at
//! the mapping site.
//!
//! The vocabulary is closed rather than a `(method, payload)` pair for the
//! reason `RpcCall`'s own doc gives: a stringly boundary re-opens the door
//! to this crate building wire values, and every consumer then decodes
//! them again with no compiler check that the two agree.

use std::path::PathBuf;

/// Everything the agent side can report.
///
/// `PartialEq` (which [`Msg`](crate::msg::Msg) itself does not carry) for
/// the reason [`RpcCall`](crate::msg::RpcCall) carries it: a decoded event
/// is asserted against the exact event the wire bytes meant, field for
/// field, rather than through a hand-written `matches!` arm that silently
/// stops checking a field the day one is added. Not `Eq`: `UsageUpdated`
/// carries a `Cost.amount` (`f64`, pinned in `docs/acp-v1-wire-capture.md`
/// as the wire's own `"format": "double"`), and a float has no total order
/// to found `Eq` on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum AiEvent {
    /// The handshake and session creation both completed; `session_id` is
    /// what every later exchange in this session is addressed to.
    SessionReady { session_id: String },
    /// One streamed chunk of a message.
    ///
    /// `from_agent` distinguishes the agent's own reply from the user turn
    /// the agent streams back: the protocol has separate discriminants for
    /// the two, and they differ only in who authored the text, so one arm
    /// with an author flag beats two arms whose payloads are identical.
    /// Chunks sharing a `message_id` belong to one message; a change of
    /// `message_id` starts a new one, and `None` means the agent declined
    /// to group at all.
    MessageChunk {
        message_id: Option<String>,
        text: String,
        from_agent: bool,
    },
    /// One streamed chunk of the agent's internal reasoning, grouped by
    /// `message_id` on the same terms as [`MessageChunk`](Self::MessageChunk).
    ///
    /// Its own arm rather than a `kind` field on `MessageChunk`, because a
    /// kind field alongside `from_agent` would make
    /// `{ from_agent: false, kind: Thought }` representable and there is no
    /// such thing on the wire -- reasoning is only ever the agent's. The
    /// split also forces every consumer's exhaustive match to decide how
    /// reasoning is presented instead of defaulting to rendering it as the
    /// answer, which is the one presentation that would mislead a reader.
    ThoughtChunk {
        message_id: Option<String>,
        text: String,
    },
    /// A tool call was announced or its state moved. `title` is the
    /// agent's own human-readable label for the call.
    ///
    /// `content` is the call's result content, already decoded to display
    /// strings: a `"text"` `ContentBlock` item becomes its own text, and
    /// every other kind (`image`/`audio`/`resource_link`/`resource`, plus
    /// `ToolCallContent`'s own `"diff"`/`"terminal"` variants) becomes a
    /// labeled placeholder naming the kind it stood in for, per
    /// `docs/acp-v1-wire-capture.md`'s `Content`/`Terminal` pin -- never
    /// dropped silently, since a client that saw `content` on the wire and
    /// showed nothing for it looks like the call produced no output.
    /// `None` when the update carried no `content` member at all (or an
    /// explicit JSON `null`): omission, not an empty result, so a consumer
    /// must leave whatever result it already rendered for this call alone
    /// rather than read the omission as the result having emptied out.
    ToolCallUpdate {
        tool_call_id: String,
        title: String,
        status: ToolCallStatus,
        content: Option<Vec<String>>,
    },
    /// The agent is asking to proceed and cannot continue until answered.
    /// `request_id` is what the answering
    /// [`AiCommand::AnswerPermission`] is correlated against; `options` is
    /// the agent's own list, never a view-side default, so a client
    /// offering a choice the agent did not present is unrepresentable.
    PermissionRequested {
        request_id: u64,
        tool_call_id: String,
        /// The tool call's human-readable name, when the agent sent one.
        /// `ToolCallUpdate` only requires `toolCallId` on the wire, so this
        /// is `None` for an agent that omits it; a consumer falls back to
        /// `tool_call_id` rather than showing nothing.
        title: Option<String>,
        options: Vec<PermissionOption>,
    },
    /// A file modification the agent proposes, carried as content rather
    /// than applied: `old_text` is `None` for a file that does not exist
    /// yet, which is the one case where there is nothing to diff against.
    DiffProposed {
        request_id: u64,
        path: PathBuf,
        old_text: Option<String>,
        new_text: String,
    },
    /// The prompt turn finished, for the reason `stop_reason` names.
    TurnEnded { stop_reason: StopReason },
    /// The agent process died or its stream ended without a turn ending.
    /// `message` is for the user, not a log line: a dead agent leaves the
    /// panel with no other way to say why it stopped answering.
    SessionCrashed { message: String },
    /// An agent-initiated file read, crossing into the sync world as plain
    /// data. The agent crate has no legal path straight into nvim (only
    /// `view-engine` speaks RPC, and the agent crate may never name a
    /// `view-engine` type), so an agent's file read travels the same
    /// channel every other agent-originated event already does.
    /// `request_id` is what the answering [`AiCommand::FsReadReply`] is
    /// correlated against.
    ///
    /// `line` and `limit` are the wire's own optional window into the file
    /// -- a 1-based start line and a maximum line count, each absent for
    /// "the whole file" (`docs/acp-v1-wire-capture.md`,
    /// `fs/read_text_file` case 1). Carried across the boundary rather than
    /// applied on this side of it, so a windowed read costs nvim the lines
    /// asked for instead of the whole buffer.
    FsReadRequested {
        request_id: u64,
        path: PathBuf,
        line: Option<u32>,
        limit: Option<u32>,
    },
    /// The write-side twin of [`Self::FsReadRequested`], carrying the
    /// content the agent proposes to write.
    FsWriteRequested {
        request_id: u64,
        path: PathBuf,
        content: String,
    },
    /// The agent replaced its execution plan. Per the wire's own
    /// description (`docs/acp-v1-wire-capture.md`'s `Plan` pin): "the agent
    /// must send a complete list of all entries with their current status.
    /// The client replaces the entire plan with each update" -- so this
    /// carries the whole plan, never a delta to merge into one already held.
    PlanUpdated { entries: Vec<PlanEntry> },
    /// The session's context-window and cost accounting changed.
    UsageUpdated {
        used: u64,
        size: u64,
        cost: Option<Cost>,
    },
}

/// Everything this session can ask of the agent.
///
/// `PartialEq`/`Eq` for the reason [`RpcCall`](crate::msg::RpcCall)
/// carries them: a command is a value a caller assembles ahead of emitting
/// it, so the assembled command has to be comparable to the exact command
/// that was meant.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiCommand {
    /// One user turn. `context` is what the user attached to it, kept
    /// separate from `text` so an attachment is never flattened into prose
    /// the agent would have to parse back out.
    Prompt {
        text: String,
        context: Vec<ContextBlock>,
    },
    /// The user's answer to an [`AiEvent::PermissionRequested`], correlated
    /// on the `request_id` that event carried.
    AnswerPermission {
        request_id: u64,
        outcome: PermissionOutcome,
    },
    /// Abandon the turn in flight. Carries no id: a session has at most one
    /// turn running, so naming which one would add a way to be wrong with
    /// nothing to gain.
    Cancel,
    /// The sync side's answer to an [`AiEvent::FsReadRequested`], routed
    /// back through the ordinary effect path, never a second channel.
    FsReadReply {
        request_id: u64,
        result: Result<String, FsError>,
    },
    /// The write-side twin of [`Self::FsReadReply`], carrying only
    /// success or failure: the agent's write reply has no payload.
    FsWriteReply {
        request_id: u64,
        result: Result<(), FsError>,
    },
    /// An [`AiEvent::DiffProposed`] the panel could not take: one review is
    /// open and a second is already queued behind it. The session forgets
    /// it was ever proposed, so the agent restating the same diff on a
    /// later `tool_call_update` proposes it again instead of being
    /// deduplicated against a proposal the user never saw.
    DiscardProposal { request_id: u64 },
}

/// Where a tool call stands in its lifecycle.
///
/// Deliberately NOT `#[non_exhaustive]`: these four are the protocol's
/// whole status domain, so the enum is closed by the API it models rather
/// than by this crate's current needs, and every mapping of it can be
/// total.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCallStatus {
    /// Not started: input is still streaming, or approval is outstanding.
    Pending,
    /// Running now.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
}

/// One task in the agent's execution plan, per `docs/acp-v1-wire-capture.md`'s
/// `PlanEntry` pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanEntryPriority,
    pub status: PlanEntryStatus,
}

/// A plan entry's relative importance.
///
/// Deliberately NOT `#[non_exhaustive]`: the wire pins exactly these three
/// (`docs/acp-v1-wire-capture.md`'s `PlanEntryPriority` dump), so the enum
/// is closed by the protocol, not by this crate's current needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

/// A plan entry's lifecycle state.
///
/// Only three, unlike [`ToolCallStatus`]'s four: the wire's
/// `PlanEntryStatus` has no `failed` counterpart (same pin as
/// [`PlanEntryPriority`]'s doc). NOT `#[non_exhaustive]` for the same
/// closed-by-protocol reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

/// Cumulative session cost, the optional payload of
/// [`AiEvent::UsageUpdated`]'s `cost` field, per
/// `docs/acp-v1-wire-capture.md`'s `Cost` pin. Not `Eq`: `amount` is the
/// wire's own `"format": "double"`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cost {
    pub amount: f64,
    pub currency: String,
}

/// Why the agent stopped processing a prompt turn.
///
/// Closed for the same reason as [`ToolCallStatus`]: the protocol defines
/// exactly these five, and a sixth would be a change of what ending a turn
/// means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The turn ended on its own.
    EndTurn,
    /// The agent hit its token ceiling.
    MaxTokens,
    /// The agent hit its ceiling on requests between user turns.
    MaxTurnRequests,
    /// The agent refused to continue; the prompt and everything after it
    /// are dropped from the next turn, which the panel has to show or the
    /// user will believe context they can no longer rely on is still there.
    Refusal,
    /// The turn was cancelled by this session.
    Cancelled,
}

/// One choice the agent offers for a permission request. `option_id` is
/// opaque and is echoed back verbatim in
/// [`PermissionOutcome::Selected`]; `name` is the agent's own label for it.
///
/// Deliberately NOT `#[non_exhaustive]`: these three are the protocol's
/// whole required shape, and the crate that decodes the wire lives outside
/// this one and has to be able to build the value it just read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// What a [`PermissionOption`] does, for a client choosing how to present
/// it. A hint only: the agent's `option_id` decides the behaviour, so two
/// options may legally share a kind.
///
/// Closed for the same reason as [`ToolCallStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionOptionKind {
    /// Permit this one operation.
    AllowOnce,
    /// Permit and remember.
    AllowAlways,
    /// Refuse this one operation.
    RejectOnce,
    /// Refuse and remember.
    RejectAlways,
}

/// How a permission request was resolved.
///
/// Closed: the protocol has exactly these two outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionOutcome {
    /// The turn was abandoned before the user answered. Every unanswered
    /// permission request the session is holding resolves this way when the
    /// turn is cancelled, which is why it is an outcome rather than a
    /// dropped request.
    Cancelled,
    /// The user picked `option_id`, one of the ids the request offered.
    Selected { option_id: String },
}

/// One piece of context attached to a [`AiCommand::Prompt`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextBlock {
    /// Literal text, e.g. a selected region the user pulled in.
    Text { text: String },
    /// A pointer to a resource the agent may fetch for itself, e.g. a
    /// mentioned file. A link rather than inlined content: a mention costs
    /// the turn nothing until the agent decides it needs the bytes.
    ResourceLink { uri: String, name: String },
    /// The current buffer's path and nvim-authoritative text, inlined
    /// rather than a [`Self::ResourceLink`]: the point of attaching it is
    /// that the agent reads it as part of the turn, not on a later fetch.
    CurrentBuffer { path: PathBuf, text: String },
    /// The active visual selection's text and its `(start_line, end_line)`
    /// range.
    Selection { text: String, range: (u32, u32) },
    /// The cursor's buffer-space line and column.
    Cursor { line: u32, col: u32 },
    /// Every current entry from `vim.diagnostic.get(0)`.
    Diagnostics {
        entries: Vec<crate::native::ai_context::DiagnosticEntry>,
    },
    /// Every current entry from `getqflist()`.
    QuickfixList {
        entries: Vec<crate::native::ai_context::QuickfixEntry>,
    },
}

/// Why a filesystem request the agent made could not be answered.
///
/// The sync side's own vocabulary, not the agent protocol's: these are the
/// outcomes an editor-mediated read or write can have, and the crate that
/// speaks the wire renders them into the error a caller sees.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// No such path.
    NotFound,
    /// The path exists but this session may not touch it.
    PermissionDenied,
    /// Anything else, carrying the operating system's own wording: an
    /// agent that is told only "failed" retries the same doomed call.
    Other { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The closed-vocabulary contract: a wildcard-free match over every
    /// arm. Adding a variant breaks this compilation, which is the point --
    /// a new arm silently absorbed by a `_` would reach a consumer that
    /// believes it handled everything.
    #[test]
    fn every_event_variant_is_matched_without_a_wildcard() {
        fn describe(event: &AiEvent) -> &'static str {
            match event {
                AiEvent::SessionReady { .. } => "session_ready",
                AiEvent::MessageChunk { .. } => "message_chunk",
                AiEvent::ThoughtChunk { .. } => "thought_chunk",
                AiEvent::ToolCallUpdate { .. } => "tool_call_update",
                AiEvent::PermissionRequested { .. } => "permission_requested",
                AiEvent::DiffProposed { .. } => "diff_proposed",
                AiEvent::TurnEnded { .. } => "turn_ended",
                AiEvent::SessionCrashed { .. } => "session_crashed",
                AiEvent::FsReadRequested { .. } => "fs_read_requested",
                AiEvent::FsWriteRequested { .. } => "fs_write_requested",
                AiEvent::PlanUpdated { .. } => "plan_updated",
                AiEvent::UsageUpdated { .. } => "usage_updated",
            }
        }

        let events = vec![
            AiEvent::SessionReady {
                session_id: "sess_abc123def456".to_string(),
            },
            AiEvent::MessageChunk {
                message_id: Some("m1".to_string()),
                text: "hello".to_string(),
                from_agent: true,
            },
            AiEvent::ThoughtChunk {
                message_id: None,
                text: "considering".to_string(),
            },
            AiEvent::ToolCallUpdate {
                tool_call_id: "call_001".to_string(),
                title: "Read file".to_string(),
                status: ToolCallStatus::InProgress,
                content: Some(vec!["fn main() {}".to_string()]),
            },
            AiEvent::PermissionRequested {
                request_id: 5,
                tool_call_id: "call_001".to_string(),
                title: Some("Read file".to_string()),
                options: vec![PermissionOption {
                    option_id: "allow-once".to_string(),
                    name: "Allow once".to_string(),
                    kind: PermissionOptionKind::AllowOnce,
                }],
            },
            AiEvent::DiffProposed {
                request_id: 6,
                path: PathBuf::from("/tmp/a.rs"),
                old_text: None,
                new_text: "fn main() {}".to_string(),
            },
            AiEvent::TurnEnded {
                stop_reason: StopReason::EndTurn,
            },
            AiEvent::SessionCrashed {
                message: "agent exited".to_string(),
            },
            AiEvent::FsReadRequested {
                request_id: 7,
                path: PathBuf::from("/tmp/a.rs"),
                line: Some(10),
                limit: Some(50),
            },
            AiEvent::FsWriteRequested {
                request_id: 8,
                path: PathBuf::from("/tmp/a.rs"),
                content: "fn main() {}".to_string(),
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
        ];

        let seen: Vec<&'static str> = events.iter().map(describe).collect();
        assert_eq!(
            seen,
            vec![
                "session_ready",
                "message_chunk",
                "thought_chunk",
                "tool_call_update",
                "permission_requested",
                "diff_proposed",
                "turn_ended",
                "session_crashed",
                "fs_read_requested",
                "fs_write_requested",
                "plan_updated",
                "usage_updated",
            ]
        );
    }

    #[test]
    fn every_command_variant_is_matched_without_a_wildcard() {
        fn describe(cmd: &AiCommand) -> &'static str {
            match cmd {
                AiCommand::Prompt { .. } => "prompt",
                AiCommand::AnswerPermission { .. } => "answer_permission",
                AiCommand::Cancel => "cancel",
                AiCommand::FsReadReply { .. } => "fs_read_reply",
                AiCommand::FsWriteReply { .. } => "fs_write_reply",
                AiCommand::DiscardProposal { .. } => "discard_proposal",
            }
        }

        let commands = [
            AiCommand::Prompt {
                text: "explain this".to_string(),
                context: vec![
                    ContextBlock::Text {
                        text: "fn main() {}".to_string(),
                    },
                    ContextBlock::ResourceLink {
                        uri: "file:///tmp/a.rs".to_string(),
                        name: "a.rs".to_string(),
                    },
                ],
            },
            AiCommand::AnswerPermission {
                request_id: 5,
                outcome: PermissionOutcome::Selected {
                    option_id: "allow-once".to_string(),
                },
            },
            AiCommand::Cancel,
            AiCommand::FsReadReply {
                request_id: 7,
                result: Ok("fn main() {}".to_string()),
            },
            AiCommand::FsWriteReply {
                request_id: 8,
                result: Err(FsError::PermissionDenied),
            },
            AiCommand::DiscardProposal { request_id: 9 },
        ];

        let seen: Vec<&'static str> = commands.iter().map(describe).collect();
        assert_eq!(
            seen,
            vec![
                "prompt",
                "answer_permission",
                "cancel",
                "fs_read_reply",
                "fs_write_reply",
                "discard_proposal",
            ]
        );
    }

    /// The leaf enums are closed by the protocol they model, so their
    /// matches are total too -- an unmapped status or stop reason would
    /// otherwise degrade to a silent no-op in whichever crate maps them.
    #[test]
    fn every_leaf_variant_is_matched_without_a_wildcard() {
        fn status(s: ToolCallStatus) -> &'static str {
            match s {
                ToolCallStatus::Pending => "pending",
                ToolCallStatus::InProgress => "in_progress",
                ToolCallStatus::Completed => "completed",
                ToolCallStatus::Failed => "failed",
            }
        }
        fn stop(s: StopReason) -> &'static str {
            match s {
                StopReason::EndTurn => "end_turn",
                StopReason::MaxTokens => "max_tokens",
                StopReason::MaxTurnRequests => "max_turn_requests",
                StopReason::Refusal => "refusal",
                StopReason::Cancelled => "cancelled",
            }
        }
        fn kind(k: PermissionOptionKind) -> &'static str {
            match k {
                PermissionOptionKind::AllowOnce => "allow_once",
                PermissionOptionKind::AllowAlways => "allow_always",
                PermissionOptionKind::RejectOnce => "reject_once",
                PermissionOptionKind::RejectAlways => "reject_always",
            }
        }
        fn outcome(o: &PermissionOutcome) -> &'static str {
            match o {
                PermissionOutcome::Cancelled => "cancelled",
                PermissionOutcome::Selected { .. } => "selected",
            }
        }
        fn plan_priority(p: PlanEntryPriority) -> &'static str {
            match p {
                PlanEntryPriority::High => "high",
                PlanEntryPriority::Medium => "medium",
                PlanEntryPriority::Low => "low",
            }
        }
        fn plan_status(s: PlanEntryStatus) -> &'static str {
            match s {
                PlanEntryStatus::Pending => "pending",
                PlanEntryStatus::InProgress => "in_progress",
                PlanEntryStatus::Completed => "completed",
            }
        }

        assert_eq!(
            [
                status(ToolCallStatus::Pending),
                status(ToolCallStatus::InProgress),
                status(ToolCallStatus::Completed),
                status(ToolCallStatus::Failed),
            ],
            ["pending", "in_progress", "completed", "failed"]
        );
        assert_eq!(
            [
                stop(StopReason::EndTurn),
                stop(StopReason::MaxTokens),
                stop(StopReason::MaxTurnRequests),
                stop(StopReason::Refusal),
                stop(StopReason::Cancelled),
            ],
            [
                "end_turn",
                "max_tokens",
                "max_turn_requests",
                "refusal",
                "cancelled"
            ]
        );
        assert_eq!(
            [
                kind(PermissionOptionKind::AllowOnce),
                kind(PermissionOptionKind::AllowAlways),
                kind(PermissionOptionKind::RejectOnce),
                kind(PermissionOptionKind::RejectAlways),
            ],
            ["allow_once", "allow_always", "reject_once", "reject_always"]
        );
        assert_eq!(
            [
                outcome(&PermissionOutcome::Cancelled),
                outcome(&PermissionOutcome::Selected {
                    option_id: "allow-once".to_string()
                }),
            ],
            ["cancelled", "selected"]
        );
        assert_eq!(
            [
                plan_priority(PlanEntryPriority::High),
                plan_priority(PlanEntryPriority::Medium),
                plan_priority(PlanEntryPriority::Low),
            ],
            ["high", "medium", "low"]
        );
        assert_eq!(
            [
                plan_status(PlanEntryStatus::Pending),
                plan_status(PlanEntryStatus::InProgress),
                plan_status(PlanEntryStatus::Completed),
            ],
            ["pending", "in_progress", "completed"]
        );
    }

    #[test]
    fn fs_error_variants_are_matched_without_a_wildcard() {
        fn describe(e: &FsError) -> &'static str {
            match e {
                FsError::NotFound => "not_found",
                FsError::PermissionDenied => "permission_denied",
                FsError::Other { .. } => "other",
            }
        }
        assert_eq!(
            [
                describe(&FsError::NotFound),
                describe(&FsError::PermissionDenied),
                describe(&FsError::Other {
                    message: "disk full".to_string()
                }),
            ],
            ["not_found", "permission_denied", "other"]
        );
    }
}
