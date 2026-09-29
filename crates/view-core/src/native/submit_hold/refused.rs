//! An error nvim reports for the line that armed a hold, which says the
//! line ran nothing and no notification will follow.

use std::collections::HashMap;

use crate::events::UiEvent;
use crate::grid::registry::{GridId, GLOBAL_GRID};
use crate::hl::{HlAttr, HlTable};
use crate::model::Model;
use crate::native::ext::Ext;
use crate::native::speculate::is_cmdline_mode;
use crate::native::text::text_width;

/// The global grid's top row of nvim's message area once the submitted
/// `line` is drawn, which is the command line's own top row: the row the
/// cursor stands on, or the grid's last row while the keys that open the
/// command line are still on their way, less the rows `line` wraps onto
/// after its `:`. nvim grows the command line upward from where it opened.
/// An external command line takes no row, and the message area starts on
/// the last one.
///
/// A line whose text view does not know is taken as wrapping onto no row,
/// so an error drawn above that estimate waits for the backstop.
pub(super) fn message_top(model: &Model, line: Option<&str>) -> u16 {
    let grid = model.engine.grids().global();
    let (width, height) = grid.size();
    let bottom = height.saturating_sub(1);
    if model.owns(Ext::Cmdline) {
        return bottom;
    }
    let row = if is_cmdline_mode(&model.engine.mode.current) {
        grid.cursor().0
    } else {
        bottom
    };
    let wrapped = line.map_or(0, |line| {
        text_width(line)
            .saturating_add(1)
            .checked_div(width)
            .unwrap_or(0)
    });
    row.saturating_sub(wrapped)
}

/// Whether `events` report an error for the submitted line and then leave
/// the command line: a `msg_show` of kind `emsg`, or a line nvim draws
/// into its own message area whose first cell, and its row's column 0,
/// are drawn as an error. nvim reads a line typed ahead as soon as the
/// one before it ends, so a mode change into the command line after an
/// error says that error was the earlier line's. nvim reports modes in
/// every attach.
///
/// On the global grid only the rows at or below `message_top` are the
/// message area, as [`message_top`] found it when the hold armed, and a
/// scroll of the rows down to the grid's bottom moves its top up to the
/// scroll's own. Every other row is a window's, where a sign can be drawn
/// in `ErrorMsg`.
///
/// Costs one pass over the batch, one more for its highlight definitions
/// at the first line drawn into the message area, and a look back for
/// column 0 on an error line that starts past it.
pub(super) fn reports_error(model: &Model, events: &[UiEvent], message_top: u16) -> bool {
    let grids = model.engine.grids();
    let height = u64::from(grids.global().size().1);
    let mut top = u64::from(message_top);
    let mut error = None;
    let mut reported = false;
    for (at, event) in events.iter().enumerate() {
        match event {
            UiEvent::ModeChange { mode, .. } if is_cmdline_mode(mode) => reported = false,
            UiEvent::ModeChange { .. } if reported => return true,
            UiEvent::MsgShow { kind, .. } if kind == "emsg" => reported = true,
            UiEvent::GridScroll {
                grid,
                top: scrolled,
                bot,
                ..
            } if GridId(*grid) == GLOBAL_GRID && *bot == height => top = top.min(*scrolled),
            UiEvent::GridLine {
                grid,
                row,
                col_start,
                cells,
            } if grids.draws_messages(GridId(*grid))
                && (GridId(*grid) != GLOBAL_GRID || *row >= top) =>
            {
                let Some(first) = cells.first() else {
                    continue;
                };
                let Some(error) = error.get_or_insert_with(|| error_attr(model, events)) else {
                    continue;
                };
                if !error.draws(first.hl_id) {
                    continue;
                }
                // nvim diffs an error against the one the row already
                // shows, and starts the line past the cells they share
                let head = if *col_start == 0 {
                    Some(first.hl_id)
                } else {
                    column_zero(&events[..at], *grid, *row).unwrap_or_else(|| {
                        let row = u16::try_from(*row).ok()?;
                        let cell = grids.grid(GridId(*grid))?.cell(row, 0)?;
                        Some(cell.hl_id)
                    })
                };
                reported |= head.is_some_and(|id| error.draws(id));
            }
            _ => {}
        }
    }
    false
}

/// The highlight column 0 of `row` on `grid` holds once `before` is
/// applied: `Some(None)` where `before` clears, scrolls or resizes the
/// grid, and `None` where it leaves that cell as the grid holds it.
fn column_zero(before: &[UiEvent], grid: u64, row: u64) -> Option<Option<u64>> {
    before.iter().rev().find_map(|event| match event {
        UiEvent::GridLine {
            grid: g,
            row: r,
            col_start: 0,
            cells,
        } if *g == grid && *r == row => Some(cells.first().map(|cell| cell.hl_id)),
        UiEvent::GridClear { grid: g }
        | UiEvent::GridScroll { grid: g, .. }
        | UiEvent::GridResize { grid: g, .. }
            if *g == grid =>
        {
            Some(None)
        }
        _ => None,
    })
}

/// How nvim draws an error in its message area.
struct ErrorAttr<'a> {
    id: u64,
    /// `ErrorMsg` laid over `MsgArea`: where a colorscheme colours the
    /// message area, nvim draws the error in a combined id `hl_group_set`
    /// never names.
    drawn: HlAttr,
    hl: &'a HlTable,
    /// The attributes the batch defines, which is applied after this
    /// reads it.
    defined: HashMap<u64, HlAttr>,
}

impl ErrorAttr<'_> {
    fn draws(&self, hl_id: u64) -> bool {
        hl_id == self.id
            || self
                .defined
                .get(&hl_id)
                .copied()
                .or_else(|| self.hl.attr(hl_id))
                == Some(self.drawn)
    }
}

/// How `events` and the highlights already defined draw an error, or
/// `None` before nvim has named `ErrorMsg`.
fn error_attr<'a>(model: &'a Model, events: &[UiEvent]) -> Option<ErrorAttr<'a>> {
    let hl = model.engine.hl();
    let id = hl.group("ErrorMsg")?;
    let mut defined = HashMap::new();
    for event in events {
        if let UiEvent::HlAttrDefine {
            id,
            fg,
            bg,
            bold,
            italic,
            underline,
            reverse,
        } = *event
        {
            defined.entry(id).or_insert(HlAttr {
                fg,
                bg,
                bold,
                italic,
                underline,
                reverse,
            });
        }
    }
    let attr = |id: u64| defined.get(&id).copied().or_else(|| hl.attr(id));
    let error = attr(id)?;
    let drawn = hl
        .group("MsgArea")
        .and_then(attr)
        .map_or(error, |area| HlAttr {
            fg: error.fg.or(area.fg),
            bg: error.bg.or(area.bg),
            bold: error.bold || area.bold,
            italic: error.italic || area.italic,
            underline: error.underline || area.underline,
            reverse: error.reverse || area.reverse,
        });
    Some(ErrorAttr {
        id,
        drawn,
        hl,
        defined,
    })
}
