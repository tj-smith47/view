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
/// v0.12.4 sends `ESC P $ q m ESC \` twice per startup, first behind
/// `ESC [ 4:3 m` (a curly underline) in its opening batch, and again behind
/// `ESC [ 48;2;1;2;3 m` -- these exact bytes -- after its XTGETTCAP round.
/// The question below is the second of those, moved into the opening
/// batch.
///
/// No cell is painted by this: the set and the reset are adjacent, with no
/// text between them, and both run before the alternate screen exists.
const QUERY_TRUECOLOR: &[u8] = b"\x1b[48;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m";

/// Writes one rounded box-drawing corner from a known column and asks the
/// terminal where the cursor ended up (a CPR, `ESC [ 6 n`), then erases the
/// line it borrowed.
///
/// This asks about cell accounting, which is the question the border
/// charset needs answered and the one no environment variable carries: a
/// terminal decoding UTF-8 advances one column for the three bytes of `╭`,
/// and one that is not advances further, having taken them for something
/// other than one box-drawing cell. How much further is the terminal's
/// business and is not predicted here: the only capture of a non-UTF-8
/// terminal reports column 3, not the 4 that drawing three characters
/// would give. Live captures of both are in
/// `docs/terminal-probe-wire-capture.md` sections D and E, which differ in
/// nothing else -- `TERM`, `COLORTERM` and every other reply are identical
/// between them.
///
/// It is not a question about the font. A terminal whose font lacks the
/// glyph advances one column and renders tofu, and no capture separates
/// that from a legible frame; [`TermCaps::unicode_boxes`] says what the
/// answer is owed to mean.
///
/// The leading `\r` is what makes column 2 the expected answer whatever
/// else has been printed on the line, and the trailing `\r ESC [ K` puts
/// the cursor back and clears from there to the end of the line -- the
/// glyph, and anything else that line was carrying. All of it runs before
/// the alternate screen exists, on a line the startup path has not yet
/// written to, so what the erase can reach is the shell prompt the user
/// launched from rather than anything this program painted.
const QUERY_BOX_GLYPH: &[u8] = "\r╭\x1b[6n\r\x1b[K".as_bytes();

/// The cursor column a terminal that advanced [`QUERY_BOX_GLYPH`]'s glyph
/// by exactly one cell reports, the `\r` in front of the glyph having put
/// it at column 1.
const BOX_GLYPH_ONE_CELL_COLUMN: u32 = 2;

/// The columns [`QUERY_BOX_GLYPH`] has been answered with, and therefore the
/// only ones a CPR is read as its answer at all.
///
/// Two, across every terminal captured: 2 where the glyph took one cell and
/// 3 where its three bytes took three, and "no capture here reports a column
/// past 3". The bound matters because a modified `F3` wears the same
/// grammar -- `Ctrl-F3` under tmux is `\x1b[1;5R` -- so a column no glyph
/// can produce is a keypress, and reading it as an answer would spend the
/// probe's one question on it and leave the terminal's real reply to be
/// typed into the buffer.
const BOX_GLYPH_ANSWERED_COLUMNS: std::ops::RangeInclusive<u32> = 2..=3;

const QUERY_DA1_FENCE: &[u8] = b"\x1b[c";

/// The SGR parameters [`QUERY_TRUECOLOR`] sets, as the DECRQSS reply must
/// echo them back for the terminal to count as truecolor: a 24-bit
/// background introducer (`48;2`) followed by the three components in
/// order.
const TRUECOLOR_SGR_PARAMS: [&str; 5] = ["48", "2", "1", "2", "3"];

/// What decides a capability whose probe went unanswered.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub enum Fallback {
    /// Nothing outside the terminal carries this fact, so an unanswered
    /// question leaves the capability off. The floor, not a guess: a
    /// terminal that did not say it can do this is driven as though it
    /// cannot.
    Unanswered,
    /// The named environment variable decides.
    EnvHint {
        /// The variable read, spelled as the environment spells it, for a
        /// listing that has to tell a user which one to look at.
        var: &'static str,
        /// How its value is read -- the same function the resolution path
        /// calls, so a row cannot advertise a reading the build does not
        /// perform.
        read: fn(&EnvHints) -> bool,
    },
}

impl Fallback {
    /// What this fallback resolves to under `hints`.
    #[must_use]
    pub fn resolve(&self, hints: &EnvHints) -> bool {
        match self {
            Self::EnvHint { read, .. } => read(hints),
            Self::Unanswered => false,
        }
    }
}

/// One capability view consumes: the probe that carries the fact, and what
/// decides the bit when that probe stays silent.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub struct CapabilityRow {
    /// The [`TermCaps`] field this row governs, spelled exactly as the
    /// field is: the drift check matches the two by name.
    pub capability: &'static str,
    /// The bytes that ask the terminal, which is the whole of what
    /// "authoritative probe" means here -- the same constant
    /// [`Probe::start`] writes, so a row cannot cite a question this build
    /// does not put on the wire.
    pub query: &'static [u8],
    /// Reads this capability out of resolved capabilities, so a listing
    /// walks the register instead of restating the field list beside it.
    pub read: fn(&TermCaps) -> bool,
    /// What decides the bit when [`Self::query`] goes unanswered.
    pub fallback: Fallback,
}

/// Synchronized output, from the DECRQM answer to [`QUERY_SYNC`]: a
/// terminal reporting mode 2026 as set or reset (`Pm` 1 or 2) has it, one
/// reporting it unrecognized does not. Captured in
/// `docs/terminal-probe-wire-capture.md`, "A. kitty 0.45.0, dev-linux" and
/// "What a terminal that does not support this answers".
const SYNC: CapabilityRow = CapabilityRow {
    capability: "sync",
    query: QUERY_SYNC,
    read: |caps| caps.sync,
    fallback: Fallback::Unanswered,
};

/// 24-bit color, from the DECRQSS readback of [`QUERY_TRUECOLOR`]: the
/// terminal echoes the SGR state it kept, so a quantized request answers
/// negatively in its own words. Captured in
/// `docs/terminal-probe-wire-capture.md`, sections A, F, G and H -- F and G
/// being the pair that disqualifies `COLORTERM` as the oracle, a truecolor
/// terminal reached over ssh with the variable unset and a tmux answering
/// negatively with it set.
const TRUECOLOR: CapabilityRow = CapabilityRow {
    capability: "truecolor",
    query: QUERY_TRUECOLOR,
    read: |caps| caps.truecolor,
    fallback: Fallback::EnvHint {
        var: "COLORTERM",
        read: |hints| truecolor_hint(hints.colorterm.as_deref()),
    },
};

/// The kitty keyboard protocol, from the progressive-enhancement answer to
/// [`QUERY_KITTY`]. Captured in `docs/terminal-probe-wire-capture.md`,
/// "A. kitty 0.45.0, dev-linux".
const KITTY_KBD: CapabilityRow = CapabilityRow {
    capability: "kitty_kbd",
    query: QUERY_KITTY,
    read: |caps| caps.kitty_kbd,
    fallback: Fallback::Unanswered,
};

/// Box-drawing cell accounting, from the cursor column reported after
/// [`QUERY_BOX_GLYPH`] writes one `╭`: one column advanced means one cell.
/// Captured in `docs/terminal-probe-wire-capture.md`, sections D and E,
/// whose limits "What D and E prove, and what they do not" states -- this
/// is the terminal's accounting, never the font's coverage.
const UNICODE_BOXES: CapabilityRow = CapabilityRow {
    capability: "unicode_boxes",
    query: QUERY_BOX_GLYPH,
    read: |caps| caps.unicode_boxes,
    fallback: Fallback::EnvHint {
        var: "LC_ALL/LC_CTYPE/LANG",
        read: |hints| unicode_boxes_hint(hints.locale.as_deref()),
    },
};

/// Every capability view consumes, as data rather than as a `match`, so the
/// set is enumerable: the drift check that every probed [`TermCaps`] field
/// still has a row has something to walk, and the resolution path and the
/// user-facing listing read the same four rows rather than each keeping a
/// list of their own.
///
/// A capability with no row here does not exist, which is what makes one
/// more capability inferred from the environment somewhere off to the side
/// unrepresentable rather than merely absent. `TermCaps::tier` has no row
/// and is not a capability: it is derived from these by
/// [`TermCaps::from_probe`] and is never probed.
///
/// Ordered as the spec's register table lists them, which is also the order
/// every listing of these bits has printed since before this table existed.
static REGISTER: [CapabilityRow; 4] = [SYNC, TRUECOLOR, KITTY_KBD, UNICODE_BOXES];

