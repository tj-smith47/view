#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use view_core::native::views::{PickerView, StyleRole};

fn picker() -> LayerKind {
    LayerKind::Picker(
        PickerView::new("Files")
            .with_query("mai")
            .with_rows(vec![
                "src/main.rs".to_string(),
                "src/lib.rs".to_string(),
                "src/paint.rs".to_string(),
            ])
            .with_selected(0),
    )
}

fn widths(rows: &Rows) -> Vec<u16> {
    rows.lines.iter().map(|l| cells(&line_text(l))).collect()
}

/// One row's text with its two edge glyphs and the padding column inside
/// each of them removed: the span a content assertion is about.
fn interior(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    chars[2..chars.len() - 2].iter().collect()
}

#[test]
fn every_row_of_a_framed_overlay_is_exactly_the_rect_wide_and_the_rect_is_filled() {
    for width in [2_u16, 3, 5, 6, 10, 24, 41] {
        for height in [2_u16, 3, 7, 12] {
            let rows = rows(width, height, &picker(), BorderSet::ROUNDED);
            assert_eq!(
                rows.lines.len(),
                usize::from(height),
                "{width}x{height} row count"
            );
            assert_eq!(
                widths(&rows),
                vec![width; usize::from(height)],
                "{width}x{height} row widths"
            );
        }
    }
}

#[test]
fn a_tier_swaps_the_border_glyphs_without_moving_any_content() {
    let full = rows(28, 8, &picker(), BorderSet::ROUNDED);
    let standard = rows(28, 8, &picker(), BorderSet::PLAIN);
    let basic = rows(28, 8, &picker(), BorderSet::ASCII);

    assert!(
        line_text(&full.lines[0]).starts_with('╭'),
        "{:?}",
        full.lines[0]
    );
    assert!(
        line_text(&standard.lines[0]).starts_with('┌'),
        "{:?}",
        standard.lines[0]
    );
    assert!(
        line_text(&basic.lines[0]).starts_with('+'),
        "{:?}",
        basic.lines[0]
    );

    // corners aside, full and standard draw the same edges, so a change to
    // the shared glyph moves both tiers and leaves ASCII alone
    assert_eq!(
        line_text(&full.lines[1]),
        line_text(&standard.lines[1]),
        "interior rows are identical"
    );
    assert_eq!(
        full.selected, standard.selected,
        "selection lands on the same row at every tier"
    );
    assert_eq!(full.selected, basic.selected);

    let strip = |line: &str| -> String {
        line.chars()
            .filter(|c| !"╭╮╰╯┌┐└┘+-|─│".contains(*c))
            .collect()
    };
    for (a, b) in full.lines.iter().zip(basic.lines.iter()) {
        assert_eq!(
            strip(&line_text(a)),
            strip(&line_text(b)),
            "content is tier-independent"
        );
    }
}

#[test]
fn the_title_is_set_into_the_top_edge_only_while_it_fits() {
    let wide = rows(28, 5, &picker(), BorderSet::ASCII);
    assert!(
        line_text(&wide.lines[0]).contains(" Files "),
        "{:?}",
        wide.lines[0]
    );

    let narrow = rows(8, 5, &picker(), BorderSet::ASCII);
    assert_eq!(
        line_text(&narrow.lines[0]),
        "+------+",
        "a top edge too short for the title keeps an unbroken run"
    );
}

#[test]
fn the_selection_marker_and_its_row_index_name_the_same_row() {
    let framed = rows(28, 8, &picker(), BorderSet::ASCII);
    let row = framed.selected.expect("the picker has a selection");
    assert!(
        line_text(&framed.lines[usize::from(row)]).contains("> src/main.rs"),
        "{:?}",
        framed.lines
    );
    for (i, line) in framed.lines.iter().enumerate() {
        if i != usize::from(row) {
            let text = line_text(line);
            assert!(!text.contains("> src/"), "only one row is marked: {text}");
        }
    }
}

