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
//! so a wedged engine costs a background thread rather than the UI: a full
//! pipe stalls the writer thread, and view keeps painting. Two things could
//! move that stall back onto the caller, and both are refused. An inline
//! `write_all` into a pipe with no room: the fast path is taken only after
//! the pipe has said it can accept the write, and only for a message small
//! enough that the accepting answer is binding. And waiting on the lock the
//! writer thread holds across that stalled write: a caller only ever *tries*
//! that lock, and reads a refusal as one more reason to hand the message
//! over. So no path through [`Outbox::send`] waits on the writer, and the
//! longest a caller can spend holding it is one write the pipe already
//! promised to take. That is a claim about this module's own lock and not
//! about every instruction underneath: enqueueing still touches the
//! channel's waker lock and can allocate, neither of which waits on the
//! peer, and both of which were on this path before the fast path existed.

use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, PoisonError, TryLockError};

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

/// The second handle on the engine pipe, held so the inline path can ask
/// about writability without borrowing the writer it would then write
/// through.
#[cfg(unix)]
pub(crate) type PipeHandle = std::os::fd::OwnedFd;

/// Windows: the same second handle, on the write end this process created
/// for the child's stdin. Not a bare `OwnedHandle`: the query behind the
/// fast path is only memory-safe on a handle opened for synchronous I/O, and
/// [`crate::winpipe::SyncPipe`] is the type that makes a handle which is not
/// one unrepresentable here.
#[cfg(windows)]
pub(crate) type PipeHandle = crate::winpipe::SyncPipe;

/// The write side of an engine connection.
pub(crate) struct Outbox {
    /// Serializes writers against each other. Held across a write by
    /// whichever thread is doing it, so its holder may be parked in a write
    /// that cannot finish until nvim reads: a caller therefore only ever
    /// tries to take it, and never waits for it.
    writer: Mutex<Box<dyn Write + Send>>,
    /// Messages handed to the writer thread and not yet written. The
    /// ordering gate reads and writes it while [`Self::writer`] is held,
    /// which is what makes a zero seen there mean "nothing is in flight"
    /// rather than "nothing was in flight a moment ago".
    /// [`Self::write_progress`] deliberately reads it outside that lock: a
    /// report about a writer stuck holding the lock cannot be gated on
    /// acquiring it, and a depth read one message stale is still a depth
    /// this outbox genuinely had.
    handed_off: AtomicUsize,
    /// How many messages each path has taken. The inline path is
    /// conditional on a runtime answer from the pipe, so without a count
    /// there is no way to tell a run that used it from one that silently
    /// fell back to the thread for every message -- which is the difference
    /// between a latency reading that means something and one that does not.
    took_inline: AtomicUsize,
    took_thread: AtomicUsize,
    /// Messages the writer thread has finished writing, counted only after
    /// the bytes are out. Nothing reads its absolute value: it is the
    /// writer's proof of forward motion, so a watcher holding an earlier
    /// reading can tell "wrote something since you last looked" from "has
    /// not moved" without sharing a clock with, or a lock with, a thread
    /// that may be parked inside a write that cannot finish. A lock would
    /// defeat the purpose outright -- the one state worth reporting is the
    /// one in which the writer is stuck holding [`Self::writer`].
    wrote: AtomicU64,
    /// The queue the writer thread drains.
    tx: mpsc::Sender<Vec<u8>>,
    /// The engine pipe, when it is one this platform can ask about
    /// writability. `None` disables the fast path entirely, which is the
    /// correct behaviour for a writer that is not a pipe this platform can
    /// answer for (every in-process test sink) and for platforms that answer
    /// for none.
    #[cfg(any(unix, windows))]
    pipe: Option<PipeHandle>,
}

impl Outbox {
    pub(crate) fn new(
        writer: Box<dyn Write + Send>,
        tx: mpsc::Sender<Vec<u8>>,
        #[cfg(any(unix, windows))] pipe: Option<PipeHandle>,
    ) -> Self {
        Self {
            writer: Mutex::new(writer),
            handed_off: AtomicUsize::new(0),
            took_inline: AtomicUsize::new(0),
            took_thread: AtomicUsize::new(0),
            wrote: AtomicU64::new(0),
            tx,
            #[cfg(any(unix, windows))]
            pipe,
        }
    }