/// Every capability row -- see [`REGISTER`].
#[must_use]
pub fn register() -> &'static [CapabilityRow] {
    &REGISTER
}

/// The register resolved against `caps`, as one `name=value` run in
/// register order.
///
/// The one rendering of these bits, shared by every listing a user or a log
/// reader sees, so a capability gained or renamed cannot leave one listing
/// saying something another does not.
#[must_use]
pub fn resolved(caps: &TermCaps) -> String {
    REGISTER
        .iter()
        .map(|row| format!("{}={}", row.capability, (row.read)(caps)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the environment claims about capabilities the terminal can be
/// asked about directly.
///
/// Hints, never oracles. Each field is read only where the probe stayed
/// silent, because an environment variable describes the emulator someone
/// configured rather than the one on the far end of this fd: `COLORTERM` is
/// unset in every ssh login whose client does not forward it, and set to
/// `truecolor` inside a tmux that answers the readback negatively (captures
/// F and G). A terminal that answered is the authority on itself.
///
/// Carried as one value rather than as parameters so that a probe reading
/// two of them cannot be handed them in the wrong order.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct EnvHints {
    /// `COLORTERM`, the truecolor hint.
    pub colorterm: Option<String>,
    /// The locale that decides the terminal's character encoding, from
    /// `LC_ALL`, `LC_CTYPE` or `LANG` in the precedence POSIX gives them.
    /// The `unicode_boxes` hint.
    pub locale: Option<String>,
}

impl EnvHints {
    /// Reads the hints out of this process's environment.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            colorterm: std::env::var("COLORTERM").ok(),
            locale: ["LC_ALL", "LC_CTYPE", "LANG"]
                .into_iter()
                .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty())),
        }
    }
}

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
///
/// `Default` is the shape of a caller that ran no probe at all: nothing
/// answered, nothing read, and every question still open.
#[derive(Default)]
pub struct ProbeOutcome {
    /// The capabilities every reply that arrived resolves to.
    pub caps: TermCaps,
    /// Every byte the probe read that was not part of a recognized reply --
    /// see [`Probe::finish`].
    pub residue: Vec<u8>,
    /// Whether the DA1 fence arrived. False means the terminal is still
    /// owed replies the probe stopped waiting for, and the caller must keep
    /// them off the input path (see
    /// [`InputSource::open_after_probe`](crate::input::InputSource::open_after_probe)).
    pub fence_seen: bool,
    /// Whether the box-glyph question has been answered, and so must not be
    /// asked of anything that arrives later.
    ///
    /// The probe's own bound on a CPR -- one match, and every later one is a
    /// keypress -- can only hold within a single scan, because the buffer it
    /// scans ends here. The fact travels on so that the guard reading the
    /// rest of the replies inherits it rather than starting over: `\x1b[1;2R`
    /// is both a cursor at row 1 column 2 and tmux's `Shift-F3`, and a
    /// question already answered is what makes those six bytes the user's.
    pub cpr_seen: bool,
    /// The buffer tail the scan stopped on: a run that is still a live
    /// prefix of some answer grammar, so it is neither resolved nor
    /// residue.
    ///
    /// Whose bytes those are is not decided here and cannot be. `ESC [ ?`
    /// is the head of a DECRPM answer and is also what a user typing
    /// `Escape`, `[`, `?` produces, so this carries a half-arrived answer
    /// and a half-typed keypress under the same shape. Both want the same
    /// treatment -- wait for the read that finishes the run -- which is why
    /// the field is one field.
    ///
    /// The settle takes no wait at all, which puts this seam at the first
    /// probe window rather than at the hard cap -- exactly where a reply a
    /// network round trip away is mid-flight. Handed to
    /// [`InputSource::open_after_probe`](crate::input::InputSource::open_after_probe),
    /// which seeds the guard with it so the read that completes an answer
    /// completes it rather than scanning a tail with no head. What is never
    /// completed is separated there, at the guard's expiry, by
    /// `is_terminal_only_remainder`: what could still have been a keypress
    /// goes to the key decoder, what is provably the terminal's own is
    /// dropped rather than typed into the buffer.
    pub partial_reply: Vec<u8>,
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
    hints: EnvHints,
}

impl<'a> Probe<'a> {
    /// Writes the query batch to `writer` and reads replies for at most
    /// `first_window`, stopping early the moment the DA1 fence arrives.
    ///
    /// `hints` is what the environment claims (see [`EnvHints`]), passed in
    /// rather than read here so this stays deterministic and safe to call
    /// from parallel unit tests.
    ///
    /// # Errors
    ///
    /// Returns the underlying `std::io::Error` if the query batch can't be
    /// written.
    pub fn start(
        source: impl ReplySource + 'a,
        writer: &mut impl Write,
        first_window: Duration,
        hints: &EnvHints,
    ) -> io::Result<Self> {
        writer.write_all(QUERY_SYNC)?;
        writer.write_all(QUERY_KITTY)?;
        writer.write_all(QUERY_TRUECOLOR)?;
        // behind the truecolor readback's own `ESC [ 0 m`, so the glyph it
        // prints is drawn in the terminal's default colors rather than the
        // 24-bit background that query just set
        writer.write_all(QUERY_BOX_GLYPH)?;
        // last, and that is the whole point: a terminal answers queries in
        // the order it received them, so this reply arriving proves every
        // earlier one either arrived or is never coming
        writer.write_all(QUERY_DA1_FENCE)?;
        writer.flush()?;

        let mut probe = Self {
            source: Box::new(source),
            buf: Vec::new(),
            started: Instant::now(),
            hints: hints.clone(),
        };
        probe.read_until(first_window);
        Ok(probe)
    }

    /// The capabilities the replies seen so far resolve to.
    #[must_use]
    pub fn caps(&self) -> TermCaps {
        self.caps_from(&scan_replies(&self.buf, true))
    }

    /// Whether the DA1 fence has arrived, i.e. whether the terminal still
    /// owes this probe anything.
    #[must_use]
    pub fn fence_seen(&self) -> bool {
        scan_replies(&self.buf, true).da1
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
        // true, unconditionally: a probe's own buffer is never drained
        // before this point, so every scan it runs sees the whole window
        // from the beginning and the "one match" bound holds inside it
        let replies = scan_replies(&self.buf, true);
        let partial_reply = self.buf.split_off(replies.consumed);
        ProbeOutcome {
            caps: self.caps_from(&replies),
            fence_seen: replies.da1,
            cpr_seen: replies.unicode_boxes.is_some(),
            residue: replies.residue,
            partial_reply,
        }
    }

    /// The capabilities `replies` resolve to, each unanswered question
    /// falling back to its hint and no answered one consulting a hint at
    /// all -- see [`EnvHints`] for why that order and not the other.
    fn caps_from(&self, replies: &Replies) -> TermCaps {
        let truecolor = replies
            .truecolor
            .unwrap_or_else(|| TRUECOLOR.fallback.resolve(&self.hints));
        let unicode_boxes = replies
            .unicode_boxes
            .unwrap_or_else(|| UNICODE_BOXES.fallback.resolve(&self.hints));
        TermCaps::from_probe(replies.sync, truecolor, replies.kitty)
            .with_unicode_boxes(unicode_boxes)
    }

