//! The top pill: the names across the middle of row 0, the session
//! identity at its left edge and the agent's state at its right.
//!
//! The row is one picture with two readers. The painter writes the names
//! into cells and the mouse router answers which name a column names, and
//! both spend [`PillView::row_slots`] so a click lands on the name under the
//! pointer. A second layout could put a different name there.

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

/// How each pill's two ends are drawn.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PillCaps {
    /// The Nerd Font half circles, U+E0B6 on the left and U+E0B4 on the
    /// right.
    #[default]
    Round,
    /// One blank cell in the pill's own background at each end, for a
    /// font without those glyphs or a terminal that draws them two cells
    /// wide. The row keeps the same width in both modes.
    Flat,
}

impl PillCaps {
    /// The word a user writes for this answer, and the word a report
    /// prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Round => "round",
            Self::Flat => "flat",
        }
    }

    /// The answer `value` spells, or `None` for a word this build does not
    /// know. `auto` is not an answer here: it is the absence of one.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "round" => Some(Self::Round),
            "flat" => Some(Self::Flat),
            _ => None,
        }
    }

    /// What `"auto"` resolves to: round ends only where the terminal was
    /// probed to draw box glyphs one cell wide, the only width fact view
    /// has about the glyphs a font carries.
    #[must_use]
    pub const fn derived(unicode_boxes: bool) -> Self {
        if unicode_boxes {
            Self::Round
        } else {
            Self::Flat
        }
    }

    /// The left and right end cells, each one column wide.
    #[must_use]
    pub const fn ends(self) -> (&'static str, &'static str) {
        match self {
            Self::Round => ("\u{e0b6}", "\u{e0b4}"),
            Self::Flat => (" ", " "),
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
    /// What the agent is doing, empty while it is idle or `[ai]` is off:
    /// an idle agent is nothing a person has to read.
    pub agent: &'static str,
    /// How each pill's ends are drawn.
    pub caps: PillCaps,
    /// The blank columns at each end of the row, the look's own
    /// [`grid_offset`](crate::model::Look::grid_offset), so the edge pills
    /// line up with the tiles' frames under them.
    pub margin: u16,
    /// The row's own width: the terminal's, which is what the layer the
    /// row is drawn into spans.
    ///
    /// Carried on the view so the painter and the mouse router cannot lay
    /// the row out on two different widths. A painter handed a narrower
    /// area writes only the cells inside it and never re-flows, so the
    /// one frame between a resize reaching the model and reaching the
    /// backend draws a clipped row, with every name where it was.
    pub width: u16,
}

/// Where one entry was placed: its own column and how many cells it took,
/// both ends and the blank either side of the name included.
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

/// The cells a pill adds to its word: an end and a blank on each side.
pub const PILL: u16 = 4;

/// The blank column between two neighbouring pills.
const SEP: u16 = 1;

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
            agent: match agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted) {
                "idle" => "",
                word => word,
            },
            caps: model
                .pill_caps
                .unwrap_or(PillCaps::derived(model.caps.unicode_boxes)),
            margin: model.look.grid_offset(),
            width: model.term_width,
        }
    }

    /// The column the host pill starts at.
    #[must_use]
    pub const fn host_col(&self) -> u16 {
        self.margin
    }

    /// The column the agent pill starts at, on this session's own row.
    #[must_use]
    pub fn agent_col(&self) -> u16 {
        self.width
            .saturating_sub(self.margin)
            .saturating_sub(edge_cells(self.agent))
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
    /// More names than fit are shown as a window onto the list, and the
    /// window always holds the current name: a row that dropped it would
    /// leave the one name a person is looking for off the screen, and the
    /// hit test unable to reach it.
    #[must_use]
    pub(crate) fn slots(&self, width: u16) -> Vec<PillSlot> {
        let host = edge_cells(&self.host);
        let agent = edge_cells(self.agent);
        let left = self
            .margin
            .saturating_add(host)
            .saturating_add(u16::from(host > 0));
        let right = width
            .saturating_sub(self.margin)
            .saturating_sub(agent)
            .saturating_sub(u16::from(agent > 0));
        let Some(room) = right.checked_sub(left) else {
            return Vec::new();
        };
        // each pill is measured with the separator after it, against a room
        // one separator wider, so the last pill owes no trailing blank
        let widths: Vec<u16> = self
            .entries
            .iter()
            .map(|entry| {
                text_width(&entry.label)
                    .saturating_add(PILL)
                    .saturating_add(SEP)
            })
            .collect();
        let room = room.saturating_add(SEP);
        let first = self.window_start(&widths, room);
        let mut shown = Vec::with_capacity(self.entries.len());
        let mut used = 0_u16;
        for (index, cells) in widths.iter().enumerate().skip(first) {
            if used.saturating_add(*cells) > room {
                break;
            }
            used = used.saturating_add(*cells);
            shown.push((index, cells.saturating_sub(SEP)));
        }
        // centred inside the room the two edges leave, never inside the
        // whole row: a long host name would otherwise push the names under
        // it
        let mut col = left.saturating_add(room.saturating_sub(used) / 2);
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
            col = col.saturating_add(cells).saturating_add(SEP);
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

/// The columns an edge pill takes, both ends included, or none at all when
/// there is no word.
#[must_use]
pub fn edge_cells(text: &str) -> u16 {
    if text.is_empty() {
        0
    } else {
        text_width(text).saturating_add(PILL)
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
/// Under tiles the row stands only while it says something nothing else on
/// screen says ([`has_unique_content`]): each tile's frame already carries
/// its own buffer name, so a row naming one workspace is a banner. Under
/// `panes = "nvim"` it is the row nvim itself would have drawn, so it
/// follows the user's own `showtabline`.
#[must_use]
pub fn shows(model: &Model) -> bool {
    row_shows(
        model.owns(crate::native::ext::Ext::Tabline),
        model.look.panes,
        has_unique_content(model),
        model
            .engine
            .tabline
            .as_ref()
            .map_or(0, |state| state.tabs.len()),
        model.showtabline,
    )
}

/// Whether the row carries anything a person cannot read elsewhere: a
/// second tabpage, two or more listed buffers under `"buffers"`, a remote
/// host, or an agent that is doing something.
///
/// O(1): counts and flags with no allocation, since `update()` reads it on
/// every fold.
#[must_use]
pub fn has_unique_content(model: &Model) -> bool {
    let tabs = model.engine.tabline.as_ref().map_or(0, |t| t.tabs.len());
    tabs > 1
        || (model.tabline_shows == TablineShows::Buffers && model.buffers.len() >= 2)
        || model.remote.is_some()
        || !matches!(
            agent_word(model.ai_panel(), model.ai_enabled, model.ai_trusted),
            "" | "idle"
        )
}

/// [`shows`] from the five answers it reads, for the callers that have
/// them before there is a model to ask: the spawn geometry, which is seeded
/// a row shorter so the child is laid out against the grid the attach will
/// ask for, and the oracle's reference session.
///
/// `showtabline` is read the way nvim reads it: `0` keeps the row off, `1`
/// shows it once a second tabpage is open, and anything from `2` up shows
/// it always. Under tiles it decides nothing, and `unique` decides instead.
#[must_use]
pub fn row_shows(
    owns_tabline: bool,
    panes: Panes,
    unique: bool,
    tabs: usize,
    showtabline: u8,
) -> bool {
    let nvim_would_draw_it = match showtabline {
        0 => false,
        1 => tabs > 1,
        _ => true,
    };
    owns_tabline
        && match panes {
            Panes::Tiles => unique,
            _ => nvim_would_draw_it,
        }
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
            caps: PillCaps::Round,
            margin: 0,
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
        // two pills of seven cells and the blank between them in a row of
        // twenty: two blank columns on the left, three on the right
        let slots = pill.slots(20);
        assert_eq!(slots.len(), 2);
        assert_eq!((slots[0].col, slots[0].cells), (2, 7));
        assert_eq!((slots[1].col, slots[1].cells), (10, 7));
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
        // ten pills of six cells each in a row of 44: six fit with the
        // blanks between them, and the last one is four names past the cut
        let scrolled = names(9).slots(44);
        assert_eq!(scrolled.len(), 6);
        assert_eq!(scrolled.first().map(|slot| slot.id), Some(5));
        let last = scrolled.last().copied().unwrap();
        assert_eq!(last.id, 10);
        assert!(last.current);

        // a current name inside the run the first name starts leaves the
        // row where it was
        let anchored = names(0).slots(44);
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
        // two pills of eight need seventeen columns with the blank between
        assert_eq!(pill.slots(17).len(), 2);
        assert_eq!(pill.slots(16).len(), 1);
        assert_eq!(pill.slots(7).len(), 0);
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
            agent: "running",
            caps: PillCaps::Round,
            margin: 0,
            width: 30,
        };
        let slots = pill.slots(30);
        // 7 for the host and 11 for the agent pill, each with a blank on
        // its inner side, the name centred in the 10 between
        assert_eq!((slots[0].col, slots[0].cells), (9, 7));
    }

    /// One row of 80 under tiles: the host pill `prod` on columns 1..=8,
    /// the agent pill `running` on 68..=78, and the two names centred in
    /// the 57 columns between the blanks beside them.
    #[test]
    fn an_eighty_column_row_places_every_pill_on_its_own_cells() {
        let pill = PillView {
            host: "prod".to_string(),
            entries: vec![
                PillEntry {
                    id: 1,
                    label: "a".to_string(),
                    current: true,
                },
                PillEntry {
                    id: 2,
                    label: "bb".to_string(),
                    current: false,
                },
            ],
            names: PillNames::Tabs,
            agent: "running",
            caps: PillCaps::Round,
            margin: 1,
            width: 80,
        };
        assert_eq!((pill.host_col(), edge_cells(&pill.host)), (1, 8));
        assert_eq!((pill.agent_col(), edge_cells(pill.agent)), (68, 11));
        let slots = pill.row_slots();
        let placed: Vec<_> = slots.iter().map(|slot| (slot.col, slot.cells)).collect();
        assert_eq!(placed, vec![(32, 5), (38, 6)]);
        assert_eq!(pill.hit(37), None, "the blank between two pills");
        assert_eq!(pill.hit(32), Some(1), "the first pill's left end");
        assert_eq!(pill.hit(43), Some(2), "the second pill's right end");
        assert_eq!(pill.hit(4), None, "the host pill");
        assert_eq!(pill.hit(70), None, "the agent pill");
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
        assert_eq!(pill.hit(1), None);
        assert_eq!(pill.hit(2), Some(7));
        assert_eq!(pill.hit(8), Some(7));
        assert_eq!(pill.hit(9), None);
        assert_eq!(pill.hit(10), Some(9));
        assert_eq!(pill.hit(16), Some(9));
        assert_eq!(pill.hit(17), None);
    }

    #[test]
    fn a_name_is_measured_in_cells_so_a_decomposed_accent_takes_no_column() {
        let pill = view(vec![PillEntry {
            id: 1,
            label: "cafe\u{301}.rs".to_string(),
            current: true,
        }]);
        assert_eq!(pill.slots(20)[0].cells, 11);
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

    /// What the agent is doing, as the walk below sets it up.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Agent {
        Disabled,
        Idle,
        Running,
        Waiting,
        Crashed,
    }

    /// Under tiles the row stands exactly while it says something the
    /// frames do not: a second tabpage, two listed buffers under
    /// `"buffers"`, a remote host, or an agent word past `idle`. Under
    /// `panes = "nvim"` the user's own `showtabline` decides it, since a
    /// threshold hardcoded to a second tabpage took the always-on row away
    /// from a user who had set 2 and gave a row to one who had set 0. A
    /// session that left the tab line with nvim draws no row in either.
    #[test]
    fn the_row_shows_exactly_when_it_carries_something_unique() {
        let agents = [
            Agent::Disabled,
            Agent::Idle,
            Agent::Running,
            Agent::Waiting,
            Agent::Crashed,
        ];
        let mut walked = 0;
        for (tabs, buffers, shows_buffers, remote) in (0..=3_usize).flat_map(|tabs| {
            (0..=3_u64).flat_map(move |buffers| {
                [false, true].into_iter().flat_map(move |shows_buffers| {
                    [false, true]
                        .into_iter()
                        .map(move |remote| (tabs, buffers, shows_buffers, remote))
                })
            })
        }) {
            for agent in agents {
                let unique = tabs > 1
                    || (shows_buffers && buffers >= 2)
                    || remote
                    || matches!(agent, Agent::Running | Agent::Waiting | Agent::Crashed);
                for (panes, showtabline, owns) in
                    [Panes::Tiles, Panes::Nvim].into_iter().flat_map(|panes| {
                        (0..=2_u8).flat_map(move |showtabline| {
                            [false, true]
                                .into_iter()
                                .map(move |owns| (panes, showtabline, owns))
                        })
                    })
                {
                    let model = walked_model(
                        (tabs, buffers, shows_buffers, remote),
                        agent,
                        (panes, showtabline, owns),
                    );
                    let nvim_would = showtabline == 2 || (showtabline == 1 && tabs > 1);
                    let expected = owns
                        && if panes == Panes::Tiles {
                            unique
                        } else {
                            nvim_would
                        };
                    let case = format!(
                        "tabs={tabs} buffers={buffers} shows_buffers={shows_buffers} \
                         remote={remote} agent={agent:?} panes={panes:?} \
                         showtabline={showtabline} owns={owns}"
                    );
                    assert_eq!(shows(&model), expected, "{case}");
                    assert_eq!(has_unique_content(&model), unique, "{case}");
                    assert_eq!(model.chrome_rows(), u16::from(expected), "{case}");
                    walked += 1;
                }
            }
        }
        assert_eq!(walked, 4 * 4 * 2 * 2 * 5 * 2 * 3 * 2);
    }

    /// One model of the walk above, built from its three groups of answers.
    fn walked_model(
        (tabs, buffers, shows_buffers, remote): (usize, u64, bool, bool),
        agent: Agent,
        (panes, showtabline, owns): (Panes, u8, bool),
    ) -> Model {
        let shows = if shows_buffers {
            TablineShows::Buffers
        } else {
            TablineShows::Tabs
        };
        let mut model = Model::with_term_size(80, 24)
            .with_remote(remote.then(|| "prod".to_string()))
            .with_tabline_shows(shows);
        model.look = crate::model::Look::new(panes, true);
        model.showtabline = showtabline;
        model.attach_surfaces(if owns {
            crate::native::ext::ALL_MULTIGRID.to_vec()
        } else {
            crate::native::ext::shipped_multigrid()
        });
        if tabs > 0 {
            let names: Vec<String> = (1..=tabs).map(|at| format!("t{at}")).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            model.engine.tabline = Some(tabline(1, &names));
        }
        model.buffers = (1..=buffers)
            .map(|buf| buffer(buf, "a.rs", buf == 1))
            .collect();
        let panel = model.ai_panel_mut();
        match agent {
            Agent::Disabled | Agent::Idle => {}
            Agent::Running => panel.session_id = Some("s-1".to_string()),
            Agent::Waiting => {
                panel.pending_permission = Some(crate::native::ai_panel::PermissionPrompt::new(
                    1,
                    "call-1",
                    None,
                    None,
                    Vec::new(),
                ));
            }
            Agent::Crashed => panel.local_error = Some("gone".to_string()),
        }
        model.ai_enabled = agent != Agent::Disabled;
        model
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
