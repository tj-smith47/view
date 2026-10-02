//! Which frame a keypress produced, and what the writes ahead of it were.
//!
//! A tap row's interval runs from a parsed redraw to the terminal write of
//! the frame carrying it, so pairing those two records out of everything
//! the pipe delivered is its own question, apart from how a row drives the
//! session that produced them.

use super::TapRecord;
use crate::BenchError;

/// The frame a keypress at `t0` produced: the earliest parsed redraw at or
/// after the keypress, paired with the terminal write of the first frame
/// to start at or after that redraw.
///
/// Anchored on the redraw, because a paint can land before the measured
/// frame has begun. view repaints for reasons
/// the engine knows nothing about -- its own chrome answers the keystroke
/// before the engine's redraw arrives -- and such a paint lands after the
/// keypress and before the redraw it does not carry. Selecting the first
/// paint after the keypress and then trying to explain it backwards makes
/// every one of those a special case; selecting the redraw first makes
/// them structurally uninteresting, since a paint before the redraw can
/// never be at or after it.
///
/// A frame already drawing when the redraw was parsed cannot carry it, so
/// its write closes nothing.
///
/// Returns `Ok(None)` while either half is still missing, which is what
/// the sample timeout is measured against: no redraw at all after a
/// keypress is the desync this row aborts on.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] when a second frame starts between the
/// redraw and the write: the redraw's own frame wrote nothing, so the
/// write belongs to a later frame and the interval would time that frame's
/// cause instead.
pub(super) fn measured_frame(
    records: &[TapRecord],
    t0: i64,
) -> Result<Option<(TapRecord, TapRecord)>, BenchError> {
    // earliest by timestamp, since `R` is stamped by the engine and
    // `B`/`T` by the tui, so two records can reach the pipe in the
    // opposite order from the one their clocks record
    let earliest = |tag: u8, from: i64| {
        records
            .iter()
            .filter(|r| r.tag == tag && r.nanos >= from)
            .min_by_key(|r| r.nanos)
            .copied()
    };
    let Some(parsed) = earliest(b'R', t0) else {
        return Ok(None);
    };
    let Some(frame) = earliest(b'B', parsed.nanos) else {
        return Ok(None);
    };
    let Some(paint) = earliest(b'T', frame.nanos) else {
        return Ok(None);
    };
    let draws = records
        .iter()
        .filter(|r| r.tag == b'B' && r.nanos >= parsed.nanos && r.nanos <= paint.nanos)
        .count();
    if draws != 1 {
        return Err(BenchError::Desync {
            context: format!(
                "{draws} frames started between the parsed redraw and the first terminal write \
                 after it: the redraw's own frame wrote nothing, so that write belongs to a later \
                 frame"
            ),
        });
    }
    Ok(Some((parsed, paint)))
}

/// The wait that ends an output-path sample: true on the first `T`, in
/// arrival order, after the first `B` stamped at or after `parsed`.
///
/// When `parsed` is the earliest redraw by stamp this closes on the frame
/// [`measured_frame`] pairs it with. An earlier-stamped `R` can still be
/// crossing the pipe when the wait arms, and then the wait closes on that
/// frame or a later one. The pairing itself is always `measured_frame`'s,
/// read over the records drained once the wait returns.
pub(super) fn closes_the_frame_after(parsed: i64) -> impl FnMut(&TapRecord) -> bool {
    let mut frame_start = None;
    move |r| {
        if frame_start.is_none() && r.tag == b'B' && r.nanos >= parsed {
            frame_start = Some(r.nanos);
        }
        frame_start.is_some_and(|start| r.tag == b'T' && r.nanos >= start)
    }
}

/// The terminal writes that landed after a keypress but before the redraw
/// answering it, split by whether anything explains them.
///
/// Neither count is a measurement fault and neither is gated: both count
/// frames the row's boundary deliberately steps over. The split is what
/// keeps the unexplained half meaning what it always meant -- see
/// [`classify_paints_before_redraw`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaintSplit {
    /// Writes carrying a cell the reconciler had a live prediction for: the
    /// predicted paint, which speculation produces between the keypress and
    /// its authoritative redraw by design.
    pub speculated: usize,
    /// Writes the painter announced as a frame whose whole damage is the
    /// agent panel: the streamed turn painting itself, which under
    /// the AI rows happens on the agent's own cadence and lands inside a
    /// sample window whenever the two coincide.
    pub agent: usize,
    /// Writes with neither a redraw nor a live prediction nor an agent
    /// repaint behind them -- view answering the keystroke from its own
    /// chrome, the regression the output-path row's floor-1 refusal exists
    /// to catch.
    pub unexplained: usize,
}

