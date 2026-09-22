//! Committed screen dumps of every native overlay at every terminal tier.
//!
//! A native overlay's frame is the part of `view` a user sees before they
//! see anything else, and it degrades in exactly one step: rounded
//! box-drawing wherever the terminal draws box-drawing glyphs, ASCII where
//! it does not. Each dump names both facts about the terminal it depicts --
//! its tier and whether its box-glyph probe came back -- because only the
//! second decides the frame; the tier legs are here so a charset that
//! started tracking color depth again parts two dumps that must not part.
//! The `full` and `standard` dumps are byte-identical by design -- a diff
//! that parts them is the regression this file exists to catch -- and
//! `a_terminals_tier_never_reaches_its_frame` holds the two crossings to
//! the same committed pictures without a fourth set of files. Unit
//! assertions on individual rows cannot say whether the whole picture is
//! right; a committed dump can, and reviewing a diff of one is how a change
//! to framing gets noticed instead of silently shipping.
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
use view_core::model::{TermCaps, Tier};
use view_core::native::statusline::{SegmentUpdate, StatuslineState};
use view_core::native::views::{
    AiPanelView, PaletteRow, PaletteView, PickerView, PromptView, Span, StatuslineView, TreeRow,
    TreeView,
};
use view_surface::{Layer, LayerKind, Rect, Surface};

/// The env var that rewrites a golden instead of asserting against it.
const UPDATE: &str = "VIEW_UPDATE_GOLDENS";

/// Where the overlay is placed on the canvas. Non-zero on both axes so a
/// dump pins that the rows land at the layer's own rect rather than at the
/// canvas origin.
const AT_ROW: u16 = 1;
const AT_COL: u16 = 2;

/// A fixture terminal whose box-glyph probe came back saying it accounts
/// for `╭` as one cell, and one whose did not. Named rather than spelled
/// `true`/`false` at a call site, because the bit is what every dump in
/// this file actually varies over.
const DRAWS_BOX_GLYPHS: bool = true;
const NO_BOX_GLYPHS: bool = false;

