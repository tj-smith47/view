//! Terminal capability detection: sends a batched capability probe right
//! after raw mode is entered (canonical-mode line buffering and echo would
//! otherwise corrupt or swallow the escape replies) and before the
//! alternate screen takes over, so `Term::init` can wire real gating
//! booleans into `Model.caps` instead of the conservative defaults.
//!
//! The probe reads whatever is sitting on stdin, which is not necessarily
//! only the terminal's own replies: a user typing before or during the
//! probe window lands on the same fd. [`detect`] separates the two,
//! returning the leftover bytes as residue instead of discarding them, so
//! `Term::init`'s caller can forward them into nvim and nothing typed at
//! startup is silently lost.
//!
//! Detection runs in two windows, for a terminal that is not on this
//! machine: [`PROBE_DEADLINE`] bounds what `Term::init` spends before the
//! first frame paints, and [`Probe::finish`] waits out the rest of
//! [`PROBE_HARD_CAP`] afterwards, alongside the engine attach rather than
//! in front of it. Detection cost is startup-only either way: one write, a
//! handful of reads, never repeated and never on the key-dispatch path.

use std::io::{self, Write};
use std::time::{Duration, Instant};
use view_core::model::{TermCaps, Tier};

/// How long the probe's first window -- the one `Term::init` spends before
/// the alternate screen goes up and the startup shell frame paints -- waits
/// for capability replies. A terminal on the same machine answers the DA1
/// fence in single-digit milliseconds and ends the window early; this is
/// the bound on what a terminal that answers nothing costs the first frame.
pub const PROBE_DEADLINE: Duration = Duration::from_millis(50);

/// The total the probe may wait for the DA1 fence, measured from the moment
/// the query batch was written.
///
/// The 50ms of [`PROBE_DEADLINE`] is a LAN assumption: over an ssh hop the
/// replies are a network round trip behind the queries and land well after
/// it, leaving `sync`, `kitty_kbd` and the terminal's own truecolor answer
/// false on a terminal that supports all three. Everything past the first
/// window is waited out by [`Probe::finish`], which the caller runs
/// *alongside* the engine attach rather than in front of it, so this budget
/// is spent out of slack the process already had rather than added to the
/// critical path.
pub const PROBE_HARD_CAP: Duration = Duration::from_millis(400);

const QUERY_SYNC: &[u8] = b"\x1b[?2026$p";
const QUERY_KITTY: &[u8] = b"\x1b[?u";
/// Sets a truecolor background, asks the terminal to report the SGR state
/// it actually applied (a DECRQSS readback of `m`), and resets it.
///
/// This is how a terminal is asked whether it renders 24-bit color rather
/// than told to by the environment: `COLORTERM` is set by the emulator and
/// therefore absent in every ssh login whose client does not forward it
/// (Terminal.app never sets it at all), while a terminal that quantized the
/// request down to 256 colors echoes back the SGR it kept, not the one it
/// was handed. The readback itself is the mechanism nvim already relies on:
/// v0.12.4's own startup batch carries `ESC [ 4:3 m  ESC P $ q m  ESC \`,
/// the same question asked about a curly underline instead of a color.
///
/// No cell is painted by this: the set and the reset are adjacent, with no
/// text between them, and both run before the alternate screen exists.
const QUERY_TRUECOLOR: &[u8] = b"\x1b[48;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m";
const QUERY_DA1_FENCE: &[u8] = b"\x1b[c";

/// The SGR parameters [`QUERY_TRUECOLOR`] sets, as the DECRQSS reply must
/// echo them back for the terminal to count as truecolor: a 24-bit
/// background introducer (`48;2`) followed by the three components in
/// order.
const TRUECOLOR_SGR_PARAMS: [&str; 5] = ["48", "2", "1", "2", "3"];

/// A source of capability-probe reply bytes, abstracting the real terminal
/// read so the detection loop in [`detect`] is unit-testable against
/// scripted fixtures instead of a live pty.
pub trait ReplySource {
    /// Returns the next chunk of bytes available within `budget`, or `None`
    /// once the source has nothing further to offer (a real I/O error, or a
    /// test fixture signaling "no more replies coming").
    fn next_chunk(&mut self, budget: Duration) -> Option<Vec<u8>>;
}

impl<S: ReplySource + ?Sized> ReplySource for &mut S {
    fn next_chunk(&mut self, budget: Duration) -> Option<Vec<u8>> {
        (**self).next_chunk(budget)
    }
}

/// Everything one finished capability probe hands its caller.
pub struct ProbeOutcome {
    /// The capabilities every reply that arrived resolves to.
    pub caps: TermCaps,
    /// Every byte the probe read that was not part of a recognized reply --
    /// see [`Probe::finish`].
    pub residue: Vec<u8>,
    /// Whether the DA1 fence arrived. False means the terminal is still
    /// owed replies the probe stopped waiting for, and the caller must keep
    /// them off the input path (see
    /// [`InputSource::open_guarded`](crate::input::InputSource::open_guarded)).
    pub fence_seen: bool,
}

/// A capability probe in flight: the query batch is written, the first
/// reply window is already spent, and whatever the terminal still owes is
/// collected by [`Probe::finish`].
///
/// Split in two because the two halves belong at different points in
/// startup. The first window has to complete before the alternate screen
/// goes up -- `caps.kitty_kbd` decides whether the keyboard protocol is
/// pushed, and the startup shell frame paints at the tier this resolves --
/// while the rest is pure waiting, which the caller overlaps with the
/// engine attach so a slow terminal costs the process nothing it was not
/// already spending on spawning nvim.
pub struct Probe<'a> {
    source: Box<dyn ReplySource + 'a>,
    buf: Vec<u8>,
    started: Instant,
    colorterm: Option<String>,
}

