//! The panel's transcript: the streamed messages, tool calls, and plan it
//! has folded so far, the folding rules that keep a chatty agent from
//! growing one row per wire chunk, and the per-entry render cache that
//! keeps a paint proportional to what changed since the last one.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Duration;

use crate::native::ai_event::{PlanEntry, PlanEntryPriority, PlanEntryStatus, ToolCallStatus};
use crate::native::views::{Span, StyleRole};

/// The glyph opening a transcript entry the user composed.
///
/// Every entry opens with one of these rather than with a spelled-out
/// speaker: the marker is what makes two consecutive messages read as two
/// items instead of one paragraph, and the colors the roles carry
/// (`StyleRole::Ai*`) are what say which voice each one is in.
const USER_MARK: &str = "\u{276f} ";
/// The glyph opening one of the agent's replies.
const AGENT_MARK: &str = "\u{25cf} ";
/// The glyph opening the agent's reasoning.
const THOUGHT_MARK: &str = "\u{25e6} ";
/// The glyph a tool call that has not started yet opens with.
const TOOL_PENDING_MARK: &str = "\u{b7} ";
/// The glyph a completed tool call opens with.
const TOOL_DONE_MARK: &str = "\u{2713} ";
/// The glyph a failed tool call opens with.
const TOOL_FAILED_MARK: &str = "\u{2717} ";

/// The braille cycle a running tool call's marker animates through.
const SPINNER_FRAMES: [&str; 8] = [
    "\u{280b} ",
    "\u{2819} ",
    "\u{2839} ",
    "\u{2838} ",
    "\u{283c} ",
    "\u{2834} ",
    "\u{2826} ",
    "\u{2827} ",
];

/// How long one spinner frame stands before [`Transcript::advance_spinner`]
/// replaces it. The whole cycle is eight frames, so this is a shade under
/// two thirds of a second per revolution -- fast enough to read as motion,
/// slow enough that a wedged agent is not asking for a repaint every frame
/// the terminal could draw.
pub const SPINNER_INTERVAL: Duration = Duration::from_millis(80);

/// The prefix every locally minted message id carries, so an id this panel
/// invented for its own echo can never collide with one an agent chose.
const LOCAL_ID_PREFIX: &str = "view-local-";

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

/// Where a transcript window starts: an entry, and how many of that entry's
/// own rendered rows sit above the window's first visible row.
///
/// An entry-and-offset pair rather than a row index counted from either
/// end, because both of those move under the transcript's own growth: a
/// row index counted from the newest end slides the held window backwards
/// every time a chunk streams in, and one counted from the oldest end can
/// only be resolved by rendering every entry before it. An entry index is
/// stable against both -- an append touches neither the entry the window
/// starts at nor how many of its rows precede the window.
///
/// Ordered, so a held window can be compared against the one that follows
/// the tail (see [`Transcript::tail_anchor`]) to decide whether following
/// has resumed. The ordering is only meaningful between normalized
/// anchors, which is every anchor this module hands out: `row` is always
/// inside its entry, except for the one anchor past the last entry.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct TranscriptAnchor {
    entry: usize,
    row: usize,
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
/// allocation), and [`Transcript::rows_from`] fills a cleared slot back
/// in on the next read. A paint that follows ten folded chunks touching one
/// entry re-renders that one entry once, not the whole transcript once per
/// chunk. `RefCell` rather than requiring `&mut self` on
/// [`Transcript::rows_from`]: the panel's `view()` is read through a
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
    /// The prompt this panel echoed locally that a replaying adapter may
    /// still restate over the wire (see [`Transcript::echo_user_prompt`]).
    echo: Option<LocalEcho>,
    /// How many locally minted message ids have been handed out, so a
    /// second prompt in one session never reuses the first one's id and
    /// folds into the entry it already wrote.
    local_seq: u64,
    /// How many tool calls are mid-flight. Counted as statuses move rather
    /// than scanned per tick: it is what decides whether the spinner is
    /// running at all, and a scan would put the length of the session into
    /// a question asked on a timer.
    running_calls: usize,
    /// Whether a spinner tick is already scheduled, so a burst of tool-call
    /// updates arms one timer rather than one per update.
    spinner_armed: bool,
    /// Which [`SPINNER_FRAMES`] entry an unresolved tool call currently
    /// paints.
    spinner_frame: usize,
}

