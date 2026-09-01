// see `crates/view-core/src/update/tests.rs` on why a sibling test module
// carries the whole-file attribute the `#[cfg(test)] mod tests;` line
// already implies
#![cfg(test)]
// see `crates/view-core/src/update/tests.rs` on why the older-clippy
// duplicate report is answered rather than removed
#![allow(clippy::duplicated_attributes)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use view_core::model::Tier;
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

/// Capabilities at `tier`'s own probe answers, with the box-glyph bit set
/// to `unicode_boxes`.
///
/// The three booleans per tier are `TermCaps::from_probe`'s own derivation
/// read backwards, which is what makes a fixture here a terminal that could
/// actually exist: a tier is not a field a test may set beside the answers
/// it derives from.
fn caps(tier: Tier, unicode_boxes: bool) -> TermCaps {
    let probed = match tier {
        Tier::Full => TermCaps::from_probe(true, true, true),
        Tier::Standard => TermCaps::from_probe(false, true, false),
        _ => TermCaps::from_probe(false, false, false),
    };
    assert_eq!(probed.tier, tier, "fixture must land on the tier it names");
    probed.with_unicode_boxes(unicode_boxes)
}

/// The decoupling, in the direction the tier-keyed charset got wrong: a
/// 16-color terminal that answered no capability query but does draw box
/// glyphs used to be handed ASCII on the strength of its color depth.
#[test]
fn a_basic_tier_terminal_that_draws_box_glyphs_gets_rounded_corners() {
    let basic = rows(
        28,
        8,
        &picker(),
        BorderSet::for_caps(caps(Tier::Basic, true)),
    );
    let full = rows(
        28,
        8,
        &picker(),
        BorderSet::for_caps(caps(Tier::Full, true)),
    );

    assert_eq!(
        line_text(&basic.lines[0]).chars().next(),
        Some('╭'),
        "a corner glyph is font coverage, not a color depth"
    );
    assert_eq!(
        BorderSet::for_caps(caps(Tier::Basic, true)),
        BorderSet::for_caps(caps(Tier::Full, true)),
        "the two charsets differ in nothing, corners included"
    );
    assert_eq!(full, basic, "the whole frame is identical");
}

/// The other direction: a terminal with every color and protocol answer
/// there is, whose box-glyph probe came back saying it does not account for
/// `╭` as one cell, gets the frame it can actually draw.
#[test]
fn a_full_tier_terminal_without_box_glyphs_gets_ascii() {
    let full = rows(
        28,
        8,
        &picker(),
        BorderSet::for_caps(caps(Tier::Full, false)),
    );

    assert_eq!(
        BorderSet::for_caps(caps(Tier::Full, false)),
        BorderSet::ASCII
    );
    assert!(
        line_text(&full.lines[0]).starts_with('+'),
        "{:?}",
        full.lines[0]
    );
    for line in &full.lines {
        let text = line_text(line);
        assert!(
            !text.chars().any(|c| "╭╮╰╯─│".contains(c)),
            "a box-drawing glyph reached an ASCII frame: {text:?}"
        );
    }
}

#[test]
fn the_charset_swaps_the_border_glyphs_without_moving_any_content() {
    let full = rows(28, 8, &picker(), BorderSet::ROUNDED);
    let basic = rows(28, 8, &picker(), BorderSet::ASCII);

    assert!(
        line_text(&full.lines[0]).starts_with('╭'),
        "{:?}",
        full.lines[0]
    );
    assert_eq!(
        full.selected, basic.selected,
        "selection lands on the same row at every tier"
    );

    let strip = |line: &str| -> String {
        line.chars().filter(|c| !"╭╮╰╯+-|─│".contains(*c)).collect()
    };
    for (a, b) in full.lines.iter().zip(basic.lines.iter()) {
        assert_eq!(
            strip(&line_text(a)),
            strip(&line_text(b)),
            "content is tier-independent"
        );
    }
}

/// The text of the one span the top edge gives [`StyleRole::Title`], or
/// `None` on an edge that carries no title at all. Read through the role
/// rather than off the joined row, so an assertion cannot pass on a border
/// run that happens to spell the same characters.
fn title_span(rows: &Rows) -> Option<String> {
    rows.lines
        .first()?
        .iter()
        .find(|s| s.role == StyleRole::Title)
        .map(|s| s.text.clone())
}

/// A view titled `title`, for the width boundaries a title's own length
/// decides.
fn titled(title: &str) -> LayerKind {
    LayerKind::Picker(PickerView::new(title).with_rows(vec!["row".to_string()]))
}

#[test]
fn a_title_that_fits_is_set_into_the_top_edge_whole() {
    let wide = rows(28, 5, &picker(), BorderSet::ASCII);
    assert!(
        line_text(&wide.lines[0]).contains(" Files "),
        "{:?}",
        wide.lines[0]
    );

    // the narrowest edge "Files" fits whole in: two corners, one horizontal
    // glyph and one blank column on each side, and the five cells between
    let exact = rows(11, 5, &picker(), BorderSet::ASCII);
    assert_eq!(line_text(&exact.lines[0]), "+- Files -+");
    assert_eq!(title_span(&exact).as_deref(), Some(" Files "));
}