    fn read_until(&mut self, deadline: Duration) {
        loop {
            if scan_replies(&self.buf, true).da1 {
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
    hints: &EnvHints,
) -> io::Result<(TermCaps, Vec<u8>)> {
    let outcome = Probe::start(source, writer, deadline, hints)?.finish(deadline);
    Ok((outcome.caps, outcome.residue))
}

/// Derives capabilities from a `--tier` override: deterministic, not
/// half-probed, so the booleans are picked directly rather than partially
/// trusting a probe. `full` sets every boolean, `standard` sets
/// `truecolor` and `unicode_boxes`, `basic` sets none.
///
/// An override is a claim about the terminal, and both Unicode tiers claim
/// glyphs: a session told it is `standard` is told it can be drawn the way
/// `standard` looks, which is with a rounded frame.
#[must_use]
pub fn caps_for_override(tier: Tier) -> TermCaps {
    match tier {
        Tier::Full => TermCaps::from_probe(true, true, true).with_unicode_boxes(true),
        Tier::Standard => TermCaps::from_probe(false, true, false).with_unicode_boxes(true),
        Tier::Basic => TermCaps::from_probe(false, false, false),
        // `Tier` is `#[non_exhaustive]`; an override the caller passed a
        // future variant for degrades to the same all-false floor as an
        // unanswered probe rather than failing to compile.
        _ => TermCaps::from_probe(false, false, false),
    }
}

/// Reads `COLORTERM` as a truecolor hint: `truecolor` or `24bit` (case
/// sensitive, matching the values terminals actually emit).
///
/// A hint, and consulted only where [`QUERY_TRUECOLOR`]'s readback went
/// unanswered. The terminal is the authority on itself and this variable
/// is not: capture F is a truecolor terminal reached over ssh with
/// `COLORTERM` unset, and capture G is the same machine's tmux answering
/// the readback negatively with `COLORTERM=truecolor` set.
fn truecolor_hint(value: Option<&str>) -> bool {
    matches!(value, Some("truecolor") | Some("24bit"))
}

/// Reads the locale as a box-glyph hint: a charset of UTF-8, which is what
/// [`QUERY_BOX_GLYPH`]'s two captured outcomes turn on.
///
/// A hint, and consulted only where the CPR went unanswered. The locale
/// belongs to the process, not to the emulator drawing for it, so it can
/// disagree with the terminal in either direction; every terminal captured
/// so far answers the CPR, which is what makes this the rarely-taken arm.
fn unicode_boxes_hint(locale: Option<&str>) -> bool {
    locale.is_some_and(|value| {
        let charset = value.rsplit('.').next().unwrap_or_default();
        charset.eq_ignore_ascii_case("UTF-8") || charset.eq_ignore_ascii_case("UTF8")
    })
}

/// Whether a DECRQSS reply body (everything between the `ESC P` introducer
/// and the string terminator) is the terminal reporting that it kept
/// [`QUERY_TRUECOLOR`]'s 24-bit background.
///
/// `None` where the reply answers no such question, which is a different
/// fact from a terminal reporting that it did not keep the color: only the
/// first may fall back to [`truecolor_hint`].
///
/// A valid response opens `1 $ r` and closes with the final byte of the
/// setting it echoes (`m`, for SGR). `0 $ r` is the terminal declining the
/// request as invalid -- an answer about the request, not about the color,
/// and the shape tmux gives whether or not the terminal underneath it
/// renders 24-bit (captures B, C and G), so reading it as "no truecolor"
/// would strip every tmux session of color it demonstrably has.
///
/// The echoed parameters are searched rather than compared whole because a
/// terminal may report its entire SGR state (`0;48;2;1;2;3`), and both `;`
/// and `:` are accepted between the components -- the two separators are
/// interchangeable in the spec and terminals differ over which they echo.
///
/// Two run lengths are accepted, which is the whole of the T.416 question.
/// The legacy spelling puts the components straight behind the introducer
/// (`48;2;1;2;3`); the ITU-T T.416 one carries a colour-space id between
/// them (`48:2:<id>:1:2:3`), and Windows ConPTY sends that field empty
/// (`48:2::1:2:3`, capture H). The id's own value is not this function's
/// business -- what it is asked is whether the terminal kept the triple --
/// so the six-field run accepts anything in that slot, empty included.
///
/// Recall is load-bearing here in a way it was not when this reading was
/// OR'd with `COLORTERM`: an answer now outranks the hint, so a spelling
/// this fails to recognize does not fall back, it reports a truecolor
/// terminal as a 256-color one.
fn truecolor_from_decrqss(body: &[u8]) -> Option<bool> {
    const INTRODUCER: usize = 2;
    let sgr = body
        .strip_prefix(b"1$r")
        .and_then(|rest| rest.strip_suffix(b"m"))?;
    let sgr = std::str::from_utf8(sgr).ok()?;
    let fields: Vec<&str> = sgr.split([';', ':']).collect();
    let legacy = fields
        .windows(TRUECOLOR_SGR_PARAMS.len())
        .any(|window| window == TRUECOLOR_SGR_PARAMS);
    let with_colour_space_id = fields
        .windows(TRUECOLOR_SGR_PARAMS.len() + 1)
        .any(|window| {
            window[..INTRODUCER] == TRUECOLOR_SGR_PARAMS[..INTRODUCER]
                && window[INTRODUCER + 1..] == TRUECOLOR_SGR_PARAMS[INTRODUCER..]
        });
    Some(legacy || with_colour_space_id)
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
    /// `COLORTERM`. `None` where it gave none: "did not answer" and
    /// "answered no" are different facts, and only the first defers to a
    /// hint (see [`EnvHints`]).
    truecolor: Option<bool>,
    /// The terminal's own answer about box-glyph cell accounting, `None`
    /// where it gave none, on the same terms as `truecolor`.
    pub(crate) unicode_boxes: Option<bool>,
    pub(crate) residue: Vec<u8>,
    /// How many leading bytes of the buffer this scan accounted for.
    /// Everything past it is a reply the terminal has only half delivered,
    /// which the next chunk completes.
    pub(crate) consumed: usize,
}

impl Replies {
    /// `known` plus everything this scan proves on top of it.
    ///
    /// Only ever adds: a capability the terminal already answered for is
    /// not withdrawn by a later read that says nothing about it, and
    /// `known` carries the hints no reply restates (see [`EnvHints`]). A
    /// late answer of "no" therefore cannot demote a session mid-flight --
    /// what it can do is arrive as an upgrade, which is the whole reason
    /// the probe may hand the terminal over before the fence.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn upgraded(&self, known: TermCaps) -> TermCaps {
        TermCaps::from_probe(
            known.sync || self.sync,
            known.truecolor || self.truecolor == Some(true),
            known.kitty_kbd || self.kitty,
        )
        .with_unicode_boxes(known.unicode_boxes || self.unicode_boxes == Some(true))
    }
}

/// Whether what [`scan_replies`] left unaccounted for can only be the
/// terminal's own half-delivered answer.
///
/// True for a run that is still a live prefix of an answer grammar and that
/// no keyboard can produce: `ESC [ ?` and the five bytes the batch's DCS
/// answer opens with. A bare `ESC [` is equally the opening of every arrow
/// and function key, so that one reports false even though the scan is
/// still waiting on it.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn is_terminal_only_remainder(buf: &[u8]) -> bool {
    matches!(
        answer_head(buf, true),
        AnswerHead::Partial {
            keyboard_possible: false
        }
    )
}

/// What one of the probe's answers resolves to.
enum AnswerKind {
    /// A DECRPM reply, and whether it reports synchronized output usable.
    Decrpm(bool),
    Kitty,
    Da1,
    /// A DECRQSS readback, and what it says about the 24-bit background --
    /// `None` where it declined the request rather than answering it.
    Decrqss(Option<bool>),
    /// A cursor-position report, and whether the column it names is the one
    /// a box glyph occupying a single cell leaves the cursor in.
    Cpr(bool),
}

/// How the head of a byte run stands against the probe's answer grammars.
enum AnswerHead {
    /// The run's first `len` bytes are one whole answer.
    Complete { len: usize, kind: AnswerKind },
    /// Every byte so far is a live prefix of at least one grammar and the
    /// rest of the answer has not arrived. `keyboard_possible` marks the
    /// prefixes a keypress can also produce (`ESC [`), which must not be
    /// discarded as the terminal's when the guard runs out of time.
    Partial {
        #[cfg_attr(not(unix), allow(dead_code))]
        keyboard_possible: bool,
    },
    /// The run leaves every grammar. `terminal_prefix` counts the leading
    /// bytes that were provably the terminal's before it did -- a stalled
    /// private-mode answer with a keypress on the end of it -- so the byte
    /// at that offset is the user's and everything before it is not.
    NotAnAnswer { terminal_prefix: usize },
}

/// Matches the head of `buf` against the five shapes the probe's batch can
/// be answered with, and only those:
///
/// | Answer | Grammar |
/// |---|---|
/// | DECRPM (`QUERY_SYNC`) | `ESC [ ? Ps ; Pm $ y`, `Ps` digits, `Pm` 0..=4 |
/// | kitty flags (`QUERY_KITTY`) | `ESC [ ? flags u`, `flags` a five-bit field |
/// | DA1 fence (`QUERY_DA1_FENCE`) | `ESC [ ? class ; ... c`, digits and `;`, `class` <= 65 |
/// | DECRQSS (`QUERY_TRUECOLOR`) | `ESC P 0/1 $ r ... ESC \` (or `BEL`) |
/// | CPR (`QUERY_BOX_GLYPH`) | `ESC [ row ; col R`, and only while `cpr_wanted` |
///
/// One acceptor for all five, because every rule that guessed from a
/// shorter prefix has been wrong about some keypress: `ESC P` is
/// Alt+Shift+P, a `c` behind a stalled `?2026` is a change operator rather
/// than a DA1 fence, and a `u` behind the same stall is not a kitty
/// terminal. A byte that leaves every grammar ends the answer, whatever
/// that byte is.
///
/// The CPR is the one answer whose bytes a keyboard can also produce:
/// `ESC [ 1 ; 2 R` is a CPR from a cursor on row 1 column 2 and is
/// byte-identical to tmux's `Shift-F3`, captured side by side in
/// `docs/terminal-probe-wire-capture.md`, "A keypress that is a CPR reply".
/// Nothing in the byte stream separates them, so two things outside the
/// stream do. `cpr_wanted` is true only inside a probe window that asked
/// the question and only until it has been answered once -- a fact that
/// travels from the probe to the late-reply guard on
/// [`ProbeOutcome::cpr_seen`], so the bound is one answer per session and
/// not one per read. And the column has to be one the glyph could have
/// produced ([`BOX_GLYPH_ANSWERED_COLUMNS`]), which is what keeps
/// `Ctrl-F3` (`\x1b[1;5R`) a keypress.
///
/// What is left is exact: a `Shift-F3` (column 2) or an `Alt-F3`
/// (column 3) pressed under tmux before the terminal's own CPR arrives, in
/// the first moments of a launch. That keypress is lost and answers the
/// question in the terminal's place. kitty does not encode modified `F3`
/// in this grammar at all (`\x1b[13;2~`), so the collision is a property
/// of the terminal's key encoding rather than of CPR.
fn answer_head(buf: &[u8], cpr_wanted: bool) -> AnswerHead {
    match buf {
        [0x1b, b'[', b'?', params @ ..] => private_csi_head(params),
        // `ESC [` alone is where an answer and an arrow key are the same
        // two bytes; the read after it decides which
        [0x1b, b'[', params @ ..] => cpr_head(params, cpr_wanted),
        [0x1b, b'P', b'0' | b'1', b'$', b'r', ..] => match dcs_body(&buf[2..]) {
            Some((body, past)) => AnswerHead::Complete {
                len: 2 + past,
                kind: AnswerKind::Decrqss(truecolor_from_decrqss(body)),
            },
            None => AnswerHead::Partial {
                keyboard_possible: false,
            },
        },
        _ => AnswerHead::NotAnAnswer { terminal_prefix: 0 },
    }
}

/// The CPR grammar, from the byte after the `ESC [` introducer that every
/// arrow and modified function key shares with it.
///
/// Nothing here is ever claimed as provably the terminal's: `terminal_prefix`
/// stays 0 on every exit, because a run that reaches this function could be
/// a keypress in a way that one behind `ESC [ ?` could not.
fn cpr_head(params: &[u8], cpr_wanted: bool) -> AnswerHead {
    const INTRODUCER: usize = 2;
    for (at, &byte) in params.iter().enumerate() {
        if (0x40..=0x7e).contains(&byte) {
            if cpr_wanted && byte == b'R' {
                if let Some(column) =
                    cpr_column(&params[..at]).filter(|c| BOX_GLYPH_ANSWERED_COLUMNS.contains(c))
                {
                    return AnswerHead::Complete {
                        len: INTRODUCER + at + 1,
                        kind: AnswerKind::Cpr(column == BOX_GLYPH_ONE_CELL_COLUMN),
                    };
                }
            }
            return AnswerHead::NotAnAnswer { terminal_prefix: 0 };
        }
        if !matches!(byte, b'0'..=b'9' | b';') {
            return AnswerHead::NotAnAnswer { terminal_prefix: 0 };
        }
    }
    AnswerHead::Partial {
        keyboard_possible: true,
    }
}

/// The column a CPR reply's parameters (`row ; col`, between the introducer
/// and the final `R`) report, which is the whole of what the box-glyph
/// probe reads: the row is where the cursor happened to be sitting when the
/// batch went out (row 1 in most captures, row 7 in the Windows one) and
/// says nothing about the glyph.
fn cpr_column(params: &[u8]) -> Option<u32> {
    let sep = params.iter().position(|&b| b == b';')?;
    parse_ascii_u32(&params[..sep])?;
    parse_ascii_u32(&params[sep + 1..])
}

/// The three private-mode grammars, from the byte after their shared
/// `ESC [ ?` introducer. `params` is what has arrived of the parameter
/// bytes, final byte included once it is there.
fn private_csi_head(params: &[u8]) -> AnswerHead {
    const INTRODUCER: usize = 3;
    for (at, &byte) in params.iter().enumerate() {
        if (0x40..=0x7e).contains(&byte) {
            let seen = &params[..at];
            let kind = match byte {
                b'y' if is_decrpm_params(seen) => AnswerKind::Decrpm(is_sync_supported(seen)),
                b'u' if is_kitty_flags(seen) => AnswerKind::Kitty,
                b'c' if is_da1_class(seen) => AnswerKind::Da1,
                _ => {
                    return AnswerHead::NotAnAnswer {
                        terminal_prefix: INTRODUCER + at,
                    }
                }
            };
            return AnswerHead::Complete {
                len: INTRODUCER + at + 1,
                kind,
            };
        }
        if !matches!(byte, b'0'..=b'9' | b';' | b'$') {
            return AnswerHead::NotAnAnswer {
                terminal_prefix: INTRODUCER + at,
            };
        }
    }
    AnswerHead::Partial {
        keyboard_possible: false,
    }
}

/// A DECRPM reply's parameters: `Ps ; Pm $`, the mode number and the state
/// the terminal reports for it. The state is one of the five the grammar
/// defines (0 not recognized, 1 set, 2 reset, 3/4 permanently so), and the
/// `$` is mandatory -- which is what tells the answer from a stalled reply
/// with a `y` typed onto the end of it.
fn is_decrpm_params(params: &[u8]) -> bool {
    let Some(core) = params.strip_suffix(b"$") else {
        return false;
    };
    let Some(sep) = core.iter().position(|&b| b == b';') else {
        return false;
    };
    parse_ascii_u32(&core[..sep]).is_some()
        && matches!(parse_ascii_u32(&core[sep + 1..]), Some(0..=4))
}

/// The kitty keyboard protocol reports its progressive-enhancement flags as
/// a five-bit field, so anything above 31 is not that answer.
fn is_kitty_flags(params: &[u8]) -> bool {
    matches!(parse_ascii_u32(params), Some(0..=31))
}

/// A DA1 reply's first parameter is the device class -- 1, 6, 12 and 62..65
/// in practice. The bound is deliberately loose (any class a VT-anything
/// has claimed) and the shape is exact: no `$`, because that byte belongs
/// to DECRPM alone.
fn is_da1_class(params: &[u8]) -> bool {
    if params.contains(&b'$') {
        return false;
    }
    let first = params.split(|&b| b == b';').next().unwrap_or_default();
    matches!(parse_ascii_u32(first), Some(0..=65))
}

/// Scans `buf` for every reply detection cares about.
///
/// Each position is matched by [`answer_head`]: a whole answer is consumed
/// silently, a live prefix ends the scan (those bytes are the terminal's
/// and belong to the read that completes them), and anything else is the
/// user's and goes to `residue` byte by byte. Nothing on this fd but the
/// terminal itself answers the probe's own query batch, so a byte outside
/// every grammar came from somewhere else -- almost always keystrokes
/// queued before or during the probe window -- and must survive to be
/// forwarded rather than vanish.
///
/// A stalled answer with a keypress on the end of it (`ESC [ ? 2026` still
/// in the buffer when the user presses `c`) is split rather than taken
/// whole: the run in front is the terminal's and goes, the byte that ended
/// it is the user's and stays.
pub(crate) fn scan_replies(buf: &[u8], cpr_wanted: bool) -> Replies {
    let mut sync = false;
    let mut kitty = false;
    let mut da1 = false;
    let mut truecolor = None;
    let mut unicode_boxes = None;
    let mut cpr_wanted = cpr_wanted;
    let mut residue = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        match answer_head(&buf[i..], cpr_wanted) {
            AnswerHead::Complete { len, kind } => {
                match kind {
                    AnswerKind::Decrpm(supported) => sync |= supported,
                    AnswerKind::Kitty => kitty = true,
                    AnswerKind::Da1 => da1 = true,
                    // first answer wins, for both: a terminal answers each
                    // query in the batch once, and a second run of the same
                    // shape is something else that looks like it
                    AnswerKind::Decrqss(answer) => truecolor = truecolor.or(answer),
                    AnswerKind::Cpr(one_cell) => {
                        unicode_boxes = Some(one_cell);
                        cpr_wanted = false;
                    }
                }
                i += len;
            }
            AnswerHead::Partial { .. } => break,
            AnswerHead::NotAnAnswer { terminal_prefix } => {
                i += terminal_prefix;
                residue.push(buf[i]);
                i += 1;
            }
        }
    }
    Replies {
        sync,
        kitty,
        da1,
        truecolor,
        unicode_boxes,
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

/// Where a session's capabilities came from, carried alongside them so the
/// caller can say so in its own diagnostics.
///
/// A label, never a capability: what a terminal can do is [`TermCaps`], and
/// two sessions with identical capabilities can still have reached them
/// different ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CapsSource {
    /// A real probe was written to the terminal and whatever answered it
    /// was read back. Includes the launch shape whose `tcgetattr` fails on
    /// a non-tty stdin: that is "probed, got nothing", the same all-false
    /// floor an unresponsive real terminal produces.
    Probed,
    /// No escape probe was attempted at all -- see [`probe_real_terminal`]'s
    /// `cfg(not(unix))` arm -- so calling the result probed would claim a
    /// negotiation that never happened.
    Assumed,
    /// `--tier` decided them outright and no terminal I/O ran.
    Override,
}

impl CapsSource {
    /// The one-word answer to "where did these capabilities come from",
    /// for a diagnostic line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Probed => "probed",
            Self::Assumed => "assumed",
            Self::Override => "--tier override",
        }
    }
}