/// The user's own prompt as this panel echoed it, and how much of it an
/// adapter's replay has restated so far.
///
/// The panel writes the prompt into the transcript itself the moment it is
/// submitted, because the one adapter view ships against never replays it
/// and a user who cannot see what they asked is reading half a
/// conversation. An adapter that *does* replay would then say it twice, so
/// the replay is matched off against this: a `from_agent: false` chunk that
/// continues what was echoed is the echo arriving over the wire and is
/// dropped, and anything else is something the user has not been shown yet
/// and appends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LocalEcho {
    /// Which entry holds the echoed prompt.
    entry: usize,
    /// How many bytes of that entry's text a replay has already matched, so
    /// a replay streamed in several chunks is recognised chunk by chunk
    /// rather than only when one chunk happens to carry the whole prompt.
    matched: usize,
}

/// Hand-written rather than derived: `row_cache` is a lazily-populated
/// rendering cache, not part of the transcript's identity, and whether a
/// given entry's slot has been computed yet depends only on paint history
/// (has `rows_from` been called since the last fold touched it), never
/// on what the transcript logically holds. Two transcripts folded from the
/// same events must compare equal whether or not either has ever been
/// painted; deriving `PartialEq` across every field would make painting a
/// transcript change what it compares equal to, which is not an identity
/// any caller should be able to observe. The spinner's frame and its armed
/// flag are excluded on the same terms -- which eighth of a second a
/// running call's glyph is on says nothing about what the transcript holds
/// -- while the pending local echo is included, because whether a replay
/// would be absorbed or appended is a real difference between two
/// transcripts.
impl PartialEq for Transcript {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
            && self.tool_call_index == other.tool_call_index
            && self.message_index == other.message_index
            && self.plan_index == other.plan_index
            && self.echo == other.echo
    }
}

impl Eq for Transcript {}

impl Transcript {
    /// An empty transcript.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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

    /// At most `budget` rendered rows starting at `anchor`, oldest first,
    /// flattened across entries (an entry may render more than one row --
    /// see [`TranscriptEntryKind::ToolCall`]'s `result` and
    /// [`TranscriptEntryKind::Plan`]). Recomputes only the entries whose
    /// cache slot a fold cleared since the last call; every other entry's
    /// rows are cloned from the cache as-is.
    ///
    /// Bounded rather than whole because this runs on the paint path and a
    /// transcript is the one piece of panel content that grows without
    /// limit: a session hours long renders and clones every row it has ever
    /// held, on every frame, to hand the overlay rows it will cut back to
    /// the panel's height anyway. `budget` is the overlay's own window, so
    /// the per-frame cost is the panel's height rather than the session's
    /// length -- and starting at an anchor rather than at row zero is what
    /// keeps that true for a panel showing the newest rows, which is every
    /// panel that has not been scrolled.
    #[must_use]
    pub fn rows_from(&self, anchor: TranscriptAnchor, budget: usize) -> Vec<Vec<Span>> {
        let mut cache = self.row_cache.borrow_mut();
        let mut rows = Vec::new();
        let mut skip = anchor.row;
        for i in anchor.entry..self.entries.len() {
            if rows.len() >= budget {
                break;
            }
            let entry_rows =
                cache[i].get_or_insert_with(|| render_entry(&self.entries[i], self.spinner_frame));
            rows.extend(entry_rows.iter().skip(skip).cloned());
            skip = 0;
        }
        rows.truncate(budget);
        rows
    }

    /// The anchor a window `viewport` rows tall starts at when its last row
    /// is the transcript's newest -- what a panel following the tail paints
    /// from, recomputed per frame so that a chunk streaming in moves the
    /// window rather than scrolling out from under it.
    #[must_use]
    pub fn tail_anchor(&self, viewport: usize) -> TranscriptAnchor {
        self.scrolled_back(
            TranscriptAnchor {
                entry: self.entries.len(),
                row: 0,
            },
            viewport,
        )
    }

