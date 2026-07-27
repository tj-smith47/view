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
/// somewhere, which is precisely the overtaking this module forbids.
///
/// Taken from the platform rather than written as a number, because the
/// number differs: 4096 on Linux but 512 on macOS and the BSDs. A fixed
/// 4096 would let a 4096-byte inline write start on a macOS pipe holding
/// only 512 bytes of room, and `write_all` would then block the caller
/// mid-message -- the one thing the fast path exists to avoid. RPC
/// notifications carrying a keystroke are tens of bytes, so even the
/// smaller bound costs the fast path nothing that matters.
#[cfg(unix)]
const MAX_INLINE_WRITE: usize = rustix::pipe::PIPE_BUF;

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

    /// Bytes per message on a real pipe.
    ///
    /// Under `PIPE_BUF` on every unix, which is 512 on some and not the
    /// 4096 Linux uses: a pipe that answers "writable" is only promised to
    /// hold `PIPE_BUF` more bytes, and a message above that could block the
    /// test's own inline write instead of measuring the outbox.
    #[cfg(unix)]
    const MESSAGE_LEN: usize = 64;

    /// Bytes a fill loop will push before declaring the pipe unfillable.
    ///
    /// Well past the largest pipe capacity in play (64 KiB by default on
    /// Linux and macOS alike). Reaching it means `poll` never withheld
    /// writability, so no message was ever handed to the writer thread and
    /// the run cannot say anything about ordering between the two paths --
    /// a result to report loudly, not to keep sending through.
    #[cfg(unix)]
    const FILL_CEILING: usize = 4 << 20;

    /// Seconds a test will wait on the pipe for bytes that were sent.
    #[cfg(unix)]
    const READ_WAIT_SECS: i64 = 30;

    /// Sends messages until one is handed to the writer thread, returning
    /// every byte sent.
    ///
    /// This is how a backlog gets arranged rather than hoped for. Nothing
    /// drains the pipe while this runs, so sends take the inline path until
    /// `poll` stops reporting writability, and that first refusal is a
    /// hand-off that has provably happened -- no scheduling decision, and
    /// no reader that happens to keep up, can take it away afterwards.
    #[cfg(unix)]
    fn fill_until_handed_off(outbox: &Outbox) -> Vec<u8> {
        let mut sent: Vec<u8> = Vec::new();
        let mut tag = 0_u8;
        while outbox.path_counts().1 == 0 {
            assert!(
                sent.len() < FILL_CEILING,
                "the pipe accepted {} bytes of inline writes without once \
                 refusing, so nothing was ever handed to the writer thread \
                 and this run proves nothing about ordering across the two \
                 paths",
                sent.len()
            );
            assert!(outbox.send(vec![tag; MESSAGE_LEN]), "send failed");
            sent.extend(std::iter::repeat_n(tag, MESSAGE_LEN));
            tag = (tag + 1) % 251;
        }
        sent
    }

    /// Reads exactly `len` bytes, refusing to wait forever for them.
    ///
    /// A pipe read has no timeout of its own, so a defect that leaves bytes
    /// unwritten would stall the whole suite on a blocked `read` rather than
    /// naming itself. Running out of patience is a failure, not a retry.
    #[cfg(unix)]
    fn read_exactly(pipe: &std::fs::File, len: usize) -> Vec<u8> {
        use rustix::event::{poll, PollFd, PollFlags};
        use std::io::Read;

        let mut got: Vec<u8> = Vec::with_capacity(len);
        let mut buf = vec![0_u8; len];
        while got.len() < len {
            let mut fds = [PollFd::new(pipe, PollFlags::IN)];
            let ready = poll(
                &mut fds,
                Some(&rustix::event::Timespec {
                    tv_sec: READ_WAIT_SECS,
                    tv_nsec: 0,
                }),
            )
            .expect("poll the read end");
            assert!(
                ready > 0,
                "the pipe delivered {} of {len} bytes and then went quiet",
                got.len()
            );
            let remaining = len - got.len();
            let mut source = pipe;
            let n = source.read(&mut buf[..remaining]).expect("read the pipe");
            assert!(
                n > 0,
                "the write end closed after {} of {len} bytes",
                got.len()
            );
            got.extend_from_slice(&buf[..n]);
        }
        got
    }

    /// Bytes of context shown either side of a byte-stream divergence.
    #[cfg(unix)]
    const DIVERGENCE_WINDOW: usize = 3 * MESSAGE_LEN;

    /// Asserts two byte streams match, reporting the first byte they differ
    /// on instead of both streams.
    ///
    /// The streams here run to megabytes, and a bare `assert_eq!` over them
    /// prints both operands in full: the single index that identifies the
    /// reordering ends up buried in megabytes of test output that has to be
    /// searched before the failure can be read at all.
    #[cfg(unix)]
    fn assert_same_bytes(seen: &[u8], expected: &[u8], what: &str) {
        assert_eq!(
            seen.len(),
            expected.len(),
            "{what}: the pipe delivered a different number of bytes than were \
             sent"
        );
        let Some(at) = seen
            .iter()
            .zip(expected)
            .position(|(got, want)| got != want)
        else {
            return;
        };
        let from = at.saturating_sub(DIVERGENCE_WINDOW);
        let to = (at + DIVERGENCE_WINDOW).min(seen.len());
        assert_eq!(
            &seen[from..to],
            &expected[from..to],
            "{what}: first divergence at byte {at} of this stream, within \
             message {} of it, shown from byte {from}",
            at / MESSAGE_LEN
        );
    }

    /// Builds an outbox over a real pipe, returning it with the read end and
    /// the queue the writer thread would drain.
    #[cfg(unix)]
    fn outbox_on_a_pipe() -> (Arc<Outbox>, std::fs::File, mpsc::Receiver<Vec<u8>>) {
        let (read_end, write_end) = rustix::pipe::pipe().expect("pipe");
        let dup: std::os::fd::OwnedFd = write_end.try_clone().expect("dup the write end");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let outbox = Arc::new(Outbox::new(
            Box::new(std::fs::File::from(write_end)),
            tx,
            Some(dup),
        ));
        (outbox, std::fs::File::from(read_end), rx)
    }

    /// The ordering guarantee at the one point it can fail: a message the
    /// writer thread still owns, and a later message sent once the pipe is
    /// writable again.
    ///
    /// Every step is arranged, none is waited for. The pipe is filled until
    /// it refuses an inline write, which hands one message to the thread;
    /// the thread is held back so that message stays unwritten; the pipe is
    /// then drained, so `poll` answers "writable" once more. The next send
    /// therefore meets a writable pipe with a message still queued ahead of
    /// it, and only [`Outbox`]'s own hand-off gate can hold it back. Remove
    /// that gate and the two assertions below fire on every run rather than
    /// on a lucky one.
    #[cfg(unix)]
    #[test]
    fn a_backlogged_pipe_keeps_order_while_both_paths_are_live() {
        let (outbox, reader, rx) = outbox_on_a_pipe();

        // held back rather than left to drain: a message the thread has
        // already written is no longer in front of anything, and the send
        // that must not overtake it needs it still pending
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let thread_outbox = Arc::clone(&outbox);
        std::thread::spawn(move || {
            if release_rx.recv().is_err() {
                return;
            }
            while let Ok(bytes) = rx.recv() {
                if !thread_outbox.write_from_thread(&bytes) {
                    break;
                }
            }
        });

        let mut expected = fill_until_handed_off(&outbox);
        let (inline, threaded) = outbox.path_counts();
        assert!(
            inline > 0,
            "an empty pipe must accept the first send inline"
        );
        assert_eq!(threaded, 1, "the fill stops at the first hand-off");

        // draining what the inline path wrote is what makes the pipe answer
        // "writable" again while the handed-off message is still unwritten
        let inline_bytes = inline * MESSAGE_LEN;
        let head = read_exactly(&reader, inline_bytes);
        assert_same_bytes(
            &head,
            &expected[..inline_bytes],
            "the inline path reordered its own writes",
        );

        let overtaker = u8::try_from((inline + 1) % 251).unwrap_or(0);
        assert!(outbox.send(vec![overtaker; MESSAGE_LEN]), "send failed");
        expected.extend(std::iter::repeat_n(overtaker, MESSAGE_LEN));
        let (inline_after, _) = outbox.path_counts();

        release_tx.send(()).expect("release the writer thread");
        let tail = read_exactly(&reader, expected.len() - inline_bytes);
        assert_same_bytes(
            &tail,
            &expected[inline_bytes..],
            "a message sent while the writer thread still owned an earlier \
             one reached the pipe ahead of it",
        );
        assert_eq!(
            inline_after, inline,
            "the send made while a message was still queued took the inline \
             path; the bytes came out in order this run by luck, not by the \
             gate that is supposed to guarantee it"
        );
    }

    /// The same guarantee under real contention: inline writes from the
    /// caller, queued writes from the writer thread, and a reader draining
    /// the pipe, all at once.
    ///
    /// Arming does not depend on the race. The pipe is filled with nothing
    /// reading it, so a hand-off is a fact before the reader thread exists;
    /// what the contention that follows decides is only which path each
    /// later message takes, never whether both paths ran at all.
    #[cfg(unix)]
    #[test]
    fn both_paths_keep_order_while_a_reader_drains_concurrently() {
        use std::io::Read;

        let (outbox, mut reader, rx) = outbox_on_a_pipe();
        let thread_outbox = Arc::clone(&outbox);
        std::thread::spawn(move || {
            while let Ok(bytes) = rx.recv() {
                if !thread_outbox.write_from_thread(&bytes) {
                    break;
                }
            }
        });

        let mut expected = fill_until_handed_off(&outbox);
        let (inline, threaded) = outbox.path_counts();
        assert!(
            inline > 0,
            "an empty pipe must accept the first send inline"
        );
        assert!(threaded > 0, "the fill stops at the first hand-off");

        let total = expected.len() + CONTENDED_MESSAGES * MESSAGE_LEN;
        let (done_tx, done_rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut seen: Vec<u8> = Vec::with_capacity(total);
            let mut buf = [0_u8; 4096];
            while seen.len() < total {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.extend_from_slice(&buf[..n]),
                }
            }
            let _ = done_tx.send(seen);
        });

        let mut tag = u8::try_from((inline + 1) % 251).unwrap_or(0);
        for _ in 0..CONTENDED_MESSAGES {
            assert!(outbox.send(vec![tag; MESSAGE_LEN]), "send failed");
            expected.extend(std::iter::repeat_n(tag, MESSAGE_LEN));
            tag = (tag + 1) % 251;
        }

        let seen = done_rx
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("the reader drained every byte that was sent");
        let (inline, threaded) = outbox.path_counts();
        assert_same_bytes(
            &seen,
            &expected,
            &format!("the outbox reordered messages: inline {inline}, threaded {threaded}"),
        );
    }

    /// Sends made once the pipe is live and a reader is draining it, enough
    /// for the two paths to interleave many times over.
    #[cfg(unix)]
    const CONTENDED_MESSAGES: usize = 20_000;

    /// The size bound is half of what makes an inline write safe: a pipe
    /// answering "writable" promises room for `PIPE_BUF` bytes and no more,
    /// so a message above that must take the writer thread even when the
    /// pipe is empty and every other condition for going inline holds.
    #[cfg(unix)]
    #[test]
    fn a_message_larger_than_the_pipes_atomic_write_never_goes_inline() {
        let (outbox, reader, _rx) = outbox_on_a_pipe();

        assert!(outbox.send(vec![7_u8; MAX_INLINE_WRITE]), "send failed");
        assert_eq!(
            outbox.path_counts(),
            (1, 0),
            "a message of exactly the atomic-write size fits the promise an \
             empty pipe just made, so it belongs on the inline path"
        );
        let written = read_exactly(&reader, MAX_INLINE_WRITE);
        assert_eq!(written, vec![7_u8; MAX_INLINE_WRITE]);

        assert!(outbox.send(vec![9_u8; MAX_INLINE_WRITE + 1]), "send failed");
        assert_eq!(
            outbox.path_counts(),
            (1, 1),
            "one byte past the atomic-write size the pipe's answer stops \
             being binding, and an inline write could block the caller"
        );
    }
}
