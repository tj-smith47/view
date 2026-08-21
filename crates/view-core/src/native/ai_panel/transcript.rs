//! The panel's transcript: the streamed messages, tool calls, and plan it
//! has folded so far, the folding rules that keep a chatty agent from
//! growing one row per wire chunk, and the per-entry render cache that
//! keeps a paint proportional to what changed since the last one.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::native::ai_event::{PlanEntry, PlanEntryPriority, PlanEntryStatus, ToolCallStatus};
use crate::native::views::Span;

/// Who spoke one transcript entry, and in which voice. Closed rather than
/// a free-form string: every renderer that ever styles a row by speaker
/// (the composer's own "you" vs "agent" convention every chat-shaped UI
/// uses) switches on this instead of matching text an adapter could spell
/// differently release to release.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRole {
    /// The user's own composed message.
    User,
    /// The agent's reply.
    Agent,
    /// The agent's reasoning, which is the agent speaking but is not its
    /// answer. Its own role rather than a second [`Self::Agent`] entry:
    /// the wire carries reasoning apart from the reply precisely so that a
    /// client cannot present one as the other, and a reader who cannot
    /// tell them apart is being shown the agent asserting things it was
    /// only considering.
    Thought,
}

/// What one transcript entry represents: a streamed message, a tool call's
/// lifecycle, or the agent's current plan. Its own enum rather than fields
/// bolted onto one shared struct, because a tool call has no speaker, a
/// message has no status, and a plan has neither -- a bolted-on shape would
/// make combinations representable that never arrive on the wire.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEntryKind {
    /// A streamed message chunk, folded by `message_id`.
    ///
    /// `message_id` stays `Option` here, matching the wire: `None` means
    /// the agent declined to group the chunk at all, which
    /// [`Transcript::append_or_extend`] treats as "never fold" rather than
    /// coalescing every ungrouped chunk into one entry.
    Message {
        message_id: Option<String>,
        role: TranscriptRole,
    },
    /// One tool call's lifecycle, folded in place by `tool_call_id` as its
    /// status advances from `pending` through to `completed`/`failed`
    /// rather than appended as a new row per update -- an update carries a
    /// state change, not a new event to narrate.
    ///
    /// `result` is the call's decoded result content (see
    /// [`crate::native::ai_event::AiEvent::ToolCallUpdate`]'s doc on how
    /// each item was already decoded before it reached here), rendered as
    /// extra rows beneath the call's title/status line.
    ToolCall {
        tool_call_id: String,
        status: ToolCallStatus,
        result: Vec<String>,
    },
    /// The agent's execution plan, replaced wholesale on every update
    /// rather than folded by id: the wire's own contract
    /// (`docs/acp-v1-wire-capture.md`'s `Plan` pin) is "the client replaces
    /// the entire plan with each update", so there is never more than one
    /// live plan to reconcile against, and [`Transcript::upsert_plan`]
    /// keeps at most one `Plan` entry in the transcript at a time.
    Plan { entries: Vec<PlanEntry> },
}

/// One folded entry in the transcript: what it is, and its text so far --
/// the streamed message body for [`TranscriptEntryKind::Message`], the
/// agent's own title for [`TranscriptEntryKind::ToolCall`]. Empty for
/// [`TranscriptEntryKind::Plan`], whose content lives entirely in the kind.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub kind: TranscriptEntryKind,
    pub text: String,
}

