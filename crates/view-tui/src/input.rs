//! The pollable terminal-input handle for the unix runtime loop: exposes
//! the terminal's read fd (and a SIGWINCH self-pipe) for *readiness*
//! polling, plus the non-blocking drain that turns ready bytes into core
//! [`Msg`]s. Only the readiness facts ever leave this crate -- every read
//! and every byte of key decode happens here, behind the same
//! only-view-tui-touches-the-terminal boundary the paint path keeps.
//!
//! This replaces a dedicated input thread blocked in
//! `crossterm::event::read()`: with the runtime loop sleeping in an fd
//! poll instead, a keystroke wakes the loop's own thread directly, and the
//! decoded key never crosses a thread at all. The old shape paid two
//! serialized deep-idle wakes per keystroke (kernel to input thread, then
//! input thread to loop); this one pays exactly the first.

use crate::keys::encode_key;
use crate::mouse::encode_mouse;
use crate::terminal::TermSizeCell;
use crossterm::event::Event;
#[cfg(unix)]
use std::io::IsTerminal;
#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
#[cfg(unix)]
use std::time::Duration;
use view_core::msg::{Key, Msg};

/// The descriptor terminal input arrives on, mirroring crossterm's own
/// choice (`tty_fd()` in its unix backend) so readiness on this fd always
/// describes the same kernel input queue crossterm's reads consume:
/// stdin when it is a terminal, a process-owned `/dev/tty` otherwise.
#[cfg(unix)]
struct TtyFd(OwnedFd);

#[cfg(unix)]
impl TtyFd {
    fn open() -> std::io::Result<Self> {
        if std::io::stdin().is_terminal() {
            // a duplicate rather than a borrow of fd 0: both descriptors
            // name the same terminal input queue, and owning the duplicate
            // gives the poll set a fd whose lifetime is `self`'s instead of
            // a temporary `Stdin` handle's
            let owned = std::io::stdin().as_fd().try_clone_to_owned()?;
            return Ok(Self(owned));
        }
        let file = std::fs::File::open("/dev/tty")?;
        Ok(Self(file.into()))
    }

    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

/// Makes descriptor 0 name this process's terminal when it does not
/// already, reporting whether terminal input has a descriptor to arrive on
/// once this returns.
///
/// `cmd | view -` starts with a pipe on fd 0 and the terminal only on fd 1
/// and fd 2, and two independent readers then have to find the terminal for
/// themselves: this module's [`InputSource`], and crossterm, whose choice
/// is internal and keyed solely off `isatty(0)`. Both fall back to opening
/// `/dev/tty`, and on macOS that descriptor cannot be watched by anything.
/// `poll(2)` answers `POLLNVAL` and a kqueue `EVFILT_READ` registration
/// fails with `EINVAL`, for the very same terminal that answers normally
/// through fd 1. crossterm's event reader is built once per process and
/// cached, so its registration failing there is permanent: the session
/// paints, and not one keystroke ever arrives.
///
/// Putting the terminal on fd 0 removes the fork rather than patching it.
/// Every reader then resolves the one descriptor the shell already opened
/// on the tty device, which every readiness mechanism on every platform can
/// watch, and none of them reaches for `/dev/tty` at all. nvim does the
/// same thing, for the same reason, when its own stdin is a pipe.
///
/// The piped content is not lost to this: `main` duplicates fd 0 for the
/// engine's stdin relay before calling here, and a session that never asked
/// for `-` has no use for those bytes in the first place.
///
/// `/dev/tty` remains the last resort, for a session whose three standard
/// descriptors are all redirected. It is last because it is the one that
/// macOS cannot watch, not because it is least likely.
#[cfg(unix)]
pub fn adopt_terminal_stdin() -> bool {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return true;
    }
    let stdout = std::io::stdout();
    if stdout.is_terminal() && rustix::stdio::dup2_stdin(&stdout).is_ok() {
        return true;
    }
    let stderr = std::io::stderr();
    if stderr.is_terminal() && rustix::stdio::dup2_stdin(&stderr).is_ok() {
        return true;
    }
    // read-write, matching what crossterm's own `/dev/tty` fallback opens:
    // fd 0 is shared with every other reader from here on, and one of them
    // opening the same device with wider access would otherwise be the only
    // difference between two descriptors that must behave identically
    std::fs::File::options()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_ok_and(|tty| rustix::stdio::dup2_stdin(&tty).is_ok())
}

