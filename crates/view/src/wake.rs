//! The runtime loop's wake seam. On unix the loop sleeps in an fd
//! readiness poll (terminal fd, SIGWINCH pipe, and the wake pipe here)
//! rather than in `Receiver::recv`, so a bare channel send can no longer
//! wake it: [`LoopSender`] pairs every send with a [`LoopWaker`] signal,
//! and [`poll_readiness`] is the poll itself. Off unix the loop still
//! blocks in `recv` and [`LoopSender`] degrades to a plain forwarding
//! wrapper.

use std::sync::mpsc;
use view_core::msg::Msg;
use view_core::sink::MsgSink;

#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::sync::Arc;

/// A self-pipe the runtime loop polls alongside the terminal fd, signaled
/// by every [`LoopSender`] send so a queued message interrupts the poll
/// exactly the way it used to interrupt `recv`.
///
/// The `pending` flag dedups producer-side writes: a signal while one is
/// already outstanding skips the pipe write, so the pipe's occupancy stays
/// bounded by the number of concurrently-signaling producers and a
/// non-blocking write can never find it full. The flag is *only* that
/// dedup -- [`clear`](Self::clear) drains the pipe unconditionally, never
/// gated on the flag, because a producer can be preempted between its flag
/// swap and its pipe write: a clear racing through that window would
/// otherwise leave a byte the flag no longer describes, and a
/// level-triggered poll over an undrained byte is a busy loop.
#[cfg(unix)]
#[derive(Clone)]
pub struct LoopWaker(Arc<WakerShared>);

#[cfg(unix)]
struct WakerShared {
    pending: AtomicBool,
    read: OwnedFd,
    write: OwnedFd,
}

/// Creates the wake pipe's two ends with `O_CLOEXEC`/`O_NONBLOCK` already
/// set on both, matching `pipe2`'s atomic guarantee.
#[cfg(all(unix, not(target_vendor = "apple")))]
fn new_wake_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    )?)
}

// macOS has no atomic pipe2-equivalent syscall, so rustix compiles
// `pipe_with`/`PipeFlags` out entirely on apple targets (see rustix's
// `pipe.rs`: both are `#[cfg(not(apple))]`). The fallback below sets the
// same two flags non-atomically, one `fcntl` call per fd, after a plain
// `pipe()`. That gap between creation and flagging is safe because a
// `LoopWaker` is always constructed before any subprocess is spawned and
// before any other thread exists -- there is no fork/exec in the
// program's lifetime yet that could inherit these fds un-flagged.
#[cfg(target_vendor = "apple")]
fn new_wake_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let (read, write) = rustix::pipe::pipe()?;
    set_cloexec_nonblock(&read)?;
    set_cloexec_nonblock(&write)?;
    Ok((read, write))
}

#[cfg(target_vendor = "apple")]
fn set_cloexec_nonblock(fd: &OwnedFd) -> std::io::Result<()> {
    let mut fd_flags = rustix::io::fcntl_getfd(fd)?;
    fd_flags.insert(rustix::io::FdFlags::CLOEXEC);
    rustix::io::fcntl_setfd(fd, fd_flags)?;

    let mut status_flags = rustix::fs::fcntl_getfl(fd)?;
    status_flags.insert(rustix::fs::OFlags::NONBLOCK);
    Ok(rustix::fs::fcntl_setfl(fd, status_flags)?)
}

#[cfg(unix)]
impl LoopWaker {
    /// Creates the wake pipe, both ends non-blocking: the write side must
    /// never block a producer (the RPC reader thread signals through
    /// here), and the read side is drained speculatively by
    /// [`clear`](Self::clear).
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the pipe cannot be
    /// created.
    pub fn new() -> std::io::Result<Self> {
        let (read, write) = new_wake_pipe()?;
        Ok(Self(Arc::new(WakerShared {
            pending: AtomicBool::new(false),
            read,
            write,
        })))
    }

    /// Signals the loop: one non-blocking pipe write per pending
    /// transition. Callable from any thread, never blocks.
    pub fn wake(&self) {
        if !self.0.pending.swap(true, Ordering::AcqRel) {
            loop {
                match rustix::io::write(&self.0.write, &[1_u8]) {
                    Ok(_) => break,
                    Err(rustix::io::Errno::INTR) => {}
                    Err(_) => {
                        // the byte never landed: fold the flag back so the
                        // next signal retries the write instead of trusting
                        // a wakeup that is not in the pipe (the same
                        // disarm-on-refused-send shape the damage pump's
                        // pending flag uses)
                        self.0.pending.store(false, Ordering::Release);
                        break;
                    }
                }
            }
        }
    }

