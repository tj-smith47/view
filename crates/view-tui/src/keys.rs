//! Crossterm key event to nvim input notation encoding.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use view_core::msg::{Key, Msg};

/// Encodes a crossterm [`KeyEvent`] as an nvim `nvim_input` notation string.
///
/// Plain printable characters pass through as themselves, except `<`,
/// which nvim reserves as the start of a special-key sequence and which is
/// therefore written `<lt>`. Named keys (arrows, `Enter`, `Esc`, function
/// keys, ...) map to their nvim `<Name>` form.
///
/// When `Ctrl` and/or `Alt` are held, the result is wrapped as
/// `<C-...>`, `<M-...>`, or `<C-M-...>`. `Shift` is folded into the
/// wrapper for named keys (`<C-S-CR>`) but dropped for plain characters,
/// since crossterm already reports the shifted character itself (`A`
/// rather than `a` with `SHIFT` set).
///
/// `BackTab` (crossterm's dedicated code for Shift+Tab, always reachable
/// on the legacy parser on both Unix and Windows) maps to `<S-Tab>`. The
/// shift semantics are baked into that mapping already, so the `Shift`
/// wrapper is never applied a second time even if crossterm also sets the
/// `SHIFT` modifier bit alongside `BackTab`; the result is always
/// `<S-Tab>` (or `<C-S-Tab>` with `Ctrl` held too), never `<S-S-Tab>`.
///
/// Plain, unmodified space passes through as a literal `" "`, byte-identical
/// to typed input. Space held with `Ctrl` and/or `Alt` uses the named token
/// `Space` instead of embedding the raw space character, producing
/// `<C-Space>` / `<M-Space>` rather than the malformed `<C- >`.
///
/// Returns `None` for key data view does not forward to nvim: key-release
/// events (only reported when a terminal's keyboard-enhancement protocol
/// is enabled) and key codes with no nvim input equivalent, such as media
/// keys and bare modifier keys.
///
/// A `Ctrl` chord is named after the character nvim names it after, which
/// for six of them is not the one the terminal reported
/// ([`nvim_control_char`]). Events crossterm's own parser produced go
/// through [`encode_terminal_key`] instead, which repairs one more
/// disagreement this cannot see from the event alone.
///
/// `view-oracle`'s compat harness (`crates/view-oracle/src/compat.rs`'s
/// `resolve_key_token`) maintains its own independent, hardcoded inverse of
/// this table to type notation into a pty as real keypress bytes;
/// dependency direction keeps the two crates from sharing the table
/// directly, so a change here that is not mirrored there would silently
/// desync what a compat scenario types from what this encoder actually
/// forwards, undetected by either crate's own tests.
#[must_use]
pub(crate) fn encode_key(ev: &KeyEvent) -> Option<String> {
    if ev.kind == KeyEventKind::Release {
        return None;
    }

    let code = match ev.code {
        KeyCode::Char(c) if ev.modifiers.contains(KeyModifiers::CONTROL) => {
            KeyCode::Char(nvim_control_char(c))
        }
        code => code,
    };
    let (mut bare, always_bracketed) = key_token(code)?;
    let is_plain_char = matches!(code, KeyCode::Char(c) if c != '<');
    // BackTab already means Shift+Tab; some terminals additionally set the
    // SHIFT bit on the event, which would otherwise double up the prefix.
    let shift_baked_in = matches!(ev.code, KeyCode::BackTab);

    let mut prefix = String::new();
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        prefix.push_str("C-");
    }
    if shift_baked_in || (!is_plain_char && ev.modifiers.contains(KeyModifiers::SHIFT)) {
        prefix.push_str("S-");
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        prefix.push_str("M-");
    }

    let wrap = always_bracketed || !prefix.is_empty();
    // Space has no dedicated KeyCode; it arrives as Char(' '). Unmodified it
    // must stay a literal space for byte-identical typing, but once wrapped
    // in a modifier prefix the raw space would render as the malformed
    // "<C- >", so swap in the named token only in the wrapped case.
    if wrap && code == KeyCode::Char(' ') {
        bare = "Space".to_string();
    }

    Some(if wrap {
        format!("<{prefix}{bare}>")
    } else {
        bare
    })
}

/// Translates raw bytes read during the startup capability probe (before
/// the real input reader -- the runtime loop's inline drain on unix, the
/// dedicated input thread elsewhere -- exists) into nvim input notation
/// strings, so [`Term::init`](crate::terminal::Term::init)'s
/// caller can forward anything the user typed at spawn instead of losing
/// it (see [`tiers::detect`](crate::tiers::detect) for how this residue is
/// separated from the probe's own capability replies).
///
/// The same decoder the runtime loop's own drain uses, entered here with
/// the terminal already drained to `EAGAIN`: what it holds is what the
/// probe's window caught, and there is no later read to resolve a run left
/// half-arrived, so this entry point forces one rather than waiting.
/// Printable ASCII passes through as literal characters (matching
/// [`encode_key`]'s plain-char case, including `<` becoming `<lt>`);
/// `\r` maps to `<CR>`, `\t` to `<Tab>`, `0x7f` to `<BS>`, and the
/// remaining C0 controls to the `<C-...>` chord they spell -- `\n`
/// included, because the terminal is in raw mode and `Enter` sends `\r`
/// there ([`plain_key`]). A run of continuous non-ASCII bytes (`0x80..`) is
/// decoded as one UTF-8 str and forwarded char-by-char if valid, dropped
/// if not (a mid-codepoint chunk boundary can produce invalid UTF-8 here,
/// and guessing at a replacement is worse than dropping a still-rare
/// case).
///
/// `ESC` immediately followed by `[` or `O` opens a CSI or SS3 sequence --
/// an arrow, a function key, a keypad key, a keyboard-protocol report (SS3
/// is what a DECCKM application-cursor mode inherited from the spawning
/// shell encodes arrows as, e.g. `ESC O A` for Up). One the tables below
/// name decodes into the key it is, through the same [`encode_key`] every
/// keystroke after startup goes through.
///
/// One they do not is consumed whole and typed as nothing, which is what
/// crossterm's parser does with a sequence it cannot name: it drops the
/// bytes it had accumulated and reads on from the one after them. Typing
/// `<lt>0;24;10M` out of an SGR mouse report is nine keystrokes nobody
/// pressed, and `h` out of `ESC [ h` is a `dd` waiting to happen; the keys
/// behind the run are the user's in both readings and survive in both.
///
/// A string sequence (`ESC P`, `ESC ]`, `ESC ^`, `ESC _`, `ESC X`) is a
/// report too and is consumed through its terminator -- but only once the
/// terminator has arrived: those same two bytes are Alt+Shift+P and
/// Alt+`]`, which cannot be held back waiting for a string that may never
/// come.
///
/// A sequence whose final byte has not arrived is unfinished rather than
/// unknown, and the difference is a whole keystroke: dropped now, its tail
/// arrives alone in the next read and types an arrow's `A` into the buffer.
/// [`decode_residue`] reports the length of such a tail so a caller that
/// can wait for the rest does; this entry point cannot and drops it.
///
/// `ESC` and one key in the same run are Alt and that key -- for a
/// multi-byte character as much as for a printable ASCII one, and for a
/// whole sequence as much as for a single byte, so `ESC ESC` is `<M-Esc>`
/// and `ESC ESC [ A` is `<M-Up>`. A lone trailing `ESC` is the Escape key,
/// forced here because the caller has already drained the fd to `EAGAIN`
/// and no later read can turn it into a chord.
#[must_use]
pub fn encode_residue_bytes(residue: &[u8]) -> Vec<String> {
    decode_residue_forced(residue)
        .into_iter()
        .filter_map(|msg| match msg {
            Msg::Key(key) => Some(key.notation),
            // a paste has no notation: handed to `nvim_input` its text
            // would run as normal-mode commands. Only a caller holding the
            // message channel can deliver one
            _ => None,
        })
        .collect()
}

/// What one run of residue bytes decodes to.
pub(crate) struct ResidueDecode {
    /// The messages the run's complete bytes represent, in wire order.
    pub(crate) msgs: Vec<Msg>,
    /// How many bytes at the end of the run are the opening of a sequence
    /// whose final byte -- or, for a bracketed paste, whose closer -- has
    /// not arrived. Zero unless the run ends mid-sequence.
    ///
    /// A caller that runs out of time with one of these in hand drops it,
    /// and for a paste that means the whole paste rather than part of it:
    /// half a paste delivered is text truncated silently, and the bytes
    /// behind the cut reach the next reader as literal keys, which in
    /// normal mode are commands. Losing it whole is the answer that cannot
    /// corrupt a buffer.
    // read by the unix descriptor loop, which is the only reader that can
    // run out of time holding a partial sequence
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) unfinished: usize,
}

/// [`encode_residue_bytes`]'s decode, in full: the messages and the tail
/// that only the bytes still in flight can resolve.
pub(crate) fn decode_residue(residue: &[u8]) -> ResidueDecode {
    let mut msgs = Vec::new();
    let mut i = 0;
    while i < residue.len() {
        match residue[i] {
            0x1b => {
                let run = &residue[i..];
                match escape_run(run, KeyModifiers::NONE) {
                    Escape::Decoded { len, msg } => {
                        msgs.extend(msg);
                        i += len;
                    }
                    Escape::Unknown { len } => i += len,
                    Escape::Unfinished => {
                        return ResidueDecode {
                            msgs,
                            unfinished: residue.len() - i,
                        }
                    }
                }
            }
            b if b >= 0x80 => match utf8_char(&residue[i..]) {
                Utf8::Char(decoded, len) => {
                    msgs.push(key_msg(decoded.to_string()));
                    i += len;
                }
                Utf8::Unfinished => {
                    return ResidueDecode {
                        msgs,
                        unfinished: residue.len() - i,
                    }
                }
                Utf8::Invalid => i += 1,
            },
            byte => {
                if let Some((code, mods)) = plain_key(byte) {
                    msgs.extend(encode_key(&KeyEvent::new(code, mods)).map(key_msg));
                }
                i += 1;
            }
        }
    }
    ResidueDecode {
        msgs,
        unfinished: 0,
    }
}