/// One non-blocking drain's outcome, telling the caller whether the
/// terminal side of the poll set is still trustworthy.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Every ready event was decoded and delivered.
    Drained,
    /// The event source itself failed (the terminal is gone): the caller
    /// must stop polling the terminal fd, or a hung-up descriptor turns
    /// the readiness poll into a busy loop.
    SourceLost,
}

/// Creates the SIGWINCH self-pipe's two ends with `O_CLOEXEC`/`O_NONBLOCK`
/// already set on both, matching `pipe2`'s atomic guarantee.
#[cfg(all(unix, not(target_vendor = "apple")))]
fn new_winch_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    )?)
}

// macOS has no atomic pipe2-equivalent syscall, so rustix compiles
// `pipe_with`/`PipeFlags` out entirely on apple targets (see rustix's
// `pipe.rs`: both are `#[cfg(not(apple))]`). The fallback below sets the
// same two flags non-atomically, one `fcntl` call per fd, after a plain
// `pipe()`. That gap between creation and flagging is safe here because
// this runs from `InputSource::open`, called synchronously from `main`
// before the engine (and therefore any subprocess) is spawned and before
// any other thread exists -- there is no fork/exec in the program's
// lifetime yet that could inherit these fds un-flagged.
#[cfg(target_vendor = "apple")]
fn new_winch_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
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

/// The pollable input handle: the terminal read fd plus a SIGWINCH
/// self-pipe, both exposed as borrowed fds for the runtime loop's
/// readiness poll, with all reading and decoding kept behind
/// [`drain`](Self::drain).
///
/// The self-pipe exists because a resize is a signal, not a byte on the
/// tty: crossterm parks SIGWINCH in its own internal signal pipe, which
/// only its `event::poll` inspects, so a loop sleeping in a raw fd poll
/// would never learn of a resize until the next keystroke. Registering a
/// second, crate-owned pipe on the same signal (signal-hook fans one
/// signal out to every registered hook) gives the loop's poll set a
/// readable fd for exactly that moment; the drain that follows lets
/// crossterm translate its own copy of the signal into `Event::Resize`.
#[cfg(unix)]
pub struct InputSource {
    tty: TtyFd,
    winch_read: OwnedFd,
    dead: bool,
}