/// Renders one framed overlay to a screen dump.
///
/// The surface is the overlay alone, with no engine grid under it: the
/// canvas is the union of the layer rects, so what a golden shows is
/// exactly the overlay plus the blank rows and columns its offset leaves
/// around it. Buffer text under an overlay is nvim's, covered by the
/// oracle's own differential legs, and putting it here would make every
/// golden churn on fixture changes that say nothing about framing.
fn dump(tier: Tier, unicode_boxes: bool, width: u16, height: u16, kind: LayerKind) -> String {
    let probed = match tier {
        Tier::Full => TermCaps::from_probe(true, true, true),
        Tier::Standard => TermCaps::from_probe(false, true, false),
        _ => TermCaps::from_probe(false, false, false),
    };
    assert_eq!(probed.tier, tier, "fixture must land on the tier it names");
    let layer = Layer::new(
        Rect::new(AT_ROW, AT_COL, width, height),
        kind,
        probed.with_unicode_boxes(unicode_boxes),
    );
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

/// The width the `nvim`-mode bar is dumped at: wide enough for every
/// segment a session has at once, since the composer drops whole segments
/// to fit and a narrower dump would show the truncation rather than the
/// row.
const NVIM_BAR_WIDTH: u16 = 72;

/// The bar under `panes = "nvim"`, composed by [`StatuslineState`] itself
/// rather than written out span by span like [`statusline`]: what this dump
/// pins is the segments a real session gives the composer and the order and
/// widths it lays them out in, the filetype behind the file name included.
fn nvim_statusline() -> LayerKind {
    let mut state = StatuslineState::default();
    state.apply(SegmentUpdate::Mode("-- INSERT --".to_string()));
    state.apply(SegmentUpdate::Buffer {
        name: "paint.rs".to_string(),
        modified: true,
        filetype: "rust".to_string(),
    });
    state.apply(SegmentUpdate::GitBranch("dev/p6-polish".to_string()));
    state.apply(SegmentUpdate::Diagnostics {
        errors: 2,
        warnings: 1,
    });
    state.apply(SegmentUpdate::Ruler("12:4".to_string()));
    LayerKind::Statusline(state.view(NVIM_BAR_WIDTH))
}

/// The pill spans the whole terminal row, so its dump is as wide as the
/// bar's and for the same reason: the two edges and the names between them
/// need room to land where a real session puts them.
const PILL_WIDTH: u16 = 72;

/// Opens `names` as tabpages through the same `update()` path a session
/// takes, because `TablineState` is non-exhaustive and cannot be built with
/// struct-literal syntax from outside `view-core`.
fn open_tabs(model: &mut view_core::model::Model, current: u64, names: &[&str]) {
    let _ = view_core::update::update(
        model,
        view_core::msg::Msg::Redraw(vec![view_core::events::UiEvent::TablineUpdate {
            current: view_core::events::TabHandle(current),
            tabs: names
                .iter()
                .enumerate()
                .map(|(index, name)| view_core::events::TabEntry {
                    tab: view_core::events::TabHandle(index as u64 + 1),
                    name: (*name).to_string(),
                })
                .collect(),
        }]),
    );
}

/// A pill on a remote session with three tabpages open and an agent
/// running: the destination at the left edge, the names centred with the
/// current one lit, and the agent's word at the right.
fn pill_tabs() -> LayerKind {
    let mut model = view_core::model::Model::with_term_size(PILL_WIDTH, 24)
        .with_remote(Some("deploy@prod-box".to_string()));
    model.ai_trusted = true;
    model.ai_panel_mut().session_id = Some("s-1".to_string());
    open_tabs(&mut model, 2, &["work", "docs", "notes"]);
    LayerKind::Pill(view_core::native::pill::PillView::from_model(&model))
}

/// The same row on a local session naming buffers instead, one of them
/// unsaved, while the agent waits on a permission.
fn pill_buffers() -> LayerKind {
    let mut model = view_core::model::Model::with_term_size(PILL_WIDTH, 24)
        .with_tabline_shows(view_core::native::pill::TablineShows::Buffers);
    model.ai_panel_mut().pending_permission =
        Some(view_core::native::ai_panel::PermissionPrompt::new(
            1,
            "call-1",
            Some("run tests?".to_string()),
            None,
            Vec::new(),
        ));
    open_tabs(&mut model, 1, &["work"]);
    model.buffers = ["main.rs", "model.rs", "paint.rs"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            view_core::model::BufferEntry::new(
                index as u64 + 1,
                name.to_string(),
                name == "model.rs",
                index == 0,
            )
        })
        .collect();
    LayerKind::Pill(view_core::native::pill::PillView::from_model(&model))
}

#[test]
fn full_pill_tabs() {
    assert_golden(
        "full-pill-tabs",
        &dump(Tier::Full, DRAWS_BOX_GLYPHS, PILL_WIDTH, 1, pill_tabs()),
    );
}

#[test]
fn standard_pill_tabs() {
    assert_golden(
        "standard-pill-tabs",
        &dump(Tier::Standard, DRAWS_BOX_GLYPHS, PILL_WIDTH, 1, pill_tabs()),
    );
}

#[test]
fn basic_pill_tabs() {
    assert_golden(
        "basic-pill-tabs",
        &dump(Tier::Basic, NO_BOX_GLYPHS, PILL_WIDTH, 1, pill_tabs()),
    );
}

#[test]
fn full_pill_buffers() {
    assert_golden(
        "full-pill-buffers",
        &dump(Tier::Full, DRAWS_BOX_GLYPHS, PILL_WIDTH, 1, pill_buffers()),
    );
}

#[test]
fn standard_pill_buffers() {
    assert_golden(
        "standard-pill-buffers",
        &dump(
            Tier::Standard,
            DRAWS_BOX_GLYPHS,
            PILL_WIDTH,
            1,
            pill_buffers(),
        ),
    );
}

#[test]
fn basic_pill_buffers() {
    assert_golden(
        "basic-pill-buffers",
        &dump(Tier::Basic, NO_BOX_GLYPHS, PILL_WIDTH, 1, pill_buffers()),
    );
}