/// [`decode_residue`]'s answer for a run whose escape timeout has expired:
/// the engine waits `ttimeoutlen` for the rest of a key code and then
/// reads what it has as an Alt chord over the byte that never became a
/// sequence (`tui/input.c` handing the run to termkey forced,
/// `driver-csi.c`'s `peekkey_csi` and `termkey.c`'s `peekkey_simple`), so
/// `ESC [` is `<M-[>` and `ESC O` is `<M-O>`, with the parameters behind
/// an unfinished introducer typed as the characters they are. An `ESC`
/// with nothing behind it is the Escape key; the `ESC`s in front of it are
/// Alt, so `ESC ESC` is `<M-Esc>`.
///
/// Only an escape run is flushed this way. A character or a bracketed
/// paste cut in half by a read boundary is dropped instead, which is
/// [`ResidueDecode::unfinished`]'s own rule for a caller that has run out
/// of reads: half a paste typed into a buffer runs its body as normal-mode
/// commands.
pub(crate) fn decode_residue_forced(residue: &[u8]) -> Vec<Msg> {
    let decoded = decode_residue(residue);
    let mut msgs = decoded.msgs;
    let held = &residue[residue.len() - decoded.unfinished..];
    if !forceable(held) {
        return msgs;
    }
    // every leading `ESC` but the last is Alt over what follows, which is
    // how the engine's own reader composes the chord -- and why a run of
    // nothing but escapes collapses to one chord rather than to a key per
    // byte
    let opened = held.iter().take_while(|&&byte| byte == 0x1b).count();
    let alt = if opened > 1 {
        KeyModifiers::ALT
    } else {
        KeyModifiers::NONE
    };
    match held.get(opened) {
        None => msgs.extend(forced_key(KeyCode::Esc, alt)),
        Some(&byte) => {
            msgs.extend(forced_key(
                KeyCode::Char(char::from(byte)),
                KeyModifiers::ALT,
            ));
            msgs.extend(decode_residue(&held[opened + 1..]).msgs);
        }
    }
    msgs
}

/// One key of a forced run, or nothing when nvim has no notation for it.
fn forced_key(code: KeyCode, mods: KeyModifiers) -> Option<Msg> {
    encode_key(&KeyEvent::new(code, mods)).map(key_msg)
}

/// Whether an unfinished tail is one an escape timeout may flush.
///
/// A bracketed paste is the exception: its closer arrives when the pasted
/// text ends, which no keystroke timeout bounds, and flushing one would
/// type the pasted body into the buffer as normal-mode commands. The
/// opener is what identifies it, so a read cut off inside those six bytes
/// is flushed like any other run -- nvim reads that boundary the same way,
/// and a terminal writes the opener whole.
pub(crate) fn forceable(held: &[u8]) -> bool {
    held.first() == Some(&0x1b) && !held.starts_with(PASTE_OPEN)
}

/// `ESC [` and `ESC O`: the two bytes a sequence opens with, and where
/// its parameters start.
const ESCAPE_INTRODUCER_LEN: usize = 2;

/// The bracketed-paste parameters and closer, as the terminal writes them
/// around pasted text once `EnableBracketedPaste` is in force -- which it
/// is from the moment view enters the alternate screen, before the
/// capability probe has finished.
const PASTE_OPEN_PARAMS: &[u8] = b"200";

/// The spec's string terminator. `BEL` is the other one terminals write,
/// and both end a report here for the same reason they do in the
/// capability probe's own scan.
const STRING_TERMINATOR: &[u8] = b"\x1b\\";
const PASTE_OPEN: &[u8] = b"\x1b[200~";
const PASTE_CLOSE: &[u8] = b"\x1b[201~";

/// How the residue decoder reads one `ESC [` / `ESC O` run.
enum Escape {
    /// A complete sequence `len` bytes long, and the message it is (`None`
    /// for one with no nvim equivalent, which is consumed all the same).
    Decoded { len: usize, msg: Option<Msg> },
    /// Complete, and nothing this decoder's tables name: `len` bytes that
    /// type nothing, the reading crossterm takes of the same run.
    Unknown { len: usize },
    /// Its final byte, or its paste closer, has not arrived.
    Unfinished,
}

/// One `ESC` run, whatever kind it is, with the modifiers the `ESC`s in
/// front of it have already spelled.
///
/// `held` is how an Alt chord composes: the terminal writes one for every
/// key it can, and `ESC` in front of any of those runs is Alt held over
/// whatever the rest of it decodes to.
fn escape_run(run: &[u8], held: KeyModifiers) -> Escape {
    if let Some(len) = string_sequence_len(run) {
        Escape::Unknown { len }
    } else if matches!(run.get(1), Some(&b'[') | Some(&b'O')) {
        escape_sequence(run, held)
    } else {
        alt_key(run, held)
    }
}

fn escape_sequence(run: &[u8], held: KeyModifiers) -> Escape {
    if run.get(1) == Some(&b'O') {
        // SS3 is exactly three bytes: introducer and one final
        let Some(&final_byte) = run.get(2) else {
            return Escape::Unfinished;
        };
        return match cursor_key(final_byte) {
            Some(code) => decoded(3, code, held),
            None => Escape::Unknown { len: 3 },
        };
    }
    csi_sequence(run, held)
}

fn csi_sequence(run: &[u8], held: KeyModifiers) -> Escape {
    let mut at = ESCAPE_INTRODUCER_LEN;
    // parameter and intermediate bytes both precede the final byte, and
    // neither can be mistaken for it: the ranges do not overlap
    while run.get(at).is_some_and(|byte| (0x20..=0x3f).contains(byte)) {
        at += 1;
    }
    let Some(&final_byte) = run.get(at) else {
        return Escape::Unfinished;
    };
    let params = &run[ESCAPE_INTRODUCER_LEN..at];
    let len = at + 1;
    if final_byte == b'~' && params == PASTE_OPEN_PARAMS {
        return paste(run, len);
    }
    let fields = param_fields(params);
    let modifier = modifiers(field(&fields, 1)) | held;
    let unnamed = Escape::Unknown { len };
    match final_byte {
        b'~' => match tilde_key(&fields) {
            Some((code, mods)) => decoded(len, code, mods | held),
            None => unnamed,
        },
        // the kitty keyboard protocol's own form, which a terminal keeps
        // speaking for as long as some earlier process left it pushed. Its
        // modifier field carries the event type as a sub-parameter, and a
        // release is not a keypress
        b'u' => match kitty_key(&fields) {
            Some((code, mods)) => Escape::Decoded {
                len,
                msg: encode_key(&KeyEvent::new_with_kind(
                    code,
                    mods | held,
                    event_kind(&fields),
                ))
                .map(key_msg),
            },
            None => unnamed,
        },
        // the Linux console's F1-F5: a second introducer and then the
        // letter, with nothing between them
        b'[' if fields.is_empty() => match run.get(len) {
            None => Escape::Unfinished,
            Some(&letter @ b'A'..=b'E') => decoded(len + 1, KeyCode::F(1 + letter - b'A'), held),
            Some(_) => Escape::Unknown { len: len + 1 },
        },
        // a focus report, which `event_to_msg` also drops for crossterm's
        // own events -- and which typed through would insert at the line
        // start or open a line above
        b'I' | b'O' if fields.is_empty() => Escape::Decoded { len, msg: None },
        // `CSI R` is a cursor-position report in every form a terminal
        // sends one unasked, and crossterm names none of those a key: bare
        // it is an error there, typing nothing, and with a row and column
        // it is the report. The one spelling that is F3 is the modifier
        // form `CSI ; m R`, whose first parameter no position report
        // leaves empty -- crossterm routes it by that leading `;` through
        // `parse_csi_modifier_key_code` and so does this. Read off the
        // bytes rather than the parsed fields: a field that is missing and
        // a field that is a literal `0` both parse to 0, and `CSI 0 R` is
        // a report crossterm types nothing from
        b'R' if params.starts_with(b";") => decoded(len, KeyCode::F(3), modifier),
        b'R' => Escape::Decoded { len, msg: None },
        b'M' | b'm' => mouse_report(run, params, final_byte, len),
        _ => match cursor_key(final_byte) {
            Some(code) => decoded(len, code, modifier),
            None => unnamed,
        },
    }
}

/// The three mouse encodings a terminal answers `EnableMouseCapture` with,
/// as the [`Msg::Mouse`] each is, or the sequence it still is when the
/// button spells no button this vocabulary has.
///
/// `?1006h` (SGR) is what a modern terminal takes, and it is the only one
/// of the three that can spell a release: its parameters open with `<` and
/// its final byte is lowercase for a release, uppercase for a press. A
/// terminal that took `?1015h` instead answers in rxvt's form, the same
/// parameters without the `<` and with the button biased by 32; one that
/// took neither answers X10's, where the button and both coordinates are
/// three raw bytes behind the final rather than parameters at all -- which
/// is why that form is the one that can be cut short by the end of a read.
///
/// Both coordinates are one-based on the wire and zero-based in every
/// consumer, and a terminal that reports a zero saturates rather than
/// wrapping to the far edge of the screen.
fn mouse_report(run: &[u8], params: &[u8], final_byte: u8, len: usize) -> Escape {
    if params.is_empty() {
        let Some(report) = run.get(len..len + X10_REPORT_LEN) else {
            return Escape::Unfinished;
        };
        return mouse_decoded(
            len + X10_REPORT_LEN,
            report[0].wrapping_sub(X10_BIAS),
            u16::from(report[1].wrapping_sub(X10_BIAS)),
            u16::from(report[2].wrapping_sub(X10_BIAS)),
            final_byte,
        );
    }
    let sgr = params.first() == Some(&b'<');
    let fields = param_fields(if sgr { &params[1..] } else { params });
    let (Some(button), Some(column), Some(row)) =
        (field(&fields, 0), field(&fields, 1), field(&fields, 2))
    else {
        return Escape::Unknown { len };
    };
    let Ok(button) = u8::try_from(button) else {
        return Escape::Unknown { len };
    };
    mouse_decoded(
        len,
        if sgr {
            button
        } else {
            button.wrapping_sub(X10_BIAS)
        },
        saturate_u16(column),
        saturate_u16(row),
        final_byte,
    )
}

/// X10's three-byte report and the offset every one of its fields carries,
/// so a coordinate stays a printable byte.
const X10_REPORT_LEN: usize = 3;
const X10_BIAS: u8 = 32;

/// One decoded mouse report as the message it is, with the one-based wire
/// coordinates brought down to the zero-based cell the rest of view counts
/// in.
fn mouse_decoded(len: usize, button: u8, column: u16, row: u16, final_byte: u8) -> Escape {
    let Some((kind, modifiers)) = mouse_kind(button) else {
        return Escape::Unknown { len };
    };
    // only SGR distinguishes a release, and it does it with the case of the
    // final byte rather than in the button field: the other two forms
    // report every release as button 3 and cannot say which button it was
    let kind = match (final_byte, kind) {
        (b'm', MouseEventKind::Down(button)) => MouseEventKind::Up(button),
        (_, kind) => kind,
    };
    Escape::Decoded {
        len,
        msg: Some(Msg::Mouse(crate::mouse::encode_mouse(&MouseEvent {
            kind,
            column: column.saturating_sub(1),
            row: row.saturating_sub(1),
            modifiers,
        }))),
    }
}