#[cfg(unix)]
impl InputSource {
    /// Opens the handle: resolves the terminal fd, registers the SIGWINCH
    /// self-pipe, and touches crossterm's event source once so its own
    /// SIGWINCH registration exists from here on (it is created lazily on
    /// first use; a resize delivered before that would otherwise be lost
    /// rather than translated on the next drain).
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the terminal fd or the
    /// signal pipe cannot be set up.
    pub fn open() -> std::io::Result<Self> {
        let tty = TtyFd::open()?;
        let (winch_read, winch_write) = new_winch_pipe()?;
        signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, winch_write)?;
        let _ = crossterm::event::poll(Duration::ZERO);
        Ok(Self {
            tty,
            winch_read,
            dead: false,
        })
    }

    /// The terminal read fd, for readiness polling only.
    #[must_use]
    pub fn tty_fd(&self) -> BorrowedFd<'_> {
        self.tty.as_fd()
    }

    /// The SIGWINCH self-pipe's read end, for readiness polling only.
    #[must_use]
    pub fn winch_fd(&self) -> BorrowedFd<'_> {
        self.winch_read.as_fd()
    }

    /// Whether a prior drain reported the terminal gone; the poll loop
    /// consults this to leave the terminal fd out of its set.
    #[must_use]
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Records the terminal as gone without a drain having said so, for the
    /// caller that learns it from the readiness side instead.
    ///
    /// The case that needs it is a descriptor the readiness mechanism
    /// refuses outright rather than one that hung up: `POLLNVAL` is
    /// level-triggered and returns instantly forever, so a fd that reports
    /// it has to leave the poll set on the spot or the runtime loop spins at
    /// full speed on a terminal it can never read.
    pub fn mark_lost(&mut self) {
        self.dead = true;
    }

    /// Whether an event is already decodable right now, including one no
    /// readiness poll on [`tty_fd`](Self::tty_fd) can ever see.
    ///
    /// One terminal has two readers here: the loop's readiness poll watches
    /// the kernel's tty queue, while crossterm's own reads move bytes out of
    /// that queue and into a userspace buffer of its own. Once bytes have
    /// moved, the kernel queue is empty and the fd is not ready, so a gate
    /// built on the fd alone reports "nothing to drain" while a fully
    /// decoded keystroke sits in crossterm waiting to be handed over. It
    /// stays there until some unrelated later byte re-arms the fd -- and if
    /// the input ended with that burst, forever. Asking crossterm directly
    /// is the half of the terminal's state the descriptor cannot describe.
    ///
    /// The query is itself a zero-timeout crossterm poll, so it can pull
    /// ready kernel bytes into that same buffer. That is deliberate rather
    /// than a leak: bytes this call moves are bytes its own answer already
    /// accounts for. An error reads as "nothing to hand over" -- liveness is
    /// [`drain`](Self::drain)'s call to make, and it makes it as soon as the
    /// fd itself reports the hangup.
    #[must_use]
    pub fn has_buffered(&self) -> bool {
        !self.dead && crossterm::event::poll(Duration::ZERO).unwrap_or(false)
    }

    /// Drains everything ready without blocking: empties the SIGWINCH
    /// self-pipe, then decodes every complete terminal event crossterm has
    /// (or can read) into core [`Msg`]s handed to `sink`, publishing any
    /// resize to `size` before its message is delivered -- the same
    /// publish-before-queue ordering the input thread kept, so no frame
    /// paints at a shape the terminal has left.
    ///
    /// Events with no nvim equivalent (key releases, keys with no
    /// notation) are dropped here, exactly as the input thread dropped
    /// them. A failing event source marks the handle dead and reports
    /// [`DrainOutcome::SourceLost`]; input delivery ends for the session
    /// (matching the input thread, which exited on a read error) while the
    /// engine-side channel keeps the session itself alive.
    pub fn drain(&mut self, size: &TermSizeCell, mut sink: impl FnMut(Msg)) -> DrainOutcome {
        let mut scratch = [0_u8; 64];
        while matches!(rustix::io::read(&self.winch_read, &mut scratch), Ok(n) if n > 0) {}
        loop {
            match crossterm::event::poll(Duration::ZERO) {
                Ok(true) => {}
                Ok(false) => return DrainOutcome::Drained,
                Err(_) => {
                    self.dead = true;
                    return DrainOutcome::SourceLost;
                }
            }
            match crossterm::event::read() {
                Ok(event) => {
                    if let Some(msg) = event_to_msg(event, size) {
                        sink(msg);
                    }
                }
                Err(_) => {
                    self.dead = true;
                    return DrainOutcome::SourceLost;
                }
            }
        }
    }
}

/// Translates one crossterm event into the core [`Msg`] the runtime loop
/// dispatches, or `None` for events with no nvim equivalent. Shared by the
/// unix drain above and the non-unix input thread, so the two platforms
/// cannot drift in what a key, resize, paste, or mouse event becomes.
pub(crate) fn event_to_msg(event: Event, size: &TermSizeCell) -> Option<Msg> {
    match event {
        Event::Key(k) => {
            #[cfg(feature = "bench-taps")]
            crate::tap::tap(crate::tap::TAG_KEY_READ);
            encode_key(&k).map(|notation| Msg::Key(Key { notation }))
        }
        Event::Resize(width, height) => {
            // published before the message is queued: the message may sit
            // behind a burst of keys or redraw tokens, and every frame
            // painted in the meantime would otherwise address the
            // terminal's previous shape
            size.publish(width, height);
            Some(Msg::Resized { width, height })
        }
        Event::Paste(text) => Some(Msg::Paste(text)),
        Event::Mouse(m) => Some(Msg::Mouse(encode_mouse(&m))),
        _ => None,
    }
}
