//! The top pill: the names across the middle of row 0, the session
//! identity at its left edge and the agent's state at its right.
//!
//! The row is one picture with two readers. The painter writes the names
//! into cells and the mouse router answers which name a column names, and
//! both spend [`PillView::row_slots`] so a click lands on the name under the
//! pointer rather than on the one a second layout put there.

use super::ai_panel::AiPanelState;
use super::ai_registry::SessionState;
use super::text::text_width;
use crate::model::{BufferEntry, Model, Panes, TablineState};

/// What the pill names when there is only one tabpage: the tabpages
/// themselves, or the listed buffers.
///
/// Buffers are offered because one tabpage with eight files open is the
/// ordinary shape of a session, and a row naming that one tabpage says
/// nothing a user can act on.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TablineShows {
    /// The open tabpages, always.
    #[default]
    Tabs,
    /// The listed buffers while one tabpage is open, the tabpages
    /// otherwise: a second tabpage is a workspace the user made on
    /// purpose, and hiding it behind a buffer list loses the only thing
    /// that says it exists.
    Buffers,
}

impl TablineShows {
    /// The word a user writes for this answer, and the word a report
    /// prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Tabs => "tabs",
            Self::Buffers => "buffers",
        }
    }

    /// The answer `value` spells, or `None` for a word this build does not
    /// know.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tabs" => Some(Self::Tabs),
            "buffers" => Some(Self::Buffers),
            _ => None,
        }
    }
}

/// One name on the pill and what selecting it switches to.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PillEntry {
    /// The tabpage or buffer handle a click on this name selects.
    pub id: u64,
    /// The name as it is drawn.
    pub label: String,
    /// Whether this is the one the session is on.
    pub current: bool,
}

/// Which of the two things the entries are, which is what decides the call
/// a click on one issues.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PillNames {
    /// Tabpage handles: `nvim_set_current_tabpage`.
    #[default]
    Tabs,
    /// Buffer handles: `nvim_set_current_buf`.
    Buffers,
}

/// The whole row, ready to be drawn or hit-tested.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PillView {
    /// The `--remote` destination this session was started against, empty
    /// for a local one. A person with three windows open on three machines
    /// has nothing else on screen that says which is which.
    pub host: String,
    /// The names across the middle, in the order nvim lists them.
    pub entries: Vec<PillEntry>,
    /// Which handles [`PillView::entries`] carries.
    pub names: PillNames,
    /// What the agent is doing, empty for a session with `[ai]` off.
    pub agent: &'static str,
    /// The row's own width: the terminal's, which is what the layer the
    /// row is drawn into spans.
    ///
    /// Carried on the view so the painter and the mouse router cannot lay
    /// the row out on two different widths. A painter handed a narrower
    /// area writes only the cells inside it and never re-flows, so the
    /// one frame between a resize reaching the model and reaching the
    /// backend draws a clipped row rather than a differently placed one.
    pub width: u16,
}

/// Where one entry was placed: its own column and how many cells it took,
/// the blank either side of the name included.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PillSlot {
    /// The tabpage or buffer this cell run names.
    pub id: u64,
    /// Which of [`PillView::entries`] this run draws. A row longer than
    /// the terminal is a window into the list, so the first run is not
    /// always the first entry and a painter that counted along would draw
    /// the wrong name under every one of them.
    pub entry: usize,
    /// The row's first column this run covers.
    pub col: u16,
    /// How many columns it covers.
    pub cells: u16,
    /// Whether it is the current one.
    pub current: bool,
}

/// The blank either side of a name, so two neighbouring names never touch
/// and the selected one's own colour reads as a pill rather than as a word.
const PAD: u16 = 1;

/// What a buffer with no file behind it is called, spelled the way nvim
/// spells it on its own tab line.
const NO_NAME: &str = "[No Name]";