impl<'a> Probe<'a> {
    /// Writes the query batch to `writer` and reads replies for at most
    /// `first_window`, stopping early the moment the DA1 fence arrives.
    ///
    /// `colorterm` is the `COLORTERM` environment value (or `None`), passed
    /// in rather than read here so this stays deterministic and safe to
    /// call from parallel unit tests.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the query batch can't be
    /// written.
    pub fn start(
        source: impl ReplySource + 'a,
        writer: &mut impl Write,
        first_window: Duration,
        colorterm: Option<&str>,
    ) -> io::Result<Self> {
        writer.write_all(QUERY_SYNC)?;
        writer.write_all(QUERY_KITTY)?;
        writer.write_all(QUERY_TRUECOLOR)?;
        // last, and that is the whole point: a terminal answers queries in
        // the order it received them, so this reply arriving proves every
        // earlier one either arrived or is never coming
        writer.write_all(QUERY_DA1_FENCE)?;
        writer.flush()?;

        let mut probe = Self {
            source: Box::new(source),
            buf: Vec::new(),
            started: Instant::now(),
            colorterm: colorterm.map(str::to_owned),
        };
        probe.read_until(first_window);
        Ok(probe)
    }

    /// The capabilities the replies seen so far resolve to.
    #[must_use]
    pub fn caps(&self) -> TermCaps {
        self.caps_from(&scan_replies(&self.buf))
    }

    /// Whether the DA1 fence has arrived, i.e. whether the terminal still
    /// owes this probe anything.
    #[must_use]
    pub fn fence_seen(&self) -> bool {
        scan_replies(&self.buf).da1
    }

    /// Waits out whatever is left of `hard_cap` (measured from the query
    /// write, not from this call) for the fence, then resolves.
    ///
    /// Returns the capabilities alongside the probe's residue: every byte
    /// the source handed back that was not part of a recognized capability
    /// reply. Anything already queued on the fd before or during the probe
    /// window (a user typing at the exact moment the process starts) shows
    /// up here rather than in the reply grammar, so the caller can forward
    /// it into the real input path instead of silently discarding it -- the
    /// bug this two-part return exists to close.
    #[must_use]
    pub fn finish(mut self, hard_cap: Duration) -> ProbeOutcome {
        self.read_until(hard_cap);
        let replies = scan_replies(&self.buf);
        ProbeOutcome {
            caps: self.caps_from(&replies),
            fence_seen: replies.da1,
            residue: replies.residue,
        }
    }

    fn caps_from(&self, replies: &Replies) -> TermCaps {
        let truecolor =
            truecolor_from_colorterm(self.colorterm.as_deref()) || replies.truecolor_reply;
        TermCaps::from_probe(replies.sync, truecolor, replies.kitty)
    }

    fn read_until(&mut self, deadline: Duration) {
        loop {
            if scan_replies(&self.buf).da1 {
                break;
            }
            let elapsed = self.started.elapsed();
            if elapsed >= deadline {
                break;
            }
            let Some(chunk) = self.source.next_chunk(deadline - elapsed) else {
                break;
            };
            self.buf.extend_from_slice(&chunk);
        }
    }
}

/// Runs the whole capability probe against `source` within one deadline:
/// [`Probe::start`] and [`Probe::finish`] back to back, for a caller with
/// no other work to overlap the wait with.
///
/// Returns the resolved capabilities alongside the probe's residue -- see
/// [`Probe::finish`] for what residue is and why it is handed back rather
/// than dropped.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the query batch can't be
/// written.
pub fn detect<S: ReplySource>(
    source: &mut S,
    writer: &mut impl Write,
    deadline: Duration,
    colorterm: Option<&str>,
) -> io::Result<(TermCaps, Vec<u8>)> {
    let outcome = Probe::start(source, writer, deadline, colorterm)?.finish(deadline);
    Ok((outcome.caps, outcome.residue))
}

/// Derives capabilities from a `--tier` override: deterministic, not
/// half-probed, so the booleans are picked directly rather than partially
/// trusting a probe. `full` sets every boolean, `standard` sets only
/// `truecolor`, `basic` sets none.
#[must_use]
pub fn caps_for_override(tier: Tier) -> TermCaps {
    match tier {
        Tier::Full => TermCaps::from_probe(true, true, true),
        Tier::Standard => TermCaps::from_probe(false, true, false),
        Tier::Basic => TermCaps::from_probe(false, false, false),
        // `Tier` is `#[non_exhaustive]`; an override the caller passed a
        // future variant for degrades to the same all-false floor as an
        // unanswered probe rather than failing to compile.
        _ => TermCaps::from_probe(false, false, false),
    }
}

/// Reads `COLORTERM` as the truecolor signal: `truecolor` or `24bit` (case
/// sensitive, matching the values terminals actually emit) means truecolor
/// is supported. Its absence proves nothing -- see [`QUERY_TRUECOLOR`] for
/// the question that does -- so the two are OR'd, never ranked.
fn truecolor_from_colorterm(value: Option<&str>) -> bool {
    matches!(value, Some("truecolor") | Some("24bit"))
}

