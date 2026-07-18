//! The color vocabulary painting resolves style through, derived purely
//! from the live highlight state nvim's `default_colors_set`/
//! `hl_attr_define`/`hl_group_set` event stream already populates into
//! [`HlTable`]. No separate theme config or RPC query: `Theme::from_hl` is
//! a read of state the engine already streams, so re-deriving it on every
//! `ColorScheme` change costs nothing beyond a handful of `HashMap` lookups.

use crate::hl::HlTable;

/// One fully-resolved cell or chrome style: colors plus text attributes,
/// backend-free so any frontend (not just `ratatui`) can consume it.
/// `reverse` is a flag rather than pre-swapped `fg`/`bg` values so it stays
/// meaningful even when one or both colors are unset: a rendering backend's
/// own reverse-video mode still inverts against its ambient default in that
/// case, where a pre-swap of two `None` values would carry no information
/// at all and silently lose that guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResolvedStyle {
    /// Foreground color, or `None` to inherit the terminal's own default.
    pub fg: Option<u32>,
    /// Background color, or `None` to inherit the terminal's own default.
    pub bg: Option<u32>,
    /// Whether the style renders bold.
    pub bold: bool,
    /// Whether the style renders italic.
    pub italic: bool,
    /// Whether the style renders underlined.
    pub underline: bool,
    /// Whether foreground and background render swapped.
    pub reverse: bool,
}

/// The active colorscheme's resolved design system: the default/"Normal"
/// colors plus the builtin chrome groups nvim's `hl_group_set` event
/// associates a name with (`StatusLine`, the tabline family, the popup-menu
/// family, `MsgArea`), so native chrome renders in the colorscheme's own
/// colors instead of a hardcoded style nvim has no way to reach.
///
/// Deliberately holds resolved *values*, not the live `HlTable` itself:
/// per-cell grid attributes are always resolved fresh against the live
/// table ([`Theme::style_for`], parametrized by `hl_id` per cell -- a fixed
/// set of `Theme` fields could never cover every cell), but the fixed
/// chrome vocabulary below is cheap to resolve once per frame and
/// correspondingly cheap to persist to disk for a themed first paint before
/// that live state exists (see `theme_cache` in the `view` bin crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Theme {
    /// The default/"Normal" foreground color.
    pub fg: Option<u32>,
    /// The default/"Normal" background color.
    pub bg: Option<u32>,
    /// The `StatusLine` builtin group. Consumed today by `view-tui`'s
    /// `paint_shell` (the startup placeholder's statusline bar, painted
    /// before the engine attaches); a full native statusline widget over
    /// live buffer state is still unbuilt, and `Theme`'s contract is to
    /// resolve every builtin group nvim's own `hl_group_set` event names
    /// regardless of how many consumers exist yet.
    pub status_line: ResolvedStyle,
    /// The `TabLine` builtin group: an unselected tab's label.
    pub tab_line: ResolvedStyle,
    /// The `TabLineSel` builtin group: the current tab's label.
    pub tab_line_sel: ResolvedStyle,
    /// The `TabLineFill` builtin group: the tabline row's background beyond
    /// the tab labels themselves.
    pub tab_line_fill: ResolvedStyle,
    /// The `Pmenu` builtin group: an unselected popup-menu row.
    pub pmenu: ResolvedStyle,
    /// The `PmenuSel` builtin group: the selected popup-menu row.
    pub pmenu_sel: ResolvedStyle,
    /// The `MsgArea` builtin group: the message-log overlay's text.
    pub msg_area: ResolvedStyle,
}

impl Theme {
    /// Derives a `Theme` from the engine's live highlight table. Pure and
    /// deterministic: the same `HlTable` state always derives the same
    /// `Theme`, so callers may re-derive on every frame with no history to
    /// track. A named group absent from `hl.groups` -- before the first
    /// `hl_group_set` batch, or under a minimal colorscheme that never
    /// redefines every builtin group -- falls back to [`Theme::normal`] for
    /// an ordinary chrome group or [`Theme::emphasis`] for a
    /// selection-style one (`TabLineSel`, `PmenuSel`), so the
    /// selected/current row stays visibly distinct even with zero color
    /// information rather than degrading to indistinguishable-from-everything-else.
    #[must_use]
    pub fn from_hl(hl: &HlTable) -> Self {
        let mut theme = Self {
            fg: hl.default_fg,
            bg: hl.default_bg,
            ..Self::default()
        };
        let normal = theme.normal();
        let emphasis = theme.emphasis();
        theme.status_line = theme.named(hl, "StatusLine", normal);
        theme.tab_line = theme.named(hl, "TabLine", normal);
        theme.tab_line_sel = theme.named(hl, "TabLineSel", emphasis);
        theme.tab_line_fill = theme.named(hl, "TabLineFill", normal);
        theme.pmenu = theme.named(hl, "Pmenu", normal);
        theme.pmenu_sel = theme.named(hl, "PmenuSel", emphasis);
        theme.msg_area = theme.named(hl, "MsgArea", normal);
        theme
    }

