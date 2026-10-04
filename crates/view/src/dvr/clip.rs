//! The `.vdvr` clip file: a fixed header, then tagged records carrying the
//! recording's frames, inputs, marks and abandoned ranges, then an end
//! record. Every integer is little-endian.
//!
//! ```text
//! header   magic b"VIEWDVR\0", version u16 = 1, flags u16 (bit 0: inputs
//!          present), reserved u32 = 0
//! record   tag u8, len u32, len payload bytes; an unknown tag is skipped
//! 0x01     keyframe: seq u64, at_us u64, w u16, h u16, cursor, then w*h
//!          cells row by row
//! 0x02     delta: seq u64, at_us u64, cursor, scroll_on u8, top u16,
//!          bottom u16, left u16, right u16, by i16, n u32, then n times
//!          x u16, y u16, cell
//! 0x03     input: after_frame u64, kind u8 (0 key, 1 paste, 2 mouse,
//!          3 resize), body: key and paste text to the end; mouse row u16,
//!          col u16, then button, action and modifier each as len u8 and
//!          text; resize width u16, height u16
//! 0x04     marker: frame u64, kind u8 (0 engine restart, 1 branch,
//!          2 invoke)
//! 0x05     dead: first u64, last u64
//! 0xFF     end: frames u64, inputs u64, always last
//! cursor   x u16, y u16, on u8
//! cell     sym_len u8, sym, fg u32, bg u32, ul u32, modifier u16
//! ```
//!
//! A symbol or mouse field longer than [`CLIP_FIELD_MAX`] bytes is cut at
//! the last character boundary that fits, and the encoder counts the
//! symbols it cut.

use std::io::{self, Write};
use std::ops::RangeInclusive;

use view_core::native::dvr::{InputKind, Marker, RecordedInput, CLIP_FIELD_MAX};
use view_tui::dvr::{CellView, FrameView, RingSnapshot, Scroll};

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "no session verb opens a clip, so only the tests read one"
    )
)]
pub(super) mod read;

const MAGIC: &[u8; 8] = b"VIEWDVR\0";
const VERSION: u16 = 1;
const FLAG_INPUTS: u16 = 1;

const KEYFRAME: u8 = 0x01;
const DELTA: u8 = 0x02;
const INPUT: u8 = 0x03;
const MARKER: u8 = 0x04;
const DEAD: u8 = 0x05;
const END: u8 = 0xFF;

/// The recording's input log as the clip's input records, copied once on
/// the loop so the log stays free to grow while the clip is written.
#[derive(Debug, Default)]
pub(crate) struct Inputs {
    records: Vec<u8>,
    count: u64,
}

impl Inputs {
    /// The input records for `log`, in fold order.
    pub(crate) fn new<'a>(log: impl Iterator<Item = RecordedInput<'a>>) -> Self {
        let mut inputs = Self::default();
        let mut payload = Vec::new();
        for input in log {
            payload.clear();
            payload.extend_from_slice(&input.after_frame.to_le_bytes());
            let kind = match input.kind {
                InputKind::Key => 0,
                InputKind::Paste => 1,
                InputKind::Mouse => 2,
                InputKind::Resized => 3,
                _ => continue,
            };
            payload.push(kind);
            if input.kind == InputKind::Mouse {
                let (head, tail) = input.body.split_at(input.body.len().min(4));
                payload.extend_from_slice(head);
                for field in tail.split(|b| *b == 0) {
                    let _ = put_short(&mut payload, &String::from_utf8_lossy(field));
                }
            } else {
                payload.extend_from_slice(input.body);
            }
            if put_record(&mut inputs.records, INPUT, &payload).is_ok() {
                inputs.count += 1;
            }
        }
        inputs
    }
}

