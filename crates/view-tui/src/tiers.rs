//! Terminal capability detection: sends a batched capability probe right
//! after raw mode is entered (canonical-mode line buffering and echo would
//! otherwise corrupt or swallow the escape replies) and before the
//! alternate screen takes over, so `Term::init` can wire real gating
//! booleans into `Model.caps` instead of the conservative defaults.
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
/// # Errors
///
/// Returns the underlying `std::io::Error` if the query batch can't be
/// written.
pub fn detect<S: ReplySource>(
    source: &mut S,
    writer: &mut impl Write,
    deadline: Duration,
    colorterm: Option<&str>,
) -> io::Result<TermCaps> {
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

    let (sync, kitty_kbd, _) = scan_csi_replies(&buf);
    let truecolor = truecolor_from_colorterm(colorterm);
    Ok(TermCaps::from_probe(sync, truecolor, kitty_kbd))
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
/// `(sync_supported, kitty_supported, da1_seen)`.
///
/// Only private-mode CSI sequences (`ESC [ ?` ...) are inspected: every
/// reply this probe cares about (DECRPM, kitty flags, DA1) is one of
/// those, so anything else in the buffer is skipped rather than misread.
fn scan_csi_replies(buf: &[u8]) -> (bool, bool, bool) {
    let mut sync = false;
    let mut kitty = false;
    let mut da1 = false;
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] != 0x1b || buf[i + 1] != b'[' || buf[i + 2] != b'?' {
            i += 1;
            continue;
        }
        let params_start = i + 3;
        let mut j = params_start;
        while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
            j += 1;
        }
        if j >= buf.len() {
            // an in-flight sequence with no final byte yet: nothing more to
            // find until the next chunk arrives
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
    (sync, kitty, da1)
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
        use std::io::Read;
        let mut buf = [0_u8; 256];
        match io::stdin().lock().read(&mut buf) {
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
/// # Errors
///
/// Returns the underlying `std::io::Error` if the probe's I/O fails.
pub fn resolve(tier_override: Option<Tier>) -> io::Result<TermCaps> {
    let overridden = tier_override.is_some();
    let caps = match tier_override {
        Some(tier) => caps_for_override(tier),
        None => probe_real_terminal()?,
    };
    log_caps(&caps, overridden);
    Ok(caps)
}

#[cfg(unix)]
fn probe_real_terminal() -> io::Result<TermCaps> {
    let mut source = StdinReplySource::new()?;
    let colorterm = std::env::var("COLORTERM").ok();
    detect(
        &mut source,
        &mut io::stdout(),
        PROBE_DEADLINE,
        colorterm.as_deref(),
    )
}

#[cfg(not(unix))]
fn probe_real_terminal() -> io::Result<TermCaps> {
    // Bounding a raw stdin read without a background thread that could
    // outlive the probe needs a termios VMIN/VTIME equivalent this crate
    // only has for unix; COLORTERM is still honored since that's a plain
    // env read, not an escape probe.
    let colorterm = std::env::var("COLORTERM").ok();
    Ok(TermCaps::from_probe(
        false,
        truecolor_from_colorterm(colorterm.as_deref()),
        false,
    ))
}

fn log_caps(caps: &TermCaps, overridden: bool) {
    let source = if overridden {
        "--tier override"
    } else {
        "probed"
    };
    eprintln!(
        "view: terminal capabilities: tier={:?} sync={} truecolor={} kitty_kbd={} ({source})",
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
            let caps = detect(&mut source, &mut sink, PROBE_DEADLINE, case.colorterm)
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
        let caps = detect(&mut source, &mut sink, PROBE_DEADLINE, Some("truecolor")).unwrap();
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
        let caps = detect(&mut source, &mut sink, PROBE_DEADLINE, None).unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "detect blocked for {:?} against a source that reported itself \
             exhausted on the first call",
            start.elapsed()
        );
        assert!(!caps.sync && !caps.truecolor && !caps.kitty_kbd);
        assert_eq!(tier_name(caps.tier), "basic");
    }

    #[test]
    fn deadline_bounds_the_loop_even_against_an_always_empty_source() {
        let mut source = ScriptedSource::new(vec![Some(b"".as_slice()); 100_000]);
        let short_deadline = Duration::from_millis(5);
        let mut sink = Vec::new();
        let start = Instant::now();
        let caps = detect(&mut source, &mut sink, short_deadline, None).unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "detect took {:?} against a source that never stops offering \
             empty chunks",
            start.elapsed()
        );
        assert!(!caps.sync && !caps.truecolor && !caps.kitty_kbd);
    }
}
