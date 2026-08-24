//! The panel's transcript: the streamed messages, tool calls, and plan it
//! has folded so far, the folding rules that keep a chatty agent from
//! growing one row per wire chunk, and the per-entry render cache that
//! keeps a paint proportional to what changed since the last one.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::wrap;
use crate::native::ai_event::{PlanEntry, PlanEntryStatus, ToolCallStatus};
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
/// The glyph opening a line view itself wrote into the transcript.
const NOTICE_MARK: &str = "\u{203c} ";
/// The glyph opening what became of a diff review.
const REVIEW_MARK: &str = "\u{00b1} ";
/// The glyph a tool call that has not started yet opens with.
const TOOL_PENDING_MARK: &str = "\u{b7} ";
/// The glyph a completed tool call opens with.
const TOOL_DONE_MARK: &str = "\u{2713} ";
/// The glyph a failed tool call opens with.
const TOOL_FAILED_MARK: &str = "\u{2717} ";
/// The glyph the plan's current task opens with. A plan task is a tool
/// call's twin -- something the agent has not done, is doing, or has done
/// -- so the two share the marker vocabulary at either end, and this is
/// the one state a task can be in that a tool call cannot: under way, but
/// not a call this panel is animating a spinner for.
const PLAN_ACTIVE_MARK: &str = "\u{25b8} ";

/// The indent standing under an entry's marker on every row that entry
/// wrapped onto, so a wrapped entry reads as one item rather than as a new
/// one per row -- the convention a wrapped composer prompt already follows
/// (`view-surface`'s `composer_lines`, indenting by `super::PROMPT_COLS`).
///
/// It is also the lead a tool call's result lines carry, which is why it is
/// the indent and not a second copy of the marker: a continuation row that
/// repeated the glyph would read as another entry.
const MARK_INDENT: &str = "  ";

