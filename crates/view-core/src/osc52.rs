//! The OSC 52 clipboard escape and the base64 codec inside it.
//!
//! Pure byte and string work, and it lives here because three crates need
//! the same answer and none of them may depend on another: `view-tui`
//! writes the escape to the terminal for view's own provider, this crate's
//! `ui_send` policy reads the escape nvim's own provider forms, and the
//! clipboard worker in `view` forms one again to answer a paste query.
//! Hand-rolled rather than a base64 dependency for the reason the
//! `view-tui` encoder this replaces already gave: the whole codec is
//! smaller than the `Cargo.lock` diff and `scripts/audit-deps.sh` row a new
//! crate would cost.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The OSC 52 selection code `register` names, matching nvim's own
/// `lua/vim/ui/clipboard/osc52.lua` (read off the pinned install, see
/// `docs/clipboard-provider-wire-capture.md`): `'*'` is the primary
/// selection `p`, everything else -- in practice always `'+'` -- the
/// clipboard `c`.
#[must_use]
pub fn selection_code(register: char) -> char {
    if register == '*' {
        'p'
    } else {
        'c'
    }
}

/// The register a selection field names, the inverse of
/// [`selection_code`]. Only `p` reads back as `'*'`: every other code the
/// OSC 52 alphabet allows (`c`, `q`, `s`, `0`-`7`, and the empty field tmux
/// answers with) shares the one clipboard backend `'+'` names, so mapping
/// them there is what the register model can actually honour.
#[must_use]
pub fn register_for_selection(selection: &str) -> char {
    if selection == "p" {
        '*'
    } else {
        '+'
    }
}

/// A complete `ESC ] 52 ; {selection} ; {base64} ESC \` clipboard escape
/// carrying `text` (`:help clipboard-osc52`), the wire form both a copy
/// written to the terminal and a paste answer handed back through
/// `nvim_ui_term_event` take.
#[must_use]
pub fn clipboard_escape(register: char, text: &str) -> String {
    format!(
        "\x1b]52;{};{}\x1b\\",
        selection_code(register),
        base64_encode(text.as_bytes())
    )
}

/// RFC 4648 base64, standard alphabet with `=` padding.
#[must_use]
pub fn base64_encode(bytes: &[u8]) -> String {
    let symbol = |six: u32| char::from(ALPHABET[usize::try_from(six & 0x3f).unwrap_or(0)]);
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(b1.unwrap_or(0)) << 8)
            | u32::from(b2.unwrap_or(0));
        out.push(symbol(n >> 18));
        out.push(symbol(n >> 12));
        out.push(if b1.is_some() { symbol(n >> 6) } else { '=' });
        out.push(if b2.is_some() { symbol(n) } else { '=' });
    }
    out
}

/// The bytes `text` encodes, or `None` when it is not well-formed RFC 4648
/// base64.
///
/// Strict rather than forgiving on purpose: this decodes a payload view is
/// about to store as the user's clipboard, and a decoder that skipped stray
/// characters would turn a malformed escape into plausible-looking text
/// that never gets flagged. Padding is required, and it may only occupy the
/// last one or two symbols of the final quad.
#[must_use]
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for quad in bytes.chunks(4) {
        let mut acc: u32 = 0;
        let mut padding = 0usize;
        for (position, &byte) in quad.iter().enumerate() {
            let six = if byte == b'=' {
                if position < 2 {
                    return None;
                }
                padding += 1;
                0
            } else {
                if padding > 0 {
                    return None;
                }
                u32::from(decode_symbol(byte)?)
            };
            acc = (acc << 6) | six;
        }
        let triple = [
            u8::try_from((acc >> 16) & 0xff).unwrap_or(0),
            u8::try_from((acc >> 8) & 0xff).unwrap_or(0),
            u8::try_from(acc & 0xff).unwrap_or(0),
        ];
        out.extend_from_slice(&triple[..3 - padding]);
    }
    Some(out)
}

fn decode_symbol(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn encoding_pads_to_the_next_multiple_of_four() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"hi!"), "aGkh");
        assert_eq!(base64_encode(b""), "");
    }

    #[test]
    fn every_payload_length_round_trips_through_both_halves() {
        for len in 0..64_usize {
            let bytes: Vec<u8> = (0..len)
                .map(|i| u8::try_from(i % 251).unwrap_or(0))
                .collect();
            let encoded = base64_encode(&bytes);
            assert_eq!(
                base64_decode(&encoded).as_deref(),
                Some(&bytes[..]),
                "length {len} must survive the round trip"
            );
        }
    }

    /// The decoder guards a store into the user's clipboard, so anything
    /// that is not exactly base64 must fail rather than decode to something
    /// plausible: a truncated quad, a symbol outside the alphabet, padding
    /// in the middle, and an over-padded quad all name a corrupt payload.
    #[test]
    fn a_malformed_payload_decodes_to_nothing() {
        for malformed in ["aGVsbG8", "aGVs bG8=", "a===", "=aGk", "aG=k", "????"] {
            assert_eq!(
                base64_decode(malformed),
                None,
                "{malformed:?} is not well-formed base64"
            );
        }
    }

    #[test]
    fn the_escape_carries_the_selection_its_register_names() {
        assert_eq!(clipboard_escape('+', "hi!"), "\x1b]52;c;aGkh\x1b\\");
        assert_eq!(clipboard_escape('*', "hi!"), "\x1b]52;p;aGkh\x1b\\");
    }

    /// An answer with nothing to report is still an answer: nvim's provider
    /// accepts the empty capture and returns at once, where silence would
    /// leave it in `vim.wait`.
    #[test]
    fn an_empty_text_still_forms_a_complete_escape() {
        assert_eq!(clipboard_escape('+', ""), "\x1b]52;c;\x1b\\");
    }

    #[test]
    fn a_selection_code_maps_back_to_the_register_that_formed_it() {
        for register in ['+', '*'] {
            assert_eq!(
                register_for_selection(&selection_code(register).to_string()),
                register
            );
        }
        assert_eq!(register_for_selection(""), '+');
        assert_eq!(register_for_selection("q"), '+');
    }
}
