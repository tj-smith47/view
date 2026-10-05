//! An error nvim reports for the line that armed a hold, which says the
//! line ran nothing and no notification will follow.

use std::collections::HashMap;

use crate::events::UiEvent;
use crate::grid::registry::GridId;
use crate::hl::{HlAttr, HlTable};
use crate::model::Model;
use crate::native::speculate::is_cmdline_mode;

/// Whether `events` report an error for the submitted line and then leave
/// the command line: a `msg_show` of kind `emsg` or one drawn in
/// `ErrorMsg`, or a line nvim draws into its own message area whose first
/// cell, and its row's column 0, are drawn as an error. A view command
/// that refuses to run sends no invocation and echoes why in `ErrorMsg`,
/// so its line ends here. nvim reads a line typed ahead as soon as the
/// one before it ends, so a mode change into the command line after an
/// error says that error was the earlier line's. nvim reports modes in
/// every attach.
///
/// Every row of a grid nvim draws messages on counts, window rows
/// included. A sign drawn in `ErrorMsg` on a window row nvim redraws as it
/// leaves a valid `:View` line reaches this only after the invocation has
/// ended the hold, because the runtime drains no redraw nvim sent after
/// an invocation before it has applied that invocation.
///
/// Costs one pass over the batch, one more for its highlight definitions
/// at the first message or line drawn into the message area, and a look
/// back for column 0 on an error line that starts past it.
pub(super) fn reports_error(model: &Model, events: &[UiEvent]) -> bool {
    let grids = model.engine.grids();
    let mut error = None;
    let mut reported = false;
    for (at, event) in events.iter().enumerate() {
        match event {
            UiEvent::ModeChange { mode, .. } if is_cmdline_mode(mode) => reported = false,
            UiEvent::ModeChange { .. } if reported => return true,
            UiEvent::MsgShow { kind, .. } if kind == "emsg" => reported = true,
            UiEvent::MsgShow { content, .. } => {
                let Some((attr, _)) = content.first() else {
                    continue;
                };
                let error = error.get_or_insert_with(|| error_attr(model, events));
                reported |= error.as_ref().is_some_and(|error| error.draws(*attr));
            }
            UiEvent::GridLine {
                grid,
                row,
                col_start,
                cells,
            } if grids.draws_messages(GridId(*grid)) => {
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
