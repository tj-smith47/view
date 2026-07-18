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
//! Detection cost is startup-only: one write, a handful of reads bounded by
//! [`PROBE_DEADLINE`], never repeated and never on the key-dispatch path.

use std::io::{self, Write};
use std::time::{Duration, Instant};
use view_core::model::{TermCaps, Tier};

/// Upper bound on how long the startup probe waits for capability replies.
/// Real terminals answer the DA1 fence within a few milliseconds; this is
/// the safety net for a terminal that ignores even DA1, not the common
/// case.
pub const PROBE_DEADLINE: Duration = Duration::from_millis(50);

const QUERY_SYNC: &[u8] = b"\x1b[?2026$p";
const QUERY_KITTY: &[u8] = b"\x1b[?u";
const QUERY_DA1_FENCE: &[u8] = b"\x1b[c";

/// A source of capability-probe reply bytes, abstracting the real terminal
/// read so the detection loop in [`detect`] is unit-testable against
/// scripted fixtures instead of a live pty.
pub trait ReplySource {
    /// Returns the next chunk of bytes available within `budget`, or `None`
    /// once the source has nothing further to offer (a real I/O error, or a
    /// test fixture signaling "no more replies coming").
    fn next_chunk(&mut self, budget: Duration) -> Option<Vec<u8>>;
}

/// Runs the batched capability probe against `source`, writing the query
/// batch to `writer` first. `colorterm` is the `COLORTERM` environment
/// value (or `None`), passed in rather than read here so this function
/// stays deterministic and safe to call from parallel unit tests.
///
/// Returns the resolved capabilities alongside the probe's residue: every
/// byte `source` handed back that was not part of a recognized capability
/// reply. Anything already queued on the fd before or during the probe
/// window (a user typing at the exact moment the process starts) shows up
/// here rather than the reply-scanning grammar, so the caller can forward
/// it into the real input path instead of silently discarding it -- the
/// bug this two-part return exists to close.
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
    writer.write_all(QUERY_SYNC)?;
    writer.write_all(QUERY_KITTY)?;
    writer.write_all(QUERY_DA1_FENCE)?;
    writer.flush()?;

    let mut buf = Vec::new();
    let start = Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= deadline {
            break;
        }
        let Some(chunk) = source.next_chunk(deadline - elapsed) else {
            break;
        };
        buf.extend_from_slice(&chunk);
        if scan_csi_replies(&buf).2 {
            break;
        }
    }

    let (sync, kitty_kbd, _, residue) = scan_csi_replies(&buf);
    let truecolor = truecolor_from_colorterm(colorterm);
    Ok((TermCaps::from_probe(sync, truecolor, kitty_kbd), residue))
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
/// is supported.
fn truecolor_from_colorterm(value: Option<&str>) -> bool {
    matches!(value, Some("truecolor") | Some("24bit"))
}

/// Scans `buf` for the three CSI replies detection cares about, returning
/// `(sync_supported, kitty_supported, da1_seen, residue)`.
///
/// Only private-mode CSI sequences (`ESC [ ?` ...) are treated as replies:
/// every reply this probe cares about (DECRPM, kitty flags, DA1) is one of
/// those. A byte that is not part of one is appended to `residue` instead
/// of being discarded: nothing on this fd but the terminal itself can
/// produce a private-mode CSI sequence in answer to our own query batch, so
/// anything else -- including a plain `ESC [` sequence like an arrow key --
/// came from somewhere else, almost always keystrokes queued before or
/// during the probe window, and must survive to be forwarded rather than
/// vanish.
///
/// A private-mode CSI sequence with no final byte yet by the end of `buf`
/// (the terminal's own reply, cut off by the probe's deadline) is dropped
/// rather than added to `residue`: a keyboard cannot produce `ESC [ ?`, so
/// a truncated fragment matching that exact shape is always the terminal's
/// half-delivered answer, never something to replay into nvim.
fn scan_csi_replies(buf: &[u8]) -> (bool, bool, bool, Vec<u8>) {
    let mut sync = false;
    let mut kitty = false;
    let mut da1 = false;
    let mut residue = Vec::new();
    let mut i = 0;
    while i < buf.len() {
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
    (sync, kitty, da1, residue)
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
/// real input thread [`spawn_input_thread`](crate::terminal::spawn_input_thread)
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
        // where crossterm's own direct-fd reader can never see it once the
        // probe hands off to `spawn_input_thread`. A raw read leaves every
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
/// Returns the resolved capabilities alongside any probe residue (always
/// empty when overridden, since the override path never touches stdin);
/// see [`detect`] for what residue means and why it exists.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the probe's I/O fails.
pub fn resolve(tier_override: Option<Tier>) -> io::Result<(TermCaps, Vec<u8>)> {
    let (caps, residue, source) = match tier_override {
        Some(tier) => (caps_for_override(tier), Vec::new(), "--tier override"),
        None => {
            let (caps, residue) = probe_real_terminal()?;
            (caps, residue, PROBE_SOURCE_LABEL)
        }
    };
    log_caps(&caps, source);
    Ok((caps, residue))
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

#[cfg(unix)]
fn probe_real_terminal() -> io::Result<(TermCaps, Vec<u8>)> {
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
    let mut source = match StdinReplySource::new() {
        Ok(source) => source,
        Err(_) => return Ok((no_probe_caps(colorterm.as_deref()), Vec::new())),
    };
    detect(
        &mut source,
        &mut io::stdout(),
        PROBE_DEADLINE,
        colorterm.as_deref(),
    )
}

#[cfg(not(unix))]
fn probe_real_terminal() -> io::Result<(TermCaps, Vec<u8>)> {
    // Bounding a raw stdin read without a background thread that could
    // outlive the probe needs a termios VMIN/VTIME equivalent this crate
    // only has for unix; COLORTERM is still honored since that's a plain
    // env read, not an escape probe. No stdin read happens here at all, so
    // there is no residue to return either.
    let colorterm = std::env::var("COLORTERM").ok();
    Ok((no_probe_caps(colorterm.as_deref()), Vec::new()))
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
        assert_eq!(sink, b"\x1b[?2026$p\x1b[?u\x1b[c");
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
            start.elapsed() < Duration::from_millis(500),
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
            start.elapsed() < Duration::from_millis(200),
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
        // one of our own DA1/DECRPM/kitty replies
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[A\x1b[?62c".as_slice())]);
        let mut sink = Vec::new();
        let (_caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert_eq!(residue, b"\x1b[A");
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