    /// Rearms the waker before the loop re-checks its queue and sleeps:
    /// folds `pending` back and drains the pipe. The caller must re-check
    /// the message queue *after* this returns and before polling -- a send
    /// that lands between this drain and the poll writes a fresh byte and
    /// wakes the poll, but one that landed just before it has already been
    /// consumed here and will never wake anything again.
    pub fn clear(&self) {
        self.0.pending.store(false, Ordering::Release);
        let mut scratch = [0_u8; 64];
        while matches!(rustix::io::read(&self.0.read, &mut scratch), Ok(n) if n > 0) {}
    }

    /// The pipe's read end, for the readiness poll only.
    #[must_use]
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.0.read.as_fd()
    }
}

/// The producer half every thread that feeds the runtime loop holds: the
/// bounded channel's sender plus (on unix) the wake signal that interrupts
/// the loop's readiness poll. Send-then-wake ordering is what makes the
/// loop's clear-then-recheck rearm race-free; see [`LoopWaker::clear`].
#[derive(Clone)]
pub struct LoopSender {
    tx: mpsc::SyncSender<Msg>,
    #[cfg(unix)]
    waker: Option<LoopWaker>,
}

impl LoopSender {
    /// Wraps `tx` with no waker: the receiving loop blocks in `recv` (the
    /// non-unix runtime loop, and every channel-driven test).
    #[cfg(any(not(unix), test))]
    pub fn new(tx: mpsc::SyncSender<Msg>) -> Self {
        Self {
            tx,
            #[cfg(unix)]
            waker: None,
        }
    }

    /// Wraps `tx` with the poll-interrupting waker the unix runtime loop
    /// requires.
    #[cfg(unix)]
    pub fn with_waker(tx: mpsc::SyncSender<Msg>, waker: LoopWaker) -> Self {
        Self {
            tx,
            waker: Some(waker),
        }
    }

    /// The waker this sender signals, if one is wired.
    #[cfg(unix)]
    pub fn waker(&self) -> Option<&LoopWaker> {
        self.waker.as_ref()
    }

    fn signal(&self) {
        #[cfg(unix)]
        if let Some(waker) = &self.waker {
            waker.wake();
        }
    }

    /// Queues `msg` without blocking and signals the waker on success.
    ///
    /// # Errors
    ///
    /// Exactly `SyncSender::try_send`'s: `Full` or `Disconnected`, handing
    /// `msg` back.
    pub fn try_send(&self, msg: Msg) -> Result<(), mpsc::TrySendError<Msg>> {
        self.tx.try_send(msg)?;
        self.signal();
        Ok(())
    }

    /// Queues `msg`, blocking while the channel is full, and signals the
    /// waker on success.
    ///
    /// # Errors
    ///
    /// Exactly `SyncSender::send`'s: `Disconnected`, handing `msg` back.
    pub fn send(&self, msg: Msg) -> Result<(), mpsc::SendError<Msg>> {
        self.tx.send(msg)?;
        self.signal();
        Ok(())
    }
}

impl MsgSink for LoopSender {
    fn try_send(&self, msg: Msg) -> Result<(), mpsc::TrySendError<Msg>> {
        LoopSender::try_send(self, msg)
    }

    fn send(&self, msg: Msg) -> Result<(), mpsc::SendError<Msg>> {
        LoopSender::send(self, msg)
    }
}

/// What one readiness poll reported.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Readiness {
    /// Terminal input or a SIGWINCH is ready: the caller drains the
    /// terminal handle.
    pub input: bool,
    /// The deadline elapsed with nothing ready.
    pub timed_out: bool,
}