#[test]
fn full_nvim_statusline_bar() {
    assert_golden(
        "full-nvim-statusline-bar",
        &dump(
            Tier::Full,
            DRAWS_BOX_GLYPHS,
            NVIM_BAR_WIDTH,
            1,
            nvim_statusline(),
        ),
    );
}

#[test]
fn standard_nvim_statusline_bar() {
    assert_golden(
        "standard-nvim-statusline-bar",
        &dump(
            Tier::Standard,
            DRAWS_BOX_GLYPHS,
            NVIM_BAR_WIDTH,
            1,
            nvim_statusline(),
        ),
    );
}

#[test]
fn basic_nvim_statusline_bar() {
    assert_golden(
        "basic-nvim-statusline-bar",
        &dump(
            Tier::Basic,
            NO_BOX_GLYPHS,
            NVIM_BAR_WIDTH,
            1,
            nvim_statusline(),
        ),
    );
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

/// Empty: no session has ever streamed a chunk into this build's transcript,
/// since a real transcript row needs a live agent to produce one.
fn ai_panel() -> LayerKind {
    LayerKind::Ai(AiPanelView::new("AI Agent"))
}

/// A crashed session's panel-local banner, distinct from the empty panel
/// [`ai_panel`] dumps: the falsifiable check for "never stalls paint" is a
/// latency claim (`native_overlay_goldens` cannot measure that), but the
/// banner reaching the painted frame at every tier is a framing claim this
/// file's whole point is to pin.
fn crashed_ai_panel() -> LayerKind {
    LayerKind::Ai(
        AiPanelView::new("AI Agent").with_local_error(vec![vec![Span::plain(
            "Error: the agent exited (signal: 9)".to_string(),
        )]]),
    )
}

/// A picker is the widest and tallest of the five, so it is the one whose
/// scroll window and title inset a dump has room to show.
#[test]
fn full_picker() {
    assert_golden(
        "full-picker",
        &dump(Tier::Full, DRAWS_BOX_GLYPHS, 34, 8, picker()),
    );
}

#[test]
fn standard_picker() {
    assert_golden(
        "standard-picker",
        &dump(Tier::Standard, DRAWS_BOX_GLYPHS, 34, 8, picker()),
    );
}

#[test]
fn basic_picker() {
    assert_golden(
        "basic-picker",
        &dump(Tier::Basic, NO_BOX_GLYPHS, 34, 8, picker()),
    );
}

#[test]
fn full_tree() {
    assert_golden(
        "full-tree",
        &dump(Tier::Full, DRAWS_BOX_GLYPHS, 30, 7, tree()),
    );
}

#[test]
fn standard_tree() {
    assert_golden(
        "standard-tree",
        &dump(Tier::Standard, DRAWS_BOX_GLYPHS, 30, 7, tree()),
    );
}

#[test]
fn basic_tree() {
    assert_golden(
        "basic-tree",
        &dump(Tier::Basic, NO_BOX_GLYPHS, 30, 7, tree()),
    );
}

#[test]
fn full_statusline() {
    assert_golden(
        "full-statusline",
        &dump(Tier::Full, DRAWS_BOX_GLYPHS, 46, 3, statusline()),
    );
}

#[test]
fn standard_statusline() {
    assert_golden(
        "standard-statusline",
        &dump(Tier::Standard, DRAWS_BOX_GLYPHS, 46, 3, statusline()),
    );
}

#[test]
fn basic_statusline() {
    assert_golden(
        "basic-statusline",
        &dump(Tier::Basic, NO_BOX_GLYPHS, 46, 3, statusline()),
    );
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
        &dump(Tier::Full, DRAWS_BOX_GLYPHS, 46, 1, statusline()),
    );
}

#[test]
fn standard_statusline_bar() {
    assert_golden(
        "standard-statusline-bar",
        &dump(Tier::Standard, DRAWS_BOX_GLYPHS, 46, 1, statusline()),
    );
}

