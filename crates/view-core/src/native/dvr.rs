//! The session DVR's core state: whether the session is being recorded, the
//! log of every input folded while it is, the frame each input followed,
//! the marks laid on the frame timeline, and the requests the loop carries
//! out on the recording's behalf.
//!
//! The painted frames themselves live with the terminal frontend. This
//! module holds no pixels, performs no I/O and hashes nothing.

use std::collections::{HashSet, VecDeque};
use std::ops::RangeInclusive;

use crate::msg::{Key, MouseInput, Msg};

/// The share of the recording's memory bound the input log reserves.
const ARENA_SHARE: usize = 8;

/// The arena bytes one log entry is reserved for. A key's notation is a
/// few bytes, so typing fills the entry list first and leaves the rest of
/// the arena to pastes.
const BYTES_PER_ENTRY: usize = 16;

/// The bytes the input log may hold, entry list and arena together, out of
/// a recording bound of `max_bytes`. The painted frames hold the rest.
#[must_use]
pub fn input_log_bytes(max_bytes: usize) -> usize {
    max_bytes / ARENA_SHARE
}

/// The keys the scrub bar names, each a key the scrub answers.
pub const SCRUB_HINT: &str = "q close  h/l frame  H/L 1s  g/G ends  b branch  e export";

/// The keys the bar names while a clip is shown, each a key the scrub
/// answers there.
pub const CLIP_HINT: &str = "q close  h/l frame  H/L 1s  g/G ends";

/// The longest symbol or mouse field a clip stores, in bytes.
pub const CLIP_FIELD_MAX: usize = 255;

/// What the thread doing the recording's file work answered.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DvrIoReply {
    /// The baselined files were hashed again.
    DiskChecked {
        /// The files whose bytes changed since their baseline.
        changed: Vec<String>,
        /// Whether the files live on a host this process cannot read.
        unverifiable: bool,
    },
    /// The recording was written as a clip.
    Exported {
        /// Where the clip was written.
        path: String,
        /// How many symbols were cut to [`CLIP_FIELD_MAX`] bytes.
        cut: usize,
    },
    /// An export was not started.
    Refused(ExportRefusal),
    /// A clip was read, and its frames wait for the loop to show them.
    ClipLoaded {
        /// The clip's file.
        path: String,
        /// The clip's oldest frames, left out to fit `[dvr] max_mb`.
        left_out: usize,
    },
    /// A file operation failed.
    Failed {
        /// The verb that failed.
        verb: &'static str,
        /// What failed, naming the path.
        reason: String,
    },
}

/// Why an export was not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExportRefusal {
    /// No frame has been recorded yet.
    NoFrame,
    /// An earlier export is still being written.
    Busy,
    /// The file thread's queue is full of other work.
    Queued,
    /// A copy of the newest frames would take the recording past its
    /// memory bound.
    OverBudget,
    /// The thread that writes clips is not running.
    NoWriter,
}

impl DvrIoReply {
    /// The notice the reply raises, `None` for one that raises none.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        Some(match self {
            // the frames a clip left out are on its bar while it is shown
            Self::DiskChecked { .. } | Self::ClipLoaded { .. } => return None,
            Self::Exported { path, cut: 0 } => format!("view: DVR clip written: {path}"),
            Self::Exported { path, cut } => format!(
                "view: DVR clip written: {path} ({cut} symbols cut to {CLIP_FIELD_MAX} bytes)"
            ),
            Self::Refused(why) => match why {
                ExportRefusal::NoFrame => "view: DVR has no frame to export yet",
                ExportRefusal::Busy => {
                    "view: DVR export busy: the last clip is still being written"
                }
                ExportRefusal::Queued => {
                    "view: DVR export busy: file work is queued, try again in a moment"
                }
                ExportRefusal::OverBudget => {
                    "view: DVR cannot export: no room to copy the newest frames: raise [dvr] max_mb"
                }
                ExportRefusal::NoWriter => {
                    "view: DVR cannot export: its file thread is not running"
                }
            }
            .to_owned(),
            Self::Failed { verb, reason } => format!("view: DVR {verb} failed: {reason}"),
        })
    }
}

/// The kind of one recorded input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputKind {
    /// A key in nvim notation.
    Key,
    /// A bracketed paste.
    Paste,
    /// A mouse event.
    Mouse,
    /// A terminal resize.
    Resized,
}

/// A mark on the frame timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Marker {
    /// The engine was replaced after it died.
    EngineRestart,
    /// The session was branched from an earlier frame.
    Branch,
    /// A DVR verb ran.
    Invoke,
}

/// One move of the scrub cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScrubStep {
    /// This many frames, negative toward the oldest.
    Frames(i32),
    /// This many seconds, negative toward the oldest.
    Seconds(i32),
    /// The oldest retained frame.
    Oldest,
    /// The newest frame.
    Newest,
}

