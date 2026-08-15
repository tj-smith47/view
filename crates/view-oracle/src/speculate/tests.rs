//! The falsifiable half of the speculative-echo battery.
//!
//! Every case runs against a real pinned engine and asserts on the report it
//! produced rather than on a screen dump read by eye: the pass/fail reading
//! is [`SpecReport::is_success`], the same one a manual reproduction prints,
//! so a case cannot pass here and fail there.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use view_core::model::Model;
use view_core::msg::RpcCall;
use view_core::native::speculate::{SpecStamp, SPECULATION_MAX_AGE};

use view_core::native::speculate::{fold_engine_call, fold_expiry, fold_redraw};

use super::{age_deadline, run_case, SpecCase, SpecReport};

/// Runs `case` and returns its report, failing with the report's own line
/// (and every divergence it carries) rather than a bare boolean: a
/// divergence must name the row and the two renderings that disagreed, or
/// whoever reads the failure has to reproduce it before they can start.
fn report(case: SpecCase) -> SpecReport {
    let report = run_case(case).expect("a pinned nvim must drive the case");
    assert!(
        report.is_success(),
        "{}\n  quiesced: {} fired: {}\n  painted: {:?}\n  settled: {:?}\n  divergences: {:#?}",
        report.report_line(),
        report.quiesced,
        report.fired,
        report.painted,
        report.settled,
        report.divergences,
    );
    report
}

// -- the round trip the feature exists to hide -------------------------

/// The first case, and the one every other case is a variation of: a burst
/// typed faster than the engine answers is painted in full, and one
/// full-line redraw covering its head retires the whole run.
///
/// The tail is the half a per-cell reading would get wrong: a redraw that
/// answers the burst's first column answers the last one too, because the
/// run it arrives in covers every column in between. What must be left
/// afterwards is the authoritative line and nothing else.
#[test]
fn a_burst_answered_by_one_full_line_redraw_leaves_no_orphan_tail() {
    let report = report(SpecCase::BurstTail);

    assert_eq!(
        report.painted.pending, 6,
        "the whole burst must be predicted, not just its head"
    );
    assert!(
        report.painted.text.starts_with("abcdef"),
        "the burst must be on screen before the engine answers: {:?}",
        report.painted.text
    );
    assert!(
        report.settled.text.starts_with("abcdef"),
        "the settled row must read from the authoritative line: {:?}",
        report.settled.text
    );
}

/// The convergence case: what one prediction claims and what the engine
/// afterwards shows are the same cell.
#[test]
fn one_predicted_character_converges_with_what_the_engine_then_shows() {
    let report = report(SpecCase::StraightLine);

    assert_eq!(report.painted.pending, 1);
    assert!(report.settled.text.starts_with('z'));
}

// -- the mask is a window, and it closes -------------------------------

/// The exemption's own closure: a predicted row is masked from the parity
/// diff while the layer is up and unmasked the moment it comes down, so the
/// settled comparison of that row runs against nvim's own screen with
/// nothing hidden.
///
/// Asserted over every case rather than one, because the mask is derived
/// from the layer list and a case that never painted a layer would satisfy
/// the closure vacuously.
#[test]
fn the_parity_mask_never_outlives_the_layer_that_earned_it() {
    for case in SpecCase::all() {
        let report = report(case);
        assert!(
            report.painted.masked,
            "{}: a pending prediction must be masked while it is painted",
            case.label()
        );
        assert!(
            !report.settled.masked,
            "{}: the row stayed masked after the layer came down",
            case.label()
        );
        assert!(
            !report.settled.layer,
            "{}: a Speculated layer survived the settle",
            case.label()
        );
        assert_eq!(
            report.settled.pending,
            0,
            "{}: a prediction survived the settle",
            case.label()
        );
        assert!(
            report.divergences.is_empty(),
            "{}: view's settled screen disagrees with nvim's own: {:#?}",
            case.label(),
            report.divergences
        );
    }
}

/// A glyph shown speculatively that nvim then contradicts: the abbreviation
/// rewrites the typed text, so two of the four painted cells were never
/// going to be confirmed. What matters is not that the guess was wrong -- it
/// is that the wrong glyph is gone at the settle point, with the corrected
/// content in its place.
#[test]
fn a_mispredicted_glyph_is_shown_and_never_survives_the_settle() {
    let report = report(SpecCase::Mispredict);

    assert!(
        report.painted.text.starts_with("teh."),
        "the mispredicted glyphs must actually reach the screen: {:?}",
        report.painted.text
    );
    assert!(
        report.settled.text.starts_with("the."),
        "the corrected content must win: {:?}",
        report.settled.text
    );
    assert!(
        !report.settled.text.starts_with("teh"),
        "a ghost of the misprediction survived: {:?}",
        report.settled.text
    );
}

/// The reordering case: the answer to a burst arrives after something else
/// has already invalidated it. A late batch may confirm nothing, and the
/// glyphs must come off the screen when the invalidation lands rather than
/// when the redraw finally does.
#[test]
fn a_burst_invalidated_before_its_answer_arrives_leaves_nothing_behind() {
    let report = report(SpecCase::LateRedraw);

    assert_eq!(report.painted.pending, 3);
    assert!(
        report.settled.text.starts_with("abc"),
        "the authoritative line must still be what is shown: {:?}",
        report.settled.text
    );
}

// -- the invalidation classes ------------------------------------------

