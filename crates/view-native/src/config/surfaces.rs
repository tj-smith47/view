//! Where each of view's own surfaces sits: a float over the buffer, or a
//! window in nvim's own layout.
//!
//! The words are read here rather than at parse time because the answer a
//! surface needs is a whole [`SurfaceLayout`], which three keys and one
//! older key between two tables decide together. A value view cannot read
//! never refuses the document: one mistyped anchor would otherwise revert
//! every other key in the file for the run, which is the term `[native]
//! tree_width` already answers on.

use view_core::config::discarded_file;
use view_core::native::geometry::{
    clamp_panel_width, Anchor, NativeSurface, SurfaceLayout, SurfacePlacement,
};

use super::{SurfaceSize, SurfaceTable, ViewConfig, TREE_WIDTH_KEY};

/// What a `placement` outside the vocabulary is answered with.
const PLACEMENT_EXPECTED: &str = "overlay or windowed";

/// What a `size` that is not a whole number of percent is answered with.
const SIZE_EXPECTED: &str = "a whole number of percent";

/// What the older key owes a user who has moved to the new one, and what it
/// owes one who has not.
const ALIAS_SUPERSEDED: &str = "view: [native] tree_width is now [ui.surfaces.tree] size; \
                                both read the same value, and the one in [ui.surfaces] wins \
                                this run";
const ALIAS_READ: &str =
    "view: [native] tree_width is now [ui.surfaces.tree] size; both read the same value";

/// The anchors each surface may be pinned to. A surface draws at one edge
/// of the screen and the set is what a sideways surface can honestly
/// answer: a tree or an agent panel at a side, the palette in the middle,
/// notifications at the top or the bottom.
const fn anchors(surface: NativeSurface) -> &'static [Anchor] {
    match surface {
        NativeSurface::Palette => &[Anchor::Center, Anchor::Top],
        NativeSurface::Notifications => &[Anchor::Top, Anchor::Bottom],
        _ => &[Anchor::Left, Anchor::Right],
    }
}

/// The anchors `surface` accepts, spelled the way a notice lists them.
fn anchors_expected(surface: NativeSurface) -> String {
    let words: Vec<&str> = anchors(surface).iter().map(|a| a.label()).collect();
    words.join(" or ")
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
        let table = match surface {
            NativeSurface::Tree => &file.ui.surfaces.tree,
            _ => continue,
        };
        let Some(layout) = layouts.get_mut(surface.index()) else {
            continue;
        };
        *layout = read(surface, table, layout.anchor, notices);
    }
    alias(file, &mut layouts, notices);
    layouts
}

/// One surface's table read through its own vocabulary.
fn read(
    surface: NativeSurface,
    table: &SurfaceTable,
    fallback: Anchor,
    notices: &mut Vec<String>,
) -> SurfaceLayout {
    let name = format!("ui.surfaces.{}", surface.id());
    let placement = match table.placement.as_deref().map(str::trim) {
        Some(word) => SurfacePlacement::parse(word).unwrap_or_else(|| {
            notices.push(discarded_file(word, PLACEMENT_EXPECTED, &name, "placement"));
            SurfacePlacement::default()
        }),
        None => SurfacePlacement::default(),
    };
    let anchor = match table.anchor.as_deref().map(str::trim) {
        Some(word) => anchors(surface)
            .iter()
            .copied()
            .find(|allowed| allowed.label() == word)
            .unwrap_or_else(|| {
                notices.push(discarded_file(
                    word,
                    &anchors_expected(surface),
                    &name,
                    "anchor",
                ));
                fallback
            }),
        None => fallback,
    };
    let size = match table.size.as_ref() {
        Some(SurfaceSize::Percent(pct)) => clamp_panel_width(*pct),
        Some(SurfaceSize::Other(written)) => {
            notices.push(discarded_file(written, SIZE_EXPECTED, &name, "size"));
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
    if file.spells("ui.surfaces.tree", "size") {
        notices.push(ALIAS_SUPERSEDED.to_string());
        return;
    }
    tree.size = file.native.tree_width();
    notices.push(ALIAS_READ.to_string());
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
            vec![ALIAS_READ.to_string()],
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
            vec![ALIAS_SUPERSEDED.to_string()],
            "the user was not told which key answered"
        );
    }

    #[test]
    fn a_silent_document_places_every_surface_at_its_default() {
        let (layouts, notices) = read_toml("");
        assert_eq!(layouts, SurfaceLayout::defaults());
        assert!(notices.is_empty(), "a silent document owes nothing");
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
}
