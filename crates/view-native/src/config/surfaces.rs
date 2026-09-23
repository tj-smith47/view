//! Where each of view's own surfaces sits: a float over the buffer, or a
//! window in nvim's own layout.
//!
//! The words are read here, after parse time, because the answer a
//! surface needs is a whole [`SurfaceLayout`], which three keys and one
//! older key between two tables decide together. A value view cannot read
//! never refuses the document: one mistyped anchor would otherwise revert
//! every other key in the file for the run, which is the term `[native]
//! tree_width` already answers on.

use view_core::config::discarded_file;
use view_core::native::geometry::{
    clamp_panel_width, Anchor, NativeSurface, SurfaceLayout, SurfacePlacement,
};
use SurfacePlacement::{Overlay, Windowed};

use view_core::config::placement_only_notice;

use super::resolve::alias_notice;
use super::{SurfaceSize, SurfaceTable, ViewConfig, TREE_WIDTH_KEY};

/// What a `placement` outside the vocabulary is answered with.
const PLACEMENT_EXPECTED: &str = "overlay or windowed";

/// What a `size` that is not a whole number of percent is answered with.
const SIZE_EXPECTED: &str = "a whole number of percent";

/// The anchors `surface` may be pinned to under `placement`.
/// [`view_core::native::geometry::SurfaceLayout::accepted_anchors`] is the
/// one table this and the ring step both read; see its own doc for what
/// each placement's vocabulary is and why the two differ per surface.
pub(super) const fn anchors(
    surface: NativeSurface,
    placement: SurfacePlacement,
) -> &'static [Anchor] {
    SurfaceLayout::accepted_anchors(surface, placement)
}

/// The anchors `surface` accepts under `placement`, spelled the way a
/// notice lists them.
pub(super) fn anchors_expected(surface: NativeSurface, placement: SurfacePlacement) -> String {
    let words: Vec<&str> = anchors(surface, placement)
        .iter()
        .map(|a| a.label())
        .collect();
    words.join(" or ")
}

/// The anchor `surface` opens at under `placement` with no `anchor` key
/// written. Overlay keeps each surface's shipped float edge
/// ([`SurfaceLayout::default_for`]); windowed reads its own default
/// ([`SurfaceLayout::default_windowed_anchor`]) because a windowed surface's
/// vocabulary excludes the overlay default for the palette (`center` is not
/// a tile edge).
pub(super) const fn default_anchor(surface: NativeSurface, placement: SurfacePlacement) -> Anchor {
    match placement {
        Overlay => SurfaceLayout::default_for(surface).anchor,
        Windowed => SurfaceLayout::default_windowed_anchor(surface),
        // `SurfacePlacement` is `#[non_exhaustive]`.
        _ => SurfaceLayout::default_windowed_anchor(surface),
    }
}

/// Every surface's placement, anchor and size, with a line for each value
/// this build could not read appended to `notices`.
///
/// `[native] tree_width` is `[ui.surfaces.tree] size` under its older name.
/// A file spelling both is answered by the newer key, and a file spelling
/// only the older one is answered by it. Either way the user is told, once,
/// which key the two share.
#[must_use]
pub fn surfaces(file: &ViewConfig, notices: &mut Vec<String>) -> [SurfaceLayout; 4] {
    let mut layouts = SurfaceLayout::defaults();
    for surface in NativeSurface::ALL {
        let table = file.ui.surfaces.get(surface);
        let Some(layout) = layouts.get_mut(surface.index()) else {
            continue;
        };
        *layout = read(surface, table, notices);
    }
    alias(file, &mut layouts, notices);
    layouts
}