/// What a non-overridden result is: probed on unix, where a real probe
/// always at least attempts terminal I/O; assumed everywhere else, where no
/// terminal I/O is attempted at all.
#[cfg(unix)]
const PROBE_SOURCE: CapsSource = CapsSource::Probed;
#[cfg(not(unix))]
const PROBE_SOURCE: CapsSource = CapsSource::Assumed;

/// Runs the full startup capability resolution: a `--tier` override wins
/// outright and skips all terminal I/O; otherwise the real probe runs
/// against stdin/stdout.
///
/// Writes nothing anywhere: a line about capabilities is a diagnostic, and
/// this crate owns no diagnostic channel. What the caller needs to write
/// one is returned instead -- the capabilities, and the [`CapsSource`] that
/// says how they were reached. Whether the answer is final is a separate
/// question the caller already has the answer to, since the probe returned
/// here is what settles it: `fence_seen` false means the terminal still
/// owes replies, and the `caps tier=` line a later
/// [`Term::settle_probe`](crate::terminal::Term::settle_probe) produces
/// supersedes the first.
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
pub fn resolve(
    tier_override: Option<Tier>,
) -> io::Result<(TermCaps, Option<Probe<'static>>, CapsSource)> {
    if let Some(tier) = tier_override {
        return Ok((caps_for_override(tier), None, CapsSource::Override));
    }
    let (caps, probe) = probe_real_terminal()?;
    Ok((caps, probe, PROBE_SOURCE))
}

