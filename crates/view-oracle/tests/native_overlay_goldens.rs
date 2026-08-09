//! Committed screen dumps of every native overlay at every terminal tier.
//!
//! A native overlay's frame is the part of `view` a user sees before they
//! see anything else, and it degrades by tier: rounded box-drawing where
//! the terminal proved it can sync, plain box-drawing where it proved only
//! color, ASCII everywhere else. Unit assertions on individual rows cannot
//! say whether the whole picture is right; a committed dump can, and
//! reviewing a diff of one is how a change to framing gets noticed instead
//! of silently shipping.
//!
//! Rendered through `view_oracle::raster::screen_text`, the same pure
//! `Surface` -> text path the differential oracle compares against a
//! reference nvim, so a golden here and an oracle divergence there describe
//! the same rendering rather than two parallel ones.
//!
//! Regenerate with `VIEW_UPDATE_GOLDENS=1 cargo test -p view-oracle
//! --test native_overlay_goldens`, then read every regenerated file before
//! committing it: a golden accepted without being read pins a bug.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use view_core::grid::Grid;
use view_core::model::Tier;
use view_core::native::views::{
    PaletteRow, PaletteView, PickerView, PromptView, StatuslineView, TreeRow, TreeView,
};
use view_surface::{Layer, LayerKind, Rect, Surface};

/// The env var that rewrites a golden instead of asserting against it.
const UPDATE: &str = "VIEW_UPDATE_GOLDENS";

/// Where the overlay is placed on the canvas. Non-zero on both axes so a
/// dump pins that the rows land at the layer's own rect rather than at the
/// canvas origin.
const AT_ROW: u16 = 1;
const AT_COL: u16 = 2;

/// Renders one framed overlay to a screen dump.
///
/// The surface is the overlay alone, with no engine grid under it: the
/// canvas is the union of the layer rects, so what a golden shows is
/// exactly the overlay plus the blank rows and columns its offset leaves
/// around it. Buffer text under an overlay is nvim's, covered by the
/// oracle's own differential legs, and putting it here would make every
/// golden churn on fixture changes that say nothing about framing.
fn dump(tier: Tier, width: u16, height: u16, kind: LayerKind) -> String {
    let layer = Layer::new(Rect::new(AT_ROW, AT_COL, width, height), kind, tier);
    view_oracle::raster::screen_text(&Surface::from_layers(vec![layer]), &Grid::new())
}

fn golden_path(name: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests");
    path.push("goldens");
    path.push(format!("{name}.txt"));
    path
}

/// Asserts `actual` matches the committed golden `name`, or rewrites it
/// when [`UPDATE`] is set.
///
/// A missing golden fails rather than being written silently: an
/// unattended first run would otherwise commit whatever the code did on
/// that day as the definition of correct.
fn assert_golden(name: &str, actual: &str) {
    let path = golden_path(name);
    if std::env::var_os(UPDATE).is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{actual}\n")).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "golden {} is missing ({err}); regenerate with {UPDATE}=1 and read it before committing",
            path.display()
        )
    });
    assert_eq!(
        actual,
        expected.trim_end_matches('\n'),
        "golden {} diverged",
        path.display()
    );
}

fn picker() -> LayerKind {
    LayerKind::Picker(
        PickerView::new("Files")
            .with_query("mai")
            .with_rows(vec![
                "src/main.rs".to_string(),
                "src/model.rs".to_string(),
                "src/domain.rs".to_string(),
            ])
            .with_selected(1),
    )
}

fn tree() -> LayerKind {
    LayerKind::Tree(
        TreeView::new("Explorer")
            .with_rows(vec![
                TreeRow::dir(0, "crates", true),
                TreeRow::dir(1, "view-core", false),
                TreeRow::dir(1, "view-tui", true),
                TreeRow::leaf(2, "paint.rs"),
                TreeRow::leaf(0, "Cargo.toml"),
            ])
            .with_selected(3),
    )
}

fn statusline() -> LayerKind {
    LayerKind::Statusline(StatuslineView::new(
        "NORMAL",
        "crates/view-tui/src/paint.rs",
        "12:4",
    ))
}