/// The button field of a mouse report, split into the event it describes
/// and the modifiers held while it happened. `None` for a button no nvim
/// action names -- the report is consumed either way, never typed.
fn mouse_kind(button: u8) -> Option<(MouseEventKind, KeyModifiers)> {
    // the button number is split across two ranges of the same field: the
    // low pair, and the two high bits a terminal reports the extra buttons
    // (wheel and beyond) in
    let number = (button & 0b0000_0011) | ((button & 0b1100_0000) >> 4);
    let dragging = button & 0b0010_0000 != 0;
    let kind = match (number, dragging) {
        (0, false) => MouseEventKind::Down(MouseButton::Left),
        (1, false) => MouseEventKind::Down(MouseButton::Middle),
        (2, false) => MouseEventKind::Down(MouseButton::Right),
        (0, true) => MouseEventKind::Drag(MouseButton::Left),
        (1, true) => MouseEventKind::Drag(MouseButton::Middle),
        (2, true) => MouseEventKind::Drag(MouseButton::Right),
        (3, false) => MouseEventKind::Up(MouseButton::Left),
        (3..=5, true) => MouseEventKind::Moved,
        (4, false) => MouseEventKind::ScrollUp,
        (5, false) => MouseEventKind::ScrollDown,
        (6, false) => MouseEventKind::ScrollLeft,
        (7, false) => MouseEventKind::ScrollRight,
        _ => return None,
    };
    let mut modifiers = KeyModifiers::empty();
    if button & 0b0000_0100 != 0 {
        modifiers |= KeyModifiers::SHIFT;
    }
    if button & 0b0000_1000 != 0 {
        modifiers |= KeyModifiers::ALT;
    }
    if button & 0b0001_0000 != 0 {
        modifiers |= KeyModifiers::CONTROL;
    }
    Some((kind, modifiers))
}

/// A wire coordinate as the `u16` a cell position is, with anything past
/// the widest terminal there is clamped rather than wrapped.
fn saturate_u16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// The kitty protocol's `base:shifted` alternate-key pair: with `Shift`
/// held a terminal reports both what the key is and what it typed, and
/// the one that reaches the buffer is the second. Crossterm resolves the
/// pair the same way, dropping the modifier the terminal has already
/// applied so `Shift`+`a` does not arrive as `<S-a>`.
fn kitty_key(fields: &[Vec<u32>]) -> Option<(KeyCode, KeyModifiers)> {
    let mods = modifiers(field(fields, 1));
    match fields.first()?.get(1) {
        Some(&shifted) if mods.contains(KeyModifiers::SHIFT) => {
            Some((char_key(shifted)?, mods.difference(KeyModifiers::SHIFT)))
        }
        _ => Some((char_key(field(fields, 0)?)?, mods)),
    }
}

/// The string sequences a terminal reports with -- DCS, OSC, SOS, PM, APC
/// -- as the length of one, terminator included, or `None` when these
/// bytes are not one (yet).
///
/// A terminator that has not arrived reads as `None` rather than as an
/// unfinished sequence, so `ESC P` and `ESC ]` alone are the Alt+Shift+P
/// and Alt+`]` they also are. What decides between the two readings is
/// which mistake is worse and which case is real: an unrequested report
/// arriving in the window is ordinary, and typing its body into the buffer
/// runs it as normal-mode commands, while the reading this loses -- a user
/// typing `Alt+]`, then text, then `Ctrl+G`, all inside one read -- is not
/// something a keyboard produces. A whole terminated string in one run is
/// therefore the report, whatever else it could have spelled.
fn string_sequence_len(run: &[u8]) -> Option<usize> {
    if !matches!(run.get(1)?, b'P' | b']' | b'^' | b'_' | b'X') {
        return None;
    }
    let body = run.get(2..)?;
    let st = body
        .windows(STRING_TERMINATOR.len())
        .position(|window| window == STRING_TERMINATOR)
        .map(|at| at + STRING_TERMINATOR.len());
    let bel = body.iter().position(|&byte| byte == 0x07).map(|at| at + 1);
    Some(2 + [st, bel].into_iter().flatten().min()?)
}

/// `ESC` and one key in the same run: crossterm reads that as the key with
/// Alt held (its parser recurses on the bytes after the `ESC` and ors the
/// modifier in), so a `<M-...>` mapping resolves the same inside this
/// window as outside it. The recursion is what makes a multi-byte
/// character an Alt chord there as much as an ASCII one, and the same
/// decode is what makes it one here.
///
/// Three runs are not that chord. A second `ESC` ends the first one and
/// nothing more: the `ESC` is Alt held over whatever the bytes behind it
/// decode to, however many of them there are, which is how the engine's
/// own reader composes the chord (termkey's `peekkey_simple` recurses past
/// an `ESC` and ors `KEYMOD_ALT` into the key it finds). So `ESC ESC` is
/// `<M-Esc>`, `ESC ESC [ A` is `<M-Up>` and `ESC ESC x` is `<M-x>` -- one
/// chord each, not a key per byte. nvim then degrades a chord no mapping
/// claims back to `<Esc>` and the key (`getchar.c`), which is why an
/// unmapped `ESC ESC` still opens as two Escape keys in a buffer.
///
/// An `ESC` with nothing behind it is unfinished: the byte that would make
/// it a chord may be in the read that has not happened yet, and the engine
/// waits `ttimeoutlen` for it before reading the `ESC` alone
/// ([`decode_residue_forced`]). A character cut short by the end of the
/// read is unfinished for the same reason.
fn alt_key(run: &[u8], held: KeyModifiers) -> Escape {
    let held = held | KeyModifiers::ALT;
    match run.get(1) {
        None => Escape::Unfinished,
        Some(&0x1b) => match escape_run(&run[1..], held) {
            Escape::Decoded { len, msg } => Escape::Decoded { len: len + 1, msg },
            Escape::Unknown { len } => Escape::Unknown { len: len + 1 },
            Escape::Unfinished => Escape::Unfinished,
        },
        Some(&byte) if byte >= 0x80 => match utf8_char(&run[1..]) {
            Utf8::Char(typed, len) => decoded(1 + len, KeyCode::Char(typed), held),
            Utf8::Unfinished => Escape::Unfinished,
            // an `ESC` in front of bytes no character encodes: the escape
            // is the user's keystroke and the rest is the terminal's noise
            Utf8::Invalid => escape_key(1, held - KeyModifiers::ALT),
        },
        Some(&byte) => match plain_key(byte) {
            Some((code, mods)) => decoded(2, code, mods | held),
            None => escape_key(1, held - KeyModifiers::ALT),
        },
    }
}

/// The Escape key itself, carrying whatever modifiers the `ESC`s in front
/// of it spelled.
fn escape_key(len: usize, held: KeyModifiers) -> Escape {
    decoded(len, KeyCode::Esc, held)
}

/// One character off the front of a byte run.
enum Utf8 {
    /// The character and the number of bytes it took.
    Char(char, usize),
    /// The run ends part-way through a character, so the rest of it is in
    /// the read that has not happened yet.
    Unfinished,
    /// Bytes no character encodes to. Dropping them is the only answer
    /// that cannot type something nobody pressed.
    Invalid,
}

fn utf8_char(run: &[u8]) -> Utf8 {
    let first = |text: &str| {
        text.chars()
            .next()
            .map_or(Utf8::Invalid, |typed| Utf8::Char(typed, typed.len_utf8()))
    };
    match std::str::from_utf8(run) {
        Ok(text) => first(text),
        // a character can be whole with the error behind it, and the
        // difference between the two errors is the whole point: bytes that
        // ran out mid-character are a tail to wait for, bytes that no
        // character encodes to are a tail to drop
        Err(err) => match std::str::from_utf8(&run[..err.valid_up_to()]) {
            Ok(text) if !text.is_empty() => first(text),
            _ if err.error_len().is_none() => Utf8::Unfinished,
            _ => Utf8::Invalid,
        },
    }
}

/// One byte outside any sequence, read as the key nvim reads it as.
///
/// The C0 controls are how a terminal spells `Ctrl` with a letter (`0x17`
/// is `Ctrl`+`w`, the byte being the letter's position in the alphabet),
/// and dropping them costs the window every `<C-...>` mapping a user has.
/// Only the four a keyboard has a key of its own for -- `Enter`, `Tab`,
/// `Esc`, `Backspace` -- keep that name instead.
///
/// nvim's own reading, not crossterm's, and the two part at `0x1c..=0x1f`:
/// crossterm names those `Ctrl`+`4`..`7`, while nvim's input layer names
/// them `<C-\>`, `<C-]>`, `<C-^>` and `<C-_>` -- the escape out of
/// terminal mode, the tag jump, the alternate file, and the byte most
/// terminals send for `Ctrl`+`/`. A user's mapping is written against the
/// nvim name, so the crossterm name reaches nvim as a key nobody bound.
/// The letter and named-key rows are `:help key-notation` of the pinned
/// engine (`runtime/doc/intro.txt` lines 128-143, v0.12.4); that table has
/// no row for `0x1c..=0x1f` at all, so those four come from the same
/// engine's own answer to each byte typed at its tty:
/// `keytrans(getcharstr())`.
fn plain_key(byte: u8) -> Option<(KeyCode, KeyModifiers)> {
    let code = match byte {
        // raw mode is what makes this a chord table rather than a line
        // discipline's: `Enter` sends `\r` here, so `\n` is the `Ctrl`+`j`
        // it also is, which is the same reading crossterm takes with the
        // mode on
        b'\r' => KeyCode::Enter,
        b'\t' => KeyCode::Tab,
        0x1b => KeyCode::Esc,
        0x7f => KeyCode::Backspace,
        0 => return Some((KeyCode::Char(' '), KeyModifiers::CONTROL)),
        0x01..=0x1a => {
            let letter = byte - 1 + b'a';
            return Some((KeyCode::Char(letter as char), KeyModifiers::CONTROL));
        }
        // the byte is the punctuation character's own code with bits 6-7
        // cleared, exactly as a letter's is
        0x1c..=0x1f => {
            let punctuation = byte + 0x40;
            return Some((KeyCode::Char(punctuation as char), KeyModifiers::CONTROL));
        }
        b if (0x20..=0x7e).contains(&b) => KeyCode::Char(b as char),
        _ => return None,
    };
    Some((code, KeyModifiers::NONE))
}

fn decoded(len: usize, code: KeyCode, mods: KeyModifiers) -> Escape {
    Escape::Decoded {
        len,
        msg: encode_key(&KeyEvent::new(code, mods)).map(key_msg),
    }
}