/// One recorded input as the export and the replay read it.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct RecordedInput<'a> {
    /// The newest frame painted before the input was folded.
    pub after_frame: u64,
    /// What the input was.
    pub kind: InputKind,
    /// The input's encoded bytes. A key is its notation, a paste its
    /// text, a resize the width then the height as little-endian `u16`s,
    /// and a mouse event the row and the column the same way followed by
    /// the button, action and modifier joined by NUL bytes.
    pub body: &'a [u8],
}

/// Work the recording needs done outside the fold.
#[derive(Debug)]
#[non_exhaustive]
pub enum DvrRequest {
    /// Hash the file at this path now, the first time a buffer shows it.
    Baseline(String),
    /// Hash every baselined file again and report which changed.
    DiskCheck,
    /// Write the recording as a clip, to this path or a derived one.
    Export(Option<String>),
    /// Open the clip at this path.
    Play(String),
    /// Replace the editor with one replayed up to a frame.
    Branch(BranchPlan),
}

/// What a branch replays into the fresh engine.
#[derive(Debug)]
#[non_exhaustive]
pub struct BranchPlan {
    /// The frame the branch starts from.
    pub at_frame: u64,
    /// The inputs folded before that frame, in fold order.
    pub replay: Vec<Msg>,
}

/// Why a recorded frame cannot be branched from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BranchRefusal {
    /// A later branch left the frame behind.
    Dead,
    /// The input log filled at this frame, before the one asked for.
    PastLog(u64),
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    kind: InputKind,
    after_frame: u64,
    start: u32,
    len: u32,
}

/// The session recording's core state. Off until [`Dvr::enable`].
#[derive(Debug, Default)]
pub struct Dvr {
    recording: bool,
    max_bytes: usize,
    scrub: Option<u64>,
    pending_moves: VecDeque<ScrubStep>,
    arena: Vec<u8>,
    entries: Vec<Entry>,
    overflowed_at: Option<u64>,
    last_frame: u64,
    markers: Vec<(u64, Marker)>,
    dead: Vec<RangeInclusive<u64>>,
    seen_paths: HashSet<String>,
    requests: VecDeque<DvrRequest>,
    /// The frame `b` asked to branch from, until the disk check answers.
    asked: Option<u64>,
    /// A confirmed branch waits for the loop; input folded meanwhile goes
    /// to the engine being replaced and is not logged.
    paused: bool,
    /// Whether a replayed input is being folded, which reaches no agent.
    replaying: bool,
    /// Whether a replay's inputs went to the engine and nvim has not yet
    /// answered the flush sent after the last of them.
    draining: bool,
    /// The flush whose answer ends the drain, once the closing resize is
    /// sent.
    flush: Option<u64>,
    /// The flushes asked for so far, which numbers the next one.
    flushes: u64,
    /// A person's input folded while a replay is owed.
    held: Vec<Msg>,
    /// The newest frame before each engine restart. Kept whole while
    /// frames are dropped, since a replay starts from the first input.
    restarts: Vec<u64>,
    /// A branch's recorded input, held until the replacement has started.
    replay: Vec<Msg>,
    /// How many of the replayed inputs, the newest the log holds, have not
    /// reached the engine yet. Input logged meanwhile goes ahead of them,
    /// since the engine gets it first.
    unfolded: usize,
    /// Whether a surface holds a replayed key as the start of a mapped
    /// sequence, which reaches the engine only once the sequence resolves.
    sequenced: bool,
    /// Whether this drain has said that it drops review and DVR verbs.
    told_drop: bool,
    /// The clip the scrub shows in place of the recording.
    clip: Option<String>,
    /// How many of the clip's oldest frames did not fit `[dvr] max_mb`.
    clip_left_out: usize,
    /// A clip opened that the loop has not shown yet.
    clip_opened: bool,
    /// The clip being read, until its frames or its failure arrive.
    loading: Option<String>,
}

impl Dvr {
    /// Starts recording, reserving the input log's whole share of
    /// `max_bytes` up front so no later input allocates.
    pub fn enable(&mut self, max_bytes: usize) {
        let entries = input_log_bytes(max_bytes) / (std::mem::size_of::<Entry>() + BYTES_PER_ENTRY);
        self.entries = Vec::with_capacity(entries);
        self.arena = Vec::with_capacity(entries * BYTES_PER_ENTRY);
        self.max_bytes = max_bytes;
        self.recording = true;
    }

    /// Starts recording as [`Self::enable`] does, logging the terminal's
    /// `size` first so every replay starts at the size the session did.
    pub fn enable_at(&mut self, max_bytes: usize, size: (u16, u16)) {
        self.enable(max_bytes);
        self.record(&Msg::Resized {
            width: size.0,
            height: size.1,
        });
    }