#[test]
fn a_top_edge_one_cell_short_of_the_title_carries_a_marked_cut_of_it() {
    // one below the width the whole title fits in, and down to the last
    // width a glyph of it survives at: the box names itself at every one of
    // them, and says it was cut at every one of them
    let cut = rows(10, 5, &picker(), BorderSet::ASCII);
    assert_eq!(line_text(&cut.lines[0]), "+- Fil… -+");
    assert_eq!(title_span(&cut).as_deref(), Some(" Fil… "));
    assert_eq!(
        line_text(&rows(9, 5, &picker(), BorderSet::ASCII).lines[0]),
        "+- Fi… -+"
    );
    // the floor: one cell of title beside the mark. Anonymity is what this
    // whole degradation exists to avoid, so a name is cut to its first
    // glyph before the edge is left blank
    let floor = rows(8, 5, &picker(), BorderSet::ASCII);
    assert_eq!(line_text(&floor.lines[0]), "+- F… -+");
    assert_eq!(title_span(&floor).as_deref(), Some(" F… "));
}

#[test]
fn a_wide_glyph_that_would_straddle_the_cut_is_dropped_rather_than_halved() {
    // four double-width glyphs against the five cells left beside the
    // mark: the third would straddle the last of them, and half a glyph is
    // not a character
    let cut = rows(12, 5, &titled("日本語版"), BorderSet::ROUNDED);
    assert_eq!(title_span(&cut).as_deref(), Some(" 日本… "));
    assert_eq!(
        cells(&line_text(&cut.lines[0])),
        12,
        "{:?}",
        line_text(&cut.lines[0])
    );

    // the same drop at the floor: three cells for the title, and the wide
    // glyph after the first will not fit beside the mark. What is left is
    // one cell of title, which is a name, and the row is still exact
    let narrow = rows(9, 5, &titled("a日本語"), BorderSet::ASCII);
    assert_eq!(line_text(&narrow.lines[0]), "+- a… --+");
    assert_eq!(title_span(&narrow).as_deref(), Some(" a… "));
}

#[test]
fn an_edge_too_short_to_mark_a_cut_still_carries_the_titles_first_word() {
    // two cells for the title and a wide first glyph: nothing survives the
    // cut once the mark is paid for, and half a glyph is not a name -- but
    // the first word is whole and fits exactly
    let edge = rows(8, 5, &titled("日 本語版"), BorderSet::ROUNDED);
    assert_eq!(line_text(&edge.lines[0]), "╭─ 日 ─╮");
    assert_eq!(title_span(&edge).as_deref(), Some(" 日 "));
}

#[test]
fn only_an_edge_too_small_for_any_form_of_the_title_goes_bare() {
    // one cell for the title: too little for a glyph and the mark both, and
    // no first word that short
    let panel = rows(
        7,
        5,
        &titled("AI Agent -- focused, Esc returns"),
        BorderSet::ROUNDED,
    );
    assert_eq!(line_text(&panel.lines[0]), "╭─────╮");
    assert!(title_span(&panel).is_none());
    assert_eq!(
        line_text(&rows(7, 5, &picker(), BorderSet::ASCII).lines[0]),
        "+-----+",
        "a first word as long as the whole title has no shorter form to fall to"
    );
}

#[test]
fn a_title_of_nothing_a_reader_can_see_never_reaches_an_edge_that_has_no_room() {
    // a combining mark on its own occupies no cell, so a label built from
    // it would be two blank columns naming nothing -- and two columns is
    // more than the smallest framed rects have to give
    for width in [2_u16, 3, 4, 5, 6, 7, 12] {
        let edge = rows(width, 4, &titled("\u{0301}"), BorderSet::ROUNDED);
        assert!(
            title_span(&edge).is_none(),
            "{width}: {:?}",
            line_text(&edge.lines[0])
        );
        assert_eq!(cells(&line_text(&edge.lines[0])), width);
    }
}

