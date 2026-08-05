//! Text <-> line conversions for the clipboard provider's local backend.
//! The system clipboard is one string; nvim's register model is a list of
//! lines plus a register type. Centralized here rather than duplicated at
//! every consumer, so a change to how a trailing newline or a stray `\r` is
//! handled is one edit instead of several.
//!
//! # The trailing-newline convention
//!
//! The system clipboard has no register-type field of its own; nvim's own
//! shell-based clipboard providers (`xclip`, `pbcopy`, ...) signal linewise
//! the only way plain text can: a linewise copy's text ends with a trailing
//! `\n` that a charwise copy's never has, and `systemlist(cmd, [''], 1)` is
//! what reads that signal back out on paste. [`text_to_lines`] and
//! [`lines_to_text`] apply that exact convention so a copy made outside
//! view (or by a previous view session) round-trips its register type
//! correctly, and so `"+yy`/`"+p` inside one view session do too --
//! dropping the trailing newline both directions (the prior behavior here)
//! collapsed every linewise copy into a charwise paste, invisibly within a
//! single process and only surfacing across two.

use view_core::msg::RegisterType;

/// Splits clipboard text into nvim's line-list register shape, alongside
/// the [`RegisterType`] a trailing `\n` signals (see the module doc). The
/// trailing `\n` itself is never kept as an empty trailing line -- it is
/// the regtype signal, not a copied blank line. Interior blank lines are
/// kept verbatim. `\r\n` is normalized to `\n` first: clipboard content
/// crossing in from Windows must not leave a stray `\r` inside each pasted
/// line.
#[must_use]
pub fn text_to_lines(text: &str) -> (Vec<String>, RegisterType) {
    let normalized = text.replace("\r\n", "\n");
    match normalized.strip_suffix('\n') {
        Some("") => (Vec::new(), RegisterType::Linewise),
        Some(trimmed) => (
            trimmed.split('\n').map(str::to_owned).collect(),
            RegisterType::Linewise,
        ),
        None if normalized.is_empty() => (Vec::new(), RegisterType::Charwise),
        None => (
            normalized.split('\n').map(str::to_owned).collect(),
            RegisterType::Charwise,
        ),
    }
}