    /// `anchor` moved `rows` rendered rows toward the oldest entry, stopping
    /// at the transcript's first row.
    ///
    /// Walks entries backwards rather than measuring the whole transcript:
    /// the cost is the distance asked for, not the session's length, so a
    /// keypress in an hours-long session costs a screenful of work.
    #[must_use]
    pub fn scrolled_back(&self, anchor: TranscriptAnchor, rows: usize) -> TranscriptAnchor {
        let mut cache = self.row_cache.borrow_mut();
        let mut left = rows;
        let mut entry = anchor.entry.min(self.entries.len());
        let mut row = anchor.row;
        while left > 0 {
            if row >= left {
                return TranscriptAnchor {
                    entry,
                    row: row - left,
                };
            }
            left -= row;
            if entry == 0 {
                return TranscriptAnchor::default();
            }
            entry -= 1;
            row = cache[entry]
                .get_or_insert_with(|| render_entry(&self.entries[entry], self.spinner_frame))
                .len();
        }
        TranscriptAnchor { entry, row }
    }

    /// `anchor` moved `rows` rendered rows toward the newest entry, stopping
    /// one past the last entry -- the position [`Self::tail_anchor`] is
    /// compared against to tell a window that has caught up with the tail
    /// from one still held behind it.
    #[must_use]
    pub fn scrolled_forward(&self, anchor: TranscriptAnchor, rows: usize) -> TranscriptAnchor {
        let mut cache = self.row_cache.borrow_mut();
        let mut left = rows;
        let mut entry = anchor.entry;
        let mut row = anchor.row;
        while left > 0 && entry < self.entries.len() {
            let below = cache[entry]
                .get_or_insert_with(|| render_entry(&self.entries[entry], self.spinner_frame))
                .len()
                .saturating_sub(row);
            if below > left {
                return TranscriptAnchor {
                    entry,
                    row: row + left,
                };
            }
            left -= below;
            entry += 1;
            row = 0;
        }
        TranscriptAnchor { entry, row }
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
            let was_running = matches!(
                self.entries[i].kind,
                TranscriptEntryKind::ToolCall {
                    status: ToolCallStatus::InProgress,
                    ..
                }
            );
            self.note_running(was_running, status == ToolCallStatus::InProgress);
            self.entries[i].kind = TranscriptEntryKind::ToolCall {
                tool_call_id,
                status,
                result,
            };
            self.entries[i].text = title;
            self.row_cache.get_mut()[i] = None;
            return;
        }
        self.note_running(false, status == ToolCallStatus::InProgress);
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

    /// Moves the in-flight tool-call count for one status transition.
    fn note_running(&mut self, was: bool, now: bool) {
        match (was, now) {
            (false, true) => self.running_calls += 1,
            (true, false) => self.running_calls = self.running_calls.saturating_sub(1),
            _ => {}
        }
    }

    /// Whether a spinner tick has to be scheduled now: a call is running and
    /// nothing is already timing the next frame. Claims the slot as it
    /// answers, so a turn announcing six tool calls arms one timer.
    ///
    /// The caller owns the clock (`view-core` has none) and turns a `true`
    /// here into `Effect::ScheduleAiSpinnerTick`.
    pub fn arm_spinner(&mut self) -> bool {
        let arm = self.running_calls > 0 && !self.spinner_armed;
        self.spinner_armed |= arm;
        arm
    }

    /// Advances every unresolved tool call's marker to the next spinner
    /// frame, reporting whether the spinner is still running -- which is
    /// both what the caller repaints on and what it re-arms the tick on.
    ///
    /// Only the entries actually spinning drop their cached rows, so the
    /// frame a tick repaints is the marker's own row and nothing else on
    /// the panel; a transcript with no call in flight clears the armed flag
    /// and does no work at all.
    pub fn advance_spinner(&mut self) -> bool {
        if self.running_calls == 0 {
            self.spinner_armed = false;
            return false;
        }
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
        let cache = self.row_cache.get_mut();
        for (i, entry) in self.entries.iter().enumerate() {
            if matches!(
                entry.kind,
                TranscriptEntryKind::ToolCall {
                    status: ToolCallStatus::InProgress,
                    ..
                }
            ) {
                cache[i] = None;
            }
        }
        true
    }

    /// Appends the prompt the user just submitted, under an id minted here.
    ///
    /// The panel says what the user said, itself, at the moment they said
    /// it: the wire is under no obligation to replay a prompt back (the ACP
    /// adapter view ships against never does), and a transcript that shows
    /// only the answers is a conversation with one side missing.
    pub fn echo_user_prompt(&mut self, text: &str) {
        self.local_seq += 1;
        let id = format!("{LOCAL_ID_PREFIX}{}", self.local_seq);
        self.append_or_extend(Some(&id), text, TranscriptRole::User);
        self.echo = Some(LocalEcho {
            entry: self.entries.len() - 1,
            matched: 0,
        });
    }

