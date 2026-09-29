//! The top pill: the row above everything else, naming what is open.
//!
//! Placement is [`PillView::row_slots`]'s, the same answer the mouse router
//! spends, so the name a click selects is the name that was drawn under
//! the pointer.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use view_core::native::pill::{edge_cells, PillCaps, PillView, AGENT_CRASHED, AGENT_WAITING};
use view_core::theme::{ChromeGroup, ResolvedStyle, Theme};

use super::text::{cluster_width, clusters, set_cluster};
use super::{ratatui_style, rgb};

/// Draws the pill across `area`, which is the terminal's own top row.
///
/// The row is filled in `Normal` first, so the columns between pills read
/// as the buffer's own background and each pill stands on it by its own
/// colour.
///
/// Laid out on [`PillView::width`], the terminal's own width, and written
/// only into the cells `area` holds. An `area` the compositor clipped
/// narrower (the one frame between a resize reaching the model and
/// reaching the backend) loses the columns off its right edge and moves
/// none of the others, so a name a click reaches is the name that was
/// drawn under the pointer.
pub(super) fn paint_pill(pill: &PillView, theme: &Theme, area: Rect, buf: &mut Buffer) {
    let normal = theme.normal();
    fill_run(buf, area, 0, area.width, ratatui_style(normal));
    let tab = theme.chrome(ChromeGroup::TabLine);
    // a role's colour over the tab pill's own background, whatever
    // `TabLine`'s foreground is: the host says which machine the session is
    // on, and a colorscheme that dims the tab row would take it down with
    // the rest
    let edge = |fg: Option<u32>| ResolvedStyle {
        fg: fg.or(tab.fg),
        ..tab
    };
    let ends = Ends {
        caps: pill.caps,
        normal,
    };
    let host = edge_cells(&pill.host);
    if host > 0 {
        let style = edge(theme.accent().fg);
        ends.draw(buf, area, (pill.host_col(), host), &pill.host, style);
    }
    let agent = edge_cells(pill.agent);
    if agent > 0 {
        let fg = match pill.agent {
            AGENT_WAITING => theme.chrome(ChromeGroup::WarningMsg).fg,
            AGENT_CRASHED => theme.chrome(ChromeGroup::ErrorMsg).fg,
            _ => theme.accent().fg,
        };
        ends.draw(buf, area, (pill.agent_col(), agent), pill.agent, edge(fg));
    }

    let selected = theme.chrome(ChromeGroup::TabLineSel);
    for slot in pill.row_slots() {
        // by the index the slot names, never by the loop's own count: a
        // list longer than the row is a window into it, and its first slot
        // is not its first entry
        let Some(entry) = pill.entries.get(slot.entry) else {
            continue;
        };
        let style = if slot.current { selected } else { tab };
        ends.draw(buf, area, (slot.col, slot.cells), &entry.label, style);
    }
}

/// How a pill's two end cells are drawn on this row.
struct Ends {
    caps: PillCaps,
    normal: ResolvedStyle,
}

impl Ends {
    /// Draws one pill of `cells` columns from `col`: its body in `pill`,
    /// the word two columns in, and an end cell at each side.
    fn draw(
        &self,
        buf: &mut Buffer,
        area: Rect,
        (col, cells): (u16, u16),
        text: &str,
        pill: ResolvedStyle,
    ) {
        let body = ratatui_style(pill);
        fill_run(buf, area, col, cells, body);
        write_at(buf, area, col.saturating_add(2), text, body);
        // a flat end is the blank the body fill already left
        let Some(end) = self.round_end(pill) else {
            return;
        };
        let (left, right) = self.caps.ends();
        write_at(buf, area, col, left, end);
        write_at(
            buf,
            area,
            col.saturating_add(cells).saturating_sub(1),
            right,
            end,
        );
    }