    /// Whether the session is being recorded.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording
    }

    /// The memory bound recording was enabled with, input log and painted
    /// frames together. Zero while off.
    #[must_use]
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// The recorded frame shown while scrubbing, or `None` on the live
    /// screen.
    #[must_use]
    pub fn scrub_frame(&self) -> Option<u64> {
        self.scrub
    }

    /// Shows frame `seq`, the frame a [`ScrubStep`] resolved to. Ignored on
    /// the live screen.
    pub fn show(&mut self, seq: u64) {
        if self.scrub.is_some() {
            self.scrub = Some(seq);
        }
    }

    /// The oldest scrub move not taken yet.
    pub fn take_step(&mut self) -> Option<ScrubStep> {
        self.pending_moves.pop_front()
    }

    /// Freezes the screen on the newest recorded frame. Returns false, and
    /// stays live, when no frame has been recorded.
    pub(crate) fn open_scrub(&mut self) -> bool {
        if self.last_frame == 0 {
            return false;
        }
        self.scrub = Some(self.last_frame);
        self.pending_moves.clear();
        self.pending_moves.push_back(ScrubStep::Newest);
        true
    }

    /// Queues one move of the scrub cursor. A move of the same unit as the
    /// one queued last is added to it, so a burst of keys is one move.
    pub(crate) fn step(&mut self, step: ScrubStep) {
        let merged = match (self.pending_moves.back_mut(), step) {
            (Some(ScrubStep::Frames(n)), ScrubStep::Frames(m))
            | (Some(ScrubStep::Seconds(n)), ScrubStep::Seconds(m)) => {
                *n = n.saturating_add(m);
                true
            }
            _ => false,
        };
        if !merged {
            self.pending_moves.push_back(step);
        }
    }

    /// Returns to the live screen, closing any clip and ending a branch
    /// ask still waiting on its disk check. Returns whether a scrub was
    /// open.
    pub(crate) fn close_scrub(&mut self) -> bool {
        self.pending_moves.clear();
        self.clip = None;
        self.clip_left_out = 0;
        self.cancel_ask();
        self.scrub.take().is_some()
    }

    /// Shows the clip at `path`, read already, in the scrub on its newest
    /// frame, `left_out` of its oldest frames left behind.
    pub(crate) fn open_clip(&mut self, path: String, left_out: usize) {
        self.scrub = Some(self.scrub.unwrap_or(self.last_frame));
        self.pending_moves.clear();
        self.pending_moves.push_back(ScrubStep::Newest);
        self.clip = Some(path);
        self.clip_left_out = left_out;
        self.clip_opened = true;
        self.loading = None;
    }

    /// The clip being read, `None` when no read is in flight.
    #[must_use]
    pub(crate) fn loading(&self) -> Option<&str> {
        self.loading.as_deref()
    }

    /// Ends the read in flight, which failed.
    pub(crate) fn end_load(&mut self) {
        self.loading = None;
    }

    /// The file of the clip the scrub shows, `None` while it shows the
    /// recording or the live screen.
    #[must_use]
    pub fn clip(&self) -> Option<&str> {
        self.clip.as_deref()
    }

    /// How many of the shown clip's oldest frames did not fit `[dvr]
    /// max_mb`, zero while no clip is shown.
    #[must_use]
    pub fn clip_left_out(&self) -> usize {
        self.clip_left_out
    }

    /// Whether a clip was opened since the last call, once.
    pub fn take_opened_clip(&mut self) -> bool {
        std::mem::take(&mut self.clip_opened)
    }

    /// Queues a read of the clip at `path`, which is loading until
    /// [`Self::open_clip`] or [`Self::end_load`].
    pub(crate) fn request_play(&mut self, path: String) {
        self.loading = Some(path.clone());
        self.requests.push_back(DvrRequest::Play(path));
    }

    /// Marks the newest frame as the one a DVR verb ran on.
    pub(crate) fn mark_invoke(&mut self) {
        if self.recording {
            self.markers.push((self.last_frame, Marker::Invoke));
        }
    }

    /// Notes that frame `seq` is the newest recorded frame, with `oldest` the
    /// oldest frame still retained. Marks on frames no longer retained are
    /// dropped.
    pub fn note_frame(&mut self, seq: u64, oldest: u64) {
        self.last_frame = seq;
        self.markers.retain(|(frame, _)| *frame >= oldest);
        self.dead.retain(|range| *range.end() >= oldest);
    }

    /// Marks the newest frame as the last one the dead engine drew.
    /// A drain ends with it, since the dead engine answers no flush.
    pub fn note_restart(&mut self) {
        self.end_drain();
        if self.recording {
            self.markers.push((self.last_frame, Marker::EngineRestart));
            self.restarts.push(self.last_frame);
        }
    }

    /// Asks to branch from frame `at`: queues the disk check the prompt
    /// reads, or says why the frame cannot be reproduced.
    pub(crate) fn ask_branch(&mut self, at: u64) -> Result<(), BranchRefusal> {
        if self.dead.iter().any(|range| range.contains(&at)) {
            return Err(BranchRefusal::Dead);
        }
        if let Some(full) = self.overflowed_at.filter(|full| at > *full) {
            return Err(BranchRefusal::PastLog(full));
        }
        self.asked = Some(at);
        self.requests.push_back(DvrRequest::DiskCheck);
        Ok(())
    }

    /// The frame a branch was asked from, once, when its disk check answers.
    pub(crate) fn take_asked(&mut self) -> Option<u64> {
        self.asked.take()
    }

    /// Ends a branch ask, so its disk check raises no confirm.
    pub(crate) fn cancel_ask(&mut self) {
        self.asked = None;
    }

    /// Whether a branch waits on its disk check.
    #[must_use]
    pub fn is_asking(&self) -> bool {
        self.asked.is_some()
    }

    /// How many engine restarts happened before frame `at`, which a replay
    /// into one engine does not reproduce.
    #[must_use]
    pub fn restarts_before(&self, at: u64) -> usize {
        self.restarts.iter().filter(|frame| **frame < at).count()
    }

    /// Queues the branch from frame `at`, replaying the inputs folded
    /// before it. Logs no input until [`Self::branched`].
    pub(crate) fn confirm_branch(&mut self, at: u64) {
        let replay = self.replay_until(at);
        self.paused = true;
        self.requests.push_back(DvrRequest::Branch(BranchPlan {
            at_frame: at,
            replay,
        }));
    }

    /// Whether a confirmed branch waits for the loop.
    #[must_use]
    pub fn is_branching(&self) -> bool {
        self.paused
    }

    /// Settles the log once the editor was replaced from frame `at`, with
    /// `replay` to fold once the replacement has started. The log keeps
    /// the inputs folded before `at`, and the frames after `at` can no
    /// longer be branched from.
    pub fn branched(&mut self, at: u64, replay: Vec<Msg>) {
        let kept = self
            .entries
            .iter()
            .take_while(|entry| entry.after_frame < at)
            .count();
        // an input logged ahead of a replayed one sits after it in the arena
        let end = self
            .entries
            .iter()
            .take(kept)
            .map(|entry| (entry.start + entry.len) as usize)
            .max()
            .unwrap_or(0);
        self.entries.truncate(kept);
        self.arena.truncate(end);
        self.overflowed_at = None;
        self.restarts.retain(|frame| *frame < at);
        if at < self.last_frame {
            self.dead.push(at + 1..=self.last_frame);
        }
        self.markers.push((self.last_frame, Marker::Branch));
        self.unfolded = 0;
        self.end_drain();
        self.replay = replay;
        self.paused = false;
    }

    /// Whether a branch's replay waits for the replacement to start.
    #[must_use]
    pub fn has_replay(&self) -> bool {
        !self.replay.is_empty()
    }

    /// Keeps `msg`, a person's input folded while a replay is owed, to
    /// follow the replay into the engine.
    pub(crate) fn hold_live(&mut self, msg: Msg) {
        self.held.push(msg);
    }

    /// Ends a branch whose replacement could not start. The recording is
    /// left as it was, since the engine that follows is a restart of the
    /// timeline it describes.
    pub fn branch_failed(&mut self) {
        self.paused = false;
    }

    /// The replay a branch staged, to fold in order now, each input a
    /// [`Msg::Replayed`], followed by a resize to `size`, the terminal's
    /// size now, and the input a person typed while it was owed. The log
    /// holds the replayed inputs already, so folding them logs nothing.
    /// The replay drains from here until nvim answers the flush sent after
    /// its last input. Empty when no replay is owed.
    pub fn take_replay(&mut self, size: (u16, u16)) -> Vec<Msg> {
        let replay = std::mem::take(&mut self.replay);
        if replay.is_empty() {
            return replay;
        }
        self.unfolded = replay.len();
        self.draining = true;
        self.flush = None;
        self.told_drop = false;
        let mut replay: Vec<Msg> = replay
            .into_iter()
            .map(|msg| Msg::Replayed(Box::new(msg)))
            .collect();
        replay.push(Msg::Resized {
            width: size.0,
            height: size.1,
        });
        replay.append(&mut self.held);
        replay
    }

    /// Notes that an input went to the engine, `replayed` or not, asking
    /// for a flush behind it while the replay drains: behind the closing
    /// resize, and behind each replayed input a hold sent after it.
    pub(crate) fn note_sent(&mut self, msg: &Msg, replayed: bool) {
        if replayed {
            self.unfolded = self.unfolded.saturating_sub(1);
        }
        let closing = !replayed && self.flush.is_none() && matches!(msg, Msg::Resized { .. });
        let late = replayed && self.flush.is_some();
        if self.draining && (closing || late) {
            self.ask_flush();
        }
    }

    /// The flush asked for last while the replay drains, whose answer ends
    /// it.
    #[must_use]
    pub fn flush(&self) -> Option<u64> {
        self.flush
    }

    /// How many flushes have been asked for, which moves with each one.
    #[must_use]
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    fn ask_flush(&mut self) {
        self.flushes += 1;
        self.flush = Some(self.flushes);
    }

    /// Whether every replayed input has gone to the engine and the flush
    /// behind the last of them is `generation`.
    fn sent_all(&self, generation: u64) -> bool {
        self.flush == Some(generation) && self.unfolded == 0 && !self.sequenced
    }

    /// Notes nvim's answer to flush `generation`, which ends the drain
    /// when it is the newest one sent and no replayed input waits in a
    /// hold.
    pub(crate) fn note_flushed(&mut self, generation: u64) {
        if self.sent_all(generation) {
            self.end_drain();
        }
    }

    /// Notes that the answer to flush `generation` never came within its
    /// bound, which ends the drain on the terms an answer would. Returns
    /// whether it did.
    pub(crate) fn note_unanswered(&mut self, generation: u64) -> bool {
        let ends = self.draining && self.sent_all(generation);
        if ends {
            self.end_drain();
        }
        ends
    }

    /// Notes that a surface holds the key being folded as the start of a
    /// sequence, `carried` behind the keys it held already. A replayed key
    /// held there has not reached the engine, unless `untimed`, where only
    /// the person's next key sends it. A sequence that does not carry the
    /// held keys sent or dropped them, which asks for a flush behind them.
    pub(crate) fn hold_sequenced(&mut self, carried: bool, untimed: bool) {
        let held = self.sequenced;
        self.sequenced = !untimed && (self.replaying || (carried && held));
        if held && !self.sequenced && self.draining && self.flush.is_some() {
            self.ask_flush();
        }
    }

    /// Notes whether a surface still `holds` a sequence once a message is
    /// folded. An empty buffer sent or dropped every replayed key it held,
    /// which asks for a flush behind them.
    pub(crate) fn note_sequence(&mut self, holds: bool) {
        if holds || !std::mem::take(&mut self.sequenced) {
            return;
        }
        if self.draining && self.flush.is_some() {
            self.ask_flush();
        }
    }

    /// Whether a review or DVR verb dropped now is the drain's first,
    /// which says why it did nothing.
    pub(crate) fn first_drop(&mut self) -> bool {
        !std::mem::replace(&mut self.told_drop, true)
    }

    fn end_drain(&mut self) {
        self.draining = false;
        self.flush = None;
        self.sequenced = false;
    }

    /// Whether a replay's inputs may still be running in the engine, so a
    /// review or DVR verb arriving now may be one of them.
    #[must_use]
    pub fn draining(&self) -> bool {
        self.draining
    }

    /// Whether a replayed input is being folded.
    #[must_use]
    pub fn replaying(&self) -> bool {
        self.replaying
    }

    /// Notes whether a replayed input is being folded, returning what was
    /// noted before.
    pub(crate) fn set_replaying(&mut self, replaying: bool) -> bool {
        std::mem::replace(&mut self.replaying, replaying)
    }

    /// The oldest request the loop has not taken yet.
    pub fn take_request(&mut self) -> Option<DvrRequest> {
        self.requests.pop_front()
    }

    /// The recorded inputs, in the order they were folded.
    pub fn inputs(&self) -> impl Iterator<Item = RecordedInput<'_>> + '_ {
        self.entries.iter().map(|e| {
            let start = e.start as usize;
            RecordedInput {
                after_frame: e.after_frame,
                kind: e.kind,
                body: self
                    .arena
                    .get(start..start + e.len as usize)
                    .unwrap_or_default(),
            }
        })
    }

    /// The marks on the retained frames, oldest first.
    #[must_use]
    pub fn markers(&self) -> &[(u64, Marker)] {
        &self.markers
    }

    /// The frame ranges a branch abandoned, which no later branch may
    /// start from.
    #[must_use]
    pub fn dead(&self) -> &[RangeInclusive<u64>] {
        &self.dead
    }

    /// The frame the input log stopped at when it filled, if it has.
    #[must_use]
    pub fn overflowed_at(&self) -> Option<u64> {
        self.overflowed_at
    }

    /// Appends `msg` to the input log when it is an input, ahead of any
    /// replayed input the engine has not been sent yet. Allocates nothing:
    /// an input the reserved log cannot hold stops the log there.
    pub(crate) fn record(&mut self, msg: &Msg) {
        if !self.recording || self.paused || self.overflowed_at.is_some() {
            return;
        }
        let (kind, len) = match msg {
            Msg::Key(key) => (InputKind::Key, key.notation.len()),
            Msg::Paste(text) => (InputKind::Paste, text.len()),
            Msg::Mouse(m) => (
                InputKind::Mouse,
                4 + m.button.len() + m.action.len() + m.modifier.len() + 2,
            ),
            Msg::Resized { .. } => (InputKind::Resized, 4),
            _ => return,
        };
        let span = u32::try_from(self.arena.len())
            .ok()
            .zip(u32::try_from(len).ok());
        let Some((start, len)) = span.filter(|_| {
            self.arena.capacity() - self.arena.len() >= len
                && self.entries.len() < self.entries.capacity()
        }) else {
            self.overflowed_at = Some(self.last_frame);
            return;
        };
        match msg {
            Msg::Key(key) => self.arena.extend_from_slice(key.notation.as_bytes()),
            Msg::Paste(text) => self.arena.extend_from_slice(text.as_bytes()),
            Msg::Mouse(m) => {
                self.arena.extend_from_slice(&m.row.to_le_bytes());
                self.arena.extend_from_slice(&m.col.to_le_bytes());
                self.arena.extend_from_slice(m.button.as_bytes());
                self.arena.push(0);
                self.arena.extend_from_slice(m.action.as_bytes());
                self.arena.push(0);
                self.arena.extend_from_slice(m.modifier.as_bytes());
            }
            Msg::Resized { width, height } => {
                self.arena.extend_from_slice(&width.to_le_bytes());
                self.arena.extend_from_slice(&height.to_le_bytes());
            }
            _ => return,
        }
        let at = self.entries.len().saturating_sub(self.unfolded);
        // the inputs moved behind this one are sent after it, so they
        // follow the frame it followed
        for entry in self.entries.iter_mut().skip(at) {
            entry.after_frame = entry.after_frame.max(self.last_frame);
        }
        self.entries.insert(
            at,
            Entry {
                kind,
                after_frame: self.last_frame,
                start,
                len,
            },
        );
    }

    /// Queues an export of the recording to `path`, or to a path the loop
    /// derives. Returns false, queuing nothing, before the first frame.
    pub(crate) fn request_export(&mut self, path: Option<String>) -> bool {
        if self.last_frame == 0 {
            return false;
        }
        self.requests.push_back(DvrRequest::Export(path));
        true
    }

    /// Queues a baseline hash of `path` the first time it is seen while
    /// recording. A buffer holding no file has an empty path and is
    /// skipped.
    pub(crate) fn see_path(&mut self, path: &str) {
        if !self.recording || path.is_empty() || self.seen_paths.contains(path) {
            return;
        }
        self.seen_paths.insert(path.to_owned());
        self.requests
            .push_back(DvrRequest::Baseline(path.to_owned()));
    }

    /// The inputs folded before frame `at_frame` was painted, rebuilt as
    /// the messages they were recorded from.
    #[must_use]
    pub fn replay_until(&self, at_frame: u64) -> Vec<Msg> {
        self.inputs()
            .take_while(|input| input.after_frame < at_frame)
            .filter_map(rebuild)
            .collect()
    }
}