/// Whether a DECRQSS reply body (everything between the `ESC P` introducer
/// and the string terminator) is the terminal reporting that it kept
/// [`QUERY_TRUECOLOR`]'s 24-bit background.
///
/// A valid response opens `1 $ r` and closes with the final byte of the
/// setting it echoes (`m`, for SGR); `0 $ r` is the terminal saying the
/// request was invalid. The echoed parameters are searched rather than
/// compared whole because a terminal may report its entire SGR state
/// (`0;48;2;1;2;3`), and both `;` and `:` are accepted between the
/// components -- the two separators are interchangeable in the spec and
/// terminals differ over which they echo.
fn truecolor_from_decrqss(body: &[u8]) -> bool {
    let Some(sgr) = body
        .strip_prefix(b"1$r")
        .and_then(|rest| rest.strip_suffix(b"m"))
    else {
        return false;
    };
    let Ok(sgr) = std::str::from_utf8(sgr) else {
        return false;
    };
    sgr.split([';', ':'])
        .collect::<Vec<_>>()
        .windows(TRUECOLOR_SGR_PARAMS.len())
        .any(|window| window == TRUECOLOR_SGR_PARAMS)
}

/// Splits a DCS at its string terminator, returning the body before it and
/// the offset just past it.
///
/// Both terminators terminals use in practice are accepted: the spec's
/// `ESC \` and the `BEL` many emulators also honor. Whichever appears first
/// ends the string, so a `BEL` inside a reply already closed by `ESC \`
/// cannot swallow the bytes that follow it.
fn dcs_body(after_intro: &[u8]) -> Option<(&[u8], usize)> {
    let st = after_intro
        .windows(2)
        .position(|w| w == b"\x1b\\")
        .map(|at| (at, at + 2));
    let bel = after_intro
        .iter()
        .position(|&b| b == 0x07)
        .map(|at| (at, at + 1));
    let (end, past) = [st, bel].into_iter().flatten().min_by_key(|(at, _)| *at)?;
    Some((&after_intro[..end], past))
}

/// What one scan of the probe's read buffer found.
pub(crate) struct Replies {
    sync: bool,
    kitty: bool,
    pub(crate) da1: bool,
    /// The terminal's own answer about 24-bit color, independent of
    /// `COLORTERM`.
    truecolor_reply: bool,
    pub(crate) residue: Vec<u8>,
    /// How many leading bytes of the buffer this scan accounted for.
    /// Everything past it is a reply the terminal has only half delivered,
    /// which the next chunk completes.
    pub(crate) consumed: usize,
}

/// Scans `buf` for every reply detection cares about.
///
/// Two shapes are treated as replies: private-mode CSI sequences
/// (`ESC [ ?` ...), which is what DECRPM, the kitty flags and DA1 all
/// answer with, and a DCS (`ESC P` ...), which is what the DECRQSS
/// truecolor readback answers with. A byte that is not part of one is
/// appended to `residue` instead of being discarded: nothing on this fd but
/// the terminal itself answers the probe's own query batch in either shape,
/// so anything else -- including a plain `ESC [` sequence like an arrow key
/// -- came from somewhere else, almost always keystrokes queued before or
/// during the probe window, and must survive to be forwarded rather than
/// vanish.
///
/// A reply of either shape with no terminator yet by the end of `buf` (the
/// terminal's own answer, cut off by the probe's deadline) is dropped
/// rather than added to `residue`, and `consumed` stops short of it: a
/// keyboard cannot produce `ESC [ ?` or `ESC P`, so a truncated fragment
/// matching either shape is always a half-delivered answer, never something
/// to replay into nvim.
pub(crate) fn scan_replies(buf: &[u8]) -> Replies {
    let mut sync = false;
    let mut kitty = false;
    let mut da1 = false;
    let mut truecolor_reply = false;
    let mut residue = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf[i] == 0x1b && matches!(buf.get(i + 1), Some(&b'P')) {
            let Some((body, past)) = dcs_body(&buf[i + 2..]) else {
                // an unterminated DCS: the rest of it is still in flight,
                // and none of these bytes is anyone's keystroke
                break;
            };
            truecolor_reply |= truecolor_from_decrqss(body);
            i += 2 + past;
            continue;
        }
        // a CSI introducer ending the read is the terminal mid-reply, not
        // typed input: forwarding it would replay an Escape plus a literal
        // `[` into the engine, the same reason the >=3-byte truncation
        // below is dropped. A lone trailing ESC stays residue -- that is
        // what the Escape key produces, so dropping it would eat a key
        if buf[i] == 0x1b && i + 1 < buf.len() && buf[i + 1] == b'[' && i + 2 >= buf.len() {
            break;
        }
        let is_private_csi_start =
            i + 2 < buf.len() && buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?';
        if !is_private_csi_start {
            residue.push(buf[i]);
            i += 1;
            continue;
        }
        let params_start = i + 3;
        let mut j = params_start;
        while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
            j += 1;
        }
        if j >= buf.len() {
            // an in-flight reply with no final byte yet: nothing more to
            // find until the next chunk arrives, and the bytes seen so far
            // are the terminal's own, not residue
            break;
        }
        let params = &buf[params_start..j];
        match buf[j] {
            b'y' if is_sync_supported(params) => sync = true,
            b'u' => kitty = true,
            b'c' => da1 = true,
            _ => {}
        }
        i = j + 1;
    }
    Replies {
        sync,
        kitty,
        da1,
        truecolor_reply,
        residue,
        consumed: i,
    }
}