    /// The style a round end is drawn in: the pill's own visible
    /// background as the glyph's colour, over the row's `Normal`
    /// background. `None` under flat ends, and for a pill whose background
    /// is the terminal's own default, which no foreground can name.
    fn round_end(&self, pill: ResolvedStyle) -> Option<Style> {
        if self.caps != PillCaps::Round {
            return None;
        }
        // a reversed style shows its foreground as the background, and an
        // unset one is the terminal's default foreground, which `Reset`
        // names on the glyph as well
        let fg = if pill.reverse {
            pill.fg.map_or(Color::Reset, rgb)
        } else {
            rgb(pill.bg.or(self.normal.bg)?)
        };
        Some(
            Style::default()
                .fg(fg)
                .bg(self.normal.bg.map_or(Color::Reset, rgb)),
        )
    }
}

/// Paints `cells` blank columns from `col`, which is what gives a selected
/// name its own background either side of the text.
///
/// `ratatui::buffer::Cell::reset` leaves a blank behind, so this writes no
/// symbol of its own and the names are the only text the row carries.
fn fill_run(buf: &mut Buffer, area: Rect, col: u16, cells: u16, style: Style) {
    for at in col..col.saturating_add(cells).min(area.width) {
        let cell = &mut buf[(area.x.saturating_add(at), area.y)];
        cell.reset();
        cell.set_style(style);
    }
}

