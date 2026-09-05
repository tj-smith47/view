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

#[cfg(not(unix))]
use crate::keys::encode_terminal_key;
#[cfg(not(unix))]
use crate::mouse::encode_mouse;
use crate::terminal::TermSizeCell;
#[cfg(not(unix))]
use crossterm::event::Event;
#[cfg(unix)]
use std::io::IsTerminal;
#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
#[cfg(unix)]
use std::time::Duration;
// the reader that upgrades a capability is the unix descriptor loop below
#[cfg_attr(not(unix), allow(unused_imports))]
use view_core::model::TermCaps;
#[cfg(not(unix))]
use view_core::msg::Key;
use view_core::msg::Msg;

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

/// Reads whatever the terminal has ready right now, or `None` when it has
/// nothing (or the read failed).
///
/// The readiness poll is what makes this safe to call on the raw-mode
/// terminal at all: `cfmakeraw` leaves `VMIN=1`, so a read issued against
/// an empty queue blocks until a key is pressed. Asking `poll(2)` with a
/// zero timeout first turns that into "read only what is already there".
#[cfg(unix)]
fn read_ready(fd: BorrowedFd<'_>) -> Option<Vec<u8>> {
    use rustix::event::{PollFd, PollFlags};

    let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
    let ready = matches!(
        rustix::event::poll(&mut fds, Some(&rustix::event::Timespec::default())),
        Ok(n) if n > 0
    );
    if !ready {
        return None;
    }
    let mut buf = [0_u8; 256];
    match rustix::io::read(fd, &mut buf) {
        Ok(0) | Err(_) => None,
        Ok(n) => Some(buf[..n].to_vec()),
    }
}

/// Whether the terminal has hung up: its far end is gone, and every read on
/// this descriptor from here on answers EOF or `EIO`.
///
/// Asked before crossterm is allowed anywhere near the descriptor, because
/// crossterm cannot answer it. Its unix event source breaks its read loop on
/// `WouldBlock` alone and treats every other outcome -- a zero-length read
/// and an `EIO` alike -- as "no event parsed yet, read again", so a hung-up
/// tty holds it inside `poll` at 100% CPU and it never returns the error
/// that would end the session (crossterm 0.29,
/// `event::source::unix::mio::UnixInternalEventSource::try_read`). That is
/// the whole of this defect: a view whose driver closed the pty master spun
/// for days over every measurement window on this host.
///
/// One zero-timeout `poll(2)` on one descriptor and no read at all:
/// `POLLHUP`, `POLLERR` and `POLLNVAL` are set by the kernel whether or not
/// they were asked for, so an empty event mask reports a hangup and stays
/// silent for an ordinary readable terminal. `POLLNVAL` counts as a hangup
/// too: it is the answer a macOS `/dev/tty` fallback descriptor gives (see
/// [`adopt_terminal_stdin`]), and a descriptor no readiness mechanism can
/// watch has exactly the same consequence as one whose far end is gone.
///
/// Unix only, as the whole descriptor loop around it is. The Windows
/// session reads through crossterm's console backend, which has no
/// descriptor to poll and no `EIO` read to spin on: a closed console ends
/// the process itself.
#[cfg(unix)]
fn terminal_hungup(fd: BorrowedFd<'_>) -> bool {
    use rustix::event::{PollFd, PollFlags};

    let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::empty())];
    let ready = matches!(
        rustix::event::poll(&mut fds, Some(&rustix::event::Timespec::default())),
        Ok(n) if n > 0
    );
    ready
        && fds[0]
            .revents()
            .intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL)
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

/// Creates a signal self-pipe's two ends with `O_CLOEXEC`/`O_NONBLOCK`
/// already set on both, matching `pipe2`'s atomic guarantee.
#[cfg(all(unix, not(target_vendor = "apple")))]
fn new_signal_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
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
fn new_signal_pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
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

/// nvim's own default `ttimeoutlen`, held until the engine relays what the
/// user's configuration actually says
/// ([`set_escape_timeout`](InputSource::set_escape_timeout)), so a session
/// that never hears differently waits exactly as long as nvim does before
/// reading a half-arrived key code as the chord its bytes spell.
#[cfg(unix)]
const DEFAULT_ESCAPE_TIMEOUT: Duration = Duration::from_millis(50);