/// Polls the terminal's fds and the wake pipe for readiness, sleeping up
/// to `timeout` (`None` sleeps until something is ready). A readiness
/// poll, not a read: every byte stays where it is until the owning side
/// drains it -- and not an RPC await: no nvim reply is ever waited on
/// here, only fd readiness, so the paint loop's never-awaits-RPC rule
/// holds by construction.
///
/// A terminal handle already marked dead keeps its fds out of the set
/// (a hung-up descriptor reports ready forever); the wake pipe alone then
/// still delivers every channel wakeup. `EINTR` retries internally rather
/// than surfacing as a phantom wake or error.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` for any poll failure other
/// than `EINTR`.
#[cfg(unix)]
pub fn poll_readiness(
    input: &view_tui::input::InputSource,
    waker: &LoopWaker,
    timeout: Option<std::time::Duration>,
) -> std::io::Result<Readiness> {
    use rustix::event::{PollFd, PollFlags};

    let timespec = timeout.map(|t| rustix::event::Timespec {
        tv_sec: i64::try_from(t.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: i64::from(t.subsec_nanos()),
    });
    let mut fds = Vec::with_capacity(3);
    fds.push(PollFd::from_borrowed_fd(waker.fd(), PollFlags::IN));
    if !input.is_dead() {
        fds.push(PollFd::from_borrowed_fd(input.tty_fd(), PollFlags::IN));
        fds.push(PollFd::from_borrowed_fd(input.winch_fd(), PollFlags::IN));
    }
    loop {
        match rustix::event::poll(&mut fds, timespec.as_ref()) {
            Ok(0) => {
                return Ok(Readiness {
                    input: false,
                    timed_out: true,
                })
            }
            Ok(_) => {
                let input_ready = fds.iter().skip(1).any(|fd| !fd.revents().is_empty());
                return Ok(Readiness {
                    input: input_ready,
                    timed_out: false,
                });
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(errno) => return Err(errno.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[cfg(unix)]
    fn pipe_has_byte(fd: BorrowedFd<'_>) -> bool {
        let mut scratch = [0_u8; 8];
        matches!(rustix::io::read(fd, &mut scratch), Ok(n) if n > 0)
    }

    #[cfg(unix)]
    #[test]
    fn a_wake_writes_exactly_one_byte_until_cleared() {
        let waker = LoopWaker::new().unwrap();
        waker.wake();
        waker.wake();
        waker.wake();
        assert!(
            pipe_has_byte(waker.fd()),
            "the first wake must reach the pipe"
        );
        assert!(
            !pipe_has_byte(waker.fd()),
            "repeat wakes while pending must not stack bytes, or a flood of \
             sends could fill the pipe and block a producer that must never block"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_wake_after_clear_signals_again() {
        let waker = LoopWaker::new().unwrap();
        waker.wake();
        waker.clear();
        waker.wake();
        assert!(
            pipe_has_byte(waker.fd()),
            "clear must rearm the pending dedup, or the second wakeup is lost"
        );
    }

    #[cfg(unix)]
    #[test]
    fn clear_drains_the_pipe_even_when_the_pending_flag_is_already_false() {
        // the race this pins: a producer preempted between its pending swap
        // and its pipe write lands the byte after a clear has already folded
        // the flag; the next clear must still drain it, or a level-triggered
        // poll spins on a byte nothing will ever read
        let waker = LoopWaker::new().unwrap();
        rustix::io::write(&waker.0.write, &[1_u8]).unwrap();
        assert!(!waker.0.pending.load(Ordering::Acquire));
        waker.clear();
        assert!(
            !pipe_has_byte(waker.fd()),
            "clear must drain unconditionally, not only when pending was set"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_sender_with_a_waker_signals_on_send_and_try_send() {
        let (tx, rx) = mpsc::sync_channel(4);
        let waker = LoopWaker::new().unwrap();
        let sender = LoopSender::with_waker(tx, waker.clone());
        sender.send(Msg::RedrawReady).unwrap();
        assert!(pipe_has_byte(waker.fd()), "send must signal the poll");
        waker.clear();
        sender.try_send(Msg::RedrawReady).unwrap();
        assert!(pipe_has_byte(waker.fd()), "try_send must signal the poll");
        drop(rx);
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_try_send_does_not_signal() {
        let (tx, _rx) = mpsc::sync_channel(1);
        let waker = LoopWaker::new().unwrap();
        let sender = LoopSender::with_waker(tx, waker.clone());
        sender.try_send(Msg::RedrawReady).unwrap();
        waker.clear();
        assert!(sender.try_send(Msg::RedrawReady).is_err());
        assert!(
            !pipe_has_byte(waker.fd()),
            "a send that queued nothing must not wake the loop to find nothing"
        );
    }

    #[test]
    fn a_plain_sender_forwards_without_a_waker() {
        let (tx, rx) = mpsc::sync_channel(1);
        let sender = LoopSender::new(tx);
        sender.send(Msg::RedrawReady).unwrap();
        assert!(matches!(rx.try_recv(), Ok(Msg::RedrawReady)));
    }
}
