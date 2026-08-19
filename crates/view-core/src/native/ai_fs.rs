//! The agent's own file reads and writes, between the request crossing in
//! and the answer crossing back.
//!
//! An `fs/read_text_file` or `fs/write_text_file` is a request the agent
//! addresses to view, and view cannot answer it from this side: the
//! authoritative text lives in nvim. So the answer takes three hops -- resolve
//! the path onto a buffer, read or write that buffer, release the hold --
//! and this module holds what has to survive between them. Pure data and
//! pure decisions, like everything else here: nothing in this file reads a
//! file, and `path` is a value carried forward for the release, never
//! something opened.

use std::path::PathBuf;

/// How many agent filesystem requests may be part way through at once.
///
/// Each one holds a hidden-buffer hold until it answers, so an unbounded
/// count is an unbounded number of loaded buffers a single misbehaving
/// agent could pin. Well above any real tool loop's concurrency -- an agent
/// issuing more than this is not reading a project, it is leaking requests
/// -- and the refusal past it is an ordinary answered error, so nothing
/// hangs waiting on a request this crate declined to start.
pub const MAX_IN_FLIGHT: usize = 64;

/// What an in-flight request will do once its path has resolved to a
/// buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsIntent {
    /// Read the buffer, optionally windowed to the wire's `line`/`limit`.
    Read {
        /// The wire's 1-based start line, `None` for the whole buffer.
        line: Option<u32>,
        /// The wire's maximum line count, `None` for "to the end."
        limit: Option<u32>,
    },
    /// Replace the buffer's whole text and save it.
    Write {
        /// The lines the agent's `content` split into.
        lines: Vec<String>,
        /// Whether that `content` ended in a newline, which is what decides
        /// the file's own trailing newline (see [`split_content`]).
        eol: bool,
    },
}

/// One agent filesystem request between the event that raised it and the
/// command that answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFsOp {
    /// The boundary id the answering `AiCommand` is correlated against.
    pub request_id: u64,
    /// The `RpcCall::LoadHidden` generation this request's resolve is
    /// tagged with. Drawn from the same counter a diff review's own resolve
    /// draws from, so no reply can ever be folded by the wrong owner.
    pub generation: u64,
    /// The path as the agent spelled it, kept for the release that must
    /// name the identical spelling the hold was taken under.
    pub path: PathBuf,
    /// What this request does once the resolve answers.
    pub intent: FsIntent,
}

/// Every agent filesystem request this session is part way through.
///
/// A `Vec` rather than a map: the list is bounded by [`MAX_IN_FLIGHT`] and
/// is empty in the overwhelming majority of frames, so a linear scan over
/// at most a few entries costs less than the hashing a map would spend on
/// every lookup.
#[derive(Debug, Clone, Default)]
pub struct AiFsState {
    open: Vec<PendingFsOp>,
}

impl AiFsState {
    /// Records `op` as in flight, or refuses it when [`MAX_IN_FLIGHT`] are
    /// already open. `false` means the caller must answer the agent itself
    /// rather than wait for a round trip that was never started.
    #[must_use]
    pub fn open(&mut self, op: PendingFsOp) -> bool {
        if self.open.len() >= MAX_IN_FLIGHT {
            return false;
        }
        self.open.push(op);
        true
    }

    /// The request whose resolve carries `generation`, left in flight.
    #[must_use]
    pub fn by_generation(&self, generation: u64) -> Option<&PendingFsOp> {
        self.open.iter().find(|op| op.generation == generation)
    }

    /// Removes and returns the request whose resolve carries `generation`.
    #[must_use]
    pub fn take_by_generation(&mut self, generation: u64) -> Option<PendingFsOp> {
        let at = self
            .open
            .iter()
            .position(|op| op.generation == generation)?;
        Some(self.open.remove(at))
    }

    /// Removes and returns the request correlated on `request_id`, or
    /// `None` for an id nothing is waiting on -- a duplicate or invented
    /// answer, which is dropped rather than acted on.
    #[must_use]
    pub fn take_by_request(&mut self, request_id: u64) -> Option<PendingFsOp> {
        let at = self
            .open
            .iter()
            .position(|op| op.request_id == request_id)?;
        Some(self.open.remove(at))
    }

    /// Empties the whole list, returning every request that was still in
    /// flight so the caller can release each one's hold. Used when the
    /// session that raised them is gone: nothing is left to answer, but the
    /// holds they took are this process's own and still have to come back.
    #[must_use]
    pub fn drain(&mut self) -> Vec<PendingFsOp> {
        std::mem::take(&mut self.open)
    }

