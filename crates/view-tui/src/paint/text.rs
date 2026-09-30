//! Native text written into buffer cells one grapheme cluster at a time.
//!
//! The walk and the measure are
//! [`view_core::native::text`](view_core::native::text)'s, so a name
//! measured where a click is routed and a name painted here can never
//! disagree about how many cells it takes. What lives here is the one
//! thing that is not pure: putting a cluster into a terminal cell.

use ratatui::buffer::Buffer;
use ratatui::style::Style;
use view_core::native::views::StyleRole;
use view_core::theme::Theme;

pub(super) use view_core::native::text::{cluster_width, clusters, group_width};

/// Writes one grapheme cluster into a cell as its whole symbol, which is
/// how a combining mark reaches the terminal in the cell its base
/// character stands in.
///
/// A cluster carrying a control character is written as a blank:
/// `ratatui::buffer::Cell::set_symbol` computes the width itself and
/// panics on one in a debug build. The cell is reset before its style is
/// set because `set_style` patches, so a chrome cell painted over the grid
/// would otherwise keep whatever background and modifiers the layer
/// beneath left in it.
pub(super) fn set_cluster(buf: &mut Buffer, x: u16, y: u16, cluster: &str, style: Style) {
    let cell = &mut buf[(x, y)];
    cell.reset();
    if cluster.chars().any(char::is_control) {
        cell.set_symbol(" ");
    } else {
        cell.set_symbol(cluster);
    }
    cell.set_style(style);
}

/// `style` with the foreground [`Theme::role_fg`] gives `role`, and `style`
/// unchanged for a role it colours nothing.
pub(super) fn tint(theme: &Theme, role: StyleRole, style: Style) -> Style {
    theme
        .role_fg(role)
        .map_or(style, |color| style.fg(super::rgb(color)))
}

/// The foreground `role` resolves to on its own: its chrome group's, else
/// the one [`Theme::role_fg`] gives it.
pub(super) fn role_fg(theme: &Theme, role: StyleRole) -> Option<u32> {
    role.chrome_group()
        .map_or_else(|| theme.role_fg(role), |group| theme.chrome(group).fg)
}
