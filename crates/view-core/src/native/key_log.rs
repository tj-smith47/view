//! The key log: every mapping that fired this session, newest first, and
//! the state of the overlay that shows it.
//!
//! A view entry point is logged when its invocation is folded, so a held
//! invocation appears once it has run. A user's own normal-mode mapping is
//! logged where a surface of view's own resolves the keys it holds to that
//! mapping. Nothing is logged for a key that fires no mapping.

use std::collections::VecDeque;
use std::time::SystemTime;

use crate::model::format_at;
use crate::native::mappings::{MappingOwner, COMMAND};
use crate::native::views::{PaletteRow, PaletteView};

/// How many entries the log keeps before it drops its oldest.
pub const CAPACITY: usize = 200;

/// The title the key log is drawn under.
pub const KEY_LOG_TITLE: &str = "Keys";

/// Which mapping fired.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fired {
    /// One of view's own entry points.
    View {
        /// The feature id invoked.
        feature: String,
        /// The verb invoked.
        verb: String,
        /// The key that invoked it, or `None` for a `:View` command.
        lhs: Option<String>,
        /// The user mapping that key was set over.
        displaced: Option<MappingOwner>,
    },
    /// One of the user's own normal-mode mappings.
    User {
        /// The keys, as `keytrans()` spells them.
        lhs: String,
        /// Whose the mapping is, when the registration read it.
        owner: Option<MappingOwner>,
    },
}

/// One mapping that fired, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyLogEntry {
    /// The fold's own clock (`Model::set_now`) when it was logged.
    pub at: SystemTime,
    /// Which mapping it was.
    pub fired: Fired,
}

impl KeyLogEntry {
    /// The row the overlay draws: the time, the key, whose it is and what
    /// it did, the widest column last so a narrow box cuts only that one.
    #[must_use]
    pub fn label(&self, utc_offset_secs: i64) -> String {
        let stamp = format_at(self.at, utc_offset_secs);
        let stamp = stamp.get(11..).unwrap_or(&stamp);
        match &self.fired {
            Fired::View {
                feature,
                verb,
                lhs,
                displaced,
            } => {
                let key = lhs.clone().unwrap_or_else(|| format!(":{COMMAND}"));
                let took = displaced
                    .as_ref()
                    .map(|owner| format!("   took it from: {}", owner.describe()))
                    .unwrap_or_default();
                format!("{stamp}  {key:<12}  view   {feature} {verb}{took}")
            }
            Fired::User { lhs, owner } => {
                let whose = owner.as_ref().map(MappingOwner::describe);
                format!("{stamp}  {lhs:<12}  yours  {}", whose.unwrap_or_default())
                    .trim_end()
                    .to_string()
            }
        }
    }
}

/// The ring of mappings that fired, bounded at [`CAPACITY`].
#[derive(Debug, Clone)]
pub struct KeyLog {
    entries: VecDeque<KeyLogEntry>,
    /// How many entries were ever pushed: an open overlay compares it with
    /// what it last read, and an eviction moves it where a length would
    /// stand still.
    pushed: usize,
    /// The clock the next push stamps itself with.
    now: SystemTime,
}

impl KeyLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(CAPACITY),
            pushed: 0,
            now: SystemTime::UNIX_EPOCH,
        }
    }

    /// Sets the clock every entry pushed from here on records.
    pub fn set_now(&mut self, now: SystemTime) {
        self.now = now;
    }

    /// Logs `fired` at the current clock, dropping the oldest entry once
    /// the log holds [`CAPACITY`].
    pub fn push(&mut self, fired: Fired) {
        if self.entries.len() >= CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(KeyLogEntry {
            at: self.now,
            fired,
        });
        self.pushed = self.pushed.saturating_add(1);
    }

    /// How many entries this log has taken over its life.
    #[must_use]
    pub fn pushed(&self) -> usize {
        self.pushed
    }

    /// The entries, newest first.
    pub fn entries(&self) -> impl Iterator<Item = &KeyLogEntry> {
        self.entries.iter().rev()
    }

    /// How many entries the log holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log holds no entry.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry `index` rows below the newest.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&KeyLogEntry> {
        let last = self.entries.len().checked_sub(1)?;
        self.entries.get(last.checked_sub(index)?)
    }
}

impl Default for KeyLog {
    fn default() -> Self {
        Self::new()
    }
}

/// The open key log overlay: which row its keys act on, and whether a
/// person has entered it. The rows themselves are read off [`KeyLog`] each
/// frame.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct KeyLogView {
    selected: usize,
    /// The log's push count at the last read.
    read: usize,
    /// Whether the last key was the first `g` of a `gg`.
    pending_g: bool,
    /// Whether the overlay holds the keyboard. It opens without it, so the
    /// keys a person watches fire still reach the editor.
    entered: bool,
}

impl KeyLogView {
    /// The overlay as it opens over `log`: not entered, the newest entry
    /// selected.
    #[must_use]
    pub fn open(log: &KeyLog) -> Self {
        Self {
            selected: 0,
            read: log.pushed(),
            pending_g: false,
            entered: false,
        }
    }

    /// Whether a person has entered the overlay.
    #[must_use]
    pub const fn entered(&self) -> bool {
        self.entered
    }

