//! Text <-> line conversions for the clipboard provider's local backend.
//! The system clipboard is one string; nvim's register model is a list of
//! lines. Centralized here rather than duplicated at every consumer, so a
//! change to how a trailing newline or a stray `\r` is handled is one edit
//! instead of several.

/// Splits clipboard text into nvim's line-list register shape.
///
/// A single trailing `\n` is dropped rather than kept as an empty trailing
/// line: nearly every system clipboard write outside view ends with one (a
/// terminal `pbpaste`/`xclip -o` convention), and keeping it would leave
/// `"+p` pasting one extra blank line on every paste, silently regressing
/// the case that is by far the most common. Interior blank lines are kept
/// verbatim. `\r\n` is normalized to `\n` first: clipboard content crossing
/// in from Windows must not leave a stray `\r` inside each pasted line.
#[must_use]
pub fn text_to_lines(text: &str) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n");
    let trimmed = normalized.strip_suffix('\n').unwrap_or(&normalized);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.split('\n').map(str::to_owned).collect()
}

/// Joins nvim's line-list register shape back into clipboard text: the
/// inverse of [`text_to_lines`] for everything but the trailing-newline
/// convention it drops. Never re-adds one: the system clipboard has no
/// register-type concept to say whether the copy was linewise (which nvim
/// would show as a trailing newline) or charwise, so guessing one back in
/// would be inventing a fact `"+yy` never carried across the wire.
#[must_use]
pub fn lines_to_text(lines: &[String]) -> String {
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_trailing_newline_is_dropped_not_kept_as_a_blank_line() {
        assert_eq!(text_to_lines("hello\nworld\n"), vec!["hello", "world"]);
    }

    #[test]
    fn no_trailing_newline_is_kept_as_is() {
        assert_eq!(text_to_lines("hello\nworld"), vec!["hello", "world"]);
    }

    #[test]
    fn interior_blank_lines_survive() {
        assert_eq!(text_to_lines("a\n\nb\n"), vec!["a", "", "b"]);
    }

    #[test]
    fn crlf_is_normalized_to_lf() {
        assert_eq!(text_to_lines("a\r\nb\r\n"), vec!["a", "b"]);
    }

    #[test]
    fn empty_text_is_no_lines() {
        assert!(text_to_lines("").is_empty());
        assert!(text_to_lines("\n").is_empty());
    }

    #[test]
    fn lines_to_text_joins_without_a_trailing_newline() {
        assert_eq!(lines_to_text(&["a".to_string(), "b".to_string()]), "a\nb");
    }
}