    /// Sends one encoded message, inline when that is provably safe and via
    /// the writer thread otherwise.
    ///
    /// Returns `false` if the connection is gone, matching what a failed
    /// channel send used to mean to every caller.
    pub(crate) fn send(&self, bytes: Vec<u8>) -> bool {
        // `try_lock`, never `lock`: the holder may be the writer thread,
        // parked inside a write for as long as nvim declines to read its
        // stdin. A refusal is not a reason to wait, it is a reason to hand
        // the message over -- which is what the thread is for.
        let claimed = match self.writer.try_lock() {
            Ok(writer) => Some(writer),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        if let Some(mut writer) = claimed {
            // ordering gate: a message already queued for the thread has not
            // been written yet, and writing here would put this one ahead of it
            if self.handed_off.load(Ordering::Acquire) == 0 && self.can_write_inline(bytes.len()) {
                let sent = writer.write_all(&bytes).and_then(|()| writer.flush());
                self.took_inline.fetch_add(1, Ordering::Relaxed);
                drop(writer);
                #[cfg(all(unix, feature = "bench-taps"))]
                crate::tap::tap(crate::tap::TAG_RPC_WRITTEN);
                return sent.is_ok();
            }
            // counted before the lock is released, so no inline writer can see
            // zero while this message is on its way to the thread
            self.count_hand_off();
            drop(writer);
        } else {
            // raised outside the lock, which the refusal says someone else
            // holds. The holder may release before this lands, so a send
            // from some other thread can still go inline in between -- but
            // that send and this one are concurrent, with no order between
            // them for anything to violate. What must hold is that this
            // thread's own next send sees the count, and a thread observes
            // its own writes in order.
            self.count_hand_off();
        }
        if self.tx.send(bytes).is_err() {
            self.handed_off.fetch_sub(1, Ordering::Release);
            return false;
        }
        true
    }

    /// Records one more message as the writer thread's and not yet written.
    fn count_hand_off(&self) {
        self.handed_off.fetch_add(1, Ordering::Release);
        self.took_thread.fetch_add(1, Ordering::Relaxed);
    }

    /// Writes one message from the writer thread, releasing its
    /// hand-off count only after the bytes are out.
    ///
    /// Returns `false` on a broken pipe, which ends the writer thread.
    pub(crate) fn write_from_thread(&self, bytes: &[u8]) -> bool {
        let mut writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let sent = writer.write_all(bytes).and_then(|()| writer.flush());
        if sent.is_ok() {
            // counted here and nowhere else: a write that returned is the
            // only evidence the peer accepted bytes. Counting a hand-off or
            // a write's start instead would report motion for a message the
            // peer never took, which is precisely the state a stall watcher
            // exists to distinguish
            self.wrote.fetch_add(1, Ordering::Relaxed);
        }
        // decremented under the same lock the inline path tests it under:
        // releasing it earlier would let an inline write start while these
        // bytes are still going out
        self.handed_off.fetch_sub(1, Ordering::Release);
        drop(writer);
        if sent.is_err() {
            return false;
        }
        #[cfg(all(unix, feature = "bench-taps"))]
        crate::tap::tap(crate::tap::TAG_RPC_WRITTEN);
        true
    }

    /// Messages the writer thread still owes the peer, paired with the
    /// count of those it has already delivered, in that order.
    ///
    /// Both loads are relaxed and neither takes a lock, so this is
    /// answerable while the writer is parked inside a write that cannot
    /// finish -- the only situation in which the answer matters. Relaxed is
    /// enough because the numbers publish nothing but themselves: a
    /// consumer compares the delivered count against an earlier reading of
    /// its own and reads no other memory on the strength of it. The pair
    /// can likewise be read slightly apart, which costs nothing: a torn
    /// pair reports the state either just before or just after one write,
    /// and both are states this outbox really was in.
    ///
    /// The inline path is deliberately absent from the delivered count: it
    /// runs only while nothing is handed off, so it can never be what
    /// drains a backlog, and a backlog is the only condition under which
    /// the count is consulted at all.
    pub(crate) fn write_progress(&self) -> (usize, u64) {
        (
            self.handed_off.load(Ordering::Relaxed),
            self.wrote.load(Ordering::Relaxed),
        )
    }

    /// How many messages went inline and how many went to the writer
    /// thread, in that order.
    ///
    /// Gated on a platform that has an inline path as well as on `test`:
    /// where there is none, every reader of this would be gated away too
    /// and the method would be dead.
    #[cfg(all(test, any(unix, windows)))]
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

    /// Whether the pipe has already said it can take `len` bytes now.
    ///
    /// Windows reports free buffer space as a byte count rather than as a
    /// readiness bit, so the answer is about this message rather than about
    /// a platform-fixed bound, and no `PIPE_BUF`-style size cap belongs
    /// beside it: a blocking-mode write waits only when the buffer cannot
    /// take the bytes still to go, so a quota of at least `len` promises
    /// the whole message fits. What an accepted answer can commit the
    /// caller to is bounded anyway, since a quota can never exceed the
    /// buffer the pipe was granted.
    ///
    /// Asking does not wait, but only because of where it is asked from. A
    /// synchronous pipe file object queues every operation behind the one
    /// in flight, so a query raised while another thread sat inside a write
    /// to a full pipe would block until that write finished.
    /// [`Self::send`] asks while holding [`Self::writer`] -- the lock every
    /// write to this pipe is made under -- so there is never an operation
    /// in flight for the query to queue behind. A test in this module pins
    /// that rather than leaving it to this paragraph:
    /// `a_caller_does_not_wait_while_the_writer_thread_is_parked_in_a_pipe_write`
    /// hangs if the check is ever hoisted out of the lock.
    #[cfg(windows)]
    fn can_write_inline(&self, len: usize) -> bool {
        let Some(pipe) = &self.pipe else {
            return false;
        };
        pipe.write_quota()
            .and_then(|free| usize::try_from(free).ok())
            .is_some_and(|free| free >= len)
    }

    /// Whether the pipe has already said it can take `len` bytes now.
    ///
    /// Neither unix's readiness poll nor the Windows write quota exists
    /// here, and a write started without one of those answers could park
    /// the caller on a full pipe, so every message goes to the writer
    /// thread exactly as it did before the fast path existed.
    #[cfg(not(any(unix, windows)))]
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

    /// Seconds a `send` is given to return while the writer thread is stuck.
    ///
    /// Wide because it separates two outcomes rather than grading one: a
    /// `send` that does not wait costs microseconds, and a `send` that waits
    /// on the writer thread waits as long as the peer refuses to read, which
    /// is unbounded rather than merely slow. No duration between the two is
    /// reachable, so no threshold in the gap can be too tight.
    const CALLER_PATIENCE_SECS: u64 = 5;

    /// The second half of the module's contract, at the one point it can
    /// fail: the writer thread inside a write that cannot finish, and a
    /// caller sending while it is stuck there.
    ///
    /// Nothing here is timed into existence. The sink announces that it has
    /// been entered, so the stall is a fact before the caller's send begins,
    /// and the send runs on its own thread so a caller that never returns
    /// fails the test instead of hanging the suite.
    #[test]
    fn a_caller_does_not_wait_for_a_writer_thread_stuck_inside_a_write() {
        let (sink, entered_rx, release_tx) = crate::test_peer::ParkedSink::new();
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let outbox = Arc::new(Outbox::new(
            Box::new(sink),
            tx,
            #[cfg(any(unix, windows))]
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

        // with no pipe the fast path is off, so this message is the thread's
        assert!(outbox.send(vec![1]));
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(
                crate::test_peer::PARKED_WRITE_ARM_SECS,
            ))
            .expect("the writer thread reached the sink");

        let caller = Arc::clone(&outbox);
        let (returned_tx, returned_rx) = mpsc::channel::<bool>();
        std::thread::spawn(move || {
            let _ = returned_tx.send(caller.send(vec![2]));
        });
        let returned =
            returned_rx.recv_timeout(std::time::Duration::from_secs(CALLER_PATIENCE_SECS));
        let queued = outbox.handed_off.load(Ordering::Acquire);
        // released before the assertions so a failing run unwedges its own
        // threads rather than leaving them stuck for the rest of the suite
        drop(release_tx);
        assert_eq!(
            returned.ok(),
            Some(true),
            "a send made while the writer thread sat inside a write it could \
             not finish never returned: the caller took on the peer's stall, \
             which is the stall the writer thread exists to absorb"
        );
        assert_eq!(
            queued, 2,
            "the send went behind the message still in flight, so nothing \
             was overtaken to keep the caller free"
        );
    }

    /// Builds an outbox plus a drained writer thread, as `start_with_pipe`
    /// does, so the test exercises the same two paths production uses.
    fn outbox_with_thread(sink: OrderSink) -> Arc<Outbox> {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let outbox = Arc::new(Outbox::new(
            Box::new(sink),
            tx,
            #[cfg(any(unix, windows))]
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
            #[cfg(any(unix, windows))]
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
    /// test's own inline write instead of measuring the outbox. Windows
    /// answers in bytes rather than in blocks, so the same frame needs no
    /// bound of its own there.
    #[cfg(any(unix, windows))]
    const MESSAGE_LEN: usize = 64;

    /// Bytes a fill loop will push before declaring the pipe unfillable.
    ///
    /// Well past the largest pipe capacity in play (64 KiB on every
    /// platform here). Reaching it means the pipe never once refused a
    /// message, so nothing was handed to the writer thread and the run
    /// cannot say anything about ordering between the two paths -- a result
    /// to report loudly, not to keep sending through.
    #[cfg(any(unix, windows))]
    const FILL_CEILING: usize = 4 << 20;

    /// Seconds a test will wait on the pipe for bytes that were sent.
    #[cfg(any(unix, windows))]
    const READ_WAIT_SECS: u64 = 30;

    /// Sends messages until one is handed to the writer thread, returning
    /// every byte sent.
    ///
    /// This is how a backlog gets arranged rather than hoped for. Nothing
    /// drains the pipe while this runs, so sends take the inline path until
    /// `poll` stops reporting writability, and that first refusal is a
    /// hand-off that has provably happened -- no scheduling decision, and
    /// no reader that happens to keep up, can take it away afterwards.
    #[cfg(any(unix, windows))]
    fn fill_until_handed_off(outbox: &Outbox) -> Vec<u8> {
        fill_until_handed_off_with(outbox, |n| {
            vec![u8::try_from(n % 251).unwrap_or(0); MESSAGE_LEN]
        })
    }

    /// The same fill over messages a caller builds, for tests that need to
    /// tell one sender's bytes from another's afterwards.
    #[cfg(any(unix, windows))]
    fn fill_until_handed_off_with(
        outbox: &Outbox,
        mut message: impl FnMut(usize) -> Vec<u8>,
    ) -> Vec<u8> {
        let mut sent: Vec<u8> = Vec::new();
        let mut n = 0_usize;
        while outbox.path_counts().1 == 0 {
            assert!(
                sent.len() < FILL_CEILING,
                "the pipe accepted {} bytes of inline writes without once \
                 refusing, so nothing was ever handed to the writer thread \
                 and this run proves nothing about ordering across the two \
                 paths",
                sent.len()
            );
            let bytes = message(n);
            assert_eq!(bytes.len(), MESSAGE_LEN, "the fill frames are fixed width");
            sent.extend_from_slice(&bytes);
            assert!(outbox.send(bytes), "send failed");
            n += 1;
        }
        sent
    }

    /// Reads exactly `len` bytes, refusing to wait forever for them.
    ///
    /// A pipe read has no timeout of its own, so a defect that leaves bytes
    /// unwritten would stall the whole suite on a blocked `read` rather than
    /// naming itself. The read therefore runs on a thread of its own, over
    /// a second handle on the read end, and running out of patience is a
    /// failure rather than a retry. The count of bytes that did arrive is
    /// published as they land, so a run that goes quiet halfway reports
    /// where it stopped rather than only that it stopped.
    #[cfg(any(unix, windows))]
    fn read_exactly(pipe: &std::fs::File, len: usize) -> Vec<u8> {
        use std::io::Read;

        let mut source = pipe.try_clone().expect("a second handle on the read end");
        let delivered = Arc::new(AtomicUsize::new(0));
        let reader_delivered = Arc::clone(&delivered);
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut got: Vec<u8> = Vec::with_capacity(len);
            let mut buf = vec![0_u8; len];
            while got.len() < len {
                let remaining = len - got.len();
                match source.read(&mut buf[..remaining]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
                reader_delivered.store(got.len(), Ordering::Relaxed);
            }
            let _ = tx.send(got);
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(READ_WAIT_SECS))
            .unwrap_or_default();
        assert_eq!(
            got.len(),
            len,
            "the pipe delivered {} of {len} bytes and then went quiet",
            delivered.load(Ordering::Relaxed)
        );
        got
    }

    /// Bytes of context shown either side of a byte-stream divergence.
    #[cfg(any(unix, windows))]
    const DIVERGENCE_WINDOW: usize = 3 * MESSAGE_LEN;

    /// Asserts two byte streams match, reporting the first byte they differ
    /// on instead of both streams.
    ///
    /// The streams here run to megabytes, and a bare `assert_eq!` over them
    /// prints both operands in full: the single index that identifies the
    /// reordering ends up buried in megabytes of test output that has to be
    /// searched before the failure can be read at all.
    #[cfg(any(unix, windows))]
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

    /// A pipe pair to build an outbox over: the read end first, the write
    /// end second.
    ///
    /// Windows builds it the way a spawn does, because how the pipe was
    /// built is exactly what decides whether the readiness query can answer
    /// on it; on unix any kernel pipe behaves as the engine's does.
    #[cfg(unix)]
    fn engine_pipe() -> (std::fs::File, PipeHandle) {
        let (read_end, write_end) = std::io::pipe().expect("pipe");
        (
            std::fs::File::from(PipeHandle::from(read_end)),
            PipeHandle::from(write_end),
        )
    }

    /// A pipe pair to build an outbox over: the read end first, the write
    /// end second.
    #[cfg(windows)]
    fn engine_pipe() -> (std::fs::File, PipeHandle) {
        let (theirs, ours) = crate::winpipe::child_stdin_pipe().expect("pipe");
        (std::fs::File::from(theirs), ours)
    }

    /// Builds an outbox over a real pipe, returning it with the read end and
    /// the queue the writer thread would drain.
    #[cfg(any(unix, windows))]
    fn outbox_on_a_pipe() -> (Arc<Outbox>, std::fs::File, mpsc::Receiver<Vec<u8>>) {
        let (reader, write_end) = engine_pipe();
        let second = write_end
            .try_clone()
            .expect("a second handle on the write end");
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let outbox = Arc::new(Outbox::new(
            Box::new(std::fs::File::from(write_end)),
            tx,
            Some(second),
        ));
        (outbox, reader, rx)
    }

    /// Bytes in the message that parks the writer thread.
    ///
    /// Past any pipe capacity in play (64 KiB on every platform here), so
    /// with nothing draining the pipe the writer thread cannot finish it and
    /// the lock it holds while trying is held for the rest of the test; and
    /// past every inline bound, so it is handed over rather than taken by the
    /// caller in the first place.
    #[cfg(any(unix, windows))]
    const PARKING_MESSAGE_LEN: usize = 1 << 20;

    /// The caller's freedom proved over a real pipe rather than a sink, which
    /// is what makes it bite on Windows.
    ///
    /// There the readiness question is an operation on a synchronous pipe
    /// file object, and such an object queues every operation behind the one
    /// in flight: asked from outside the writer lock, with the writer thread
    /// parked in a write a full pipe cannot accept, the question would not be
    /// answered until the pipe drained -- which, with nothing reading, is
    /// never. Asking under that same lock is the whole of what keeps it
    /// unreachable, and this test is what holds the check there: hoist it out
    /// of the lock and this run stops taking microseconds and starts hanging.
    ///
    /// Nothing here is timed into existence. The parking message is larger
    /// than the pipe, so the writer thread provably cannot finish it, and the
    /// lock it holds is therefore held from the moment it enters the write
    /// onwards -- a refused `try_lock` is a fact about a parked writer, not a
    /// transient.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_caller_does_not_wait_while_the_writer_thread_is_parked_in_a_pipe_write() {
        let (outbox, reader, rx) = outbox_on_a_pipe();
        assert!(outbox.send(vec![1_u8; PARKING_MESSAGE_LEN]), "send failed");
        assert_eq!(
            outbox.path_counts(),
            (0, 1),
            "a message larger than the pipe belongs to the writer thread on \
             every platform, so this run must start with one queued"
        );

        let thread_outbox = Arc::clone(&outbox);
        std::thread::spawn(move || {
            while let Ok(bytes) = rx.recv() {
                if !thread_outbox.write_from_thread(&bytes) {
                    break;
                }
            }
        });
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(crate::test_peer::PARKED_WRITE_ARM_SECS);
        while !matches!(outbox.writer.try_lock(), Err(TryLockError::WouldBlock)) {
            assert!(
                std::time::Instant::now() < deadline,
                "the writer thread never entered the write it cannot finish, \
                 so nothing was parked for this run to prove a caller free of"
            );
            std::thread::yield_now();
        }

        let caller = Arc::clone(&outbox);
        let (returned_tx, returned_rx) = mpsc::channel::<bool>();
        std::thread::spawn(move || {
            let _ = returned_tx.send(caller.send(vec![2_u8; MESSAGE_LEN]));
        });
        let returned =
            returned_rx.recv_timeout(std::time::Duration::from_secs(CALLER_PATIENCE_SECS));
        // closed before the assertion so a failing run unwedges the parked
        // writer rather than leaving it stuck for the rest of the suite
        drop(reader);
        assert_eq!(
            returned.ok(),
            Some(true),
            "a send made while the writer thread sat inside a write to a full \
             pipe never returned: the readiness question was asked somewhere \
             the pipe's own serialization can make it wait"
        );
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
    #[cfg(any(unix, windows))]
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
    #[cfg(any(unix, windows))]
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
    #[cfg(any(unix, windows))]
    const CONTENDED_MESSAGES: usize = 20_000;

    /// Caller threads sending into one outbox at once.
    #[cfg(any(unix, windows))]
    const CONTENDING_CALLERS: u8 = 2;

    /// Messages each contending caller sends per round.
    #[cfg(any(unix, windows))]
    const MESSAGES_PER_CALLER: u64 = 20_000;

    /// Nanoseconds a contending caller leaves between its sends, one round
    /// per entry.
    ///
    /// The branch under test needs two things at once, and no single rate
    /// supplies both. Sending flat out maximises lock refusals but latches
    /// the outbox into threaded mode within a few hundred messages, after
    /// which the ordering gate closes on the backlog instead of on the
    /// branch. Pacing the callers to the writer thread's own rate keeps the
    /// hand-off count crossing zero all run but lets the callers miss each
    /// other on the lock. Measured against a deliberately broken branch, the
    /// flat-out rate caught it 3 times in 3 and the paced rate 2 times in 5,
    /// so the rate is swept rather than chosen. A host too slow to keep a
    /// pace falls back to sending flat out, which is a covered round rather
    /// than a failure.
    #[cfg(any(unix, windows))]
    const CALLER_GAPS_NANOS: [u64; 3] = [0, 2_000, 5_000];

    /// Stamps a message with its sender and that sender's position in its own
    /// stream, in a fixed-width frame a reader can split without a parser.
    ///
    /// Every write to the pipe is made under one lock and completed before
    /// that lock is released, on the inline path as on the writer thread's,
    /// so the stream a reader sees is whole frames in order and the split is
    /// a division rather than a guess.
    #[cfg(any(unix, windows))]
    fn stamped(caller: u8, seq: u64) -> Vec<u8> {
        let mut bytes = vec![caller; MESSAGE_LEN];
        bytes[1..9].copy_from_slice(&seq.to_le_bytes());
        bytes
    }

    /// The order two concurrent callers are owed: each one's own messages,
    /// in the order that caller produced them.
    ///
    /// This is the interleaving a single-caller test cannot reach. A caller
    /// refused the writer lock takes a branch that raises the hand-off count
    /// from outside that lock, and if the count did not go up, the same
    /// caller's *next* send would find the gate open and go inline past the
    /// message it just queued. Nothing about cross-caller order is asserted,
    /// because concurrent senders establish none.
    #[cfg(any(unix, windows))]
    #[test]
    fn each_callers_own_messages_keep_their_order_while_two_callers_contend() {
        for gap in CALLER_GAPS_NANOS {
            two_callers_keep_their_own_order(gap);
        }
    }

    /// One round of the two-caller ordering check, at one send rate.
    #[cfg(any(unix, windows))]
    fn two_callers_keep_their_own_order(gap_nanos: u64) {
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

        // armed before either caller starts, so both paths are live by
        // construction and no scheduling decision can take that away
        let prologue = fill_until_handed_off_with(&outbox, |n| {
            stamped(CONTENDING_CALLERS, u64::try_from(n).unwrap_or(0))
        });
        let (inline, threaded) = outbox.path_counts();
        assert!(
            inline > 0,
            "an empty pipe must accept the first send inline"
        );
        assert!(threaded > 0, "the fill stops at the first hand-off");

        let total = prologue.len()
            + usize::from(CONTENDING_CALLERS) * MESSAGES_PER_CALLER as usize * MESSAGE_LEN;
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

        let callers: Vec<_> = (0..CONTENDING_CALLERS)
            .map(|caller| {
                let outbox = Arc::clone(&outbox);
                std::thread::spawn(move || {
                    let gap = std::time::Duration::from_nanos(gap_nanos);
                    let mut due = std::time::Instant::now();
                    for seq in 0..MESSAGES_PER_CALLER {
                        while std::time::Instant::now() < due {
                            std::thread::yield_now();
                        }
                        due += gap;
                        assert!(outbox.send(stamped(caller, seq)), "send failed");
                    }
                })
            })
            .collect();
        for caller in callers {
            caller.join().expect("a caller thread finished");
        }

        let seen = done_rx
            .recv_timeout(std::time::Duration::from_secs(120))
            .expect("the reader drained every byte that was sent");
        assert_eq!(
            seen.len(),
            total,
            "the pipe delivered a different number of bytes than were sent"
        );
        let mut next = vec![0_u64; usize::from(CONTENDING_CALLERS) + 1];
        for (frame, bytes) in seen.chunks_exact(MESSAGE_LEN).enumerate() {
            let caller = usize::from(bytes[0]);
            assert!(caller < next.len(), "frame {frame} names no sender");
            let seq = u64::from_le_bytes(bytes[1..9].try_into().unwrap_or([0; 8]));
            assert_eq!(
                seq, next[caller],
                "at a {gap_nanos} ns send gap, caller {caller}'s messages \
                 left the outbox out of their own order at pipe frame {frame}"
            );
            next[caller] += 1;
        }
        let (inline, threaded) = outbox.path_counts();
        assert!(
            inline > 0 && threaded > 0,
            "at a {gap_nanos} ns send gap: inline {inline}, threaded \
             {threaded}. One path never ran, so this round says nothing about \
             order across them"
        );
    }

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

    /// The same bound where Windows draws it. The quota answers in bytes,
    /// so the cutoff is the pipe's own buffer rather than a fixed constant:
    /// a message that size can be promised room when the pipe is empty, and
    /// one byte more can never be promised room at all, however empty the
    /// pipe is or how long the caller waits.
    #[cfg(windows)]
    #[test]
    fn a_message_larger_than_the_pipe_can_hold_never_goes_inline() {
        let (outbox, reader, _rx) = outbox_on_a_pipe();
        let capacity = outbox
            .pipe
            .as_ref()
            .and_then(crate::winpipe::SyncPipe::capacity)
            .and_then(|bytes| usize::try_from(bytes).ok())
            .expect("the pipe reports its buffer size");

        assert!(outbox.send(vec![7_u8; capacity]), "send failed");
        assert_eq!(
            outbox.path_counts(),
            (1, 0),
            "a message of exactly the buffer size fits the room an empty \
             pipe just reported, so it belongs on the inline path"
        );
        let written = read_exactly(&reader, capacity);
        assert_eq!(written, vec![7_u8; capacity]);

        assert!(outbox.send(vec![9_u8; capacity + 1]), "send failed");
        assert_eq!(
            outbox.path_counts(),
            (1, 1),
            "no quota can ever reach one byte past the buffer, so a message \
             that size has no answer that would let it go inline"
        );
    }
}