#[test]
fn a_selection_below_the_window_scrolls_it_into_view_by_the_smallest_step() {
    let many: Vec<String> = (0..20).map(|i| format!("row-{i:02}")).collect();
    let kind = |selected: usize| {
        LayerKind::Picker(
            PickerView::new("Files")
                .with_rows(many.clone())
                .with_selected(selected),
        )
    };

    // interior height 4, of which the prompt line and rule take 2: two item
    // rows are visible at a time
    let top = rows(20, 6, &kind(1), BorderSet::ASCII);
    assert!(
        line_text(&top.lines[3]).contains("row-00"),
        "{:?}",
        top.lines
    );
    assert!(
        line_text(&top.lines[4]).contains("> row-01"),
        "{:?}",
        top.lines
    );

    let scrolled = rows(20, 6, &kind(7), BorderSet::ASCII);
    assert!(
        line_text(&scrolled.lines[3]).contains("row-06"),
        "{:?}",
        scrolled.lines
    );
    assert!(
        line_text(&scrolled.lines[4]).contains("> row-07"),
        "the selection sits on the last visible row, not the first: {:?}",
        scrolled.lines
    );
    assert_eq!(scrolled.selected, Some(4));
}

#[test]
fn a_selection_past_the_end_of_the_rows_highlights_nothing() {
    let kind = LayerKind::Picker(
        PickerView::new("Files")
            .with_rows(vec!["only".to_string()])
            .with_selected(9),
    );
    let framed = rows(20, 6, &kind, BorderSet::ASCII);
    assert_eq!(framed.selected, None);
    assert!(
        framed
            .lines
            .iter()
            .all(|l| !line_text(l).contains("> only")),
        "{:?}",
        framed.lines
    );
}

#[test]
fn a_layer_that_is_not_a_native_overlay_paints_no_rows_at_all() {
    let framed = rows(20, 6, &LayerKind::EngineGrid, BorderSet::ASCII);
    assert!(framed.lines.is_empty());
    assert_eq!(framed.selected, None);
}

#[test]
fn a_degenerate_rect_yields_no_rows_rather_than_half_a_frame() {
    for (w, h) in [(0_u16, 6_u16), (20, 0), (0, 0)] {
        let framed = rows(w, h, &picker(), BorderSet::ASCII);
        assert!(framed.lines.is_empty(), "{w}x{h}");
    }
    // one cell on an axis has no distinct edge cells, so content is drawn
    // unframed rather than as stacked corner glyphs
    let thin = rows(6, 1, &picker(), BorderSet::ASCII);
    let texts: Vec<String> = thin.lines.iter().map(|l| line_text(l)).collect();
    assert_eq!(texts, vec!["> mai ".to_string()]);
}

#[test]
fn a_wide_glyph_is_counted_in_cells_so_the_right_edge_stays_put() {
    let kind = LayerKind::Picker(
        PickerView::new("界")
            .with_query("界界界")
            .with_rows(vec!["界界界界界界界界".to_string()])
            .with_selected(0),
    );
    let framed = rows(16, 5, &kind, BorderSet::ASCII);
    assert_eq!(widths(&framed), vec![16; 5], "{:?}", framed.lines);
    for line in &framed.lines {
        let text = line_text(line);
        assert!(text.ends_with('|') || text.ends_with('+'), "{text}");
    }
}

#[test]
fn a_statusline_puts_its_three_segments_left_centered_and_right() {
    let kind = LayerKind::Statusline(view_core::native::views::StatuslineView::new(
        "NORMAL",
        "src/main.rs",
        "12:4",
    ));
    let framed = rows(40, 3, &kind, BorderSet::ASCII);
    let row = interior(&line_text(&framed.lines[1]));
    assert!(row.starts_with("NORMAL"), "{row}");
    assert!(row.trim_end().ends_with("12:4"), "{row}");
    let center_at = row
        .find("src/main.rs")
        .expect("centered segment is present");
    // 40 cells less the two edge glyphs and the two padding columns
    let ideal = (36 - "src/main.rs".len()) / 2;
    assert!(
        center_at.abs_diff(ideal) <= 1,
        "centered on the interior's own middle: {center_at} vs {ideal}"
    );
}