/// The geometry the acceptance sweep found the title vanishing at: the AI
/// panel's default 30% share of a 112-column terminal, which is a narrower
/// top edge than its focused title is long.
#[test]
fn the_ai_panels_share_of_a_laptop_width_terminal_still_names_the_panel() {
    let title = "AI Agent -- focused, Esc returns";
    let edge = rows(33, 8, &titled(title), BorderSet::ROUNDED);
    let label = title_span(&edge).expect("a 33-cell edge names the panel");
    assert!(
        label.starts_with(" AI Agent -- "),
        "the panel is named by what survives the cut: {label:?}"
    );
    assert!(label.ends_with("… "), "the cut is marked: {label:?}");
    assert!(
        !label.contains(title),
        "the whole title does not fit, so this width proves nothing if it did: {label:?}"
    );
    assert_eq!(cells(&line_text(&edge.lines[0])), 33);
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
fn a_git_decorated_tree_row_carries_a_styled_glyph_span_before_its_label() {
    use view_core::native::views::{GitMark, TreeRow};
    let spans = tree_row_spans(&TreeRow::leaf(0, "a.txt").with_status(Some(GitMark::Modified)));
    let glyph = spans
        .iter()
        .find(|s| s.role == StyleRole::GitModified)
        .unwrap_or_else(|| panic!("no GitModified-styled span in {spans:?}"));
    assert_eq!(glyph.text.trim(), "M");
    assert!(
        spans
            .iter()
            .any(|s| s.role == StyleRole::Plain && s.text == "a.txt"),
        "the label itself must stay in the plain role: {spans:?}"
    );
}

#[test]
fn an_undecorated_tree_row_carries_only_plain_spans() {
    use view_core::native::views::TreeRow;
    let spans = tree_row_spans(&TreeRow::leaf(0, "a.txt"));
    assert!(
        spans.iter().all(|s| s.role == StyleRole::Plain),
        "a row with no git status must add no styled span: {spans:?}"
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

/// The prompt has to reach the painted frame, not only `AiPanelView`'s own
/// field. A pending permission's question and its options render into the
/// header, above the rule that separates them from the scrolling
/// transcript.
#[test]
fn an_ai_panel_with_a_pending_permission_renders_its_question_and_options() {
    use view_core::native::views::AiPanelView;
    let kind = LayerKind::Ai(
        AiPanelView::new("AI Agent")
            .with_input("")
            .with_pending_permission(vec![
                vec![Span::plain(
                    "Permission requested for Delete config.yaml".to_string(),
                )],
                vec![Span::plain("  Allow once (allow_once)".to_string())],
            ]),
    );
    let framed = rows(50, 8, &kind, BorderSet::ASCII);
    assert!(
        line_text(&framed.lines[2]).contains("Delete config.yaml"),
        "{:?}",
        framed.lines
    );
    assert!(
        line_text(&framed.lines[3]).contains("Allow once (allow_once)"),
        "{:?}",
        framed.lines
    );
    assert!(
        line_text(&framed.lines[4]).contains("---"),
        "the rule still separates the header from the transcript once a \
         permission prompt has grown it"
    );
}

/// An open review has to reach the painted frame, not only
/// `AiPanelView`'s own fields -- and it reaches it as two summary rows in
/// the header and nothing else. The diff itself is drawn in the file by
/// nvim, so the panel goes on painting the transcript underneath: a
/// reader watching the agent work does not lose the conversation the
/// moment a proposal lands.
#[test]
fn an_ai_panel_with_an_open_review_summarizes_it_and_keeps_the_transcript() {
    use view_core::native::views::AiPanelView;
    let transcript: Vec<Vec<Span>> = (0..30)
        .map(|row| vec![Span::plain(format!("transcript row {row}"))])
        .collect();
    let kind = LayerKind::Ai(
        AiPanelView::new("AI Agent")
            .with_input("")
            .with_rows(transcript)
            .with_review(vec![vec![Span::plain(
                "Review src/main.rs -- hunk 3/4, 2 open".to_string(),
            )]]),
    );

    let framed = rows(60, 10, &kind, BorderSet::ASCII);

    let text: Vec<String> = framed.lines.iter().map(|line| line_text(line)).collect();
    assert!(
        text.iter().any(|line| line.contains("hunk 3/4, 2 open")),
        "the review summary belongs in the header: {text:?}"
    );
    assert!(
        text.iter().any(|line| line.contains("transcript row")),
        "the transcript is not displaced by a review any more: {text:?}"
    );
}

/// The whole path the dogfood complaint travelled: state derives a window,
/// the overlay frames it, and what a reader ends up looking at is the
/// newest line. Painted across a range of heights because the panel's own
/// window arithmetic and this module's framing have to agree at every one
/// of them -- a window a row out at some sizes is exactly how the newest
/// line goes missing again.
#[test]
fn the_painted_ai_panel_keeps_the_newest_transcript_row_at_every_height() {
    use view_core::native::ai_panel::{AiPanelState, TranscriptRole};
    let mut state = AiPanelState::new();
    for i in 0..200 {
        state.transcript.append_or_extend(
            Some(&format!("m{i}")),
            &format!("line {i}"),
            TranscriptRole::Agent,
        );
    }

    for height in [6_u16, 9, 12, 24, 40] {
        let kind = LayerKind::Ai(state.view(usize::from(height), 60));
        let text: Vec<String> = rows(60, height, &kind, BorderSet::ASCII)
            .lines
            .iter()
            .map(|line| line_text(line))
            .collect();
        assert!(
            text.iter().any(|line| line.contains("● line 199")),
            "a {height}-row panel must still show the newest line: {text:?}"
        );
    }
}

/// Every transcript line is reachable by paging, at every panel height, in
/// both directions -- asserted against the rows the overlay actually paints
/// rather than the rows the panel handed it.
///
/// The held state is where this can go wrong and the following state
/// cannot: a held window spends one of its rows on the "more below" marker,
/// so a page sized to the panel's room rather than to the rows it drew
/// steps over exactly one line per page, invisibly.
#[test]
fn paging_a_held_ai_panel_reaches_every_line_at_every_height() {
    use view_core::native::ai_panel::{AiPanelState, TranscriptRole, TranscriptScroll};
    const LINES: usize = 200;

    fn transcript_lines(state: &AiPanelState, height: u16) -> Vec<String> {
        rows(
            60,
            height,
            &LayerKind::Ai(state.view(usize::from(height), 60)),
            BorderSet::ASCII,
        )
        .lines
        .iter()
        .map(|line| line_text(line))
        .filter(|line| line.contains("● line"))
        .collect()
    }

    for height in [6_u16, 9, 12, 24, 40] {
        let mut state = AiPanelState::new();
        for i in 0..LINES {
            state.transcript.append_or_extend(
                Some(&format!("m{i}")),
                &format!("line {i}"),
                TranscriptRole::Agent,
            );
        }

        let mut back = transcript_lines(&state, height);
        while state.scroll_transcript(TranscriptScroll::PageBack, usize::from(height), 60) {
            back.extend(transcript_lines(&state, height));
        }
        let mut forward = transcript_lines(&state, height);
        while state.scroll_transcript(TranscriptScroll::PageForward, usize::from(height), 60) {
            forward.extend(transcript_lines(&state, height));
        }

        for i in 0..LINES {
            let line = format!("● line {i}");
            assert!(
                back.iter().any(|painted| painted.contains(&line)),
                "a {height}-row panel paged up over {line} without painting it"
            );
            assert!(
                forward.iter().any(|painted| painted.contains(&line)),
                "a {height}-row panel paged down over {line} without painting it"
            );
        }
        assert!(
            transcript_lines(&state, height)
                .last()
                .is_some_and(|line| line.contains("● line 199")),
            "paging all the way down ends on the newest line, following again"
        );
    }
}

/// The composer half of the same path the dogfood complaint travelled: a
/// prompt longer than the panel is wide reaches the painted frame whole,
/// wrapped under its own prompt mark, with its tail on the last of its
/// rows. Painted across a range of widths because the panel wraps at the
/// column this module frames to -- a wrap a cell out from the paint is
/// exactly how the tail goes missing again.
#[test]
fn a_painted_ai_panel_wraps_a_long_prompt_and_keeps_its_tail() {
    use view_core::native::ai_panel::AiPanelState;
    for width in [20_u16, 40, 60, 120] {
        let typed: String = (0..usize::from(width) * 3)
            .map(|i| char::from(b'a' + u8::try_from(i % 26).unwrap_or(0)))
            .collect();
        let mut state = AiPanelState::new();
        state.push_input(&typed);

        let view = state.view(24, usize::from(width));
        let tail = view.input.last().cloned().unwrap_or_default();
        let kind = LayerKind::Ai(view);
        let text: Vec<String> = rows(width, 24, &kind, BorderSet::ASCII)
            .lines
            .iter()
            .map(|line| line_text(line))
            .collect();

        // the top edge carries the title, whose own letters are not typed
        let painted: String = text
            .iter()
            .skip(1)
            .flat_map(|line| line.chars().filter(char::is_ascii_lowercase))
            .collect();
        assert_eq!(
            painted, typed,
            "a {width}-wide panel painted every character typed: {text:?}"
        );
        assert!(
            typed.ends_with(&tail) && !tail.is_empty(),
            "the composer's last row is the prompt's own tail"
        );
        assert!(
            text.iter().any(|line| line.contains(&tail)),
            "a {width}-wide panel paints that tail, cursor and all: {text:?}"
        );
    }
}

/// The transcript half of that same complaint: a prompt longer than the
/// panel is wide reads back whole after it is sent, not as one row cut at
/// the frame's edge.
///
/// Painted across a range of widths for the reason the composer's twin
/// above is, and this is the layer that has to assert it: the transcript is
/// laid out as the frame's *item* rows, which open with a selection marker
/// the wrap has to leave room for ([`LIST_MARKER_COLS`]), and a wrap that
/// counted the whole interior lost the last two characters of every row
/// with nothing in `view-core`'s own tests able to see it.
#[test]
fn a_painted_ai_panel_wraps_a_submitted_prompt_and_keeps_every_character() {
    use view_core::native::ai_panel::AiPanelState;
    for width in [20_u16, 40, 60, 120] {
        let sent: String = (0..usize::from(width) * 3)
            .map(|i| char::from(b'a' + u8::try_from(i % 26).unwrap_or(0)))
            .collect();
        let mut state = AiPanelState::new();
        state.transcript.echo_user_prompt(&sent);

        let kind = LayerKind::Ai(state.view(24, usize::from(width)));
        let text: Vec<String> = rows(width, 24, &kind, BorderSet::ASCII)
            .lines
            .iter()
            .map(|line| line_text(line))
            .collect();

        // the top edge carries the title, whose own letters are not sent
        let painted: String = text
            .iter()
            .skip(1)
            .flat_map(|line| line.chars().filter(char::is_ascii_lowercase))
            .collect();
        assert_eq!(
            painted, sent,
            "a {width}-wide panel painted every character sent: {text:?}"
        );
    }
}

/// The prompt mark this module paints and the columns `view-core` wraps the
/// composer to are the same two cells. They are named in different crates
/// (the wrap needs the number, the paint needs the glyph), and a pair that
/// drifted would break every composer row one cell from where it is drawn.
#[test]
fn the_painted_prompt_mark_is_as_wide_as_the_composer_wrap_reserves() {
    assert_eq!(
        format!("{PROMPT_MARK} ").chars().count(),
        view_core::native::ai_panel::PROMPT_COLS
    );
}

/// The header rows the panel's own window arithmetic deliberately does not
/// count (see `AiPanelState::transcript_viewport`) come out of the
/// transcript's oldest rows, never its newest: a permission prompt is
/// exactly when a reader needs to see what the agent just did.
#[test]
fn a_permission_prompt_costs_the_transcripts_oldest_rows_not_its_newest() {
    use view_core::native::ai_event::{PermissionOption, PermissionOptionKind};
    use view_core::native::ai_panel::{AiPanelState, PermissionPrompt, TranscriptRole};
    let mut state = AiPanelState::new();
    state.focused = true;
    for i in 0..200 {
        state.transcript.append_or_extend(
            Some(&format!("m{i}")),
            &format!("line {i}"),
            TranscriptRole::Agent,
        );
    }
    state.pending_permission = Some(PermissionPrompt::new(
        1,
        "call_1",
        Some("Delete config.yaml".to_string()),
        Some("edit".to_string()),
        vec![PermissionOption {
            option_id: "allow-once".to_string(),
            name: "Allow once".to_string(),
            kind: PermissionOptionKind::AllowOnce,
        }],
    ));

    let kind = LayerKind::Ai(state.view(24, 60));
    let text: Vec<String> = rows(60, 24, &kind, BorderSet::ASCII)
        .lines
        .iter()
        .map(|line| line_text(line))
        .collect();

    assert!(
        text.iter().any(|line| line.contains("Allow once")),
        "the answerable option is still on screen: {text:?}"
    );
    assert!(
        text.iter().any(|line| line.contains("● line 199")),
        "and so is the newest transcript line: {text:?}"
    );
}

/// The three-way protection order, pinned end to end. A panel too short
/// for all of it keeps the crash banner first, then the permission's
/// options, and sacrifices the review summary before either: a dead
/// session is what explains why nothing else here will ever answer, and a
/// review is a decision the user can still come back to.
#[test]
fn a_short_ai_panel_keeps_the_crash_banner_over_the_permission_and_the_review() {
    use view_core::native::views::AiPanelView;
    let panel = AiPanelView::new("AI Agent")
        .with_input("draft prompt")
        .with_review(vec![vec![Span::plain(
            "Review src/main.rs -- hunk 1/2".to_string(),
        )]])
        .with_pending_permission(vec![vec![Span::plain(
            "  Allow once (allow_once)".to_string(),
        )]])
        .with_local_error(vec![vec![Span::plain(
            "Error: the agent exited -- dismiss".to_string(),
        )]]);

    // interior = height - 2 = 2: one header row and the rule
    let text: Vec<String> = rows(60, 4, &LayerKind::Ai(panel.clone()), BorderSet::ASCII)
        .lines
        .iter()
        .map(|line| line_text(line))
        .collect();
    assert!(
        text.iter().any(|line| line.contains("the agent exited")),
        "the crash banner is the last row standing: {text:?}"
    );
    assert!(
        !text.iter().any(|line| line.contains("Review src/main.rs")),
        "the review summary is sacrificed before the banner: {text:?}"
    );

    // one row more: the permission's option joins it, the review still not
    let text: Vec<String> = rows(60, 5, &LayerKind::Ai(panel), BorderSet::ASCII)
        .lines
        .iter()
        .map(|line| line_text(line))
        .collect();
    assert!(
        text.iter().any(|line| line.contains("the agent exited")),
        "{text:?}"
    );
    assert!(
        text.iter().any(|line| line.contains("Allow once")),
        "the answerable option outranks the review summary: {text:?}"
    );
    assert!(
        !text.iter().any(|line| line.contains("Review src/main.rs")),
        "{text:?}"
    );
}

/// A request blocking the agent's own turn is unanswerable if the overlay
/// is too short to show how to answer it: the option must survive, and the
/// question and composer line -- context, not action -- are what a short
/// overlay sacrifices first.
#[test]
fn a_short_ai_panel_keeps_the_permissions_options_and_drops_the_question_first() {
    use view_core::native::views::AiPanelView;
    let kind = LayerKind::Ai(
        AiPanelView::new("AI Agent")
            .with_input("draft prompt")
            .with_pending_permission(vec![
                vec![Span::plain(
                    "Permission requested for Delete config.yaml".to_string(),
                )],
                vec![Span::plain("  Allow once (allow_once)".to_string())],
            ]),
    );
    // interior = height - 2 = 2: room for exactly one header row and the
    // rule, not both the composer line and the question above them
    let framed = rows(50, 4, &kind, BorderSet::ASCII);
    let text: Vec<String> = framed.lines.iter().map(|line| line_text(line)).collect();
    assert!(
        text.iter()
            .any(|line| line.contains("Allow once (allow_once)")),
        "the answerable option must survive truncation: {text:?}"
    );
    assert!(
        !text.iter().any(|line| line.contains("Delete config.yaml")),
        "the question is context, sacrificed first under truncation: {text:?}"
    );
    assert!(
        !text.iter().any(|line| line.contains("draft prompt")),
        "the composer line is context, sacrificed first under truncation: {text:?}"
    );
}

/// The crash banner's own falsifiable check, the same shape
/// `an_ai_panel_with_a_pending_permission_renders_its_question_and_options`
/// proves for the permission prompt: a session-local error has to reach the
/// painted frame, not only `AiPanelView`'s own field.
#[test]
fn an_ai_panel_with_a_local_error_renders_its_crash_banner() {
    use view_core::native::views::AiPanelView;
    let kind = LayerKind::Ai(
        AiPanelView::new("AI Agent")
            .with_input("")
            .with_local_error(vec![vec![Span::plain(
                "Error: the agent exited (signal: 9)".to_string(),
            )]]),
    );
    let framed = rows(50, 8, &kind, BorderSet::ASCII);
    assert!(
        line_text(&framed.lines[2]).contains("the agent exited"),
        "{:?}",
        framed.lines
    );
    assert!(
        line_text(&framed.lines[3]).contains("---"),
        "the rule still separates the header from the transcript once a \
         crash banner has grown it"
    );
}

/// A crashed session is unmissable even if the overlay is too short to show
/// everything: the banner must survive, on the same "actionable content
/// outlives context" terms `a_short_ai_panel_keeps_the_permissions_options_and_drops_the_question_first`
/// proves for the permission prompt.
#[test]
fn a_short_ai_panel_keeps_the_crash_banner_and_drops_the_composer_line_first() {
    use view_core::native::views::AiPanelView;
    let kind = LayerKind::Ai(
        AiPanelView::new("AI Agent")
            .with_input("draft prompt")
            .with_local_error(vec![vec![Span::plain(
                "Error: the agent exited (signal: 9)".to_string(),
            )]]),
    );
    // interior = height - 2 = 2: room for exactly one header row and the
    // rule, not both the composer line and the banner above them
    let framed = rows(50, 4, &kind, BorderSet::ASCII);
    let text: Vec<String> = framed.lines.iter().map(|line| line_text(line)).collect();
    assert!(
        text.iter().any(|line| line.contains("the agent exited")),
        "the crash banner must survive truncation: {text:?}"
    );
    assert!(
        !text.iter().any(|line| line.contains("draft prompt")),
        "the composer line is context, sacrificed first under truncation: {text:?}"
    );
}

/// A pasted line break is stored in the composer as it was copied and ends
/// a row when the panel wraps it, so a multi-line prompt paints as the
/// lines it was pasted as -- under the indent that keeps them reading as
/// one field. Painted from the panel's own rows rather than from a row
/// built here, because the shape under test is the one the wrap produces.
/// No control character may reach the terminal either way: one surviving
/// into a row would move the terminal's own cursor out of the frame the
/// panel just drew.
#[test]
fn a_pasted_multi_line_prompt_paints_a_row_per_line() {
    use view_core::native::ai_panel::AiPanelState;
    use view_core::native::views::AiPanelView;
    let mut state = AiPanelState::new();
    state.push_input("first\nsecond");
    let kind =
        LayerKind::Ai(AiPanelView::new("AI Agent").with_input_rows(state.view(10, 40).input));

    let framed = rows(40, 10, &kind, BorderSet::ASCII);
    let text: Vec<String> = framed.lines.iter().map(|line| line_text(line)).collect();

    assert!(
        text.iter().any(|line| line.contains("> first")),
        "the first line keeps the prompt mark: {text:?}"
    );
    assert!(
        text.iter()
            .any(|line| line.contains("  second") && !line.contains('>')),
        "the second reads under it, indented rather than re-marked: {text:?}"
    );
    assert!(
        text.iter().all(|line| !line.chars().any(char::is_control)),
        "no control character reaches the terminal: {text:?}"
    );
}

/// The caret follows a multi-line prompt down: the row it lands on is the
/// composer's last, counted through the same header the panel was drawn
/// from. A caret placed from the composer's first row instead would sit on
/// the line above the one being typed the moment a prompt has two.
#[test]
fn the_caret_lands_on_the_last_row_of_a_multi_line_prompt() {
    use view_core::native::ai_panel::AiPanelState;
    use view_core::native::views::AiPanelView;
    let (width, height) = (40, 10);
    let mut state = AiPanelState::new();
    state.push_input("first\nsecond");
    let view = AiPanelView::new("AI Agent")
        .with_input_rows(state.view(usize::from(height), usize::from(width)).input);

    let framed = rows(
        width,
        height,
        &LayerKind::Ai(view.clone()),
        BorderSet::ASCII,
    );
    let (row, col) = ai_caret(&view, width, height).expect("the panel has cells");
    let text = line_text(&framed.lines[usize::from(row)]);

    assert!(
        text.contains("second"),
        "the caret must be on the prompt's last row: {text:?}"
    );
    assert_eq!(
        text.chars().take(usize::from(col)).collect::<String>(),
        "|   second",
        "the caret stands one cell past the last character painted"
    );
}

/// One row shorter still than
/// `a_short_ai_panel_keeps_the_crash_banner_and_drops_the_composer_line_first`,
/// where the header's one surviving row and the rule are no longer both
/// affordable: the banner must still win that contest, not the rule. A
/// rule folded into `header` as its own trailing `Line::Rule` -- the shape
/// every overlay body used before `Body::rule` existed -- would keep the
/// rule instead at this exact budget, since it was the literal last
/// element of the vector the old "keep the last `budget` rows" slice read
/// from; that is the defect this test exists to pin.
#[test]
fn a_maximally_short_ai_panel_keeps_the_crash_banner_over_the_rule() {
    use view_core::native::views::AiPanelView;
    let kind = LayerKind::Ai(
        AiPanelView::new("AI Agent")
            .with_input("draft prompt")
            .with_local_error(vec![vec![Span::plain(
                "Error: the agent exited (signal: 9)".to_string(),
            )]]),
    );
    // interior = height - 2 = 1: room for exactly one row, and it must go
    // to the banner, not the rule and not the composer line
    let framed = rows(50, 3, &kind, BorderSet::ASCII);
    let text: Vec<String> = framed.lines.iter().map(|line| line_text(line)).collect();
    assert!(
        text.iter().any(|line| line.contains("the agent exited")),
        "the crash banner must outrank the rule at the tightest budget: {text:?}"
    );
    assert!(
        !text.iter().any(|line| line.contains("draft prompt")),
        "the composer line is still context, still sacrificed first: {text:?}"
    );
}

/// The counted header and the built one must agree row for row: the caret's
/// row is resolved from the count, and the panel is laid out from the build,
/// so a row group that reaches one and not the other puts the caret on a row
/// the panel never drew.
#[test]
fn the_counted_ai_header_is_as_long_as_the_built_one_for_every_row_group() {
    use view_core::native::views::AiPanelView;

    let plain = AiPanelView::new("AI Agent");
    let empty_composer = AiPanelView::new("AI Agent").with_input_rows(Vec::new());
    let everything = AiPanelView::new("AI Agent")
        .with_input_rows(vec!["one".to_string(), "two".to_string()])
        .with_usage(vec![Span::plain("context 10/100".to_string())])
        .with_review(vec![vec![Span::plain("src/main.rs hunk 1/2".to_string())]])
        .with_pending_permission(vec![
            vec![Span::plain("Permission requested".to_string())],
            vec![Span::plain("  1 Allow (allow_once)".to_string())],
        ])
        .with_local_error(vec![vec![Span::plain(
            "Error: the agent exited".to_string(),
        )]]);

    for view in [plain, empty_composer, everything] {
        assert_eq!(
            ai_header_len(&view),
            ai_header(&view).len(),
            "the counted header must be the built header's own length"
        );
    }
}

/// The caret's row is counted through the same header the panel was drawn
/// from, so the accounting row above the composer moves the caret down with
/// the text it moved: a caret counted from the frame's top edge instead
/// would sit on the accounting row itself the moment a session reports its
/// first usage.
#[test]
fn the_caret_lands_on_the_composer_row_beneath_the_accounting_row() {
    use view_core::native::views::AiPanelView;
    let view = AiPanelView::new("AI Agent")
        .with_input("hello")
        .with_usage(vec![Span::plain("context 10/100".to_string())]);
    let (width, height) = (40, 10);

    let framed = rows(
        width,
        height,
        &LayerKind::Ai(view.clone()),
        BorderSet::ASCII,
    );
    let (row, col) = ai_caret(&view, width, height).expect("the panel has cells");
    let text = line_text(&framed.lines[usize::from(row)]);

    assert!(
        text.contains("> hello"),
        "the caret must be on the composer's own row: {text:?}"
    );
    assert_eq!(
        text.chars().take(usize::from(col)).collect::<String>(),
        "| > hello",
        "the caret stands one cell past the last character painted"
    );
}

/// A panel too short to paint the composer at all still owns the keyboard,
/// so its caret stays inside its own frame. Falling back to the engine's
/// grid cursor there would put the caret on the buffer while every
/// keystroke went to the panel, which is the exact confusion a caret exists
/// to settle.
#[test]
fn a_panel_too_short_to_paint_the_composer_keeps_the_caret_inside_its_frame() {
    use view_core::native::views::AiPanelView;
    let view = AiPanelView::new("AI Agent")
        .with_input("draft prompt")
        .with_local_error(vec![vec![Span::plain(
            "Error: the agent exited".to_string(),
        )]]);
    let (width, height) = (50, 4);

    let framed = rows(
        width,
        height,
        &LayerKind::Ai(view.clone()),
        BorderSet::ASCII,
    );
    let text: Vec<String> = framed.lines.iter().map(|line| line_text(line)).collect();
    let (row, col) = ai_caret(&view, width, height).expect("the panel has cells");

    assert!(
        !text.iter().any(|line| line.contains("draft prompt")),
        "this height is the one where the composer's row was cut: {text:?}"
    );
    assert_eq!(
        (row, col),
        interior_origin(width, height),
        "a cut composer parks the caret on the panel's first interior cell"
    );
}

/// A rect with no cells has nowhere to put a caret, and a caret named on it
/// would be a position no painter can honour.
#[test]
fn a_panel_with_no_cells_names_no_caret() {
    use view_core::native::views::AiPanelView;
    let view = AiPanelView::new("AI Agent").with_input("hello");
    assert_eq!(ai_caret(&view, 0, 10), None);
    assert_eq!(ai_caret(&view, 40, 0), None);
}

/// Walks the whole tier population against both answers of the bit that
/// actually decides, so a tier added to `Tier` cannot quietly acquire a
/// charset of its own: the row a tier sits in is the box-glyph bit's, at
/// every tier there is.
#[test]
fn the_box_glyph_bit_alone_maps_to_a_border_charset() {
    for tier in [Tier::Full, Tier::Standard, Tier::Basic] {
        assert_eq!(
            BorderSet::for_caps(caps(tier, true)),
            BorderSet::ROUNDED,
            "{tier:?} draws box glyphs"
        );
        assert_eq!(
            BorderSet::for_caps(caps(tier, false)),
            BorderSet::ASCII,
            "{tier:?} does not draw box glyphs"
        );
    }
    assert_ne!(BorderSet::ASCII.horizontal, BorderSet::ROUNDED.horizontal);
}

/// The charset a layer carries follows from its kind and its terminal's
/// box-glyph bit, and nothing else: a native overlay always gets one (never
/// the silent blank rect an unframed native kind painted), a non-overlay
/// kind never does (never the framed nothing an engine grid handed a
/// charset produced).
#[test]
fn a_layers_charset_is_derived_from_its_kind_and_its_box_glyph_bit() {
    let rect = crate::Rect {
        row: 2,
        col: 4,
        width: 20,
        height: 6,
    };
    let layer = crate::Layer::new(rect, picker(), caps(Tier::Standard, true));
    assert_eq!(layer.rect, rect);
    assert_eq!(layer.borders, Some(BorderSet::ROUNDED));
    assert!(layer.kind.is_native_overlay());
    assert_eq!(
        crate::Layer::new(rect, picker(), caps(Tier::Standard, false)).borders,
        Some(BorderSet::ASCII),
        "the same kind at the same tier, without the glyphs"
    );

    for kind in [LayerKind::EngineGrid, LayerKind::Shell] {
        assert!(!kind.is_native_overlay(), "{kind:?}");
        let layer = crate::Layer::new(rect, kind, caps(Tier::Full, true));
        assert_eq!(
            layer.borders, None,
            "a kind with no body to lay out must carry no frame"
        );
    }
}

/// A spot check, not the drift guard. The guard against
/// `is_native_overlay` and `body` naming different sets is that both are
/// matched exhaustively over `LayerKind` with no wildcard arm, so a variant
/// added to one without the other does not compile -- that is enforcement
/// this test could not add, since it can only ever sample the variants
/// someone remembered to list here.
///
/// What it does buy: a reading of both rules against each other on real
/// values, so an arm moved to the wrong side (still exhaustive, still
/// compiling, now wrong) is caught. Three of thirteen variants -- the two
/// non-overlay kinds constructible without `view-core`'s `#[non_exhaustive]`
/// wire-state structs, plus one overlay.
#[test]
fn the_framing_predicate_and_the_layout_pass_agree_on_the_kinds_sampled_here() {
    for kind in [picker(), LayerKind::EngineGrid, LayerKind::Shell] {
        let laid = rows(20, 6, &kind, BorderSet::ASCII);
        assert_eq!(
            kind.is_native_overlay(),
            !laid.lines.is_empty(),
            "{kind:?} is framed by one rule and laid out by the other"
        );
    }
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
    let out = clip_spans(&spans, 3);
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
    let out = clip_spans(&spans, 2);
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
    let out = clip_spans(&spans, 6);
    assert_eq!(out, spans);
}

/// Padding past the last span's content is always its own trailing
/// `StyleRole::Plain` span, never merged into whatever role the row's last
/// real content span carries.
#[test]
fn clip_spans_pads_with_a_trailing_plain_span_not_the_last_roles_span() {
    let spans = vec![Span::new("AB", StyleRole::Mode)];
    let out = clip_spans(&spans, 5);
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