/// `params` is a DECRPM reply's parameter bytes between `?` and the final
/// `y`: `Pd ; Pm $`. Supported means the mode number is 2026 (synchronized
/// output) and the state is `1` (set) or `2` (reset); both mean the
/// terminal recognizes the mode. `0` (not recognized) does not. `3`/`4`
/// (permanently set/reset) are treated as unsupported here too: a
/// terminal that locks the mode can't be toggled per frame, which is what
/// `caps.sync` promises callers, even though the DECRPM grammar itself
/// marks `3`/`4` as "recognized" the same as `1`/`2`.
fn is_sync_supported(params: &[u8]) -> bool {
    let Some(core) = params.strip_suffix(b"$") else {
        return false;
    };
    let Some(sep) = core.iter().position(|&b| b == b';') else {
        return false;
    };
    let pd = parse_ascii_u32(&core[..sep]);
    let pm = parse_ascii_u32(&core[sep + 1..]);
    pd == Some(2026) && matches!(pm, Some(1) | Some(2))
}

fn parse_ascii_u32(digits: &[u8]) -> Option<u32> {
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// The real capability-probe reply source: stdin, put into a
/// non-blocking-style read mode (`VMIN=0, VTIME=0`) for the probe's
/// duration only.
///
/// A background thread doing an ordinary blocking `read()` would still be
/// parked in that syscall after the probe's deadline on a terminal that
/// never replies, and would go on to race whatever reads stdin next (the
/// real input reader (the runtime loop's inline drain on unix, the
/// dedicated input thread elsewhere)
/// starts once `Term::init` returns) for the first bytes that terminal ever
/// sends, silently stealing a keystroke. Reading synchronously on the
/// calling thread with `VMIN=0`/`VTIME=0` means every read call returns
/// immediately, so this source is fully done (no thread, no pending
/// syscall) by the time [`detect`] returns.
#[cfg(unix)]
struct StdinReplySource {
    saved: rustix::termios::Termios,
}

#[cfg(unix)]
impl StdinReplySource {
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the terminal attributes
    /// can't be read or changed.
    fn new() -> io::Result<Self> {
        let stdin = io::stdin();
        let saved = rustix::termios::tcgetattr(&stdin)?;
        let mut probing = saved.clone();
        probing.special_codes[rustix::termios::SpecialCodeIndex::VMIN] = 0;
        probing.special_codes[rustix::termios::SpecialCodeIndex::VTIME] = 0;
        rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &probing)?;
        Ok(Self { saved })
    }
}

#[cfg(unix)]
impl Drop for StdinReplySource {
    fn drop(&mut self) {
        let stdin = io::stdin();
        let _ =
            rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &self.saved);
    }
}

#[cfg(unix)]
impl ReplySource for StdinReplySource {
    fn next_chunk(&mut self, _budget: Duration) -> Option<Vec<u8>> {
        // Reads the fd directly via `rustix::io::read`, deliberately
        // bypassing std's locking, internally-buffered handle to this same
        // fd: that handle can pull more than 256 bytes out of the kernel in
        // one call and strand the remainder in its own userspace buffer,
        // where the crossterm reader that takes over after the probe can
        // never see it. A raw read leaves every
        // byte beyond what this call consumes still sitting in the kernel's
        // tty input queue for the next reader to pick up.
        use std::os::fd::AsFd;
        let mut buf = [0_u8; 256];
        match rustix::io::read(io::stdin().as_fd(), &mut buf) {
            Ok(0) => {
                // VMIN=0/VTIME=0 returns immediately with nothing rather
                // than blocking; a short sleep keeps the caller's deadline
                // loop from busy-spinning the CPU while it waits the rest
                // of the probe out.
                std::thread::sleep(Duration::from_millis(1));
                Some(Vec::new())
            }
            Ok(n) => Some(buf[..n].to_vec()),
            Err(_) => None,
        }
    }
}

/// Runs the full startup capability resolution: a `--tier` override wins
/// outright and skips all terminal I/O; otherwise the real probe runs
/// against stdin/stdout. Either way, logs the chosen capabilities and why
/// to stderr before returning, so `Term::init` can call this between
/// entering raw mode and entering the alternate screen: the log line must
/// land while stderr is still the visible screen, not scrolled away under
/// the alt-screen buffer.
///
/// Returns the capabilities the first window resolved alongside the probe
/// still in flight, if one is. The caller runs [`Probe::finish`] on it once
/// it has other work in the air (see [`Probe`]); until then these
/// capabilities are what the terminal has already admitted to, never less.
/// `None` means there is nothing left to wait for: a `--tier` override, or
/// a launch shape no probe can run in at all.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the probe's I/O fails.
pub fn resolve(tier_override: Option<Tier>) -> io::Result<(TermCaps, Option<Probe<'static>>)> {
    if let Some(tier) = tier_override {
        let caps = caps_for_override(tier);
        log_caps(&caps, "--tier override");
        return Ok((caps, None));
    }
    let (caps, probe) = probe_real_terminal()?;
    let source = match &probe {
        Some(probe) if !probe.fence_seen() => PROBE_PENDING_LABEL,
        _ => PROBE_SOURCE_LABEL,
    };
    log_caps(&caps, source);
    Ok((caps, probe))
}

/// [`log_caps`]'s label for a non-overridden result: `"probed"` on unix,
/// where a real probe always at least attempts terminal I/O (even a
/// `tcgetattr` failure on a non-tty stdin is "probed, got nothing" -- the
/// same all-false-Basic outcome an unresponsive real terminal produces);
/// `"assumed"` everywhere else, where no terminal I/O is attempted at all
/// (see [`probe_real_terminal`]'s `cfg(not(unix))` arm), so labeling it
/// "probed" would claim a negotiation that never happened.
#[cfg(unix)]
const PROBE_SOURCE_LABEL: &str = "probed";
#[cfg(not(unix))]
const PROBE_SOURCE_LABEL: &str = "assumed";