/// One surface's table read through its own vocabulary, the anchor set
/// picked from the placement this same table resolved to -- a windowed
/// palette never sees `center` in its allowed set or its fallback, because
/// neither is a word the windowed vocabulary carries.
fn read(surface: NativeSurface, table: &SurfaceTable, notices: &mut Vec<String>) -> SurfaceLayout {
    let name = surface.dotted_table();
    let placement = match table.placement.as_deref().map(str::trim) {
        Some(word) => SurfacePlacement::parse(word).unwrap_or_else(|| {
            notices.push(discarded_file(word, PLACEMENT_EXPECTED, name, "placement"));
            SurfacePlacement::default()
        }),
        None => SurfacePlacement::default(),
    };
    let fallback = default_anchor(surface, placement);
    let anchor = match table.anchor.as_deref().map(str::trim) {
        Some(word) => anchors(surface, placement)
            .iter()
            .copied()
            .find(|allowed| allowed.label() == word)
            .unwrap_or_else(|| {
                notices.push(discarded_file(
                    word,
                    &anchors_expected(surface, placement),
                    name,
                    "anchor",
                ));
                fallback
            }),
        None => fallback,
    };
    let size = match table.size.as_ref() {
        Some(SurfaceSize::Percent(pct)) => {
            // the notification history's overlay is a fixed `OverlayBox`
            // (`open_message_history`), so a `size` written under `overlay`
            // is kept in the layout but never reaches a window or a box --
            // silently reading it would tell the user their number took.
            if surface == NativeSurface::Notifications && placement == Overlay {
                notices.push(placement_only_notice(
                    name,
                    "size",
                    "the windowed placement",
                ));
            }
            clamp_panel_width(*pct)
        }
        Some(SurfaceSize::Other(written)) => {
            notices.push(discarded_file(written, SIZE_EXPECTED, name, "size"));
            SurfaceLayout::default_for(surface).size
        }
        None => SurfaceLayout::default_for(surface).size,
    };
    SurfaceLayout::new(placement, anchor, size)
}