fn rebuild(input: RecordedInput<'_>) -> Option<Msg> {
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    let pair = |bytes: &[u8]| -> Option<(u16, u16)> {
        let a = u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]);
        let b = u16::from_le_bytes([*bytes.get(2)?, *bytes.get(3)?]);
        Some((a, b))
    };
    Some(match input.kind {
        InputKind::Key => Msg::Key(Key {
            notation: text(input.body),
        }),
        InputKind::Paste => Msg::Paste(text(input.body)),
        InputKind::Resized => {
            let (width, height) = pair(input.body)?;
            Msg::Resized { width, height }
        }
        InputKind::Mouse => {
            let (row, col) = pair(input.body)?;
            let mut parts = input.body.get(4..)?.split(|b| *b == 0).map(text);
            Msg::Mouse(MouseInput {
                button: parts.next()?,
                action: parts.next()?,
                modifier: parts.next()?,
                row,
                col,
            })
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn key(notation: &str) -> Msg {
        Msg::Key(Key {
            notation: notation.to_owned(),
        })
    }

    fn mouse() -> Msg {
        Msg::Mouse(MouseInput {
            button: "left".to_owned(),
            action: "press".to_owned(),
            modifier: "C-".to_owned(),
            row: 3,
            col: 300,
        })
    }

    fn recording() -> Dvr {
        let mut dvr = Dvr::default();
        dvr.enable(64 * 1024);
        dvr
    }

    #[test]
    fn record_keeps_inputs_in_fold_order_with_the_frame_they_followed() {
        let mut dvr = recording();
        dvr.record(&key("i"));
        dvr.note_frame(1, 0);
        dvr.record(&Msg::Paste("pasted".to_owned()));
        dvr.record(&mouse());
        dvr.note_frame(2, 0);
        dvr.record(&Msg::Resized {
            width: 120,
            height: 40,
        });
        dvr.record(&Msg::RedrawReady);
        let seen: Vec<_> = dvr.inputs().map(|i| (i.after_frame, i.kind)).collect();
        assert_eq!(
            seen,
            [
                (0, InputKind::Key),
                (1, InputKind::Paste),
                (1, InputKind::Mouse),
                (2, InputKind::Resized),
            ]
        );
        let bodies: Vec<_> = dvr.inputs().map(|i| i.body.to_vec()).collect();
        assert_eq!(bodies[0], b"i");
        assert_eq!(bodies[1], b"pasted");
    }

    #[test]
    fn recording_never_grows_the_arena_past_its_reservation() {
        let mut dvr = Dvr::default();
        dvr.enable(8 * 64);
        let arena = dvr.arena.capacity();
        let entries = dvr.entries.capacity();
        dvr.note_frame(7, 0);
        for _ in 0..200 {
            dvr.record(&key("<C-x>"));
        }
        assert_eq!(dvr.arena.capacity(), arena);
        assert_eq!(dvr.entries.capacity(), entries);
        assert_eq!(dvr.overflowed_at(), Some(7));
        let before = dvr.inputs().count();
        dvr.note_frame(8, 0);
        dvr.record(&key("a"));
        assert_eq!(dvr.inputs().count(), before, "a full log stays stopped");
    }

    #[test]
    fn replay_until_rebuilds_the_messages_before_that_frame() {
        let mut dvr = recording();
        let originals = [
            key("<Esc>"),
            Msg::Paste("text".to_owned()),
            mouse(),
            Msg::Resized {
                width: 80,
                height: 24,
            },
        ];
        for (frame, msg) in originals.iter().enumerate() {
            dvr.note_frame(u64::try_from(frame).unwrap(), 0);
            dvr.record(msg);
        }
        let replay = dvr.replay_until(3);
        let shown = |msgs: &[Msg]| msgs.iter().map(|m| format!("{m:?}")).collect::<Vec<_>>();
        assert_eq!(shown(&replay), shown(&originals[..3]));
    }

    #[test]
    fn a_disabled_dvr_records_nothing() {
        let mut dvr = Dvr::default();
        dvr.record(&key("a"));
        dvr.note_restart();
        dvr.see_path("/a.rs");
        assert_eq!(dvr.inputs().count(), 0);
        assert!(dvr.markers().is_empty());
        assert!(dvr.take_request().is_none());
        assert!(dvr.overflowed_at().is_none());
    }

    #[test]
    fn the_input_log_stays_inside_its_share_of_the_bound() {
        assert_eq!(std::mem::size_of::<Entry>(), 24);
        for max_mb in [64, 4096] {
            let max_bytes = max_mb << 20;
            let mut dvr = Dvr::default();
            dvr.enable(max_bytes);
            let held = dvr.arena.capacity() + dvr.entries.capacity() * std::mem::size_of::<Entry>();
            assert!(
                held <= input_log_bytes(max_bytes),
                "{max_mb} MiB: the log holds {held} bytes"
            );
            assert!(
                held * 10 >= input_log_bytes(max_bytes) * 9,
                "{max_mb} MiB: {held}"
            );
        }
    }

    #[test]
    fn a_paste_larger_than_the_remaining_arena_is_refused() {
        let mut dvr = Dvr::default();
        dvr.enable(8 * 400);
        let arena = dvr.arena.capacity();
        assert!(dvr.entries.capacity() > 1, "the entry list has room left");
        dvr.note_frame(3, 0);
        dvr.record(&Msg::Paste("x".repeat(arena + 1)));
        assert_eq!(dvr.inputs().count(), 0);
        assert_eq!(dvr.overflowed_at(), Some(3));
        assert_eq!(dvr.arena.capacity(), arena);
    }

    #[test]
    fn a_branch_past_the_full_input_log_is_refused() {
        let mut dvr = Dvr::default();
        dvr.enable(8 * 64);
        dvr.note_frame(4, 0);
        for _ in 0..200 {
            dvr.record(&key("<C-x>"));
        }
        dvr.note_frame(9, 0);
        assert_eq!(dvr.ask_branch(5), Err(BranchRefusal::PastLog(4)));
        assert!(dvr.take_request().is_none());
        assert_eq!(dvr.ask_branch(4), Ok(()));
        assert!(matches!(dvr.take_request(), Some(DvrRequest::DiskCheck)));
        dvr.confirm_branch(4);
        dvr.branched(4, Vec::new());
        assert_eq!(dvr.overflowed_at(), None, "the log has room again");
        assert_eq!(dvr.inputs().count(), 0);
        dvr.record(&key("a"));
        assert_eq!(dvr.inputs().count(), 1);
    }

    #[test]
    fn a_branch_replays_the_resizes_and_ends_at_the_current_size() {
        let mut dvr = recording();
        dvr.note_frame(1, 0);
        dvr.record(&Msg::Resized {
            width: 100,
            height: 30,
        });
        dvr.record(&key("i"));
        dvr.note_frame(2, 0);
        dvr.confirm_branch(2);
        assert!(dvr.is_branching());
        dvr.record(&key("lost"));
        let plan = queued(&mut dvr);
        dvr.branched(plan.at_frame, plan.replay);
        dvr.hold_live(key("early"));
        assert_eq!(dvr.inputs().count(), 2, "a key typed meanwhile waits");
        let replay = dvr.take_replay((120, 40));
        let shown: Vec<_> = replay.iter().map(|m| format!("{m:?}")).collect();
        assert_eq!(shown.len(), 4, "{shown:?}");
        assert!(
            shown[2].contains("120") && shown[2].contains("40"),
            "{shown:?}"
        );
        assert!(shown[3].contains("early"), "{shown:?}");
        // as `update` folds them: a replayed input is logged already
        for msg in &replay {
            match msg {
                Msg::Replayed(inner) => dvr.note_sent(inner, true),
                msg => {
                    dvr.record(msg);
                    dvr.note_sent(msg, false);
                }
            }
        }
        let logged: Vec<_> = dvr.inputs().map(|i| i.body.to_vec()).collect();
        assert_eq!(logged.len(), 4, "the closing resize and the key are logged");
        assert_eq!(logged[2], [120, 0, 40, 0]);
        assert_eq!(logged[3], b"early", "in the order the engine got them");
        dvr.record(&key("j"));
        assert_eq!(dvr.inputs().count(), 5, "a key after the replay is logged");
    }

    fn queued(dvr: &mut Dvr) -> BranchPlan {
        match dvr.take_request() {
            Some(DvrRequest::Branch(plan)) => Some(plan),
            _ => None,
        }
        .expect("a branch is queued")
    }

    fn resized(width: u16, height: u16) -> Msg {
        Msg::Resized { width, height }
    }

    /// Branches at `at` the way the loop does once the new engine has
    /// started at `size`, folding the replay into the log.
    fn branch_and_fold(dvr: &mut Dvr, at: u64, size: (u16, u16)) {
        dvr.confirm_branch(at);
        let plan = queued(dvr);
        dvr.branched(plan.at_frame, plan.replay);
        for msg in &dvr.take_replay(size) {
            dvr.record(msg);
        }
    }

    #[test]
    fn a_click_after_a_branch_replays_at_the_size_it_was_made_at() {
        let mut dvr = recording();
        dvr.note_frame(1, 0);
        dvr.record(&resized(100, 30));
        dvr.note_frame(2, 0);
        dvr.record(&resized(120, 40));
        dvr.note_frame(3, 0);
        branch_and_fold(&mut dvr, 2, (120, 40));
        dvr.record(&mouse());
        dvr.note_frame(4, 0);
        let replay: Vec<_> = dvr
            .replay_until(5)
            .iter()
            .map(|m| format!("{m:?}"))
            .collect();
        let click = replay.iter().position(|m| m.contains("Mouse")).unwrap();
        let size = replay[..click]
            .iter()
            .rfind(|m| m.contains("Resized"))
            .unwrap();
        assert!(size.contains("120") && size.contains("40"), "{replay:?}");
    }

    #[test]
    fn a_replay_with_no_resize_runs_at_the_launch_size() {
        let mut dvr = Dvr::default();
        dvr.enable_at(64 * 1024, (80, 24));
        dvr.note_frame(1, 0);
        dvr.record(&key("i"));
        dvr.note_frame(2, 0);
        dvr.record(&resized(120, 40));
        dvr.note_frame(3, 0);
        dvr.confirm_branch(2);
        let plan = queued(&mut dvr);
        let shown: Vec<_> = plan.replay.iter().map(|m| format!("{m:?}")).collect();
        assert_eq!(shown.len(), 2, "{shown:?}");
        assert!(
            shown[0].contains("80") && shown[0].contains("24"),
            "{shown:?}"
        );
    }

    #[test]
    fn a_failed_branch_leaves_the_recording_as_it_was() {
        let mut dvr = recording();
        dvr.note_frame(1, 0);
        dvr.record(&key("i"));
        dvr.note_frame(2, 0);
        dvr.record(&key("a"));
        dvr.note_frame(3, 0);
        dvr.confirm_branch(2);
        assert!(dvr.is_branching());
        dvr.branch_failed();
        assert!(!dvr.is_branching());
        assert!(!dvr.has_replay());
        assert_eq!(dvr.inputs().count(), 2);
        assert!(dvr.dead().is_empty());
        assert!(dvr.markers().is_empty());
        dvr.record(&key("x"));
        assert_eq!(dvr.inputs().count(), 3, "recording carries on");
    }
}
