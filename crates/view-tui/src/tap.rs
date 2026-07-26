//! Monotonic-timestamp tap at this crate's paint boundary (frame
//! content written to the terminal), compiled only
//! under the `bench-taps` feature; a plain build contains none of this
//! module, so shipping binaries carry zero tap cost by construction.
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// A frame's content bytes written and flushed to the terminal. A late
/// (conservative) reading of the "first byte of the output flush"
/// boundary: the tap fires after the frame's single coalesced
/// write+flush returns, so measured intervals overstate, never
/// understate, latency.
pub const TAG_TERM_WRITTEN: u8 = b'T';
/// A key event decoded off the host terminal, immediately before it is
/// sent to the runtime loop's channel.
pub const TAG_KEY_DECODED: u8 = b'K';
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
/// immediately before the terminal size is queried; with
/// [`TAG_DRAW_START`] this brackets the frame preamble on its own.
pub const TAG_FRAME_PREPARED: u8 = b'P';
/// The frame's paint area has been resolved from the model's terminal
/// size. With [`TAG_FRAME_PREPARED`] this isolates area resolution from
/// everything around it; it bracketed a live size syscall until the area
/// became a plain `Rect` construction, and still marks that boundary so a
/// syscall reappearing there shows up as a stage regression.
pub const TAG_SIZE_PROBED: u8 = b'G';
/// This frame's damaged rows are composited into the shadow. With
/// [`TAG_SIZE_PROBED`] this brackets damage resolution and compositing,
/// and with [`TAG_FLUSH_START`] the backend diff and escape encode, so
/// the two halves of the paint's CPU are separable rather than pooled.
pub const TAG_COMPOSED: u8 = b'C';

static SEQ: AtomicU64 = AtomicU64::new(0);
static SINK: OnceLock<Option<File>> = OnceLock::new();

fn open_sink() -> Option<File> {
    let fd: u32 = std::env::var("VIEW_BENCH_TAP_FD").ok()?.parse().ok()?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .custom_flags(i32::try_from(rustix::fs::OFlags::NONBLOCK.bits()).unwrap_or_default());
    }
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
    let line = format!("{} {seq} {nanos}\n", tag as char);
    // one write syscall per record: pipe writes below PIPE_BUF are
    // atomic, so records from different threads never interleave
    let _ = (&mut &*sink).write(line.as_bytes());
}
