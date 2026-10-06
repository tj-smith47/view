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
    ///
    /// A report naming a view command that matches no line by its text,
    /// while a command hold stands and an older line is owed, is the
    /// oldest owed line's, whose text nvim expanded, and is spent alone.
    pub(crate) fn note_line_reported(&mut self, line: &str) {
        let reported = match self.timed_out.iter().find(|old| same_line(&old.line, line)) {
            Some(old) => old.seq,
            None if same_line(&self.armed_line, line) && !self.armed_line.is_empty() => {
                self.lines_armed
            }
            None if matches!(self.held, Some((Armed::Command, _))) && names_view(line) => {
                if self.evicted > 0 {
                    self.evicted = self.evicted.saturating_sub(1);
                    return;
                }
                match self.timed_out.pop_front() {
                    Some(_) => return,
                    None => self.lines_armed,
                }
            }
            None => return,
        };
        self.timed_out.retain(|old| old.seq > reported);
        self.evicted = 0;
    }

    /// Whether a line kept unreported is owed a report, which nvim sends
    /// ahead of any newer line's.
    pub(super) fn owes_unreported(&self) -> bool {
        self.evicted > 0 || !self.timed_out.is_empty()
    }

    /// Notes nvim's answer to the registration a bound sent once the lines
    /// up to the `armed`-th had armed: the report had been cleared, so
    /// none of those lines is ever reported.
    pub(crate) fn note_report_restored(&mut self, armed: u64) {
        self.timed_out.retain(|old| old.seq > armed);
        self.evicted = 0;
    }

    /// Forgets every line the replaced engine owed a report, since only
    /// that engine could send one.
    pub(crate) fn note_engine_replaced(&mut self) {
        self.timed_out.clear();
        self.evicted = 0;
        self.armed_line.clear();
        self.armed_unshown = false;
    }

    /// Whether `line` is the report of a timed-out line by its text.
    pub(super) fn reports_timed_out(&self, line: &str) -> bool {
        self.timed_out.iter().any(|old| same_line(&old.line, line))
    }
}

/// Notes what `msg` says of the lines owed a report, and returns how many
/// lines a command hold had armed when `msg` is the bound of a hold a
/// `:View` line armed, which nvim has not reported by then. An engine
/// replaced owes nothing, and nvim's answer that the report had been
/// cleared drops the lines it names.
///
/// The line is kept so its late report ends no newer hold, and so is a
/// line whose hold `msg` ends at a prompt, which nvim may report once the
/// prompt is answered. A line still running at its bound (`:make`,
/// `:!cmd`) reports later, and one whose report a config cleared
/// (`:autocmd! view_line_ran`) never does, so the bound puts the
/// registration back, which a running line does not notice. The count
/// rides the registration, which answers with it where the report had
/// been cleared ([`SubmitHold::note_report_restored`]).
#[must_use]
pub fn note_line_msg(model: &mut Model, msg: &Msg) -> Option<u64> {
    let hold = &mut model.submit_hold;
    match msg {
        Msg::EngineAttached => hold.note_engine_replaced(),
        Msg::LineReportRestored { armed } => hold.note_report_restored(*armed),
        _ => {}
    }
    if !matches!(hold.held, Some((Armed::Command, _))) {
        return None;
    }
    let bounds =
        matches!(msg, Msg::SubmitHoldExpired { generation } if *generation == hold.generation);
    let prompted = matches!(msg, Msg::Redraw(events) if events.iter().any(shows_a_prompt));
    if bounds || prompted {
        if hold.timed_out.len() == TIMED_OUT_KEPT {
            hold.timed_out.pop_front();
            hold.evicted = hold.evicted.saturating_add(1);
        }
        hold.timed_out.push_back(Unreported {
            line: std::mem::take(&mut hold.armed_line),
            seq: hold.lines_armed,
        });
    }
    bounds.then_some(hold.lines_armed)
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