/// `[native] tree_width` folded into the tree's size, and the line that
/// tells the user the two keys are one.
fn alias(file: &ViewConfig, layouts: &mut [SurfaceLayout; 4], notices: &mut Vec<String>) {
    if !file.spells("native", TREE_WIDTH_KEY) {
        return;
    }
    let Some(tree) = layouts.get_mut(NativeSurface::Tree.index()) else {
        return;
    };
    let notice = alias_notice("native", "tree_width", "ui.surfaces.tree", "size");
    if file.spells("ui.surfaces.tree", "size") {
        notices.push(notice);
        return;
    }
    tree.size = file.native.tree_width();
    notices.push(notice);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// The layouts a document resolves to, with the notices it raised.
    fn read_toml(document: &str) -> ([SurfaceLayout; 4], Vec<String>) {
        let cfg = ViewConfig::from_toml_str(document).expect("the document must parse");
        let mut notices = Vec::new();
        let layouts = surfaces(&cfg, &mut notices);
        (layouts, notices)
    }

    /// The tree's own layout out of a document.
    fn tree(document: &str) -> (SurfaceLayout, Vec<String>) {
        let (layouts, notices) = read_toml(document);
        (layouts[NativeSurface::Tree.index()], notices)
    }

    #[test]
    fn a_tree_anchor_outside_its_set_notices_and_defaults() {
        let (layout, notices) = tree("[ui.surfaces.tree]\nanchor = \"top\"\n");
        assert_eq!(
            layout.anchor,
            Anchor::Left,
            "an anchor the tree does not accept was taken anyway"
        );
        assert_eq!(notices.len(), 1, "one notice was owed: {notices:?}");
        assert!(
            notices[0].contains("[ui.surfaces.tree] anchor")
                && notices[0].contains("left or right"),
            "the notice must name the key and the set it accepts: {}",
            notices[0]
        );
    }

    #[test]
    fn the_tree_width_alias_is_read_when_the_surfaces_table_is_silent() {
        let (layout, notices) = tree("[native]\ntree_width = 45\n");
        assert_eq!(layout.size, 45, "the older key was not read");
        assert_eq!(
            notices,
            vec![alias_notice(
                "native",
                "tree_width",
                "ui.surfaces.tree",
                "size"
            )],
            "the user was not told which key the two share"
        );
    }

    #[test]
    fn the_surfaces_table_beats_the_alias_with_a_notice() {
        let (layout, notices) =
            tree("[native]\ntree_width = 45\n\n[ui.surfaces.tree]\nsize = 22\n");
        assert_eq!(layout.size, 22, "the older key won over the newer one");
        assert_eq!(
            notices,
            vec![alias_notice(
                "native",
                "tree_width",
                "ui.surfaces.tree",
                "size"
            )],
            "the user was not told which key answered"
        );
    }

    #[test]
    fn the_alias_notice_names_both_keys() {
        let notice = alias_notice("native", "tree_width", "ui.surfaces.tree", "size");
        assert!(
            notice.contains("[native] tree_width") && notice.contains("[ui.surfaces.tree] size"),
            "the notice must name both keys the pair shares: {notice}"
        );
    }

    #[test]
    fn a_silent_document_places_every_surface_at_its_default() {
        let (layouts, notices) = read_toml("");
        assert_eq!(layouts, SurfaceLayout::defaults());
        assert!(notices.is_empty(), "a silent document owes nothing");
    }

    #[test]
    fn a_notifications_anchor_names_all_four_corners_overlay_and_all_four_edges_windowed() {
        assert_eq!(
            anchors(NativeSurface::Notifications, SurfacePlacement::Overlay),
            &[
                Anchor::TopLeft,
                Anchor::TopRight,
                Anchor::BottomLeft,
                Anchor::BottomRight
            ],
            "the overlay toast stack floats at a corner"
        );
        assert_eq!(
            anchors_expected(NativeSurface::Notifications, SurfacePlacement::Overlay),
            "top-left or top-right or bottom-left or bottom-right"
        );
        assert_eq!(
            anchors(NativeSurface::Notifications, SurfacePlacement::Windowed),
            &[Anchor::Left, Anchor::Right, Anchor::Top, Anchor::Bottom],
            "a windowed stream/ticker tiles at any one of the four edges"
        );
        assert_eq!(
            anchors_expected(NativeSurface::Notifications, SurfacePlacement::Windowed),
            "left or right or top or bottom"
        );
    }

    #[test]
    fn the_palette_floats_at_center_top_or_bottom_and_tiles_at_top_or_bottom() {
        assert_eq!(
            anchors(NativeSurface::Palette, SurfacePlacement::Overlay),
            &[Anchor::Center, Anchor::Top, Anchor::Bottom],
            "the design gives the overlay palette a third float position"
        );
        assert_eq!(
            anchors(NativeSurface::Palette, SurfacePlacement::Windowed),
            &[Anchor::Top, Anchor::Bottom],
            "a windowed palette has no centred tile to take"
        );
        let (layouts, notices) = read_toml("[ui.surfaces.palette]\nplacement = \"windowed\"\n");
        assert_eq!(
            layouts[NativeSurface::Palette.index()].anchor,
            Anchor::Bottom,
            "the design's windowed default is the bottom edge"
        );
        assert!(notices.is_empty(), "the default anchor owes no notice");
        let (layouts, notices) =
            read_toml("[ui.surfaces.palette]\nplacement = \"windowed\"\nanchor = \"bottom\"\n");
        assert_eq!(
            layouts[NativeSurface::Palette.index()].anchor,
            Anchor::Bottom,
            "the design's own windowed default word was refused"
        );
        assert!(
            notices.is_empty(),
            "a word the windowed vocabulary carries owes no notice"
        );
    }

    #[test]
    fn the_windowed_word_is_read_and_a_word_outside_the_pair_is_not() {
        let (layout, notices) = tree("[ui.surfaces.tree]\nplacement = \"windowed\"\n");
        assert!(layout.windowed(), "the word that opens a tile was not read");
        assert!(notices.is_empty(), "a word view reads owes nothing");
        let (layout, notices) = tree("[ui.surfaces.tree]\nplacement = \"docked\"\n");
        assert_eq!(layout.placement, SurfacePlacement::Overlay);
        assert_eq!(notices.len(), 1, "{notices:?}");
    }

    /// A document spelling every one of `surface`'s three keys, `anchor`
    /// set to `word`.
    fn document_for(surface: NativeSurface, anchor: &str) -> String {
        format!(
            "[ui.surfaces.{}]\nplacement = \"windowed\"\nanchor = \"{anchor}\"\nsize = 42\n",
            surface.id()
        )
    }

    #[test]
    fn every_surfaces_own_placement_anchor_and_size_is_read() {
        for surface in NativeSurface::ALL {
            let windowed = anchors(surface, SurfacePlacement::Windowed);
            for anchor in [
                windowed[0],
                *windowed
                    .last()
                    .expect("every surface accepts at least one windowed anchor"),
            ] {
                let (layouts, notices) = read_toml(&document_for(surface, anchor.label()));
                assert!(
                    notices.is_empty(),
                    "{}'s own vocabulary owes no notice: {notices:?}",
                    surface.id()
                );
                let layout = layouts[surface.index()];
                assert_eq!(
                    layout.placement,
                    SurfacePlacement::Windowed,
                    "{}",
                    surface.id()
                );
                assert_eq!(layout.anchor, anchor, "{}", surface.id());
                assert_eq!(layout.size, 42, "{}", surface.id());
            }
        }
    }

    #[test]
    fn every_surface_every_placement_every_accepted_anchor_word_is_read_with_no_notice() {
        for surface in NativeSurface::ALL {
            for placement in [SurfacePlacement::Overlay, SurfacePlacement::Windowed] {
                for anchor in anchors(surface, placement) {
                    let document = format!(
                        "[{}]\nplacement = \"{}\"\nanchor = \"{}\"\n",
                        surface.dotted_table(),
                        placement.label(),
                        anchor.label(),
                    );
                    let (layouts, notices) = read_toml(&document);
                    assert!(
                        notices.is_empty(),
                        "{} {} accepts {} per the design's own table: {notices:?}",
                        surface.id(),
                        placement.label(),
                        anchor.label(),
                    );
                    let layout = layouts[surface.index()];
                    assert_eq!(layout.placement, placement, "{}", surface.id());
                    assert_eq!(layout.anchor, *anchor, "{}", surface.id());
                    if placement == SurfacePlacement::Windowed {
                        // `WinSplit::for_anchor` is total over exactly the
                        // words a windowed surface's own vocabulary carries.
                        let split = view_core::msg::WinSplit::for_anchor(layout.anchor);
                        let expected = match layout.anchor {
                            Anchor::Right => view_core::msg::WinSplit::Right,
                            Anchor::Top => view_core::msg::WinSplit::Above,
                            Anchor::Bottom => view_core::msg::WinSplit::Below,
                            _ => view_core::msg::WinSplit::Left,
                        };
                        assert_eq!(
                            split,
                            expected,
                            "{} at {} did not resolve to the split it named",
                            surface.id(),
                            anchor.label()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_untouched_config_reaches_the_designs_default_positions() {
        let (layouts, notices) = read_toml("");
        assert!(notices.is_empty());
        let at = |s: NativeSurface| layouts[s.index()];
        assert_eq!(at(NativeSurface::Tree).anchor, Anchor::Left);
        assert_eq!(at(NativeSurface::Tree).size, 30);
        assert_eq!(at(NativeSurface::Agent).anchor, Anchor::Right);
        assert_eq!(at(NativeSurface::Agent).size, 30);
        assert_eq!(at(NativeSurface::Palette).anchor, Anchor::Center);
        assert_eq!(
            at(NativeSurface::Palette).size,
            40,
            "the palette's own default is 40 percent of rows"
        );
        assert_eq!(at(NativeSurface::Notifications).anchor, Anchor::TopRight);
        assert_eq!(at(NativeSurface::Notifications).size, 30);
        for surface in NativeSurface::ALL {
            assert_eq!(at(surface).placement, SurfacePlacement::Overlay);
        }
    }

    #[test]
    fn every_surfaces_bad_word_notices_by_name() {
        for surface in NativeSurface::ALL {
            let name = surface.dotted_table();
            for (key, document) in [
                ("placement", format!("[{name}]\nplacement = \"docked\"\n")),
                ("anchor", format!("[{name}]\nanchor = \"nowhere\"\n")),
                ("size", format!("[{name}]\nsize = \"wide\"\n")),
            ] {
                let (_, notices) = read_toml(&document);
                assert_eq!(notices.len(), 1, "{name} {key}: {notices:?}");
                assert!(
                    notices[0].contains(&format!("[{name}] {key}")),
                    "{name} {key}'s notice does not name its own key: {}",
                    notices[0]
                );
            }
        }
    }

    /// Every surface's `size`, written under both placements: the number
    /// always lands in the layout, so the placement/anchor/size triple the
    /// design's own table promises is answered every time. What the
    /// notification stream's overlay history never reads is the one case
    /// silent storage would misreport -- so a written `size` there owes a
    /// notice, and every other surface/placement pair owes none.
    #[test]
    fn every_surfaces_size_is_stored_and_notices_only_where_it_has_no_effect() {
        for surface in NativeSurface::ALL {
            for placement in [SurfacePlacement::Overlay, SurfacePlacement::Windowed] {
                let name = surface.dotted_table();
                let document = format!(
                    "[{name}]\nplacement = \"{}\"\nsize = 55\n",
                    placement.label()
                );
                let (layouts, notices) = read_toml(&document);
                assert_eq!(
                    layouts[surface.index()].size,
                    55,
                    "{name} under {}: the written size never reached the layout",
                    placement.label()
                );
                let size_notice = notices
                    .iter()
                    .find(|notice| notice.contains(&format!("[{name}] size")));
                let effectless = surface == NativeSurface::Notifications
                    && placement == SurfacePlacement::Overlay;
                assert_eq!(
                    size_notice.is_some(),
                    effectless,
                    "{name} under {}: {}",
                    placement.label(),
                    if effectless {
                        format!("a size with no reader owed a notice: {notices:?}")
                    } else {
                        format!("a size the surface reads owed no notice: {notices:?}")
                    }
                );
            }
        }
    }
}