    /// The base/"Normal" resolved style: this theme's colors, no attributes.
    #[must_use]
    pub fn normal(&self) -> ResolvedStyle {
        ResolvedStyle {
            fg: self.fg,
            bg: self.bg,
            ..ResolvedStyle::default()
        }
    }

    /// Chrome's "this row stands out" style: `normal`'s colors plus the
    /// `reverse` flag, which -- unlike pre-swapping color values -- stays
    /// visibly distinct even before any theme color is known (an unset
    /// `fg`/`bg` still inverts against a rendering backend's own ambient
    /// default). The documented fallback [`Theme::from_hl`] uses for a
    /// selection-style named group it cannot resolve yet.
    #[must_use]
    pub fn emphasis(&self) -> ResolvedStyle {
        ResolvedStyle {
            reverse: true,
            ..self.normal()
        }
    }

    /// One grid cell's resolved style: `hl_id`'s attributes from `hl`
    /// layered over this theme's base colors, matching nvim's own
    /// fallback rule (an attribute the highlight group does not set falls
    /// back to the default/"Normal" value).
    #[must_use]
    pub fn style_for(&self, hl_id: u64, hl: &HlTable) -> ResolvedStyle {
        let mut style = self.normal();
        if let Some(a) = hl.attrs.get(&hl_id) {
            if a.fg.is_some() {
                style.fg = a.fg;
            }
            if a.bg.is_some() {
                style.bg = a.bg;
            }
            style.reverse = a.reverse;
            style.bold = a.bold;
            style.italic = a.italic;
            style.underline = a.underline;
        }
        style
    }