/// Splits the writes in `[t0, parsed)` into the ones a live prediction
/// explains and the ones nothing does.
///
/// An announcement precedes the write it explains: the painter taps
/// `D` at the head of a frame carrying a predicted cell and `A` inside a
/// frame whose whole damage is the agent panel's rows, and each is
/// consumed by the next write.
///
/// A speculated paint announces itself before it happens: the painter taps
/// `D` at the head of a frame carrying a predicted cell, and that frame's
/// own `T` follows it on the same thread, so the pairing is "the next write
/// after each announcement" rather than a timestamp window that would have
/// to guess how long a frame takes. Announcements are consumed one per
/// write, so a second write behind one announcement is unexplained, which
/// is what keeps a stuck or duplicated `D` from laundering real chrome
/// paints.
///
/// Records are ordered by their own stamps rather than by arrival: `D` and
/// `T` are stamped on the paint thread but reach the harness through a pipe
/// the engine's threads write to as well.
pub fn classify_paints_before_redraw(records: &[TapRecord], t0: i64, parsed: i64) -> PaintSplit {
    // An announcement is stamped at the head of the frame whose write it
    // explains, so a frame already in flight when the key was pressed
    // announces before `t0` and writes after it. Announcements are
    // therefore collected from the last write before the keypress -- the
    // boundary of that in-flight frame, everything before it already
    // consumed -- while the writes counted stay the ones inside the
    // window. Cutting announcements at `t0` instead reports the frames
    // that straddle it as explained by nothing, which under a streaming
    // agent turn is most of them.
    let frame_start = records
        .iter()
        .filter(|r| r.tag == b'T' && r.nanos < t0)
        .map(|r| r.nanos)
        .max()
        .unwrap_or(t0);
    let mut window: Vec<&TapRecord> = records
        .iter()
        .filter(|r| match r.tag {
            b'T' => r.nanos >= t0 && r.nanos < parsed,
            b'D' | b'A' => r.nanos >= frame_start && r.nanos < parsed,
            _ => false,
        })
        .collect();
    window.sort_by_key(|r| (r.nanos, r.seq));
    let mut split = PaintSplit::default();
    let mut announced = None;
    for record in window {
        match record.tag {
            b'T' => match announced.take() {
                Some(b'D') => split.speculated += 1,
                Some(_) => split.agent += 1,
                None => split.unexplained += 1,
            },
            // a frame announcing both is a predicted paint first: the
            // prediction is the reason it is on screen ahead of the redraw
            tag => announced = announced.filter(|held| *held == b'D').or(Some(tag)),
        }
    }
    split
}

/// Whether the glyph a sample watched appear between `start` and `seen`
/// was put there by a prediction rather than by the engine's own answer.
///
/// True when a write the painter announced as carrying a predicted cell
/// landed inside the sample's window and ahead of the redraw answering the
/// keystroke -- which is the whole claim a speculated-echo number makes,
/// and the only evidence for it that exists: on screen the predicted glyph
/// and the authoritative one are the same character in the same cell, so
/// nothing the harness parses out of the pty can tell them apart.
///
/// Conservative in both directions it can be wrong. A redraw stamped after
/// `start` that belongs to the previous keystroke closes the window early
/// and reads as unattributed, and a window with no redraw in it at all is
/// bounded by `seen` rather than assumed to be all prediction.
#[must_use]
pub fn answered_by_prediction(records: &[TapRecord], start: i64, seen: i64) -> bool {
    let parsed = records
        .iter()
        .filter(|r| r.tag == b'R' && r.nanos >= start)
        .map(|r| r.nanos)
        .min()
        .unwrap_or(seen);
    classify_paints_before_redraw(records, start, parsed.min(seen)).speculated > 0
}