/// The pollable input handle: the terminal read fd plus a SIGWINCH
/// self-pipe, both exposed as borrowed fds for the runtime loop's
/// readiness poll, with all reading and decoding kept behind
/// [`drain`](Self::drain).
///
/// The self-pipe exists because a resize is a signal, not a byte on the
/// tty: a loop sleeping in a raw fd poll would never learn of one until
/// the next keystroke. A crate-owned pipe registered on SIGWINCH gives the
/// loop's poll set a readable fd for exactly that moment; the drain that
/// follows asks the terminal its new size and delivers the message.
///
/// A second self-pipe carries the signals that ask this process to stop.
/// They are folded into the loop rather than left to their default
/// disposition because the default one ends the process where it stands:
/// raw mode on, the alternate screen up, the kitty keyboard protocol still
/// pushed, no destructor and no panic hook run --
/// a terminal the user has to repair from another shell, which over SSH
/// (the link drops, sshd HUPs the session) is the ordinary way to die. A
/// self-pipe is what keeps the handler itself async-signal-safe: it stores
/// the signal number and writes one byte, and every restore runs on the
/// loop thread afterwards.
#[cfg(unix)]
pub struct InputSource {
    tty: TtyFd,
    winch_read: OwnedFd,
    fatal_read: OwnedFd,
    /// The write end the signal handlers were registered on, kept so a
    /// hangup can wake the readiness poll through the same pipe a signal
    /// wakes it through.
    fatal_write: OwnedFd,
    fatal_signal: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    dead: bool,
    /// Armed only when the startup capability probe handed the terminal
    /// over without its DA1 fence, i.e. only when the terminal may still
    /// owe a reply. See [`InputSource::open_after_probe`].
    guard: Option<LateReplyGuard>,
    /// What the guard decoded out of a swept read -- keystrokes, and the
    /// capability upgrade a recognized answer resolves to -- waiting for
    /// the next [`drain`](InputSource::drain) to hand them over.
    guard_msgs: std::collections::VecDeque<Msg>,
    /// A sequence whose final byte has not arrived, with the instant its
    /// first byte did.
    pending: Option<(Vec<u8>, std::time::Instant)>,
    /// How long a half-arrived key code may wait for the rest of itself
    /// before it is read as the chord its bytes spell. Zero is what both
    /// of nvim's own sentinels resolve to: read it on the pass that read
    /// the bytes.
    escape_timeout: Duration,
}

/// The state behind [`InputSource::open_after_probe`]: how long the terminal is
/// still allowed to answer, the bytes of a reply it has so far only half
/// delivered, and the capabilities its answers have resolved to.
#[cfg(unix)]
struct LateReplyGuard {
    until: std::time::Instant,
    buf: Vec<u8>,
    /// Whether the DA1 fence is still owed. False means the probe already
    /// had it, so the only thing keeping this guard armed is `buf`: once
    /// that resolves there is nothing left to recognize and the terminal
    /// goes back to crossterm rather than idling out the cap.
    fence_owed: bool,
    /// Everything the probe's own window settled on, plus every later
    /// answer folded into it. Held so a sweep can tell a reply that
    /// changes the session's capabilities from one that only restates
    /// them.
    caps: TermCaps,
    /// Whether a cursor-position report still answers the box-glyph
    /// question rather than being a keypress that looks like one.
    ///
    /// Inherited from the probe and cleared for good by the first CPR any
    /// sweep consumes, because `scan_replies` starts over on a buffer this
    /// guard drains between sweeps and would otherwise re-ask a question
    /// already answered on every read for the rest of the cap. `\x1b[1;2R`
    /// is tmux's `Shift-F3`; once the answer is in, those six bytes are the
    /// user's.
    cpr_wanted: bool,
}

/// The signals that must end the session through view's own teardown
/// rather than where they land.
///
/// `SIGINT` is in the set for signals sent from outside (`kill -INT`, a
/// process-group interrupt): raw mode clears `ISIG`, so a user's own
/// `<C-c>` reaches view as a key and never reaches this path.
#[cfg(unix)]
const FATAL_SIGNALS: [std::ffi::c_int; 3] = [
    signal_hook::consts::SIGHUP,
    signal_hook::consts::SIGTERM,
    signal_hook::consts::SIGINT,
];

#[cfg(unix)]
impl InputSource {
    /// Opens the handle: resolves the terminal fd and registers the
    /// SIGWINCH and fatal-signal self-pipes.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the terminal fd or the
    /// signal pipe cannot be set up.
    pub fn open() -> std::io::Result<Self> {
        Self::open_with(None)
    }

