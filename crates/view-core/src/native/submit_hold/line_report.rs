//! The line a `<CR>` submits, and what nvim tells view about it after: the
//! report that the line has run, and a prompt the line stopped at.

use super::{in_flight, names_view, Armed, Line, State, SubmitHold, Typed, WordEnd};
use crate::events::UiEvent;
use crate::model::Model;
use crate::msg::Msg;

/// How many characters of a submitted line nvim's report carries and a
/// command hold compares. A line longer than this is matched on these
/// alone, and a pasted line of any length costs one bounded message.
pub const LINE_REPORT_CHARS: usize = 256;

/// How many lines whose hold ended unreported are kept, so that their
/// late reports are told apart from the line a hold waits for.
const TIMED_OUT_KEPT: usize = 4;

/// The modes nvim reads a `:` in as text, opening no command line.
pub(super) const TYPES_TEXT: [&str; 3] = ["insert", "replace", "terminal"];

/// The `msg_show` kinds nvim asks a question in that waits for a key: a
/// `confirm()` dialog, the swap file's ATTENTION dialog, an `:s///c`
/// question.
const PROMPT_KINDS: [&str; 2] = ["confirm", "confirm_sub"];

/// Whether nvim's report of `reported` is the report of `armed`, compared
/// on the characters the report carries.
pub(super) fn same_line(armed: &str, reported: &str) -> bool {
    armed
        .chars()
        .take(LINE_REPORT_CHARS)
        .eq(reported.chars().take(LINE_REPORT_CHARS))
}

/// Whether `event` shows a prompt nvim waits at for an answer: a command
/// line carrying a prompt (`input()`, a dialog's choices) or a question of
/// a [`PROMPT_KINDS`] kind.
#[must_use]
pub fn shows_a_prompt(event: &UiEvent) -> bool {
    match event {
        UiEvent::CmdlineShow { prompt, .. } => !prompt.is_empty(),
        UiEvent::MsgShow { kind, .. } => PROMPT_KINDS.contains(&kind.as_str()),
        _ => false,
    }
}

/// A line whose hold ended at its bound or a prompt before nvim reported
/// it.
#[derive(Debug, Clone)]
pub(super) struct Unreported {
    pub(super) line: String,
    /// The line's place among the lines a hold armed, counted from the
    /// session's first.
    seq: u64,
}

impl SubmitHold {
    /// Notes nvim's report of `line`, which is the report of the oldest
    /// timed-out line of its text, else of the armed line where it is that
    /// line's. nvim reports lines in the order they were submitted, so the
    /// timed-out lines armed before the line it reports are forgotten with
    /// it: their reports have come or never will.
    pub(crate) fn note_line_reported(&mut self, line: &str) {
        let reported = match self.timed_out.iter().find(|old| same_line(&old.line, line)) {
            Some(old) => old.seq,
            None if self.reports_armed(line) => self.lines_armed,
            None => return,
        };
        self.timed_out.retain(|old| old.seq > reported);
    }

    /// Whether `line`, matching no timed-out line, is the report of the
    /// line the newest hold armed: its text, or a view command while that
    /// hold stands, as [`SubmitHold::ended_by`] reads it.
    fn reports_armed(&self, line: &str) -> bool {
        !self.armed_line.is_empty()
            && (same_line(&self.armed_line, line)
                || matches!(self.held, Some((Armed::Command, _))) && names_view(line))
    }

    /// Whether `line` is the report of a timed-out line.
    pub(super) fn reports_timed_out(&self, line: &str) -> bool {
        self.timed_out.iter().any(|old| same_line(&old.line, line))
    }
}

/// Whether `msg` is the bound of a hold a `:View` line armed, which nvim
/// has not reported by then.
///
/// The line is kept so its late report ends no newer hold, and so is a
/// line whose hold `msg` ends at a prompt, which nvim may report once the
/// prompt is answered. A line still running at its bound (`:make`,
/// `:!cmd`) reports later, and one whose report a config cleared
/// (`:autocmd! view_line_ran`) never does, so the bound puts the
/// registration back, which a running line does not notice.
#[must_use]
pub fn note_line_bound(model: &mut Model, msg: &Msg) -> bool {
    let hold = &mut model.submit_hold;
    if !matches!(hold.held, Some((Armed::Command, _))) {
        return false;
    }
    let bounds =
        matches!(msg, Msg::SubmitHoldExpired { generation } if *generation == hold.generation);
    let prompted = matches!(msg, Msg::Redraw(events) if events.iter().any(shows_a_prompt));
    if bounds || prompted {
        if hold.timed_out.len() == TIMED_OUT_KEPT {
            hold.timed_out.pop_front();
        }
        hold.timed_out.push_back(Unreported {
            line: std::mem::take(&mut hold.armed_line),
            seq: hold.lines_armed,
        });
    }
    bounds
}

/// The line a `<CR>` submits, read from the engine's last `cmdline_show`
/// of it, and from the keys view sent where the engine has shown none or
/// is showing a text those keys gave the line on the way (`states`), since
/// the keys after it are still in flight. `None` where view knows neither.
///
/// A `cnoremap` or a `cabbrev` can give a line a text its keys never
/// spelled. The engine's line shows a mapping's text once nvim has read
/// its keys, and the user's command-line mappings and abbreviations expand
/// the keys it has not read yet. An abbreviation that ends the line is
/// expanded by the `<CR>` itself, after nvim's last show of the line.
pub(super) fn submitted_line(
    model: &Model,
    opened: bool,
    typed: Option<&Typed>,
    states: &[State],
) -> Option<String> {
    let hold = &model.submit_hold;
    let shown = model
        .engine
        .cmdline
        .as_ref()
        .filter(|line| line.firstc == ":")
        .map(|line| {
            line.content
                .iter()
                .map(|(_, s)| s.as_str())
                .collect::<String>()
        });
    let from_shown = |shown: &str| {
        hold.expand_abbreviation(Line::typed(shown), WordEnd::Submit)
            .text
    };
    match (typed, shown) {
        (Some(Typed::Known(_)), Some(shown)) if opened && !in_flight(states, &shown) => {
            Some(from_shown(&shown))
        }
        (Some(Typed::Known(text)), _) => Some(hold.expand_typed(text, true).text),
        (_, shown) => shown.map(|shown| from_shown(&shown)),
    }
}