/// Writes the clip of `frames`, `inputs`, `markers` and `dead` to `w`.
/// Returns how many symbols were cut to fit.
///
/// # Errors
///
/// Returns the error `w` failed with.
pub(crate) fn encode(
    w: &mut impl Write,
    frames: &RingSnapshot,
    inputs: &Inputs,
    markers: &[(u64, Marker)],
    dead: &[RangeInclusive<u64>],
) -> io::Result<usize> {
    let flags = if inputs.count > 0 { FLAG_INPUTS } else { 0 };
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&flags.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?;
    let mut cut = 0;
    let mut count = 0u64;
    let mut record = Vec::new();
    for frame in frames.frames() {
        record.clear();
        cut += put_frame(&mut record, &frame)?;
        w.write_all(&record)?;
        count += 1;
    }
    w.write_all(&inputs.records)?;
    for &(frame, marker) in markers {
        let kind: u8 = match marker {
            Marker::EngineRestart => 0,
            Marker::Branch => 1,
            Marker::Invoke => 2,
            _ => continue,
        };
        record.clear();
        let payload = [&frame.to_le_bytes()[..], &[kind]].concat();
        put_record(&mut record, MARKER, &payload)?;
        w.write_all(&record)?;
    }
    for range in dead {
        record.clear();
        let payload = [range.start().to_le_bytes(), range.end().to_le_bytes()].concat();
        put_record(&mut record, DEAD, &payload)?;
        w.write_all(&record)?;
    }
    record.clear();
    let payload = [count.to_le_bytes(), inputs.count.to_le_bytes()].concat();
    put_record(&mut record, END, &payload)?;
    w.write_all(&record)?;
    Ok(cut)
}

/// Appends the keyframe or delta record of `frame` to `out`. Returns how
/// many symbols were cut.
fn put_frame(out: &mut Vec<u8>, frame: &FrameView<'_>) -> io::Result<usize> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&frame.seq.to_le_bytes());
    payload.extend_from_slice(&frame.at_us.to_le_bytes());
    if frame.key {
        payload.extend_from_slice(&frame.area.0.to_le_bytes());
        payload.extend_from_slice(&frame.area.1.to_le_bytes());
    }
    let (x, y) = frame.cursor.unwrap_or_default();
    payload.extend_from_slice(&x.to_le_bytes());
    payload.extend_from_slice(&y.to_le_bytes());
    payload.push(u8::from(frame.cursor.is_some()));
    let mut cut = 0;
    if frame.key {
        for cell in frame.cells() {
            cut += usize::from(put_cell(&mut payload, &cell));
        }
        put_record(out, KEYFRAME, &payload)?;
        return Ok(cut);
    }
    let s = frame.scroll.unwrap_or_else(|| Scroll::new(0..0, 0..0, 0));
    payload.push(u8::from(frame.scroll.is_some()));
    for edge in [s.top, s.bottom, s.left, s.right] {
        payload.extend_from_slice(&edge.to_le_bytes());
    }
    payload.extend_from_slice(&s.by.to_le_bytes());
    let count_at = payload.len();
    payload.extend_from_slice(&0u32.to_le_bytes());
    let mut n = 0u32;
    for cell in frame.cells() {
        payload.extend_from_slice(&cell.x.to_le_bytes());
        payload.extend_from_slice(&cell.y.to_le_bytes());
        cut += usize::from(put_cell(&mut payload, &cell));
        n = n.saturating_add(1);
    }
    if let Some(slot) = payload.get_mut(count_at..count_at + 4) {
        slot.copy_from_slice(&n.to_le_bytes());
    }
    put_record(out, DELTA, &payload)?;
    Ok(cut)
}

/// Appends `cell`'s symbol, colors and attributes. Returns whether its
/// symbol was cut.
fn put_cell(out: &mut Vec<u8>, cell: &CellView<'_>) -> bool {
    let cut = put_short(out, cell.symbol);
    for color in [cell.fg, cell.bg, cell.ul] {
        out.extend_from_slice(&color.to_le_bytes());
    }
    out.extend_from_slice(&cell.modifier.to_le_bytes());
    cut
}

