//! Reads a `.vdvr` clip back into a ring of frames, its inputs rebuilt as
//! the messages they were recorded from.

use std::fmt;
use std::io::{self, Read};
use std::ops::RangeInclusive;

use view_core::msg::{Key, MouseInput, Msg};
use view_core::native::dvr::Marker;
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
}

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
/// `max_bytes`.
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
    };
    let mut frames = 0u64;
    let mut payload = Vec::new();
    loop {
        let mut tag = [0u8; 5];
        r.read_exact(&mut tag)?;
        let [tag, len @ ..] = tag;
        let len = u64::from(u32::from_le_bytes(len));
        payload.clear();
        let read = r.by_ref().take(len).read_to_end(&mut payload)?;
        if u64::try_from(read).ok() != Some(len) {
            return Err(ClipError::Truncated);
        }
        let mut body = Bytes(&payload);
        match tag {
            KEYFRAME | DELTA => {
                frames += 1;
                if read_frame(&mut builder, tag, &mut body)?.is_none() {
                    clip.dropped += 1;
                }
            }
            INPUT => {
                let after = body.u64()?;
                if let Some(msg) = read_input(&mut body)? {
                    clip.inputs.push((after, msg));
                }
            }
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
                let inputs = u64::try_from(clip.inputs.len()).ok();
                if body.u64()? != frames || Some(body.u64()?) != inputs {
                    return Err(ClipError::Malformed("the end record miscounts the clip"));
                }
                if r.read(&mut [0u8; 1])? != 0 {
                    return Err(ClipError::Malformed("bytes follow the end record"));
                }
                clip.ring = builder.finish();
                return Ok(clip);
            }
            _ => {}
        }
    }
}

/// Adds the keyframe or delta in `body` to `builder`. Returns its seq, or
/// `None` when the ring could not hold it.
fn read_frame(
    builder: &mut RingBuilder,
    tag: u8,
    body: &mut Bytes<'_>,
) -> Result<Option<u64>, ClipError> {
    // the builder numbers the frames it keeps itself
    let _seq = body.u64()?;
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
        for i in 0..usize::from(w) * usize::from(h) {
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
