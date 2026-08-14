//! Monotonic-timestamp taps at this crate's internal measurement
//! boundaries (RPC bytes written, redraw batch parsed), compiled only on
//! unix under the `bench-taps` feature; a plain build contains none of
//! this module, so shipping binaries carry zero tap cost by
//! construction, and a Windows build carries none at all because the
//! channel below is a unix mechanism.
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
//! `view-tui` carries a structurally identical module for its own paint
//! boundary: the only crate both could share is `view-core`, which stays
//! free of I/O, so each side of the RPC/terminal split keeps its own
//! copy rather than bending the dependency direction for a test-only
//! feature.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// RPC bytes written to the engine (post write+flush).
pub const TAG_RPC_WRITTEN: u8 = b'W';
/// A redraw notification batch fully parsed into `UiEvent`s.
pub const TAG_REDRAW_PARSED: u8 = b'R';
/// An outgoing notification encoded and handed to the writer thread's
/// channel; with [`TAG_RPC_WRITTEN`] this brackets the writer-thread
/// handoff (one scheduler hop plus the pipe write itself).
pub const TAG_RPC_HANDOFF: u8 = b'S';

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