    /// [`open`](Self::open) for a startup handing the terminal over from its
    /// capability probe, with the late-reply guard armed unless the probe
    /// ended with nothing outstanding.
    ///
    /// Four things in [`ProbeOutcome`](crate::tiers::ProbeOutcome) concern
    /// the input path, and the whole outcome is taken rather than picked
    /// apart so that a fifth reaches here by existing: `caps` is what the
    /// probe resolved, the floor every later answer is folded onto;
    /// `fence_seen` is whether the DA1 fence arrived; `partial_reply` is
    /// the run the probe's scan stopped on, still a live prefix of some
    /// answer grammar, carried across the handover so the read that brings
    /// its tail completes an answer rather than scanning a headless one;
    /// and `cpr_seen` is the box-glyph question's own bound, without which
    /// the guard re-asks on every sweep a question the probe already had
    /// answered.
    ///
    /// Either a missing fence or a non-empty tail arms it, and the caller is
    /// not asked which. Neither condition proves the terminal owes anything:
    /// the fence is answered last, so a missing one means every earlier
    /// reply *may* still be owed, and a tail is only bytes that *could*
    /// still become an answer -- a user's bare `ESC [` landing in the same
    /// read as the fence arms the guard on a terminal that owes nothing at
    /// all. Arming on the possibility is the cheap side of both: a guard
    /// that idles costs typed bytes one decoder, and a guard that was not
    /// armed costs a capability for the session or a keypress for the user.
    /// Which of the two the bytes actually were is decided when the run
    /// finishes or the cap expires, not here.
    ///
    /// A terminal answers queries in the order it received them and the
    /// fence is asked last, so a fence that arrived proves nothing is still
    /// in flight and plain [`open`](Self::open) is right. A fence that never
    /// arrived proves the opposite, and a reply landing after the probe has
    /// handed the terminal over is not harmless: an unrecognized
    /// private-mode answer (`ESC [ ? 2026 ; 1 $ y` is one) reaches the key
    /// path as a sequence to consume, so what it answered about the
    /// terminal is lost -- and the capability it carries is one the session
    /// then paints a whole run of frames without.
    ///
    /// While armed, ready bytes are read here first and matched against the
    /// four grammars the query batch can be answered with
    /// ([`scan_replies`](crate::tiers::scan_replies)). A run that completes
    /// one is the terminal's: its bytes are kept off the key path, and what
    /// it answered is folded into `settled` and handed to the loop as
    /// [`Msg::CapsUpgraded`](view_core::msg::Msg::CapsUpgraded) whenever
    /// that changes the session's capabilities. This is what lets the probe
    /// stop waiting: the answer it would have blocked the first frame for
    /// arrives here instead, and upgrades a session that is already
    /// editable. A run that is still a live
    /// prefix of one waits for the read that finishes it, and so does a
    /// keypress whose own sequence is half arrived, whether or not the
    /// terminal put bytes of its own behind it in the same read. The first
    /// byte
    /// that leaves every grammar ends the answer there: everything from
    /// that byte on is the user's and is decoded
    /// ([`decode_residue`](crate::keys::decode_residue)) into the keys it
    /// is, and where the run in front of it was provably the terminal's --
    /// `ESC [ ?` is a shape no keyboard emits -- that stalled answer is
    /// dropped without taking the keypress with it.
    ///
    /// Only the grammars decide, which is what keeps a keystroke out of the
    /// guard's mouth: `?2026` plus the `c`, `u` or `y` a user pressed while
    /// the terminal stalled is not a DA1 fence, a kitty claim or a DECRPM
    /// answer, and `ESC P` is Alt+Shift+P rather than the opening of a DCS.
    ///
    /// Recognizing an answer costs a typed byte nothing, so it lasts as long
    /// as anything is still outstanding: until the fence arrives if one was
    /// owed, until the seeded tail resolves if that was the whole reason it
    /// armed, and in either case no longer than
    /// [`PROBE_HARD_CAP`](crate::tiers::PROBE_HARD_CAP) from the handover.
    /// Replies land in whatever read the link happens to deliver them in,
    /// and one that arrives whole, in its own read, after a keystroke has
    /// already come through is the ordinary case on a slow link, not an
    /// exotic one.
    ///
    /// Two shapes are kept back rather than decoded on arrival, both of them
    /// buffer tails with nothing typed behind them to delay: a live answer
    /// prefix, and a keypress whose own sequence is still arriving (`ESC [`,
    /// `ESC O`, a CSI whose final byte has not landed). The read that
    /// finishes either one decodes it -- a split arrow arrives as the arrow
    /// -- and a read that finishes neither drops the introducer alone, so
    /// `ESC [` followed by `hello` costs two bytes and types five keys.
    ///
    /// Nothing of the user's is dropped unseen. When the fence lands or the
    /// cap expires, whatever is still buffered goes to that same decoder --
    /// unless it is provably the terminal's own half-arrived answer, which
    /// is dropped, because replaying that would type the answer into the
    /// buffer.
    ///
    /// One shape stays out of reach: a reply split immediately after its
    /// `ESC`. A lone trailing `ESC` is residue by policy -- it is the
    /// Escape key, the one key that cannot afford to wait a read -- so a
    /// `[ ? 2 0 2 6 ; 1 $ y` tail arrives with no introducer in front of it
    /// and reads as literal keys.
    ///
    /// A separate constructor rather than a call after `open`, so the
    /// seeded tail is in hand before this handle's first read rather than
    /// one read later.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the terminal fd or the
    /// signal pipe cannot be set up.
    pub fn open_after_probe(outcome: &crate::tiers::ProbeOutcome) -> std::io::Result<Self> {
        if outcome.fence_seen && outcome.partial_reply.is_empty() {
            return Self::open_with(None);
        }
        Self::open_with(Some(LateReplyGuard {
            until: std::time::Instant::now() + crate::tiers::PROBE_HARD_CAP,
            buf: outcome.partial_reply.clone(),
            caps: outcome.caps,
            fence_owed: !outcome.fence_seen,
            cpr_wanted: !outcome.cpr_seen,
        }))
    }