fn paste(run: &[u8], body_at: usize) -> Escape {
    let Some(end) = run[body_at..]
        .windows(PASTE_CLOSE.len())
        .position(|window| window == PASTE_CLOSE)
    else {
        return Escape::Unfinished;
    };
    Escape::Decoded {
        len: body_at + end + PASTE_CLOSE.len(),
        msg: Some(Msg::Paste(
            String::from_utf8_lossy(&run[body_at..body_at + end]).into_owned(),
        )),
    }
}

/// The numeric parameters of a CSI, each split into its sub-parameters,
/// with an omitted or unreadable one as `0` -- a value none of the tables
/// below accepts, so it resolves to the same "not a key this decoder
/// names" as any other unknown.
///
/// Sub-parameters are not decoration: the kitty protocol spells an
/// alternate key as `base:shifted` and an event type as `mods:event`, and
/// a parser that splits on `;` alone reads the whole field as unparseable
/// and types the `:` into the buffer.
fn param_fields(params: &[u8]) -> Vec<Vec<u32>> {
    if params.is_empty() {
        return Vec::new();
    }
    params
        .split(|&byte| byte == b';')
        .map(|field| {
            field
                .split(|&byte| byte == b':')
                .map(|sub| {
                    std::str::from_utf8(sub)
                        .ok()
                        .and_then(|digits| digits.parse().ok())
                        .unwrap_or(0)
                })
                .collect()
        })
        .collect()
}

/// Parameter `at`, as the first of its sub-parameters -- the base key of a
/// kitty `base:shifted` pair, and the plain value of every other field.
fn field(fields: &[Vec<u32>], at: usize) -> Option<u32> {
    fields.get(at)?.first().copied()
}

/// The kitty event type, carried as the modifier field's second
/// sub-parameter: 1 press, 2 repeat, 3 release. [`encode_key`] is what
/// then drops a release, so this decode and crossterm's agree on which
/// events become keys.
fn event_kind(fields: &[Vec<u32>]) -> KeyEventKind {
    match fields.get(1).and_then(|field| field.get(1)) {
        Some(2) => KeyEventKind::Repeat,
        Some(3) => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    }
}

/// xterm's modifier parameter: a bitfield offset by one, so an absent
/// parameter and a `1` both mean no modifier held.
fn modifiers(field: Option<u32>) -> KeyModifiers {
    let bits = field.unwrap_or(1).saturating_sub(1);
    let mut mods = KeyModifiers::NONE;
    if bits & 0b001 != 0 {
        mods |= KeyModifiers::SHIFT;
    }
    if bits & 0b010 != 0 {
        mods |= KeyModifiers::ALT;
    }
    if bits & 0b100 != 0 {
        mods |= KeyModifiers::CONTROL;
    }
    mods
}

/// The final byte of a cursor or editing-key sequence, in the spelling CSI
/// and SS3 share.
///
/// `R` reaches this from SS3 alone: as a CSI final it is answered above,
/// where the same byte is a cursor-position report.
fn cursor_key(final_byte: u8) -> Option<KeyCode> {
    Some(match final_byte {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'F' => KeyCode::End,
        b'H' => KeyCode::Home,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        b'Z' => KeyCode::BackTab,
        _ => return None,
    })
}

/// A `~`-terminated CSI: the keypad and function keys, plus xterm's
/// `modifyOtherKeys` form (`CSI 27 ; mods ; code ~`), which spells an
/// ordinary key whose modifiers the legacy encoding could not carry.
fn tilde_key(fields: &[Vec<u32>]) -> Option<(KeyCode, KeyModifiers)> {
    if fields.len() == 3 && field(fields, 0) == Some(27) {
        return Some((char_key(field(fields, 2)?)?, modifiers(field(fields, 1))));
    }
    let code = keypad_key(field(fields, 0)?)?;
    Some((code, modifiers(field(fields, 1))))
}

fn keypad_key(code: u32) -> Option<KeyCode> {
    Some(match code {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        11..=15 => KeyCode::F(u8::try_from(code - 10).ok()?),
        17..=21 => KeyCode::F(u8::try_from(code - 11).ok()?),
        23..=26 => KeyCode::F(u8::try_from(code - 12).ok()?),
        28..=29 => KeyCode::F(u8::try_from(code - 15).ok()?),
        31..=34 => KeyCode::F(u8::try_from(code - 17).ok()?),
        _ => return None,
    })
}

/// A codepoint as the key that produces it. The four control codes are
/// named rather than passed through as characters, matching what
/// [`encode_key`] receives for the same keys from crossterm's own parser,
/// so `CSI 13 ; 2 u` is `<S-CR>` here and there alike.
fn char_key(code: u32) -> Option<KeyCode> {
    Some(match code {
        8 | 127 => KeyCode::Backspace,
        9 => KeyCode::Tab,
        13 => KeyCode::Enter,
        27 => KeyCode::Esc,
        0 => return None,
        FUNCTIONAL_FIRST..=FUNCTIONAL_LAST => functional_key(code)?,
        _ => KeyCode::Char(char::from_u32(code)?),
    })
}

/// The private-use block the kitty keyboard protocol spells its functional
/// keys in, which are keys rather than characters: passed through as
/// characters they type a glyph no font has and no user pressed.
const FUNCTIONAL_FIRST: u32 = 57358;
const FUNCTIONAL_LAST: u32 = 57454;

/// A functional codepoint as the key it names, mirroring crossterm's
/// `translate_functional_key_code` so a kitty terminal answering late in
/// the window produces the same key it would a millisecond afterwards.
///
/// The media keys and the bare modifier keys resolve to `None`: nvim has
/// no notation for either, and [`key_token`] drops crossterm's own events
/// for them the same way -- so both readings end as a consumed sequence
/// that types nothing.
fn functional_key(code: u32) -> Option<KeyCode> {
    Some(match code {
        57358 => KeyCode::CapsLock,
        57359 => KeyCode::ScrollLock,
        57360 => KeyCode::NumLock,
        57361 => KeyCode::PrintScreen,
        57362 => KeyCode::Pause,
        57363 => KeyCode::Menu,
        57376..=57398 => KeyCode::F(u8::try_from(code - 57376 + 13).ok()?),
        57399..=57408 => KeyCode::Char(char::from_u32(code - 57399 + u32::from(b'0'))?),
        57409 => KeyCode::Char('.'),
        57410 => KeyCode::Char('/'),
        57411 => KeyCode::Char('*'),
        57412 => KeyCode::Char('-'),
        57413 => KeyCode::Char('+'),
        57414 => KeyCode::Enter,
        57415 => KeyCode::Char('='),
        57416 => KeyCode::Char(','),
        57417 => KeyCode::Left,
        57418 => KeyCode::Right,
        57419 => KeyCode::Up,
        57420 => KeyCode::Down,
        57421 => KeyCode::PageUp,
        57422 => KeyCode::PageDown,
        57423 => KeyCode::Home,
        57424 => KeyCode::End,
        57425 => KeyCode::Insert,
        57426 => KeyCode::Delete,
        57427 => KeyCode::KeypadBegin,
        _ => return None,
    })
}

fn key_msg(notation: impl Into<String>) -> Msg {
    Msg::Key(Key {
        notation: notation.into(),
    })
}

/// Maps a [`KeyCode`] to its bare nvim notation name and whether that name
/// must always render inside `<...>` even without modifiers.
///
/// Plain characters (other than `<`) return `always_bracketed: false` so
/// [`encode_key`] can emit them unwrapped when no modifier applies.
/// The character nvim names a `Ctrl` chord after, which for six of them is
/// not the character the terminal reported.
///
/// nvim's input layer folds a `Ctrl` chord onto the C0 byte the reported
/// character's code carries, so `Ctrl` and a backtick becomes `<C-@>` --
/// the name nvim also spells `<Nul>`, both of them the same `\x80\xffX` on
/// the wire -- and the `{|}~` quartet becomes the `[\]^` one; `Ctrl`+`6` is
/// the separate legacy alias for `<C-^>`, which is where `^` sits on a US
/// keyboard. Only the keyboard protocol ever reports these as characters --
/// a legacy terminal sends the C0 byte itself, which [`plain_key`] names
/// directly -- so the fold is what makes a protocol terminal's chord land
/// on the mapping the same chord fires without it.
///
/// Read off the pinned engine (v0.12.4) by typing each `CSI <code>;5u` at
/// its own tty and asking `keytrans(getcharstr())` for the name it gave.
fn nvim_control_char(c: char) -> char {
    match c {
        '`' => '@',
        '{' => '[',
        '|' => '\\',
        '}' => ']',
        '~' | '6' => '^',
        c => c,
    }
}

/// The C0 bytes a terminal with no keyboard protocol in force sends for
/// the four `Ctrl` chords crossterm names after digits, in `0x1c..=0x1f`
/// order.
const CROSSTERM_DIGIT_CHORDS: [(char, char); 4] = [('4', '\\'), ('5', ']'), ('6', '^'), ('7', '_')];

/// The nvim name for a key crossterm's own parser read off the terminal.
///
/// [`encode_key`] names the key the event says it is; this repairs the one
/// place crossterm's legacy-byte table disagrees with nvim's before it
/// does. A terminal with no keyboard protocol in force spells `<C-\>`,
/// `<C-]>`, `<C-^>` and `<C-_>` as the bare bytes `0x1c..=0x1f`, which
/// crossterm reads as `Ctrl`+`4`..`7` and nvim reads as those four
/// punctuation chords -- and the nvim name is the one a user's mapping is
/// written against (`<C-\><C-n>`, a tag jump, the alternate file).
///
/// Under the kitty keyboard protocol those bytes never arrive: the
/// terminal reports every `Ctrl` chord as `CSI u`, where `Ctrl`+`4` and
/// `Ctrl`+`\` are separate keys that keep their own names -- which is what
/// `kitty_kbd` distinguishes. It is the protocol view actually pushed, not
/// the terminal's advertised capability: a push that never happened leaves
/// the terminal sending legacy bytes whatever it can speak.
#[must_use]
pub fn encode_terminal_key(ev: &KeyEvent, kitty_kbd: bool) -> Option<String> {
    let mut ev = *ev;
    if !kitty_kbd && ev.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(typed) = ev.code {
            if let Some(&(_, chord)) = CROSSTERM_DIGIT_CHORDS
                .iter()
                .find(|(digit, _)| *digit == typed)
            {
                ev.code = KeyCode::Char(chord);
            }
        }
    }
    encode_key(&ev)
}

