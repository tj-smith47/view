//! Reads a `.vdvr` clip back into a ring of frames, its inputs rebuilt as
//! the messages they were recorded from.

use std::fmt;
use std::io::{self, Read};
use std::ops::RangeInclusive;

use view_core::msg::{Key, MouseInput, Msg};
use view_core::native::dvr::{input_log_bytes, Marker};
use view_tui::dvr::{CellView, FrameRing, RingBuilder, Scroll};

use super::{DEAD, DELTA, END, INPUT, KEYFRAME, MAGIC, MARKER, VERSION};

/// A clip read back.
#[derive(Debug)]
pub(crate) struct Clip {
    /// The clip's frames.
    pub(crate) ring: FrameRing,
    /// The recorded inputs, each with the frame it followed, in fold order.
    pub(crate) inputs: Vec<(u64, Msg)>,
    /// The marks on the clip's frames.
    pub(crate) markers: Vec<(u64, Marker)>,
    /// The frame ranges a branch abandoned.
    pub(crate) dead: Vec<RangeInclusive<u64>>,
    /// The frames the ring could not hold, with the deltas built on them.
    pub(crate) dropped: usize,
    /// The oldest frames the ring let go of to hold the newer ones.
    pub(crate) left_out: usize,
    /// The inputs past the input log's share of the recording bound, and
    /// those of a kind this build does not know.
    pub(crate) dropped_inputs: usize,
}

/// The marks, and apart from them the abandoned ranges, a clip is read with
/// at most. A recording lays one per verb, restart or branch, so a clip
/// past this many is damaged and refused.
pub(super) const MARKS_MAX: usize = 1 << 16;

/// The fewest bytes a recorded cell takes: an empty symbol, three colors
/// and the modifier.
const CELL_BYTES_MIN: usize = 15;

/// Why a clip could not be read.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum ClipError {
    /// Reading failed.
    Io(io::Error),
    /// The file does not start with the clip magic.
    NotAClip,
    /// The clip was written by a newer build, in this format version.
    Newer(u16),
    /// The file ends before its end record.
    Truncated,
    /// A record is damaged.
    Malformed(&'static str),
}

impl fmt::Display for ClipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::NotAClip => f.write_str("not a view DVR clip"),
            Self::Newer(v) => write!(f, "written in clip format {v}, newer than this build reads"),
            Self::Truncated => f.write_str("the clip ends before its end record"),
            Self::Malformed(what) => write!(f, "damaged clip: {what}"),
        }
    }
}

impl From<io::Error> for ClipError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Self::Truncated
        } else {
            Self::Io(e)
        }
    }
}

/// Reads a clip from `r` into a ring holding a recording bound of
/// `max_bytes`. A record longer than that bound is refused before it is
/// read, and the inputs past the input log's share of it are dropped and
/// counted.
///
/// # Errors
///
/// Returns why the clip could not be read.
pub(crate) fn decode(r: &mut impl Read, max_bytes: usize) -> Result<Clip, ClipError> {
    let mut header = [0u8; 16];
    r.read_exact(&mut header).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => ClipError::NotAClip,
        _ => ClipError::Io(e),
    })?;
    let mut head = Bytes(&header);
    if head.take(MAGIC.len())? != MAGIC {
        return Err(ClipError::NotAClip);
    }
    let version = head.u16()?;
    if version > VERSION {
        return Err(ClipError::Newer(version));
    }
    let mut builder = RingBuilder::new(max_bytes);
    let mut clip = Clip {
        ring: FrameRing::new(0),
        inputs: Vec::new(),
        markers: Vec::new(),
        dead: Vec::new(),
        dropped: 0,
        left_out: 0,
        dropped_inputs: 0,
    };
    let (mut frames, mut inputs_read, mut input_bytes) = (0u64, 0u64, 0usize);
    let mut last_seq = None;
    let mut payload = Vec::new();
    loop {
        let mut tag = [0u8; 5];
        r.read_exact(&mut tag)?;
        let [tag, len @ ..] = tag;
        let len = u32::from_le_bytes(len);
        if !usize::try_from(len).is_ok_and(|len| len <= max_bytes) {
            return Err(ClipError::Malformed(
                "a record is larger than the recording bound",
            ));
        }
        payload.clear();
        let read = r.by_ref().take(u64::from(len)).read_to_end(&mut payload)?;
        if u32::try_from(read).ok() != Some(len) {
            return Err(ClipError::Truncated);
        }
        let mut body = Bytes(&payload);
        match tag {
            KEYFRAME | DELTA => {
                let seq = body.u64()?;
                let follows = last_seq.map_or(seq > 0, |last: u64| {
                    seq > last && (tag == KEYFRAME || last.checked_add(1) == Some(seq))
                });
                if !follows {
                    return Err(ClipError::Malformed("frames out of order"));
                }
                last_seq = Some(seq);
                frames += 1;
                builder.seat(seq);
                if read_frame(&mut builder, tag, &mut body)?.is_none() {
                    clip.dropped += 1;
                }
            }
            INPUT => {
                inputs_read += 1;
                let after = body.u64()?;
                input_bytes = input_bytes.saturating_add(payload.len());
                let kept = input_bytes <= input_log_bytes(max_bytes);
                match read_input(&mut body)?.filter(|_| kept) {
                    Some(msg) => clip.inputs.push((after, msg)),
                    None => clip.dropped_inputs += 1,
                }
            }
            MARKER if clip.markers.len() >= MARKS_MAX => return Err(too_many_marks()),
            DEAD if clip.dead.len() >= MARKS_MAX => return Err(too_many_marks()),
            MARKER => {
                let frame = body.u64()?;
                let marker = match body.u8()? {
                    0 => Marker::EngineRestart,
                    1 => Marker::Branch,
                    2 => Marker::Invoke,
                    _ => continue,
                };
                clip.markers.push((frame, marker));
            }
            DEAD => clip.dead.push(body.u64()?..=body.u64()?),
            END => {
                if body.u64()? != frames || body.u64()? != inputs_read {
                    return Err(ClipError::Malformed("the end record miscounts the clip"));
                }
                if r.read(&mut [0u8; 1])? != 0 {
                    return Err(ClipError::Malformed("bytes follow the end record"));
                }
                clip.ring = builder.finish();
                clip.left_out = usize::try_from(frames)
                    .unwrap_or(usize::MAX)
                    .saturating_sub(clip.dropped)
                    .saturating_sub(clip.ring.frame_count());
                return Ok(clip);
            }
            _ => {}
        }
    }
}

