//! The engine's write side: every RPC byte view sends leaves through here.
//!
//! A background writer thread used to be the only way out. That costs a
//! cross-thread wake per message, and a wake is charged for bringing an
//! idle core back: measured on this project's own channel primitive, one
//! hop costs 7.8 us after 50 us of idle and 40 us after 10 ms. Steady
//! typing leaves the writer idle for a keystroke interval, so every
//! keystroke paid the deep end of that curve -- 42.5 us p50 of a 163.5 us
//! view-versus-nvim gap, for a write the caller's own thread was already
//! awake to do.
//!
//! So the caller writes the message itself when it provably can, and hands
//! it to the thread when it cannot. Two invariants decide "provably", and
//! neither may be traded for the other:
//!
//! **Nothing may be overtaken.** RPC reaches nvim in the order it was
//! produced or the buffer is corrupted -- keystrokes arriving out of order
//! are silently wrong text, not a crash. The inline path therefore runs
//! only while holding [`Outbox::writer`] *and* seeing no message already
//! handed to the thread, so there is nothing in flight for it to pass.
//!
//! **The caller may not block.** "The paint loop never awaits RPC" exists
//! so a wedged engine costs a background thread rather than the UI: today
//! a full pipe stalls the writer thread and view keeps painting. An inline
//! `write_all` would move that stall onto the paint loop. The fast path is
//! taken only after the pipe has said it can accept the write, and only
//! for a message small enough that the accepting answer is binding.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, PoisonError};

/// Largest message the inline path will attempt.
///
/// POSIX makes a pipe write of at most `PIPE_BUF` bytes atomic, and makes
/// a poll reporting writability mean at least that much room is free. Both
/// halves are needed: without the size bound a writable answer permits a
/// short write, and a short write inline would have to leave its remainder
/// somewhere, which is precisely the overtaking this module forbids. RPC
/// notifications carrying a keystroke are tens of bytes, so the bound
/// costs the fast path nothing that matters.
#[cfg(unix)]
const MAX_INLINE_WRITE: usize = 4096;

/// The write side of an engine connection.
pub(crate) struct Outbox {
    /// Serializes writers against each other. Held across a write, by
    /// whichever thread is doing it.
    writer: Mutex<Box<dyn Write + Send>>,
    /// Messages handed to the writer thread and not yet written. Read and
    /// written only while [`Self::writer`] is held, which is what makes
    /// "zero" mean "nothing is in flight" rather than "nothing was in
    /// flight a moment ago".
    handed_off: AtomicUsize,
    /// How many messages each path has taken. The inline path is
    /// conditional on a runtime answer from the pipe, so without a count
    /// there is no way to tell a run that used it from one that silently
    /// fell back to the thread for every message -- which is the difference
    /// between a latency reading that means something and one that does not.
    took_inline: AtomicUsize,
    took_thread: AtomicUsize,
    /// The queue the writer thread drains.
    tx: mpsc::Sender<Vec<u8>>,
    /// The engine pipe, when it is one this platform can ask about
    /// writability. `None` disables the fast path entirely, which is the
    /// correct behaviour for a writer that is not a pollable pipe (every
    /// in-process test sink) and for platforms without the guarantee.
    #[cfg(unix)]
    pipe: Option<std::os::fd::OwnedFd>,
}

impl Outbox {
    pub(crate) fn new(
        writer: Box<dyn Write + Send>,
        tx: mpsc::Sender<Vec<u8>>,
        #[cfg(unix)] pipe: Option<std::os::fd::OwnedFd>,
    ) -> Self {
        Self {
            writer: Mutex::new(writer),
            handed_off: AtomicUsize::new(0),
            took_inline: AtomicUsize::new(0),
            took_thread: AtomicUsize::new(0),
            tx,
            #[cfg(unix)]
            pipe,
        }
    }

