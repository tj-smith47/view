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
pub const SCRUB_HINT: &str = "q close  h/l frame  H/L 1s  g/G ends  e export";

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
            Self::DiskChecked { .. } => return None,
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

    /// Returns to the live screen. Returns whether a scrub was open.
    pub(crate) fn close_scrub(&mut self) -> bool {
        self.pending_moves.clear();
        self.scrub.take().is_some()
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
    pub fn note_restart(&mut self) {
        if self.recording {
            self.markers.push((self.last_frame, Marker::EngineRestart));
        }
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

    /// Appends `msg` to the input log when it is an input. Allocates
    /// nothing: an input the reserved log cannot hold stops the log there.
    pub(crate) fn record(&mut self, msg: &Msg) {
        if !self.recording || self.overflowed_at.is_some() {
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
        self.entries.push(Entry {
            kind,
            after_frame: self.last_frame,
            start,
            len,
        });
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
}
