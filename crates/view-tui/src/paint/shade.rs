//! The colours a native float's frame and its selected row take, derived
//! from the colorscheme's own groups.

use view_core::theme::{ChromeGroup, ResolvedStyle, Theme};

/// A frame's foreground given the style of the surface it encloses: a
/// dimmed variant of that surface's own foreground, or the neutral grey
/// floor when it has none. See [`super::toast::toast_border_color`] for why the floor
/// is a fixed color rather than a dimmed background.
pub(super) fn border_color(interior: ResolvedStyle) -> u32 {
    interior.fg.map_or(0x0080_8080, dim)
}

/// The foreground every frame view draws around a native float takes: the
/// colorscheme's own `FloatBorder` when it states one, and the derived
/// dimmed shade of `interior` only when it states nothing. A derivation is
/// a guess at what the theme's author would have chosen, so a stated answer
/// outranks it -- and a colorscheme that paints its floats transparent
/// states a border color precisely because nothing else is left to read the
/// frame's edge from.
///
/// Not the whole resolved style: the frame keeps the interior's background
/// so the box reads as one continuous surface, the same reason
/// [`ChromeGroup::FloatTitle`]'s background is pinned to it.
pub(super) fn float_border_color(theme: &Theme, interior: ResolvedStyle) -> u32 {
    theme
        .chrome(ChromeGroup::FloatBorder)
        .fg
        .unwrap_or_else(|| border_color(interior))
}

/// The style a selected row takes over an interior styled `base`:
/// `PmenuSel` -- the group a colorscheme already uses for "this row is the
/// one you are on" -- with its background made concrete.
///
/// Reverse video is resolved into colors here rather than sent as an SGR
/// attribute. A colorscheme that never defines `PmenuSel` leaves it on
/// `Theme::emphasis`, which is the reverse flag over the theme's own
/// colors; emitting that as `ESC[7m` gave the user a full-width inverted
/// bar whose color no colorscheme chose, and inverting an *unset*
/// foreground/background inverts whatever the terminal's ambient default
/// happens to be. Swapping the two resolved colors instead paints the same
/// intent in the theme's palette. With neither color known there is nothing
/// to swap, and the flag stays as the one selection signal any terminal can
/// still carry.
pub(super) fn selection_style(theme: &Theme, base: ResolvedStyle) -> ResolvedStyle {
    let sel = theme.float_chrome(ChromeGroup::PmenuSel, base.bg);
    let fg = sel.fg.or(base.fg);
    if !sel.reverse {
        return ResolvedStyle { fg, ..sel };
    }
    if fg.is_none() && sel.bg.is_none() {
        return sel;
    }
    ResolvedStyle {
        fg: sel.bg,
        bg: fg,
        reverse: false,
        ..sel
    }
}

/// Scales each RGB channel of `c` to 60% of its original value, the muted
/// transform [`super::toast::toast_border_color`] applies when no themed group already
/// carries one.
fn dim(c: u32) -> u32 {
    let channel = |shift: u32| -> u32 { ((c >> shift) & 0xFF) * 3 / 5 };
    (channel(16) << 16) | (channel(8) << 8) | channel(0)
}
