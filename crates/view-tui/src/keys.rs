//! Crossterm key event to nvim input notation encoding.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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
/// [`spawn_input_thread`](crate::terminal::spawn_input_thread)'s own reader
/// exists) into nvim input notation strings, so [`Term::init`](crate::terminal::Term::init)'s
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
/// `ESC` immediately followed by `[` or `O` is the hard case: it is
/// ambiguous between an `<Esc>` keypress followed by someone separately
/// typing `[`/`O`, and a real CSI or SS3 sequence (an arrow key, a function
/// key -- SS3 is what a DECCKM application-cursor mode inherited from the
/// spawning shell encodes arrow keys as, e.g. `ESC O A` for Up).
/// Disambiguating needs the same multi-byte lookahead grammar `crossterm`'s
/// own parser uses, which is out of scope for a startup-only residue drain,
/// so the policy here is to drop the rest of the buffer from that `ESC`
/// onward rather than guess: forwarding a misdecoded fragment byte-by-byte
/// would inject garbage keystrokes nvim then has to live with (an
/// undecoded SS3 arrow reaching normal mode as literal `O`+`A` opens a line
/// and inserts text), while a dropped arrow key is a keystroke the user can
/// simply press again once the editor is up. A bare `ESC` not followed by
/// `[` or `O` is unambiguous and maps to `<Esc>`.
#[must_use]
pub fn encode_residue_bytes(residue: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < residue.len() {
        match residue[i] {
            0x1b if matches!(residue.get(i + 1), Some(&b'[') | Some(&b'O')) => break,
            0x1b => {
                out.push("<Esc>".to_string());
                i += 1;
            }
            b'\r' | b'\n' => {
                out.push("<CR>".to_string());
                i += 1;
            }
            b'\t' => {
                out.push("<Tab>".to_string());
                i += 1;
            }
            0x7f | 0x08 => {
                out.push("<BS>".to_string());
                i += 1;
            }
            b'<' => {
                out.push("<lt>".to_string());
                i += 1;
            }
            b if (0x20..=0x7e).contains(&b) => {
                out.push((b as char).to_string());
                i += 1;
            }
            b if b >= 0x80 => {
                let start = i;
                i += 1;
                while residue.get(i).is_some_and(|&b| b >= 0x80) {
                    i += 1;
                }
                if let Ok(s) = std::str::from_utf8(&residue[start..i]) {
                    out.extend(s.chars().map(String::from));
                }
            }
            // any other control byte (bell, null, ...) has no sensible nvim
            // notation and is dropped rather than guessed at
            _ => i += 1,
        }
    }
    out
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
        assert_eq!(encode_residue_bytes(b"\x1bx"), vec!["<Esc>", "x"]);
    }

    #[test]
    fn residue_esc_followed_by_bracket_drops_the_rest_of_the_buffer() {
        // ambiguous between "Esc then someone typed [" and a real CSI
        // sequence (an arrow key); the documented policy is to drop rather
        // than guess, including bytes that would otherwise be perfectly
        // decodable if they followed the escape fragment
        assert_eq!(encode_residue_bytes(b"ok\x1b[Aignored"), vec!["o", "k"]);
    }

    #[test]
    fn residue_esc_followed_by_ss3_drops_the_rest_of_the_buffer() {
        // same ambiguity as the CSI case above but for SS3 (DECCKM
        // application-cursor arrows, e.g. "ESC O A" for Up): undecoded, the
        // fragment would forward as literal "O","A" and, in normal mode,
        // open a line and insert text instead of moving the cursor
        assert_eq!(encode_residue_bytes(b"ok\x1bOAignored"), vec!["o", "k"]);
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
}