    /// One builtin chrome element's resolved style, looked up in
    /// `hl.groups` by nvim's `hl_group_set` name and resolved the same way
    /// a grid cell's `hl_id` resolves ([`Theme::style_for`]). `fallback` is
    /// used only when nvim has not associated `name` with an `hl_id` at
    /// all yet; once it has, that resolution is trusted as-is, even where
    /// it happens to equal `fallback`.
    #[must_use]
    pub fn named(&self, hl: &HlTable, name: &str, fallback: ResolvedStyle) -> ResolvedStyle {
        match hl.groups.get(name) {
            Some(&hl_id) => self.style_for(hl_id, hl),
            None => fallback,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::hl::HlAttr;
    use crate::model::Model;
    use crate::msg::Msg;
    use crate::update::update;
    use std::collections::HashMap;

    fn table_with(
        default_fg: Option<u32>,
        default_bg: Option<u32>,
        id: u64,
        attr: HlAttr,
    ) -> HlTable {
        let mut attrs = HashMap::new();
        attrs.insert(id, attr);
        HlTable {
            default_fg,
            default_bg,
            attrs,
            groups: HashMap::new(),
        }
    }

    fn no_attrs() -> HlAttr {
        HlAttr {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        }
    }

    #[test]
    fn from_hl_derives_default_colors_as_normal() {
        let hl = table_with(Some(0xFFFFFF), Some(0x000000), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.fg, Some(0xFFFFFF));
        assert_eq!(theme.bg, Some(0x000000));
    }

    /// Derivation stability: the same `HlTable` state always derives an
    /// identical `Theme`, whether re-derived once or many times -- the
    /// property `theme_cache`'s round trip and `paint`'s per-frame
    /// re-derivation both depend on. Asserts concrete field values (not
    /// just that the three derivations match each other): a `from_hl` that
    /// always returned the same wrong `Theme` would still pass a
    /// three-way equality check alone.
    #[test]
    fn from_hl_is_stable_across_repeated_derivation() {
        let hl = table_with(Some(0x112233), Some(0x445566), 7, no_attrs());
        let first = Theme::from_hl(&hl);
        let second = Theme::from_hl(&hl);
        let third = Theme::from_hl(&hl);
        assert_eq!(first, second);
        assert_eq!(second, third);
        assert_eq!(first.fg, Some(0x112233));
        assert_eq!(first.bg, Some(0x445566));
    }

    /// Derivation reads live `Model.engine.hl` state built the same way
    /// production code builds it: through `update()` decoding real
    /// `DefaultColorsSet`/`HlAttrDefine`/`HlGroupSet` events, not
    /// hand-built structs.
    #[test]
    fn from_hl_derives_stably_from_events_decoded_through_update() {
        let mut model = Model::new();
        let _ = update(
            &mut model,
            Msg::Redraw(vec![
                crate::events::UiEvent::DefaultColorsSet {
                    fg: Some(0xABCDEF),
                    bg: Some(0x010203),
                    sp: None,
                },
                crate::events::UiEvent::HlAttrDefine {
                    id: 3,
                    fg: Some(0xFF0000),
                    bg: None,
                    bold: true,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                crate::events::UiEvent::HlGroupSet {
                    name: "StatusLine".to_string(),
                    hl_id: 3,
                },
            ]),
        );
        let first = Theme::from_hl(&model.engine.hl);
        let second = Theme::from_hl(&model.engine.hl);
        assert_eq!(first, second);
        assert_eq!(first.fg, Some(0xABCDEF));
        assert_eq!(first.bg, Some(0x010203));
        assert_eq!(
            first.style_for(3, &model.engine.hl),
            ResolvedStyle {
                fg: Some(0xFF0000),
                bg: Some(0x010203),
                bold: true,
                italic: false,
                underline: false,
                reverse: false,
            }
        );
        assert_eq!(first.status_line, first.style_for(3, &model.engine.hl));
    }

    #[test]
    fn style_for_unknown_hl_id_falls_back_to_normal() {
        let hl = table_with(Some(0x111111), Some(0x222222), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.style_for(999, &hl), theme.normal());
    }

    #[test]
    fn style_for_partial_override_keeps_unset_channel_at_default() {
        let hl = table_with(
            Some(0x111111),
            Some(0x222222),
            1,
            HlAttr {
                fg: Some(0x333333),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
        );
        let theme = Theme::from_hl(&hl);
        let style = theme.style_for(1, &hl);
        assert_eq!(style.fg, Some(0x333333));
        assert_eq!(style.bg, Some(0x222222));
    }

    #[test]
    fn style_for_reverse_sets_the_reverse_flag_not_swapped_values() {
        let hl = table_with(
            Some(0x111111),
            Some(0x222222),
            1,
            HlAttr {
                fg: Some(0xAAAAAA),
                bg: Some(0xBBBBBB),
                bold: false,
                italic: false,
                underline: false,
                reverse: true,
            },
        );
        let theme = Theme::from_hl(&hl);
        let style = theme.style_for(1, &hl);
        assert_eq!(style.fg, Some(0xAAAAAA));
        assert_eq!(style.bg, Some(0xBBBBBB));
        assert!(style.reverse);
    }

    #[test]
    fn normal_carries_no_text_attributes() {
        let hl = table_with(Some(0x1), Some(0x2), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        let style = theme.normal();
        assert!(!style.bold);
        assert!(!style.italic);
        assert!(!style.underline);
        assert!(!style.reverse);
    }

    #[test]
    fn emphasis_sets_reverse_over_normal_colors() {
        let hl = table_with(Some(0x1), Some(0x2), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        let style = theme.emphasis();
        assert_eq!(style.fg, Some(0x1));
        assert_eq!(style.bg, Some(0x2));
        assert!(style.reverse);
    }

    /// The load-bearing property this whole extension exists to prove: once
    /// nvim has named a group, `Theme` prefers its real resolved colors
    /// over the derived fallback, even where the two would otherwise
    /// collide by coincidence for an ordinary (non-selection) group.
    #[test]
    fn named_prefers_the_mapped_groups_colors_over_the_derived_fallback() {
        let mut hl = table_with(
            Some(0x000000),
            Some(0xFFFFFF),
            9,
            HlAttr {
                fg: Some(0x123456),
                bg: Some(0x654321),
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
        );
        hl.groups.insert("StatusLine".to_string(), 9);
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.status_line.fg, Some(0x123456));
        assert_eq!(theme.status_line.bg, Some(0x654321));
        assert_ne!(theme.status_line, theme.normal());
    }

    /// The other half of the same property: a selection-style group with
    /// no mapping yet falls back to `emphasis()` (reverse-video), not
    /// `normal()`, so the selected/current row stays visually distinct.
    #[test]
    fn named_selection_group_falls_back_to_emphasis_when_unmapped() {
        let hl = table_with(Some(0x1), Some(0x2), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.tab_line_sel, theme.emphasis());
        assert_eq!(theme.pmenu_sel, theme.emphasis());
        assert_ne!(theme.tab_line_sel, theme.normal());
    }

    /// And the plain-group counterpart: an ordinary (non-selection) group
    /// with no mapping yet falls back to `normal()`, not `emphasis()` --
    /// unmapped chrome should look like plain text, not spuriously
    /// reverse-video.
    #[test]
    fn named_plain_group_falls_back_to_normal_when_unmapped() {
        let hl = table_with(Some(0x1), Some(0x2), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.status_line, theme.normal());
        assert_eq!(theme.tab_line, theme.normal());
        assert_eq!(theme.tab_line_fill, theme.normal());
        assert_eq!(theme.pmenu, theme.normal());
        assert_eq!(theme.msg_area, theme.normal());
    }
}