    /// Sends one encoded message, inline when that is provably safe and via
    /// the writer thread otherwise.
    ///
    /// Returns `false` if the connection is gone, matching what a failed
    /// channel send used to mean to every caller.
    pub(crate) fn send(&self, bytes: Vec<u8>) -> bool {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        // ordering gate: a message already queued for the thread has not
        // been written yet, and writing here would put this one ahead of it
        if self.handed_off.load(Ordering::Acquire) == 0 && self.can_write_inline(bytes.len()) {
            let sent = writer.write_all(&bytes).and_then(|()| writer.flush());
            self.took_inline.fetch_add(1, Ordering::Relaxed);
            drop(writer);
            #[cfg(feature = "bench-taps")]
            crate::tap::tap(crate::tap::TAG_RPC_WRITTEN);
            return sent.is_ok();
        }
        // counted before the lock is released, so no inline writer can see
        // zero while this message is on its way to the thread
        self.handed_off.fetch_add(1, Ordering::Release);
        self.took_thread.fetch_add(1, Ordering::Relaxed);
        drop(writer);
        if self.tx.send(bytes).is_err() {
            self.handed_off.fetch_sub(1, Ordering::Release);
            return false;
        }
        true
    }

    /// Writes one message from the writer thread, releasing its
    /// hand-off count only after the bytes are out.
    ///
    /// Returns `false` on a broken pipe, which ends the writer thread.
    pub(crate) fn write_from_thread(&self, bytes: &[u8]) -> bool {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let sent = writer.write_all(bytes).and_then(|()| writer.flush());
        // decremented under the same lock the inline path tests it under:
        // releasing it earlier would let an inline write start while these
        // bytes are still going out
        self.handed_off.fetch_sub(1, Ordering::Release);
        drop(writer);
        if sent.is_err() {
            return false;
        }
        #[cfg(feature = "bench-taps")]
        crate::tap::tap(crate::tap::TAG_RPC_WRITTEN);
        true
    }

    /// How many messages went inline and how many went to the writer
    /// thread, in that order.
    #[cfg(test)]
    pub(crate) fn path_counts(&self) -> (usize, usize) {
        (
            self.took_inline.load(Ordering::Relaxed),
            self.took_thread.load(Ordering::Relaxed),
        )
    }

    /// Whether the pipe has already said it can take `len` bytes now.
    ///
    /// A zero-timeout poll, so this asks rather than waits: an unwritable
    /// pipe falls through to the writer thread instead of stalling the
    /// caller, which is the whole point of the thread still existing.
    #[cfg(unix)]
    fn can_write_inline(&self, len: usize) -> bool {
        use rustix::event::{poll, PollFd, PollFlags};
        if len > MAX_INLINE_WRITE {
            return false;
        }
        let Some(pipe) = &self.pipe else {
            return false;
        };
        let mut fds = [PollFd::new(pipe, PollFlags::OUT)];
        match poll(
            &mut fds,
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        ) {
            Ok(0) | Err(_) => false,
            Ok(_) => fds[0].revents().contains(PollFlags::OUT),
        }
    }

    /// Off unix there is no portable "can this write complete now" answer,
    /// so every message goes to the writer thread exactly as before.
    #[cfg(not(unix))]
    #[allow(clippy::unused_self)]
    fn can_write_inline(&self, _len: usize) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::sync::Arc;

    /// A sink that records the exact byte order it was written in.
    #[derive(Clone, Default)]
    struct OrderSink(Arc<Mutex<Vec<u8>>>);

    impl Write for OrderSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Builds an outbox plus a drained writer thread, as `start_with_pipe`
    /// does, so the test exercises the same two paths production uses.
    fn outbox_with_thread(sink: OrderSink) -> Arc<Outbox> {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let outbox = Arc::new(Outbox::new(
            Box::new(sink),
            tx,
            #[cfg(unix)]
            None,
        ));
        let writer = Arc::clone(&outbox);
        std::thread::spawn(move || {
            while let Ok(bytes) = rx.recv() {
                if !writer.write_from_thread(&bytes) {
                    break;
                }
            }
        });
        outbox
    }