    /// Folds one `from_agent: false` chunk, dropping it when it is the
    /// adapter restating the prompt this panel already echoed (see
    /// [`LocalEcho`]) and appending it when it is something else -- an
    /// adapter that injects context of its own into the user's turn has
    /// said something the user has not seen, and dropping that would hide
    /// it.
    pub fn append_user_chunk(&mut self, message_id: Option<&str>, text: &str) {
        if self.absorbs_replay(text) {
            return;
        }
        self.append_or_extend(message_id, text, TranscriptRole::User);
    }

    /// Whether `text` continues the echoed prompt, consuming that much of it
    /// when it does.
    fn absorbs_replay(&mut self, text: &str) -> bool {
        let Some(echo) = self.echo else {
            return false;
        };
        // `get` rather than an index and a slice: a replay that has matched
        // part way into a multi-byte character leaves `matched` off a char
        // boundary, and asking for the rest of the text is how that answers
        // "not a match" instead of panicking on the paint path's own state.
        let absorbed = self
            .entries
            .get(echo.entry)
            .and_then(|entry| entry.text.get(echo.matched..))
            .is_some_and(|rest| rest.starts_with(text));
        if absorbed {
            self.echo = Some(LocalEcho {
                matched: echo.matched + text.len(),
                ..echo
            });
        }
        absorbed
    }