#[test]
fn basic_statusline_bar() {
    assert_golden(
        "basic-statusline-bar",
        &dump(Tier::Basic, NO_BOX_GLYPHS, 46, 1, statusline()),
    );
}

#[test]
fn full_prompt() {
    assert_golden(
        "full-prompt",
        &dump(Tier::Full, DRAWS_BOX_GLYPHS, 32, 7, prompt()),
    );
}

#[test]
fn standard_prompt() {
    assert_golden(
        "standard-prompt",
        &dump(Tier::Standard, DRAWS_BOX_GLYPHS, 32, 7, prompt()),
    );
}

#[test]
fn basic_prompt() {
    assert_golden(
        "basic-prompt",
        &dump(Tier::Basic, NO_BOX_GLYPHS, 32, 7, prompt()),
    );
}

#[test]
fn full_palette() {
    assert_golden(
        "full-palette",
        &dump(Tier::Full, DRAWS_BOX_GLYPHS, 38, 8, palette()),
    );
}

#[test]
fn standard_palette() {
    assert_golden(
        "standard-palette",
        &dump(Tier::Standard, DRAWS_BOX_GLYPHS, 38, 8, palette()),
    );
}

#[test]
fn basic_palette() {
    assert_golden(
        "basic-palette",
        &dump(Tier::Basic, NO_BOX_GLYPHS, 38, 8, palette()),
    );
}

#[test]
fn full_ai_panel() {
    assert_golden(
        "full-ai-panel",
        &dump(Tier::Full, DRAWS_BOX_GLYPHS, 30, 7, ai_panel()),
    );
}

#[test]
fn standard_ai_panel() {
    assert_golden(
        "standard-ai-panel",
        &dump(Tier::Standard, DRAWS_BOX_GLYPHS, 30, 7, ai_panel()),
    );
}

#[test]
fn basic_ai_panel() {
    assert_golden(
        "basic-ai-panel",
        &dump(Tier::Basic, NO_BOX_GLYPHS, 30, 7, ai_panel()),
    );
}

#[test]
fn full_crashed_ai_panel() {
    assert_golden(
        "full-crashed-ai-panel",
        &dump(Tier::Full, DRAWS_BOX_GLYPHS, 30, 7, crashed_ai_panel()),
    );
}

#[test]
fn standard_crashed_ai_panel() {
    assert_golden(
        "standard-crashed-ai-panel",
        &dump(Tier::Standard, DRAWS_BOX_GLYPHS, 30, 7, crashed_ai_panel()),
    );
}

#[test]
fn basic_crashed_ai_panel() {
    assert_golden(
        "basic-crashed-ai-panel",
        &dump(Tier::Basic, NO_BOX_GLYPHS, 30, 7, crashed_ai_panel()),
    );
}

/// The two crossings the committed dumps do not have files for: a `basic`
/// terminal whose box-glyph probe came back, and a `full` one whose did
/// not. Each is asserted against the same-fixture dump of the tier that
/// does have a file, so both pictures are the committed ones without a
/// fourth and fifth set of goldens to review. Re-point the charset at the
/// tier and every row of this table fails.
#[test]
fn a_terminals_tier_never_reaches_its_frame() {
    let fixtures: Vec<(&str, u16, u16, LayerKind)> = vec![
        ("picker", 34, 8, picker()),
        ("tree", 30, 7, tree()),
        ("statusline", 46, 3, statusline()),
        ("statusline-bar", 46, 1, statusline()),
        ("nvim-statusline-bar", NVIM_BAR_WIDTH, 1, nvim_statusline()),
        ("pill-tabs", PILL_WIDTH, 1, pill_tabs()),
        ("pill-buffers", PILL_WIDTH, 1, pill_buffers()),
        ("prompt", 32, 7, prompt()),
        ("palette", 38, 8, palette()),
        ("ai-panel", 30, 7, ai_panel()),
        ("crashed-ai-panel", 30, 7, crashed_ai_panel()),
    ];
    for (name, width, height, kind) in fixtures {
        assert_eq!(
            dump(Tier::Basic, DRAWS_BOX_GLYPHS, width, height, kind.clone()),
            dump(Tier::Full, DRAWS_BOX_GLYPHS, width, height, kind.clone()),
            "{name}: a 16-color terminal that draws box glyphs is framed like any other"
        );
        assert_eq!(
            dump(Tier::Full, NO_BOX_GLYPHS, width, height, kind.clone()),
            dump(Tier::Basic, NO_BOX_GLYPHS, width, height, kind),
            "{name}: a terminal that cannot draw box glyphs is framed in ASCII, whatever its colors"
        );
    }
}