/// The error for a clip carrying more marks or ranges than [`MARKS_MAX`].
fn too_many_marks() -> ClipError {
    ClipError::Malformed("more marks than a recording lays")
}

/// Adds the keyframe or delta in `body`, read past its seq, to `builder`.
/// Returns its seq, or `None` when the ring could not hold it.
fn read_frame(
    builder: &mut RingBuilder,
    tag: u8,
    body: &mut Bytes<'_>,
) -> Result<Option<u64>, ClipError> {
    let at_us = body.u64()?;
    let area = if tag == KEYFRAME {
        Some((body.u16()?, body.u16()?))
    } else {
        None
    };
    let (x, y, on) = (body.u16()?, body.u16()?, body.u8()?);
    let cursor = (on != 0).then_some((x, y));
    let mut cells = Vec::new();
    if let Some((w, h)) = area {
        let cells_named = usize::from(w) * usize::from(h);
        if cells_named.saturating_mul(CELL_BYTES_MIN) > body.0.len() {
            return Err(ClipError::Malformed(
                "a keyframe holds fewer cells than its size",
            ));
        }
        if !builder.holds((w, h)) {
            // the cells stay unread, and the refusal leaves the deltas
            // after this frame nothing to build on
            return Ok(builder.push_key(at_us, (w, h), cursor, std::iter::empty()));
        }
        for i in 0..cells_named {
            let at = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
            let (x, y) = (at(i % usize::from(w)), at(i / usize::from(w)));
            cells.push(body.cell(x, y)?);
        }
        return Ok(builder.push_key(at_us, (w, h), cursor, cells));
    }
    let on = body.u8()? != 0;
    let (top, bottom, left, right) = (body.u16()?, body.u16()?, body.u16()?, body.u16()?);
    let by = body.i16()?;
    let scroll = on.then(|| Scroll::new(top..bottom, left..right, by));
    for _ in 0..body.u32()? {
        let (x, y) = (body.u16()?, body.u16()?);
        cells.push(body.cell(x, y)?);
    }
    Ok(builder.push_delta(at_us, cursor, scroll, cells.iter().copied()))
}

/// The input in `body`, rebuilt as the message it was recorded from.
/// `None` for an input kind this build does not know.
fn read_input(body: &mut Bytes<'_>) -> Result<Option<Msg>, ClipError> {
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    Ok(Some(match body.u8()? {
        0 => Msg::Key(Key {
            notation: text(body.rest()),
        }),
        1 => Msg::Paste(text(body.rest())),
        2 => {
            let (row, col) = (body.u16()?, body.u16()?);
            let mut field = || -> Result<String, ClipError> {
                let len = body.u8()?;
                Ok(text(body.take(usize::from(len))?))
            };
            Msg::Mouse(MouseInput {
                button: field()?,
                action: field()?,
                modifier: field()?,
                row,
                col,
            })
        }
        3 => Msg::Resized {
            width: body.u16()?,
            height: body.u16()?,
        },
        _ => return Ok(None),
    }))
}

/// A record's payload, read from the front.
struct Bytes<'a>(&'a [u8]);

impl<'a> Bytes<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ClipError> {
        let (head, rest) = self
            .0
            .split_at_checked(n)
            .ok_or(ClipError::Malformed("a record ends inside a field"))?;
        self.0 = rest;
        Ok(head)
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ClipError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, ClipError> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    fn u16(&mut self) -> Result<u16, ClipError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn i16(&mut self) -> Result<i16, ClipError> {
        Ok(i16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ClipError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ClipError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn cell(&mut self, x: u16, y: u16) -> Result<CellView<'a>, ClipError> {
        let len = self.u8()?;
        let symbol = std::str::from_utf8(self.take(usize::from(len))?)
            .map_err(|_| ClipError::Malformed("a symbol is not UTF-8"))?;
        let colors = [self.u32()?, self.u32()?, self.u32()?];
        Ok(CellView::new(x, y, symbol, colors, self.u16()?))
    }
}