/// Appends one record of `tag` carrying `payload` to `out`.
fn put_record(out: &mut Vec<u8>, tag: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a clip record passed 4 GiB"))?;
    out.push(tag);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Appends `text` as a length byte and its bytes, cut at the last character
/// boundary inside [`CLIP_FIELD_MAX`]. Returns whether it was cut.
fn put_short(out: &mut Vec<u8>, text: &str) -> bool {
    let mut end = text.len().min(CLIP_FIELD_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let kept = text.get(..end).unwrap_or_default();
    out.push(u8::try_from(kept.len()).unwrap_or(u8::MAX));
    out.extend_from_slice(kept.as_bytes());
    end < text.len()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use view_core::model::Model;
    use view_core::update::update;

    use view_core::msg::{Key, MouseInput, Msg};
    use view_tui::dvr::{FrameRing, RingBuilder};

    use super::read::{decode, ClipError};
    use super::*;

    const MAX: usize = 1 << 20;

    /// A cell's every field, owned, so two rings' frames compare.
    type Seen = (u16, u16, String, u32, u32, u32, u16);

    fn seen(frames: &RingSnapshot) -> Vec<String> {
        seen_as(frames, str::to_owned)
    }

    /// Every frame of `frames`, each symbol read through `symbol`.
    fn seen_as(frames: &RingSnapshot, symbol: impl Fn(&str) -> String) -> Vec<String> {
        frames
            .frames()
            .map(|f| {
                let cells: Vec<Seen> = f
                    .cells()
                    .map(|c| (c.x, c.y, symbol(c.symbol), c.fg, c.bg, c.ul, c.modifier))
                    .collect();
                format!(
                    "{} {} {:?} {:?} {} {:?} {cells:?}",
                    f.seq, f.at_us, f.area, f.cursor, f.key, f.scroll
                )
            })
            .collect()
    }

    fn cell(x: u16, y: u16, symbol: &str) -> CellView<'_> {
        CellView::new(x, y, symbol, [0x0300_0102, 0x0200_0011, 0x0100_0009], 0b101)
    }

    /// Three frames: a keyframe, a delta under a shift, a plain delta.
    fn ring() -> FrameRing {
        let mut builder = RingBuilder::new(MAX);
        let key = (0..4u16).flat_map(|y| (0..6u16).map(move |x| (x, y)));
        let glyphs = ["a", "世", "e\u{301}", "│"];
        let cells: Vec<_> = key
            .enumerate()
            .map(|(i, (x, y))| cell(x, y, glyphs[i % glyphs.len()]))
            .collect();
        builder
            .push_key(10, (6, 4), Some((1, 2)), cells.iter().copied())
            .unwrap();
        let shift = Scroll::new(0..3, 0..6, 1);
        builder
            .push_delta(20, None, Some(shift), [cell(0, 3, "z")])
            .unwrap();
        builder
            .push_delta(35, Some((5, 0)), None, [cell(4, 1, "q"), cell(5, 1, "r")])
            .unwrap();
        builder.finish()
    }

    /// A model whose input log holds one input of each kind and whose
    /// timeline carries two marks.
    fn logged() -> Model {
        let mut model = Model::with_term_size(80, 24);
        model.dvr.enable(MAX);
        model.dvr.note_frame(1, 1);
        for msg in [
            Msg::Key(Key {
                notation: "<C-x>".to_owned(),
            }),
            Msg::Paste("pasted\ntext".to_owned()),
            Msg::Mouse(MouseInput {
                button: "left".to_owned(),
                action: "press".to_owned(),
                modifier: "C-".to_owned(),
                row: 3,
                col: 300,
            }),
        ] {
            let _ = update(&mut model, msg);
        }
        model.dvr.note_frame(2, 1);
        let _ = update(
            &mut model,
            Msg::Resized {
                width: 120,
                height: 40,
            },
        );
        model.dvr.note_restart();
        let _ = update(
            &mut model,
            Msg::FeatureInvoke {
                generation: None,
                feature: "dvr".to_owned(),
                verb: "close".to_owned(),
            },
        );
        model
    }

    fn encoded(ring: &mut FrameRing, model: &Model) -> (Vec<u8>, usize) {
        let frames = ring.snapshot().unwrap();
        let inputs = Inputs::new(model.dvr.inputs());
        let mut out = Vec::new();
        let dead = [RangeInclusive::new(2, 3)];
        let cut = encode(&mut out, &frames, &inputs, model.dvr.markers(), &dead).unwrap();
        (out, cut)
    }

    #[test]
    fn clip_round_trips_frames_inputs_and_markers() {
        let mut ring = ring();
        let model = logged();
        let (bytes, cut) = encoded(&mut ring, &model);
        assert_eq!(cut, 0);
        assert_eq!(&bytes[..10], b"VIEWDVR\0\x01\x00");
        let clip = decode(&mut bytes.as_slice(), MAX).unwrap();
        let mut back = clip.ring;
        assert_eq!(
            seen(&back.snapshot().unwrap()),
            seen(&ring.snapshot().unwrap())
        );
        assert_eq!(seen(&ring.snapshot().unwrap()).len(), 3);
        let want: Vec<_> = model
            .dvr
            .inputs()
            .map(|i| i.after_frame)
            .zip(model.dvr.replay_until(u64::MAX))
            .map(|(at, msg)| format!("{at} {msg:?}"))
            .collect();
        let got: Vec<_> = clip
            .inputs
            .iter()
            .map(|(at, msg)| format!("{at} {msg:?}"))
            .collect();
        assert_eq!(got, want);
        assert_eq!(got.len(), 4);
        assert_eq!(clip.markers, model.dvr.markers());
        assert_eq!(clip.markers.len(), 2);
        assert_eq!(clip.dead, [RangeInclusive::new(2, 3)]);
        assert_eq!((clip.dropped, clip.dropped_inputs), (0, 0));
    }

    /// Splices a record of `tag` carrying `payload` in after the header.
    fn with_record(bytes: &[u8], tag: u8, payload: &[u8]) -> Vec<u8> {
        let mut spliced = bytes[..16].to_vec();
        put_record(&mut spliced, tag, payload).unwrap();
        spliced.extend_from_slice(&bytes[16..]);
        spliced
    }

    #[test]
    fn a_decoder_skips_an_unknown_tag_and_refuses_a_newer_version() {
        let mut ring = ring();
        let (bytes, _) = encoded(&mut ring, &logged());
        let spliced = with_record(&bytes, 0x7E, b"later");
        let clip = decode(&mut spliced.as_slice(), MAX).unwrap();
        let mut back = clip.ring;
        assert_eq!(
            seen(&back.snapshot().unwrap()),
            seen(&ring.snapshot().unwrap())
        );
        let mut newer = bytes.clone();
        newer[8] = 2;
        assert!(matches!(
            decode(&mut newer.as_slice(), MAX),
            Err(ClipError::Newer(2))
        ));
        let mut foreign = bytes;
        foreign[0] = b'X';
        assert!(matches!(
            decode(&mut foreign.as_slice(), MAX),
            Err(ClipError::NotAClip)
        ));
    }

    #[test]
    fn a_clip_without_its_end_record_is_refused() {
        let mut ring = ring();
        let (bytes, _) = encoded(&mut ring, &logged());
        let end = bytes.len() - 21;
        assert_eq!(bytes[end], END);
        for cut in [end, end + 4, bytes.len() - 1, 30] {
            assert!(
                matches!(decode(&mut &bytes[..cut], MAX), Err(ClipError::Truncated)),
                "cut at {cut}"
            );
        }
        let trailing = [bytes.as_slice(), &[KEYFRAME]].concat();
        assert!(matches!(
            decode(&mut trailing.as_slice(), MAX),
            Err(ClipError::Malformed(_))
        ));
    }

    #[test]
    fn a_symbol_longer_than_its_length_byte_is_cut_and_counted() {
        let long = format!("e{}", "\u{301}".repeat(150));
        let mut builder = RingBuilder::new(MAX);
        builder
            .push_key(0, (2, 1), None, [cell(0, 0, &long), cell(1, 0, "b")])
            .unwrap();
        let mut ring = builder.finish();
        let (bytes, cut) = encoded(&mut ring, &Model::with_term_size(80, 24));
        assert_eq!(cut, 1);
        let mut back = decode(&mut bytes.as_slice(), MAX).unwrap().ring;
        let frames = back.snapshot().unwrap();
        let first = frames.frames().next().unwrap();
        let symbols: Vec<String> = first.cells().map(|c| c.symbol.to_owned()).collect();
        assert_eq!(symbols[0].len(), 255, "e and 127 accents");
        assert!(long.starts_with(&symbols[0]));
        assert_eq!(symbols[1], "b");
    }

    /// The offset, tag and payload length of every record after the header.
    fn records(bytes: &[u8]) -> Vec<(usize, u8, usize)> {
        let mut at = 16;
        let mut out = Vec::new();
        while at < bytes.len() {
            let len: [u8; 4] = bytes[at + 1..at + 5].try_into().unwrap();
            let len = usize::try_from(u32::from_le_bytes(len)).unwrap();
            out.push((at, bytes[at], len));
            at += 5 + len;
        }
        out
    }

    /// A model whose inputs, marks and abandoned range all name frames
    /// `first` and `last`.
    fn logged_over(first: u64, last: u64) -> Model {
        let mut model = Model::with_term_size(80, 24);
        model.dvr.enable(MAX);
        model.dvr.note_frame(first, first);
        let _ = update(&mut model, Msg::Paste("one".to_owned()));
        model.dvr.note_restart();
        model.dvr.note_frame(last, first);
        let _ = update(&mut model, Msg::Paste("two".to_owned()));
        model
    }

    /// The clip of `ring`, with inputs, a mark and an abandoned range on its
    /// first and last frames, and the frames it was taken from.
    fn clip_of(ring: &mut FrameRing) -> (Vec<u8>, Vec<String>) {
        let frames = ring.snapshot().unwrap();
        let seqs: Vec<u64> = frames.frames().map(|f| f.seq).collect();
        let (first, last) = (seqs[0], seqs[seqs.len() - 1]);
        let model = logged_over(first, last);
        let inputs = Inputs::new(model.dvr.inputs());
        let mut bytes = Vec::new();
        let dead = [std::ops::RangeInclusive::new(first, last)];
        encode(&mut bytes, &frames, &inputs, model.dvr.markers(), &dead).unwrap();
        (bytes, seen(&frames))
    }

    /// Asserts that `clip` reads back the frames `taken` lists, less those
    /// `dropped` names, under the numbers they were recorded with, and that
    /// every input, mark and abandoned range names a frame the clip holds.
    fn agrees_on_seq(taken: Vec<String>, clip: read::Clip, dropped: &[u64]) {
        let mut back = clip.ring;
        let want: Vec<_> = taken
            .into_iter()
            .filter(|f| !dropped.iter().any(|s| f.starts_with(&format!("{s} "))))
            .collect();
        assert_eq!(seen(&back.snapshot().unwrap()), want);
        let held = |seq: u64| back.age(seq).is_some();
        assert_eq!(clip.inputs.len(), 2);
        assert!(
            clip.inputs.iter().all(|(at, _)| held(*at)),
            "{:?}",
            clip.inputs
        );
        assert!(!clip.markers.is_empty());
        assert!(
            clip.markers.iter().all(|(at, _)| held(*at)),
            "{:?}",
            clip.markers
        );
        assert!(clip
            .dead
            .iter()
            .all(|range| held(*range.start()) && held(*range.end())));
    }

    #[test]
    fn a_clip_keeps_the_numbers_its_frames_were_recorded_under() {
        // a ring whose first groups were evicted before the export
        let mut builder = RingBuilder::new(64 << 10);
        for at in 0..12u64 {
            builder
                .push_key(at, (6, 4), None, [cell(0, 0, "k")])
                .unwrap();
        }
        let (bytes, taken) = clip_of(&mut builder.finish());
        assert!(!taken[0].starts_with("1 "), "nothing was evicted");
        let clip = decode(&mut bytes.as_slice(), MAX).unwrap();
        agrees_on_seq(taken, clip, &[]);

        // a frame the reader cannot build, and the delta after it, dropped
        // mid-clip: a keyframe, two deltas, then a keyframe and a delta
        let mut builder = RingBuilder::new(MAX);
        builder
            .push_key(0, (6, 4), None, [cell(0, 0, "a")])
            .unwrap();
        builder
            .push_delta(1, None, None, [cell(1, 0, "b")])
            .unwrap();
        builder
            .push_delta(2, None, None, [cell(2, 0, "c")])
            .unwrap();
        builder
            .push_key(3, (6, 4), None, [cell(3, 0, "d")])
            .unwrap();
        builder
            .push_delta(4, None, None, [cell(4, 0, "e")])
            .unwrap();
        let (mut bytes, taken) = clip_of(&mut builder.finish());
        // the first delta's shift reaches past the screen
        let (at, _, _) = records(&bytes)[1];
        let shift = at + 5 + 8 + 8 + 5;
        bytes[shift] = 1;
        bytes[shift + 3..shift + 5].copy_from_slice(&999u16.to_le_bytes());
        let clip = decode(&mut bytes.as_slice(), MAX).unwrap();
        assert_eq!(clip.dropped, 2);
        agrees_on_seq(taken, clip, &[2, 3]);
    }

    #[test]
    fn frames_out_of_order_are_refused() {
        let mut ring = ring();
        let (bytes, _) = encoded(&mut ring, &logged());
        let frames: Vec<_> = records(&bytes)
            .into_iter()
            .filter(|(_, tag, _)| matches!(*tag, KEYFRAME | DELTA))
            .collect();
        // the second frame numbered as the first, then the third numbered
        // two past the second
        for (index, seq) in [(1, 1u64), (2, 4)] {
            let mut damaged = bytes.clone();
            let at = frames[index].0 + 5;
            damaged[at..at + 8].copy_from_slice(&seq.to_le_bytes());
            assert!(
                matches!(
                    decode(&mut damaged.as_slice(), MAX),
                    Err(ClipError::Malformed("frames out of order"))
                ),
                "frame {index} as {seq}"
            );
        }
    }

    /// A reader counting the bytes taken from it.
    struct Counted<'a> {
        bytes: &'a [u8],
        taken: usize,
    }

    impl std::io::Read for Counted<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.bytes.read(buf)?;
            self.taken += n;
            Ok(n)
        }
    }

    #[test]
    fn a_record_past_the_recording_bound_is_refused_unread() {
        let mut ring = ring();
        let (bytes, _) = encoded(&mut ring, &logged());
        let mut huge = bytes[..16].to_vec();
        huge.push(KEYFRAME);
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        huge.resize(huge.len() + (1 << 20), 0);
        let mut reader = Counted {
            bytes: &huge,
            taken: 0,
        };
        assert!(matches!(
            decode(&mut reader, MAX),
            Err(ClipError::Malformed(
                "a record is larger than the recording bound"
            ))
        ));
        assert_eq!(reader.taken, 21, "only the header and the record head");

        let mut payload = [1u64.to_le_bytes(), 0u64.to_le_bytes()].concat();
        payload.extend_from_slice(&u16::MAX.to_le_bytes());
        payload.extend_from_slice(&u16::MAX.to_le_bytes());
        payload.extend_from_slice(&[0; 5 + 64]);
        let wide = with_record(&bytes, KEYFRAME, &payload);
        assert!(matches!(
            decode(&mut wide.as_slice(), MAX),
            Err(ClipError::Malformed(
                "a keyframe holds fewer cells than its size"
            ))
        ));
    }

    #[test]
    fn inputs_past_the_input_share_and_of_an_unknown_kind_are_dropped() {
        let mut builder = RingBuilder::new(MAX);
        builder
            .push_key(0, (2, 1), None, [cell(0, 0, "a")])
            .unwrap();
        let mut ring = builder.finish();
        let (bytes, _) = encoded(&mut ring, &logged());
        let later = [&7u64.to_le_bytes()[..], &[4, 1, 2, 3]].concat();
        let mut spliced = with_record(&bytes, INPUT, &later);
        // the end record of the newer build counts the input it wrote
        let end = spliced.len() - 8;
        spliced[end..].copy_from_slice(&5u64.to_le_bytes());
        let clip = decode(&mut spliced.as_slice(), MAX).unwrap();
        assert_eq!((clip.inputs.len(), clip.dropped_inputs), (4, 1));
        // a bound whose input share holds the key and the paste alone
        let tight = decode(&mut bytes.as_slice(), 400).unwrap();
        assert_eq!((tight.inputs.len(), tight.dropped_inputs), (2, 2));
    }

    /// A ring painted through the terminal: typing, a scroll, a wide
    /// character and a 300-byte cluster, over more frames than one group
    /// holds.
    fn painted() -> FrameRing {
        use view_core::grid::GridOp;
        let mut model = Model::with_term_size(20, 6);
        model.engine.apply_grid(GridOp::Resize {
            width: 20,
            height: 6,
        });
        let mut term = view_tui::terminal::Term::frame_probe(model.caps);
        let mut ring = FrameRing::new(64 << 20);
        let long = format!("e{}", "\u{301}".repeat(150));
        // one cell per entry, a wide character's second column empty
        let put = |row: u16, cells: Vec<String>| GridOp::PutLine {
            row,
            col_start: 0,
            cells: cells.into_iter().map(|c| (c, 0, 1)).collect(),
        };
        let owned = |cells: &[&str]| cells.iter().map(|c| (*c).to_owned()).collect();
        for i in 0..300u16 {
            let op = match i % 4 {
                // a full row per cycle, so the shift moves more cells than a row
                0 => put(
                    3,
                    format!("{i:04}")
                        .repeat(5)
                        .chars()
                        .map(String::from)
                        .collect(),
                ),
                1 => GridOp::Scroll {
                    top: 0,
                    bot: 4,
                    left: 0,
                    right: 20,
                    rows: 1,
                },
                2 => put(0, owned(&["世", "", "界", ""])),
                _ => put(1, owned(&[&long, "x"])),
            };
            model.engine.apply_grid(op);
            let damage = model.take_paint_damage();
            let surface = view_surface::render(&model);
            term.probe_paint(&model, &surface, &damage).unwrap();
            let at = std::time::Duration::from_millis(u64::from(i));
            assert!(term.record_frame(&mut ring, at).is_some(), "frame {i}");
        }
        ring
    }

    /// `text` as a clip stores it, cut to [`CLIP_FIELD_MAX`] bytes.
    fn clipped(text: &str) -> String {
        let mut out = Vec::new();
        let _ = put_short(&mut out, text);
        String::from_utf8(out.split_off(1)).unwrap()
    }

    #[test]
    fn frames_painted_through_the_terminal_round_trip_across_groups() {
        let mut ring = painted();
        let frames = ring.snapshot().unwrap();
        let keys = frames.frames().filter(|f| f.key).count();
        assert!((2..300).contains(&keys), "{keys} keyframes in 300 frames");
        assert!(frames.frames().any(|f| f.scroll.is_some()), "a shift");
        let mut bytes = Vec::new();
        let cut = encode(&mut bytes, &frames, &Inputs::default(), &[], &[]).unwrap();
        assert!(cut > 0, "the 300-byte cluster reached the clip");
        let want = seen_as(&frames, clipped);
        drop(frames);
        assert_eq!(want.len(), 300);
        let mut back = decode(&mut bytes.as_slice(), 64 << 20).unwrap().ring;
        assert_eq!(seen(&back.snapshot().unwrap()), want);
    }
}