/// Content moved, dropped or reshaped without the moved cells being resent:
/// each of these retires a pending prediction on the batch that carries it,
/// because the coordinates the prediction holds no longer name the place its
/// glyph was predicted for.
#[test]
fn a_scroll_a_clear_and_a_resize_each_retire_what_is_pending() {
    for case in [SpecCase::Scroll, SpecCase::Clear, SpecCase::Resize] {
        let report = report(case);
        assert!(
            report.fired,
            "{}: the schedule never produced the redraw kind it exists for",
            case.label()
        );
    }
}

/// Text and cursor movement that reaches the engine without any character
/// reaching the predictor: both owe the invalidation a notation key owes,
/// and both must pay it before any redraw arrives.
#[test]
fn a_paste_and_a_click_retire_what_is_pending_before_any_redraw() {
    for case in [SpecCase::Paste, SpecCase::Mouse] {
        let report = report(case);
        assert!(
            report.fired,
            "{}: the call left predictions pending",
            case.label()
        );
    }
}

// -- the bound that does not need a redraw -----------------------------

/// A window that moves under a typing burst in a redraw that repaints line
/// by line: no cell in the batch answers the predicted ones, and what
/// retires them is the viewport report itself.
///
/// The assertion that carries the weight is `retired_after: None`, checked
/// by `is_success` for every case that is not the clock's own: the glyphs
/// are gone at the settle point, not a second later when the age bound
/// sweeps up what nothing else answered.
#[test]
fn a_viewport_shift_retires_the_burst_on_the_batch_that_moved_the_window() {
    let report = report(SpecCase::ViewportShift);

    assert!(
        report.fired,
        "the schedule never moved the window it exists to move"
    );
    assert_eq!(
        report.retired_after, None,
        "the shift was left to the age bound instead of retiring on its own batch"
    );
}

/// The one retirement a redraw cannot deliver: a prediction the engine never
/// answers is taken off the screen by the age bound, on a pass no traffic
/// arrived on.
///
/// Bounded on both sides. Retiring early would mean the bound is not the
/// number it says it is, and retiring late (or never) is the stale glyph the
/// bound exists to prevent.
#[test]
fn an_unanswered_prediction_retires_on_the_age_bound_and_not_before() {
    let report = report(SpecCase::AgeBound);

    let took = report
        .retired_after
        .expect("an unanswered prediction must retire");
    assert!(
        took >= SPECULATION_MAX_AGE,
        "retired {took:?} in, before the bound at {SPECULATION_MAX_AGE:?}"
    );
    assert!(
        took <= age_deadline(),
        "retired {took:?} in, past the deadline at {:?}",
        age_deadline()
    );
}

// -- the caret settles like the glyphs do ------------------------------

/// The caret is measured the same way a glyph is: ahead of the engine while
/// predictions are painted on its row, and authoritative again the instant
/// they are gone -- where authoritative means nvim's own report of the
/// column it is showing the caret at, not view's own idea of it.
#[test]
fn the_speculative_caret_leads_the_burst_and_settles_on_nvims_own_column() {
    let report = report(SpecCase::BurstTail);

    let (painted, engine) = report.painted.caret;
    let painted = painted.expect("a painted burst must carry a caret");
    assert!(
        painted > engine,
        "the caret must lead a painted burst: painted {painted}, engine {engine}"
    );
    let (settled, engine, nvim) = report.settled.caret;
    let settled = settled.expect("a settled frame must carry a caret");
    assert_eq!(
        settled, engine,
        "the engine's cursor must be authoritative once nothing is pending"
    );
    assert_eq!(settled, nvim, "view's caret disagrees with nvim's own");
}

// -- the folds are production code, driven here exactly as the loop drives
//    them: one stamped keystroke, one batch, one aged-out pass ------------

/// The battery drives the runtime's own folds rather than a copy of them,
/// which is what makes every case above evidence about the runtime. Pinned
/// at the seam: a stamped keystroke claims a cell, a batch that never
/// reaches it leaves it standing, and the bound retires it on a pass no
/// redraw arrived on.
#[test]
fn the_battery_drives_the_runtime_folds_a_stamp_at_a_time() {
    let mut model = insert_model();
    fold_engine_call(&mut model, &input("x"), SpecStamp::new(Duration::ZERO));
    assert_eq!(model.speculate.pending().len(), 1);

    fold_redraw(&mut model, &[view_core::events::UiEvent::Flush]);
    assert_eq!(
        model.speculate.pending().len(),
        1,
        "a batch that answers nothing retired a prediction"
    );

    fold_expiry(
        &mut model,
        SpecStamp::new(SPECULATION_MAX_AGE - Duration::from_millis(1)),
    );
    assert_eq!(
        model.speculate.pending().len(),
        1,
        "retired inside the bound"
    );

    fold_expiry(&mut model, SpecStamp::new(SPECULATION_MAX_AGE));
    assert!(model.speculate.pending().is_empty());
}

/// A model parked in insert mode with a grid the cursor sits inside.
fn insert_model() -> Model {
    use view_core::grid::GridOp;
    let mut model = Model::with_term_size(super::COLS, super::ROWS);
    model.engine.apply_grid(GridOp::Resize {
        width: super::COLS,
        height: super::ROWS,
    });
    model
        .engine
        .apply_grid(GridOp::CursorGoto { row: 2, col: 4 });
    model.engine.mode.current = "insert".to_string();
    model.dirty = false;
    model
}

/// One engine-bound keystroke.
fn input(notation: &str) -> RpcCall {
    RpcCall::Input {
        notation: notation.to_string(),
    }
}