/// Joins nvim's line-list register shape back into clipboard text: the
/// inverse of [`text_to_lines`]. Appends the trailing `\n` a
/// [`RegisterType::Linewise`] copy signals to the next reader of the system
/// clipboard (see the module doc); a [`RegisterType::Charwise`] copy gets
/// none, matching every non-view tool's own charwise convention.
///
/// A real `g:clipboard.copy` invocation from nvim (verified against the
/// pinned nvim by instrumenting a throwaway `copy` closure and yanking
/// through it directly, headless -- never assumed) already terminates
/// `lines` with an empty-string element whenever the source register's
/// text ends at a line boundary, matching `jobsend()`/`chansend()`'s own
/// documented list-to-bytestream convention: nvim's bundled `xclip`-based
/// provider relies on that exact same trailing `""` to make its piped
/// child process see the final newline. `Vec::join` already reproduces
/// that byte for a trailing `""` element, so appending a second `\n`
/// whenever one is already present would double it -- the fix for the
/// `"+p` charwise-instead-of-linewise regression this module exists to
/// close. Only append the synthesized `\n` when nvim's own marker is
/// absent, which happens for every regtype `text_to_lines` itself can
/// produce (its output never carries the marker) and keeps this a true
/// inverse for that path.
#[must_use]
pub fn lines_to_text(lines: &[String], regtype: RegisterType) -> String {
    let joined = lines.join("\n");
    let already_newline_terminated = lines.last().is_some_and(String::is_empty);
    match regtype {
        RegisterType::Linewise if already_newline_terminated => joined,
        RegisterType::Linewise => format!("{joined}\n"),
        RegisterType::Charwise => joined,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_trailing_newline_is_not_kept_as_a_blank_line_and_signals_linewise() {
        let (lines, regtype) = text_to_lines("hello\nworld\n");
        assert_eq!(lines, vec!["hello", "world"]);
        assert_eq!(regtype, RegisterType::Linewise);
    }

    #[test]
    fn no_trailing_newline_is_kept_as_is_and_signals_charwise() {
        let (lines, regtype) = text_to_lines("hello\nworld");
        assert_eq!(lines, vec!["hello", "world"]);
        assert_eq!(regtype, RegisterType::Charwise);
    }

    #[test]
    fn interior_blank_lines_survive_either_regtype() {
        assert_eq!(text_to_lines("a\n\nb\n").0, vec!["a", "", "b"]);
        assert_eq!(text_to_lines("a\n\nb").0, vec!["a", "", "b"]);
    }

    #[test]
    fn crlf_is_normalized_to_lf() {
        assert_eq!(text_to_lines("a\r\nb\r\n").0, vec!["a", "b"]);
    }

    #[test]
    fn empty_text_is_no_lines_and_charwise() {
        assert_eq!(text_to_lines(""), (Vec::new(), RegisterType::Charwise));
    }

    #[test]
    fn a_lone_trailing_newline_is_no_lines_but_still_linewise() {
        assert_eq!(text_to_lines("\n"), (Vec::new(), RegisterType::Linewise));
    }

    #[test]
    fn lines_to_text_appends_a_trailing_newline_for_linewise() {
        assert_eq!(
            lines_to_text(&["a".to_string(), "b".to_string()], RegisterType::Linewise),
            "a\nb\n"
        );
    }

    #[test]
    fn lines_to_text_omits_the_trailing_newline_for_charwise() {
        assert_eq!(
            lines_to_text(&["a".to_string(), "b".to_string()], RegisterType::Charwise),
            "a\nb"
        );
    }

    /// nvim's real `copy()` invocation for a linewise yank -- confirmed by
    /// instrumenting a throwaway `g:clipboard.copy` closure against the
    /// pinned nvim, headless, and reading back exactly what it received for
    /// `"+yy` -- already terminates `lines` with an empty-string marker
    /// element (`{ "hello world", "" }`, matching `jobsend()`'s own
    /// trailing-newline convention). `lines_to_text` must not add a second
    /// `\n` on top of that marker: doing so is exactly the
    /// `"view-clipboard-oracle-marker\n\n"` double-newline regression this
    /// test locks in against ever reappearing without a live probe.
    #[test]
    fn a_linewise_copy_carrying_nvims_own_trailing_marker_is_not_double_newlined() {
        let lines = vec!["hello world".to_string(), String::new()];
        assert_eq!(
            lines_to_text(&lines, RegisterType::Linewise),
            "hello world\n"
        );
    }

    /// The same marker convention nvim used for a single-line yank also
    /// appears, unchanged, on a multi-line linewise yank (`VG"+y` observed
    /// against the pinned nvim as `{ "a", "b", "" }`): only the final
    /// element is the marker, interior lines are untouched.
    #[test]
    fn a_multi_line_linewise_copy_carrying_nvims_own_trailing_marker_joins_once() {
        let lines = vec!["a".to_string(), "b".to_string(), String::new()];
        assert_eq!(lines_to_text(&lines, RegisterType::Linewise), "a\nb\n");
    }

    /// The pair-form contract [`docs/clipboard-provider-wire-capture.md`]
    /// validated: whatever `lines_to_text` writes to the system clipboard,
    /// `text_to_lines` must read back as the identical lines and regtype --
    /// this is finding 1's cross-process bug made a property test rather
    /// than a one-off example, since the bug was exactly this round trip
    /// silently not holding.
    #[test]
    fn every_regtype_round_trips_through_lines_to_text_and_back() {
        for regtype in [RegisterType::Charwise, RegisterType::Linewise] {
            let lines = vec!["XXX".to_string(), "AAA".to_string(), "BBB".to_string()];
            let text = lines_to_text(&lines, regtype);
            assert_eq!(
                text_to_lines(&text),
                (lines.clone(), regtype),
                "{regtype:?} did not round-trip through {text:?}"
            );
        }
    }
}