fn key_token(code: KeyCode) -> Option<(String, bool)> {
    Some(match code {
        KeyCode::Char('<') => ("lt".to_string(), true),
        KeyCode::Char(c) => (c.to_string(), false),
        KeyCode::Backspace => ("BS".to_string(), true),
        KeyCode::Enter => ("CR".to_string(), true),
        KeyCode::Esc => ("Esc".to_string(), true),
        KeyCode::Tab => ("Tab".to_string(), true),
        KeyCode::BackTab => ("Tab".to_string(), true),
        KeyCode::Up => ("Up".to_string(), true),
        KeyCode::Down => ("Down".to_string(), true),
        KeyCode::Left => ("Left".to_string(), true),
        KeyCode::Right => ("Right".to_string(), true),
        KeyCode::Home => ("Home".to_string(), true),
        KeyCode::End => ("End".to_string(), true),
        KeyCode::PageUp => ("PageUp".to_string(), true),
        KeyCode::PageDown => ("PageDown".to_string(), true),
        KeyCode::Delete => ("Del".to_string(), true),
        KeyCode::Insert => ("Insert".to_string(), true),
        KeyCode::F(n) => (format!("F{n}"), true),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn plain_chars_pass_through() {
        assert_eq!(
            encode_key(&key(KeyCode::Char('a'), KeyModifiers::NONE)).unwrap(),
            "a"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('A'), KeyModifiers::SHIFT)).unwrap(),
            "A"
        );
    }

    #[test]
    fn special_chars_nvim_escapes() {
        assert_eq!(
            encode_key(&key(KeyCode::Char('<'), KeyModifiers::NONE)).unwrap(),
            "<lt>"
        );
    }

    #[test]
    fn named_keys() {
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::NONE)).unwrap(),
            "<CR>"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Esc, KeyModifiers::NONE)).unwrap(),
            "<Esc>"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Backspace, KeyModifiers::NONE)).unwrap(),
            "<BS>"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Tab, KeyModifiers::NONE)).unwrap(),
            "<Tab>"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Up, KeyModifiers::NONE)).unwrap(),
            "<Up>"
        );
        assert_eq!(
            encode_key(&key(KeyCode::F(5), KeyModifiers::NONE)).unwrap(),
            "<F5>"
        );
    }

    #[test]
    fn modifier_wrapping() {
        assert_eq!(
            encode_key(&key(KeyCode::Char('x'), KeyModifiers::CONTROL)).unwrap(),
            "<C-x>"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('x'), KeyModifiers::ALT)).unwrap(),
            "<M-x>"
        );
        assert_eq!(
            encode_key(&key(
                KeyCode::Enter,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ))
            .unwrap(),
            "<C-S-CR>"
        );
    }

    #[test]
    fn backtab_maps_to_shift_tab() {
        assert_eq!(
            encode_key(&key(KeyCode::BackTab, KeyModifiers::NONE)).unwrap(),
            "<S-Tab>"
        );
    }

    #[test]
    fn backtab_with_shift_bit_does_not_double_wrap() {
        assert_eq!(
            encode_key(&key(KeyCode::BackTab, KeyModifiers::SHIFT)).unwrap(),
            "<S-Tab>"
        );
    }

    /// The producing half of the sidebar resize keys: `update`'s routing
    /// answers exactly these two spellings, so a change here that stopped
    /// wrapping shift onto an arrow would leave the whole feature
    /// unreachable with every one of its own tests still green.
    #[test]
    fn shift_arrows_encode_as_the_sidebar_resize_notations() {
        assert_eq!(
            encode_key(&key(KeyCode::Left, KeyModifiers::SHIFT)).unwrap(),
            "<S-Left>"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Right, KeyModifiers::SHIFT)).unwrap(),
            "<S-Right>"
        );
    }

    #[test]
    fn ctrl_space_uses_named_token() {
        assert_eq!(
            encode_key(&key(KeyCode::Char(' '), KeyModifiers::CONTROL)).unwrap(),
            "<C-Space>"
        );
    }

    #[test]
    fn alt_space_uses_named_token() {
        assert_eq!(
            encode_key(&key(KeyCode::Char(' '), KeyModifiers::ALT)).unwrap(),
            "<M-Space>"
        );
    }

    #[test]
    fn plain_space_stays_literal() {
        assert_eq!(
            encode_key(&key(KeyCode::Char(' '), KeyModifiers::NONE)).unwrap(),
            " "
        );
    }

    #[test]
    fn residue_printable_ascii_passes_through_as_literal_chars() {
        assert_eq!(
            encode_residue_bytes(b"itypeahead-marker"),
            vec![
                "i", "t", "y", "p", "e", "a", "h", "e", "a", "d", "-", "m", "a", "r", "k", "e", "r"
            ]
        );
    }

    #[test]
    fn residue_lt_byte_is_escaped_like_encode_key_does() {
        assert_eq!(encode_residue_bytes(b"<"), vec!["<lt>"]);
    }

    #[test]
    fn residue_cr_lf_tab_backspace_map_to_named_tokens() {
        assert_eq!(encode_residue_bytes(b"\r"), vec!["<CR>"]);
        // raw mode: `Enter` is `\r`, and `\n` is the chord that sends it
        assert_eq!(encode_residue_bytes(b"\n"), vec!["<C-j>"]);
        assert_eq!(encode_residue_bytes(b"\t"), vec!["<Tab>"]);
        assert_eq!(encode_residue_bytes(b"\x7f"), vec!["<BS>"]);
    }

    #[test]
    fn residue_control_bytes_are_the_chords_they_spell() {
        // dropped, every `<C-...>` mapping a user has is dead for the
        // window; typed as their bytes they are not keys at all
        assert_eq!(encode_residue_bytes(b"\x17"), vec!["<C-w>"]);
        assert_eq!(encode_residue_bytes(b"\x04"), vec!["<C-d>"]);
        assert_eq!(encode_residue_bytes(b"\x12"), vec!["<C-r>"]);
        // `Ctrl`+`h` and `Backspace` are different keys and different
        // bytes, as crossterm also reports them
        assert_eq!(encode_residue_bytes(b"\x08"), vec!["<C-h>"]);
        assert_eq!(encode_residue_bytes(b"\x00"), vec!["<C-Space>"]);
        // the four nvim names after punctuation and crossterm after
        // digits: `<C-\><C-n>` is the documented way out of terminal mode
        // and `<C-4>` is a key no vim user has ever bound
        assert_eq!(encode_residue_bytes(b"\x1c"), vec!["<C-\\>"]);
        assert_eq!(encode_residue_bytes(b"\x1f"), vec!["<C-_>"]);
        // and with `ESC` in front, the same chord with Alt held
        assert_eq!(encode_residue_bytes(b"\x1b\x17"), vec!["<C-M-w>"]);
    }

    /// Every byte a terminal with no keyboard protocol in force can send
    /// outside a sequence, with the name the pinned engine's own input
    /// layer gives it: nvim v0.12.4 typed each of these at its tty and
    /// answered `keytrans(getcharstr())` with the name in the row.
    ///
    /// Two rows spell a name nvim writes differently and resolves
    /// identically: it answers `<NL>` for `0x0a` and an uppercase `<C-A>`
    /// for each letter, and `nvim_replace_termcodes` maps `<NL>`/`<C-j>`
    /// both to `0x0a` and `<C-A>`/`<C-a>` both to `0x01`. The lowercase
    /// spelling is the one view's own `[keys]` notation is written in.
    const NVIM_CONTROL_BYTE_NAMES: &[(u8, &str)] = &[
        (0x00, "<C-Space>"),
        (0x01, "<C-a>"),
        (0x02, "<C-b>"),
        (0x03, "<C-c>"),
        (0x04, "<C-d>"),
        (0x05, "<C-e>"),
        (0x06, "<C-f>"),
        (0x07, "<C-g>"),
        (0x08, "<C-h>"),
        (0x09, "<Tab>"),
        (0x0a, "<C-j>"),
        (0x0b, "<C-k>"),
        (0x0c, "<C-l>"),
        (0x0d, "<CR>"),
        (0x0e, "<C-n>"),
        (0x0f, "<C-o>"),
        (0x10, "<C-p>"),
        (0x11, "<C-q>"),
        (0x12, "<C-r>"),
        (0x13, "<C-s>"),
        (0x14, "<C-t>"),
        (0x15, "<C-u>"),
        (0x16, "<C-v>"),
        (0x17, "<C-w>"),
        (0x18, "<C-x>"),
        (0x19, "<C-y>"),
        (0x1a, "<C-z>"),
        (0x1b, "<Esc>"),
        (0x1c, "<C-\\>"),
        (0x1d, "<C-]>"),
        (0x1e, "<C-^>"),
        (0x1f, "<C-_>"),
        (0x7f, "<BS>"),
    ];

    /// The whole legacy byte table walked by value, so a byte that reaches
    /// nvim under a name nvim never gave it fails naming itself rather
    /// than waiting for a user to press it.
    #[test]
    fn every_legacy_control_byte_carries_the_name_nvim_gives_it() {
        for byte in (0x00..=0x1f_u8).chain(std::iter::once(0x7f)) {
            let row = NVIM_CONTROL_BYTE_NAMES.iter().find(|(row, _)| *row == byte);
            assert!(
                row.is_some(),
                "byte 0x{byte:02x} has no row in the table read off the pinned engine"
            );
            let name = row.unwrap().1;
            assert_eq!(
                encode_residue_bytes(&[byte]),
                vec![name.to_string()],
                "byte 0x{byte:02x}"
            );
        }
        assert_eq!(
            NVIM_CONTROL_BYTE_NAMES.len(),
            33,
            "the table is 0x00..=0x1f and 0x7f exactly: a duplicated row hides the byte it \
             shadows from the walk above"
        );
    }

    /// crossterm reads `0x1c..=0x1f` as `Ctrl` with a digit; nvim reads
    /// the same bytes as the four punctuation chords, and a mapping is
    /// written against nvim's name.
    #[test]
    fn a_legacy_terminals_ctrl_digits_are_the_chords_nvim_names() {
        for (digit, nvim) in [
            ('4', "<C-\\>"),
            ('5', "<C-]>"),
            ('6', "<C-^>"),
            ('7', "<C-_>"),
        ] {
            let ev = key(KeyCode::Char(digit), KeyModifiers::CONTROL);
            assert_eq!(encode_terminal_key(&ev, false).unwrap(), nvim);
        }
    }

    /// With the protocol pushed those bytes never arrive and the digits a
    /// terminal reports are the digit keys it says they are -- except
    /// `Ctrl`+`6`, which nvim's own input layer answers as `<C-^>` in
    /// either encoding.
    #[test]
    fn a_protocol_terminals_ctrl_digits_keep_their_own_name() {
        for (digit, nvim) in [
            ('4', "<C-4>"),
            ('5', "<C-5>"),
            ('6', "<C-^>"),
            ('7', "<C-7>"),
        ] {
            let ev = key(KeyCode::Char(digit), KeyModifiers::CONTROL);
            assert_eq!(encode_terminal_key(&ev, true).unwrap(), nvim);
        }
    }

    /// The protocol's own decode, which reports the chord's character
    /// rather than a C0 byte: the four punctuation chords arrive under
    /// their own names with no repair, and `Ctrl`+`4` is never rewritten
    /// into one of them.
    #[test]
    fn a_protocol_terminals_ctrl_chords_are_decoded_as_the_keys_reported() {
        for (report, nvim) in [
            (&b"\x1b[92;5u"[..], "<C-\\>"),
            (b"\x1b[93;5u", "<C-]>"),
            (b"\x1b[94;5u", "<C-^>"),
            (b"\x1b[95;5u", "<C-_>"),
            (b"\x1b[52;5u", "<C-4>"),
            (b"\x1b[53;5u", "<C-5>"),
            (b"\x1b[55;5u", "<C-7>"),
        ] {
            assert_eq!(encode_residue_bytes(report), vec![nvim.to_string()]);
        }
    }

    /// The chords nvim folds onto another character's C0 byte, which only
    /// the protocol ever reports as characters at all.
    #[test]
    fn a_protocol_terminals_folded_chords_reach_nvim_folded() {
        for (report, nvim) in [
            (&b"\x1b[54;5u"[..], "<C-^>"),
            (b"\x1b[96;5u", "<C-@>"),
            (b"\x1b[123;5u", "<C-[>"),
            (b"\x1b[124;5u", "<C-\\>"),
            (b"\x1b[125;5u", "<C-]>"),
            (b"\x1b[126;5u", "<C-^>"),
        ] {
            assert_eq!(encode_residue_bytes(report), vec![nvim.to_string()]);
        }
    }

    /// The Alt chord an `ESC` in front of anything spells, which is the
    /// engine's own reading of the pair (termkey recurses past the escape
    /// and ors `KEYMOD_ALT` into whatever it finds). A user whose mapping
    /// is written against the chord gets it; one with no such mapping gets
    /// nvim's own degrade back to `<Esc>` and the key, which is why an
    /// unmapped doubled `ESC` still opens as two Escape keys.
    #[test]
    fn residue_an_escape_in_front_of_a_key_is_that_key_with_alt() {
        assert_eq!(encode_residue_bytes(b"\x1b"), vec!["<Esc>"]);
        assert_eq!(encode_residue_bytes(b"\x1bx"), vec!["<M-x>"]);
        assert_eq!(encode_residue_bytes(b"\x1b\r"), vec!["<M-CR>"]);
        assert_eq!(encode_residue_bytes(b"\x1b<"), vec!["<M-lt>"]);
        // Alt holds over a multi-byte character as much as an ASCII one:
        // crossterm's parser recurses on the bytes behind the `ESC` and
        // ors the modifier into whatever they decode to
        assert_eq!(
            encode_residue_bytes("\x1b\u{e9}".as_bytes()),
            vec!["<M-\u{e9}>"]
        );
        // two `ESC`s in one run are the Escape key with Alt held, and the
        // sequence behind a doubled `ESC` is that sequence with Alt held
        assert_eq!(encode_residue_bytes(b"\x1b\x1b"), vec!["<M-Esc>"]);
        assert_eq!(encode_residue_bytes(b"\x1b\x1b[A"), vec!["<M-Up>"]);
        // Alt is held, not accumulated: a run of escapes is one chord over
        // the key at the end of it, however many of them the terminal sent
        assert_eq!(encode_residue_bytes(b"\x1b\x1bx"), vec!["<M-x>"]);
        assert_eq!(encode_residue_bytes(b"\x1b\x1b\x1b"), vec!["<M-Esc>"]);
        // and the `ESC` that ends a run is the Escape key: this entry
        // point has no later read to make a chord out of it
        assert_eq!(encode_residue_bytes(b"ok\x1b"), vec!["o", "k", "<Esc>"]);
    }

    #[test]
    fn residue_csi_and_ss3_arrows_decode_as_the_arrows_they_are() {
        // undecoded, the fragment forwards as literal "O","A" and, in
        // normal mode, opens a line and inserts text instead of moving the
        // cursor -- and dropping the buffer from the `ESC` costs every key
        // typed behind it
        assert_eq!(
            encode_residue_bytes(b"ok\x1b[Aon"),
            vec!["o", "k", "<Up>", "o", "n"]
        );
        assert_eq!(
            encode_residue_bytes(b"ok\x1bOAon"),
            vec!["o", "k", "<Up>", "o", "n"]
        );
    }

    #[test]
    fn residue_an_escape_run_no_table_names_is_consumed_whole() {
        // `ESC [ h` is a valid CSI (set mode) and equally an `<Esc>`, a `[`
        // and the first letter of typing. Crossterm reads it as the
        // sequence, types none of it, and reads on from the byte after --
        // so this does too, inside the window and outside it alike
        assert_eq!(
            encode_residue_bytes(b"\x1b[hello"),
            vec!["e", "l", "l", "o"]
        );
        assert_eq!(
            encode_residue_bytes(b"\x1bOhello"),
            vec!["e", "l", "l", "o"]
        );
    }

    #[test]
    fn residue_the_linux_consoles_own_function_keys_decode_as_function_keys() {
        // `CSI [ A`..`CSI [ E`: a second introducer where a parameter
        // would be, which is how the Linux console spells F1-F5
        assert_eq!(encode_residue_bytes(b"\x1b[[A"), vec!["<F1>"]);
        assert_eq!(encode_residue_bytes(b"\x1b[[Eok"), vec!["<F5>", "o", "k"]);
        // one byte short of the letter is a tail to wait for
        assert_eq!(decode_residue(b"\x1b[[").unfinished, 3);
        // and a letter no key has is the sequence it still is
        assert!(encode_residue_bytes(b"\x1b[[Z").is_empty());
    }

    #[test]
    fn residue_modified_and_keypad_sequences_carry_their_modifiers() {
        assert_eq!(encode_residue_bytes(b"\x1b[1;5A"), vec!["<C-Up>"]);
        assert_eq!(encode_residue_bytes(b"\x1b[3~"), vec!["<Del>"]);
        assert_eq!(encode_residue_bytes(b"\x1b[6;2~"), vec!["<S-PageDown>"]);
        assert_eq!(encode_residue_bytes(b"\x1b[Z"), vec!["<S-Tab>"]);
    }

    #[test]
    fn residue_keyboard_protocol_forms_decode_like_crossterms_own_parser() {
        // the same bytes `kitty_key_decode`'s pty test drives through
        // crossterm: one terminal, two readers, one answer
        assert_eq!(encode_residue_bytes(b"\x1b[13;2u"), vec!["<S-CR>"]);
        assert_eq!(encode_residue_bytes(b"\x1b[105;5u"), vec!["<C-i>"]);
        assert_eq!(encode_residue_bytes(b"\x1b[27u"), vec!["<Esc>"]);
        // and the run is finished: an Escape the terminal spelled in full
        // must never sit in the pending tail waiting out an escape timeout
        // whose whole job is to resolve a run that stopped short
        assert_eq!(decode_residue(b"\x1b[27u").unfinished, 0);
        // xterm's modifyOtherKeys spelling of the same kind of key
        assert_eq!(encode_residue_bytes(b"\x1b[27;5;13~"), vec!["<C-CR>"]);
        // an alternate-key pair reports both the key and what it typed,
        // and the buffer gets what it typed
        assert_eq!(encode_residue_bytes(b"\x1b[97:65;2u"), vec!["A"]);
        // a release is not a keypress, and its event type says so
        assert!(encode_residue_bytes(b"\x1b[97;5:3u").is_empty());
        assert_eq!(encode_residue_bytes(b"\x1b[97;5:1u"), vec!["<C-a>"]);
        // the function-key rows above F12, which the same terminals send
        assert_eq!(encode_residue_bytes(b"\x1b[28~"), vec!["<F13>"]);
        assert_eq!(encode_residue_bytes(b"\x1b[34~"), vec!["<F17>"]);
    }

    /// The kitty protocol's other spelling of a functional key: a
    /// private-use codepoint in a `CSI u`, which crossterm names through
    /// `translate_functional_key_code` and which this table has to name
    /// the same way or a real keypress arrives as a glyph nobody has a key
    /// for.
    #[test]
    fn residue_kitty_functional_codepoints_are_the_keys_crossterm_names() {
        // the empty rows are the keys nvim has no notation for, which
        // crossterm's own events also end as nothing
        let named: [(u32, &[&str]); 15] = [
            (57358, &[]),        // CapsLock
            (57363, &[]),        // Menu
            (57376, &["<F13>"]), // the row `CSI 28 ~` also spells
            (57398, &["<F35>"]), // the last one nvim can name
            (57399, &["0"]),     // the keypad digits, as the digits
            (57408, &["9"]),     // they type
            (57411, &["*"]),
            (57414, &["<CR>"]),   // keypad Enter
            (57417, &["<Left>"]), // the keypad's own arrows
            (57422, &["<PageDown>"]),
            (57426, &["<Del>"]),
            (57427, &[]), // KeypadBegin
            (57430, &[]), // a media key
            (57441, &[]), // a bare modifier key
            (57454, &[]), // the last codepoint in the block
        ];
        for (code, expected) in named {
            assert_eq!(
                encode_residue_bytes(format!("\x1b[{code}u").as_bytes()),
                expected.to_vec(),
                "CSI {code} u"
            );
        }
        // a modifier rides one exactly as it rides any other key
        assert_eq!(encode_residue_bytes(b"\x1b[57376;5u"), vec!["<C-F13>"]);
        // and nothing in the block reaches the buffer as its glyph
        for code in FUNCTIONAL_FIRST..=FUNCTIONAL_LAST {
            let glyph = char::from_u32(code).map(|typed| typed.to_string());
            assert!(
                !encode_residue_bytes(format!("\x1b[{code}u").as_bytes())
                    .iter()
                    .any(|notation| Some(notation) == glyph.as_ref()),
                "CSI {code} u types the private-use glyph nobody pressed"
            );
        }
    }

    #[test]
    fn residue_a_terminal_report_is_consumed_rather_than_typed() {
        // `CSI R` answers "where is the cursor" and shares its final byte
        // with F3. An empty first parameter is what tells the two apart:
        // a position report always names a row, the modifier form never
        // does
        assert!(encode_residue_bytes(b"\x1b[1;40R").is_empty());
        // and a bare one is a report crossterm cannot parse and types
        // nothing from, rather than the F3 that final byte also spells
        assert!(encode_residue_bytes(b"\x1b[R").is_empty());
        // the one spelling that is the key: a leading `;`, which is the
        // modifier form and never a position
        assert_eq!(encode_residue_bytes(b"\x1b[;5R"), vec!["<C-F3>"]);
        // a literal `0` where the modifier form has nothing is a numbered
        // report, which crossterm reads as a position and types nothing
        // from -- the two are one value once parsed, so only the bytes
        // tell them apart
        assert!(encode_residue_bytes(b"\x1b[0R").is_empty());
        assert!(encode_residue_bytes(b"\x1b[0;5R").is_empty());
        // SS3 keeps its own F3, which crossterm names too
        assert_eq!(encode_residue_bytes(b"\x1bOR"), vec!["<F3>"]);
        for report in [
            // an SGR mouse press, which typed through is nine keystrokes
            b"\x1b[<0;24;10M".as_slice(),
            // focus in and out, which `event_to_msg` drops for crossterm's
            // own events too
            b"\x1b[I".as_slice(),
            b"\x1b[O".as_slice(),
            // a key no table here names, in a form that cannot be anything
            // but a sequence
            b"\x1b[3;2;9^".as_slice(),
        ] {
            assert!(
                encode_residue_bytes(report).is_empty(),
                "{report:?} must not reach the buffer"
            );
        }
        // and the keys behind one still do
        assert_eq!(encode_residue_bytes(b"\x1b[<0;24;10Mok"), vec!["o", "k"]);
    }

    #[test]
    fn residue_a_string_report_is_consumed_only_once_its_terminator_lands() {
        // DCS, OSC and APC answers: consumed whole, keys behind them kept
        for report in [
            b"\x1bP1$r0m\x1b\\ok".as_slice(),
            b"\x1b]11;rgb:1f1f/1f1f/1f1f\x07ok".as_slice(),
            b"\x1b_Gi=1;OK\x1b\\ok".as_slice(),
        ] {
            assert_eq!(
                encode_residue_bytes(report),
                vec!["o", "k"],
                "{report:?} must be consumed whole"
            );
        }
        // without a terminator the same two bytes are the Alt chord they
        // also are
        assert_eq!(encode_residue_bytes(b"\x1bP"), vec!["<M-P>"]);
        assert_eq!(encode_residue_bytes(b"\x1b]"), vec!["<M-]>"]);
    }

    #[test]
    fn residue_a_terminated_string_is_the_report_whatever_it_could_spell() {
        // the ceiling of the rule above, stated: these bytes are also
        // `Alt+]`, `1`, `2`, `Ctrl+G`, and in one read they are read as
        // the report. A terminal sending one unasked is ordinary; a
        // keyboard producing that run inside a single read is not, and
        // typing a report's body runs it as normal-mode commands
        assert!(encode_residue_bytes(b"\x1b]12\x07").is_empty());
    }

    /// Every modifier combination a legacy terminal can spell, as the
    /// parameter it spells it with: bit 1 shift, bit 2 alt, bit 4 ctrl,
    /// biased by one.
    fn modifier_params() -> impl Iterator<Item = (u32, KeyModifiers)> {
        (0..8).map(|bits| {
            let mut mods = KeyModifiers::empty();
            if bits & 0b001 != 0 {
                mods |= KeyModifiers::SHIFT;
            }
            if bits & 0b010 != 0 {
                mods |= KeyModifiers::ALT;
            }
            if bits & 0b100 != 0 {
                mods |= KeyModifiers::CONTROL;
            }
            (bits + 1, mods)
        })
    }

    /// What one decoded message reads as, for a corpus that has to name
    /// mouse reports and pastes as well as keys.
    fn rendered(msgs: Vec<Msg>) -> Vec<String> {
        msgs.into_iter()
            .map(|msg| match msg {
                Msg::Key(key) => key.notation,
                Msg::Paste(text) => format!("paste:{text}"),
                Msg::Mouse(m) => format!(
                    "mouse:{}:{}:{}:{}:{}",
                    m.button, m.action, m.modifier, m.row, m.col
                ),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// The whole population the unix read path took over from crossterm's
    /// parser, walked one byte run at a time.
    ///
    /// The expectations are written out by hand from that parser's own
    /// source (crossterm 0.29, `event/sys/unix/parse.rs`) rather than
    /// obtained from it: `parse_event` is private and crossterm's public
    /// reader is built once per process against the real terminal, so no
    /// test can put bytes of its own through it. What each run means is
    /// stated as the `(KeyCode, KeyModifiers)` that parser resolves it to,
    /// leaving the naming of that pair to [`encode_key`], which the tests
    /// above hold to the pinned engine's own answers.
    ///
    /// Four readings are deliberately not crossterm's, and each is pinned
    /// on its own above: an `ESC` in front of another `ESC` or a whole
    /// sequence is Alt over it rather than an Escape key of its own
    /// (`residue_an_escape_in_front_of_a_key_is_that_key_with_alt`), a
    /// trailing `ESC` waits for the byte that would make it a chord
    /// instead of being read at once (same pin), a CSI no table names is
    /// consumed rather than raising an error
    /// (`residue_an_escape_run_no_table_names_is_consumed_whole`), and a
    /// run whose final byte has not arrived is reported as a tail rather
    /// than held in a buffer with no timeout
    /// (`a_run_out_of_time_is_the_alt_chord_its_bytes_spell`). The first
    /// two are the engine's own reading of those bytes, which is the
    /// contract this decoder answers to.
    #[test]
    fn every_crossterm_key_event_decodes_identically() {
        let expected = |code, mods| vec![encode_key(&KeyEvent::new(code, mods)).unwrap()];
        // the cursor and edit keys in both of their spellings, at every
        // modifier a terminal can put on them
        for (final_byte, code) in [
            (b'A', KeyCode::Up),
            (b'B', KeyCode::Down),
            (b'C', KeyCode::Right),
            (b'D', KeyCode::Left),
            (b'H', KeyCode::Home),
            (b'F', KeyCode::End),
        ] {
            assert_eq!(
                encode_residue_bytes(&[0x1b, b'[', final_byte]),
                expected(code, KeyModifiers::NONE)
            );
            assert_eq!(
                encode_residue_bytes(&[0x1b, b'O', final_byte]),
                expected(code, KeyModifiers::NONE)
            );
            for (param, mods) in modifier_params() {
                let run = format!("\x1b[1;{param}{}", char::from(final_byte));
                assert_eq!(
                    encode_residue_bytes(run.as_bytes()),
                    expected(code, mods),
                    "{run:?}"
                );
            }
        }
        // the keypad and function keys, in the `~` form and the modifier
        // parameter crossterm reads off the same field
        for (param, code) in [
            (2, KeyCode::Insert),
            (3, KeyCode::Delete),
            (5, KeyCode::PageUp),
            (6, KeyCode::PageDown),
            (11, KeyCode::F(1)),
            (12, KeyCode::F(2)),
            (13, KeyCode::F(3)),
            (14, KeyCode::F(4)),
            (15, KeyCode::F(5)),
            (17, KeyCode::F(6)),
            (18, KeyCode::F(7)),
            (19, KeyCode::F(8)),
            (20, KeyCode::F(9)),
            (21, KeyCode::F(10)),
            (23, KeyCode::F(11)),
            (24, KeyCode::F(12)),
        ] {
            assert_eq!(
                encode_residue_bytes(format!("\x1b[{param}~").as_bytes()),
                expected(code, KeyModifiers::NONE)
            );
            for (modifier, mods) in modifier_params() {
                assert_eq!(
                    encode_residue_bytes(format!("\x1b[{param};{modifier}~").as_bytes()),
                    expected(code, mods),
                    "CSI {param};{modifier}~"
                );
            }
        }
        // F1-F4's SS3 spelling, the one a terminal sends unmodified
        for (final_byte, code) in [
            (b'P', KeyCode::F(1)),
            (b'Q', KeyCode::F(2)),
            (b'R', KeyCode::F(3)),
            (b'S', KeyCode::F(4)),
        ] {
            assert_eq!(
                encode_residue_bytes(&[0x1b, b'O', final_byte]),
                expected(code, KeyModifiers::NONE)
            );
        }
        // `ESC` and a printable is that key with Alt held, which is how
        // crossterm's parser reads the pair (it recurses on the bytes
        // behind the `ESC` and ors the modifier in)
        for byte in b' '..=b'~' {
            let typed = char::from(byte);
            // except the two bytes an escape sequence opens with, which
            // are that opening until the read that finishes it
            if typed == '[' || typed == 'O' {
                continue;
            }
            assert_eq!(
                encode_residue_bytes(&[0x1b, byte]),
                expected(KeyCode::Char(typed), KeyModifiers::ALT),
                "ESC {typed}"
            );
        }
        // every C0 byte is one key and exactly one, whatever it is named
        for byte in 0x00..=0x1f_u8 {
            assert_eq!(
                encode_residue_bytes(&[byte]).len(),
                1,
                "the byte {byte:#04x} decodes to one key"
            );
        }
        // the kitty protocol's own form for the same keys
        for (code, key) in [
            (13, KeyCode::Enter),
            (27, KeyCode::Esc),
            (9, KeyCode::Tab),
            (127, KeyCode::Backspace),
            (97, KeyCode::Char('a')),
        ] {
            for (param, mods) in modifier_params() {
                assert_eq!(
                    encode_residue_bytes(format!("\x1b[{code};{param}u").as_bytes()),
                    expected(key, mods),
                    "CSI {code};{param}u"
                );
            }
        }
        // focus in and out reach no key at all, in either decoder: typed
        // through they would open a line or insert at the line start
        assert!(encode_residue_bytes(b"\x1b[I").is_empty());
        assert!(encode_residue_bytes(b"\x1b[O").is_empty());
        // a paste is one message however much of an escape sequence its
        // body spells
        assert_eq!(
            rendered(decode_residue(b"\x1b[200~a\x1b[Ab\x1b[201~").msgs),
            vec!["paste:a\x1b[Ab"]
        );
    }

    /// The mouse forms are the half of the population no key notation can
    /// carry, so they are walked against the [`MouseInput`] they encode to
    /// rather than against a notation.
    ///
    /// Same provenance as the corpus above: read off crossterm's `parse_cb`
    /// and its three mouse parsers by hand.
    #[test]
    fn every_crossterm_mouse_report_decodes_identically() {
        // SGR (`?1006h`), the form a modern terminal answers with: press,
        // drag, release, and each wheel direction, at column 10 row 5 on
        // the wire and one less than that in every cell coordinate
        for (bytes, expected) in [
            (b"\x1b[<0;10;5M".as_slice(), "mouse:left:press::4:9"),
            (b"\x1b[<1;10;5M".as_slice(), "mouse:middle:press::4:9"),
            (b"\x1b[<2;10;5M".as_slice(), "mouse:right:press::4:9"),
            (b"\x1b[<0;10;5m".as_slice(), "mouse:left:release::4:9"),
            (b"\x1b[<32;10;5M".as_slice(), "mouse:left:drag::4:9"),
            (b"\x1b[<35;10;5M".as_slice(), "mouse:move:move::4:9"),
            (b"\x1b[<64;10;5M".as_slice(), "mouse:wheel:up::4:9"),
            (b"\x1b[<65;10;5M".as_slice(), "mouse:wheel:down::4:9"),
            (b"\x1b[<66;10;5M".as_slice(), "mouse:wheel:left::4:9"),
            (b"\x1b[<67;10;5M".as_slice(), "mouse:wheel:right::4:9"),
            // the modifier bits ride in the same field as the button
            (b"\x1b[<4;10;5M".as_slice(), "mouse:left:press:S-:4:9"),
            (b"\x1b[<8;10;5M".as_slice(), "mouse:left:press:M-:4:9"),
            (b"\x1b[<16;10;5M".as_slice(), "mouse:left:press:C-:4:9"),
            (b"\x1b[<28;10;5M".as_slice(), "mouse:left:press:C-S-M-:4:9"),
        ] {
            assert_eq!(
                rendered(decode_residue(bytes).msgs),
                vec![expected.to_owned()],
                "{bytes:?}"
            );
        }
        // X10's form, where the button and both coordinates are three raw
        // bytes behind the final rather than parameters -- and so the one
        // form of the three that a read boundary can cut in half
        assert_eq!(
            rendered(decode_residue(b"\x1b[M\x20\x2a\x25").msgs),
            vec!["mouse:left:press::4:9"]
        );
        assert_eq!(decode_residue(b"\x1b[M\x20\x2a").unfinished, 5);
        // rxvt's (`?1015h`), the same parameters without the `<` and with
        // the button carrying X10's bias
        assert_eq!(
            rendered(decode_residue(b"\x1b[32;10;5M").msgs),
            vec!["mouse:left:press::4:9"]
        );
        // a button this vocabulary does not name consumes its report
        // rather than typing it
        assert!(decode_residue(b"\x1b[<200;10;5M").msgs.is_empty());
        // and the keys behind a report are the keys they were
        assert_eq!(
            rendered(decode_residue(b"\x1b[<0;10;5Mok").msgs),
            vec!["mouse:left:press::4:9", "o", "k"]
        );
    }

    #[test]
    fn a_run_out_of_time_is_the_alt_chord_its_bytes_spell() {
        // the engine's own `ttimeoutlen` flush: what a terminal never
        // finished sending is read as Alt over the byte that failed to
        // open a sequence, with the parameters behind it typed as the
        // characters they are, so a user who presses Escape and nothing
        // else still gets a key rather than a decoder waiting forever
        let notations = |run: &[u8]| {
            decode_residue_forced(run)
                .into_iter()
                .filter_map(|msg| match msg {
                    Msg::Key(key) => Some(key.notation),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(notations(b"\x1b["), vec!["<M-[>"]);
        assert_eq!(notations(b"\x1bO"), vec!["<M-O>"]);
        assert_eq!(notations(b"\x1b[1;5"), vec!["<M-[>", "1", ";", "5"]);
        // an `ESC` with nothing behind it is the Escape key, and the
        // escapes in front of one are Alt over it
        assert_eq!(notations(b"\x1b"), vec!["<Esc>"]);
        assert_eq!(notations(b"\x1b\x1b"), vec!["<M-Esc>"]);
        assert_eq!(notations(b"\x1b\x1b\x1b"), vec!["<M-Esc>"]);
        // the keys in front of a half-arrived run are the keys they were
        assert_eq!(notations(b"ok\x1b["), vec!["o", "k", "<M-[>"]);
        // and a finished run is decoded exactly as it is without this
        assert_eq!(notations(b"\x1b[A"), vec!["<Up>"]);
    }

    #[test]
    fn only_an_escape_run_is_ever_flushed_by_a_timeout() {
        assert!(forceable(b"\x1b["), "a bare CSI introducer waits on a key");
        assert!(forceable(b"\x1bO"));
        // a paste's closer arrives when the pasted text ends, which no
        // keystroke timeout bounds
        assert!(!forceable(b"\x1b[200~half a paste"));
        // and half a character is not an escape run at all
        assert!(!forceable(&"\u{e9}".as_bytes()[..1]));
        assert!(!forceable(b""));
        // half a paste flushed anyway would type its body as normal-mode
        // commands, so the forced decode drops it rather than splitting it
        assert!(decode_residue_forced(b"\x1b[200~rm -rf").is_empty());
    }

    #[test]
    fn residue_a_bracketed_paste_arrives_as_one_paste_not_as_keystrokes() {
        let decoded = decode_residue(b"\x1b[200~two words\x1b[201~a");
        assert_eq!(decoded.unfinished, 0);
        assert!(
            matches!(
                decoded.msgs.as_slice(),
                [Msg::Paste(text), Msg::Key(key)]
                    if text == "two words" && key.notation == "a"
            ),
            "{:?}",
            decoded.msgs
        );
    }

    #[test]
    fn residue_reports_an_unfinished_tail_rather_than_decoding_half_of_it() {
        // what the late-reply guard keeps back: each of these is one read
        // short of a keypress, and the bytes in front of it are not
        for run in [
            b"ok\x1b[".as_slice(),
            b"ok\x1bO".as_slice(),
            b"ok\x1b[1;5".as_slice(),
            b"ok\x1b[200~half".as_slice(),
        ] {
            let decoded = decode_residue(run);
            assert_eq!(
                decoded.unfinished,
                run.len() - 2,
                "the whole fragment must be reported for {run:?}"
            );
        }
        // the guard's own entry point has no later read to wait for, so an
        // escape run it holds is forced rather than dropped -- a paste is
        // the one shape that is dropped instead
        assert_eq!(encode_residue_bytes(b"ok\x1b["), vec!["o", "k", "<M-[>"]);
        assert_eq!(encode_residue_bytes(b"ok\x1b[200~half"), vec!["o", "k"]);
        // half a multi-byte character is a tail too: its second byte is in
        // the read that has not happened yet, and dropped now it would
        // reach that read as bytes no character encodes to
        let split = &"ok\u{e9}".as_bytes()[..3];
        assert_eq!(decode_residue(split).unfinished, 1);
        assert_eq!(encode_residue_bytes(split), vec!["o", "k"]);
        // including behind an `ESC`, where the whole chord is the tail
        assert_eq!(decode_residue(&"\x1b\u{e9}".as_bytes()[..2]).unfinished, 2);
        // a bare `ESC` is a tail too: the byte that would make it a chord
        // may be in the read that has not happened yet, which is the wait
        // `ttimeoutlen` bounds
        assert_eq!(decode_residue(b"ok\x1b").unfinished, 1);
    }

    #[test]
    fn residue_valid_utf8_multibyte_run_decodes_char_by_char() {
        assert_eq!(
            encode_residue_bytes("caf\u{e9}".as_bytes()),
            vec!["c", "a", "f", "\u{e9}"]
        );
    }

    #[test]
    fn residue_invalid_utf8_tail_is_dropped_not_guessed_at() {
        // a lone continuation byte with no valid leading byte: not valid
        // UTF-8 on its own, so it must be dropped rather than produce a
        // replacement character or panic
        let mut bytes = b"ok".to_vec();
        bytes.push(0x80);
        assert_eq!(encode_residue_bytes(&bytes), vec!["o", "k"]);
    }

    #[test]
    fn residue_control_bytes_keep_their_place_among_the_letters() {
        assert_eq!(
            encode_residue_bytes(b"a\x00b\x07c"),
            vec!["a", "<C-Space>", "b", "<C-g>", "c"]
        );
    }

    #[test]
    fn residue_empty_input_yields_empty_output() {
        assert!(encode_residue_bytes(b"").is_empty());
    }

    /// The composer's default line break, welded to the encoder that has to
    /// produce it: a modifier held on Enter is the whole binding, so the
    /// notation this table spells it with is the notation `view-core` is
    /// waiting for. Alt reaches this build from nearly every terminal;
    /// Shift only where a keyboard protocol reports it, which is why both
    /// are bound and why the unmodified key still sends the prompt.
    #[test]
    fn a_modified_enter_is_the_notation_the_composers_line_break_is_bound_to() {
        use view_core::native::keys::{Action, KeyBindings, Resolved};

        for m in [KeyModifiers::ALT, KeyModifiers::SHIFT] {
            let notation = encode_key(&key(KeyCode::Enter, m)).expect("Enter encodes");
            assert_eq!(
                KeyBindings::default().resolve(None, &notation),
                Some(Resolved::Act(Action::ComposerNewline)),
                "{notation} is what a held modifier on Enter arrives as"
            );
        }
        let plain = encode_key(&key(KeyCode::Enter, KeyModifiers::NONE)).expect("Enter encodes");
        assert_eq!(
            KeyBindings::default().resolve(None, &plain),
            None,
            "and {plain} is left to the composer's own submit"
        );
    }

    /// `[keys]` is checked for shape in `view-core`, which cannot depend on
    /// this crate and so restates the vocabulary this encoder emits. That
    /// restatement is welded here: every notation a real keystroke can
    /// produce is fed back through the check, so a named key added above
    /// and forgotten there fails as a test rather than as a user's binding
    /// being refused for looking malformed.
    #[test]
    fn every_notation_this_encoder_emits_is_a_key_view_core_accepts() {
        use view_core::native::keys::{Action, Direction, KeyBindings};

        let named = [
            KeyCode::Backspace,
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::F(1),
            KeyCode::F(12),
        ];
        // every character a terminal can report rather than a sample of
        // them, so a spelling the Ctrl fold maps onto -- or away from --
        // cannot be missed the way `\`, `]`, `^` and `_` were. It subsumes
        // the legacy bytes too: every code `plain_key` gives one is either
        // a named key above or a character in this range
        let printable = (0x20..=0x7e_u8).map(|byte| KeyCode::Char(byte as char));
        let codes = named.into_iter().chain(printable);
        let mods = [
            KeyModifiers::NONE,
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT | KeyModifiers::ALT,
        ];
        for code in codes {
            for m in mods {
                let Some(notation) = encode_key(&key(code, m)) else {
                    continue;
                };
                assert!(
                    KeyBindings::default().rebind(
                        Action::Resize(Direction::Wider),
                        std::slice::from_ref(&notation)
                    ),
                    "view-core refuses {notation}, which this encoder emits"
                );
            }
        }
    }
}