/// [`log_caps`]'s label for a probe whose fence has not arrived yet. This
/// line is written before the alternate screen goes up and can therefore
/// never be revised on screen, so it says outright that the answer is not
/// final; the session's own `VIEW_LOG` startup line, written once
/// [`Probe::finish`] has run, is the record of what the session settled on.
const PROBE_PENDING_LABEL: &str = "probed, still listening";

#[cfg(unix)]
fn probe_real_terminal() -> io::Result<(TermCaps, Option<Probe<'static>>)> {
    let colorterm = std::env::var("COLORTERM").ok();
    // A `tcgetattr` failure means stdin is not a real tty (e.g. redirected
    // from `/dev/null`, as a headless launch or a test harness might do):
    // there is no raw-mode state to probe through and no escape reply will
    // ever arrive, but that is not a reason to abort startup, nor to ignore
    // `COLORTERM`: stdout can still be a real truecolor tty in this launch
    // shape (stdin and stdout are independent fds) even though stdin isn't
    // one, exactly like the `cfg(not(unix))` arm below already does.
    // Degrading sync/kitty_kbd to the same false floor an unanswered probe
    // already produces keeps this a non-fatal, silent-by-design outcome
    // rather than a hard failure the caller must special-case.
    let source = match StdinReplySource::new() {
        Ok(source) => source,
        Err(_) => return Ok((no_probe_caps(colorterm.as_deref()), None)),
    };
    let probe = Probe::start(
        source,
        &mut io::stdout(),
        PROBE_DEADLINE,
        colorterm.as_deref(),
    )?;
    Ok((probe.caps(), Some(probe)))
}

#[cfg(not(unix))]
fn probe_real_terminal() -> io::Result<(TermCaps, Option<Probe<'static>>)> {
    // Bounding a raw stdin read without a background thread that could
    // outlive the probe needs a termios VMIN/VTIME equivalent this crate
    // only has for unix; COLORTERM is still honored since that's a plain
    // env read, not an escape probe. No stdin read happens here at all, so
    // there is no residue to return either.
    let colorterm = std::env::var("COLORTERM").ok();
    Ok((no_probe_caps(colorterm.as_deref()), None))
}

/// The capabilities floor for a launch shape where no escape-sequence probe
/// can run at all: sync and kitty_kbd are always false (both need a real
/// reply), but truecolor still honors `COLORTERM` -- stdout can be a real
/// truecolor tty independent of whether stdin is probeable, since the two
/// are independent fds. Shared by both `probe_real_terminal` arms: the
/// unix arm's `tcgetattr`-failure fallback (non-tty stdin, e.g.
/// `/dev/null`) and the `cfg(not(unix))` arm (no termios equivalent to
/// bound a raw read with at all).
fn no_probe_caps(colorterm: Option<&str>) -> TermCaps {
    TermCaps::from_probe(false, truecolor_from_colorterm(colorterm), false)
}

