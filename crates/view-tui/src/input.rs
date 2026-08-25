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
use view_core::model::TermCaps;
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

/// How many zero-timeout crossterm polls one
/// [`InputSource::has_buffered`] answer may spend before it gives up.
///
/// crossterm's poll returns as soon as it has parsed one event, leaving the
/// rest of the same read in its own queue, and reports nothing at all for an
/// event its public filter rejects. Three such events exist -- a cursor
/// position report, the keyboard-enhancement flags, and the primary device
/// attributes, every one of them a terminal's answer to a query -- so at
/// most three can sit ahead of a keystroke and a fourth poll always reaches
/// it. The bound is what keeps this off the shape where an endlessly
/// answering source could hold the loop here.
#[cfg(unix)]
const BUFFERED_POLL_LIMIT: usize = 4;

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
    fatal_signal: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    dead: bool,
    /// Armed only when the startup capability probe handed the terminal
    /// over without its DA1 fence, i.e. only when the terminal may still
    /// owe a reply. See [`InputSource::open_listening`].
    guard: Option<LateReplyGuard>,
    /// What the guard decoded out of a swept read -- keystrokes, and the
    /// capability upgrade a recognized answer resolves to -- waiting for
    /// the next [`drain`](InputSource::drain) to hand them over.
    guard_msgs: std::collections::VecDeque<Msg>,
}

/// The state behind [`InputSource::open_listening`]: how long the terminal is
/// still allowed to answer, the bytes of a reply it has so far only half
/// delivered, and the capabilities its answers have resolved to.
#[cfg(unix)]
struct LateReplyGuard {
    until: std::time::Instant,
    buf: Vec<u8>,
    /// Everything the probe's own window settled on, plus every later
    /// answer folded into it. Held so a sweep can tell a reply that
    /// changes the session's capabilities from one that only restates
    /// them.
    caps: TermCaps,
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
    /// Opens the handle: resolves the terminal fd, registers the SIGWINCH
    /// and fatal-signal self-pipes, and touches crossterm's event source
    /// once so its own SIGWINCH registration exists from here on (it is
    /// created lazily on first use; a resize delivered before that would
    /// otherwise be lost rather than translated on the next drain).
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the terminal fd or the
    /// signal pipe cannot be set up.
    pub fn open() -> std::io::Result<Self> {
        Self::open_with(None)
    }

    /// [`open`](Self::open) with the late-reply guard armed, for a startup
    /// whose capability probe handed the terminal over without ever seeing
    /// its DA1 fence (`ProbeOutcome::fence_seen` false). `settled` is what
    /// that probe resolved, which is the floor every later answer is folded
    /// onto, and `partial_reply` is
    /// [`ProbeOutcome::partial_reply`](crate::tiers::ProbeOutcome::partial_reply)
    /// -- the head of an answer that was still arriving at the settle,
    /// carried across the handover so the read that brings its tail
    /// completes an answer rather than scanning a headless one.
    ///
    /// A terminal answers queries in the order it received them and the
    /// fence is asked last, so a fence that arrived proves nothing is still
    /// in flight and plain [`open`](Self::open) is right. A fence that never
    /// arrived proves the opposite, and a reply landing after the probe has
    /// handed the terminal over is not harmless: crossterm's parser holds an
    /// unrecognized private-mode sequence (`ESC [ ? 2026 ; 1 $ y` is one --
    /// it resolves neither to an event nor to an error) in its buffer and
    /// appends every later byte to it, so the reply does not merely arrive
    /// as garbage, it swallows every keystroke behind it until one happens
    /// to complete a sequence it recognizes. A DCS answer is worse still: it
    /// decodes into a run of literal keys typed into the buffer.
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
    /// Recognizing an answer costs a typed byte nothing, so it lasts until
    /// the fence arrives or until
    /// [`PROBE_HARD_CAP`](crate::tiers::PROBE_HARD_CAP) has passed again,
    /// whichever comes first: replies land in whatever read the link happens
    /// to deliver them in, and one that arrives whole, in its own read,
    /// after a keystroke has already come through is the ordinary case on a
    /// slow link, not an exotic one.
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
    /// A separate constructor rather than a call after `open`, because
    /// `open` itself lets crossterm's reader touch the terminal: a reply
    /// landing in that gap would reach the parser this exists to keep it
    /// away from.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the terminal fd or the
    /// signal pipe cannot be set up.
    pub fn open_listening(settled: TermCaps, partial_reply: Vec<u8>) -> std::io::Result<Self> {
        Self::open_with(Some((settled, partial_reply)))
    }

    fn open_with(guard_late_replies: Option<(TermCaps, Vec<u8>)>) -> std::io::Result<Self> {
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
            fatal_signal,
            dead: false,
            guard: None,
            guard_msgs: std::collections::VecDeque::new(),
        };
        if let Some((caps, partial_reply)) = guard_late_replies {
            source.guard = Some(LateReplyGuard {
                until: std::time::Instant::now() + crate::tiers::PROBE_HARD_CAP,
                buf: partial_reply,
                caps,
            });
            // ahead of the crossterm touch below, which reads: the guard
            // has to own the terminal from this handle's first byte
            source.sweep_late_replies();
        }
        let _ = crossterm::event::poll(Duration::ZERO);
        Ok(source)
    }

    /// Reads whatever the terminal still owes the capability probe before
    /// crossterm can see it, keeping its bytes off the key path and its
    /// answer as a capability upgrade. A no-op unless armed, which is the
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
        let mut replies = crate::tiers::scan_replies(&guard.buf);
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
        // only the fence ends the filtering: everything else the terminal
        // owes is still owed, whatever else has happened on the fd since
        if replies.da1 {
            if !crate::tiers::is_terminal_only_remainder(&guard.buf) {
                self.queue_residue(&guard.buf);
            }
        } else {
            self.guard = Some(guard);
        }
    }

    /// Queues `bytes` as the messages they decode to, for a caller that has
    /// read them off the terminal ahead of crossterm. A sequence still
    /// arriving when this runs has run out of reads to arrive in, so it is
    /// dropped rather than typed as the fragment it is.
    fn queue_residue(&mut self, bytes: &[u8]) {
        self.guard_msgs
            .extend(crate::keys::decode_residue(bytes).msgs);
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
    ///
    /// One such poll answers for at most one newly parsed event, so a reply
    /// crossterm parses but never hands out -- a terminal answering a
    /// capability query after the prober that asked has stopped listening --
    /// spends that answer while the keys parsed behind it, out of the very
    /// same read, stay invisible. Repeating the poll walks past those
    /// replies one at a time, up to `BUFFERED_POLL_LIMIT`, which is what
    /// makes the answer describe the whole buffer rather than its first
    /// entry.
    ///
    /// Runs the late-reply guard's sweep first when one is armed, for the
    /// same reason [`drain`](Self::drain) does: this is the call the runtime
    /// loop makes before every sleep, and it lets crossterm read, so a
    /// terminal's late answer would otherwise reach crossterm's parser here
    /// rather than through the drain.
    pub fn has_buffered(&mut self) -> bool {
        if self.dead {
            return false;
        }
        self.sweep_late_replies();
        if !self.guard_msgs.is_empty() {
            return true;
        }
        for _ in 0..BUFFERED_POLL_LIMIT {
            match crossterm::event::poll(Duration::ZERO) {
                Ok(true) => return true,
                Ok(false) => {}
                Err(_) => return false,
            }
        }
        false
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
        self.sweep_late_replies();
        for msg in self.guard_msgs.drain(..) {
            sink(msg);
        }
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
            #[cfg(all(unix, feature = "bench-taps"))]
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