#[cfg(unix)]
fn probe_real_terminal() -> io::Result<(TermCaps, Option<Probe<'static>>)> {
    let hints = EnvHints::from_env();
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
        Err(_) => return Ok((no_probe_caps(&hints), None)),
    };
    let probe = Probe::start(source, &mut io::stdout(), PROBE_DEADLINE, &hints)?;
    Ok((probe.caps(), Some(probe)))
}

#[cfg(not(unix))]
fn probe_real_terminal() -> io::Result<(TermCaps, Option<Probe<'static>>)> {
    // Bounding a raw stdin read without a background thread that could
    // outlive the probe needs a termios VMIN/VTIME equivalent this crate
    // only has for unix; the environment hints are still honored since
    // those are plain env reads, not escape probes. No stdin read happens
    // here at all, so there is no residue to return either.
    Ok((no_probe_caps(&EnvHints::from_env()), None))
}

/// The capabilities floor for a launch shape where no escape-sequence probe
/// can run at all: sync and kitty_kbd are always false (both need a real
/// reply), but truecolor and unicode_boxes still honor their hints --
/// stdout can be a real truecolor tty independent of whether stdin is
/// probeable, since the two are independent fds. A probe that was never
/// written is the purest case of the question going unanswered, which is
/// exactly when a hint decides. Shared by both `probe_real_terminal` arms:
/// the unix arm's `tcgetattr`-failure fallback (non-tty stdin, e.g.
/// `/dev/null`) and the `cfg(not(unix))` arm (no termios equivalent to
/// bound a raw read with at all).
fn no_probe_caps(hints: &EnvHints) -> TermCaps {
    TermCaps::from_probe(false, TRUECOLOR.fallback.resolve(hints), false)
        .with_unicode_boxes(UNICODE_BOXES.fallback.resolve(hints))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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

    /// The hints a session whose emulator set `COLORTERM` starts with, and
    /// nothing else.
    fn colorterm(value: &str) -> EnvHints {
        EnvHints {
            colorterm: Some(value.to_owned()),
            locale: None,
        }
    }

    /// The hints a session whose locale names UTF-8 starts with.
    fn utf8_locale() -> EnvHints {
        EnvHints {
            colorterm: None,
            locale: Some("en_US.UTF-8".to_owned()),
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
            let hints = EnvHints {
                colorterm: case.colorterm.map(str::to_owned),
                locale: None,
            };
            let (caps, _residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, &hints)
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
        let (caps, _residue) = detect(
            &mut source,
            &mut sink,
            PROBE_DEADLINE,
            &colorterm("truecolor"),
        )
        .unwrap();
        assert!(caps.sync);
        assert!(caps.kitty_kbd);
        assert!(caps.truecolor);
    }

    /// The batch `docs/terminal-probe-wire-capture.md` records on its
    /// `sent:` line, unescaped out of the document rather than copied into
    /// this file.
    ///
    /// Every `received:` line in that document is some terminal's answer to
    /// exactly these bytes, so a batch that drifts from it is reading
    /// answers to a question nobody asked -- and a copy sitting here would
    /// let the two drift apart silently, which is the whole failure this
    /// reads the doc to prevent.
    fn captured_batch() -> Vec<u8> {
        const DOC: &str = include_str!("../../../docs/terminal-probe-wire-capture.md");
        let line = DOC
            .lines()
            .find_map(|line| line.strip_prefix("sent:"))
            .expect("the capture doc must record the batch it sent")
            .trim();
        let mut bytes = Vec::new();
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                let mut utf8 = [0u8; 4];
                bytes.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
                continue;
            }
            match chars.next() {
                Some('r') => bytes.push(b'\r'),
                Some('\\') => bytes.push(b'\\'),
                Some('x') => {
                    let hex: String = chars.by_ref().take(2).collect();
                    let byte = u8::from_str_radix(&hex, 16)
                        .expect("the doc's `sent:` line must escape bytes as two hex digits");
                    bytes.push(byte);
                }
                other => panic!("unknown escape in the capture doc's `sent:` line: {other:?}"),
            }
        }
        bytes
    }

    #[test]
    fn writes_the_query_batch_before_reading_any_reply() {
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[?62c".as_slice())]);
        let mut sink = Vec::new();
        detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert_eq!(
            sink,
            captured_batch(),
            "the DA1 fence must stay last -- it is what proves every earlier \
             query has been answered -- the truecolor readback must be \
             bracketed by the SGR set and its reset, and the box glyph must \
             be printed behind that reset and erased behind its own CPR"
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

        // an override is a claim about the terminal, and both Unicode tiers
        // claim glyphs; `basic` is the one that claims none
        assert!(full.unicode_boxes && standard.unicode_boxes && !basic.unicode_boxes);
    }

    #[test]
    fn never_replying_fake_yields_all_false_caps_and_never_hangs() {
        let mut source = ScriptedSource::new(vec![None]);
        let mut sink = Vec::new();
        let start = Instant::now();
        let (caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
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
        let (caps, residue) =
            detect(&mut source, &mut sink, short_deadline, &EnvHints::default()).unwrap();
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
        let (caps, residue) = detect(
            &mut source,
            &mut sink,
            PROBE_DEADLINE,
            &colorterm("truecolor"),
        )
        .unwrap();
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
        let (_caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert_eq!(residue, b"\x1b[A");
    }

    #[test]
    fn a_two_byte_csi_prefix_at_end_of_read_is_dropped_not_forwarded() {
        // the same half-delivered reply cut one byte earlier. `ESC [` ending
        // a read is the terminal mid-reply, and forwarding it replays an
        // Escape plus a literal `[` into the engine as if typed
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[".as_slice()), None]);
        let mut sink = Vec::new();
        let (_caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
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
        let (_caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert_eq!(residue, b"\x1b", "a typed Escape must survive the probe");
    }

    #[test]
    fn a_trailing_csi_introducer_is_dropped_rather_than_replayed_as_two_keys() {
        // the other side of that boundary: `ESC [` ending the read is the
        // terminal mid-reply, and `encode_residue_bytes` would turn it into
        // an Escape followed by a literal `[` typed into the buffer
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[".as_slice()), None]);
        let mut sink = Vec::new();
        let (_caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
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
        let (_caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert!(
            residue.is_empty(),
            "residue should be empty, got {residue:?}"
        );
    }

    #[test]
    fn residue_bytes_before_an_incomplete_trailing_reply_still_survive() {
        let mut source = ScriptedSource::new(vec![Some(b"typed\x1b[?62".as_slice()), None]);
        let mut sink = Vec::new();
        let (_caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
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
        let cases: [(&str, &[u8], Option<bool>); 9] = [
            (
                "semicolon separators, whole SGR state echoed",
                b"1$r0;48;2;1;2;3m",
                Some(true),
            ),
            (
                "colon separators (the other form terminals echo)",
                b"1$r48:2:1:2:3m",
                Some(true),
            ),
            (
                "a terminal that quantized the request to 256 colors",
                b"1$r48;5;17m",
                Some(false),
            ),
            (
                // captures B, C and G: tmux declines the request whether or
                // not the terminal under it renders 24-bit color, so this
                // shape must leave the question open for COLORTERM rather
                // than answering it negatively
                "an invalid-request answer carries no setting at all",
                b"0$r",
                None,
            ),
            (
                "the components must be the ones that were set",
                b"1$r48;2;9;9;9m",
                Some(false),
            ),
            (
                "a reply about some other setting is not an SGR answer",
                b"1$r2 q",
                None,
            ),
            ("an empty body decides nothing", b"", None),
            (
                // capture H, the ITU-T T.416 spelling: the colour-space id
                // field is present and empty
                "empty colour-space id (Windows ConPTY)",
                b"1$r0;48:2::1:2:3m",
                Some(true),
            ),
            (
                // the same grammar with the id filled in. Unobserved, but
                // one field away from a form that was: since an answer now
                // outranks the hint, a spelling this parser fails to
                // recognize costs a truecolor terminal its color outright
                "colour-space id present",
                b"1$r0;48:2:0:1:2:3m",
                Some(true),
            ),
        ];
        for (name, body, want) in cases {
            assert_eq!(truecolor_from_decrqss(body), want, "{name}");
        }
    }

    #[test]
    fn the_box_glyph_reply_is_the_probes_own_and_never_typed_input() {
        // docs/terminal-probe-wire-capture.md section E, verbatim
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[1;3R\x1b[?1;2c".as_slice())]);
        let mut sink = Vec::new();
        let (_caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert!(
            residue.is_empty(),
            "the CPR is the box-glyph probe's answer, not a keypress: {residue:?}"
        );
    }

    #[test]
    fn the_terminals_truecolor_answer_reaches_full_without_colorterm() {
        let mut source = ScriptedSource::new(vec![Some(
            b"\x1b[?2026;1$y\x1b[?1u\x1bP1$r0;48;2;1;2;3m\x1b\\\x1b[?62c".as_slice(),
        )]);
        let mut sink = Vec::new();
        let (caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
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
        let (caps, _residue) = detect(
            &mut source,
            &mut sink,
            PROBE_DEADLINE,
            &colorterm("truecolor"),
        )
        .unwrap();
        assert!(caps.truecolor, "COLORTERM keeps its own, unchanged say");
        assert_eq!(tier_name(caps.tier), "full");
    }

    /// Every terminal `docs/terminal-probe-wire-capture.md` was captured
    /// against, as the bytes it put on the wire and the environment the
    /// session that read them had.
    ///
    /// Copied from the capture doc's `received:` lines rather than written
    /// here, section by section, so the detection path is pinned to what
    /// terminals were observed to say and not to what a plan expected them
    /// to say.
    struct Capture {
        section: &'static str,
        replies: &'static [u8],
        hints: fn() -> EnvHints,
        expect_tier: &'static str,
        expect_sync: bool,
        expect_truecolor: bool,
        expect_kitty: bool,
        expect_boxes: bool,
    }

    fn live_captures() -> [Capture; 8] {
        [
            Capture {
                section: "A. kitty 0.45.0, dev-linux",
                replies: b"\x1b[?2026;2$y\x1b[?0u\x1bP1$r0;48:2:1:2:3m\x1b\\\x1b[1;2R\x1b[?62;52;c",
                hints: || colorterm("truecolor"),
                expect_tier: "full",
                expect_sync: true,
                expect_truecolor: true,
                expect_kitty: true,
                expect_boxes: true,
            },
            Capture {
                section: "B. tmux 3.6 inside kitty, dev-linux",
                replies: b"\x1bP0$r\x1b\\\x1b[1;2R\x1b[?1;2;4c",
                hints: || colorterm("truecolor"),
                expect_tier: "standard",
                expect_sync: false,
                // the readback was declined, not answered: COLORTERM is the
                // only signal left and tmux does render 24-bit here
                expect_truecolor: true,
                expect_kitty: false,
                expect_boxes: true,
            },
            Capture {
                section: "C. tmux with terminal-features RGB, dev-linux",
                replies: b"\x1bP0$r\x1b\\\x1b[1;2R\x1b[?1;2;4c",
                hints: || colorterm("truecolor"),
                expect_tier: "standard",
                expect_sync: false,
                expect_truecolor: true,
                expect_kitty: false,
                expect_boxes: true,
            },
            Capture {
                section: "D. GNU screen inside kitty, UTF-8",
                replies: b"\x1b[1;2R\x1b[?1;2c",
                hints: || colorterm("truecolor"),
                expect_tier: "standard",
                expect_sync: false,
                expect_truecolor: true,
                expect_kitty: false,
                expect_boxes: true,
            },
            Capture {
                // the whole discrimination the box-glyph probe exists for:
                // identical to D in TERM, COLORTERM and every other reply
                section: "E. GNU screen, defutf8 off, LANG=C",
                replies: b"\x1b[1;3R\x1b[?1;2c",
                hints: || colorterm("truecolor"),
                expect_tier: "standard",
                expect_sync: false,
                expect_truecolor: true,
                expect_kitty: false,
                expect_boxes: false,
            },
            Capture {
                // the observed defect this whole charter answers: ssh does
                // not forward COLORTERM, and the readback is the only thing
                // on this login that knows the terminal renders 24-bit
                section: "F. mbp over ssh, COLORTERM unset",
                replies: b"\x1b[?2026;2$y\x1b[?0u\x1bP1$r0;48:2:1:2:3m\x1b\\\x1b[1;2R\x1b[?62;52;c",
                hints: EnvHints::default,
                expect_tier: "full",
                expect_sync: true,
                expect_truecolor: true,
                expect_kitty: true,
                expect_boxes: true,
            },
            Capture {
                section: "G. tmux 3.6a on macOS inside F",
                replies: b"\x1bP0$r\x1b\\\x1b[1;2R\x1b[?1;2;4c",
                hints: || colorterm("truecolor"),
                expect_tier: "standard",
                expect_sync: false,
                expect_truecolor: true,
                expect_kitty: false,
                expect_boxes: true,
            },
            Capture {
                section: "H. Windows ConPTY over OpenSSH",
                replies: b"\x1b[?2026;0$y\x1bP1$r0;48:2::1:2:3m\x1b\\\x1b[7;2R\
                           \x1b[?61;6;7;21;22;23;24;28;32;42c",
                hints: EnvHints::default,
                expect_tier: "standard",
                // Pm=0 is an answer, not a silence: the mode is not
                // recognized
                expect_sync: false,
                // the T.416 spelling, and no COLORTERM to recover it with
                expect_truecolor: true,
                expect_kitty: false,
                expect_boxes: true,
            },
        ]
    }

    #[test]
    fn every_live_capture_resolves_to_what_the_doc_reads_it_as() {
        for case in live_captures() {
            let mut source = ScriptedSource::new(vec![Some(case.replies)]);
            let mut sink = Vec::new();
            let (caps, residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, &(case.hints)())
                .expect("detect should not error against an in-memory writer");
            assert_eq!(caps.sync, case.expect_sync, "{}: sync", case.section);
            assert_eq!(
                caps.truecolor, case.expect_truecolor,
                "{}: truecolor",
                case.section
            );
            assert_eq!(
                caps.kitty_kbd, case.expect_kitty,
                "{}: kitty_kbd",
                case.section
            );
            assert_eq!(
                caps.unicode_boxes, case.expect_boxes,
                "{}: unicode_boxes",
                case.section
            );
            assert_eq!(
                tier_name(caps.tier),
                case.expect_tier,
                "{}: tier",
                case.section
            );
            assert!(
                residue.is_empty(),
                "{}: a reply is not typed input: {residue:?}",
                case.section
            );
            assert!(
                scan_replies(case.replies, true).da1,
                "{}: fence",
                case.section
            );
        }
    }

    #[test]
    fn a_ssh_session_without_colorterm_still_probes_truecolor() {
        // capture F, the regression test of record: the same terminal as A,
        // reached from another machine whose ssh client forwards no
        // COLORTERM at all
        let capture = &live_captures()[5];
        let mut source = ScriptedSource::new(vec![Some(capture.replies)]);
        let mut sink = Vec::new();
        let (caps, _residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert!(
            caps.truecolor,
            "the terminal answered that it kept the 24-bit background"
        );
        assert_eq!(tier_name(caps.tier), "full");
    }

    #[test]
    fn a_quantizing_terminal_reports_no_truecolor_despite_colorterm() {
        // the terminal parsed the readback and reported it kept 256 colors:
        // an answer, and one an environment variable may not overrule
        let mut source =
            ScriptedSource::new(vec![Some(b"\x1bP1$r0;48;5;17m\x1b\\\x1b[?62c".as_slice())]);
        let mut sink = Vec::new();
        let (caps, _residue) = detect(
            &mut source,
            &mut sink,
            PROBE_DEADLINE,
            &colorterm("truecolor"),
        )
        .unwrap();
        assert!(!caps.truecolor, "the hint must not override an answer");
        assert_eq!(tier_name(caps.tier), "basic");
    }

    #[test]
    fn a_declined_readback_leaves_the_colorterm_hint_deciding() {
        // captures B, C and G: `0$r` is the terminal declining the request,
        // which says nothing about color -- reading it as "no" would strip
        // every tmux session of the color it demonstrably renders
        let mut source = ScriptedSource::new(vec![Some(b"\x1bP0$r\x1b\\\x1b[?1;2;4c".as_slice())]);
        let mut sink = Vec::new();
        let (caps, _residue) = detect(
            &mut source,
            &mut sink,
            PROBE_DEADLINE,
            &colorterm("truecolor"),
        )
        .unwrap();
        assert!(caps.truecolor);
    }

    #[test]
    fn box_glyph_cpr_of_column_two_reads_as_supported() {
        // capture D
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[1;2R\x1b[?1;2c".as_slice())]);
        let mut sink = Vec::new();
        let (caps, _residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert!(caps.unicode_boxes, "one cell for the glyph");
    }

    #[test]
    fn box_glyph_cpr_of_column_three_reads_as_unsupported() {
        // capture E, and the locale hint on top of it: an answered probe
        // decides, so a session whose LANG says UTF-8 inside a screen that
        // is not decoding it still gets ASCII
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[1;3R\x1b[?1;2c".as_slice())]);
        let mut sink = Vec::new();
        let (caps, _residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &utf8_locale()).unwrap();
        assert!(
            !caps.unicode_boxes,
            "three columns for three bytes is a terminal not decoding UTF-8"
        );
    }

    #[test]
    fn the_locale_hint_decides_only_where_the_cpr_went_unanswered() {
        for (hints, want, name) in [
            (utf8_locale(), true, "a UTF-8 locale"),
            (EnvHints::default(), false, "no locale at all"),
            (
                EnvHints {
                    colorterm: None,
                    locale: Some("C".to_owned()),
                },
                false,
                "the C locale",
            ),
        ] {
            let mut source = ScriptedSource::new(vec![Some(b"\x1b[?62c".as_slice())]);
            let mut sink = Vec::new();
            let (caps, _residue) = detect(&mut source, &mut sink, PROBE_DEADLINE, &hints).unwrap();
            assert_eq!(caps.unicode_boxes, want, "{name}");
        }
    }

    #[test]
    fn a_modified_f3_after_the_cpr_reply_survives_as_residue() {
        // `\x1b[1;2R` is a CPR from row 1 column 2 and is byte-identical to
        // tmux's Shift-F3 (the capture doc's "A keypress that is a CPR
        // reply"). The probe consumes one, and one only: the second is the
        // user's, so the ambiguity costs exactly one sequence rather than
        // every `...R` in the window
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[1;2R\x1b[1;2R\x1b[?62c".as_slice())]);
        let mut sink = Vec::new();
        let (caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert!(caps.unicode_boxes);
        assert_eq!(
            residue, b"\x1b[1;2R",
            "the second one is a keypress and must reach the engine whole"
        );
    }

    #[test]
    fn a_modified_f3_the_glyph_could_not_have_produced_is_not_an_answer() {
        // capture page, "A keypress that is a CPR reply": Ctrl-F3 under tmux
        // is `\x1b[1;5R`. Column 5 is a column no capture reports and the
        // glyph cannot produce, so reading it as an answer would eat the
        // key, pin unicode_boxes for the session on a keypress, and leave
        // the terminal's own reply behind a spent arm -- where it falls out
        // of every grammar and is typed into the buffer
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[1;5R\x1b[1;2R\x1b[?62c".as_slice())]);
        let mut sink = Vec::new();
        let (caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert_eq!(
            residue, b"\x1b[1;5R",
            "the keypress must reach the engine whole"
        );
        assert!(
            caps.unicode_boxes,
            "the terminal's own reply arrived behind it and is the answer"
        );
    }

    #[test]
    fn a_keystroke_typed_during_the_dcs_reply_is_still_returned_as_residue() {
        // the residue contract under the two new arms: a user typing while
        // the terminal is mid-batch loses nothing
        let mut source = ScriptedSource::new(vec![Some(
            b"\x1bP1$r0;48:2:1:2:3m\x1b\\iw\x1b[1;2R\x1b[?62c".as_slice(),
        )]);
        let mut sink = Vec::new();
        let (caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert!(caps.truecolor && caps.unicode_boxes);
        assert_eq!(residue, b"iw");
    }

    #[test]
    fn a_stalled_private_reply_never_fabricates_an_answer_from_a_keypress() {
        // the keys most pressed in normal mode are the ones a private-mode
        // CSI's grammar can end on: a `c` read as the DA1 fence would drop
        // the late-reply guard, a `u` would claim a kitty terminal that
        // never answered, and a `y` would eat a yank
        for key in *b"cuyh:" {
            let mut buf = b"\x1b[?2026".to_vec();
            buf.push(key);
            let replies = scan_replies(&buf, true);
            assert_eq!(
                replies.residue,
                vec![key],
                "{} must reach the engine as the key it is",
                key as char
            );
            assert_eq!(replies.consumed, buf.len());
            assert!(
                !replies.da1 && !replies.kitty && !replies.sync,
                "no capability may be read out of `?2026` plus {}",
                key as char
            );
        }
    }

    #[test]
    fn each_answer_the_batch_can_receive_is_consumed_whole_and_silently() {
        for (answer, name) in [
            (b"\x1b[?2026;1$y".as_slice(), "DECRPM"),
            (b"\x1b[?2026;0$y".as_slice(), "DECRPM, mode unrecognized"),
            (b"\x1b[?0u".as_slice(), "kitty flags"),
            (b"\x1b[?62;4c".as_slice(), "DA1"),
            (b"\x1bP1$r0;48;2;1;2;3m\x1b\\".as_slice(), "DECRQSS"),
            (b"\x1b[1;2R".as_slice(), "CPR"),
        ] {
            let replies = scan_replies(answer, true);
            assert!(
                replies.residue.is_empty() && replies.consumed == answer.len(),
                "{name} must leave nothing behind: {replies:?}",
                replies = replies.residue
            );
        }
        assert!(scan_replies(b"\x1b[?2026;1$y", true).sync);
        assert!(scan_replies(b"\x1b[?0u", true).kitty);
        assert!(scan_replies(b"\x1b[?62;4c", true).da1);
        assert_eq!(
            scan_replies(b"\x1bP1$r0;48;2;1;2;3m\x1b\\", true).truecolor,
            Some(true)
        );
        assert_eq!(scan_replies(b"\x1b[1;2R", true).unicode_boxes, Some(true));
    }

    #[test]
    fn a_parameter_outside_its_grammars_range_is_not_that_answer() {
        // the kitty field is five bits and DA1's class is a small number;
        // past either bound the run is a stalled reply with a key on it
        assert!(!scan_replies(b"\x1b[?32u", true).kitty);
        assert!(!scan_replies(b"\x1b[?66c", true).da1);
        assert!(scan_replies(b"\x1b[?31u", true).kitty);
        assert!(scan_replies(b"\x1b[?65c", true).da1);
        // DECRPM without its `$`, and with a state outside 0..=4
        assert_eq!(scan_replies(b"\x1b[?2026;1y", true).residue, b"y");
        assert_eq!(scan_replies(b"\x1b[?2026;5$y", true).residue, b"y");
    }

    #[test]
    fn only_the_shapes_no_keyboard_emits_are_held_as_the_terminals_own() {
        assert!(is_terminal_only_remainder(b"\x1b[?2026"));
        assert!(is_terminal_only_remainder(b"\x1bP1$r0;48"));
        // equally the opening of every arrow key, so the flush hands it to
        // the decoder rather than discarding it as an answer
        assert!(!is_terminal_only_remainder(b"\x1b["));
        assert!(!is_terminal_only_remainder(b"\x1bP"));
        assert!(!is_terminal_only_remainder(b"\x1b"));
        assert!(!is_terminal_only_remainder(b""));
    }

    #[test]
    fn a_key_that_terminates_a_stalled_private_reply_survives_it() {
        // the terminal stopped mid-answer and the user typed `h`, whose
        // byte is a legal CSI final: the answer is the terminal's, the key
        // is not, and only one of them is anyone's input
        let replies = scan_replies(b"\x1b[?2026h", true);
        assert_eq!(replies.residue, b"h");
        assert_eq!(replies.consumed, 8);
        assert!(!replies.sync, "`h` answers nothing the batch asked");
    }

    #[test]
    fn a_truncated_decrqss_answer_is_dropped_rather_than_forwarded() {
        // the reply cut off mid-body by the deadline: only the terminal can
        // produce `ESC P`, so this is never something to replay into nvim
        let mut source = ScriptedSource::new(vec![Some(b"\x1bP1$r0;48;2;1;2"), None]);
        let mut sink = Vec::new();
        let (caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        assert!(!caps.truecolor, "half an answer answers nothing");
        assert!(residue.is_empty(), "residue should be empty: {residue:?}");
    }

    #[test]
    fn a_bel_terminated_decrqss_answer_is_read_and_consumed() {
        let mut source = ScriptedSource::new(vec![Some(b"\x1bP1$r48;2;1;2;3m\x07typed")]);
        let mut sink = Vec::new();
        let (caps, residue) =
            detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
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
        let probe = Probe::start(source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
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
        let probe =
            Probe::start(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
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
        let probe = Probe::start(
            &mut source,
            &mut sink,
            Duration::from_millis(5),
            &EnvHints::default(),
        )
        .expect("start must not error against an in-memory writer");
        assert!(!probe.fence_seen());
        assert!(!probe.finish(Duration::from_millis(10)).fence_seen);
    }

    #[test]
    fn no_probe_caps_honors_its_hints_even_though_sync_and_kitty_stay_false() {
        // covers both `probe_real_terminal` arms that can never get a real
        // escape reply (unix tcgetattr failure on a non-tty stdin, and the
        // cfg(not(unix)) arm). A probe that was never written is the purest
        // unanswered question, so every hint decides here: stdout can be a
        // real truecolor tty independent of stdin.
        let none = no_probe_caps(&EnvHints::default());
        assert!(!none.sync && !none.truecolor && !none.kitty_kbd && !none.unicode_boxes);
        assert_eq!(tier_name(none.tier), "basic");

        let truecolor = no_probe_caps(&colorterm("truecolor"));
        assert!(!truecolor.sync && truecolor.truecolor && !truecolor.kitty_kbd);
        assert_eq!(tier_name(truecolor.tier), "standard");

        let bit24 = no_probe_caps(&colorterm("24bit"));
        assert!(bit24.truecolor);

        let unrecognized = no_probe_caps(&colorterm("bogus"));
        assert!(!unrecognized.truecolor);

        assert!(no_probe_caps(&utf8_locale()).unicode_boxes);
    }

    #[test]
    fn caps_source_labels_match_the_probe_arm_taken() {
        // The override arm is the one `resolve` can be run against here: it
        // returns before any terminal I/O, while the probe arm reads the
        // process's real stdin, which a test runner shares with every other
        // test in the binary. What that arm would label its result is
        // asserted through `PROBE_SOURCE` instead -- the same constant
        // `resolve` hands back.
        let (caps, probe, source) =
            resolve(Some(Tier::Basic)).expect("an override resolves without terminal I/O");
        assert_eq!(source, CapsSource::Override);
        assert_eq!(source.label(), "--tier override");
        assert!(probe.is_none(), "an override leaves nothing to wait for");
        assert_eq!(caps, caps_for_override(Tier::Basic));

        assert_eq!(
            PROBE_SOURCE,
            if cfg!(unix) {
                CapsSource::Probed
            } else {
                CapsSource::Assumed
            }
        );
        assert_eq!(CapsSource::Probed.label(), "probed");
        assert_eq!(CapsSource::Assumed.label(), "assumed");
    }

    /// The field names a `TermCaps` value renders under `{:?}`.
    ///
    /// The debug shape is the honest enumeration available in-crate:
    /// `TermCaps` is `#[non_exhaustive]`, so no exhaustive struct pattern
    /// can be written here, and a field added over there lands in this
    /// rendering with no edit anywhere.
    fn termcaps_debug_fields() -> Vec<String> {
        let rendered = format!("{:?}", TermCaps::default());
        let body = rendered
            .split_once('{')
            .and_then(|(_, rest)| rest.rsplit_once('}'))
            .map(|(body, _)| body)
            .expect("TermCaps derives Debug as a braced struct");
        body.split(',')
            .filter_map(|field| field.split(':').next())
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect()
    }

    #[test]
    fn every_termcaps_field_has_a_register_row() {
        let fields = termcaps_debug_fields();
        assert!(
            fields.contains(&"tier".to_string()),
            "the exclusion below names a field that must exist, got {fields:?}"
        );
        // `tier` is excluded by construction, not by oversight: it is
        // derived by `TermCaps::from_probe` from the rows below and is
        // never probed, so a row for it would name a question no terminal
        // is ever asked.
        let probed: Vec<&str> = fields
            .iter()
            .map(String::as_str)
            .filter(|name| *name != "tier")
            .collect();

        for field in &probed {
            assert!(
                register().iter().any(|row| row.capability == *field),
                "TermCaps::{field} is a probed capability with no capability \
                 register row: add one naming the probe that carries the fact \
                 and what decides it when that probe is silent"
            );
        }
        for row in register() {
            assert!(
                probed.contains(&row.capability),
                "capability register row `{}` names no TermCaps field; the \
                 probed fields are {probed:?}",
                row.capability
            );
        }
    }

    #[test]
    fn no_row_lists_an_env_var_as_its_probe() {
        const NEVER_A_PROBE: [&str; 5] = ["COLORTERM", "LANG", "LC_ALL", "LC_CTYPE", "TERM"];
        for row in register() {
            let query = String::from_utf8_lossy(row.query);
            for var in NEVER_A_PROBE {
                assert!(
                    !query.contains(var),
                    "`{}` names {var} as its probe; an environment variable is \
                     a hint that shortens a probe, never the question itself",
                    row.capability
                );
            }
            assert!(
                query.starts_with('\x1b') || query.starts_with('\r'),
                "`{}` has a probe that puts nothing on the wire: {query:?}",
                row.capability
            );
        }
    }

    #[test]
    fn every_rows_probe_is_a_query_the_batch_actually_writes() {
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[?62c".as_slice())]);
        let mut sink = Vec::new();
        detect(&mut source, &mut sink, PROBE_DEADLINE, &EnvHints::default()).unwrap();
        for row in register() {
            assert!(
                sink.windows(row.query.len()).any(|w| w == row.query),
                "`{}` cites a query the startup batch never sends",
                row.capability
            );
        }
    }

    #[test]
    fn a_silent_probe_resolves_every_row_to_its_registered_fallback() {
        let hints = EnvHints {
            colorterm: Some("truecolor".to_string()),
            locale: Some("en_US.UTF-8".to_string()),
            ..EnvHints::default()
        };
        // the fence and nothing else: every capability question goes
        // unanswered, which is exactly the case each row's fallback claims
        // to describe
        let mut source = ScriptedSource::new(vec![Some(b"\x1b[?62c".as_slice())]);
        let mut sink = Vec::new();
        let (caps, _) = detect(&mut source, &mut sink, PROBE_DEADLINE, &hints).unwrap();
        for row in register() {
            assert_eq!(
                (row.read)(&caps),
                row.fallback.resolve(&hints),
                "`{}` resolves to something its registered fallback ({:?}) \
                 does not describe",
                row.capability,
                row.fallback
            );
        }
        // the hint each row declares, named: the resolution above passes
        // whichever variable a row happens to point at, and these two are
        // the ones spec and captures oblige
        assert!(matches!(
            TRUECOLOR.fallback,
            Fallback::EnvHint {
                var: "COLORTERM",
                ..
            }
        ));
        assert!(matches!(SYNC.fallback, Fallback::Unanswered));
    }
}