#[test]
fn a_palette_binding_is_pushed_against_the_right_edge_of_its_row() {
    let kind = LayerKind::Palette(
        view_core::native::views::PaletteView::new("Commands")
            .with_rows(vec![
                view_core::native::views::PaletteRow::new("Find File").with_binding("<C-p>"),
                view_core::native::views::PaletteRow::new("Reload"),
            ])
            .with_selected(0),
    );
    let framed = rows(30, 6, &kind, BorderSet::ASCII);
    let row = interior(&line_text(&framed.lines[3]));
    assert!(row.starts_with("> Find File"), "{row}");
    assert!(row.trim_end().ends_with("<C-p>"), "{row}");
    assert!(
        !line_text(&framed.lines[4]).contains('<'),
        "an unbound command has no binding column"
    );
}

#[test]
fn a_tree_row_shows_its_depth_and_whether_it_can_be_opened() {
    use view_core::native::views::{TreeRow, TreeView};
    let kind = LayerKind::Tree(
        TreeView::new("Explorer")
            .with_rows(vec![
                TreeRow::dir(0, "src", true),
                TreeRow::leaf(1, "main.rs"),
                TreeRow::dir(0, "target", false),
            ])
            .with_selected(1),
    );
    let framed = rows(30, 5, &kind, BorderSet::ASCII);
    assert!(
        line_text(&framed.lines[1]).contains("  - src"),
        "{:?}",
        framed.lines
    );
    let row2 = line_text(&framed.lines[2]);
    assert!(
        row2.contains(">   " /* marker */) && row2.contains("main.rs"),
        "{:?}",
        framed.lines
    );
    assert!(
        line_text(&framed.lines[3]).contains("  + target"),
        "{:?}",
        framed.lines
    );
}

#[test]
fn a_prompt_carries_its_question_its_input_and_its_choices() {
    use view_core::native::views::PromptView;
    let kind = LayerKind::Prompt(
        PromptView::new("Confirm", "Overwrite src/main.rs?")
            .with_input("y")
            .with_choices(vec!["Yes".to_string(), "No".to_string()])
            .with_selected(1),
    );
    let framed = rows(34, 7, &kind, BorderSet::ASCII);
    assert!(
        line_text(&framed.lines[1]).contains("Overwrite"),
        "{:?}",
        framed.lines
    );
    assert!(
        line_text(&framed.lines[2]).contains("> y"),
        "{:?}",
        framed.lines
    );
    assert!(
        line_text(&framed.lines[3]).contains("---"),
        "the rule separates the typed input from the choices, so the two
         rows carrying the same marker glyph never touch"
    );
    assert!(
        line_text(&framed.lines[4]).contains("  Yes"),
        "{:?}",
        framed.lines
    );
    assert!(
        line_text(&framed.lines[5]).contains("> No"),
        "{:?}",
        framed.lines
    );
    assert_eq!(framed.selected, Some(5));
}

#[test]
fn a_tier_maps_to_exactly_one_border_charset() {
    assert_eq!(BorderSet::for_tier(Tier::Full), BorderSet::ROUNDED);
    assert_eq!(BorderSet::for_tier(Tier::Standard), BorderSet::PLAIN);
    assert_eq!(BorderSet::for_tier(Tier::Basic), BorderSet::ASCII);
    assert_eq!(
        BorderSet::ROUNDED.horizontal,
        BorderSet::PLAIN.horizontal,
        "the two box-drawing tiers differ only at the corners"
    );
    assert_ne!(BorderSet::ASCII.horizontal, BorderSet::ROUNDED.horizontal);
}

#[test]
fn a_framed_layer_carries_the_charset_it_was_built_with() {
    let rect = Rect {
        row: 2,
        col: 4,
        width: 20,
        height: 6,
    };
    let layer = framed(rect, picker(), BorderSet::PLAIN);
    assert_eq!(layer.rect, rect);
    assert_eq!(layer.borders, Some(BorderSet::PLAIN));
}