/// The columns [`MARK_INDENT`] and every marker above it take. Each marker
/// is one glyph and the space after it, and every glyph among them is one
/// display cell wide, so the two agree by construction.
const MARK_COLS: usize = MARK_INDENT.len();

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
    /// view itself, speaking in the transcript about the conversation --
    /// today, a permission request a standing grant answered without
    /// asking.
    ///
    /// Its own role for the same reason [`Self::Thought`] is one: a line
    /// view wrote is not the agent talking, and attributing an answer view
    /// gave on the user's behalf to either of them is the opposite of the
    /// audit trail it exists to be.
    Notice,
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
    /// What became of one diff review: which hunks were accepted and
    /// rejected in which file, or that the proposal was dismissed. The
    /// sentence itself is the entry's own `text`.
    ///
    /// Its own kind rather than a [`Self::Message`] in
    /// [`TranscriptRole::Agent`]'s voice, for the reason that role's doc
    /// gives: the agent proposed the diff, it did not decide it, and a
    /// record of the user's decision attributed to the agent is the
    /// opposite of the audit trail this log exists to be.
    Review,
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
    /// The row width every slot in `row_cache` was wrapped to. A panel that
    /// changed width breaks its rows in a different column, so a slot
    /// cached at the old one holds text at the wrong place and a row count
    /// no anchor arithmetic should be done against; `reflow` drops the lot
    /// on the first read at a new width. One width for the whole cache
    /// rather than one per slot: every entry is wrapped to the same panel,
    /// so a mismatch is never partial.
    cache_width: Cell<usize>,
    /// The prompt this panel echoed locally that a replaying adapter may
    /// still restate over the wire (see [`Transcript::echo_user_prompt`]).
    echo: Option<LocalEcho>,
    /// How many locally minted message ids have been handed out, so a
    /// second prompt in one session never reuses the first one's id and
    /// folds into the entry it already wrote.
    local_seq: u64,
    /// Which entries are animating: the one authority on that, held rather
    /// than derived from entry status, so that the renderer and the tick
    /// agree by construction. A call the wire left unresolved when its turn
    /// ended stays `InProgress` -- the panel invents no result the agent
    /// never reported -- but it leaves this set, so it neither animates nor
    /// resumes animating behind a later call. Membership rather than a
    /// count, so the tick's work is the number of markers moving instead of
    /// the length of the session.
    animating: HashSet<usize>,
    /// Which [`SPINNER_FRAMES`] entry an animating tool call currently
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
/// any caller should be able to observe. The spinner's frame and the set
/// of markers it is moving are excluded on the same terms -- which eighth
/// of a second a running call's glyph is on says nothing about what the
/// transcript holds -- while the pending local echo is included, because
/// whether a replay
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
    ///
    /// `width` is the cells one row has (`super::transcript_width`), which
    /// every entry is wrapped to -- so how many rows an entry is depends on
    /// the panel, and the anchor arithmetic below has to be asked at the
    /// same width the paint will use.
    #[must_use]
    pub fn rows_from(
        &self,
        anchor: TranscriptAnchor,
        budget: usize,
        width: usize,
    ) -> Vec<Vec<Span>> {
        let mut cache = self.row_cache.borrow_mut();
        self.reflow(&mut cache, width);
        let mut rows = Vec::new();
        let mut skip = anchor.row;
        for i in anchor.entry..self.entries.len() {
            if rows.len() >= budget {
                break;
            }
            let entry_rows = cache[i]
                .get_or_insert_with(|| render_entry(&self.entries[i], self.frame_at(i), width));
            rows.extend(entry_rows.iter().skip(skip).cloned());
            skip = 0;
        }
        rows.truncate(budget);
        rows
    }

    /// Empties every cache slot when `width` is not the width they hold rows
    /// for, so a resized panel renders its entries again at the column it
    /// now breaks at instead of painting yesterday's wrap.
    fn reflow(&self, cache: &mut [Option<Vec<Vec<Span>>>], width: usize) {
        if self.cache_width.get() == width {
            return;
        }
        self.cache_width.set(width);
        for slot in cache.iter_mut() {
            *slot = None;
        }
    }

    /// `anchor` with its row brought back inside its entry at `width`.
    ///
    /// An anchor holds a row count, and how many rows an entry is depends on
    /// the width it wrapped to -- so a window held across a resize can name
    /// a row its own entry no longer has, which is the one way an anchor
    /// comes to break the invariant [`TranscriptAnchor`] documents. It is
    /// clamped onto that entry rather than spilled forward onto whatever
    /// entry the old count now reaches: the reader was reading *this*
    /// entry, and a resize is not a scroll.
    ///
    /// An anchor past the last entry is left alone -- that is the one
    /// anchor the invariant admits.
    #[must_use]
    pub fn normalized(&self, anchor: TranscriptAnchor, width: usize) -> TranscriptAnchor {
        let mut cache = self.row_cache.borrow_mut();
        self.reflow(&mut cache, width);
        let Some(slot) = cache.get_mut(anchor.entry) else {
            return anchor;
        };
        let rows = slot
            .get_or_insert_with(|| {
                render_entry(
                    &self.entries[anchor.entry],
                    self.frame_at(anchor.entry),
                    width,
                )
            })
            .len();
        TranscriptAnchor {
            entry: anchor.entry,
            row: anchor.row.min(rows.saturating_sub(1)),
        }
    }

    /// The anchor a window `viewport` rows tall starts at when its last row
    /// is the transcript's newest -- what a panel following the tail paints
    /// from, recomputed per frame so that a chunk streaming in moves the
    /// window rather than scrolling out from under it.
    #[must_use]
    pub fn tail_anchor(&self, viewport: usize, width: usize) -> TranscriptAnchor {
        self.scrolled_back(
            TranscriptAnchor {
                entry: self.entries.len(),
                row: 0,
            },
            viewport,
            width,
        )
    }

    /// `anchor` moved `rows` rendered rows toward the oldest entry, stopping
    /// at the transcript's first row.
    ///
    /// Walks entries backwards rather than measuring the whole transcript:
    /// the cost is the distance asked for, not the session's length, so a
    /// keypress in an hours-long session costs a screenful of work.
    #[must_use]
    pub fn scrolled_back(
        &self,
        anchor: TranscriptAnchor,
        rows: usize,
        width: usize,
    ) -> TranscriptAnchor {
        let mut cache = self.row_cache.borrow_mut();
        self.reflow(&mut cache, width);
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
                .get_or_insert_with(|| {
                    render_entry(&self.entries[entry], self.frame_at(entry), width)
                })
                .len();
        }
        TranscriptAnchor { entry, row }
    }

    /// `anchor` moved `rows` rendered rows toward the newest entry, stopping
    /// one past the last entry -- the position [`Self::tail_anchor`] is
    /// compared against to tell a window that has caught up with the tail
    /// from one still held behind it.
    #[must_use]
    pub fn scrolled_forward(
        &self,
        anchor: TranscriptAnchor,
        rows: usize,
        width: usize,
    ) -> TranscriptAnchor {
        let mut cache = self.row_cache.borrow_mut();
        self.reflow(&mut cache, width);
        let mut left = rows;
        let mut entry = anchor.entry;
        let mut row = anchor.row;
        while left > 0 && entry < self.entries.len() {
            let below = cache[entry]
                .get_or_insert_with(|| {
                    render_entry(&self.entries[entry], self.frame_at(entry), width)
                })
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
            self.note_running(i, status == ToolCallStatus::InProgress);
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
        let at = self.entries.len() - 1;
        self.note_running(at, status == ToolCallStatus::InProgress);
        self.tool_call_index.insert(tool_call_id, at);
    }

    /// Enrols entry `at` in the animating set, or takes it out.
    ///
    /// Idempotent in both directions rather than a delta on a counter: a
    /// recovered session is free to restate `in_progress` on a call it
    /// already announced, and a set that only moved on transitions would
    /// read that restatement as "no change" and leave the marker frozen.
    fn note_running(&mut self, at: usize, running: bool) {
        if running {
            self.animating.insert(at);
        } else {
            self.animating.remove(&at);
        }
    }

    /// Whether any marker is currently animating -- what the caller arms
    /// its next frame's wakeup on, and stops arming when it goes false.
    ///
    /// The caller owns the clock; `view-core` has none.
    #[must_use]
    pub fn is_spinning(&self) -> bool {
        !self.animating.is_empty()
    }

    /// Moves every animating marker on to the next spinner frame.
    ///
    /// Only those entries drop their cached rows, so the frame a tick
    /// repaints is the markers' own rows and nothing else on the panel --
    /// and the work is the number of calls in flight, never the length of
    /// the session.
    pub fn advance_spinner(&mut self) {
        self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
        let cache = self.row_cache.get_mut();
        for &i in &self.animating {
            cache[i] = None;
        }
    }

    /// Which spinner frame entry `at` paints, or `None` when it is not one
    /// of the markers moving.
    fn frame_at(&self, at: usize) -> Option<usize> {
        self.animating.contains(&at).then_some(self.spinner_frame)
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
    ///
    /// A replay already under way that is interrupted by something else
    /// ends the window: once a chunk of the prompt has been restated, the
    /// next chunk either continues it or the adapter is saying something
    /// of its own, and treating a later chunk as the prompt's remainder
    /// would swallow part of that other message. Nothing matched yet is
    /// not an interruption -- an adapter free to inject its own context
    /// ahead of the prompt is still owed the dedupe when the prompt
    /// arrives.
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
        } else if echo.matched > 0 {
            self.echo = None;
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
    /// -- would otherwise keep a marker animating, and a wakeup armed
    /// behind it, for the rest of the run. Their last status stands: the
    /// panel does not invent a result the agent never reported, it stops
    /// pretending the call is still moving, and the marker it settles on is
    /// the static one an unstarted call wears.
    pub fn end_turn(&mut self) {
        self.echo = None;
        let cache = self.row_cache.get_mut();
        for i in self.animating.drain() {
            cache[i] = None;
        }
    }

    /// Records what became of one diff review (see
    /// [`TranscriptEntryKind::Review`]).
    ///
    /// Always a new entry, never folded into anything: each review is one
    /// decision the user made, and a session that reviewed the same file
    /// twice owes two lines.
    pub fn record_review(&mut self, text: String) {
        self.entries.push(TranscriptEntry {
            kind: TranscriptEntryKind::Review,
            text,
        });
        self.row_cache.get_mut().push(None);
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

/// One transcript entry's rendered rows, wrapped to a `width`-cell row: a
/// message's text, a tool call's title followed by its result lines, or one
/// task per plan entry -- each of them as many rows as its own text and the
/// panel's width work out to. `spinner` is the frame this entry's marker is
/// currently on ([`Transcript::advance_spinner`]), or `None` for an entry
/// that is not animating -- including a call the wire left unresolved,
/// whose marker holds still rather than claiming work nobody is doing.
///
/// Every entry opens with a marker glyph in its own span, and the row's
/// meaning is carried by color and that glyph rather than by a word: a
/// panel that spells out who is speaking spends the start of every line
/// restating what a reader learns once, and the transcript is the one
/// surface here where the content is the point.
fn render_entry(entry: &TranscriptEntry, spinner: Option<usize>, width: usize) -> Vec<Vec<Span>> {
    #[cfg(test)]
    RENDER_ENTRY_CALLS.with(|calls| calls.set(calls.get() + 1));
    let mut paint = EntryRows::new(width);
    match &entry.kind {
        TranscriptEntryKind::Message { role, .. } => {
            let (mark, style) = match role {
                TranscriptRole::User => (USER_MARK, StyleRole::AiUser),
                TranscriptRole::Agent => (AGENT_MARK, StyleRole::AiAgent),
                TranscriptRole::Thought => (THOUGHT_MARK, StyleRole::AiThought),
                TranscriptRole::Notice => (NOTICE_MARK, StyleRole::AiNotice),
            };
            paint.add(Span::new(mark, style), Some(style), &entry.text);
        }
        TranscriptEntryKind::ToolCall { status, result, .. } => {
            let (mark, style) = match status {
                ToolCallStatus::Pending => (TOOL_PENDING_MARK, StyleRole::AiToolRunning),
                ToolCallStatus::InProgress => (
                    spinner.map_or(TOOL_PENDING_MARK, |frame| {
                        SPINNER_FRAMES[frame % SPINNER_FRAMES.len()]
                    }),
                    StyleRole::AiToolRunning,
                ),
                ToolCallStatus::Completed => (TOOL_DONE_MARK, StyleRole::AiToolDone),
                ToolCallStatus::Failed => (TOOL_FAILED_MARK, StyleRole::AiToolFailed),
            };
            paint.add(Span::new(mark, style), None, &entry.text);
            for line in result {
                paint.add(Span::plain(MARK_INDENT), None, line);
            }
        }
        TranscriptEntryKind::Review => paint.add(
            Span::new(REVIEW_MARK, StyleRole::AiNotice),
            Some(StyleRole::AiNotice),
            &entry.text,
        ),
        TranscriptEntryKind::Plan { entries } => {
            for e in entries {
                let (mark, style) = match e.status {
                    PlanEntryStatus::Pending => (TOOL_PENDING_MARK, StyleRole::AiToolRunning),
                    PlanEntryStatus::InProgress => (PLAN_ACTIVE_MARK, StyleRole::AiAgent),
                    PlanEntryStatus::Completed => (TOOL_DONE_MARK, StyleRole::AiToolDone),
                };
                paint.add(Span::new(mark, style), None, &e.content);
            }
        }
    }
    paint.finish()
}

/// One entry's rows as they are built, and what is left of the entry's own
/// [`ROW_PAINT_CEILING`] byte budget.
///
/// The budget spans the whole entry rather than each piece of text in it,
/// because the entry is what the cache holds and what an anchor counts rows
/// of: a tool call answering with ten thousand result lines is one entry,
/// and a per-line cut would leave it painting ten thousand rows.
struct EntryRows {
    rows: Vec<Vec<Span>>,
    /// Bytes of text the entry may still render.
    left: usize,
    /// Cells one row has for text after its marker.
    body: usize,
    /// Whether the ceiling has already cut this entry short, which stops
    /// every later piece of it and is what [`Self::finish`] closes the
    /// entry with a notice for.
    cut: bool,
}

impl EntryRows {
    fn new(width: usize) -> Self {
        Self {
            rows: Vec::new(),
            left: ROW_PAINT_CEILING,
            // A panel too narrow for even one cell of text would otherwise
            // open a row per character; the floor spends a column the frame
            // clips anyway rather than the entry's whole row budget.
            body: width.saturating_sub(MARK_COLS).max(1),
            cut: false,
        }
    }

    /// Adds `text` under `mark`, breaking it at its own newlines and then at
    /// the row's width, with [`MARK_INDENT`] standing under `mark` on every
    /// row after the first.
    ///
    /// Wrapped rather than clipped, and through
    /// [`super::wrap`](super::wrap) rather than a second measure of its
    /// own: a prompt is composed against that wrap, so anything else here
    /// would reflow the user's own text the moment they sent it. Trailing
    /// newlines come off because an entry is a log line and not a terminal
    /// -- an agent ending its reply with one owes no blank row -- while
    /// newlines inside it are the shape the writer gave the text and are
    /// kept, blank lines and all.
    fn add(&mut self, mark: Span, body_style: Option<StyleRole>, text: &str) {
        if self.cut {
            return;
        }
        let capped = cut_to(text, self.left);
        self.left -= capped.len();
        self.cut = capped.len() < text.len();
        // Nothing of this piece survived the cut, so it gets no marker row
        // of its own -- an entry ends on its last readable row and then the
        // notice, not on an empty marker.
        if capped.is_empty() && !text.is_empty() {
            return;
        }
        let opened = self.rows.len();
        for line in capped.trim_end_matches(['\n', '\r']).split('\n') {
            // `usize::MAX` keeps every row: the wrap's tail-keeping cut
            // exists for a composer whose newest row is the interesting
            // one, and a log read oldest first wants the opposite end.
            for row in wrap(line.trim_end_matches('\r'), self.body, usize::MAX) {
                let lead = if self.rows.len() == opened {
                    mark.clone()
                } else {
                    Span::plain(MARK_INDENT)
                };
                let body = match body_style {
                    Some(role) => Span::new(row, role),
                    None => Span::plain(row),
                };
                self.rows.push(vec![lead, body]);
            }
        }
    }

    /// The entry's rows, closed with [`ENTRY_CUT`] when the ceiling stopped
    /// it short.
    fn finish(mut self) -> Vec<Vec<Span>> {
        if self.cut {
            self.rows.push(vec![Span::plain(ENTRY_CUT)]);
        }
        self.rows
    }
}

/// The row closing an entry the ceiling cut, so an entry that stops
/// mid-sentence reads as cut rather than as all there was.
const ENTRY_CUT: &str = "-- cut here, the rest of this entry is too long to paint --";

/// The most of one entry's text that renders, in bytes, cut at a char
/// boundary.
///
/// An entry is wrapped, so every byte it holds does reach a row and the
/// ceiling is what stops one entry becoming a session's worth of them: a
/// submitted megabyte (a paste into the composer is the easy way to make
/// one) would otherwise be tens of thousands of rendered rows, rebuilt
/// whole every time a chunk folds into that entry. Bytes rather than rows
/// because it also bounds the wrap's own walk, and one byte is at most one
/// row -- so this ceiling is a row ceiling too.
const ROW_PAINT_CEILING: usize = 8 << 10;

/// `text` cut to `max` bytes at a char boundary.
fn cut_to(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::native::ai_event::PlanEntryPriority;

    /// A row budget larger than any transcript a test builds, for the tests
    /// whose subject is what renders rather than how much of it does.
    const ROOM: usize = 1_000;

    /// A row wide enough that none of the short lines these tests write
    /// wrap, so a test naming the rows it expects gets one per line. The
    /// wrap itself is the subject of the tests that name their own width.
    const WIDE: usize = 60;

    /// Every body span of `rows` run back together -- the text an entry
    /// painted, with its markers and indents taken back off.
    fn body(rows: &[Vec<Span>]) -> String {
        rows.iter()
            .filter_map(|row| row.get(1))
            .map(|span| span.text.as_str())
            .collect()
    }

    /// A submitted paste puts more text in one entry than a panel could
    /// ever paint, and every row of it is cloned out of the cache on each
    /// frame: what the entry carries stops at the ceiling, on a char
    /// boundary, and the cut is a suffix -- the opening of what was pasted,
    /// which is where a reader starts.
    #[test]
    fn a_paste_longer_than_the_ceiling_carries_only_what_the_ceiling_allows() {
        let mut transcript = Transcript::new();
        let pasted = "\u{2026}".repeat(ROW_PAINT_CEILING);
        transcript.append_or_extend(None, &pasted, TranscriptRole::User);

        let rows = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);
        let painted = body(&rows[..rows.len() - 1]);

        assert_eq!(painted.len(), 8_190, "cut back to the char boundary");
        assert!(pasted.starts_with(painted.as_str()));
        assert_eq!(
            rows.last().map(|row| row[0].text.as_str()),
            Some(ENTRY_CUT),
            "an entry that stops mid-paste must say so, or it reads as all \
             there was"
        );
    }

    /// The ceiling is the whole entry's, not each piece of text in it: a
    /// tool call answering with more result lines than a session could read
    /// is one entry, and an anchor counts its rows.
    #[test]
    fn a_tool_call_answering_with_a_file_stops_at_the_entrys_own_ceiling() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "t1".to_string(),
            "cat".to_string(),
            ToolCallStatus::Completed,
            Some((0..10_000).map(|i| format!("line {i}")).collect()),
        );

        let rows = transcript.rows_from(TranscriptAnchor::default(), usize::MAX, WIDE);

        assert!(
            rows.len() <= ROW_PAINT_CEILING,
            "one entry painted {} rows",
            rows.len()
        );
        assert_eq!(rows.last().map(|row| row[0].text.as_str()), Some(ENTRY_CUT));
    }

    /// The budget running out exactly on a result line's last byte is still
    /// a cut: the line after it has nowhere to go, and an entry that closed
    /// on an empty marker row instead would show a line it never painted.
    #[test]
    fn a_result_line_that_lands_exactly_on_the_ceiling_still_reads_as_cut() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "t1".to_string(),
            "x".to_string(),
            ToolCallStatus::Completed,
            Some(vec![
                "a".repeat(ROW_PAINT_CEILING - 1),
                "dropped".to_string(),
            ]),
        );

        let rows = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);

        assert_eq!(rows.last().map(|row| row[0].text.as_str()), Some(ENTRY_CUT));
        assert!(
            !body(&rows).contains("dropped"),
            "the line past the ceiling is not painted"
        );
        assert!(
            rows[rows.len() - 2][1].text.starts_with('a'),
            "the cut closes the last row that had text, not an empty marker: \
             {:?}",
            rows[rows.len() - 2]
        );
    }

    /// The reproduced complaint: a prompt written over several lines came
    /// back as one row with its newlines flattened to spaces and everything
    /// past the panel's width gone. It keeps its own shape instead --
    /// blank lines and all, since a blank line between paragraphs is text
    /// the writer wrote.
    #[test]
    fn a_multi_line_prompt_keeps_its_own_lines() {
        let mut transcript = Transcript::new();
        transcript.echo_user_prompt("fix the parser\n\n- it drops tabs\n- and CRs\n");

        assert_eq!(
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE)),
            vec![
                "\u{276f} fix the parser",
                "  ",
                "  - it drops tabs",
                "  - and CRs",
            ],
            "the trailing newline owes no row of its own, the blank line \
             between the paragraphs does"
        );
    }

    /// A line past the row's width wraps rather than being clipped, and the
    /// rows it wraps onto stand under the marker's own indent -- one entry,
    /// not a new one per row.
    #[test]
    fn a_line_past_the_rows_width_wraps_under_the_marker() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(None, "abcdefghijklmnopqrstuvwxyz", TranscriptRole::Agent);

        // 12 = MARK_COLS plus the ten cells a body row is left with
        assert_eq!(
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM, 12)),
            vec!["\u{25cf} abcdefghij", "  klmnopqrst", "  uvwxyz"],
            "the break is the composer's own cell wrap, not a word wrap"
        );
    }

    /// A panel resized mid-session re-wraps what is already on screen: rows
    /// cached at the old width break in a column the frame no longer has.
    #[test]
    fn a_narrowed_panel_rewraps_the_rows_it_already_rendered() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(None, "abcdefghijkl", TranscriptRole::Agent);

        assert_eq!(
            transcript
                .rows_from(TranscriptAnchor::default(), ROOM, WIDE)
                .len(),
            1
        );
        assert_eq!(
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM, 8)),
            vec!["\u{25cf} abcdef", "  ghijkl"],
            "a cached row is text at a column the narrowed panel no longer \
             paints"
        );
    }

    /// An anchor taken while the panel was narrow holds a row count its own
    /// entry no longer has once the panel is widened. It is clamped back
    /// onto that entry, so the window still opens on the entry the reader
    /// was reading -- an anchor naming a row nothing renders would drop the
    /// entry from the window instead.
    #[test]
    fn an_anchor_held_across_a_resize_is_clamped_onto_its_own_entry() {
        let mut transcript = Transcript::new();
        for i in 0..4 {
            transcript.append_or_extend(
                Some(&format!("m{i}")),
                "abcdefghijkl",
                TranscriptRole::Agent,
            );
        }
        // 8 cells a row leaves 6 for text, so each entry is two rows there
        // and one at WIDE
        let narrow = TranscriptAnchor { entry: 2, row: 1 };
        assert_eq!(transcript.normalized(narrow, 8), narrow, "already inside");

        assert_eq!(
            transcript.normalized(narrow, WIDE),
            TranscriptAnchor { entry: 2, row: 0 },
            "the reader stays on the entry they were reading, at its first \
             row -- a resize is not a scroll"
        );
        assert_eq!(
            transcript.normalized(TranscriptAnchor { entry: 4, row: 0 }, WIDE),
            TranscriptAnchor { entry: 4, row: 0 },
            "the one anchor past the last entry is the invariant's own \
             exception and is left alone"
        );
    }

    /// A carriage return at a line's end is the writer's line ending, not a
    /// cell of its own: left in, the frame paints it as a trailing space and
    /// the row measures one cell wider than the text it shows.
    #[test]
    fn a_crlf_line_ending_costs_no_cell_of_its_own() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(None, "first\r\nsecond\r\n", TranscriptRole::User);

        assert_eq!(
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE)),
            vec!["\u{276f} first", "  second"]
        );
    }

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
            transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE),
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
                .rows_from(TranscriptAnchor::default(), ROOM, WIDE)
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

    /// A transcript of `lines` one-row agent messages, `line 0`
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
        let rows = transcript.rows_from(transcript.tail_anchor(5, WIDE), 5, WIDE);

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

        let rows = transcript.rows_from(transcript.tail_anchor(3, WIDE), 3, WIDE);

        assert_eq!(texts(&rows), vec!["  hit 6", "  hit 7", "  hit 8"]);
    }

    /// Scrolling back and forward again by the same distance lands on the
    /// row it started from: the two walks must agree about what a row is,
    /// or a page down after a page up leaves the reader somewhere they
    /// never chose.
    #[test]
    fn scrolling_back_then_forward_returns_to_the_same_row() {
        let transcript = long_transcript(50);
        let tail = transcript.tail_anchor(10, WIDE);

        let back = transcript.scrolled_back(tail, 17, WIDE);
        assert_eq!(
            texts(&transcript.rows_from(back, 1, WIDE)),
            vec!["● line 23"]
        );
        assert_eq!(transcript.scrolled_forward(back, 17, WIDE), tail);
    }

    /// Neither walk runs off its end: a page up from the first row and a
    /// page down from the last both stop rather than naming an entry that
    /// does not exist.
    #[test]
    fn the_walks_stop_at_the_transcripts_own_ends() {
        let transcript = long_transcript(4);
        let start = TranscriptAnchor::default();

        assert_eq!(transcript.scrolled_back(start, 1_000, WIDE), start);
        assert_eq!(
            transcript.scrolled_forward(start, 1_000, WIDE),
            TranscriptAnchor { entry: 4, row: 0 }
        );
        assert_eq!(
            transcript.tail_anchor(1_000, WIDE),
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
        let rows = transcript.rows_from(transcript.tail_anchor(window, WIDE), window, WIDE);

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
        let first_pass = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);
        assert_eq!(first_pass.len(), 2);
        assert_eq!(
            render_entry_calls(),
            2,
            "both entries are cold on the first call, so both must render once"
        );

        reset_render_entry_calls();
        let repeat_pass = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);
        assert_eq!(repeat_pass, first_pass);
        assert_eq!(
            render_entry_calls(),
            0,
            "nothing changed since the last call: a real cache hit renders \
             neither entry again, where a bypassed cache would render both"
        );

        transcript.append_or_extend(Some("m2"), " more", TranscriptRole::Agent);
        reset_render_entry_calls();
        let second_pass = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);
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
        let rows = transcript.rows_from(TranscriptAnchor::default(), window, WIDE);

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
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE)),
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
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE)),
            vec![
                "\u{276f} fix the retry policy",
                "\u{276f} <context: 3 files>"
            ]
        );
    }

    /// A replay interrupted part way through stops being one: the chunk
    /// that broke it is the adapter's own, and so is everything after it
    /// until the next prompt -- matching a later chunk against what is
    /// left of the echo would swallow half of a message the user has never
    /// been shown.
    #[test]
    fn a_replay_broken_off_part_way_stops_absorbing_what_follows() {
        let mut transcript = Transcript::new();
        transcript.echo_user_prompt("fix the retry policy");

        transcript.append_user_chunk(Some("wire-1"), "fix the ");
        transcript.append_user_chunk(Some("wire-2"), "<ctx>");
        transcript.append_user_chunk(Some("wire-2"), "retry policy");

        assert_eq!(
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE)),
            vec![
                "\u{276f} fix the retry policy",
                "\u{276f} <ctx>retry policy"
            ],
            "the injected message must arrive whole, not with its tail eaten"
        );
    }

    /// An adapter free to put its own context in front of the prompt has
    /// still said nothing of the prompt yet, so the window it would be
    /// deduplicated in is still open when it finally replays.
    #[test]
    fn context_injected_ahead_of_the_replay_does_not_close_the_window() {
        let mut transcript = Transcript::new();
        transcript.echo_user_prompt("fix the retry policy");

        transcript.append_user_chunk(Some("wire-1"), "<ctx>");
        transcript.append_user_chunk(Some("wire-2"), "fix the retry policy");

        assert_eq!(
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE)),
            vec!["\u{276f} fix the retry policy", "\u{276f} <ctx>"],
            "the replay is still recognised as the prompt already on screen"
        );
    }

    /// A plan task is a task: it opens with the glyph its state has earned
    /// and carries its state in colour, exactly as every other row on the
    /// panel does, rather than spelling its state out in words.
    #[test]
    fn a_plan_task_opens_with_the_glyph_for_its_state() {
        let mut transcript = Transcript::new();
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
            PlanEntry {
                content: "Run the tests".to_string(),
                priority: PlanEntryPriority::Low,
                status: PlanEntryStatus::Pending,
            },
        ]);

        let rows = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);
        assert_eq!(
            rows,
            vec![
                vec![
                    Span::new(TOOL_DONE_MARK, StyleRole::AiToolDone),
                    Span::plain("Read the file".to_string()),
                ],
                vec![
                    Span::new(PLAN_ACTIVE_MARK, StyleRole::AiAgent),
                    Span::plain("Write the fix".to_string()),
                ],
                vec![
                    Span::new(TOOL_PENDING_MARK, StyleRole::AiToolRunning),
                    Span::plain("Run the tests".to_string()),
                ],
            ]
        );
        let painted = texts(&rows).join("\n");
        for word in ["plan", "pending", "in_progress", "completed", "high", "low"] {
            assert!(
                !painted.contains(word),
                "a plan row must not spell out `{word}`: {painted}"
            );
        }
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
            texts(&transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE)),
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
        let _ = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);

        reset_render_entry_calls();
        transcript.advance_spinner();
        let _ = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);

        assert_eq!(
            render_entry_calls(),
            1,
            "a tick must re-render the running call and nothing else"
        );
    }

    /// A call the agent never resolved -- a crashed session leaves one on
    /// every turn it was mid-way through -- must not leave a marker
    /// animating and a wakeup armed behind it for the rest of the run.
    #[test]
    fn a_turn_that_ends_mid_call_stops_the_spinner() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        assert!(transcript.is_spinning());

        transcript.end_turn();

        assert!(!transcript.is_spinning());
    }

    /// The call the crashed turn abandoned is not the panel's to resolve,
    /// but it is the panel's not to animate: a later turn starting a call
    /// of its own must leave the abandoned one exactly as the turn left it,
    /// tick after tick, rather than picking it back up because it still
    /// says `in_progress` on the wire.
    #[test]
    fn an_abandoned_call_never_animates_again_behind_a_later_one() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Run tests".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        transcript.end_turn();
        let settled = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);
        assert_eq!(
            texts(&settled),
            vec!["\u{b7} Run tests"],
            "an abandoned call wears the marker of a call that is not moving"
        );

        transcript.upsert_tool_call(
            "call_2".to_string(),
            "Read file".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        transcript.advance_spinner();
        transcript.advance_spinner();

        let rows = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);
        assert_eq!(
            rows[0], settled[0],
            "the abandoned call's row must not have moved"
        );
        assert_ne!(
            texts(&rows)[1],
            "\u{b7} Read file",
            "the new call is the one that spins"
        );
    }

    /// A recovered session is free to restate `in_progress` on a call it
    /// already announced, and the panel has to read that as "still
    /// working" -- a transcript that only moved on transitions would leave
    /// the marker frozen for the whole turn.
    #[test]
    fn a_call_restated_in_progress_after_a_turn_ended_spins_again() {
        let mut transcript = Transcript::new();
        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Run tests".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        transcript.end_turn();

        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Run tests".to_string(),
            ToolCallStatus::InProgress,
            None,
        );

        assert!(transcript.is_spinning());
        let before = transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE);
        transcript.advance_spinner();
        assert_ne!(
            transcript.rows_from(TranscriptAnchor::default(), ROOM, WIDE),
            before
        );
    }

    /// Nothing running means nothing to animate, and the caller reads that
    /// answer to stop arming the next frame's wakeup.
    #[test]
    fn a_transcript_with_no_call_in_flight_never_spins() {
        let mut transcript = Transcript::new();
        transcript.append_or_extend(Some("m1"), "hello", TranscriptRole::Agent);
        assert!(!transcript.is_spinning());

        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Completed,
            None,
        );
        assert!(
            !transcript.is_spinning(),
            "a call that arrived already finished never spins"
        );
    }

    /// Two calls in flight animate together and the spinner keeps running
    /// until the last of them resolves -- a panel that stopped on the
    /// first result would freeze the marker of a call still working.
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
        assert!(transcript.is_spinning());

        transcript.upsert_tool_call(
            "call_1".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Completed,
            None,
        );
        assert!(transcript.is_spinning(), "the second call is still running");

        transcript.upsert_tool_call(
            "call_2".to_string(),
            "Read file".to_string(),
            ToolCallStatus::Failed,
            None,
        );
        assert!(!transcript.is_spinning());

        transcript.upsert_tool_call(
            "call_3".to_string(),
            "Write file".to_string(),
            ToolCallStatus::InProgress,
            None,
        );
        assert!(
            transcript.is_spinning(),
            "a later call starts the spinner back up"
        );
    }
}
