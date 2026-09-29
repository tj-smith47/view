//! An error nvim reports for the line that armed a hold, which says the
//! line ran nothing and no notification will follow.

use crate::events::UiEvent;
use crate::grid::registry::GridId;
use crate::hl::HlAttr;
use crate::model::Model;
use crate::native::ext::Ext;

/// Whether `events` report an error once nvim has hidden every line the
/// hold counts ended: a `msg_show` of kind `emsg`, or a line nvim draws
/// into its own message area opening in the highlight it draws errors in.
/// An error before those hides belongs to a line typed ahead of the one
/// that armed the hold. Without `ext_cmdline` nvim sends no hide, and the
/// first error counts.
pub(super) fn reports_error(model: &Model, events: &[UiEvent]) -> bool {
    let mut unhidden = if model.owns(Ext::Cmdline) {
        model.submit_hold.unhidden
    } else {
        0
    };
    events.iter().any(|event| match event {
        UiEvent::CmdlineHide { level: 1 } => {
            unhidden = unhidden.saturating_sub(1);
            false
        }
        UiEvent::MsgShow { kind, .. } => unhidden == 0 && kind == "emsg",
        UiEvent::GridLine {
            grid,
            col_start: 0,
            cells,
            ..
        } => {
            unhidden == 0
                && model.engine.grids().draws_messages(GridId(*grid))
                && cells
                    .first()
                    .is_some_and(|cell| drawn_as_error(model, events, cell.hl_id))
        }
        _ => false,
    })
}

/// Whether a message-area cell drawn in `hl_id` is drawn as an error.
/// nvim lays `ErrorMsg` over `MsgArea`, so where a colorscheme colours the
/// message area the cell's id is a combination `hl_group_set` never names,
/// and its attributes are compared instead. The batch defines that id
/// ahead of the line and is applied after this reads it.
fn drawn_as_error(model: &Model, events: &[UiEvent], hl_id: u64) -> bool {
    let hl = model.engine.hl();
    let Some(error_id) = hl.group("ErrorMsg") else {
        return false;
    };
    if hl_id == error_id {
        return true;
    }
    let attr = |id: u64| {
        events
            .iter()
            .find_map(|event| match *event {
                UiEvent::HlAttrDefine {
                    id: defined,
                    fg,
                    bg,
                    bold,
                    italic,
                    underline,
                    reverse,
                } if defined == id => Some(HlAttr {
                    fg,
                    bg,
                    bold,
                    italic,
                    underline,
                    reverse,
                }),
                _ => None,
            })
            .or_else(|| hl.attr(id))
    };
    let Some(error) = attr(error_id) else {
        return false;
    };
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
    attr(hl_id) == Some(drawn)
}