    /// Settles what the turn leaves behind, at the point nothing is still
    /// answering it.
    ///
    /// The prompt awaiting a replay is forgotten: a chunk arriving after
    /// this is a new turn's, and matching it against the last turn's echo
    /// would drop a message the user never saw.
    ///
    /// The spinner stops too, whatever the wire last said about the calls
    /// still on screen. A turn that ended with a call unresolved -- which
    /// is every crashed session, since a dead agent sends no final status
    /// -- would otherwise keep a marker animating, and a timer re-arming
    /// behind it, for the rest of the run. Their last status stands: the
    /// panel does not invent a result the agent never reported, it stops
    /// pretending the call is still moving.
    pub fn end_turn(&mut self) {
        self.echo = None;
        self.running_calls = 0;
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
    /// work [`Transcript::rows_from`]'s cache exists to avoid, not just
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
/// per task for a plan. `spinner` is the frame an unresolved tool call's
/// marker is currently on ([`Transcript::advance_spinner`]).
///
/// Every row opens with a marker glyph in its own span, and the row's
/// meaning is carried by color and that glyph rather than by a word: a
/// panel that spells out who is speaking spends the start of every line
/// restating what a reader learns once, and the transcript is the one
/// surface here where the content is the point.
fn render_entry(entry: &TranscriptEntry, spinner: usize) -> Vec<Vec<Span>> {
    #[cfg(test)]
    RENDER_ENTRY_CALLS.with(|calls| calls.set(calls.get() + 1));
    match &entry.kind {
        TranscriptEntryKind::Message { role, .. } => {
            let (mark, style) = match role {
                TranscriptRole::User => (USER_MARK, StyleRole::AiUser),
                TranscriptRole::Agent => (AGENT_MARK, StyleRole::AiAgent),
                TranscriptRole::Thought => (THOUGHT_MARK, StyleRole::AiThought),
            };
            vec![vec![
                Span::new(mark, style),
                Span::new(entry.text.clone(), style),
            ]]
        }
        TranscriptEntryKind::ToolCall { status, result, .. } => {
            let (mark, style) = match status {
                ToolCallStatus::Pending => (TOOL_PENDING_MARK, StyleRole::AiToolRunning),
                ToolCallStatus::InProgress => (
                    SPINNER_FRAMES[spinner % SPINNER_FRAMES.len()],
                    StyleRole::AiToolRunning,
                ),
                ToolCallStatus::Completed => (TOOL_DONE_MARK, StyleRole::AiToolDone),
                ToolCallStatus::Failed => (TOOL_FAILED_MARK, StyleRole::AiToolFailed),
            };
            let mut rows = vec![vec![
                Span::new(mark, style),
                Span::plain(entry.text.clone()),
            ]];
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

    /// A row budget larger than any transcript a test builds, for the tests
    /// whose subject is what renders rather than how much of it does.
    const ROOM: usize = 1_000;

    /// One agent reply's rendered row, marker span and all.
    fn agent_row(text: &str) -> Vec<Span> {
        vec![
            Span::new(AGENT_MARK, StyleRole::AiAgent),
            Span::new(text, StyleRole::AiAgent),
        ]
    }

    #[test]
    fn reasoning_renders_apart_from_the_reply_it_shares_an_id_with() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(Some("m1"), "weighing it", TranscriptRole::Thought);
        transcript.append_or_extend(Some("m1"), "the answer", TranscriptRole::Agent);

        assert_eq!(
            transcript.rows_from(TranscriptAnchor::default(), ROOM),
            vec![
                vec![
                    Span::new(THOUGHT_MARK, StyleRole::AiThought),
                    Span::new("weighing it", StyleRole::AiThought),
                ],
                vec![
                    Span::new(AGENT_MARK, StyleRole::AiAgent),
                    Span::new("the answer", StyleRole::AiAgent),
                ],
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
            transcript
                .rows_from(TranscriptAnchor::default(), ROOM)
                .len(),
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

    /// A transcript of `lines` one-row agent messages, `Agent: line 0`
    /// upward -- long enough that no window shows all of it.
    fn long_transcript(lines: usize) -> Transcript {
        let mut transcript = Transcript::new();
        for i in 0..lines {
            transcript.append_or_extend(
                Some(&format!("m{i}")),
                &format!("line {i}"),
                TranscriptRole::Agent,
            );
        }
        transcript
    }

    fn texts(rows: &[Vec<Span>]) -> Vec<String> {
        rows.iter()
            .map(|row| row.iter().map(|span| span.text.clone()).collect())
            .collect()
    }

    /// The window a panel paints by default ends on the newest row. A
    /// transcript taller than the panel otherwise shows its opening
    /// screenful and nothing else for the rest of the session.
    #[test]
    fn the_tail_window_ends_on_the_newest_row() {
        let transcript = long_transcript(50);
        let rows = transcript.rows_from(transcript.tail_anchor(5), 5);

        assert_eq!(
            texts(&rows),
            vec![
                "● line 45",
                "● line 46",
                "● line 47",
                "● line 48",
                "● line 49",
            ]
        );
    }

    /// An anchor is an entry plus an offset into it, so a window can start
    /// part way down a single entry -- the one shape that can be taller
    /// than the panel on its own, and so the one that a per-entry anchor
    /// would make impossible to scroll through.
    #[test]
    fn a_window_can_start_part_way_down_one_tall_entry() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "t1".to_string(),
            "grep".to_string(),
            ToolCallStatus::Completed,
            Some((0..9).map(|i| format!("hit {i}")).collect()),
        );

        let rows = transcript.rows_from(transcript.tail_anchor(3), 3);

        assert_eq!(texts(&rows), vec!["  hit 6", "  hit 7", "  hit 8"]);
    }

    /// Scrolling back and forward again by the same distance lands on the
    /// row it started from: the two walks must agree about what a row is,
    /// or a page down after a page up leaves the reader somewhere they
    /// never chose.
    #[test]
    fn scrolling_back_then_forward_returns_to_the_same_row() {
        let transcript = long_transcript(50);
        let tail = transcript.tail_anchor(10);

        let back = transcript.scrolled_back(tail, 17);
        assert_eq!(texts(&transcript.rows_from(back, 1)), vec!["● line 23"]);
        assert_eq!(transcript.scrolled_forward(back, 17), tail);
    }

    /// Neither walk runs off its end: a page up from the first row and a
    /// page down from the last both stop rather than naming an entry that
    /// does not exist.
    #[test]
    fn the_walks_stop_at_the_transcripts_own_ends() {
        let transcript = long_transcript(4);
        let start = TranscriptAnchor::default();

        assert_eq!(transcript.scrolled_back(start, 1_000), start);
        assert_eq!(
            transcript.scrolled_forward(start, 1_000),
            TranscriptAnchor { entry: 4, row: 0 }
        );
        assert_eq!(
            transcript.tail_anchor(1_000),
            start,
            "a transcript shorter than the window starts at its first row"
        );
    }

    /// The same "a frame costs the window, not the session" bar the
    /// oldest-first window already holds, from the end a panel actually
    /// paints: following the tail must not render the history behind it.
    #[test]
    fn following_the_tail_costs_a_frame_no_more_than_a_short_session() {
        let transcript = long_transcript(2_000);

        reset_render_entry_calls();
        let window = 20;
        let rows = transcript.rows_from(transcript.tail_anchor(window), window);

        assert_eq!(rows.len(), window);
        assert!(
            render_entry_calls() <= window + 1,
            "a frame must not render entries no window can show: {} entries \
             rendered for a {window}-row window",
            render_entry_calls()
        );
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
    /// cannot tell a real cache from `rows_from` bypassing it and
    /// calling `render_entry` unconditionally. `render_entry_calls()` is
    /// the observable that distinguishes them.
    #[test]
    fn a_render_only_recomputes_the_entry_that_changed() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(Some("m1"), "one", TranscriptRole::Agent);
        transcript.append_or_extend(Some("m2"), "two", TranscriptRole::Agent);

        reset_render_entry_calls();
        let first_pass = transcript.rows_from(TranscriptAnchor::default(), ROOM);
        assert_eq!(first_pass.len(), 2);
        assert_eq!(
            render_entry_calls(),
            2,
            "both entries are cold on the first call, so both must render once"
        );

        reset_render_entry_calls();
        let repeat_pass = transcript.rows_from(TranscriptAnchor::default(), ROOM);
        assert_eq!(repeat_pass, first_pass);
        assert_eq!(
            render_entry_calls(),
            0,
            "nothing changed since the last call: a real cache hit renders \
             neither entry again, where a bypassed cache would render both"
        );

        transcript.append_or_extend(Some("m2"), " more", TranscriptRole::Agent);
        reset_render_entry_calls();
        let second_pass = transcript.rows_from(TranscriptAnchor::default(), ROOM);
        assert_eq!(
            second_pass[0], first_pass[0],
            "the untouched entry's cached row must be reused byte for byte"
        );
        assert_eq!(
            second_pass[1],
            agent_row("two more"),
            "the touched entry's row must reflect the fold"
        );
        assert_eq!(
            render_entry_calls(),
            1,
            "only the entry the fold touched should re-render -- a bypassed \
             cache would render both entries again here"
        );
    }

    /// Pins the paint path's cost to the window rather than the session:
    /// asserting only the rows returned would pass with the whole transcript
    /// rendered and then thrown away, so this asserts the rendering too.
    #[test]
    fn a_long_session_costs_a_frame_no_more_than_a_short_one() {
        let mut transcript = Transcript::new();
        for i in 0..2_000 {
            transcript.append_or_extend(Some(&format!("m{i}")), "hello", TranscriptRole::Agent);
        }

        reset_render_entry_calls();
        let window = 20;
        let rows = transcript.rows_from(TranscriptAnchor::default(), window);

        assert_eq!(
            rows.len(),
            window,
            "a frame paints its window, not a history"
        );
        assert_eq!(
            rows[0],
            agent_row("hello"),
            "the window starts where the panel's own window starts"
        );
        assert!(
            render_entry_calls() <= window,
            "a frame must not render entries no window can show: {} entries \
             rendered for a {window}-row window",
            render_entry_calls()
        );
    }

    /// The echo is the panel's own copy of the prompt, so an adapter that
    /// also replays it must not put a second copy of the same sentence on
    /// screen -- however many chunks it takes to say it.
    #[test]
    fn a_replayed_prompt_folds_into_the_echo_rather_than_repeating_it() {
        let mut transcript = Transcript::new();
        transcript.echo_user_prompt("fix the retry policy");

        transcript.append_user_chunk(Some("wire-1"), "fix the ");
        transcript.append_user_chunk(Some("wire-1"), "retry policy");

        assert_eq!(transcript.len(), 1, "one prompt, said once");
        assert_eq!(
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM)),
            vec!["\u{276f} fix the retry policy"]
        );
    }

    /// The other direction, and the reason the match is a prefix check
    /// rather than an unconditional drop: an adapter that injects context
    /// of its own into the user's turn has said something the user has
    /// never seen, and a panel that swallowed it would be hiding what the
    /// agent was actually asked.
    #[test]
    fn a_user_chunk_that_is_not_the_echo_still_appends() {
        let mut transcript = Transcript::new();
        transcript.echo_user_prompt("fix the retry policy");

        transcript.append_user_chunk(Some("wire-1"), "<context: 3 files>");

        assert_eq!(transcript.len(), 2);
        assert_eq!(
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM)),
            vec![
                "\u{276f} fix the retry policy",
                "\u{276f} <context: 3 files>"
            ]
        );
    }