/// Walks the committed dumps rather than naming them: a `standard-` golden
/// added for a surface invented later joins this pin by existing, and the
/// day a tier is allowed to change corner glyphs again, every pair fails
/// here at once instead of one dump quietly diverging.
#[test]
fn every_standard_golden_is_byte_identical_to_its_full_sibling() {
    let dir = golden_path("full-picker")
        .parent()
        .expect("goldens live in a directory")
        .to_path_buf();
    let mut pairs = 0;
    for entry in std::fs::read_dir(&dir).expect("goldens directory is readable") {
        let path = entry.expect("a readable directory entry").path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(rest) = name.strip_prefix("standard-") else {
            continue;
        };
        let full = dir.join(format!("full-{rest}"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("standard golden is readable"),
            std::fs::read_to_string(&full)
                .unwrap_or_else(|err| panic!("{} has no full sibling: {err}", path.display())),
            "{name} parts from its full sibling; corner glyphs are font coverage, not a tier"
        );
        pairs += 1;
    }
    assert!(pairs >= 8, "only {pairs} standard goldens were walked");
}

/// The box a single settled toast paints, wide enough for its own text plus
/// the border on both edges.
const TOAST_WIDTH: u16 = 15;
const TOAST_HEIGHT: u16 = 3;

/// One toast, settled (no exit slide in flight), the subject a corner
/// golden pins: the box's own framing, not the stack's slide, which
/// `notifications_corner_scenes` in `view-tui` already pins end to end.
fn notification_corner(left_corner: bool) -> LayerKind {
    LayerKind::Toast {
        lines: vec![vec![Span::plain("3 files saved".to_string())]],
        slot: 0,
        x_offset: 0,
        left_corner,
        paused: false,
    }
}

/// The four committed pictures of one toast, one per named corner, three
/// tiers each.
#[test]
fn notifications_corner_scenes() {
    let corners = [
        ("top-left", true),
        ("top-right", false),
        ("bottom-left", true),
        ("bottom-right", false),
    ];
    let tiers = [
        (Tier::Full, "full", DRAWS_BOX_GLYPHS),
        (Tier::Standard, "standard", DRAWS_BOX_GLYPHS),
        (Tier::Basic, "basic", NO_BOX_GLYPHS),
    ];
    for (name, left_corner) in corners {
        for (tier, tier_name, unicode_boxes) in tiers {
            assert_golden(
                &format!("{tier_name}-notifications-corner-{name}"),
                &dump(
                    tier,
                    unicode_boxes,
                    TOAST_WIDTH,
                    TOAST_HEIGHT,
                    notification_corner(left_corner),
                ),
            );
        }
    }
}

/// Every stem the overlay family owns: a scene added later without its
/// three tier files fails by the stem's own name instead of shipping an
/// unpinned picture.
const OVERLAY_STEMS: [&str; 4] = [
    "notifications-corner-top-left",
    "notifications-corner-top-right",
    "notifications-corner-bottom-left",
    "notifications-corner-bottom-right",
];

#[test]
fn every_overlay_scene_has_a_golden_at_every_tier() {
    let dir = golden_path("full-picker")
        .parent()
        .expect("goldens live in a directory")
        .to_path_buf();
    for stem in OVERLAY_STEMS {
        for tier_name in ["full", "standard", "basic"] {
            let path = dir.join(format!("{tier_name}-{stem}.txt"));
            assert!(
                path.exists(),
                "{stem} has no {tier_name} golden at {}",
                path.display()
            );
        }
    }
}