/// The panel's transcript: an ordered log of folded entries, oldest first.
///
/// A newtype over `Vec<TranscriptEntry>` rather than the bare `Vec` the
/// panel held before -- folding a wire chunk is a rule (compare against the
/// last entry, extend or start fresh), not a plain collection operation,
/// and a method on this type is the one place that rule lives instead of
/// being re-derived at every call site that streams into a panel.
///
/// `tool_call_index`/`message_index` map an id straight to its entry's
/// position, so folding a chunk or a tool-call update costs one hash lookup
/// regardless of how many other entries (message chunks, tool calls, plan
/// updates) have interleaved since -- a backward scan over the whole
/// transcript would otherwise grow with session length on every fold.
///
/// `row_cache` holds each entry's already-rendered rows, recomputed lazily:
/// a fold only clears the touched entry's slot (an `Option` write, no
/// allocation), and [`Transcript::rendered_rows`] fills a cleared slot back
/// in on the next read. A paint that follows ten folded chunks touching one
/// entry re-renders that one entry once, not the whole transcript once per
/// chunk. `RefCell` rather than requiring `&mut self` on
/// [`Transcript::rendered_rows`]: the panel's `view()` is read through a
/// `&Model` all the way from `view-surface::render`, which never holds a
/// `&mut Model`, so a lazily-populated cache needs interior mutability to
/// exist at all.
#[derive(Debug, Clone, Default)]
pub struct Transcript {
    entries: Vec<TranscriptEntry>,
    tool_call_index: HashMap<String, usize>,
    message_index: HashMap<String, usize>,
    plan_index: Option<usize>,
    row_cache: RefCell<Vec<Option<Vec<Vec<Span>>>>>,
}

/// Hand-written rather than derived: `row_cache` is a lazily-populated
/// rendering cache, not part of the transcript's identity, and whether a
/// given entry's slot has been computed yet depends only on paint history
/// (has `rendered_rows` been called since the last fold touched it), never
/// on what the transcript logically holds. Two transcripts folded from the
/// same events must compare equal whether or not either has ever been
/// painted; deriving `PartialEq` across every field would make painting a
/// transcript change what it compares equal to, which is not an identity
/// any caller should be able to observe.
impl PartialEq for Transcript {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
            && self.tool_call_index == other.tool_call_index
            && self.message_index == other.message_index
            && self.plan_index == other.plan_index
    }
}

impl Eq for Transcript {}