impl PillView {
    /// The pill this model would draw.
    #[must_use]
    pub fn from_model(model: &Model) -> Self {
        let (entries, names) = entries(
            model.engine.tabline.as_ref(),
            &model.buffers,
            model.tabline_shows,
        );
        Self {
            host: model.remote.clone().unwrap_or_default(),
            entries,
            names,
            agent: agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted),
            width: model.term_width,
        }
    }

    /// Where each entry lands on this session's own row, which is the one
    /// layout both the painter and the mouse router spend.
    #[must_use]
    pub fn row_slots(&self) -> Vec<PillSlot> {
        self.slots(self.width)
    }

    /// Where each entry lands on a row `width` cells wide, left to right.
    ///
    /// The run is centred on the row and clipped to what the host and the
    /// agent word leave: a name that does not fit whole is dropped, for
    /// the reason a frame edge drops a whole segment -- half a file name
    /// reads as a different file.
    ///
    /// More names than fit are a window rather than a prefix, and the
    /// window always holds the current name: a row that dropped it would
    /// leave the one name a person is looking for off the screen, and the
    /// hit test unable to reach it.
    #[must_use]
    pub(crate) fn slots(&self, width: u16) -> Vec<PillSlot> {
        let host = edge_cells(&self.host);
        let agent = edge_cells(self.agent);
        let Some(room) = width.checked_sub(host.saturating_add(agent)) else {
            return Vec::new();
        };
        let widths: Vec<u16> = self
            .entries
            .iter()
            .map(|entry| text_width(&entry.label).saturating_add(PAD * 2))
            .collect();
        let first = self.window_start(&widths, room);
        let mut shown = Vec::with_capacity(self.entries.len());
        let mut used = 0_u16;
        for (index, cells) in widths.iter().enumerate().skip(first) {
            if used.saturating_add(*cells) > room {
                break;
            }
            used = used.saturating_add(*cells);
            shown.push((index, *cells));
        }
        // centred inside the room the two edges leave, never inside the
        // whole row: a long host name would otherwise push the names under
        // it
        let mut col = host.saturating_add(room.saturating_sub(used) / 2);
        let mut slots = Vec::with_capacity(shown.len());
        for (index, cells) in shown {
            let Some(entry) = self.entries.get(index) else {
                break;
            };
            slots.push(PillSlot {
                id: entry.id,
                entry: index,
                col,
                cells,
                current: entry.current,
            });
            col = col.saturating_add(cells);
        }
        slots
    }

    /// The first entry the row starts at, given what each one takes and the
    /// `room` the two edges leave.
    ///
    /// Zero while the current name fits in the run that starts at the first
    /// name, which is every row that holds all of them. Past that the
    /// current name becomes the last one drawn and the names before it fill
    /// back towards the left, so moving forward through a long list scrolls
    /// the row by one name at a time.
    fn window_start(&self, widths: &[u16], room: u16) -> usize {
        let Some(current) = self.entries.iter().position(|entry| entry.current) else {
            return 0;
        };
        let mut used = 0_u16;
        for (index, cells) in widths.iter().enumerate() {
            if used.saturating_add(*cells) <= room {
                used = used.saturating_add(*cells);
                continue;
            }
            if index > current {
                return 0;
            }
            let mut start = current;
            let mut back = widths.get(current).copied().unwrap_or(0);
            while let Some(previous) = start.checked_sub(1).and_then(|at| widths.get(at)) {
                if back.saturating_add(*previous) > room {
                    break;
                }
                back = back.saturating_add(*previous);
                start -= 1;
            }
            return start;
        }
        0
    }

    /// The entry column `col` of this session's row names, or `None` for
    /// a column carrying no name.
    #[must_use]
    pub fn hit(&self, col: u16) -> Option<u64> {
        self.row_slots()
            .into_iter()
            .find(|slot| col >= slot.col && col < slot.col.saturating_add(slot.cells))
            .map(|slot| slot.id)
    }
}

/// The columns an edge word takes, its own blank either side included, or
/// none at all when there is no word.
#[must_use]
pub fn edge_cells(text: &str) -> u16 {
    if text.is_empty() {
        0
    } else {
        text_width(text).saturating_add(PAD * 2)
    }
}