fn prompt() -> LayerKind {
    LayerKind::Prompt(
        PromptView::new("Confirm", "Overwrite paint.rs?")
            .with_input("y")
            .with_choices(vec!["Yes".to_string(), "No".to_string()])
            .with_selected(0),
    )
}

fn palette() -> LayerKind {
    LayerKind::Palette(
        PaletteView::new("Commands")
            .with_query("fi")
            .with_rows(vec![
                PaletteRow::new("Find File").with_binding("<C-p>"),
                PaletteRow::new("Find in Files").with_binding("<C-S-f>"),
                PaletteRow::new("Filter Buffers"),
            ])
            .with_selected(1),
    )
}

/// A picker is the widest and tallest of the five, so it is the one whose
/// scroll window and title inset a dump has room to show.
#[test]
fn full_picker() {
    assert_golden("full-picker", &dump(Tier::Full, 34, 8, picker()));
}

#[test]
fn standard_picker() {
    assert_golden("standard-picker", &dump(Tier::Standard, 34, 8, picker()));
}

#[test]
fn basic_picker() {
    assert_golden("basic-picker", &dump(Tier::Basic, 34, 8, picker()));
}

#[test]
fn full_tree() {
    assert_golden("full-tree", &dump(Tier::Full, 30, 7, tree()));
}

#[test]
fn standard_tree() {
    assert_golden("standard-tree", &dump(Tier::Standard, 30, 7, tree()));
}

#[test]
fn basic_tree() {
    assert_golden("basic-tree", &dump(Tier::Basic, 30, 7, tree()));
}

#[test]
fn full_statusline() {
    assert_golden("full-statusline", &dump(Tier::Full, 46, 3, statusline()));
}

#[test]
fn standard_statusline() {
    assert_golden(
        "standard-statusline",
        &dump(Tier::Standard, 46, 3, statusline()),
    );
}

#[test]
fn basic_statusline() {
    assert_golden("basic-statusline", &dump(Tier::Basic, 46, 3, statusline()));
}

/// The height the statusline actually ships at: `render()` sizes its layer
/// from `Model::statusline_rows()`, which is one row or none, so the box
/// the trio above dumps is a shape a user never sees. A rect with no room
/// for distinct edge cells lays out as content edge to edge -- the bar --
/// and that degrade is the thing worth a committed picture, since it is
/// the only overlay in the product that always takes it. The framed trio
/// stays: it is what pins the frame's own tier degradation for a rect that
/// does have room, which no other statusline dump would show.
///
/// All three tiers are dumped even though a bar has no border glyph to
/// degrade, and the three files are byte-identical on purpose: that
/// sameness is the claim -- the row a user actually gets must not start
/// varying with the terminal's capability tier.
#[test]
fn full_statusline_bar() {
    assert_golden(
        "full-statusline-bar",
        &dump(Tier::Full, 46, 1, statusline()),
    );
}

#[test]
fn standard_statusline_bar() {
    assert_golden(
        "standard-statusline-bar",
        &dump(Tier::Standard, 46, 1, statusline()),
    );
}

#[test]
fn basic_statusline_bar() {
    assert_golden(
        "basic-statusline-bar",
        &dump(Tier::Basic, 46, 1, statusline()),
    );
}

#[test]
fn full_prompt() {
    assert_golden("full-prompt", &dump(Tier::Full, 32, 7, prompt()));
}

#[test]
fn standard_prompt() {
    assert_golden("standard-prompt", &dump(Tier::Standard, 32, 7, prompt()));
}

#[test]
fn basic_prompt() {
    assert_golden("basic-prompt", &dump(Tier::Basic, 32, 7, prompt()));
}

#[test]
fn full_palette() {
    assert_golden("full-palette", &dump(Tier::Full, 38, 8, palette()));
}

#[test]
fn standard_palette() {
    assert_golden("standard-palette", &dump(Tier::Standard, 38, 8, palette()));
}

#[test]
fn basic_palette() {
    assert_golden("basic-palette", &dump(Tier::Basic, 38, 8, palette()));
}