/// Feature-supplied text is data, never a layout directive. A picker row
/// (or a tree label, a palette label, a prompt choice) can legally hold a
/// control character: `\x01` is a valid byte in a POSIX filename, and
/// nvim-sourced text is not filtered on the way in. Such a row must render
/// as its own literal content, left-flush like every other row, with the
/// control character reduced to the one blank cell a terminal gives it.
#[test]
fn a_control_character_in_a_row_is_blanked_never_read_as_a_column_break() {
    let kind = LayerKind::Picker(
        PickerView::new("Files")
            .with_rows(vec![
                "src/\u{1}main.rs".to_string(),
                "a\u{1}b\u{1}c\u{1}d".to_string(),
            ])
            .with_selected(0),
    );
    let framed = rows(30, 6, &kind, BorderSet::ASCII);

    assert!(
        framed
            .lines
            .iter()
            .all(|l| !line_text(l).chars().any(char::is_control)),
        "no control character survives into a painted row: {:?}",
        framed.lines
    );
    assert!(
        interior(&line_text(&framed.lines[3])).starts_with("> src/ main.rs"),
        "one mark must not right-align the tail of the row: {:?}",
        framed.lines[3]
    );
    assert!(
        interior(&line_text(&framed.lines[4])).starts_with("  a b c d"),
        "three marks must not fall through to a raw copy: {:?}",
        framed.lines[4]
    );
}

/// The statusline's three segments are placed because they are three
/// separate values, so a control character inside one of them shifts
/// nothing: the same text with and without it lands in the same columns.
#[test]
fn a_control_character_in_a_statusline_segment_moves_no_column() {
    // the same display width as the dirty left segment: a blanked control
    // character still occupies its cell, so a narrower clean segment would
    // be measuring the sanitization rather than the placement
    let clean = LayerKind::Statusline(view_core::native::views::StatuslineView::new(
        "NOR MAL",
        "src/main.rs",
        "12:4",
    ));
    let dirty = LayerKind::Statusline(view_core::native::views::StatuslineView::new(
        "NOR\u{1}MAL",
        "src/main.rs",
        "12:4",
    ));
    let clean = rows(40, 3, &clean, BorderSet::ASCII);
    let dirty = rows(40, 3, &dirty, BorderSet::ASCII);
    assert_eq!(
        interior(&line_text(&clean.lines[1])),
        interior(&line_text(&dirty.lines[1]))
    );
}

/// A title is feature-supplied text like any row is. The `*View` types take
/// `impl Into<String>` for it, so a future feature naming an overlay after a
/// buffer, a path, or a search term can put a control character in the top
/// edge just as easily as in a row.
#[test]
fn a_control_character_in_a_title_is_blanked_never_set_into_the_edge() {
    let kind = LayerKind::Picker(
        PickerView::new("Fi\u{1}les")
            .with_rows(vec!["src/main.rs".to_string()])
            .with_selected(0),
    );
    let framed = rows(30, 6, &kind, BorderSet::ASCII);
    let top = line_text(&framed.lines[0]);

    assert!(
        !top.chars().any(char::is_control),
        "no control character survives into the top edge: {top:?}"
    );
    assert!(
        top.contains(" Fi les "),
        "the title still reads, with the control character reduced to a blank: {top:?}"
    );
    assert_eq!(
        cells(&top),
        30,
        "blanking a control character moves no cell: {top:?}"
    );

    // a title with nothing left after blanking leaves the edge unbroken,
    // rather than punching a hole of blanks into it
    let blank_title = LayerKind::Picker(PickerView::new("\u{1}\u{2}"));
    let edge = rows(30, 4, &blank_title, BorderSet::ASCII);
    assert_eq!(line_text(&edge.lines[0]), format!("+{}+", "-".repeat(28)));
}

/// A cut landing inside a span's own text keeps that span's role on
/// whatever text survives the cut -- the span is not dropped just because
/// it did not fit in full.
#[test]
fn clip_spans_landing_mid_span_keeps_the_role_on_the_surviving_text() {
    let spans = vec![
        Span::new("AB", StyleRole::Mode),
        Span::new("CD", StyleRole::File),
    ];
    let out = clip_spans(spans, 3);
    assert_eq!(
        out,
        vec![
            Span::new("AB", StyleRole::Mode),
            Span::new("C", StyleRole::File),
        ]
    );
}