    /// How many requests are in flight.
    #[must_use]
    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// Whether nothing is in flight.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }
}

/// Splits an agent's `content` into the buffer lines nvim holds it as, plus
/// whether the file it lands in ends with a newline.
///
/// nvim's line list carries no record of a final newline (a file ending
/// `"a\n"` and one ending `"a"` are both the single line `["a"]`), so the
/// terminator has to travel beside the lines -- see
/// `docs/acp-fs-wire-capture.md` case 7, where it becomes the buffer's
/// `endofline`/`fixendofline` pair and decides the byte the file ends with.
/// Without it every agent write would append a newline the agent did not
/// send.
///
/// Splitting on `\n` alone, never `\r\n`: a trailing `\r` is stripped from
/// no line. nvim's own `fileformat` is what turns a line list into line
/// terminators on disk, and a `unix`-format file may legitimately hold a
/// `\r` at end of line -- which the read side hands back as part of the
/// line. Stripping here would make a read-then-write round trip of such a
/// file silently drop those bytes, which is the worse of the two failures.
#[must_use]
pub fn split_content(content: &str) -> (Vec<String>, bool) {
    match content.strip_suffix('\n') {
        Some(body) => (body.split('\n').map(str::to_owned).collect(), true),
        None => (content.split('\n').map(str::to_owned).collect(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(request_id: u64, generation: u64) -> PendingFsOp {
        PendingFsOp {
            request_id,
            generation,
            path: PathBuf::from("/tmp/a.rs"),
            intent: FsIntent::Read {
                line: None,
                limit: None,
            },
        }
    }

    #[test]
    fn a_request_is_found_by_its_generation_and_removed_by_its_id() {
        let mut state = AiFsState::default();
        assert!(state.open(op(7, 3)));
        assert!(state.open(op(8, 4)));
        assert_eq!(state.by_generation(4).map(|op| op.request_id), Some(8));
        assert_eq!(state.take_by_request(7).map(|op| op.generation), Some(3));
        assert!(
            state.take_by_request(7).is_none(),
            "a second answer to the same id finds nothing waiting"
        );
        assert_eq!(state.len(), 1);
    }

    /// The cap is what keeps an agent's request count from becoming a
    /// hidden-buffer count. Deleting it lets the 65th request through.
    #[test]
    fn the_in_flight_cap_refuses_rather_than_growing_without_bound() {
        let mut state = AiFsState::default();
        for i in 0..MAX_IN_FLIGHT {
            assert!(state.open(op(i as u64, i as u64)), "request {i} must open");
        }
        assert!(
            !state.open(op(9999, 9999)),
            "the request past the cap is refused, not queued"
        );
        assert_eq!(state.len(), MAX_IN_FLIGHT);
    }

    #[test]
    fn draining_returns_every_open_request_and_empties_the_list() {
        let mut state = AiFsState::default();
        assert!(state.open(op(1, 1)));
        assert!(state.open(op(2, 2)));
        let drained = state.drain();
        assert_eq!(drained.len(), 2);
        assert!(state.is_empty());
    }

    /// The trailing-newline half of `split_content`: every case that
    /// decides a byte on disk.
    #[test]
    fn content_splits_into_lines_and_a_trailing_newline_flag() {
        assert_eq!(
            split_content("alpha\nbravo\n"),
            (vec!["alpha".to_owned(), "bravo".to_owned()], true)
        );
        assert_eq!(
            split_content("alpha\nbravo"),
            (vec!["alpha".to_owned(), "bravo".to_owned()], false)
        );
        assert_eq!(split_content(""), (vec![String::new()], false));
        assert_eq!(split_content("\n"), (vec![String::new()], true));
        assert_eq!(
            split_content("a\n\nb\n"),
            (vec!["a".to_owned(), String::new(), "b".to_owned()], true)
        );
    }

    /// A `\r` at end of line is content, not a terminator: a read of a
    /// unix-format file holding one hands it back, and the write that
    /// follows must put back exactly what it was given.
    #[test]
    fn a_carriage_return_at_end_of_line_survives_the_split() {
        assert_eq!(
            split_content("alpha\r\nbravo\r\n"),
            (vec!["alpha\r".to_owned(), "bravo\r".to_owned()], true)
        );
    }
}