    /// A turn's echo dies with the turn: the next turn's chunks are matched
    /// against that turn's echo or against nothing at all, never against a
    /// prompt two turns old that happens to start the same way.
    #[test]
    fn a_replay_arriving_after_the_turn_ended_is_shown_rather_than_absorbed() {
        let mut transcript = Transcript::new();
        transcript.echo_user_prompt("run the tests");
        transcript.end_turn();

        transcript.append_user_chunk(Some("wire-1"), "run the tests");

        assert_eq!(transcript.len(), 2);
    }

    /// Two prompts in one session are two entries, not one folded pair: a
    /// reused id would extend the first prompt with the second one's text.
    #[test]
    fn a_second_prompt_gets_its_own_entry() {
        let mut transcript = Transcript::new();
        transcript.echo_user_prompt("first");
        transcript.end_turn();
        transcript.echo_user_prompt("second");

        assert_eq!(transcript.len(), 2);
        assert_eq!(
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM)),
            vec!["\u{276f} first", "\u{276f} second"]
        );
    }

    /// The spinner is the one thing on the panel that repaints on a timer,
    /// so it must cost exactly the entries that are spinning: a tick that
    /// dropped every cached row would turn a running tool call into a
    /// whole-transcript re-render eight times a second.
    #[test]
    fn a_spinner_tick_re_renders_only_the_calls_still_running() {
        let mut transcript = Transcript::new();
        for i in 0..20 {
            transcript.append_or_extend(Some(&format!("m{i}")), "chatter", TranscriptRole::Agent);
        }
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        let _ = transcript.rows_from(TranscriptAnchor::default(), ROOM);

        reset_render_entry_calls();
        assert!(transcript.advance_spinner());
        let _ = transcript.rows_from(TranscriptAnchor::default(), ROOM);

        assert_eq!(
            render_entry_calls(),
            1,
            "a tick must re-render the running call and nothing else"
        );
    }

    /// A call the agent never resolved -- a crashed session leaves one on
    /// every turn it was mid-way through -- must not leave a marker
    /// animating and a timer re-arming behind it for the rest of the run.
    #[test]
    fn a_turn_that_ends_mid_call_stops_the_spinner() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        assert!(transcript.arm_spinner());

        transcript.end_turn();

        assert!(!transcript.advance_spinner());
        assert!(!transcript.arm_spinner());
    }

    /// Nothing running means nothing to animate, and the caller reads that
    /// answer to stop re-arming the timer.
    #[test]
    fn a_transcript_with_no_call_in_flight_neither_arms_nor_advances() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(Some("m1"), "hello", TranscriptRole::Agent);
        assert!(!transcript.arm_spinner());
        assert!(!transcript.advance_spinner());

        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Completed,
            None,
        );
        assert!(
            !transcript.arm_spinner(),
            "a call that arrived already finished never spins"
        );
    }

    /// Two calls in flight share one timer, and it keeps running until the
    /// last of them resolves -- an arm-per-call would leave a second timer
    /// ticking against a panel with nothing moving on it.
    #[test]
    fn the_spinner_stops_only_once_every_call_has_resolved() {
        let mut transcript = Transcript::new();
        for id in ["call_1", "call_2"] {
            transcript.upsert_tool_call(
                id.to_string(),
                "Read file".to_string(),
                ToolCallStatus::InProgress,
                None,
            );
        }
        assert!(transcript.arm_spinner());
        assert!(!transcript.arm_spinner());

        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Completed,
            None,
        );
        assert!(
            transcript.advance_spinner(),
            "the second call is still running"
        );

        transcript.upsert_tool_call(
            "call_2".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Failed,
            None,
        );
        assert!(!transcript.advance_spinner());

        transcript.upsert_tool_call(
            "call_3".to_string(),
            "Write file".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        assert!(
            transcript.arm_spinner(),
            "the armed slot is handed back, so a later call gets its own timer"
        );
    }
}