    fn open_with(guard: Option<LateReplyGuard>) -> std::io::Result<Self> {
        let tty = TtyFd::open()?;
        let (winch_read, winch_write) = new_signal_pipe()?;
        signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, winch_write)?;
        let (fatal_read, fatal_write) = new_signal_pipe()?;
        let fatal_signal = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        // one signal's hooks run in registration order, so the three below
        // are ordered by what each of them makes true for the next:
        //
        // 1. the number is recorded first, so a byte on the pipe always
        //    has a signal to read behind it;
        // 2. the shutdown is armed on a flag still false, so it fires on
        //    the *second* delivery only -- a loop too wedged to reach its
        //    own quit stays killable with a repeat `kill`, not just
        //    `SIGKILL`;
        // 3. the flag is raised, arming that second delivery;
        // 4. the byte wakes the readiness poll last of all.
        //
        // that repeat-signal escape hatch is a deliberately blunt one:
        // `_exit` runs from the handler, so a second signal ends the process
        // with raw mode on, the alternate screen up and the kitty keyboard
        // protocol still pushed -- the very state this whole path exists to
        // avoid. It is still the better outcome,
        // because the alternative is an editor that cannot be ended without
        // `SIGKILL`, and a `reset` repairs a terminal where a lost session
        // cannot be recovered. The flag is shared across all three signals
        // on purpose: any second fatal signal, of any kind, is the user
        // saying the first one did not work.
        let repeat = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        for signal in FATAL_SIGNALS {
            // a signal that cannot be recorded is left unregistered rather
            // than recorded as 0, which is `take_fatal_signal`'s "nothing
            // delivered" answer and would spin the readiness poll on a woken
            // fd with no message behind it
            let Ok(recorded) = usize::try_from(signal) else {
                continue;
            };
            signal_hook::flag::register_usize(
                signal,
                std::sync::Arc::clone(&fatal_signal),
                recorded,
            )?;
            signal_hook::flag::register_conditional_shutdown(
                signal,
                128 + signal,
                std::sync::Arc::clone(&repeat),
            )?;
            signal_hook::flag::register(signal, std::sync::Arc::clone(&repeat))?;
            signal_hook::low_level::pipe::register(signal, fatal_write.try_clone()?)?;
        }
        let mut source = Self {
            tty,
            winch_read,
            fatal_read,
            fatal_write,
            fatal_signal,
            dead: false,
            guard: None,
            guard_msgs: std::collections::VecDeque::new(),
            pending: None,
            escape_timeout: DEFAULT_ESCAPE_TIMEOUT,
        };
        if guard.is_some() {
            source.guard = guard;
            source.sweep_late_replies();
        }
        Ok(source)
    }

    /// Whether this handle is still holding the terminal's bytes back for
    /// an answer the startup probe did not get.
    ///
    /// False is the steady state and the state of every session whose
    /// terminal answered its fence in time. It is also the only way to
    /// observe the guard from outside: every other difference it makes is
    /// a reply that never reached the key path.
    #[must_use]
    pub fn still_listening(&self) -> bool {
        self.guard.is_some()
    }

    /// Reads whatever the terminal still owes the capability probe,
    /// keeping its bytes off the key path and its answer as a capability
    /// upgrade. A no-op unless armed, which is the
    /// whole cost on every session whose terminal answered the fence.
    fn sweep_late_replies(&mut self) {
        let Some(mut guard) = self.guard.take() else {
            return;
        };
        if std::time::Instant::now() >= guard.until {
            // out of time is not proof of anything about bytes that could
            // still be a keypress, so those go to the decoder rather than
            // dying with the guard. What is provably the terminal's own
            // half-arrived answer does die here: replaying it as keys would
            // type the answer into the buffer
            if !crate::tiers::is_terminal_only_remainder(&guard.buf) {
                self.queue_residue(&guard.buf);
            }
            return;
        }
        while let Some(chunk) = read_ready(self.tty.as_fd()) {
            guard.buf.extend_from_slice(&chunk);
        }
        let mut replies = crate::tiers::scan_replies(&guard.buf, guard.cpr_wanted);
        // a question answered stays answered, whichever sweep answered it:
        // the buffer is drained below, so nothing else remembers
        guard.cpr_wanted &= replies.unicode_boxes.is_none();
        // the answer's payload, not only its bytes: this is the whole
        // reason the probe may hand the terminal over before the fence --
        // what it would have waited for is recognized here instead, and
        // reaches the loop as the one message that upgrades the tier
        let upgraded = replies.upgraded(guard.caps);
        if upgraded != guard.caps {
            guard.caps = upgraded;
            self.guard_msgs.push_back(Msg::CapsUpgraded(upgraded));
        }
        let typed = crate::keys::decode_residue(&replies.residue);
        // a keypress whose own sequence is still arriving waits for the
        // read that finishes it, exactly as a half-arrived answer does:
        // decoded now it would be dropped down to its `ESC [`, and its tail
        // would reach the next read alone and type an arrow's `A` into the
        // buffer as a literal key. It is put back in front of whatever the
        // scan held on to rather than rewound over, because an answer
        // between the two is consumed here and must not be scanned twice
        let held = replies
            .residue
            .split_off(replies.residue.len() - typed.unfinished);
        guard.buf.drain(..replies.consumed);
        guard.buf.splice(..0, held);
        self.guard_msgs.extend(typed.msgs);
        // a fence ends what the guard is *listening* for: it is answered
        // last, so nothing the probe asked for can still be behind it,
        // whatever else has happened on the fd since
        if replies.da1 {
            // a keypress still arriving behind the fence is the same shape
            // the settle cut leaves, one read later: handed over now it
            // would be decoded down to its `ESC [` and its own last byte
            // would reach the next read alone, typing an arrow's `A` into
            // the buffer as a literal key. So the guard stays for the tail
            // alone, owed no fence, and the rule below hands the fd back
            // the moment that tail resolves -- to an answer or to the key
            if !guard.buf.is_empty() && !crate::tiers::is_terminal_only_remainder(&guard.buf) {
                guard.fence_owed = false;
                self.guard = Some(guard);
            }
            return;
        }
        // a guard that armed on a tail alone, with its fence already in the
        // probe's hands, is owed nothing once that tail resolves -- and an
        // empty buffer is exactly that, since anything half-arrived is put
        // back above. Without this it would idle out the whole cap with the
        // key path routed through a decoder for no reply that can arrive.
        if !guard.fence_owed && guard.buf.is_empty() {
            return;
        }
        self.guard = Some(guard);
    }

    /// Queues `bytes` as the messages they decode to, for a caller that
    /// read them off the terminal during the probe's own window. An escape
    /// run still arriving when this runs has run out of reads to arrive
    /// in, so it is forced here rather than dropped: the guard has already
    /// drained the descriptor, and the keystroke behind a run it dropped
    /// would be one the user typed at startup and never saw.
    fn queue_residue(&mut self, bytes: &[u8]) {
        self.guard_msgs
            .extend(crate::keys::decode_residue_forced(bytes));
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

    /// The fatal-signal self-pipe's read end, for readiness polling only.
    ///
    /// Stays in the poll set even after the terminal is marked dead: the
    /// terminal going away is exactly when a `SIGHUP` is most likely, and a
    /// session that dropped this fd then would be one no signal could end
    /// through its own teardown.
    #[must_use]
    pub fn fatal_fd(&self) -> BorrowedFd<'_> {
        self.fatal_read.as_fd()
    }

    /// The signal that asked this process to stop, if one has been
    /// delivered since the last call; clears the record and drains the
    /// self-pipe, so a second call reports only a genuinely new signal.
    ///
    /// The number is read before the pipe is drained, and that is also why
    /// the drain is skipped when there is no number: the handlers store it
    /// before they write the byte ([`open`](Self::open)), so a pipe holding
    /// a byte always has a number behind it and the steady state -- every
    /// wakeup of every session that is never signalled -- costs one atomic
    /// swap and no syscall at all.
    pub fn take_fatal_signal(&mut self) -> Option<std::ffi::c_int> {
        use std::sync::atomic::Ordering;

        let signal = self.fatal_signal.swap(0, Ordering::AcqRel);
        if signal == 0 {
            return None;
        }
        let mut scratch = [0_u8; 64];
        while matches!(rustix::io::read(&self.fatal_read, &mut scratch), Ok(n) if n > 0) {}
        std::ffi::c_int::try_from(signal).ok()
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

    /// Records a hung-up terminal and asks the session to end the way a
    /// `SIGHUP` ends it.
    ///
    /// A closed pty master delivers no signal at all unless the process on
    /// the far side was the session leader, so a driver that never called
    /// `setsid` takes its editor's terminal away and leaves nothing behind
    /// but a descriptor that never sleeps again. Routing it into the
    /// fatal-signal record, byte and all, is what gives that the one
    /// teardown every other exit takes: raw mode restored, the alternate
    /// screen taken down, the children stopped, and the `129` a shell reads
    /// for a hangup as the exit status.
    fn note_hangup(&mut self) {
        use std::sync::atomic::Ordering;

        self.dead = true;
        let Ok(recorded) = usize::try_from(signal_hook::consts::SIGHUP) else {
            return;
        };
        // a real signal already recorded outranks this one: it has its own
        // byte on the pipe and its own exit status, and the terminal it is
        // ending has gone away either way
        if self
            .fatal_signal
            .compare_exchange(0, recorded, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let _ = rustix::io::write(&self.fatal_write, &[1_u8]);
        }
    }

    /// Whether a message is already decodable right now, without waiting
    /// for the terminal fd to become readable again.
    ///
    /// The one thing that can be: what the late-reply guard's own sweep
    /// pulled off the fd and decoded ahead of this call. Nothing else is
    /// held in userspace -- every byte this handle reads is decoded in the
    /// same call that reads it, so the descriptor's own readiness describes
    /// the whole of the terminal's state and the loop can sleep on it.
    ///
    /// A sequence whose final byte has not arrived is the exception that
    /// proves it: those bytes wait here, but what they are waiting for is a
    /// read or a deadline ([`next_deadline`](Self::next_deadline)), not a
    /// caller to come and collect them.
    pub fn has_buffered(&mut self) -> bool {
        if self.dead {
            return false;
        }
        if terminal_hungup(self.tty.as_fd()) {
            self.note_hangup();
            return false;
        }
        self.sweep_late_replies();
        !self.guard_msgs.is_empty()
    }

    /// Sets how long a half-arrived key code may wait for the rest of
    /// itself: nvim's own effective `ttimeoutlen`, relayed from the engine.
    ///
    /// The value is the user's rather than view's because the decision it
    /// makes is one nvim would otherwise be making: with view reading the
    /// terminal's bytes itself, a `ttimeoutlen` the user tuned for a slow
    /// link would have stopped applying to the very sequences it was tuned
    /// for. `ttimeout` off and a negative `ttimeoutlen` both arrive here as
    /// zero, which is the engine's own reading of them.
    pub fn set_escape_timeout(&mut self, within: Duration) {
        self.escape_timeout = within;
    }

    /// When a half-arrived key code must be read as the chord its bytes
    /// spell, so the runtime loop can bound its sleep on it; `None` while
    /// nothing is waiting, or while what is waiting is a paste no keystroke
    /// timeout bounds.
    ///
    /// A deadline in the past is a flush the next
    /// [`drain`](Self::drain) performs, so a caller that turns this into a
    /// sleep saturates rather than waiting a whole clock's wrap.
    #[must_use]
    pub fn next_deadline(&self) -> Option<std::time::Instant> {
        escape_deadline(self.pending.as_ref(), self.escape_timeout)
    }

    /// Drains everything ready without blocking: empties the SIGWINCH
    /// self-pipe, then reads whatever the terminal has and decodes it into
    /// core [`Msg`]s handed to `sink`, publishing any resize to `size`
    /// before its message is delivered -- the same publish-before-queue
    /// ordering the input thread kept, so no frame paints at a shape the
    /// terminal has left.
    ///
    /// Events with no nvim equivalent (key releases, keys with no
    /// notation) are dropped here, exactly as the input thread dropped
    /// them.
    ///
    /// A terminal that has *hung up* is the one case that ends more than
    /// input: there is nothing left to paint to and nothing left to read, so
    /// it is reported through [`note_hangup`](Self::note_hangup) and the
    /// session leaves by the fatal-signal path.
    pub fn drain(&mut self, size: &TermSizeCell, mut sink: impl FnMut(Msg)) -> DrainOutcome {
        if !self.dead && terminal_hungup(self.tty.as_fd()) {
            self.note_hangup();
            return DrainOutcome::SourceLost;
        }
        let mut scratch = [0_u8; 64];
        let mut resized = false;
        while matches!(rustix::io::read(&self.winch_read, &mut scratch), Ok(n) if n > 0) {
            resized = true;
        }
        self.sweep_late_replies();
        for msg in std::mem::take(&mut self.guard_msgs) {
            emit(msg, &mut sink);
        }
        // a resize is the one thing the guard used to hand the fd back
        // for. It arrives as a signal, and its new shape is a TIOCGWINSZ
        // away on the fd this already holds, so the frame is corrected here
        // without the read that would cost the guard a reply. The ioctl is
        // issued directly rather than through `crossterm::terminal::size`,
        // which re-opens `/dev/tty` per call and, when that and stdout both
        // refuse the ioctl, shells out to `tput` -- a subprocess spawn
        // inside the first-paint window. A shape this cannot read, or one
        // the kernel reports as zero because it does not know it yet, is
        // corrected by the next resize rather than lost for good
        if resized {
            if let Ok(shape) = rustix::termios::tcgetwinsize(self.tty_fd()) {
                let (width, height) = (shape.ws_col, shape.ws_row);
                if width > 0 && height > 0 {
                    size.publish(width, height);
                    sink(Msg::Resized { width, height });
                }
            }
        }
        // while the guard is armed the fd is the sweep's alone: a read here
        // would take the byte that finishes an answer and decode it as the
        // keys it is not
        if self.guard.is_none() {
            self.read_and_decode(&mut sink);
        }
        DrainOutcome::Drained
    }

    /// Reads every byte the terminal has ready and hands `sink` what they
    /// decode to, keeping a sequence whose final byte has not arrived for
    /// the read that brings it.
    ///
    /// The kept bytes are prepended to the next read rather than decoded
    /// twice, so an arrow split across two reads arrives as the arrow. What
    /// bounds that wait is nvim's own `ttimeoutlen`
    /// ([`set_escape_timeout`](Self::set_escape_timeout)): once it has
    /// passed, a run still waiting for its final byte is read as the Alt
    /// chord its bytes spell -- `<M-[>` for a bare `ESC [`, `<Esc>` for an
    /// Escape with nothing behind it -- which is termkey's own reading of
    /// the same bytes, and the reading under which either ever reaches the
    /// buffer at all.
    fn read_and_decode(&mut self, sink: &mut impl FnMut(Msg)) {
        let (mut buf, since) = match self.pending.take() {
            Some((bytes, since)) => (bytes, Some(since)),
            None => (Vec::new(), None),
        };
        let mut arrived = false;
        while let Some(chunk) = read_ready(self.tty.as_fd()) {
            #[cfg(all(unix, feature = "bench-taps"))]
            crate::tap::tap(crate::tap::TAG_BYTES_READ);
            arrived = true;
            buf.extend_from_slice(&chunk);
        }
        if buf.is_empty() {
            return;
        }
        let decoded = crate::keys::decode_residue(&buf);
        let tail = buf.split_off(buf.len() - decoded.unfinished);
        for msg in decoded.msgs {
            emit(msg, sink);
        }
        if tail.is_empty() {
            return;
        }
        // dated from the read that delivered a byte, not from the one the
        // run opened with: the engine re-arms its escape timer on every
        // read that leaves a sequence unfinished (`tui/input.c`), so a
        // sequence trickling in a byte at a time gets the whole wait again
        // for each of them. A pass that read nothing is the timer itself
        // coming due, and keeps the instant it is being measured against
        let opened = match since {
            Some(since) if !arrived => since,
            _ => std::time::Instant::now(),
        };
        if opened + self.escape_timeout <= std::time::Instant::now()
            && crate::keys::forceable(&tail)
        {
            for msg in crate::keys::decode_residue_forced(&tail) {
                emit(msg, sink);
            }
            return;
        }
        self.pending = Some((tail, opened));
    }
}

