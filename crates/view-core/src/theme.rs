//! The color vocabulary painting resolves style through, derived purely
//! from the live highlight state nvim's `default_colors_set`/
//! `hl_attr_define` event stream already populates into [`HlTable`]. No
//! separate theme config or RPC query: `Theme::from_hl` is a read of state
//! the engine already streams, so re-deriving it on every `ColorScheme`
//! change costs nothing beyond the struct copy.

use crate::hl::HlTable;

/// One fully-resolved cell or chrome style: colors plus text attributes,
/// backend-free so any frontend (not just `ratatui`) can consume it.
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
}

/// The base colorscheme colors, derived from `HlTable`'s default
/// foreground/background (nvim's `default_colors_set` event: the "Normal"
/// group's colors). Deliberately holds only these two fields, not the full
/// `hl_attr_define` table: per-cell attributes are always resolved against
/// the live `HlTable` ([`Theme::style_for`]), and only the base colors are
/// worth caching to disk for a themed first paint before that live state
/// exists (see `theme_cache` in the `view` bin crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Theme {
    /// The default/"Normal" foreground color.
    pub fg: Option<u32>,
    /// The default/"Normal" background color.
    pub bg: Option<u32>,
}

impl Theme {
    /// Derives a `Theme` from the engine's live highlight table. Pure and
    /// deterministic: the same `HlTable` state always derives the same
    /// `Theme`, so callers may re-derive on every frame with no history to
    /// track.
    #[must_use]
    pub fn from_hl(hl: &HlTable) -> Self {
        Self {
            fg: hl.default_fg,
            bg: hl.default_bg,
        }
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

    /// One grid cell's resolved style: `hl_id`'s attributes from `hl`
    /// layered over this theme's base colors, matching nvim's own
    /// fallback rule (an attribute the highlight group does not set falls
    /// back to the default/"Normal" value). `reverse` swaps the resolved
    /// foreground and background, mirroring nvim's own `hl_attr_define`
    /// semantics for that flag.
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
            if a.reverse {
                std::mem::swap(&mut style.fg, &mut style.bg);
            }
            style.bold = a.bold;
            style.italic = a.italic;
            style.underline = a.underline;
        }
        style
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
    /// re-derivation both depend on.
    #[test]
    fn from_hl_is_stable_across_repeated_derivation() {
        let hl = table_with(Some(0x112233), Some(0x445566), 7, no_attrs());
        let first = Theme::from_hl(&hl);
        let second = Theme::from_hl(&hl);
        let third = Theme::from_hl(&hl);
        assert_eq!(first, second);
        assert_eq!(second, third);
    }

    /// Derivation reads live `Model.engine.hl` state built the same way
    /// production code builds it: through `update()` decoding real
    /// `DefaultColorsSet`/`HlAttrDefine` events, not hand-built structs.
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
            }
        );
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
    fn style_for_reverse_swaps_resolved_colors() {
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
        assert_eq!(style.fg, Some(0xBBBBBB));
        assert_eq!(style.bg, Some(0xAAAAAA));
    }

    #[test]
    fn normal_carries_no_text_attributes() {
        let hl = table_with(Some(0x1), Some(0x2), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        let style = theme.normal();
        assert!(!style.bold);
        assert!(!style.italic);
        assert!(!style.underline);
    }
}