/// Once the cut lands exactly on a span boundary, every span still to come
/// is dropped entirely rather than emitting a role with no text.
#[test]
fn clip_spans_drops_a_span_entirely_once_the_width_is_already_used() {
    let spans = vec![
        Span::new("AB", StyleRole::Mode),
        Span::new("CD", StyleRole::File),
        Span::new("EF", StyleRole::GitBranch),
    ];
    let out = clip_spans(spans, 2);
    assert_eq!(out, vec![Span::new("AB", StyleRole::Mode)]);
}

/// Spans that all fit keep their own role and relative order untouched --
/// clipping to exactly the combined width is a no-op on the span sequence.
#[test]
fn clip_spans_preserves_the_role_sequence_when_everything_fits() {
    let spans = vec![
        Span::new("AB", StyleRole::Mode),
        Span::new("CD", StyleRole::DiagnosticError),
        Span::new("EF", StyleRole::DiagnosticWarning),
    ];
    let out = clip_spans(spans.clone(), 6);
    assert_eq!(out, spans);
}

/// Padding past the last span's content is always its own trailing
/// `StyleRole::Plain` span, never merged into whatever role the row's last
/// real content span carries.
#[test]
fn clip_spans_pads_with_a_trailing_plain_span_not_the_last_roles_span() {
    let spans = vec![Span::new("AB", StyleRole::Mode)];
    let out = clip_spans(spans, 5);
    assert_eq!(
        out,
        vec![Span::new("AB", StyleRole::Mode), Span::plain("   "),]
    );
    assert_eq!(out[1].role, StyleRole::Plain);
}

/// A picker carrying preview lines must paint them: `rows` is the one
/// layout pass both `view-tui`'s real terminal backend and `view-oracle`'s
/// rasterizer consume (see this module's doc), so a preview line reaching
/// the frame here is what "the preview pane is actually painted" means for
/// both painters at once, with no separate wiring on either side.
/// Disabling `content_rows`'s preview branch (routing every picker through
/// plain `lay_out` regardless of `view.preview`) makes this fail by name.
#[test]
fn a_pickers_preview_lines_reach_the_painted_frame() {
    let kind = LayerKind::Picker(
        PickerView::new("Files")
            .with_query("mai")
            .with_rows(vec!["src/main.rs".to_string(), "src/lib.rs".to_string()])
            .with_selected(0)
            .with_preview(vec![
                "fn main() {".to_string(),
                "    println!(\"hi\");".to_string(),
            ]),
    );
    let framed = rows(50, 8, &kind, BorderSet::ROUNDED);

    let joined: String = framed.lines.iter().map(|l| line_text(l)).collect();
    assert!(
        joined.contains("fn main() {"),
        "preview content must reach a painted row: {:?}",
        framed.lines
    );
    assert!(
        joined.contains("println!"),
        "every preview line must reach a painted row, not just the first: {:?}",
        framed.lines
    );
    // the results list is still on-screen beside the preview, not replaced
    // by it
    assert!(
        joined.contains("src/main.rs") && joined.contains("src/lib.rs"),
        "the preview pane must sit beside the results list, not over it: {:?}",
        framed.lines
    );
    assert_eq!(
        widths(&framed),
        vec![50; 8],
        "the split-column layout keeps the same total-rect contract every \
         other overlay makes: {:?}",
        framed.lines
    );
}

/// A picker with nothing to preview yet keeps the single-column layout it
/// always had: no separator rule appears inside the frame, and the results
/// list uses the full interior width, exactly as it did before a preview
/// pane existed.
#[test]
fn a_picker_with_no_preview_keeps_the_single_column_layout() {
    let framed = rows(50, 8, &picker(), BorderSet::ROUNDED);
    for line in &framed.lines {
        let inside = interior(&line_text(line));
        assert!(
            !inside.contains(BorderSet::ROUNDED.vertical),
            "no interior separator without a preview to split for: {:?}",
            framed.lines
        );
    }
}