/// Hands one decoded message to the runtime loop, marking a key as the
/// first instant view owns that keystroke.
///
/// The tap fires per delivered key rather than per read: the gated
/// input-path interval closes on the RPC the key turns into, so a tap for a
/// byte run that decodes to no key at all would pair with the next
/// keystroke's RPC and report an interval spanning two keys.
#[cfg(unix)]
fn emit(msg: Msg, sink: &mut impl FnMut(Msg)) {
    #[cfg(all(unix, feature = "bench-taps"))]
    if matches!(msg, Msg::Key(_)) {
        crate::tap::tap(crate::tap::TAG_KEY_READ);
    }
    sink(msg);
}

/// When the bytes held from an unfinished read must be given up on, given
/// the wait in force: [`InputSource::next_deadline`]'s whole decision, as a
/// function of the two values it reads, because the source itself can only
/// be built on a real terminal.
#[cfg(unix)]
fn escape_deadline(
    pending: Option<&(Vec<u8>, std::time::Instant)>,
    within: Duration,
) -> Option<std::time::Instant> {
    let (bytes, since) = pending?;
    crate::keys::forceable(bytes).then(|| *since + within)
}

/// Translates one crossterm event into the core [`Msg`] the runtime loop
/// dispatches, or `None` for events with no nvim equivalent.
///
/// The non-unix input thread's own translation. On unix the terminal's
/// bytes are decoded here rather than by crossterm's parser
/// ([`crate::keys::decode_residue`]), because that parser folds a doubled
/// `ESC` into one Escape key and holds a bare `ESC [` for a final byte no
/// timeout of its own ever gives up on -- neither of which is what nvim
/// does with the same bytes, and view's whole contract is that a key means
/// there what it means in nvim.
#[cfg(not(unix))]
pub(crate) fn event_to_msg(event: Event, size: &TermSizeCell) -> Option<Msg> {
    match event {
        Event::Key(k) => encode_terminal_key(&k, crate::terminal::kitty_keyboard_pushed())
            .map(|notation| Msg::Key(Key { notation })),
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

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// One decoder, in the guard's window and out of it: a second reader
    /// of the same fd is what folded a doubled `ESC` into one Escape key
    /// and held a bare `ESC [` forever, and either is back the moment a
    /// call site here reaches for crossterm's parser again.
    #[test]
    fn nothing_on_the_unix_path_reads_the_terminal_through_crossterm() {
        let source = include_str!("input.rs");
        let reads = [
            concat!("crossterm::event::", "poll("),
            concat!("crossterm::event::", "read("),
        ];
        for (at, line) in source.lines().enumerate() {
            // prose may name the parser this stopped using; code may not
            if line.trim_start().starts_with("//") {
                continue;
            }
            for read in reads {
                assert!(
                    !line.contains(read),
                    "line {} reads the terminal through crossterm: its parser \
                     answers `ESC ESC` with one `<Esc>` and waits on `ESC [` \
                     with no timeout of its own, so a byte it sees is a byte \
                     nvim's own timing no longer decides",
                    at + 1
                );
            }
        }
        // the translation the non-unix thread still needs is fenced off
        // this platform rather than left compiled and unreachable
        assert!(
            source.contains(concat!("#[cfg(not(unix))]\n", "pub(crate) fn event_to_msg")),
            "`event_to_msg` is reachable on unix again, which is a second \
             reading of the same bytes"
        );
    }

    /// The default is nvim's own, so a session whose engine never relays
    /// anything still waits exactly as long as nvim would before reading a
    /// half-arrived key code as the Escape key.
    #[test]
    fn the_escape_timeout_starts_at_nvims_own_default() {
        assert_eq!(DEFAULT_ESCAPE_TIMEOUT, Duration::from_millis(50));
    }

    /// The three answers the relayed wait produces, each of which is a
    /// wedged session if it drifts: a run that stops short is given up on
    /// at the user's own `ttimeoutlen`, a zero wait is already out the
    /// moment it is armed, and a paste is bounded by its own closing
    /// sequence rather than by a keystroke timeout that would type the
    /// rest of the payload as commands.
    #[test]
    fn the_relayed_wait_decides_when_a_short_run_is_given_up_on() {
        let now = std::time::Instant::now();
        let unfinished = (b"\x1b[".to_vec(), now);
        assert_eq!(
            escape_deadline(Some(&unfinished), Duration::from_millis(120)),
            Some(now + Duration::from_millis(120))
        );
        assert_eq!(
            escape_deadline(Some(&unfinished), Duration::ZERO),
            Some(now),
            "both of nvim's sentinels arrive as zero, which is due at once"
        );
        assert_eq!(
            escape_deadline(None, DEFAULT_ESCAPE_TIMEOUT),
            None,
            "nothing is waiting, so nothing bounds the loop's sleep"
        );
        let paste = (b"\x1b[200~half a file".to_vec(), now);
        assert_eq!(
            escape_deadline(Some(&paste), DEFAULT_ESCAPE_TIMEOUT),
            None,
            "a paste slower than the wait must never be typed as commands"
        );
    }
}