    /// Gives the overlay the keyboard, answering whether that changed.
    pub fn enter(&mut self) -> bool {
        !std::mem::replace(&mut self.entered, true)
    }

    /// Catches the overlay up with `log`, answering whether a row arrived.
    /// The selection moves down with the rows that arrived above it, so it
    /// stays on the entry it was on.
    pub fn refresh(&mut self, log: &KeyLog) -> bool {
        if log.pushed() == self.read {
            return false;
        }
        let arrived = log.pushed().saturating_sub(self.read);
        self.read = log.pushed();
        if self.entered {
            let last = log.len().saturating_sub(1);
            self.selected = self.selected.saturating_add(arrived).min(last);
        }
        true
    }

    /// Arms the `g` prefix, so the next `g` is a `gg`.
    pub fn arm_g(&mut self) {
        self.pending_g = true;
    }

    /// Whether a `g` was pending, clearing it either way.
    pub fn take_g(&mut self) -> bool {
        std::mem::take(&mut self.pending_g)
    }

    /// Moves the selection to `index`, clamped to the oldest entry,
    /// answering whether it moved.
    pub fn select(&mut self, log: &KeyLog, index: usize) -> bool {
        let Some(last) = log.len().checked_sub(1) else {
            return false;
        };
        let next = index.min(last);
        let moved = next != self.selected;
        self.selected = next;
        moved
    }

    /// [`Self::select`] relative to the current row, saturating at both
    /// ends.
    pub fn move_selection(&mut self, log: &KeyLog, delta: isize) -> bool {
        let step = delta.unsigned_abs();
        let target = if delta < 0 {
            self.selected.saturating_sub(step)
        } else {
            self.selected.saturating_add(step)
        };
        self.select(log, target)
    }

    /// The selected row's text, as the overlay draws it.
    #[must_use]
    pub fn selected_text(&self, log: &KeyLog, utc_offset_secs: i64) -> Option<String> {
        log.get(self.selected)
            .map(|entry| entry.label(utc_offset_secs))
    }

    /// The rows the overlay paints. The selection is drawn only once a
    /// person has entered it.
    #[must_use]
    pub fn view(&self, log: &KeyLog, utc_offset_secs: i64) -> PaletteView {
        let rows = log
            .entries()
            .map(|entry| PaletteRow::new(entry.label(utc_offset_secs)))
            .collect();
        let view = PaletteView::new(KEY_LOG_TITLE).with_rows(rows);
        if self.entered && !log.is_empty() {
            return view.with_selected(self.selected);
        }
        view
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn user(n: usize) -> Fired {
        Fired::User {
            lhs: format!("#{n}"),
            owner: None,
        }
    }

    fn lhs(entry: &KeyLogEntry) -> &str {
        match &entry.fired {
            Fired::User { lhs, .. } => lhs,
            Fired::View { .. } => "",
        }
    }

    #[test]
    fn the_log_keeps_the_newest_two_hundred_and_drops_the_oldest() {
        let mut log = KeyLog::new();
        for n in 1..=250 {
            log.push(user(n));
        }
        assert_eq!(log.len(), CAPACITY);
        assert_eq!(log.pushed(), 250);
        let oldest = log.entries().last().map(lhs);
        assert_eq!(oldest, Some("#51"), "the oldest surviving push is #51");
    }

    #[test]
    fn the_log_reads_newest_first() {
        let mut log = KeyLog::new();
        for n in 1..=3 {
            log.push(user(n));
        }
        let order: Vec<&str> = log.entries().map(lhs).collect();
        assert_eq!(order, ["#3", "#2", "#1"]);
        assert_eq!(log.get(0).map(lhs), Some("#3"));
        assert_eq!(log.get(2).map(lhs), Some("#1"));
        assert!(log.get(3).is_none());
    }

    #[test]
    fn an_entered_overlay_keeps_its_selection_on_the_entry_it_was_on() {
        let mut log = KeyLog::new();
        log.push(user(1));
        log.push(user(2));
        let mut view = KeyLogView::open(&log);
        view.enter();
        assert!(view.select(&log, 1));
        log.push(user(3));
        assert!(view.refresh(&log));
        assert_eq!(
            view.selected_text(&log, 0).as_deref(),
            Some("00:00:00  #1            yours")
        );
        assert!(!view.refresh(&log), "nothing arrived since the last read");
    }

    #[test]
    fn a_view_row_names_the_key_its_verb_and_what_it_took_the_key_from() {
        let entry = KeyLogEntry {
            at: SystemTime::UNIX_EPOCH,
            fired: Fired::View {
                feature: "picker".into(),
                verb: "files".into(),
                lhs: Some("<leader>ff".into()),
                displaced: Some(MappingOwner::new(
                    "Telescope find files",
                    Some("lua/plugins/telescope.lua".into()),
                )),
            },
        };
        assert_eq!(
            entry.label(0),
            "00:00:00  <leader>ff    view   picker files   took it from: \
             Telescope find files (lua/plugins/telescope.lua)"
        );
        let typed = KeyLogEntry {
            at: SystemTime::UNIX_EPOCH,
            fired: Fired::View {
                feature: "tree".into(),
                verb: "toggle".into(),
                lhs: None,
                displaced: None,
            },
        };
        assert_eq!(typed.label(0), "00:00:00  :View         view   tree toggle");
    }
}