    #[test]
    fn messages_reach_the_pipe_in_the_order_they_were_sent() {
        // the invariant that fails silently: out-of-order keystrokes are
        // wrong text in the buffer, not an error anyone would see
        let sink = OrderSink::default();
        let outbox = outbox_with_thread(sink.clone());
        for i in 0..2000_u16 {
            assert!(outbox.send(vec![u8::try_from(i % 251).unwrap_or(0)]));
        }
        // drain: the writer thread may still hold queued messages
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let len = sink.0.lock().unwrap_or_else(PoisonError::into_inner).len();
            if len == 2000 || std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::yield_now();
        }
        let written = sink
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let expected: Vec<u8> = (0..2000_u16)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        assert_eq!(written, expected, "the outbox reordered messages");
    }

    #[test]
    fn a_send_that_takes_the_thread_is_never_overtaken_by_a_later_inline_send() {
        // the specific race the handed_off counter exists to stop: with the
        // counter removed, an inline write racing a queued message passes it
        let sink = OrderSink::default();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let outbox = Outbox::new(
            Box::new(sink.clone()),
            tx,
            #[cfg(unix)]
            None,
        );
        assert!(outbox.send(vec![1]));
        assert_eq!(
            outbox.handed_off.load(Ordering::Acquire),
            1,
            "with no pipe the fast path must be off, so the message is the thread's"
        );
        assert!(
            sink.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty(),
            "nothing may reach the pipe before the thread writes it"
        );
        let queued = rx.recv().expect("the message was handed to the thread");
        assert_eq!(queued, vec![1]);
        assert!(outbox.write_from_thread(&queued));
        assert_eq!(outbox.handed_off.load(Ordering::Acquire), 0);
    }

    /// Drives the outbox over a real pipe whose reader is deliberately
    /// slower than the sender, so the pipe spends the run oscillating
    /// between full and writable.
    ///
    /// That oscillation is the whole test. A reader that keeps up lets
    /// every message take the inline path, and a run where only one path
    /// ever runs cannot detect reordering between the two. Verified by
    /// removing the `handed_off` gate from [`Outbox::send`]: with the gate
    /// this passes, without it the assertion below fires.
    #[cfg(unix)]
    #[test]
    fn a_backlogged_pipe_keeps_order_while_both_paths_are_live() {
        use std::io::Read;
        use std::os::fd::OwnedFd;

        let (read_end, write_end) = rustix::pipe::pipe().expect("pipe");
        let dup: OwnedFd = write_end.try_clone().expect("dup the write end");
        let writer = std::fs::File::from(write_end);

        let (done_tx, done_rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut r = std::fs::File::from(read_end);
            let mut seen = Vec::with_capacity(MESSAGES);
            let mut buf = [0_u8; 64];
            while seen.len() < MESSAGES {
                match r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.extend_from_slice(&buf[..n]),
                }
                // slower than the sender on purpose: this is what keeps a
                // backlog in front of the writer thread for an unguarded
                // inline write to jump
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
            let _ = done_tx.send(seen);
        });

        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let outbox = Arc::new(Outbox::new(Box::new(writer), tx, Some(dup)));
        let thread_outbox = Arc::clone(&outbox);
        std::thread::spawn(move || {
            while let Ok(bytes) = rx.recv() {
                if !thread_outbox.write_from_thread(&bytes) {
                    break;
                }
            }
        });

        let expected: Vec<u8> = (0..MESSAGES)
            .map(|i| u8::try_from(i % 251).unwrap_or(0))
            .collect();
        for byte in &expected {
            assert!(outbox.send(vec![*byte]), "send failed");
        }
        let seen = done_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("reader drained the pipe");

        let (inline, threaded) = outbox.path_counts();
        assert!(
            inline > 0 && threaded > 0,
            "both paths must run for this to be an ordering proof; inline {inline}, threaded \
             {threaded}"
        );
        assert_eq!(
            seen.len(),
            expected.len(),
            "the pipe delivered a different number of bytes than were sent"
        );
        assert_eq!(
            seen, expected,
            "the outbox reordered messages: inline {inline}, threaded {threaded}"
        );
    }

    /// Enough sends to keep the slow reader behind for the whole run, so
    /// the writer thread always has a backlog in front of it.
    #[cfg(unix)]
    const MESSAGES: usize = 200_000;
}
