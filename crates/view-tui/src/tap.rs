//! Monotonic-timestamp tap at this crate's paint boundary (frame
//! content written to the terminal), compiled only on unix under the
//! `bench-taps` feature; a plain build contains none of this module, so
//! shipping binaries carry zero tap cost by construction, and a Windows
//! build carries none at all because the channel below is a unix
//! mechanism.
//!
//! Records are one ASCII line each, `<tag> <seq> <nanos>\n`, written to
//! the file descriptor named by `VIEW_BENCH_TAP_FD`. The descriptor is
//! reached by opening `/dev/fd/<N>` with `O_NONBLOCK` rather than
//! adopting the raw fd: adoption requires `unsafe`, which the workspace
//! denies, and the reopen reaches the same pipe object. A full pipe
//! drops the record instead of ever blocking the tapped thread (the
//! paint loop and the RPC threads must never stall on measurement); the
//! per-process sequence number lets the harness detect any drop as a gap
//! instead of silently mispairing records.
//!
//! `view-engine` carries a structurally identical module for its RPC
//! boundaries: the only crate both could share is `view-core`, which
//! stays free of I/O, so each side of the RPC/terminal split keeps its
//! own copy rather than bending the dependency direction for a test-only
//! feature.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// A frame's content bytes written and flushed to the terminal. A late
/// (conservative) reading of the "first byte of the output flush"
/// boundary: the tap fires after the frame's single coalesced
/// write+flush returns, so measured intervals overstate, never
/// understate, latency.
pub const TAG_TERM_WRITTEN: u8 = b'T';
/// A key event handed to view by the host terminal, before view has done
/// anything with it. The first instant view owns the keystroke:
/// everything earlier is the OS pty read and the terminal-byte parse,
/// neither of which any view code schedules, which is why this and not
/// the harness's pty write is the opening boundary of the input-path
/// budget.
///
/// It fires ahead of the encode rather than after it, and so also fires
/// for the key data view drops (releases, codes with no nvim
/// equivalent). Bracketing the encode separately was measured and
/// abandoned: with the encode between them a tap pair read 14.3
/// microseconds p50 against 16.1 with nothing between them, so the
/// encode is smaller than the instrumentation's own noise and a second
/// tap would have cost the gated interval more than the segment it
/// resolved.
pub const TAG_KEY_READ: u8 = b'K';
/// The runtime loop dequeued one message (any kind); emitted by the bin
/// crate's loop through this module so the loop-wakeup boundary shares
/// the paint tag's sequence counter.
pub const TAG_LOOP_WAKE: u8 = b'U';
/// A frame draw is starting (before compositing and the backend diff);
/// with [`TAG_TERM_WRITTEN`] this brackets the paint's own CPU cost.
pub const TAG_DRAW_START: u8 = b'B';
/// The frame's coalesced bytes are fully built and the single pty
/// `write_all`+`flush` is about to run; with [`TAG_DRAW_START`] this
/// brackets the composite+diff+encode CPU, and with [`TAG_TERM_WRITTEN`]
/// it isolates the pty write+flush cost on its own, directly rather than
/// by subtraction.
pub const TAG_FLUSH_START: u8 = b'F';
/// The frame's mode toggles are queued and its overlay rows resolved,
/// immediately before its paint area is resolved; with [`TAG_DRAW_START`]
/// this brackets the frame preamble on its own.
pub const TAG_FRAME_PREPARED: u8 = b'P';
/// The frame's paint area has been resolved from the model's terminal
/// size. With [`TAG_FRAME_PREPARED`] this isolates area resolution from
/// everything around it; it bracketed a live size syscall until the area
/// became a plain `Rect` construction, and still marks that boundary so a
/// syscall reappearing there shows up as a stage regression.
pub const TAG_AREA_RESOLVED: u8 = b'G';
/// The frame now being drawn carries at least one cell a live prediction
/// put there, so the [`TAG_TERM_WRITTEN`] that closes this frame is a write
/// speculation explains rather than one nothing does.
///
/// Emitted at the head of the frame instead of beside the write it
/// qualifies, for two reasons: the answer is already known there (the
/// surface handed to the painter is the frame), and the write's own bracket
/// ([`TAG_FLUSH_START`] to [`TAG_TERM_WRITTEN`]) isolates the pty write
/// cost, which a tap landing inside it would inflate. A reader pairs it
/// with the next terminal write, which is this frame's.
pub const TAG_SPECULATED_PAINT: u8 = b'D';
/// The frame now being drawn paints the agent panel and carries no engine
/// grid damage at all, so the write closing it is view's own surfaces
/// repainting -- a streamed agent chunk, most of all -- rather than
/// anything the keystroke did.
///
/// Paired with the next terminal write exactly as [`TAG_SPECULATED_PAINT`]
/// is, and deliberately silent on any frame that also carries grid damage:
/// such a frame is at least partly the engine's answer, and announcing it
/// would explain away a paint the engine is entitled to be blamed for.
pub const TAG_AGENT_PAINT: u8 = b'A';
/// This frame's damaged rows are composited into the shadow. With
/// [`TAG_AREA_RESOLVED`] this brackets damage resolution and compositing,
/// and with [`TAG_FLUSH_START`] the backend diff and escape encode, so
/// the two halves of the paint's CPU are separable rather than pooled.
pub const TAG_COMPOSED: u8 = b'C';

static SEQ: AtomicU64 = AtomicU64::new(0);
static SINK: OnceLock<Option<File>> = OnceLock::new();

fn open_sink() -> Option<File> {
    let fd: u32 = std::env::var("VIEW_BENCH_TAP_FD").ok()?.parse().ok()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    options.custom_flags(i32::try_from(rustix::fs::OFlags::NONBLOCK.bits()).unwrap_or_default());
    options.open(format!("/dev/fd/{fd}")).ok()
}

/// Writes one tap record; a no-op when `VIEW_BENCH_TAP_FD` is unset or
/// unopenable, and lossy-not-blocking when the pipe is full.
pub fn tap(tag: u8) {
    let Some(sink) = SINK.get_or_init(open_sink).as_ref() else {
        return;
    };
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let nanos = now
        .tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(now.tv_nsec);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut buf = [0_u8; 64];
    let mut at = buf.len();
    push_byte(&mut buf, &mut at, b'\n');
    push_decimal(&mut buf, &mut at, u64::try_from(nanos).unwrap_or(0));
    push_byte(&mut buf, &mut at, b' ');
    push_decimal(&mut buf, &mut at, seq);
    push_byte(&mut buf, &mut at, b' ');
    push_byte(&mut buf, &mut at, tag);
    // one write syscall per record: pipe writes below PIPE_BUF are
    // atomic, so records from different threads never interleave
    let _ = (&mut &*sink).write(&buf[at..]);
}

/// Prepends one byte to the record being built backwards in `buf`.
fn push_byte(buf: &mut [u8; 64], at: &mut usize, byte: u8) {
    *at = at.saturating_sub(1);
    buf[*at] = byte;
}

/// Prepends `n`'s decimal digits to the record being built backwards in
/// `buf`. The record is formatted into a stack buffer rather than through
/// `format!` because the tap runs on the paint and RPC threads, where a
/// heap allocation is both a cost the measurement would charge to the
/// code under test and a lock the allocator can contend on.
fn push_decimal(buf: &mut [u8; 64], at: &mut usize, n: u64) {
    let mut n = n;
    loop {
        push_byte(buf, at, b'0' + u8::try_from(n % 10).unwrap_or(0));
        n /= 10;
        if n == 0 {
            return;
        }
    }
}