/// The names the pill carries, and which handles they are.
///
/// Buffers only while one tabpage is open: past that the tabpages are what
/// the user arranged, and a row that stopped naming them would leave the
/// second workspace unreachable and unmentioned.
fn entries(
    tabline: Option<&TablineState>,
    buffers: &[BufferEntry],
    shows: TablineShows,
) -> (Vec<PillEntry>, PillNames) {
    let Some(state) = tabline else {
        return (Vec::new(), PillNames::Tabs);
    };
    if shows == TablineShows::Buffers && state.tabs.len() <= 1 {
        let entries = buffers
            .iter()
            .map(|buffer| PillEntry {
                id: buffer.buf,
                // a buffer with no file behind it is nvim's own [No Name];
                // the empty string it reports would draw as two blank pad
                // cells a person cannot tell from a gap
                label: {
                    let name = if buffer.name.is_empty() {
                        NO_NAME
                    } else {
                        buffer.name.as_str()
                    };
                    if buffer.modified {
                        format!("{name} +")
                    } else {
                        name.to_string()
                    }
                },
                current: buffer.current,
            })
            .collect();
        return (entries, PillNames::Buffers);
    }
    let entries = state
        .tabs
        .iter()
        .map(|tab| PillEntry {
            id: tab.tab.0,
            label: tab.name.clone(),
            current: tab.tab == state.current,
        })
        .collect();
    (entries, PillNames::Tabs)
}

/// What the agent is doing, in one word, or empty for a session with the
/// agent turned off.
///
/// A pending permission outranks the session's own state because it is the
/// one condition that is waiting on the person reading the row.
#[must_use]
pub fn agent_word(panel: &AiPanelState, enabled: bool, trusted: bool) -> &'static str {
    if !enabled {
        return "";
    }
    if panel.pending_permission.is_some() {
        return "waiting";
    }
    match SessionState::derive(panel, trusted) {
        SessionState::Active => "running",
        SessionState::Crashed => "crashed",
        SessionState::Trusted | SessionState::NotStarted => "idle",
    }
}

/// nvim's own default `showtabline`, and what view holds until the bridge
/// relays the session's own value.
pub const DEFAULT_SHOWTABLINE: u8 = 1;

/// Whether the pill takes the top row of this session's terminal.
///
/// Read off the attach rather than off the arrival of a `tabline_update`,
/// because the row is reserved from the frame the session starts drawing:
/// waiting for nvim's first tabline event would paint one frame a row
/// taller and then shift everything down.
///
/// Under tiles the row stands whatever is open, because a pill with one
/// workspace still carries the host and the agent word; under
/// `panes = "nvim"` it is the row nvim itself would have drawn, so it
/// follows the user's own `showtabline`.
#[must_use]
pub fn shows(model: &Model) -> bool {
    shows_under(
        model.owns(crate::native::ext::Ext::Tabline),
        model.look.panes,
        model
            .engine
            .tabline
            .as_ref()
            .map_or(0, |state| state.tabs.len()),
        model.showtabline,
    )
}