/// Writes `text` from column `col`, one grapheme cluster per cell,
/// stopping at the row's own end.
fn write_at(buf: &mut Buffer, area: Rect, col: u16, text: &str, style: Style) {
    let mut at = col;
    for cluster in clusters(text) {
        let width = cluster_width(cluster);
        if at.saturating_add(width) > area.width {
            return;
        }
        set_cluster(buf, area.x.saturating_add(at), area.y, cluster, style);
        // ratatui's own convention for the cell a two-cell glyph covers:
        // the diff skips it, and anything left in it would be drawn one
        // column right of where it was written
        if width == 2 {
            buf[(area.x.saturating_add(at).saturating_add(1), area.y)].reset();
        }
        at = at.saturating_add(width);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use view_core::model::Model;

    use super::{paint_pill, Buffer, PillCaps, PillView, Rect};
    use crate::paint::{ratatui_style, rgb, ChromeGroup, Theme};

    /// The accent this fixture names, which is what the host cell has to
    /// carry over the row's own foreground.
    const ACCENT: u32 = 0x44_44_44;

    /// The row's own background, which is `Normal`'s.
    const NORMAL_BG: u32 = 0x0f_0f_0f;

    /// A session on a remote host with two tabpages on a terminal `width`
    /// cells wide, `Normal` and the two pill groups three different colours
    /// so a cell says which one painted it.
    ///
    /// The terminal's width is the row's own layout width, so a fixture
    /// that left it at zero would place every name nowhere.
    fn two_tabs(width: u16) -> Model {
        let mut model = Model::with_term_size(width, 24).with_remote(Some("prod".to_string()));
        model.engine.set_accent_token(Some(ACCENT));
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::Redraw(vec![view_core::events::UiEvent::DefaultColorsSet {
                fg: Some(0xee_ee_ee),
                bg: Some(NORMAL_BG),
                sp: None,
            }]),
        );
        for (id, group, fg, bg) in [
            (1_u64, ChromeGroup::TabLine, 0x11_11_11_u32, 0xa1_a1_a1_u32),
            (2, ChromeGroup::TabLineSel, 0x22_22_22, 0xa2_a2_a2),
        ] {
            for event in [
                view_core::events::UiEvent::HlAttrDefine {
                    id,
                    fg: Some(fg),
                    bg: Some(bg),
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                view_core::events::UiEvent::HlGroupSet {
                    name: group.hl_name().to_string(),
                    hl_id: id,
                },
            ] {
                let _ =
                    view_core::update::update(&mut model, view_core::msg::Msg::Redraw(vec![event]));
            }
        }
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::Redraw(vec![view_core::events::UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(2),
                tabs: vec![
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(1),
                        name: "one".to_string(),
                    },
                    view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(2),
                        name: "two".to_string(),
                    },
                ],
            }]),
        );
        model
    }

    /// Every `.rs` file under `path`, however deep.
    fn sources(path: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let at = entry.path();
            if at.is_dir() {
                sources(&at, found);
            } else if at.extension().is_some_and(|ext| ext == "rs") {
                found.push(at);
            }
        }
    }

    /// One painter for the top row, under either look. A second one drew
    /// the same names left-aligned from column 0 while the mouse router
    /// hit-tested the centred layout, so a click on that row selected a
    /// tabpage the user was not pointing at.
    #[test]
    fn no_second_painter_for_the_top_row_remains() {
        // spelled in halves so this pin is not its own match
        let gone = concat!("paint_", "tabline");
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("every crate sits inside the crates directory");
        let mut files = Vec::new();
        sources(crates, &mut files);
        let carriers: Vec<_> = files
            .iter()
            .filter(|at| std::fs::read_to_string(at).is_ok_and(|text| text.contains(gone)))
            .collect();
        assert!(
            carriers.is_empty(),
            "a second painter for the top row is back in {carriers:?}"
        );
    }

    /// A row too narrow for every name is a window into the list, so its
    /// first run is not the first entry. Each run paints the name at the
    /// index it names; walking the list in order put a neighbour's name
    /// under every one of them.
    #[test]
    fn a_row_too_narrow_for_the_list_paints_the_names_its_slots_name() {
        // room for two of the four names beside the host's pill, so the
        // window holds the current one and the one before it
        let width = 28;
        let mut model = two_tabs(width);
        model.ai_enabled = false;
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::Redraw(vec![view_core::events::UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(4),
                tabs: (1..=4)
                    .map(|at| view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(at),
                        name: format!("name{at}"),
                    })
                    .collect(),
            }]),
        );
        let theme = Theme::from_hl(model.engine.hl());
        let pill = PillView::from_model(&model);
        let slots = pill.row_slots();
        assert_eq!(slots.len(), 2, "the fixture drew {} names", slots.len());
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| paint_pill(&pill, &theme, frame.area(), frame.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let row: String = (0..width).map(|col| buf[(col, 0)].symbol()).collect();
        assert_eq!(
            row.trim(),
            "prod     name3     name4",
            "the row reads {row:?} where its slots name entries 2 and 3"
        );
    }

    /// The row the mouse router hit-tests is the row the painter draws,
    /// on the terminal's own width. A painter that laid the names out on
    /// the area handed to it re-centred them for the one frame between a
    /// resize reaching the model and reaching the backend, and a click in
    /// that frame landed on a name drawn at another column.
    #[test]
    fn a_clipped_row_paints_the_columns_the_router_would_hit() {
        let model = two_tabs(40);
        let theme = Theme::from_hl(model.engine.hl());
        let pill = PillView::from_model(&model);
        let row = |area: u16| {
            let mut terminal = Terminal::new(TestBackend::new(area, 1)).unwrap();
            terminal
                .draw(|frame| paint_pill(&pill, &theme, frame.area(), frame.buffer_mut()))
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            (0..area)
                .map(|col| buf[(col, 0)].symbol().to_string())
                .collect::<Vec<_>>()
        };

        let clipped = 24;
        assert_eq!(
            row(clipped),
            row(40)[..usize::from(clipped)],
            "a row clipped to {clipped} columns re-flowed the names"
        );
        // the router's own reading of the same view: every column a slot
        // covers answers with that slot's id
        for slot in pill.row_slots() {
            for col in slot.col..slot.col.saturating_add(slot.cells) {
                assert_eq!(
                    pill.hit(col),
                    Some(slot.id),
                    "column {col} was painted for tab {} and hit-tests as something else",
                    slot.id
                );
            }
        }
    }

    /// The bool on the slot is not the colour on the screen: only this
    /// composite says the current name is the one lit, and the goldens
    /// carry text without styling.
    #[test]
    fn the_current_name_is_the_one_the_selected_group_paints() {
        let model = two_tabs(40);
        let theme = Theme::from_hl(model.engine.hl());
        let pill = PillView::from_model(&model);
        let slots = pill.row_slots();
        assert_eq!(slots.len(), 2, "the fixture drew {} names", slots.len());
        let mut terminal = Terminal::new(TestBackend::new(40, 1)).unwrap();
        terminal
            .draw(|frame| paint_pill(&pill, &theme, frame.area(), frame.buffer_mut()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let fg_at = |col: u16| buf[(col, 0)].style().fg;

        let plain = ratatui_style(theme.chrome(ChromeGroup::TabLine)).fg;
        let lit = ratatui_style(theme.chrome(ChromeGroup::TabLineSel)).fg;
        assert_ne!(plain, lit, "the fixture gave the two groups one colour");
        for col in slots[0].col..slots[0].col + slots[0].cells {
            assert_eq!(fg_at(col), plain, "column {col} of the first name");
        }
        for col in slots[1].col..slots[1].col + slots[1].cells {
            assert_eq!(fg_at(col), lit, "column {col} of the current name");
        }
        assert_eq!(
            fg_at(pill.host_col() + 2),
            Some(rgb(ACCENT)),
            "the host's word is drawn in the accent"
        );
        assert_eq!(
            buf[(slots[0].col - 1, 0)].style().bg,
            Some(rgb(NORMAL_BG)),
            "the gap before the first name is not Normal's background"
        );
        assert_eq!(
            (
                buf[(pill.host_col() + 2, 0)].symbol(),
                buf[(slots[1].col + 2, 0)].symbol()
            ),
            ("p", "t"),
            "the host and the current name are not where the slots put them"
        );
    }

    /// A word that needs the person reading the row stands in the group
    /// nvim gives that urgency, any other word in the accent, and an idle
    /// agent draws no word at all.
    #[test]
    fn the_agent_word_is_coloured_by_what_it_asks_of_the_reader() {
        let mut model = two_tabs(40);
        assert_eq!(
            view_core::native::pill::agent_word(
                model.ai_panel(),
                model.ai_enabled,
                model.ai_trusted
            ),
            view_core::native::pill::AGENT_IDLE
        );
        assert_eq!(PillView::from_model(&model).agent, "", "the idle agent");
        for (id, group, fg) in [
            (3_u64, ChromeGroup::WarningMsg, 0x33_33_33_u32),
            (4, ChromeGroup::ErrorMsg, 0x55_55_55),
        ] {
            for event in [
                view_core::events::UiEvent::HlAttrDefine {
                    id,
                    fg: Some(fg),
                    bg: None,
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                view_core::events::UiEvent::HlGroupSet {
                    name: group.hl_name().to_string(),
                    hl_id: id,
                },
            ] {
                let _ =
                    view_core::update::update(&mut model, view_core::msg::Msg::Redraw(vec![event]));
            }
        }
        let theme = Theme::from_hl(model.engine.hl());
        for (word, fg) in [
            (super::AGENT_WAITING, 0x33_33_33),
            (super::AGENT_CRASHED, 0x55_55_55),
            ("running", ACCENT),
        ] {
            let mut pill = PillView::from_model(&model);
            pill.agent = word;
            let buf = painted(&pill, &theme);
            assert_eq!(
                buf[(pill.agent_col() + 2, 0)].style().fg,
                Some(rgb(fg)),
                "the agent word {word:?}"
            );
        }
    }

    /// Paints `pill` into a buffer as wide as its own row.
    fn painted(pill: &PillView, theme: &Theme) -> Buffer {
        let area = Rect::new(0, 0, pill.width, 1);
        let mut buf = Buffer::empty(area);
        paint_pill(pill, theme, area, &mut buf);
        buf
    }

    /// Which columns a pill painted: its body stands off `Normal`'s
    /// background, and a round end stands on it as a glyph.
    fn pill_cells(buf: &Buffer, width: u16) -> Vec<bool> {
        let (left, right) = PillCaps::Round.ends();
        (0..width)
            .map(|col| {
                let cell = &buf[(col, 0)];
                cell.style().bg != Some(rgb(NORMAL_BG))
                    || cell.symbol() == left
                    || cell.symbol() == right
            })
            .collect()
    }

    /// Each run of painted cells, as its first column, its width and the
    /// word drawn in it.
    fn runs(buf: &Buffer, width: u16) -> Vec<(u16, u16, String)> {
        let cells = pill_cells(buf, width);
        let (left, right) = PillCaps::Round.ends();
        let mut found = Vec::new();
        let mut col = 0;
        while col < width {
            if !cells[usize::from(col)] {
                col += 1;
                continue;
            }
            let start = col;
            let mut word = String::new();
            while col < width && cells[usize::from(col)] {
                let symbol = buf[(col, 0)].symbol();
                if symbol != left && symbol != right {
                    word.push_str(symbol);
                }
                col += 1;
            }
            found.push((start, col - start, word.trim().to_string()));
        }
        found
    }

    /// `count` tabpages named `n01`, `n02`, ... with handle `index + 1`,
    /// the one at `current` the session's own.
    fn listed(count: u64, current: u64) -> PillView {
        let mut model = two_tabs(80);
        let _ = view_core::update::update(
            &mut model,
            view_core::msg::Msg::Redraw(vec![view_core::events::UiEvent::TablineUpdate {
                current: view_core::events::TabHandle(current + 1),
                tabs: (0..count)
                    .map(|at| view_core::events::TabEntry {
                        tab: view_core::events::TabHandle(at + 1),
                        name: format!("n{:02}", at + 1),
                    })
                    .collect(),
            }]),
        );
        PillView::from_model(&model)
    }

    /// Every column of every row a person can see answers a click with
    /// the name painted under it, or with nothing where no name is. A
    /// router and a painter that each did their own arithmetic agreed on
    /// the widths a fixture happened to pick and nowhere else.
    #[test]
    fn every_click_on_a_pill_selects_the_name_painted_under_it() {
        let base = two_tabs(80);
        let theme = Theme::from_hl(base.engine.hl());
        let mut walked = 0_u32;
        for count in 1..=12 {
            for current in 0..count {
                let listed = listed(count, current);
                for (host, agent) in [("", ""), ("prod", ""), ("", "running"), ("prod", "waiting")]
                {
                    for caps in [PillCaps::Round, PillCaps::Flat] {
                        for width in 10..=120 {
                            let mut pill = listed.clone();
                            pill.host = host.to_string();
                            pill.agent = agent;
                            pill.caps = caps;
                            pill.width = width;
                            let buf = painted(&pill, &theme);
                            let mut owner = vec![None; usize::from(width)];
                            for (start, cells, word) in runs(&buf, width) {
                                let id = word.strip_prefix('n').and_then(|n| n.parse::<u64>().ok());
                                for col in start..start + cells {
                                    owner[usize::from(col)] = id;
                                }
                            }
                            for col in 0..width {
                                assert_eq!(
                                    pill.hit(col),
                                    owner[usize::from(col)],
                                    "{count} names, current {current}, host {host:?}, agent \
                                     {agent:?}, {caps:?}, width {width}: column {col}"
                                );
                            }
                            walked += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(walked, 78 * 4 * 2 * 111, "the walk skipped cases");
    }

    /// Flat ends are a blank in the pill's own colour where a round end is
    /// a glyph, so switching the key moves no name and no click.
    #[test]
    fn flat_caps_take_the_cells_round_caps_take() {
        let base = two_tabs(80);
        let theme = Theme::from_hl(base.engine.hl());
        let mut round = listed(3, 1);
        round.agent = "running";
        round.caps = PillCaps::Round;
        let mut flat = round.clone();
        flat.caps = PillCaps::Flat;
        let (round_buf, flat_buf) = (painted(&round, &theme), painted(&flat, &theme));
        assert_eq!(
            pill_cells(&round_buf, 80),
            pill_cells(&flat_buf, 80),
            "the two cap modes painted different cells"
        );
        assert_eq!(round.row_slots(), flat.row_slots());
        let (left, right) = PillCaps::Round.ends();
        for slot in round.row_slots() {
            let last = slot.col + slot.cells - 1;
            assert_eq!(
                (
                    round_buf[(slot.col, 0)].symbol(),
                    round_buf[(last, 0)].symbol()
                ),
                (left, right),
                "round ends of the name at {}",
                slot.col
            );
            assert_eq!(
                (
                    flat_buf[(slot.col, 0)].symbol(),
                    flat_buf[(last, 0)].symbol()
                ),
                (" ", " "),
                "flat ends of the name at {}",
                slot.col
            );
            assert_eq!(
                flat_buf[(slot.col, 0)].style().bg,
                flat_buf[(slot.col + 2, 0)].style().bg,
                "a flat end is not in its pill's colour"
            );
        }
    }
}