impl Transcript {
    /// An empty transcript.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            tool_call_index: HashMap::new(),
            message_index: HashMap::new(),
            plan_index: None,
            row_cache: RefCell::new(Vec::new()),
        }
    }

    /// Whether no entry has folded in yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// How many entries have folded in.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every entry, oldest first.
    pub fn iter(&self) -> std::slice::Iter<'_, TranscriptEntry> {
        self.entries.iter()
    }

    /// Every entry's rendered rows, oldest first, flattened across entries
    /// (an entry may render more than one row -- see
    /// [`TranscriptEntryKind::ToolCall`]'s `result` and
    /// [`TranscriptEntryKind::Plan`]). Recomputes only the entries whose
    /// cache slot a fold cleared since the last call; every other entry's
    /// rows are cloned from the cache as-is.
    #[must_use]
    pub fn rendered_rows(&self) -> Vec<Vec<Span>> {
        let mut cache = self.row_cache.borrow_mut();
        let mut rows = Vec::new();
        for (i, entry) in self.entries.iter().enumerate() {
            let entry_rows = cache[i].get_or_insert_with(|| render_entry(entry));
            rows.extend(entry_rows.iter().cloned());
        }
        rows
    }

    /// Folds one streamed message chunk in place.
    ///
    /// `message_id` and `text` are taken as borrows rather than owned
    /// values: a chunk that folds into the transcript's last entry only
    /// ever needs to compare and append, and the wire delivers these
    /// chunks at whatever cadence the agent emits them -- potentially many
    /// per second -- so forcing an allocation (a cloned id, an owned
    /// string) on every call regardless of whether this one folds would
    /// turn the fold-vs-start-fresh check itself into the one-allocation-
    /// per-chunk cost this method exists to avoid. An id is only ever
    /// allocated when a new entry actually starts.
    ///
    /// Looked up by [`Self::message_index`] rather than only the last
    /// entry: the wire's own contract for `messageId`
    /// (`docs/acp-v1-wire-capture.md`'s `ContentChunk` pin) is "a change in
    /// `messageId` indicates a new message", which bounds when a *new*
    /// entry starts but says nothing about a later chunk resuming an
    /// earlier id after something else (a tool call announcement, a plan
    /// update) interleaved -- an agent streaming its answer around a tool
    /// call it dispatches mid-reply still owns one message, so that chunk
    /// must resume the original entry rather than starting a second one.
    pub fn append_or_extend(&mut self, message_id: Option<&str>, text: &str, role: TranscriptRole) {
        if let Some(id) = message_id {
            // Same id, same voice, or it is a different entry. An agent is
            // free to stream its reasoning and its answer under one id, and
            // folding those together would append the thought into the
            // reply as though the agent had said it.
            if let Some(&i) = self.message_index.get(id) {
                if matches!(
                    &self.entries[i].kind,
                    TranscriptEntryKind::Message { role: held, .. } if *held == role
                ) {
                    self.entries[i].text.push_str(text);
                    self.row_cache.get_mut()[i] = None;
                    return;
                }
            }
        }
        self.entries.push(TranscriptEntry {
            kind: TranscriptEntryKind::Message {
                message_id: message_id.map(ToString::to_string),
                role,
            },
            text: text.to_string(),
        });
        self.row_cache.get_mut().push(None);
        if let Some(id) = message_id {
            self.message_index
                .insert(id.to_string(), self.entries.len() - 1);
        }
    }

    /// Folds one `tool_call`/`tool_call_update` in place, keyed by
    /// `tool_call_id`: the first sighting of an id starts a new entry, and
    /// every later sighting of the same id updates that entry's status,
    /// title, and result content rather than appending a new row -- an
    /// update is a state transition on one call, not a new call.
    ///
    /// Looked up by [`Self::tool_call_index`] for the same reason
    /// [`Self::append_or_extend`] looks up `message_index`: a message chunk
    /// can stream between a tool call's announcement and its next update,
    /// so the call an update names is not always the most recent entry.
    ///
    /// `content: None` means the update said nothing about the call's
    /// result -- omission, not an empty result -- so an existing entry's
    /// `result` is left exactly as it stood; only `Some` ever overwrites
    /// it. A brand-new entry has no prior `result` to preserve, so `None`
    /// there starts it empty, same as `Some(vec![])` would.
    pub fn upsert_tool_call(
        &mut self,
        tool_call_id: String,
        title: String,
        status: ToolCallStatus,
        content: Option<Vec<String>>,
    ) {
        if let Some(&i) = self.tool_call_index.get(&tool_call_id) {
            let result = match content {
                Some(content) => content,
                None => match &self.entries[i].kind {
                    TranscriptEntryKind::ToolCall { result, .. } => result.clone(),
                    _ => Vec::new(),
                },
            };
            self.entries[i].kind = TranscriptEntryKind::ToolCall {
                tool_call_id,
                status,
                result,
            };
            self.entries[i].text = title;
            self.row_cache.get_mut()[i] = None;
            return;
        }
        self.entries.push(TranscriptEntry {
            kind: TranscriptEntryKind::ToolCall {
                tool_call_id: tool_call_id.clone(),
                status,
                result: content.unwrap_or_default(),
            },
            text: title,
        });
        self.row_cache.get_mut().push(None);
        self.tool_call_index
            .insert(tool_call_id, self.entries.len() - 1);
    }

    /// Replaces the transcript's plan wholesale, per the wire's own
    /// full-replace contract (see [`TranscriptEntryKind::Plan`]'s doc). The
    /// first plan update starts the transcript's one `Plan` entry; every
    /// later update overwrites that same entry in place rather than
    /// appending a second one.
    pub fn upsert_plan(&mut self, entries: Vec<PlanEntry>) {
        if let Some(i) = self.plan_index {
            self.entries[i].kind = TranscriptEntryKind::Plan { entries };
            self.row_cache.get_mut()[i] = None;
            return;
        }
        self.entries.push(TranscriptEntry {
            kind: TranscriptEntryKind::Plan { entries },
            text: String::new(),
        });
        self.row_cache.get_mut().push(None);
        self.plan_index = Some(self.entries.len() - 1);
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only tally of [`render_entry`] calls, so a test can assert the
    /// work [`Transcript::rendered_rows`]'s cache exists to avoid, not just
    /// the bytes it produces: recomputing an unchanged entry reproduces the
    /// same rendered bytes a cache hit would, so a byte-equality assertion
    /// alone cannot tell a cache hit from a cache silently bypassed.
    /// Thread-local rather than the workspace's process-global
    /// `CountingAllocator`: the default test harness runs a binary's tests
    /// on separate threads, and this counter needs isolation per test, not
    /// per binary -- the same shared state a `RefCell` gives `Transcript`
    /// its cache through, reused here for the same reason.
    static RENDER_ENTRY_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One transcript entry's rendered rows: a single line for a message, a
/// title line plus one line per result item for a tool call, or one line
/// per task for a plan.
fn render_entry(entry: &TranscriptEntry) -> Vec<Vec<Span>> {
    #[cfg(test)]
    RENDER_ENTRY_CALLS.with(|calls| calls.set(calls.get() + 1));
    match &entry.kind {
        TranscriptEntryKind::Message { role, .. } => {
            let prefix = match role {
                TranscriptRole::User => "You",
                TranscriptRole::Agent => "Agent",
                TranscriptRole::Thought => "Thinking",
            };
            vec![vec![Span::plain(format!("{prefix}: {}", entry.text))]]
        }
        TranscriptEntryKind::ToolCall { status, result, .. } => {
            let prefix = match status {
                ToolCallStatus::Pending => "pending",
                ToolCallStatus::InProgress => "running",
                ToolCallStatus::Completed => "done",
                ToolCallStatus::Failed => "failed",
            };
            let mut rows = vec![vec![Span::plain(format!("{prefix}: {}", entry.text))]];
            rows.extend(
                result
                    .iter()
                    .map(|line| vec![Span::plain(format!("  {line}"))]),
            );
            rows
        }
        TranscriptEntryKind::Plan { entries } => entries
            .iter()
            .map(|e| {
                let priority = match e.priority {
                    PlanEntryPriority::High => "high",
                    PlanEntryPriority::Medium => "medium",
                    PlanEntryPriority::Low => "low",
                };
                let status = match e.status {
                    PlanEntryStatus::Pending => "pending",
                    PlanEntryStatus::InProgress => "in_progress",
                    PlanEntryStatus::Completed => "completed",
                };
                vec![Span::plain(format!(
                    "plan [{status}, {priority}]: {}",
                    e.content
                ))]
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn reasoning_renders_apart_from_the_reply_it_shares_an_id_with() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(Some("m1"), "weighing it", TranscriptRole::Thought);
        transcript.append_or_extend(Some("m1"), "the answer", TranscriptRole::Agent);

        assert_eq!(
            transcript.rendered_rows(),
            vec![
                vec![Span::plain("Thinking: weighing it")],
                vec![Span::plain("Agent: the answer")],
            ],
            "reasoning must be readable as reasoning, not as what the agent said"
        );
    }

    #[test]
    fn two_chunks_sharing_a_message_id_fold_into_one_entry() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(Some("m1"), "hel", TranscriptRole::Agent);
        transcript.append_or_extend(Some("m1"), "lo", TranscriptRole::Agent);

        assert_eq!(transcript.len(), 1);
        let entry = transcript.iter().next().expect("one entry");
        assert_eq!(entry.text, "hello");
        assert_eq!(
            entry.kind,
            TranscriptEntryKind::Message {
                message_id: Some("m1".to_string()),
                role: TranscriptRole::Agent,
            }
        );
    }

    #[test]
    fn chunks_with_no_message_id_never_fold_together() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(None, "one", TranscriptRole::Agent);
        transcript.append_or_extend(None, "two", TranscriptRole::Agent);

        assert_eq!(transcript.len(), 2);
    }

    #[test]
    fn a_new_message_id_starts_a_new_entry_rather_than_extending_the_old_one() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(Some("m1"), "first message", TranscriptRole::Agent);
        transcript.append_or_extend(Some("m2"), "second message", TranscriptRole::Agent);

        assert_eq!(transcript.len(), 2);
        let mut entries = transcript.iter();
        assert_eq!(entries.next().expect("first entry").text, "first message");
        assert_eq!(entries.next().expect("second entry").text, "second message");
    }

    #[test]
    fn a_message_id_resumes_its_entry_across_an_intervening_tool_call() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(Some("m1"), "chunk A", TranscriptRole::Agent);
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Pending,
            None,
        );
        transcript.append_or_extend(Some("m1"), ", chunk A'", TranscriptRole::Agent);

        assert_eq!(
            transcript.len(),
            2,
            "the resumed chunk must fold into the existing message entry, \
             not start a third one"
        );
        let mut entries = transcript.iter();
        assert_eq!(
            entries.next().expect("message entry").text,
            "chunk A, chunk A'",
            "text from both sides of the interleaving must concatenate"
        );
        assert_eq!(
            entries.next().expect("tool call entry").kind,
            TranscriptEntryKind::ToolCall {
                tool_call_id: "call_1".to_string(),
                status: ToolCallStatus::Pending,
                result: Vec::new(),
            }
        );
    }

    #[test]
    fn a_tool_call_announcement_then_update_fold_into_one_entry_by_status() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Pending,
            None,
        );
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Completed,
            Some(vec!["fn main() {}".to_string()]),
        );

        assert_eq!(transcript.len(), 1);
        let entry = transcript.iter().next().expect("one entry");
        assert_eq!(
            entry.kind,
            TranscriptEntryKind::ToolCall {
                tool_call_id: "call_1".to_string(),
                status: ToolCallStatus::Completed,
                result: vec!["fn main() {}".to_string()],
            }
        );
    }

    #[test]
    fn a_tool_call_update_between_message_chunks_still_finds_its_entry() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Pending,
            None,
        );
        transcript.append_or_extend(Some("m1"), "meanwhile, agent text", TranscriptRole::Agent);
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Completed,
            None,
        );

        assert_eq!(transcript.len(), 2);
        let mut entries = transcript.iter();
        assert_eq!(
            entries.next().expect("tool call entry").kind,
            TranscriptEntryKind::ToolCall {
                tool_call_id: "call_1".to_string(),
                status: ToolCallStatus::Completed,
                result: Vec::new(),
            }
        );
        assert_eq!(
            entries.next().expect("message entry").text,
            "meanwhile, agent text"
        );
    }

    #[test]
    fn a_different_tool_call_id_starts_its_own_entry() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Completed,
            None,
        );
        transcript.upsert_tool_call(
            "call_2".to_string(),
            "Write file".to_string(),
            ToolCallStatus::Pending,
            None,
        );

        assert_eq!(transcript.len(), 2);
    }

    /// A `tool_call_update` that says nothing about `content` (`None`, the
    /// wire's own omission -- see [`Transcript::upsert_tool_call`]'s doc)
    /// must leave an already-rendered result exactly as it stood, even
    /// past a terminal status: a call finishes with a result on screen,
    /// then the agent sends one more update naming only some other
    /// property (`rawOutput`, `locations`), and the result rows must not
    /// vanish out from under a call the panel already renders as done.
    #[test]
    fn upsert_tool_call_with_no_content_leaves_the_rendered_result_rows_intact() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Run tests".to_string(),
            ToolCallStatus::Completed,
            Some(vec!["42 passed".to_string()]),
        );
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Run tests".to_string(),
            ToolCallStatus::Completed,
            None,
        );

        assert_eq!(transcript.len(), 1);
        let entry = transcript.iter().next().expect("one entry");
        assert_eq!(
            entry.kind,
            TranscriptEntryKind::ToolCall {
                tool_call_id: "call_1".to_string(),
                status: ToolCallStatus::Completed,
                result: vec!["42 passed".to_string()],
            },
            "the result row must survive a contentless update, not empty out"
        );
        assert_eq!(
            transcript.rendered_rows().len(),
            2,
            "the title/status row plus the one result row must both still \
             render"
        );
    }

    #[test]
    fn a_second_plan_update_replaces_the_first_rather_than_appending() {
        let mut transcript = Transcript::new();
        transcript.upsert_plan(vec![PlanEntry {
            content: "Read the file".to_string(),
            priority: PlanEntryPriority::High,
            status: PlanEntryStatus::Pending,
        }]);
        transcript.upsert_plan(vec![
            PlanEntry {
                content: "Read the file".to_string(),
                priority: PlanEntryPriority::High,
                status: PlanEntryStatus::Completed,
            },
            PlanEntry {
                content: "Write the fix".to_string(),
                priority: PlanEntryPriority::Medium,
                status: PlanEntryStatus::InProgress,
            },
        ]);

        assert_eq!(
            transcript.len(),
            1,
            "a plan update replaces the whole plan, never appends a second entry"
        );
        let TranscriptEntryKind::Plan { entries } = &transcript.iter().next().unwrap().kind else {
            unreachable!("upsert_plan always stores TranscriptEntryKind::Plan");
        };
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn a_plan_between_message_chunks_does_not_disturb_them() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(Some("m1"), "before", TranscriptRole::Agent);
        transcript.upsert_plan(vec![PlanEntry {
            content: "task".to_string(),
            priority: PlanEntryPriority::Low,
            status: PlanEntryStatus::Pending,
        }]);
        transcript.append_or_extend(Some("m1"), " after", TranscriptRole::Agent);

        assert_eq!(transcript.len(), 2);
        assert_eq!(transcript.iter().next().unwrap().text, "before after");
    }

    fn render_entry_calls() -> usize {
        RENDER_ENTRY_CALLS.with(std::cell::Cell::get)
    }

    fn reset_render_entry_calls() {
        RENDER_ENTRY_CALLS.with(|calls| calls.set(0));
    }

    /// Asserts the work the cache exists to avoid, not just the bytes it
    /// happens to reproduce: recomputing an unchanged entry from scratch
    /// yields byte-identical rows to a cache hit, so a bytes-only assertion
    /// cannot tell a real cache from `rendered_rows` bypassing it and
    /// calling `render_entry` unconditionally. `render_entry_calls()` is
    /// the observable that distinguishes them.
    #[test]
    fn rendered_rows_only_recomputes_the_entry_that_changed() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(Some("m1"), "one", TranscriptRole::Agent);
        transcript.append_or_extend(Some("m2"), "two", TranscriptRole::Agent);

        reset_render_entry_calls();
        let first_pass = transcript.rendered_rows();
        assert_eq!(first_pass.len(), 2);
        assert_eq!(
            render_entry_calls(),
            2,
            "both entries are cold on the first call, so both must render once"
        );

        reset_render_entry_calls();
        let repeat_pass = transcript.rendered_rows();
        assert_eq!(repeat_pass, first_pass);
        assert_eq!(
            render_entry_calls(),
            0,
            "nothing changed since the last call: a real cache hit renders \
             neither entry again, where a bypassed cache would render both"
        );

        transcript.append_or_extend(Some("m2"), " more", TranscriptRole::Agent);
        reset_render_entry_calls();
        let second_pass = transcript.rendered_rows();
        assert_eq!(
            second_pass[0], first_pass[0],
            "the untouched entry's cached row must be reused byte for byte"
        );
        assert_eq!(
            second_pass[1],
            vec![Span::plain("Agent: two more")],
            "the touched entry's row must reflect the fold"
        );
        assert_eq!(
            render_entry_calls(),
            1,
            "only the entry the fold touched should re-render -- a bypassed \
             cache would render both entries again here"
        );
    }
}