/// [`shows`] from the four answers it reads, for the one caller that has
/// them before there is a model to ask: the spawn geometry, which is seeded
/// a row shorter so the child is laid out against the grid the attach will
/// ask for.
///
/// `showtabline` is read the way nvim reads it: `0` keeps the row off, `1`
/// shows it once a second tabpage is open, and anything from `2` up shows
/// it always. Under tiles it decides nothing, because the row carries the
/// host and the agent word whatever nvim would have drawn there.
#[must_use]
pub fn shows_under(owns_tabline: bool, panes: Panes, tabs: usize, showtabline: u8) -> bool {
    let nvim_would_draw_it = match showtabline {
        0 => false,
        1 => tabs > 1,
        _ => true,
    };
    owns_tabline && (panes == Panes::Tiles || nvim_would_draw_it)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::events::{TabEntry, TabHandle};

    fn tabline(current: u64, names: &[&str]) -> TablineState {
        TablineState {
            current: TabHandle(current),
            tabs: names
                .iter()
                .enumerate()
                .map(|(i, name)| TabEntry {
                    tab: TabHandle(u64::try_from(i).unwrap() + 1),
                    name: (*name).to_string(),
                })
                .collect(),
        }
    }

    fn buffer(buf: u64, name: &str, current: bool) -> BufferEntry {
        BufferEntry {
            buf,
            name: name.to_string(),
            modified: false,
            current,
        }
    }

    fn view(entries: Vec<PillEntry>) -> PillView {
        PillView {
            host: String::new(),
            entries,
            names: PillNames::Tabs,
            agent: "",
            width: 20,
        }
    }

    #[test]
    fn the_pill_centres_its_names_and_lights_the_current_one() {
        let pill = view(vec![
            PillEntry {
                id: 1,
                label: "one".to_string(),
                current: false,
            },
            PillEntry {
                id: 2,
                label: "two".to_string(),
                current: true,
            },
        ]);
        // two names of five cells each in a row of twenty: five blank
        // columns either side
        let slots = pill.slots(20);
        assert_eq!(slots.len(), 2);
        assert_eq!((slots[0].col, slots[0].cells), (5, 5));
        assert_eq!((slots[1].col, slots[1].cells), (10, 5));
        assert!(!slots[0].current && slots[1].current);
    }

    #[test]
    fn the_current_name_is_always_on_the_row() {
        let names = |current: usize| {
            view(
                (0..10)
                    .map(|index| PillEntry {
                        id: index + 1,
                        label: format!("f{index}"),
                        current: usize::try_from(index).unwrap() == current,
                    })
                    .collect(),
            )
        };
        // ten names of four cells each in a row of thirty: seven fit, and
        // the last one is three names past the cut
        let scrolled = names(9).slots(30);
        assert_eq!(scrolled.len(), 7);
        assert_eq!(scrolled.first().map(|slot| slot.id), Some(4));
        let last = scrolled.last().copied().unwrap();
        assert_eq!(last.id, 10);
        assert!(last.current);

        // a current name inside the run the first name starts leaves the
        // row where it was
        let anchored = names(0).slots(30);
        assert_eq!(anchored.first().map(|slot| slot.id), Some(1));
        assert!(anchored.first().copied().unwrap().current);
    }

    #[test]
    fn an_unnamed_buffer_is_named_the_way_nvim_names_it() {
        let (entries, names) = entries(
            Some(&tabline(1, &["one"])),
            &[buffer(7, "", true)],
            TablineShows::Buffers,
        );
        assert_eq!(names, PillNames::Buffers);
        assert_eq!(
            entries.first().map(|entry| entry.label.as_str()),
            Some("[No Name]")
        );
    }

    #[test]
    fn a_name_that_does_not_fit_whole_is_dropped_with_everything_behind_it() {
        let pill = view(vec![
            PillEntry {
                id: 1,
                label: "aaaa".to_string(),
                current: true,
            },
            PillEntry {
                id: 2,
                label: "bbbb".to_string(),
                current: false,
            },
        ]);
        assert_eq!(pill.slots(9).len(), 1);
        assert_eq!(pill.slots(5).len(), 0);
    }

    #[test]
    fn the_host_and_the_agent_word_take_their_room_off_the_centre() {
        let pill = PillView {
            host: "sir".to_string(),
            entries: vec![PillEntry {
                id: 1,
                label: "one".to_string(),
                current: true,
            }],
            names: PillNames::Tabs,
            agent: "idle",
            width: 30,
        };
        let slots = pill.slots(30);
        // 5 for the host, 6 for the agent word, the name centred in 19
        assert_eq!((slots[0].col, slots[0].cells), (12, 5));
    }

    #[test]
    fn a_click_lands_on_the_name_under_it() {
        let pill = view(vec![
            PillEntry {
                id: 7,
                label: "one".to_string(),
                current: false,
            },
            PillEntry {
                id: 9,
                label: "two".to_string(),
                current: true,
            },
        ]);
        assert_eq!(pill.hit(4), None);
        assert_eq!(pill.hit(5), Some(7));
        assert_eq!(pill.hit(9), Some(7));
        assert_eq!(pill.hit(10), Some(9));
        assert_eq!(pill.hit(15), None);
    }

    #[test]
    fn a_name_is_measured_in_cells_so_a_decomposed_accent_takes_no_column() {
        let pill = view(vec![PillEntry {
            id: 1,
            label: "cafe\u{301}.rs".to_string(),
            current: true,
        }]);
        assert_eq!(pill.slots(20)[0].cells, 9);
    }

    #[test]
    fn tabline_shows_buffers_only_with_one_tabpage() {
        let buffers = vec![buffer(3, "a.rs", true), buffer(4, "b.rs", false)];
        let (one, names) = entries(
            Some(&tabline(1, &["work"])),
            &buffers,
            TablineShows::Buffers,
        );
        assert_eq!(names, PillNames::Buffers);
        assert_eq!(
            one.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![3, 4],
            "one tabpage under buffers names the buffers"
        );

        let (two, names) = entries(
            Some(&tabline(1, &["work", "docs"])),
            &buffers,
            TablineShows::Buffers,
        );
        assert_eq!(names, PillNames::Tabs);
        assert_eq!(
            two.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![1, 2],
            "a second tabpage is what the row names, whatever the key says"
        );

        let (tabs, names) = entries(Some(&tabline(1, &["work"])), &buffers, TablineShows::Tabs);
        assert_eq!(names, PillNames::Tabs);
        assert_eq!(tabs.len(), 1);
    }

    #[test]
    fn an_unsaved_buffer_carries_its_marker_into_the_name() {
        let mut modified = buffer(3, "a.rs", true);
        modified.modified = true;
        let (entries, _) = entries(
            Some(&tabline(1, &["work"])),
            &[modified],
            TablineShows::Buffers,
        );
        assert_eq!(entries[0].label, "a.rs +");
    }

    #[test]
    fn the_pill_prints_waiting_while_a_permission_is_pending() {
        let mut model = Model::with_term_size(80, 24);
        model.ai_trusted = true;
        model.ai_panel_mut().session_id = Some("s-1".to_string());
        assert_eq!(
            agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted),
            "running"
        );
        model.ai_panel_mut().pending_permission =
            Some(crate::native::ai_panel::PermissionPrompt::new(
                1,
                "call-1",
                Some("run tests?".to_string()),
                None,
                Vec::new(),
            ));
        assert_eq!(
            agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted),
            "waiting"
        );
    }

    /// Under `panes = "nvim"` the row is the one nvim itself would have
    /// drawn, so the user's own `showtabline` decides it. A threshold
    /// hardcoded to a second tabpage took the always-on row away from a
    /// user who had set 2, and gave a row to one who had set 0.
    #[test]
    fn the_nvim_look_row_follows_showtabline() {
        for (showtabline, tabs, under_nvim) in [
            (0_u8, 1_usize, false),
            (0, 2, false),
            (1, 1, false),
            (1, 2, true),
            (2, 1, true),
            (2, 2, true),
        ] {
            assert_eq!(
                shows_under(true, Panes::Nvim, tabs, showtabline),
                under_nvim,
                "showtabline={showtabline} with {tabs} tabpage(s) under the nvim look"
            );
            assert!(
                shows_under(true, Panes::Tiles, tabs, showtabline),
                "showtabline={showtabline} with {tabs} tabpage(s) took the tiles row away"
            );
            assert!(
                !shows_under(false, Panes::Nvim, tabs, showtabline)
                    && !shows_under(false, Panes::Tiles, tabs, showtabline),
                "a session that left the tab line with nvim drew a row anyway"
            );
        }
    }

    /// The whole row is laid out once, on the terminal's own width, and
    /// both readers spend that layout: the painter draws the names there
    /// and the router answers which name a column carries.
    #[test]
    fn the_row_is_laid_out_on_the_terminals_own_width() {
        let mut model = Model::with_term_size(30, 5);
        model.engine.tabline = Some(tabline(1, &["work", "docs"]));
        let pill = PillView::from_model(&model);
        assert_eq!(pill.width, model.term_width);
        assert_eq!(pill.row_slots(), pill.slots(model.term_width));
        assert_eq!(pill.row_slots().len(), 2);
        for slot in pill.row_slots() {
            for col in slot.col..slot.col.saturating_add(slot.cells) {
                assert_eq!(
                    pill.hit(col),
                    Some(slot.id),
                    "column {col} was drawn for tab {} and hit-tests as something else",
                    slot.id
                );
            }
        }
    }

    #[test]
    fn the_pill_prints_running_for_an_active_session() {
        let mut model = Model::with_term_size(80, 24);
        assert_eq!(
            agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted),
            "idle"
        );
        model.ai_trusted = true;
        assert_eq!(
            agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted),
            "idle"
        );
        model.ai_panel_mut().session_id = Some("s-1".to_string());
        assert_eq!(
            agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted),
            "running"
        );
        model.ai_panel_mut().local_error = Some("the agent died".to_string());
        assert_eq!(
            agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted),
            "crashed"
        );
        model.ai_enabled = false;
        assert_eq!(
            agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted),
            ""
        );
    }
}
