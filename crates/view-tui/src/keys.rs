//! Crossterm key event to nvim input notation encoding.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
/// `view-oracle`'s compat harness (`crates/view-oracle/src/compat.rs`'s
/// `resolve_key_token`) maintains its own independent, hardcoded inverse of
/// this table to type notation into a pty as real keypress bytes;
/// dependency direction keeps the two crates from sharing the table
/// directly, so a change here that is not mirrored there would silently
/// desync what a compat scenario types from what this encoder actually
/// forwards, undetected by either crate's own tests.
#[must_use]
pub fn encode_key(ev: &KeyEvent) -> Option<String> {
    if ev.kind == KeyEventKind::Release {
        return None;
    }

    let (mut bare, always_bracketed) = key_token(ev.code)?;
    let is_plain_char = matches!(ev.code, KeyCode::Char(c) if c != '<');
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
    if wrap && ev.code == KeyCode::Char(' ') {
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
/// This is not a general terminal-input decoder: it only has to cover what
/// a person can type inside the probe's ~50ms window, not the full grammar
/// [`crossterm::event::read`] already owns for every keystroke after
/// startup. Printable ASCII passes through as literal characters (matching
/// [`encode_key`]'s plain-char case, including `<` becoming `<lt>`);
/// `\r`/`\n` map to `<CR>`, `\t` to `<Tab>`, backspace (`0x08`/`0x7f`) to
/// `<BS>`. A run of continuous non-ASCII bytes (`0x80..`) is decoded as one
/// UTF-8 str and forwarded char-by-char if valid, dropped if not (a
/// mid-codepoint chunk boundary can produce invalid UTF-8 here, and
/// guessing at a replacement is worse than dropping a still-rare case).
///
/// `ESC` immediately followed by `[` or `O` opens a CSI or SS3 sequence --
/// an arrow, a function key, a keypad key, a keyboard-protocol report (SS3
/// is what a DECCKM application-cursor mode inherited from the spawning
/// shell encodes arrows as, e.g. `ESC O A` for Up). One the tables below
/// name decodes into the key it is, through the same [`encode_key`] every
/// keystroke after startup goes through.
///
/// One they do not splits by whether it carries parameters. A run with
/// them is the terminal reporting something (a mouse position, a focus
/// change, a cursor position, a key this table is too old for) and is
/// consumed whole, exactly as crossterm's parser consumes what it cannot
/// name: typing `<lt>0;24;10M` out of an SGR mouse report is nine
/// keystrokes nobody pressed. A run without them is the other reading of
/// those two bytes -- an `<Esc>` and then someone typing -- so only the
/// introducer goes and the keys behind it survive. Both are better than
/// dropping the buffer from the `ESC` onward, which costs every key typed
/// behind two bytes that were never a sequence.
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
/// `ESC` and a printable in the same run are Alt and that key, which is
/// how crossterm reads them for the rest of the session. A lone trailing
/// `ESC` is the Escape key and maps to `<Esc>`.
#[must_use]
pub fn encode_residue_bytes(residue: &[u8]) -> Vec<String> {
    decode_residue(residue)
        .msgs
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
    pub(crate) unfinished: usize,
}

/// [`encode_residue_bytes`]'s decode, in full: the messages and the tail
/// that only the bytes still in flight can resolve.
pub(crate) fn decode_residue(residue: &[u8]) -> ResidueDecode {
    let mut msgs = Vec::new();
    let mut i = 0;
    while i < residue.len() {
        match residue[i] {
            0x1b if matches!(residue.get(i + 1), Some(&b'[') | Some(&b'O')) => {
                match escape_sequence(&residue[i..]) {
                    Escape::Decoded { len, msg } => {
                        msgs.extend(msg);
                        i += len;
                    }
                    Escape::Unknown { len, parameterised } => {
                        i += if parameterised {
                            len
                        } else {
                            ESCAPE_INTRODUCER_LEN
                        };
                    }
                    Escape::Unfinished => {
                        return ResidueDecode {
                            msgs,
                            unfinished: residue.len() - i,
                        }
                    }
                }
            }
            0x1b => {
                if let Some(len) = string_sequence_len(&residue[i..]) {
                    i += len;
                } else if let Some((len, msg)) = alt_key(&residue[i..]) {
                    msgs.extend(msg);
                    i += len;
                } else {
                    msgs.push(key_msg("<Esc>"));
                    i += 1;
                }
            }
            b'\r' | b'\n' => {
                msgs.push(key_msg("<CR>"));
                i += 1;
            }
            b'\t' => {
                msgs.push(key_msg("<Tab>"));
                i += 1;
            }
            0x7f | 0x08 => {
                msgs.push(key_msg("<BS>"));
                i += 1;
            }
            b'<' => {
                msgs.push(key_msg("<lt>"));
                i += 1;
            }
            b if (0x20..=0x7e).contains(&b) => {
                msgs.push(key_msg((b as char).to_string()));
                i += 1;
            }
            b if b >= 0x80 => {
                let start = i;
                i += 1;
                while residue.get(i).is_some_and(|&b| b >= 0x80) {
                    i += 1;
                }
                if let Ok(s) = std::str::from_utf8(&residue[start..i]) {
                    msgs.extend(s.chars().map(|c| key_msg(c.to_string())));
                }
            }
            // any other control byte (bell, null, ...) has no sensible nvim
            // notation and is dropped rather than guessed at
            _ => i += 1,
        }
    }
    ResidueDecode {
        msgs,
        unfinished: 0,
    }
}

/// `ESC [` and `ESC O`: the two bytes that are equally the opening of a
/// sequence and of nothing, and so the only ones ever dropped alone.
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
const PASTE_CLOSE: &[u8] = b"\x1b[201~";

/// How the residue decoder reads one `ESC [` / `ESC O` run.
enum Escape {
    /// A complete sequence `len` bytes long, and the message it is (`None`
    /// for one with no nvim equivalent, which is consumed all the same).
    Decoded { len: usize, msg: Option<Msg> },
    /// Complete, and nothing this decoder's tables name. `parameterised`
    /// is what tells a terminal's report (consume all `len` bytes) from an
    /// `<Esc>` with typing behind it (consume the introducer only).
    Unknown { len: usize, parameterised: bool },
    /// Its final byte, or its paste closer, has not arrived.
    Unfinished,
}

fn escape_sequence(run: &[u8]) -> Escape {
    if run.get(1) == Some(&b'O') {
        // SS3 is exactly three bytes: introducer and one final
        let Some(&final_byte) = run.get(2) else {
            return Escape::Unfinished;
        };
        return match cursor_key(final_byte) {
            Some(code) => decoded(3, code, KeyModifiers::NONE),
            // SS3 carries no parameters, so an unnamed one reads as the
            // `<Esc>` `O` it equally is
            None => Escape::Unknown {
                len: 3,
                parameterised: false,
            },
        };
    }
    csi_sequence(run)
}

fn csi_sequence(run: &[u8]) -> Escape {
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
    let modifier = modifiers(field(&fields, 1));
    let unnamed = Escape::Unknown {
        len,
        parameterised: !fields.is_empty(),
    };
    match final_byte {
        b'~' => match tilde_key(&fields) {
            Some((code, mods)) => decoded(len, code, mods),
            None => unnamed,
        },
        // the kitty keyboard protocol's own form, which a terminal keeps
        // speaking for as long as some earlier process left it pushed. Its
        // modifier field carries the event type as a sub-parameter, and a
        // release is not a keypress
        b'u' => match kitty_key(&fields) {
            Some((code, mods)) => Escape::Decoded {
                len,
                msg: encode_key(&KeyEvent::new_with_kind(code, mods, event_kind(&fields)))
                    .map(key_msg),
            },
            None => unnamed,
        },
        // a focus report, which `event_to_msg` also drops for crossterm's
        // own events -- and which typed through would insert at the line
        // start or open a line above
        b'I' | b'O' if fields.is_empty() => Escape::Decoded { len, msg: None },
        // `CSI R` with parameters is a cursor-position report; without
        // them it is the F3 this table and crossterm's both name
        b'R' if !fields.is_empty() => Escape::Decoded { len, msg: None },
        _ => match cursor_key(final_byte) {
            Some(code) => decoded(len, code, modifier),
            None => unnamed,
        },
    }
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
/// unfinished sequence on purpose: `ESC P` and `ESC ]` are Alt+Shift+P and
/// Alt+`]`, and holding every one of them against a string that may never
/// come would cost a keypress to save a report.
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
/// Alt held (its parser recurses on the byte after the `ESC` and ors the
/// modifier in), so a `<M-...>` mapping resolves the same inside this
/// window as outside it. `None` for a run that is a lone `ESC`, or one
/// whose next byte no single key produces.
fn alt_key(run: &[u8]) -> Option<(usize, Option<Msg>)> {
    let code = match *run.get(1)? {
        b'\r' | b'\n' => KeyCode::Enter,
        b'\t' => KeyCode::Tab,
        0x7f | 0x08 => KeyCode::Backspace,
        byte if (0x20..=0x7e).contains(&byte) => KeyCode::Char(byte as char),
        _ => return None,
    };
    Some((
        2,
        encode_key(&KeyEvent::new(code, KeyModifiers::ALT)).map(key_msg),
    ))
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
        _ => KeyCode::Char(char::from_u32(code)?),
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
        assert_eq!(encode_residue_bytes(b"\n"), vec!["<CR>"]);
        assert_eq!(encode_residue_bytes(b"\t"), vec!["<Tab>"]);
        assert_eq!(encode_residue_bytes(b"\x7f"), vec!["<BS>"]);
        assert_eq!(encode_residue_bytes(b"\x08"), vec!["<BS>"]);
    }

    #[test]
    fn residue_bare_esc_not_followed_by_bracket_maps_to_esc_token() {
        assert_eq!(encode_residue_bytes(b"\x1b"), vec!["<Esc>"]);
        // `ESC` and a key in one run is that key with Alt held, which is
        // how crossterm reads the pair everywhere else in the session
        assert_eq!(encode_residue_bytes(b"\x1bx"), vec!["<M-x>"]);
        assert_eq!(encode_residue_bytes(b"\x1b\r"), vec!["<M-CR>"]);
        assert_eq!(encode_residue_bytes(b"\x1b<"), vec!["<M-lt>"]);
        // and the `ESC` that ends a run is still the Escape key
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
    fn residue_an_escape_run_no_table_names_costs_its_introducer_and_no_more() {
        // `ESC [ h` is a valid CSI (set mode) and equally an `<Esc>`, a `[`
        // and the first letter of typing; the keys behind it are the user's
        // either way, so only the two bytes that were never a key go
        assert_eq!(
            encode_residue_bytes(b"\x1b[hello"),
            vec!["h", "e", "l", "l", "o"]
        );
        assert_eq!(
            encode_residue_bytes(b"\x1bOhello"),
            vec!["h", "e", "l", "l", "o"]
        );
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

    #[test]
    fn residue_a_terminal_report_is_consumed_rather_than_typed() {
        // `CSI R` answers "where is the cursor" and shares its final byte
        // with F3. Its parameters are what tell the two apart: a report
        // carries them, the key does not
        assert!(encode_residue_bytes(b"\x1b[1;40R").is_empty());
        assert_eq!(encode_residue_bytes(b"\x1b[R"), vec!["<F3>"]);
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
        // also are, and holding them back would cost the keypress
        assert_eq!(encode_residue_bytes(b"\x1bP"), vec!["<M-P>"]);
        assert_eq!(encode_residue_bytes(b"\x1b]"), vec!["<M-]>"]);
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
            assert_eq!(encode_residue_bytes(run), vec!["o", "k"]);
        }
        // a bare `ESC` is the Escape key, never a tail to wait on
        assert_eq!(decode_residue(b"ok\x1b").unfinished, 0);
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
    fn residue_unrecognized_control_bytes_are_dropped() {
        assert_eq!(encode_residue_bytes(b"a\x00b\x07c"), vec!["a", "b", "c"]);
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

        let codes = [
            KeyCode::Char('a'),
            KeyCode::Char('<'),
            KeyCode::Char(' '),
            KeyCode::Char('.'),
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