fn log_caps(caps: &TermCaps, source: &str) {
    // eprintln! ends the line with a bare `\n`; raw mode disables OPOST, so
    // the terminal never translates that to a carriage return on its own,
    // leaving this log line's next line starting mid-column instead of at
    // the left margin. This runs while still in raw mode (pre-alt-screen),
    // so the `\r` has to be explicit.
    eprint!(
        "view: terminal capabilities: tier={:?} sync={} truecolor={} kitty_kbd={} ({source})\r\n",
        caps.tier, caps.sync, caps.truecolor, caps.kitty_kbd
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::collections::VecDeque;

    /// A scripted [`ReplySource`]: each call to `next_chunk` pops the next
    /// entry regardless of the requested budget, so tests run instantly
    /// instead of waiting out real wall-clock time. `None` in the script
    /// means "source exhausted starting here", the same signal a real I/O
    /// error or an unresponsive terminal's deadline produces.
    struct ScriptedSource {
        script: VecDeque<Option<Vec<u8>>>,
    }

    impl ScriptedSource {
        fn new(script: Vec<Option<&[u8]>>) -> Self {
            Self {
                script: script.into_iter().map(|c| c.map(<[u8]>::to_vec)).collect(),
            }
        }
    }

    impl ReplySource for ScriptedSource {
        fn next_chunk(&mut self, _budget: Duration) -> Option<Vec<u8>> {
            self.script.pop_front().flatten()
        }
    }

    fn tier_name(tier: Tier) -> &'static str {
        match tier {
            Tier::Full => "full",
            Tier::Standard => "standard",
            Tier::Basic => "basic",
            // `Tier` is `#[non_exhaustive]`; a future variant this test
            // doesn't know about must not fail to compile.
            _ => "unknown",
        }
    }

    struct Case {
        name: &'static str,
        replies: &'static [u8],
        colorterm: Option<&'static str>,
        expect_tier: &'static str,
        expect_sync: bool,
        expect_truecolor: bool,
        expect_kitty: bool,
    }

    #[test]
    fn mapping_table_from_injected_capability_sets() {
        let cases = [
            Case {
                name: "fully replying fake yields full",
                replies: b"\x1b[?2026;1$y\x1b[?1u\x1b[?62c",
                colorterm: Some("truecolor"),
                expect_tier: "full",
                expect_sync: true,
                expect_truecolor: true,
                expect_kitty: true,
            },
            Case {
                name: "truecolor only (no escape replies at all) yields standard",
                replies: b"\x1b[?62c",
                colorterm: Some("truecolor"),
                expect_tier: "standard",
                expect_sync: false,
                expect_truecolor: true,
                expect_kitty: false,
            },
            Case {
                name: "sync supported but no truecolor still yields basic",
                replies: b"\x1b[?2026;1$y\x1b[?62c",
                colorterm: None,
                expect_tier: "basic",
                expect_sync: true,
                expect_truecolor: false,
                expect_kitty: false,
            },
            Case {
                name: "kitty supported but no truecolor still yields basic",
                replies: b"\x1b[?1u\x1b[?62c",
                colorterm: None,
                expect_tier: "basic",
                expect_sync: false,
                expect_truecolor: false,
                expect_kitty: true,
            },
            Case {
                name: "da1 with no preceding capability replies yields basic",
                replies: b"\x1b[?62c",
                colorterm: None,
                expect_tier: "basic",
                expect_sync: false,
                expect_truecolor: false,
                expect_kitty: false,
            },
            Case {
                name: "decrpm not-recognized state (0) is not supported",
                replies: b"\x1b[?2026;0$y\x1b[?62c",
                colorterm: Some("truecolor"),
                expect_tier: "standard",
                expect_sync: false,
                expect_truecolor: true,
                expect_kitty: false,
            },
            Case {
                name: "decrpm permanently-set state (3) is treated as unsupported",
                replies: b"\x1b[?2026;3$y\x1b[?62c",
                colorterm: Some("truecolor"),
                expect_tier: "standard",
                expect_sync: false,
                expect_truecolor: true,
                expect_kitty: false,
            },
            Case {
                name: "decrpm permanently-reset state (4) is treated as unsupported",
                replies: b"\x1b[?2026;4$y\x1b[?62c",
                colorterm: Some("truecolor"),
                expect_tier: "standard",
                expect_sync: false,
                expect_truecolor: true,
                expect_kitty: false,
            },
            Case {
                name: "colorterm 24bit also counts as truecolor",
                replies: b"\x1b[?62c",
                colorterm: Some("24bit"),
                expect_tier: "standard",
                expect_sync: false,
                expect_truecolor: true,
                expect_kitty: false,
            },
        ];

        for case in cases {
            let mut source = ScriptedSource::new(vec![Some(case.replies)]);
            let mut sink = Vec::new();
            let (caps, _residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, case.colorterm)
                .expect("detect should not error against an in-memory writer");
            assert_eq!(caps.sync, case.expect_sync, "{}: sync", case.name);
            assert_eq!(
                caps.truecolor, case.expect_truecolor,
                "{}: truecolor",
                case.name
            );
            assert_eq!(
                caps.kitty_kbd, case.expect_kitty,
                "{}: kitty_kbd",
                case.name
            );
            assert_eq!(
                tier_name(caps.tier),
                case.expect_tier,
                "{}: tier",
                case.name
            );
        }
    }

    #[test]
    fn reply_split_across_multiple_chunks_is_reassembled() {
        let mut source = ScriptedSource::new(vec![
            Some(b"\x1b[?2026;1$y\x1b[?".as_slice()),
            Some(b"1u\x1b[?62c".as_slice()),
        ]);
        let mut sink = Vec::new();
        let (caps, _residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, Some("truecolor")).unwrap();
        assert!(caps.sync);
        assert!(caps.kitty_kbd);
        assert!(caps.truecolor);
    }

    #[test]
    fn writes_the_query_batch_before_reading_any_reply() {
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[?62c".as_slice())]);
        let mut sink = Vec::new();
        detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert_eq!(
            sink, b"\x1b[?2026$p\x1b[?u\x1b[48;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m\x1b[c",
            "the DA1 fence must stay last -- it is what proves every earlier \
             query has been answered -- and the truecolor readback must be \
             bracketed by the SGR set and its reset"
        );
    }

    #[test]
    fn override_wins_and_derives_booleans_deterministically() {
        let full = caps_for_override(Tier::Full);
        assert!(full.sync && full.truecolor && full.kitty_kbd);
        assert_eq!(tier_name(full.tier), "full");

        let standard = caps_for_override(Tier::Standard);
        assert!(!standard.sync && standard.truecolor && !standard.kitty_kbd);
        assert_eq!(tier_name(standard.tier), "standard");

        let basic = caps_for_override(Tier::Basic);
        assert!(!basic.sync && !basic.truecolor && !basic.kitty_kbd);
        assert_eq!(tier_name(basic.tier), "basic");
    }

    #[test]
    fn never_replying_fake_yields_all_false_caps_and_never_hangs() {
        let mut source = ScriptedSource::new(vec![None]);
        let mut sink = Vec::new();
        let start = Instant::now();
        let (caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert!(
            start.elapsed() < view_test_support::host_deadline(Duration::from_millis(500)),
            "detect blocked for {:?} against a source that reported itself \
             exhausted on the first call",
            start.elapsed()
        );
        assert!(!caps.sync && !caps.truecolor && !caps.kitty_kbd);
        assert_eq!(tier_name(caps.tier), "basic");
        assert!(residue.is_empty());
    }

    #[test]
    fn deadline_bounds_the_loop_even_against_an_always_empty_source() {
        let mut source = ScriptedSource::new(vec![Some(b"".as_slice()); 100_000]);
        let short_deadline = Duration::from_millis(5);
        let mut sink = Vec::new();
        let start = Instant::now();
        let (caps, residue) = detect(&mut source, &mut sink, short_deadline, None).unwrap();
        assert!(
            start.elapsed() < view_test_support::host_deadline(Duration::from_millis(200)),
            "detect took {:?} against a source that never stops offering \
             empty chunks",
            start.elapsed()
        );
        assert!(!caps.sync && !caps.truecolor && !caps.kitty_kbd);
        assert!(residue.is_empty());
    }

    #[test]
    fn typed_bytes_preceding_replies_are_preserved_as_residue() {
        // matches the real failure shape: a user types before the terminal's
        // replies come back, so the plain bytes arrive in an earlier read
        // than the escape replies do
        let mut source = ScriptedSource::new(vec![
            Some(b"ityped-before-reply".as_slice()),
            Some(b"\x1b[?2026;1$y\x1b[?1u\x1b[?62c".as_slice()),
        ]);
        let mut sink = Vec::new();
        let (caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, Some("truecolor")).unwrap();
        assert!(caps.sync && caps.truecolor && caps.kitty_kbd);
        assert_eq!(residue, b"ityped-before-reply");
    }

    #[test]
    fn non_private_mode_escape_sequences_are_preserved_in_residue() {
        // an arrow key (`ESC [ A`, no `?`) typed during the probe window
        // does not match the private-mode reply grammar, so it must survive
        // into residue untouched rather than being swallowed as if it were
        // one of the probe's own DA1/DECRPM/kitty replies
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[A\x1b[?62c".as_slice())]);
        let mut sink = Vec::new();
        let (_caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert_eq!(residue, b"\x1b[A");
    }

    #[test]
    fn a_two_byte_csi_prefix_at_end_of_read_is_dropped_not_forwarded() {
        // the same half-delivered reply cut one byte earlier. `ESC [` ending
        // a read is the terminal mid-reply, and forwarding it replays an
        // Escape plus a literal `[` into the engine as if typed
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[".as_slice()), None]);
        let mut sink = Vec::new();
        let (_caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert!(
            residue.is_empty(),
            "residue should be empty, got {residue:?}"
        );
    }

    #[test]
    fn a_lone_trailing_escape_is_still_forwarded_as_typed_input() {
        // the boundary of the rule above: a bare `ESC` is what the Escape
        // key produces, so dropping it would eat a keystroke. Only a
        // confirmed CSI introducer is treated as an in-flight reply
        let mut source = ScriptedSource::new(vec![Some(b"\x1b".as_slice()), None]);
        let mut sink = Vec::new();
        let (_caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert_eq!(residue, b"\x1b", "a typed Escape must survive the probe");
    }

    #[test]
    fn a_trailing_csi_introducer_is_dropped_rather_than_replayed_as_two_keys() {
        // the other side of that boundary: `ESC [` ending the read is the
        // terminal mid-reply, and `encode_residue_bytes` would turn it into
        // an Escape followed by a literal `[` typed into the buffer
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[".as_slice()), None]);
        let mut sink = Vec::new();
        let (_caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert!(
            residue.is_empty(),
            "residue should be empty, got {residue:?}"
        );
    }

    #[test]
    fn trailing_incomplete_private_mode_reply_is_dropped_not_forwarded() {
        // a DA1 reply cut off by the deadline mid-sequence: this shape is
        // only ever the terminal's own half-delivered reply (a keyboard
        // cannot produce `ESC [ ?`), so it must never leak into residue as
        // if a user had typed it
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[?62".as_slice()), None]);
        let mut sink = Vec::new();
        let (_caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert!(
            residue.is_empty(),
            "residue should be empty, got {residue:?}"
        );
    }

    #[test]
    fn residue_bytes_before_an_incomplete_trailing_reply_still_survive() {
        let mut source = ScriptedSource::new(vec![Some(b"typed\x1b[?62".as_slice()), None]);
        let mut sink = Vec::new();
        let (_caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert_eq!(residue, b"typed");
    }

    /// A source that hands its whole script over only once `delay` has
    /// really elapsed, so a test can place a reply on either side of a
    /// deadline in wall-clock terms rather than in script order.
    struct LateSource {
        delay: Duration,
        opened: Instant,
        replies: Option<&'static [u8]>,
    }

    impl LateSource {
        fn new(delay: Duration, replies: &'static [u8]) -> Self {
            Self {
                delay,
                opened: Instant::now(),
                replies: Some(replies),
            }
        }
    }

    impl ReplySource for LateSource {
        fn next_chunk(&mut self, budget: Duration) -> Option<Vec<u8>> {
            let waited = self.opened.elapsed();
            if waited < self.delay {
                std::thread::sleep(budget.min(self.delay - waited));
                return Some(Vec::new());
            }
            Some(self.replies.take().unwrap_or_default().to_vec())
        }
    }

    /// The DECRQSS answer a truecolor terminal gives [`QUERY_TRUECOLOR`],
    /// reporting its whole SGR state rather than only the parameters asked
    /// about -- the shape a reply actually arrives in.
    const TRUECOLOR_REPLY: &[u8] = b"\x1bP1$r0;48;2;1;2;3m\x1b\\";

    #[test]
    fn decrqss_grammar_decides_truecolor_from_what_the_terminal_echoed() {
        let cases: [(&str, &[u8], bool); 7] = [
            (
                "semicolon separators, whole SGR state echoed",
                b"1$r0;48;2;1;2;3m",
                true,
            ),
            (
                "colon separators (the other form terminals echo)",
                b"1$r48:2:1:2:3m",
                true,
            ),
            (
                "a terminal that quantized the request to 256 colors",
                b"1$r48;5;17m",
                false,
            ),
            (
                "an invalid-request answer carries no setting at all",
                b"0$r",
                false,
            ),
            (
                "the components must be the ones that were set",
                b"1$r48;2;9;9;9m",
                false,
            ),
            (
                "a reply about some other setting is not an SGR answer",
                b"1$r2 q",
                false,
            ),
            ("an empty body decides nothing", b"", false),
        ];
        for (name, body, want) in cases {
            assert_eq!(truecolor_from_decrqss(body), want, "{name}");
        }
    }

    #[test]
    fn the_terminals_truecolor_answer_reaches_full_without_colorterm() {
        let mut source = ScriptedSource::new(vec![Some(
            b"\x1b[?2026;1$y\x1b[?1u\x1bP1$r0;48;2;1;2;3m\x1b\\\x1b[?62c".as_slice(),
        )]);
        let mut sink = Vec::new();
        let (caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert!(
            caps.truecolor,
            "the terminal itself said it kept a 24-bit background; an unset \
             COLORTERM (every ssh login whose client does not forward it) \
             must not override that"
        );
        assert_eq!(tier_name(caps.tier), "full");
        assert!(
            residue.is_empty(),
            "the DECRQSS answer is a reply, not typed input: {residue:?}"
        );
    }

    #[test]
    fn colorterm_still_decides_truecolor_when_the_terminal_never_answers_it() {
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[?2026;1$y\x1b[?1u\x1b[?62c")]);
        let mut sink = Vec::new();
        let (caps, _residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, Some("truecolor")).unwrap();
        assert!(caps.truecolor, "COLORTERM keeps its own, unchanged say");
        assert_eq!(tier_name(caps.tier), "full");
    }

    #[test]
    fn a_truncated_decrqss_answer_is_dropped_rather_than_forwarded() {
        // the reply cut off mid-body by the deadline: only the terminal can
        // produce `ESC P`, so this is never something to replay into nvim
        let mut source = ScriptedSource::new(vec![Some(b"\x1bP1$r0;48;2;1;2"), None]);
        let mut sink = Vec::new();
        let (caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert!(!caps.truecolor, "half an answer answers nothing");
        assert!(residue.is_empty(), "residue should be empty: {residue:?}");
    }

    #[test]
    fn a_bel_terminated_decrqss_answer_is_read_and_consumed() {
        let mut source = ScriptedSource::new(vec![Some(b"\x1bP1$r48;2;1;2;3m\x07typed")]);
        let mut sink = Vec::new();
        let (caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert!(caps.truecolor);
        assert_eq!(
            residue, b"typed",
            "the BEL ends the reply; what follows it is the user's"
        );
    }

    #[test]
    fn a_reply_arriving_after_the_first_window_still_reaches_full() {
        // the ssh-over-a-WAN shape: the fence lands well past
        // PROBE_DEADLINE, and the second window is what catches it
        let source = LateSource::new(
            Duration::from_millis(120),
            b"\x1b[?2026;1$y\x1b[?1u\x1bP1$r0;48;2;1;2;3m\x1b\\\x1b[?62c",
        );
        let mut sink = Vec::new();
        let probe = Probe::start(source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert_eq!(
            tier_name(probe.caps().tier),
            "basic",
            "nothing has answered inside the first window yet"
        );
        assert!(!probe.fence_seen());

        let outcome = probe.finish(PROBE_HARD_CAP);
        assert_eq!(tier_name(outcome.caps.tier), "full");
        assert!(outcome.caps.sync && outcome.caps.truecolor && outcome.caps.kitty_kbd);
        assert!(outcome.fence_seen);
    }

    #[test]
    fn a_fence_already_seen_makes_the_second_window_return_at_once() {
        // what keeps the overlapped wait off the startup critical path for
        // every terminal that answers promptly: there is nothing left to
        // wait for, so `finish` never sleeps out the hard cap
        let mut source = ScriptedSource::new(vec![Some(TRUECOLOR_REPLY), Some(b"\x1b[?62c")]);
        let mut sink = Vec::new();
        let probe = Probe::start(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert!(probe.fence_seen());
        let start = Instant::now();
        let outcome = probe.finish(PROBE_HARD_CAP);
        assert!(
            start.elapsed() < view_test_support::host_deadline(Duration::from_millis(50)),
            "finish waited {:?} on a probe that was already fenced",
            start.elapsed()
        );
        assert!(outcome.caps.truecolor && outcome.fence_seen);
    }

    #[test]
    fn an_unanswered_probe_reports_its_fence_missing_so_the_caller_can_guard() {
        let mut source = ScriptedSource::new(vec![None]);
        let mut sink = Vec::new();
        let probe = Probe::start(&mut source, &mut sink, Duration::from_millis(5), None)
            .expect("start must not error against an in-memory writer");
        assert!(!probe.fence_seen());
        assert!(!probe.finish(Duration::from_millis(10)).fence_seen);
    }

    #[test]
    fn no_probe_caps_honors_colorterm_even_though_sync_and_kitty_stay_false() {
        // covers both `probe_real_terminal` arms that can never get a real
        // escape reply (unix tcgetattr failure on a non-tty stdin, and the
        // cfg(not(unix)) arm): truecolor must still reflect COLORTERM since
        // stdout can be a real truecolor tty independent of stdin.
        let none = no_probe_caps(None);
        assert!(!none.sync && !none.truecolor && !none.kitty_kbd);
        assert_eq!(tier_name(none.tier), "basic");

        let truecolor = no_probe_caps(Some("truecolor"));
        assert!(!truecolor.sync && truecolor.truecolor && !truecolor.kitty_kbd);
        assert_eq!(tier_name(truecolor.tier), "standard");

        let bit24 = no_probe_caps(Some("24bit"));
        assert!(bit24.truecolor);

        let unrecognized = no_probe_caps(Some("bogus"));
        assert!(!unrecognized.truecolor);
    }
}
