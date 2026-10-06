//! The line a `<CR>` submits, and what nvim tells view about it after: the
//! report that the line has run, and a prompt the line stopped at.

use std::time::Duration;

use super::{in_flight, Armed, Line, State, SubmitHold, Typed, WordEnd};
use crate::events::UiEvent;
use crate::model::Model;
use crate::msg::Msg;
use crate::native::speculate::SpecStamp;

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

impl SubmitHold {
    /// Notes nvim's report of `line`, which spends the oldest timed-out
    /// line it is the report of.
    pub(crate) fn note_line_reported(&mut self, line: &str) {
        if let Some(at) = self
            .timed_out
            .iter()
            .position(|(old, _)| same_line(old, line))
        {
            self.timed_out.remove(at);
        }
    }

    /// The `:View` lines submitted by hand that nvim has not reported yet,
    /// the one a hold waits on first.
    pub(crate) fn unreported_lines(&self) -> impl Iterator<Item = &str> {
        let armed = matches!(self.held, Some((Armed::Command, _))).then_some(&self.armed_line);
        armed
            .into_iter()
            .chain(self.timed_out.iter().map(|(line, _)| line))
            .map(String::as_str)
    }

    /// Forgets every timed-out line the host's age check at `now` has seen
    /// for `bound` with no report, so a line nvim never reports is told
    /// apart for one bound and no longer. A line is stamped on the first
    /// check after it timed out.
    pub(crate) fn age_unreported(&mut self, now: SpecStamp, bound: Duration) {
        self.timed_out.retain_mut(|(_, seen)| {
            let since = *seen.get_or_insert(now);
            now.age_since(since) < bound
        });
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
        hold.timed_out
            .push_back((std::mem::take(&mut hold.armed_line), None));
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
