// declares its own test-only-ness rather than inheriting it from the
// `#[cfg(test)] mod tests;` line that reaches it: the per-file path of
// `scripts/audit-god-files.sh` (and so the on-save hook) classifies a file
// by that file alone, and without this it reads a sibling test module as
// production code over the ceiling
#![cfg(test)]
// clippy before 1.94 reads the file-level gate and the one on the
// `#[cfg(test)] mod` declaration that reaches this file as the same
// attribute written twice; the file-level one is load-bearing for the
// reason directly above, so the report is answered where it is raised
#![allow(clippy::duplicated_attributes)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use super::*;
use crate::events::UiEvent;
use crate::model::{OverlayId, OverlayKind};
use crate::msg::{ExitInfo, RegisterType, ReplyToken};
use crate::native::geometry::OverlayBox;
use crate::native::supervision::{
    ReconnectProgress, SupervisionChoice, WedgeKind, AUTOMATIC_RECOVERY_ATTEMPTS,
    ENGINE_BUSY_MODAL_THRESHOLD, INTERRUPT_NOTATION, INTERRUPT_REACTION_WINDOW, QUIT_NOTATION,
    RESTART_NOTATION,
};
use std::time::Duration;

/// Every message line currently on screen, joined per row -- the same
/// selection `view-surface` paints from, so a test asserting on it is
/// asserting on what a user would read.
fn visible_texts(model: &Model) -> Vec<String> {
    model
        .engine
        .messages
        .visible_lines(4)
        .into_iter()
        .map(|spans| spans.into_iter().map(|s| s.text).collect::<String>())
        .collect()
}

fn model() -> Model {
    Model::new()
}

/// A model on an 80x24 terminal, so an overlay's share of it resolves to
/// a rect with cells both inside and outside to click on.
fn full_screen_model() -> Model {
    Model::with_term_size(80, 24)
}

/// A real `OverlayKind::Prompt`, built through the same `MsgShow` path
/// production uses. Stands in for "some open overlay" in tests that
/// only exercise the overlay stack's own behavior -- routing,
/// geometry, focus, stacking -- and do not care about prompt
/// semantics: `Prompt` is the only concrete overlay kind there is.
fn some_overlay_kind() -> OverlayKind {
    let mut throwaway = Model::new();
    let _ = update(
        &mut throwaway,
        Msg::Redraw(vec![UiEvent::MsgShow {
            kind: "confirm".into(),
            content: vec![(0, "test".into())],
            replace_last: false,
        }]),
    );
    throwaway
        .pop_focused_overlay()
        .expect("MsgShow opens a Prompt overlay")
        .kind
}

/// Opens a real, live confirm-style Prompt overlay: `learn_cmdline`
/// puts it in `Answer::Choices` and `model.engine.cmdline` is set the
/// same way a real `cmdline_show` would, together the exact end state
/// nvim leaves a `confirm()` dialog in once both halves of its paired
/// `msg_show`/`cmdline_show` batch have arrived (see
/// docs/prompt-overlay-wire-capture.md section 1). `MsgShow` alone
/// leaves `cmdline` at `None`, a state a live dialog is never actually
/// in: the lazy-dismiss check at the top of `Msg::Key` treats a Prompt
/// overlay open with the cmdline closed as already resolved and
/// awaiting its dismissal keystroke, which is correct for that case
/// and wrong to build by construction here. This stays a raw stack
/// push rather than replaying both events through `update`, so that
/// stacking several of these still creates distinct overlays: a
/// second `MsgShow` while one is already open replaces it in place
/// instead of pushing (see the `MsgShow` handler above). Covers rows
/// 6..18 and columns 20..60 of an 80x24 terminal.
fn open_overlay(model: &mut Model) -> OverlayId {
    let cmdline = CmdlineState {
        content: vec![],
        pos: 0,
        firstc: String::new(),
        prompt: "[Y]es, (N)o: ".into(),
        indent: 0,
        level: 1,
    };
    let mut kind = some_overlay_kind();
    let OverlayKind::Prompt(p) = &mut kind else {
        unreachable!("some_overlay_kind always returns OverlayKind::Prompt")
    };
    p.learn_cmdline(&cmdline);
    model.engine.cmdline = Some(cmdline);
    model.push_overlay(OverlayBox::new(50, 50), kind)
}

/// The ids on the stack, bottom first.
fn stack_ids(model: &Model) -> Vec<u64> {
    model.overlays().iter().map(|o| o.id.0).collect()
}

fn mouse(action: &str, row: u16, col: u16) -> Msg {
    Msg::Mouse(MouseInput {
        button: "left".into(),
        action: action.into(),
        modifier: String::new(),
        row,
        col,
    })
}

fn click(row: u16, col: u16) -> Msg {
    mouse("press", row, col)
}

#[test]
fn redraw_batch_applies_to_grid_and_sets_dirty_only_on_flush() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::Redraw(vec![
            UiEvent::GridResize {
                grid: 1,
                width: 10,
                height: 3,
            },
            UiEvent::GridLine {
                grid: 1,
                row: 0,
                col_start: 0,
                cells: vec![crate::events::GridCell {
                    text: "h".into(),
                    hl_id: 0,
                    repeat: 1,
                }],
            },
        ]),
    );
    assert!(effects.is_empty());
    assert!(!m.dirty, "no Flush yet: must not request paint");
    let effects = update(&mut m, Msg::Redraw(vec![UiEvent::Flush]));
    assert!(effects.is_empty());
    assert!(m.dirty);
    assert_eq!(m.engine.grid().row_text(0).trim_end(), "h");
}

#[test]
fn hl_group_set_records_the_name_to_hl_id_mapping() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::HlGroupSet {
            name: "StatusLine".to_string(),
            hl_id: 41,
        }]),
    );
    assert!(effects.is_empty());
    assert_eq!(m.engine.hl().group("StatusLine"), Some(41));
}

#[test]
fn hl_group_set_overwrites_a_prior_mapping_for_the_same_name() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::HlGroupSet {
            name: "StatusLine".to_string(),
            hl_id: 1,
        }]),
    );
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::HlGroupSet {
            name: "StatusLine".to_string(),
            hl_id: 168,
        }]),
    );
    assert_eq!(m.engine.hl().group("StatusLine"), Some(168));
}

/// The `hl_attr_define` counterpart to the mapping test above: a
/// redefinition that changes exactly one attribute must land, for every
/// attribute a resolved style reads.
///
/// `HlTable::define_attr` drops a redefinition it judges identical to
/// what it already holds, so any field missing from `HlAttr`'s equality
/// turns that field's redefinition into a silent no-op: the stored
/// attributes keep the old value, every cell holding the id keeps
/// painting the old style, and nothing anywhere reports it. A state
/// assertion rather than a clipped-versus-full paint comparison,
/// because those composite both sides from one model and so read the
/// same dropped mutation twice, agreeing with each other about the
/// wrong screen.
#[test]
fn an_hl_attr_redefinition_lands_for_every_field_a_resolved_style_reads() {
    type AttrProbe = fn(&HlAttr) -> bool;
    type StyleProbe = fn(&crate::theme::ResolvedStyle) -> bool;
    const ID: u64 = 7;
    let base = UiEvent::HlAttrDefine {
        id: ID,
        fg: Some(0x111111),
        bg: Some(0x222222),
        bold: false,
        italic: false,
        underline: false,
        reverse: false,
    };
    // each case redefines the baseline with exactly one field moved, so
    // a field dropped from the equality can only be caught by its own
    // case and never masked by another field changing alongside it
    let cases: [(&str, UiEvent, AttrProbe, StyleProbe); 6] = [
        (
            "fg",
            UiEvent::HlAttrDefine {
                id: ID,
                fg: Some(0x999999),
                bg: Some(0x222222),
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
            |a| a.fg == Some(0x999999),
            |s| s.fg == Some(0x999999),
        ),
        (
            "bg",
            UiEvent::HlAttrDefine {
                id: ID,
                fg: Some(0x111111),
                bg: Some(0x888888),
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
            |a| a.bg == Some(0x888888),
            |s| s.bg == Some(0x888888),
        ),
        (
            "bold",
            UiEvent::HlAttrDefine {
                id: ID,
                fg: Some(0x111111),
                bg: Some(0x222222),
                bold: true,
                italic: false,
                underline: false,
                reverse: false,
            },
            |a| a.bold,
            |s| s.bold,
        ),
        (
            "italic",
            UiEvent::HlAttrDefine {
                id: ID,
                fg: Some(0x111111),
                bg: Some(0x222222),
                bold: false,
                italic: true,
                underline: false,
                reverse: false,
            },
            |a| a.italic,
            |s| s.italic,
        ),
        (
            "underline",
            UiEvent::HlAttrDefine {
                id: ID,
                fg: Some(0x111111),
                bg: Some(0x222222),
                bold: false,
                italic: false,
                underline: true,
                reverse: false,
            },
            |a| a.underline,
            |s| s.underline,
        ),
        (
            "reverse",
            UiEvent::HlAttrDefine {
                id: ID,
                fg: Some(0x111111),
                bg: Some(0x222222),
                bold: false,
                italic: false,
                underline: false,
                reverse: true,
            },
            |a| a.reverse,
            |s| s.reverse,
        ),
    ];

    for (field, redefine, attr_probe, style_probe) in cases {
        let mut m = model();
        let _ = update(&mut m, Msg::Redraw(vec![base.clone()]));
        let style_before = crate::theme::Theme::from_hl(m.engine.hl()).style_for(ID, m.engine.hl());
        // the baseline definition is itself a change, so its damage is
        // drained before the redefinition under test runs
        let _ = m.take_paint_damage();

        let _ = update(&mut m, Msg::Redraw(vec![redefine]));

        let stored = m.engine.hl().attr(ID).expect("the id stays defined");
        assert!(
            attr_probe(&stored),
            "{field}: the redefinition never reached the stored attributes ({stored:?})"
        );
        let style_after = crate::theme::Theme::from_hl(m.engine.hl()).style_for(ID, m.engine.hl());
        assert!(
            style_probe(&style_after),
            "{field}: the redefinition never reached the resolved style ({style_after:?})"
        );
        assert_ne!(
            style_before, style_after,
            "{field}: every field here changes the style cells paint with"
        );
        assert!(
            m.take_paint_damage().full,
            "{field}: a landed redefinition restyles every cell holding the id"
        );
    }
}

#[test]
fn key_in_engine_focus_becomes_rpc_input_effect() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: "<C-x>".into(),
        }),
    );
    assert!(matches!(
        &effects[..],
        [Effect::Rpc(RpcCall::Input { notation })] if notation == "<C-x>"
    ));
}

#[test]
fn a_keypress_dismisses_an_already_flushed_transient_toast_and_marks_dirty() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![
            UiEvent::MsgShow {
                kind: "echomsg".into(),
                content: vec![(0, "info".into())],
                replace_last: false,
            },
            UiEvent::Flush,
        ]),
    );
    assert_eq!(m.engine.messages.entries.len(), 1);
    m.dirty = false;

    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: "l".into(),
        }),
    );
    assert!(
        m.engine.messages.entries.is_empty(),
        "a transient toast that already survived one Flush must be dismissed on the next keypress"
    );
    assert!(
        m.dirty,
        "dismissing a visible toast must mark the model dirty for a repaint"
    );
}

#[test]
fn a_keypress_never_dismisses_a_persistent_toast() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![
            UiEvent::MsgShow {
                kind: "echoerr".into(),
                content: vec![(0, "boom".into())],
                replace_last: false,
            },
            UiEvent::Flush,
        ]),
    );

    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: "l".into(),
        }),
    );
    assert_eq!(
        m.engine.messages.entries.len(),
        1,
        "an error/warn-kind toast must persist across a keypress"
    );
}

#[test]
fn a_keypress_does_not_dismiss_a_transient_toast_shown_in_the_same_batch_pre_flush() {
    // no Flush yet: the toast has not necessarily been painted even
    // once, so it must survive this keypress and only be dismissed on
    // the one after -- guarantees at least one visible frame
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::MsgShow {
            kind: "echomsg".into(),
            content: vec![(0, "info".into())],
            replace_last: false,
        }]),
    );

    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: "l".into(),
        }),
    );
    assert_eq!(m.engine.messages.entries.len(), 1);
}

// routing table: focus x input-kind -> effect. Pins the seam native
// overlays build on, using a test-only Focus::Native(OverlayId)
// placeholder in the absence of a real overlay implementation.

#[test]
fn paste_in_engine_focus_becomes_rpc_paste_effect() {
    let mut m = model();
    let effects = update(&mut m, Msg::Paste("hello\nworld".into()));
    assert!(matches!(
        &effects[..],
        [Effect::Rpc(RpcCall::Paste { text })] if text == "hello\nworld"
    ));
}

#[test]
fn mouse_in_engine_focus_becomes_rpc_input_mouse_effect() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::Mouse(MouseInput {
            button: "left".into(),
            action: "press".into(),
            modifier: String::new(),
            row: 5,
            col: 10,
        }),
    );
    assert!(matches!(
        &effects[..],
        [Effect::Rpc(RpcCall::InputMouse {
            button,
            action,
            modifier,
            row: 5,
            col: 10,
        })] if button == "left" && action == "press" && modifier.is_empty()
    ));
}

#[test]
fn mouse_row_is_offset_by_reserved_chrome_rows_before_reaching_the_engine() {
    use crate::events::{TabEntry, TabHandle};
    let mut m = model();
    // opening a second tab reserves one chrome row for the tabline
    // (Model::chrome_rows), so a click on terminal row 3 must land on
    // grid row 2, not grid row 3.
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::TablineUpdate {
            current: TabHandle(1),
            tabs: vec![
                TabEntry {
                    tab: TabHandle(1),
                    name: "a".into(),
                },
                TabEntry {
                    tab: TabHandle(2),
                    name: "b".into(),
                },
            ],
        }]),
    );
    let effects = update(
        &mut m,
        Msg::Mouse(MouseInput {
            button: "left".into(),
            action: "press".into(),
            modifier: String::new(),
            row: 3,
            col: 0,
        }),
    );
    assert!(matches!(
        &effects[..],
        [Effect::Rpc(RpcCall::InputMouse { row: 2, .. })]
    ));
}

#[test]
fn mouse_click_on_a_reserved_chrome_row_is_dropped_not_forwarded() {
    use crate::events::{TabEntry, TabHandle};
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::TablineUpdate {
            current: TabHandle(1),
            tabs: vec![
                TabEntry {
                    tab: TabHandle(1),
                    name: "a".into(),
                },
                TabEntry {
                    tab: TabHandle(2),
                    name: "b".into(),
                },
            ],
        }]),
    );
    let effects = update(
        &mut m,
        Msg::Mouse(MouseInput {
            button: "left".into(),
            action: "press".into(),
            modifier: String::new(),
            row: 0,
            col: 0,
        }),
    );
    assert!(
        effects.is_empty(),
        "click on the tabline row must not reach the engine grid"
    );
}

#[test]
fn key_in_native_focus_is_consumed_and_esc_forwards_without_closing() {
    // a live choice prompt's Esc resolves/aborts nvim's own blocking
    // confirm() call, so it must reach the engine rather than pop
    // locally: a local-only pop would desync view's overlay from
    // nvim's still-blocked prompt, exactly the silent-hang class this
    // overlay exists to prevent (docs/prompt-overlay-wire-capture.md
    // section 2). The overlay only closes later, via the lazy-dismiss
    // keypress that follows nvim's own cmdline_hide -- covered
    // separately below.
    let mut m = model();
    let id = open_overlay(&mut m);
    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: "x".into(),
        }),
    );
    assert!(
        effects.is_empty(),
        "native focus consumes keys that name none of the prompt's choices"
    );
    assert_eq!(
        m.focus(),
        Focus::Native(id),
        "a key naming none of the choices must not close the overlay"
    );

    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: "<Esc>".into(),
        }),
    );
    assert!(
        matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::Input { notation })] if notation == "<Esc>"
        ),
        "Esc on a resolved choice prompt must reach the still-blocked engine: {effects:?}"
    );
    assert_eq!(
        m.focus(),
        Focus::Native(id),
        "forwarding Esc to the engine does not itself close the overlay"
    );
    assert_eq!(m.overlays().len(), 1, "Esc alone pops nothing");

    // nvim resolves its own confirm() call and hides the cmdline; the
    // overlay's lazy-dismiss timing then closes it on the next key,
    // exactly like the underlying toast (see the `Msg::Key` handler's
    // own doc comment)
    let _ = update(&mut m, Msg::Redraw(vec![UiEvent::CmdlineHide]));
    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: "j".into(),
        }),
    );
    assert!(
        matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::Input { notation })] if notation == "j"
        ),
        "closing the overlay hands focus back to the engine within the \
             same dispatch, so the dismissing keystroke also reaches it, \
             the same double duty a keypress already does for dismissing \
             a transient toast: {effects:?}"
    );
    assert_eq!(
        m.focus(),
        Focus::Engine,
        "closing the last overlay returns focus to the engine"
    );
    assert!(m.overlays().is_empty());
}

#[test]
fn any_sequence_of_pushes_pops_and_escapes_leaves_focus_on_the_stack_top() {
    // a seeded xorshift rather than a property-test crate: view-core
    // carries no dev-dependency beyond criterion, and the invariant lives
    // in a state space (stack depth crossed with input kind) small enough
    // that a fixed seed walks all of it. The expected focus comes from a
    // shadow stack rebuilt from the operations alone, so the assertion
    // cannot restate the implementation it checks.
    let mut rng = 0x2545_f491_4f6c_dd1d_u64;
    let mut roll = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    for _ in 0..2_000 {
        let mut m = full_screen_model();
        let mut shadow: Vec<u64> = Vec::new();
        let mut ever_issued: Vec<u64> = Vec::new();

        for _ in 0..(roll() % 24) {
            let depth_before = shadow.len();
            match roll() % 7 {
                0 => {
                    let id = open_overlay(&mut m).0;
                    assert!(
                        !ever_issued.contains(&id),
                        "an overlay id must never be reissued: {id}"
                    );
                    ever_issued.push(id);
                    shadow.push(id);
                }
                1 => {
                    m.pop_focused_overlay();
                    shadow.pop();
                }
                2 => {
                    let effects = update(
                        &mut m,
                        Msg::Key(Key {
                            notation: "<Esc>".into(),
                        }),
                    );
                    // Esc always reaches the engine now: with no
                    // overlay open it is plain input, and with a
                    // resolved choice prompt on top (every overlay
                    // this generator opens is one) forwarding it IS
                    // the prompt's resolution keystroke -- nvim owns
                    // closing the overlay later, via the lazy-dismiss
                    // keypress that follows its own cmdline_hide,
                    // which this generator never sends. Esc alone
                    // never pops, so the shadow stack does not either.
                    assert!(
                        matches!(
                            &effects[..],
                            [Effect::Rpc(RpcCall::Input { notation })] if notation == "<Esc>"
                        ),
                        "Esc must always reach the engine: {effects:?}"
                    );
                }
                3 => {
                    let effects = update(
                        &mut m,
                        Msg::Key(Key {
                            notation: "j".into(),
                        }),
                    );
                    assert_eq!(
                        effects.is_empty(),
                        depth_before > 0,
                        "an ordinary key reaches the engine only with no overlay open"
                    );
                }
                4 => {
                    let effects = update(&mut m, click(0, 0));
                    assert!(
                        matches!(&effects[..], [Effect::Rpc(RpcCall::InputMouse { .. })]),
                        "the terminal corner is outside every overlay opened here"
                    );
                }
                5 => {
                    // the middle of the terminal is inside every overlay
                    // this generator opens, and outside all of them when
                    // the stack is empty
                    let effects = update(&mut m, click(12, 40));
                    assert_eq!(
                        effects.is_empty(),
                        depth_before > 0,
                        "a click on a covered cell belongs to the overlay covering it"
                    );
                    assert_eq!(
                        m.overlay_at(12, 40).map(|id| id.0),
                        shadow.last().copied(),
                        "the cell is claimed by the topmost overlay over it"
                    );
                }
                _ => {
                    let effects = update(&mut m, Msg::Paste("p".into()));
                    assert_eq!(effects.is_empty(), depth_before > 0);
                }
            }

            assert_eq!(
                stack_ids(&m),
                shadow,
                "the stack must hold exactly what the operations opened"
            );
            let expected = match shadow.last() {
                Some(id) => Focus::Native(OverlayId(*id)),
                None => Focus::Engine,
            };
            assert_eq!(m.focus(), expected, "focus must name the topmost overlay");
            assert!(
                shadow.len() + 1 >= depth_before,
                "no single operation may close more than one overlay"
            );
        }
    }
}

#[test]
fn focus_is_the_top_of_the_stack_and_the_engine_when_it_is_empty() {
    let mut m = model();
    assert_eq!(m.focus(), Focus::Engine, "no overlays means engine focus");
    let lower = open_overlay(&mut m);
    assert_eq!(m.focus(), Focus::Native(lower));
    let upper = open_overlay(&mut m);
    assert_eq!(
        m.focus(),
        Focus::Native(upper),
        "the overlay opened last is the one on top"
    );
    m.pop_focused_overlay();
    assert_eq!(m.focus(), Focus::Native(lower));
}

#[test]
fn every_open_overlay_holds_an_id_no_other_one_holds() {
    let mut m = model();
    let ids: Vec<OverlayId> = (0..8).map(|_| open_overlay(&mut m)).collect();
    let mut seen = ids.clone();
    seen.sort_unstable_by_key(|id| id.0);
    seen.dedup_by_key(|id| id.0);
    assert_eq!(seen.len(), ids.len(), "the model must not reissue an id");

    // an id is retired with its overlay rather than recycled: a token
    // held past a close names nothing instead of a later overlay
    let closed = m.pop_focused_overlay().map(|o| o.id);
    let reopened = open_overlay(&mut m);
    assert_ne!(Some(reopened), closed);
    assert_eq!(stack_ids(&m).len(), 8);
}

#[test]
fn esc_on_the_top_of_a_stack_forwards_without_touching_what_is_beneath() {
    // a live choice prompt's Esc forwards to the engine rather than
    // popping (see key_in_native_focus_is_consumed_and_esc_forwards_
    // without_closing): stacking a second one behind the top proves
    // that forwarding leaves the whole stack, not just the top
    // overlay, untouched.
    let mut m = model();
    let picker = open_overlay(&mut m);
    let prompt = open_overlay(&mut m);
    assert_ne!(picker, prompt);

    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: "<Esc>".into(),
        }),
    );
    assert!(
        matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::Input { notation })] if notation == "<Esc>"
        ),
        "Esc on the top choice prompt must reach the engine: {effects:?}"
    );
    assert_eq!(
        m.focus(),
        Focus::Native(prompt),
        "forwarding Esc to the engine does not close the top overlay"
    );
    assert_eq!(
        m.overlays().len(),
        2,
        "Esc alone pops nothing off the stack"
    );
}

#[test]
fn paste_in_native_focus_is_consumed_not_forwarded() {
    let mut m = model();
    open_overlay(&mut m);
    assert!(update(&mut m, Msg::Paste("x".into())).is_empty());
}

#[test]
fn a_click_inside_an_overlay_is_claimed_by_it_and_never_reaches_the_engine() {
    let mut m = full_screen_model();
    let id = open_overlay(&mut m);
    // the overlay covers the middle half of an 80x24 terminal: rows
    // 6..18, columns 20..60
    let effects = update(&mut m, click(12, 40));
    assert!(
        effects.is_empty(),
        "a click on a cell the overlay covers belongs to the overlay"
    );
    assert_eq!(m.overlay_at(12, 40), Some(id));
}

#[test]
fn a_click_outside_an_open_overlay_still_moves_the_engine_cursor() {
    let mut m = full_screen_model();
    open_overlay(&mut m);
    let effects = update(&mut m, click(2, 3));
    assert!(
        matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::InputMouse { row: 2, col: 3, .. })]
        ),
        "grid still visible beside an overlay keeps taking clicks: {effects:?}"
    );
    assert_eq!(m.overlay_at(2, 3), None);
}

#[test]
fn a_click_lands_on_the_topmost_overlay_covering_it_not_the_one_with_focus() {
    let mut m = full_screen_model();
    // a wide overlay with a narrower one stacked on top of it
    let lower = m.push_overlay(OverlayBox::new(100, 100), some_overlay_kind());
    let upper = m.push_overlay(OverlayBox::new(50, 50), some_overlay_kind());
    assert_eq!(
        m.overlay_at(12, 40),
        Some(upper),
        "where both cover the cell, the top one claims it"
    );
    assert_eq!(
        m.overlay_at(0, 0),
        Some(lower),
        "outside the top overlay, the one beneath still claims its own cells"
    );
}

#[test]
fn a_left_anchored_overlay_claims_the_column_it_is_painted_in() {
    use crate::native::geometry::Anchor;
    let mut m = full_screen_model();
    let sidebar = m.push_overlay(
        OverlayBox::new(30, 100).with_anchor(Anchor::Left),
        some_overlay_kind(),
    );
    assert_eq!(
        m.overlay_at(0, 0),
        Some(sidebar),
        "a sidebar owns the terminal's first column, not a centered band"
    );
    assert!(update(&mut m, click(0, 0)).is_empty());
    assert_eq!(m.overlay_at(0, 40), None, "and nothing past its own width");
    assert!(matches!(
        &update(&mut m, click(0, 40))[..],
        [Effect::Rpc(RpcCall::InputMouse { .. })]
    ));
}

// gesture routing: a drag belongs to the surface that took its press for
// the whole of its life, however far the pointer travels

#[test]
fn a_drag_off_an_overlay_stays_with_the_overlay_through_its_release() {
    let mut m = full_screen_model();
    open_overlay(&mut m);
    assert!(update(&mut m, mouse("press", 12, 40)).is_empty());
    assert_eq!(m.mouse_capture(), Some(MouseCapture::Overlay(OverlayId(1))));

    assert!(
        update(&mut m, mouse("drag", 2, 3)).is_empty(),
        "a drag that left the overlay must not start moving the engine cursor"
    );
    assert!(
        update(&mut m, mouse("release", 2, 3)).is_empty(),
        "the engine must not receive a release for a press it never saw"
    );
    assert_eq!(m.mouse_capture(), None, "release ends the gesture");
}

#[test]
fn a_drag_onto_an_overlay_keeps_delivering_to_the_engine_until_release() {
    let mut m = full_screen_model();
    open_overlay(&mut m);
    assert!(matches!(
        &update(&mut m, mouse("press", 2, 3))[..],
        [Effect::Rpc(RpcCall::InputMouse { row: 2, col: 3, .. })]
    ));
    assert_eq!(m.mouse_capture(), Some(MouseCapture::Engine));

    // without capture the engine would be left mid-selection here, with
    // a press it never got to finish
    assert!(
        matches!(
            &update(&mut m, mouse("drag", 12, 40))[..],
            [Effect::Rpc(RpcCall::InputMouse {
                row: 12,
                col: 40,
                ..
            })]
        ),
        "a drag that crossed onto the overlay still belongs to the engine"
    );
    assert!(matches!(
        &update(&mut m, mouse("release", 12, 40))[..],
        [Effect::Rpc(RpcCall::InputMouse { .. })]
    ));
    assert_eq!(m.mouse_capture(), None);
}

#[test]
fn closing_an_overlay_mid_gesture_releases_the_capture_it_held() {
    // closes the overlay by a direct pop rather than Esc: a live
    // choice prompt's Esc forwards to the engine instead of popping
    // (see esc_on_the_top_of_a_stack_forwards_without_touching_what_
    // is_beneath), which is a separate concern from the one this
    // test checks -- that a gesture capture does not outlive the
    // overlay that held it, however the overlay came to close.
    let mut m = full_screen_model();
    open_overlay(&mut m);
    let _ = update(&mut m, mouse("press", 12, 40));
    assert!(m.mouse_capture().is_some());

    m.pop_focused_overlay();
    assert_eq!(
        m.mouse_capture(),
        None,
        "a gesture must not stay captured by an overlay that is gone"
    );
    assert!(
        matches!(
            &update(&mut m, mouse("release", 2, 3))[..],
            [Effect::Rpc(RpcCall::InputMouse { .. })]
        ),
        "with the overlay closed the gesture falls back to position routing"
    );
}

#[test]
fn the_wheel_routes_by_position_and_leaves_a_gesture_in_flight_alone() {
    let mut m = full_screen_model();
    open_overlay(&mut m);
    let _ = update(&mut m, mouse("press", 2, 3));
    assert_eq!(m.mouse_capture(), Some(MouseCapture::Engine));

    let wheel = Msg::Mouse(MouseInput {
        button: "wheel".into(),
        action: "up".into(),
        modifier: String::new(),
        row: 12,
        col: 40,
    });
    assert!(
        update(&mut m, wheel).is_empty(),
        "a wheel over the overlay scrolls the overlay, not the buffer under it"
    );
    assert_eq!(
        m.mouse_capture(),
        Some(MouseCapture::Engine),
        "a wheel carries no gesture and must not steal one in flight"
    );
}

#[test]
fn a_release_with_no_press_behind_it_falls_back_to_position() {
    let mut m = full_screen_model();
    open_overlay(&mut m);
    assert_eq!(m.mouse_capture(), None);
    assert!(matches!(
        &update(&mut m, mouse("release", 2, 3))[..],
        [Effect::Rpc(RpcCall::InputMouse { .. })]
    ));
    assert!(
        update(&mut m, mouse("drag", 12, 40)).is_empty(),
        "an unclaimed drag over the overlay belongs to the overlay"
    );
}

#[test]
fn mouse_on_off_events_set_the_model_flag_the_terminal_reads_for_capture() {
    let mut m = model();
    assert!(!m.engine.mouse_on, "mouse capture must default off");
    let _ = update(&mut m, Msg::Redraw(vec![UiEvent::MouseOn]));
    assert!(m.engine.mouse_on);
    let _ = update(&mut m, Msg::Redraw(vec![UiEvent::MouseOff]));
    assert!(!m.engine.mouse_on);
}

#[test]
fn engine_down_maps_signal_and_code_to_exit_effects() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::EngineDown(ExitInfo {
            code: Some(5),
            by_signal: false,
        }),
    );
    assert!(matches!(&effects[..], [Effect::Quit { exit_code: 5 }]));
    assert!(!m.running);
}

#[test]
fn engine_down_without_code_exits_one_and_signal_code_passes_through() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::EngineDown(ExitInfo {
            code: None,
            by_signal: false,
        }),
    );
    assert!(matches!(&effects[..], [Effect::Quit { exit_code: 1 }]));
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::EngineDown(ExitInfo {
            code: Some(137),
            by_signal: true,
        }),
    );
    assert!(matches!(&effects[..], [Effect::Quit { exit_code: 137 }]));
}

#[test]
fn loop_tokens_are_noops_and_engine_request_always_replies() {
    let mut m = model();
    assert!(update(&mut m, Msg::RedrawReady).is_empty());
    assert!(update(
        &mut m,
        Msg::EngineStopped {
            generation: 1,
            reason: None
        }
    )
    .is_empty());
    let effects = update(
        &mut m,
        Msg::EngineRequest(EngineRequest::VimEnter {
            token: ReplyToken { msgid: 9 },
        }),
    );
    assert!(matches!(
        &effects[..],
        [Effect::Reply {
            token: ReplyToken { msgid: 9 },
            value: ReplyValue::Nil
        }]
    ));
}

/// The worker owns the actual reply (see `Effect::ClipboardRead`'s
/// doc); this arm's whole job is routing the token and register
/// through unmodified.
#[test]
fn clipboard_get_produces_a_clipboard_read_effect_carrying_the_token_and_register() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::EngineRequest(EngineRequest::ClipboardGet {
            token: ReplyToken { msgid: 11 },
            register: '+',
        }),
    );
    assert!(matches!(
        &effects[..],
        [Effect::ClipboardRead {
            token: ReplyToken { msgid: 11 },
            register: '+'
        }]
    ));
}

/// One arm, two effects: the local write and the OSC52 escape are
/// companions (see `Effect::Osc52Copy`'s doc), not a branch on whether
/// a display is present, and both must carry the same `lines` and
/// `regtype` the copy itself carried -- deleting either effect from
/// this arm would fail nothing else in the suite.
#[test]
fn clipboard_set_produces_a_write_and_an_osc52_copy_effect_from_one_arm() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::EngineRequest(EngineRequest::ClipboardSet {
            token: ReplyToken { msgid: 12 },
            register: '*',
            lines: vec!["a".to_string(), "b".to_string()],
            regtype: RegisterType::Linewise,
        }),
    );
    assert!(matches!(
        &effects[..],
        [
            Effect::ClipboardWrite {
                token: ReplyToken { msgid: 12 },
                register: '*',
                lines: ref w_lines,
                regtype: RegisterType::Linewise,
            },
            Effect::Osc52Copy {
                register: '*',
                lines: ref o_lines,
                regtype: RegisterType::Linewise,
            }
        ] if w_lines == &vec!["a".to_string(), "b".to_string()]
            && o_lines == &vec!["a".to_string(), "b".to_string()]
    ));
}

#[test]
fn resize_marks_the_model_dirty_so_the_new_area_is_actually_painted() {
    let mut m = model();
    m.dirty = false;
    let _ = update(
        &mut m,
        Msg::Resized {
            width: 120,
            height: 40,
        },
    );
    assert!(
        m.dirty,
        "a resize changes the paint area, so the frontend must repaint on its own \
             rather than waiting for an engine redraw that a clamped grid_target may never produce"
    );
}

#[test]
fn resize_produces_try_resize_effect() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::Resized {
            width: 120,
            height: 40,
        },
    );
    assert!(matches!(
        &effects[..],
        [Effect::Rpc(RpcCall::TryResize {
            width: 120,
            height: 40
        })]
    ));
}

// fixtures below are shaped from crates/view-engine/src/ui_events.rs's
// decode test fixtures, which are themselves copied from the live
// capture at ~/.claude/tmp/capture-*.log (see that module's tests for
// the raw wire fixtures); these tests exercise state application only, not
// decoding.

#[test]
fn mode_events_update_state_without_dirty() {
    use crate::events::ModeInfo;
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::Redraw(vec![
            UiEvent::ModeInfoSet {
                cursor_style_enabled: true,
                modes: vec![
                    ModeInfo {
                        name: "normal".into(),
                        cursor_shape: "block".into(),
                        ..ModeInfo::default()
                    },
                    ModeInfo {
                        name: "insert".into(),
                        cursor_shape: "vertical".into(),
                        cell_percentage: 25,
                        ..ModeInfo::default()
                    },
                ],
            },
            UiEvent::ModeChange {
                mode: "insert".into(),
                mode_idx: 1,
            },
        ]),
    );
    assert!(effects.is_empty());
    assert!(!m.dirty, "mode events alone must not request paint");
    assert!(m.engine.mode.cursor_style_enabled);
    assert_eq!(m.engine.mode.current, "insert");
    assert_eq!(
        m.engine
            .mode
            .active_cursor()
            .map(|c| c.cursor_shape.as_str()),
        Some("vertical")
    );
}

#[test]
fn mode_state_active_cursor_is_none_before_mode_info_set() {
    let m = model();
    assert!(m.engine.mode.active_cursor().is_none());
}

#[test]
fn cmdline_show_pos_hide_set_and_clear_state() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::CmdlineShow {
            content: vec![(0, "q".into())],
            pos: 1,
            firstc: ":".into(),
            prompt: "".into(),
            indent: 0,
            level: 1,
        }]),
    );
    assert!(!m.dirty);
    let cmdline = m.engine.cmdline.as_ref().expect("cmdline must be set");
    assert_eq!(cmdline.content, vec![(0, "q".to_string())]);
    assert_eq!(cmdline.firstc, ":");

    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::CmdlinePos { pos: 2, level: 1 }]),
    );
    assert_eq!(m.engine.cmdline.as_ref().unwrap().pos, 2);

    let _ = update(&mut m, Msg::Redraw(vec![UiEvent::CmdlineHide]));
    assert!(m.engine.cmdline.is_none());
}

#[test]
fn a_confirm_questions_toast_lives_exactly_as_long_as_the_prompt_does() {
    // the event order is nvim 0.12.4's own, observed over a real RPC
    // session driving `confirm('Save changes?', "&Yes\n&No")`: the
    // question arrives once as msg_show, the answer line separately as
    // cmdline_show, and a key answering none of the choices re-arms the
    // prompt with cmdline_hide + cmdline_show and NO second msg_show.
    let mut m = model();
    let prompt = || UiEvent::CmdlineShow {
        content: vec![],
        pos: 0,
        firstc: String::new(),
        prompt: "[Y]es, (N)o: ".into(),
        indent: 0,
        level: 1,
    };
    let question = || UiEvent::MsgShow {
        kind: "confirm".into(),
        content: vec![(0, "Save changes?".into())],
        replace_last: false,
    };
    let shown = |m: &Model| {
        m.engine
            .messages
            .visible_lines(4)
            .into_iter()
            .map(|spans| spans.into_iter().map(|s| s.text).collect::<String>())
            .collect::<Vec<_>>()
    };

    let _ = update(&mut m, Msg::Redraw(vec![question(), UiEvent::Flush]));
    let _ = update(&mut m, Msg::Redraw(vec![prompt(), UiEvent::Flush]));
    assert_eq!(shown(&m), vec!["Save changes?"]);

    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: "q".into(),
        }),
    );
    assert_eq!(
        shown(&m),
        vec!["Save changes?"],
        "a key the prompt refuses must not take the question away with it"
    );

    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::CmdlineHide, UiEvent::Flush]),
    );
    let _ = update(&mut m, Msg::Redraw(vec![prompt(), UiEvent::Flush]));
    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: "y".into(),
        }),
    );
    assert_eq!(shown(&m), vec!["Save changes?"]);

    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::CmdlineHide, UiEvent::Flush]),
    );
    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: "j".into(),
        }),
    );
    assert!(
        shown(&m).is_empty(),
        "with the prompt closed the question is ordinary transient text"
    );
}

#[test]
fn a_second_distinct_confirm_replaces_the_first_with_no_intervening_keystroke() {
    // nvim's own Lua can resolve one confirm and immediately raise a
    // second, distinct one before the overlay ever sees a keystroke --
    // routine in plugin bootstraps. A genuine re-arm of the SAME
    // question never sends a second msg_show at all (see
    // docs/prompt-overlay-wire-capture.md, section 1), so a msg_show
    // arriving while a Prompt overlay is already open always names a
    // distinct question, never the open one.
    let mut m = model();
    let msg_show = |text: &str| UiEvent::MsgShow {
        kind: "confirm".into(),
        content: vec![(0, text.into())],
        replace_last: false,
    };
    let cmdline_show = |prompt: &str| UiEvent::CmdlineShow {
        content: vec![],
        pos: 0,
        firstc: String::new(),
        prompt: prompt.into(),
        indent: 0,
        level: 1,
    };
    let prompt_view = |m: &Model| match m.overlays().last().map(|ov| &ov.kind) {
        Some(OverlayKind::Prompt(p)) => Some(p.view()),
        _ => None,
    };

    let _ = update(
        &mut m,
        Msg::Redraw(vec![msg_show("Save changes?"), UiEvent::Flush]),
    );
    let _ = update(
        &mut m,
        Msg::Redraw(vec![cmdline_show("[Y]es, (N)o: "), UiEvent::Flush]),
    );
    let first = prompt_view(&m).expect("a prompt overlay must open on the first confirm");
    assert_eq!(first.message, "Save changes?");
    assert_eq!(first.choices, vec!["Yes".to_string(), "No".to_string()]);

    // #1 resolves and #2 arrives in the same redraw batch, with no
    // Msg::Key between them -- the shape a Lua-driven bootstrap sends.
    let _ = update(
        &mut m,
        Msg::Redraw(vec![
            UiEvent::CmdlineHide,
            msg_show("Discard changes?"),
            cmdline_show("[D]iscard, (C)ancel: "),
            UiEvent::Flush,
        ]),
    );

    let second =
        prompt_view(&m).expect("the overlay must stay open, now answering the new question");
    assert_eq!(
        second.message, "Discard changes?",
        "the overlay must show #2's message, not #1's stale one"
    );
    assert_eq!(
        second.choices,
        vec!["Discard".to_string(), "Cancel".to_string()],
        "and #2's choices, coherent with #2's message"
    );
}

#[test]
fn cmdline_pos_without_prior_show_is_a_noop() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::CmdlinePos { pos: 3, level: 1 }]),
    );
    assert!(m.engine.cmdline.is_none());
}

#[test]
fn msg_show_appends_and_replace_last_overwrites() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::MsgShow {
            kind: "echomsg".into(),
            content: vec![(73, "first".into())],
            replace_last: false,
        }]),
    );
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::MsgShow {
            kind: "echomsg".into(),
            content: vec![(73, "second".into())],
            replace_last: false,
        }]),
    );
    assert_eq!(m.engine.messages.entries.len(), 2);

    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::MsgShow {
            kind: "echomsg".into(),
            content: vec![(73, "replaced".into())],
            replace_last: true,
        }]),
    );
    assert_eq!(m.engine.messages.entries.len(), 2);
    assert_eq!(
        m.engine.messages.entries.last().unwrap().content,
        vec![(73, "replaced".to_string())]
    );

    let _ = update(&mut m, Msg::Redraw(vec![UiEvent::MsgClear]));
    assert!(m.engine.messages.entries.is_empty());
}

#[test]
fn tabline_update_sets_current_and_tabs() {
    use crate::events::{TabEntry, TabHandle};
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::TablineUpdate {
            current: TabHandle(2),
            tabs: vec![
                TabEntry {
                    tab: TabHandle(1),
                    name: "[No Name]".into(),
                },
                TabEntry {
                    tab: TabHandle(2),
                    name: "[No Name]".into(),
                },
            ],
        }]),
    );
    assert!(!m.dirty);
    let tabline = m.engine.tabline.as_ref().expect("tabline must be set");
    assert_eq!(tabline.current, TabHandle(2));
    assert_eq!(tabline.tabs.len(), 2);
}

#[test]
fn popupmenu_show_select_hide_set_and_clear_state() {
    use crate::events::PmItem;
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::PopupmenuShow {
            items: vec![PmItem {
                word: "foobar".into(),
                ..PmItem::default()
            }],
            selected: 0,
            row: 0,
            col: 11,
            grid: 1,
        }]),
    );
    assert_eq!(m.engine.popupmenu.as_ref().unwrap().selected, 0);

    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::PopupmenuSelect { selected: 1 }]),
    );
    assert_eq!(m.engine.popupmenu.as_ref().unwrap().selected, 1);

    let _ = update(&mut m, Msg::Redraw(vec![UiEvent::PopupmenuHide]));
    assert!(m.engine.popupmenu.is_none());
}

#[test]
fn popupmenu_select_without_prior_show_is_a_noop() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::PopupmenuSelect { selected: 0 }]),
    );
    assert!(m.engine.popupmenu.is_none());
}

#[test]
fn resize_target_shrinks_by_chrome_rows_once_more_than_one_tab_is_open() {
    use crate::events::{TabEntry, TabHandle};
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::TablineUpdate {
            current: TabHandle(1),
            tabs: vec![
                TabEntry {
                    tab: TabHandle(1),
                    name: "a".into(),
                },
                TabEntry {
                    tab: TabHandle(2),
                    name: "b".into(),
                },
            ],
        }]),
    );
    let effects = update(
        &mut m,
        Msg::Resized {
            width: 80,
            height: 24,
        },
    );
    assert!(matches!(
        &effects[..],
        [Effect::Rpc(RpcCall::TryResize {
            width: 80,
            height: 23
        })]
    ));
}

#[test]
fn tabline_crossing_the_one_tab_boundary_round_trips_the_reserved_row() {
    use crate::events::{TabEntry, TabHandle};
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Resized {
            width: 80,
            height: 24,
        },
    );

    let opened = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::TablineUpdate {
            current: TabHandle(1),
            tabs: vec![
                TabEntry {
                    tab: TabHandle(1),
                    name: "a".into(),
                },
                TabEntry {
                    tab: TabHandle(2),
                    name: "b".into(),
                },
            ],
        }]),
    );
    assert!(
        matches!(
            &opened[..],
            [Effect::Rpc(RpcCall::TryResize {
                width: 80,
                height: 23
            })]
        ),
        "opening a second tab must reserve the tabline row: {opened:?}"
    );

    let closed = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::TablineUpdate {
            current: TabHandle(1),
            tabs: vec![TabEntry {
                tab: TabHandle(1),
                name: "a".into(),
            }],
        }]),
    );
    assert!(
        matches!(
            &closed[..],
            [Effect::Rpc(RpcCall::TryResize {
                width: 80,
                height: 24
            })]
        ),
        "closing back to one tab must release the reserved row: {closed:?}"
    );
}

#[test]
fn tabline_update_within_the_same_tab_count_bucket_emits_no_resize() {
    use crate::events::{TabEntry, TabHandle};
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::TablineUpdate {
            current: TabHandle(1),
            tabs: vec![
                TabEntry {
                    tab: TabHandle(1),
                    name: "a".into(),
                },
                TabEntry {
                    tab: TabHandle(2),
                    name: "b".into(),
                },
            ],
        }]),
    );
    // renaming a tab (still 2 tabs) never crosses the boundary
    let effects = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::TablineUpdate {
            current: TabHandle(1),
            tabs: vec![
                TabEntry {
                    tab: TabHandle(1),
                    name: "a-renamed".into(),
                },
                TabEntry {
                    tab: TabHandle(2),
                    name: "b".into(),
                },
            ],
        }]),
    );
    assert!(effects.is_empty());
}

/// A style change has no rows of its own: the highlight table sits
/// behind every cell's resolved style, so any change to it can restyle
/// the whole screen while no grid cell's text moves. Damage clipped to
/// grid rows alone would repaint whatever row happened to change and
/// leave the rest of the screen in the previous colors.
#[test]
fn a_highlight_change_with_no_grid_change_damages_the_whole_frame() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::GridResize {
            grid: 1,
            width: 8,
            height: 4,
        }]),
    );
    let _ = m.take_paint_damage();

    for ev in [
        UiEvent::DefaultColorsSet {
            fg: Some(0xF8F8F2),
            bg: Some(0x282A36),
            sp: None,
        },
        UiEvent::HlAttrDefine {
            id: 3,
            fg: Some(0xFF0000),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        },
        UiEvent::HlGroupSet {
            name: "StatusLine".to_string(),
            hl_id: 3,
        },
    ] {
        let _ = update(&mut m, Msg::Redraw(vec![ev.clone()]));
        assert!(
            m.take_paint_damage().full,
            "{ev:?} must damage every cell, not none of them"
        );
    }

    let generation = m.engine.hl().probe_generation();
    let _ = update(
        &mut m,
        Msg::HlProbeReply {
            generation,
            fg: Some(0xF8F8F2),
            bg: Some(0x282A36),
        },
    );
    assert!(
        m.take_paint_damage().full,
        "an accepted probe reply re-derives the theme and must damage every cell"
    );
}

/// The one write in the highlight table that marks changed without
/// comparing values first, and the state that makes the difference
/// visible: nvim resends the same default colors, so neither colour
/// moves, but the probe generation those colours carry moves anyway and
/// strands a `confirmed` reply that was matching an instant earlier.
/// The derived theme changes with no value in the write itself
/// changing, so a value comparison here would leave the whole screen
/// painted in a theme the model no longer holds.
///
/// Reaching this live needs `nvim_get_hl`'s answer to differ from the
/// `default_colors_set` that opened the same generation, so the state
/// is built from an explicit event sequence rather than left to a
/// live path to happen upon.
#[test]
fn resending_identical_default_colors_still_damages_the_frame() {
    let mut m = model();
    let resend = UiEvent::DefaultColorsSet {
        fg: Some(0xFFFFFF),
        bg: Some(0),
        sp: None,
    };
    let _ = update(&mut m, Msg::Redraw(vec![resend.clone()]));
    let generation = m.engine.hl().probe_generation();
    // the reply disambiguates the wire-ambiguous zero and reports a fg
    // the wire never carried, so the theme it derives is distinguishable
    // from the one the raw wire values derive on their own
    let _ = update(
        &mut m,
        Msg::HlProbeReply {
            generation,
            fg: Some(0xF8F8F2),
            bg: Some(0x123456),
        },
    );
    let before = crate::theme::Theme::from_hl(m.engine.hl());
    let _ = m.take_paint_damage();

    let _ = update(&mut m, Msg::Redraw(vec![resend]));

    assert_eq!(
        (m.engine.hl().default_fg(), m.engine.hl().default_bg()),
        (Some(0xFFFFFF), Some(0)),
        "the resent colours must be identical, or this proves nothing"
    );
    let after = crate::theme::Theme::from_hl(m.engine.hl());
    assert_ne!(
        before, after,
        "the bumped generation strands the matching reply and re-derives the theme"
    );
    assert!(
        m.take_paint_damage().full,
        "a re-derived theme restyles every cell, however little the write itself changed"
    );
}

/// Installing a whole highlight table is a style change like any other:
/// every painted cell resolves through the table, so the frame after
/// the swap must repaint all of them even though no grid row moved.
/// The installed table is drained clean first, so the only thing that
/// can supply the damage is the install itself.
#[test]
fn installing_a_whole_highlight_table_damages_the_frame() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::GridResize {
            grid: 1,
            width: 8,
            height: 4,
        }]),
    );
    let _ = m.take_paint_damage();

    let mut replacement = crate::hl::HlTable::new();
    replacement.define_attr(
        3,
        HlAttr {
            fg: Some(0x00FF00),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        },
    );
    assert!(
        replacement.take_dirty(),
        "the replacement must arrive carrying no damage of its own"
    );

    m.engine.replace_hl(replacement);

    assert_eq!(
        m.engine.hl().attr(3).map(|a| a.fg),
        Some(Some(0x00FF00)),
        "the replacement must actually be installed"
    );
    assert!(
        m.take_paint_damage().full,
        "a swapped table restyles every cell on screen at once"
    );
}

/// A first-ever definition of an `hl_id` damages the whole frame the
/// same way a redefinition does, and the pessimism is deliberate: the
/// highlight table holds no record of which ids the painted cells
/// reference, so it cannot tell a definition that restyles nothing from
/// one that restyles cells already on screen resolving that id through
/// the `Normal` fallback. Skipping the mark for an id the table has not
/// seen would trade one whole-frame composite for a silently stale
/// screen wherever nvim's own define-before-use ordering does not hold,
/// which the table cannot check, against a damage contract that
/// over-reports rather than under-reports.
#[test]
fn a_first_definition_of_a_previously_unknown_hl_id_still_damages_the_frame() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::GridResize {
            grid: 1,
            width: 8,
            height: 4,
        }]),
    );
    let _ = m.take_paint_damage();
    assert!(
        m.engine.hl().attr(12).is_none(),
        "the id must be one the table has never held"
    );

    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::HlAttrDefine {
            id: 12,
            fg: Some(0x00FF00),
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        }]),
    );

    assert!(
        m.take_paint_damage().full,
        "a first definition may still restyle cells already holding the id"
    );
}

/// The other direction: a definition that changes nothing must not cost
/// a repaint. The value comparison this pins is defensive rather than
/// load-bearing -- the pinned engine was measured never to resend an
/// unchanged definition (see [`HlTable::define_attr`]) -- but an engine
/// that did would turn every resend into a whole-frame repaint, giving
/// back the frames damage clipping exists to save.
#[test]
fn a_highlight_definition_that_changes_nothing_produces_no_damage() {
    let mut m = model();
    let define = UiEvent::HlAttrDefine {
        id: 3,
        fg: Some(0xFF0000),
        bg: None,
        bold: false,
        italic: false,
        underline: false,
        reverse: false,
    };
    let group = UiEvent::HlGroupSet {
        name: "StatusLine".to_string(),
        hl_id: 3,
    };
    let _ = update(&mut m, Msg::Redraw(vec![define.clone(), group.clone()]));
    let _ = m.take_paint_damage();

    let _ = update(&mut m, Msg::Redraw(vec![define, group]));
    let damage = m.take_paint_damage();
    assert!(!damage.full, "an identical redefinition is not a repaint");
    assert!(damage.rows.is_empty(), "and touches no grid row either");
}

#[test]
fn default_colors_set_bumps_generation_and_emits_a_matching_probe_effect() {
    let mut m = model();
    assert_eq!(m.engine.hl().probe_generation(), 0);
    let effects = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::DefaultColorsSet {
            fg: Some(0xFFFFFF),
            bg: Some(0),
            sp: None,
        }]),
    );
    assert_eq!(m.engine.hl().probe_generation(), 1);
    assert!(matches!(
        effects.as_slice(),
        [Effect::Rpc(RpcCall::GetDefaultHl { generation: 1 })]
    ));
}

#[test]
fn a_second_default_colors_set_bumps_generation_again() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::DefaultColorsSet {
            fg: None,
            bg: Some(0),
            sp: None,
        }]),
    );
    let effects = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::DefaultColorsSet {
            fg: None,
            bg: Some(0x282a36),
            sp: None,
        }]),
    );
    assert_eq!(m.engine.hl().probe_generation(), 2);
    assert!(matches!(
        effects.as_slice(),
        [Effect::Rpc(RpcCall::GetDefaultHl { generation: 2 })]
    ));
}

#[test]
fn hl_probe_reply_matching_the_current_generation_is_accepted_and_marks_dirty() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::DefaultColorsSet {
            fg: Some(0x111111),
            bg: Some(0),
            sp: None,
        }]),
    );
    m.dirty = false;
    let effects = update(
        &mut m,
        Msg::HlProbeReply {
            generation: 1,
            fg: Some(0x111111),
            bg: None,
        },
    );
    assert!(effects.is_empty());
    assert!(m.dirty, "an accepted probe reply must trigger a repaint");
    let confirmed = m.engine.hl().confirmed().expect("reply must be recorded");
    assert_eq!(confirmed.generation, 1);
    assert_eq!(confirmed.fg, Some(0x111111));
    assert_eq!(confirmed.bg, None);
}

/// The write-time half of the generation guard: a reply for a
/// superseded generation must never overwrite a newer (or absent)
/// confirmed value, even though nothing else has happened between the
/// two `DefaultColorsSet`s to make the stale reply's arrival implausible
/// on a real connection (out-of-order delivery is not assumed to be
/// impossible, only rare).
#[test]
fn hl_probe_reply_for_a_stale_generation_is_dropped_without_marking_dirty() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::DefaultColorsSet {
            fg: None,
            bg: Some(0),
            sp: None,
        }]),
    );
    // a second DefaultColorsSet bumps the generation to 2 before the
    // generation-1 probe's reply ever arrives
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::DefaultColorsSet {
            fg: None,
            bg: Some(0x282a36),
            sp: None,
        }]),
    );
    m.dirty = false;
    let effects = update(
        &mut m,
        Msg::HlProbeReply {
            generation: 1,
            fg: None,
            bg: None,
        },
    );
    assert!(effects.is_empty());
    assert!(
        !m.dirty,
        "a stale-generation reply must not trigger a repaint"
    );
    assert!(
        m.engine.hl().confirmed().is_none(),
        "a stale-generation reply must not be recorded"
    );
}

/// A colorscheme switch mid-session: `Normal` goes from confirmed
/// transparent to a wire-ambiguous zero that will turn out to be
/// genuinely black, once its own probe replies. Between the switch's
/// `DefaultColorsSet` and that reply, `Theme::from_hl` must keep
/// reading the *old* confirmed value (still transparent) rather than
/// painting the new, not-yet-disambiguated wire zero -- only once the
/// matching reply lands does the theme converge on black.
#[test]
fn colorscheme_switch_to_ambiguous_zero_holds_the_prior_confirmed_value_until_its_own_probe_replies(
) {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::DefaultColorsSet {
            fg: Some(0xF8F8F2),
            bg: Some(0),
            sp: None,
        }]),
    );
    let _ = update(
        &mut m,
        Msg::HlProbeReply {
            generation: 1,
            fg: Some(0xF8F8F2),
            bg: None,
        },
    );
    assert_eq!(crate::theme::Theme::from_hl(m.engine.hl()).bg, None);

    // the colorscheme switch: a second DefaultColorsSet, still an
    // ambiguous wire zero, bumps the generation and starts a fresh probe
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::DefaultColorsSet {
            fg: Some(0xFFFFFF),
            bg: Some(0),
            sp: None,
        }]),
    );
    assert_eq!(
        crate::theme::Theme::from_hl(m.engine.hl()).bg,
        None,
        "must keep the prior confirmed transparent value while the new probe is in flight"
    );

    let _ = update(
        &mut m,
        Msg::HlProbeReply {
            generation: 2,
            fg: Some(0xFFFFFF),
            bg: Some(0),
        },
    );
    assert_eq!(
        crate::theme::Theme::from_hl(m.engine.hl()).bg,
        Some(0),
        "must converge on the new theme's genuinely-black bg once its probe replies"
    );
}

#[test]
fn an_unregistered_invoke_says_so_rather_than_going_quiet() {
    // every registered picker verb (files/buffers/grep) now has a real
    // handler, so a feature+verb the registry has never heard of is the
    // only way left to exercise this "must not go quiet" contract
    // end-to-end; the sibling test below locks the wording for the
    // still-real "registered but unhandled" branch directly
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "wat".to_string(),
            verb: "huh".to_string(),
        },
    );
    assert!(
        matches!(effects.as_slice(), [Effect::ScheduleToastExpiry { .. }]),
        "an unrecognized invoke must not talk to the engine, only schedule \
             its own notice's expiry through the same choke point every other \
             locally-synthesized notice uses: {effects:?}"
    );
    let entry = m
        .engine
        .messages
        .entries
        .last()
        .expect("the invoke must reach the user through the message surface");
    let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
    assert!(
        text.contains("picker files"),
        "an unrecognized invoke must fall back to the usage line, not go quiet, \
             got {text:?}"
    );
    assert!(
        text.starts_with(":View "),
        "the usage line opens with the ex-command itself, never a `view: ` \
             prefix stuttering into `view: :View ...`, got {text:?}"
    );
    assert!(m.dirty, "the notice must be painted without another event");
    assert!(
        m.overlays().is_empty(),
        "no overlay exists to open yet: an invoke must not push one"
    );
}

#[test]
fn a_registered_verb_with_no_handler_yet_would_name_what_was_invoked() {
    // no registry entry is currently unhandled (picker answers all
    // three of its own), so this exercises `feature_invoke_notice`
    // directly rather than through a real, presently-unreachable
    // registry gap; it stands ready for the next feature that lands a
    // registry row before its own update() handler
    let text = feature_invoke_notice("tree", "open", true);
    assert_eq!(text, "view: no handler for tree open in this build");
}

#[test]
fn a_feature_invoke_while_a_blocked_prompt_is_topmost_does_not_steal_focus() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::MsgShow {
            kind: "confirm".into(),
            content: vec![(0, "test".into())],
            replace_last: false,
        }]),
    );
    let prompt_id = match m.focus() {
        Focus::Native(id) => id,
        Focus::Engine => unreachable!("MsgShow must open a Prompt overlay"),
    };
    assert!(matches!(
        m.overlays().last().map(|o| &o.kind),
        Some(OverlayKind::Prompt(_))
    ));

    let effects = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "files".to_string(),
        },
    );
    assert!(
        matches!(effects.as_slice(), [Effect::PickerQuery { .. }]),
        "opening beneath the prompt must still issue the picker's first \
             query: {effects:?}"
    );
    assert_eq!(
        m.focus(),
        Focus::Native(prompt_id),
        "a picker opening while a blocked prompt is topmost must not \
             steal its focus"
    );
    assert_eq!(
        m.overlays().len(),
        2,
        "the picker must open beneath the prompt, not replace it"
    );
    assert!(
        matches!(m.overlays()[0].kind, OverlayKind::Picker(_)),
        "the picker must sit beneath the prompt on the stack: {:?}",
        m.overlays()
    );
    assert!(
        matches!(m.overlays()[1].kind, OverlayKind::Prompt(_)),
        "the prompt must remain topmost: {:?}",
        m.overlays()
    );
    assert!(
        m.picker_mut().is_some(),
        "the picker must still be reachable so a streamed reply can \
             reach it even while the prompt holds focus"
    );
}

#[test]
fn a_prompt_opening_over_an_open_picker_takes_focus_and_returns_it_on_resolve() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "files".to_string(),
        },
    );
    let picker_id = match m.focus() {
        Focus::Native(id) => id,
        Focus::Engine => unreachable!("FeatureInvoke picker files must open a Picker overlay"),
    };

    // the same real msg_show + cmdline_show pairing
    // a_confirm_questions_toast_lives_exactly_as_long_as_the_prompt_does
    // captured against the pinned engine
    let _ = update(
        &mut m,
        Msg::Redraw(vec![
            UiEvent::MsgShow {
                kind: "confirm".into(),
                content: vec![(0, "Save changes?".into())],
                replace_last: false,
            },
            UiEvent::Flush,
        ]),
    );
    let _ = update(
        &mut m,
        Msg::Redraw(vec![
            UiEvent::CmdlineShow {
                content: vec![],
                pos: 0,
                firstc: String::new(),
                prompt: "[Y]es, (N)o: ".into(),
                indent: 0,
                level: 1,
            },
            UiEvent::Flush,
        ]),
    );

    let prompt_id = match m.focus() {
        Focus::Native(id) => id,
        Focus::Engine => unreachable!("MsgShow must open a Prompt overlay over the picker"),
    };
    assert_ne!(
        prompt_id, picker_id,
        "the prompt must be a distinct, new overlay"
    );
    assert_eq!(
        m.overlays().len(),
        2,
        "the prompt must stack over the picker, not replace it"
    );
    assert!(
        matches!(m.overlays()[0].kind, OverlayKind::Picker(_)),
        "the picker must remain underneath the prompt: {:?}",
        m.overlays()
    );

    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::CmdlineHide, UiEvent::Flush]),
    );
    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: "y".into(),
        }),
    );

    assert_eq!(
        m.focus(),
        Focus::Native(picker_id),
        "resolving the prompt must return focus to the picker underneath it"
    );
    assert_eq!(
        m.overlays().len(),
        1,
        "the resolved prompt must be gone from the stack"
    );
}

/// The real close path a user takes to abandon a picker: `<Esc>` routed
/// through `update()`, not a direct `model.pop_focused_overlay()` call, must
/// emit `Effect::PickerClose` so the matcher worker drops its live
/// `Session` and stops any `Files` scan still walking a huge tree (see
/// `Effect::PickerClose`'s own doc). Disabling the emission at the
/// `Msg::Key` `<Esc>`-on-Picker arm makes this fail by name.
#[test]
fn esc_on_an_open_picker_emits_picker_close() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "files".to_string(),
        },
    );
    assert!(
        matches!(
            m.overlays().last().map(|o| &o.kind),
            Some(OverlayKind::Picker(_))
        ),
        "FeatureInvoke picker/files must open a Picker overlay on top"
    );

    // cleared so the dirty assertion below can only be satisfied by the
    // close arm itself, not by the open that preceded it
    m.dirty = false;
    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: "<Esc>".into(),
        }),
    );

    assert!(m.overlays().is_empty(), "Esc must close the picker overlay");
    assert!(
        matches!(effects.as_slice(), [Effect::PickerClose]),
        "closing the picker via Esc must emit exactly Effect::PickerClose: \
             {effects:?}"
    );
    // the paint loop repaints only when dirty, and a pop produces no
    // engine redraw: without this the closed picker stays on screen
    // until some unrelated event repaints (observed as a bench desync
    // whose failure screen still showed the popped overlay)
    assert!(m.dirty, "closing the picker must mark the model dirty");
}

/// A candidate landing in `Msg::PickerResults` must itself trigger a
/// preview request for the now-selected row -- there is no separate
/// navigation message yet, so `PickerResults` is the only place a
/// selection is ever established. Disabling `picker_preview_request`'s
/// call in that arm makes this fail by name.
#[test]
fn picker_results_issues_a_preview_request_for_the_selected_candidate() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "files".to_string(),
        },
    );
    let generation = m.picker_mut().expect("picker must be open").generation();

    let effects = update(
        &mut m,
        Msg::PickerResults {
            generation,
            items: vec![crate::native::picker::PickerItem::new("a.rs")],
        },
    );

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::Rpc(RpcCall::PreviewBuffer { path, .. })] if path.ends_with("a.rs")
        ),
        "a fresh result set must issue exactly one PreviewBuffer request for \
             the selected candidate: {effects:?}"
    );
}

/// A `LiveGrep` query commonly streams several result batches while the
/// scan is still running, and a later batch reordering rows without
/// changing the *selected* candidate must not re-issue a preview
/// request for a path already current or in flight -- two successive
/// `PickerResults` batches selecting the same first row must together
/// issue exactly one `PreviewBuffer` request, not two. Reverting
/// `PickerState::refresh_preview`'s dedupe check makes this fail by
/// name (it would then see two).
#[test]
fn two_result_batches_with_the_same_selection_issue_one_preview_request() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "files".to_string(),
        },
    );
    let generation = m.picker_mut().expect("picker must be open").generation();

    let first = update(
        &mut m,
        Msg::PickerResults {
            generation,
            items: vec![
                crate::native::picker::PickerItem::new("a.rs"),
                crate::native::picker::PickerItem::new("b.rs"),
            ],
        },
    );
    assert!(
        matches!(
            first.as_slice(),
            [Effect::Rpc(RpcCall::PreviewBuffer { path, .. })] if path.ends_with("a.rs")
        ),
        "the first batch must issue the usual single preview request: {first:?}"
    );

    // a second, streamed batch: still selecting the first row (`a.rs`),
    // just a longer result list -- the selection itself never changed
    let second = update(
        &mut m,
        Msg::PickerResults {
            generation,
            items: vec![
                crate::native::picker::PickerItem::new("a.rs"),
                crate::native::picker::PickerItem::new("b.rs"),
                crate::native::picker::PickerItem::new("c.rs"),
            ],
        },
    );
    assert!(
        second.is_empty(),
        "a second batch that keeps the same selection must issue no new \
             preview request: {second:?}"
    );
}

/// The falsifiable check `PickerState::apply_preview`'s own doc names,
/// driven through `update()`: a reply tagged with a preview generation
/// this session has since superseded (a newer selection issued its own
/// preview request before the old one answered) must be dropped, not
/// merged -- a naive always-apply handler passes every other preview
/// test and only this one catches it.
#[test]
fn a_preview_reply_for_a_stale_generation_is_dropped() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "files".to_string(),
        },
    );
    let generation = m.picker_mut().expect("picker must be open").generation();
    let _ = update(
        &mut m,
        Msg::PickerResults {
            generation,
            items: vec![crate::native::picker::PickerItem::new("a.rs")],
        },
    );
    let stale_generation = m
        .picker_mut()
        .expect("picker must be open")
        .preview_generation();

    // a second result set whose first row now names a *different*
    // candidate (`b.rs`, not `a.rs`) -- the dedupe `refresh_preview`
    // applies (see its own doc) only skips a request for a path
    // already current, so a genuinely new selection still allocates a
    // fresh preview generation, superseding the one just captured
    let _ = update(
        &mut m,
        Msg::PickerResults {
            generation,
            items: vec![crate::native::picker::PickerItem::new("b.rs")],
        },
    );

    let _ = update(
        &mut m,
        Msg::PickerPreviewReply {
            generation: stale_generation,
            path: "/tmp/a.rs".to_string(),
            loaded: true,
            lines: vec!["stale content".to_string()],
        },
    );

    assert!(
        m.picker_mut()
            .expect("picker must still be open")
            .view()
            .preview
            .is_empty(),
        "a reply tagged with a superseded preview generation must never reach \
             the pane"
    );
}

/// `loaded: true` is applied straight to the preview pane, with no disk
/// fallback issued -- the RPC answer already carries nvim's own
/// authoritative content, modified-but-unsaved or not (see
/// `docs/picker-preview-wire-capture.md` case 3).
#[test]
fn a_loaded_preview_reply_applies_its_lines_and_issues_no_fallback() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "files".to_string(),
        },
    );
    let generation = m.picker_mut().expect("picker must be open").generation();
    let _ = update(
        &mut m,
        Msg::PickerResults {
            generation,
            items: vec![crate::native::picker::PickerItem::new("a.rs")],
        },
    );
    let preview_generation = m
        .picker_mut()
        .expect("picker must be open")
        .preview_generation();

    let effects = update(
        &mut m,
        Msg::PickerPreviewReply {
            generation: preview_generation,
            path: "/tmp/a.rs".to_string(),
            loaded: true,
            lines: vec![
                "modified line one".to_string(),
                "modified line two".to_string(),
            ],
        },
    );

    assert!(
        effects.is_empty(),
        "a loaded reply must not also issue a disk-fallback effect: {effects:?}"
    );
    assert_eq!(
        m.picker_mut()
            .expect("picker must still be open")
            .view()
            .preview,
        vec![
            "modified line one".to_string(),
            "modified line two".to_string()
        ]
    );
}

/// `loaded: false` means nvim has no buffer open for the candidate; the
/// only remaining source of truth is disk, handed off to
/// `Effect::PickerPreviewFallback` rather than treated as "nothing to
/// preview" (see `update`'s `Msg::PickerPreviewReply` arm doc).
#[test]
fn an_unloaded_preview_reply_issues_a_disk_fallback_effect() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "files".to_string(),
        },
    );
    let generation = m.picker_mut().expect("picker must be open").generation();
    let _ = update(
        &mut m,
        Msg::PickerResults {
            generation,
            items: vec![crate::native::picker::PickerItem::new("a.rs")],
        },
    );
    let preview_generation = m
        .picker_mut()
        .expect("picker must be open")
        .preview_generation();

    let effects = update(
        &mut m,
        Msg::PickerPreviewReply {
            generation: preview_generation,
            path: "/tmp/a.rs".to_string(),
            loaded: false,
            lines: Vec::new(),
        },
    );

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::PickerPreviewFallback { generation, path }]
                if *generation == preview_generation && path == "/tmp/a.rs"
        ),
        "loaded: false must hand off to the disk-fallback effect, echoing the \
             same generation and path: {effects:?}"
    );
}

/// `Msg::PickerPreviewFile` is the disk-fallback read's own answer,
/// gated on the same preview generation as an RPC reply -- a stale
/// fallback landing after a newer selection's own request is issued
/// must be dropped on the identical terms.
#[test]
fn a_picker_preview_file_reply_applies_disk_fallback_lines() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "files".to_string(),
        },
    );
    let generation = m.picker_mut().expect("picker must be open").generation();
    let _ = update(
        &mut m,
        Msg::PickerResults {
            generation,
            items: vec![crate::native::picker::PickerItem::new("a.rs")],
        },
    );
    let preview_generation = m
        .picker_mut()
        .expect("picker must be open")
        .preview_generation();

    let effects = update(
        &mut m,
        Msg::PickerPreviewFile {
            generation: preview_generation,
            lines: Some(vec!["disk line one".to_string()]),
        },
    );

    assert!(effects.is_empty());
    assert_eq!(
        m.picker_mut()
            .expect("picker must still be open")
            .view()
            .preview,
        vec!["disk line one".to_string()]
    );
}

#[test]
fn a_bare_view_command_is_answered_with_what_it_could_have_asked_for() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: String::new(),
            verb: String::new(),
        },
    );
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ScheduleToastExpiry { .. }, Effect::Rpc(RpcCall::Input { notation })]
                if notation == ":View "
        ),
        "a bare :View's usage notice must schedule its own expiry the same way \
             every other locally-synthesized notice does, and reopen the cmdline \
             pre-seeded with the command name so its own completion is one <Tab> \
             away inside the palette: {effects:?}"
    );
    let entry = m
        .engine
        .messages
        .entries
        .last()
        .expect("a bare :View must still reach the user");
    let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
    assert!(
        !text.contains("  "),
        "a bare invocation must not render its two empty tokens as a gap, got {text:?}"
    );
    for form in crate::native::mappings::invocations() {
        assert!(
            text.contains(&form),
            "the usage line must offer {form}, got {text:?}"
        );
    }
    assert!(m.dirty);
}

#[test]
fn a_bare_view_command_reopens_the_cmdline_seeded_with_the_command_name() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: String::new(),
            verb: String::new(),
        },
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Rpc(RpcCall::Input { notation }) if notation == ":View ")),
        "a bare :View must reopen the cmdline pre-seeded with its own command \
             name, so :View<Tab> completion is one keystroke away: {effects:?}"
    );
}

#[test]
fn an_unmatched_named_invocation_gets_only_the_notice_not_a_cmdline_reopen() {
    // only the truly bare (feature="", verb="") case is discoverability;
    // a typo'd or not-yet-handled named form (e.g. a stale default map)
    // must not replay itself back into the cmdline, since there is
    // nothing about it a reopened `:View ` prompt would fix
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "nonexistent".to_string(),
        },
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Rpc(RpcCall::Input { .. }))),
        "an unmatched named invocation must not reopen the cmdline: {effects:?}"
    );
}

#[test]
fn a_native_invoke_notice_is_wired_through_the_same_choke_point_as_a_wire_toast() {
    // a native notice must flow through the same classify/expire/history
    // path a wire toast does: it must schedule the identical
    // ScheduleToastExpiry effect and land in ToastHistory that a
    // wire-decoded UiEvent::MsgShow transient toast gets, or an idle
    // editor keeps the notice on screen forever and it's invisible to
    // a future :messages view
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: String::new(),
            verb: String::new(),
        },
    );
    let entry = m
        .engine
        .messages
        .entries
        .last()
        .expect("the invoke notice must reach the message surface");
    assert!(
        matches!(
            &effects[..],
            [Effect::ScheduleToastExpiry { id, after }, Effect::Rpc(RpcCall::Input { .. })]
                if *id == entry.id() && *after == crate::native::toast::TRANSIENT_TOAST_TIMEOUT
        ),
        "expected a ScheduleToastExpiry for {:?} after {:?}, followed by the bare \
             invocation's cmdline-reopen effect, got {effects:?}",
        entry.id(),
        crate::native::toast::TRANSIENT_TOAST_TIMEOUT
    );
    let recorded = m
        .engine
        .toast_history
        .entries()
        .next()
        .expect("the invoke notice must land in scrollback history, not just on screen");
    assert_eq!(
        recorded.id(),
        entry.id(),
        "history must record the same entry that's on screen, not a stale one"
    );
}

#[test]
fn a_verb_this_build_does_not_answer_is_told_the_ones_it_does() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "nonesuch".to_string(),
        },
    );
    let entry = m.engine.messages.entries.last().expect("a notice");
    let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
    assert!(
        text.contains("picker files"),
        "an unanswerable verb must be told the forms that work, got {text:?}"
    );
}

#[test]
fn claimed_keys_are_recorded_for_the_handover_report() {
    use crate::native::mappings::MappingClaim;
    let mut m = model();
    let claimed = vec![MappingClaim {
        feature: "picker".to_string(),
        lhs: "<leader>ff".to_string(),
        had_user_mapping: true,
    }];
    let effects = update(
        &mut m,
        Msg::MappingsClaimed {
            claimed: claimed.clone(),
        },
    );
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(
        m.claimed_keys(),
        claimed.as_slice(),
        "the report has no other source for what the keys took"
    );
}

#[test]
fn tree_toggle_opens_a_sidebar_and_issues_both_the_scan_and_the_git_scan() {
    let mut m = model();
    let effects = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    assert!(
        matches!(
            m.overlays().last().map(|o| &o.kind),
            Some(OverlayKind::Tree(_))
        ),
        "toggling tree must open a Tree overlay: {:?}",
        m.overlays()
    );
    let (mut saw_scan, mut saw_git_scan) = (false, false);
    for effect in &effects {
        match effect {
            Effect::TreeScan { .. } => saw_scan = true,
            Effect::TreeGitScan { .. } => saw_git_scan = true,
            _ => {}
        }
    }
    assert!(
        saw_scan,
        "opening a tree must issue its filesystem scan: {effects:?}"
    );
    assert!(
        saw_git_scan,
        "opening a tree must issue its git-status refresh too: {effects:?}"
    );
}

#[test]
fn tree_toggle_again_closes_the_sidebar_and_cancels_its_scan_worker() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let effects = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    assert!(
        m.overlays().is_empty(),
        "a second toggle must close the tree it just opened: {:?}",
        m.overlays()
    );
    // `Effect::TreeClose` flips whatever scan the executor still has
    // running (see `Executor::tree_scan_cancel`'s own doc): closing
    // issues exactly this one effect, not none, so a huge tree closed
    // mid-scan does not leave its worker thread walking unobserved.
    assert!(
        matches!(effects.as_slice(), [Effect::TreeClose]),
        "closing must cancel the scan worker via Effect::TreeClose: {effects:?}"
    );
}

#[test]
fn tree_toggle_while_a_blocked_prompt_is_topmost_opens_beneath_it_without_stealing_focus() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::Redraw(vec![UiEvent::MsgShow {
            kind: "confirm".into(),
            content: vec![(0, "test".into())],
            replace_last: false,
        }]),
    );
    let prompt_id = match m.focus() {
        Focus::Native(id) => id,
        Focus::Engine => unreachable!("MsgShow must open a Prompt overlay"),
    };
    assert!(matches!(
        m.overlays().last().map(|o| &o.kind),
        Some(OverlayKind::Prompt(_))
    ));

    let effects = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let (mut saw_scan, mut saw_git_scan) = (false, false);
    for effect in &effects {
        match effect {
            Effect::TreeScan { .. } => saw_scan = true,
            Effect::TreeGitScan { .. } => saw_git_scan = true,
            _ => {}
        }
    }
    assert!(
        saw_scan && saw_git_scan,
        "opening beneath the prompt must still issue both scans: {effects:?}"
    );
    assert_eq!(
        m.focus(),
        Focus::Native(prompt_id),
        "a tree opening while a blocked prompt is topmost must not \
             steal its focus"
    );
    assert_eq!(
        m.overlays().len(),
        2,
        "the tree must open beneath the prompt, not replace it"
    );
    assert!(
        matches!(m.overlays()[0].kind, OverlayKind::Tree(_)),
        "the tree must sit beneath the prompt on the stack: {:?}",
        m.overlays()
    );
    assert!(
        matches!(m.overlays()[1].kind, OverlayKind::Prompt(_)),
        "the prompt must remain topmost: {:?}",
        m.overlays()
    );
    assert!(
        m.tree_mut().is_some(),
        "the tree must still be reachable so a streamed scan reply can \
             reach it even while the prompt holds focus"
    );
}

#[test]
fn close_tree_releases_its_own_mouse_capture_but_not_a_different_overlays() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let tree_id = match m.overlays().last() {
        Some(overlay) if matches!(overlay.kind, OverlayKind::Tree(_)) => overlay.id,
        other => unreachable!("toggle must have opened a Tree overlay: {other:?}"),
    };
    m.capture_mouse(MouseCapture::Overlay(tree_id));
    assert_eq!(m.mouse_capture(), Some(MouseCapture::Overlay(tree_id)));

    assert!(m.close_tree(), "the open tree must be found and closed");
    assert_eq!(
        m.mouse_capture(),
        None,
        "closing the tree that owns the in-flight gesture must release \
             it, or a drag whose target just disappeared would keep \
             routing to an overlay id nothing on the stack holds anymore"
    );

    // a capture belonging to some other, unrelated overlay id must
    // survive a tree close that never held it
    let unrelated_id = OverlayId(9999);
    m.capture_mouse(MouseCapture::Overlay(unrelated_id));
    assert!(
        !m.close_tree(),
        "no tree is open at this point, so this must report nothing \
             was found to close"
    );
    assert_eq!(
        m.mouse_capture(),
        Some(MouseCapture::Overlay(unrelated_id)),
        "close_tree must never release a capture it does not own"
    );
}

/// Opens a tree on a fresh `Model` and returns the generation
/// `TreeCreatePromptReply` must echo back to be honored -- the shared
/// setup every `tree_create_prompt_reply_*` test below starts from.
fn model_with_open_tree() -> (Model, u64) {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let generation = m
        .tree_mut()
        .expect("toggle must have opened the tree")
        .generation();
    (m, generation)
}

/// The path-traversal fix's positive case: an ordinary single-component
/// name must still reach `Effect::TreeCreateFile`, unaffected by the
/// new validation -- without this, the rejection tests below would
/// prove nothing (a validator that refuses everything would also pass
/// them).
#[test]
fn tree_create_prompt_reply_with_a_plain_name_creates_beside_the_root() {
    let (mut m, generation) = model_with_open_tree();
    let effects = update(
        &mut m,
        Msg::TreeCreatePromptReply {
            generation,
            name: Some("new_file.txt".to_string()),
        },
    );
    assert!(
        matches!(
            &effects[..],
            [Effect::TreeCreateFile { path, .. }]
                if path.file_name().and_then(|n| n.to_str()) == Some("new_file.txt")
        ),
        "a plain leaf name must still produce exactly one TreeCreateFile: {effects:?}"
    );
}

/// An absolute answer must never reach `Effect::TreeCreateFile`:
/// `PathBuf::join` with an absolute argument discards the base
/// entirely, so an unvalidated `target_dir.join(name)` would write
/// wherever the typed text names, anywhere on the filesystem the
/// process can reach, not beside the tree's own root.
#[test]
fn tree_create_prompt_reply_rejects_an_absolute_path() {
    let (mut m, generation) = model_with_open_tree();
    let evil = if cfg!(windows) {
        "C:\\evil.txt"
    } else {
        "/evil.txt"
    };
    let effects = update(
        &mut m,
        Msg::TreeCreatePromptReply {
            generation,
            name: Some(evil.to_string()),
        },
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::TreeCreateFile { .. })),
        "an absolute path must never reach TreeCreateFile: {effects:?}"
    );
    let entry = m
        .engine
        .messages
        .entries
        .last()
        .expect("the refusal must surface a visible notice");
    assert!(
        entry.content.iter().any(|(_, t)| t.contains("invalid")),
        "expected an \"invalid file name\" notice, got {:?}",
        entry.content
    );
}

/// A `..`-laden answer must never reach `Effect::TreeCreateFile`
/// either: joined onto `target_dir`, it climbs out of the tree root
/// the same way an absolute path replaces it outright.
#[test]
fn tree_create_prompt_reply_rejects_a_parent_traversal() {
    let (mut m, generation) = model_with_open_tree();
    let effects = update(
        &mut m,
        Msg::TreeCreatePromptReply {
            generation,
            name: Some("../../etc/cron.d/evil".to_string()),
        },
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::TreeCreateFile { .. })),
        "a `..`-laden path must never reach TreeCreateFile: {effects:?}"
    );
}

/// Nested creation (a name embedding its own directory separator) was
/// never a feature this prompt supports -- its contract is one leaf
/// name beside the selection -- so a relative-but-multi-component
/// answer is refused on the same terms as an absolute or `..`-laden
/// one, not silently accepted as a new subdirectory structure.
#[test]
fn tree_create_prompt_reply_rejects_a_nested_relative_path() {
    let (mut m, generation) = model_with_open_tree();
    let effects = update(
        &mut m,
        Msg::TreeCreatePromptReply {
            generation,
            name: Some("sub/dir/file.txt".to_string()),
        },
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::TreeCreateFile { .. })),
        "a multi-component relative path must never reach TreeCreateFile: {effects:?}"
    );
}

/// The rename fix's positive case, mirroring
/// `tree_create_prompt_reply_with_a_plain_name_creates_beside_the_root`:
/// an ordinary single-component answer must still reach
/// `Effect::Rpc(RpcCall::RenameFile)`, unaffected by the new
/// validation.
#[test]
fn tree_rename_prompt_reply_with_a_plain_name_renames_beside_the_original() {
    let (mut m, generation) = model_with_open_tree();
    let effects = update(
        &mut m,
        Msg::TreeRenamePromptReply {
            generation,
            old_path: "/tree/root/old.txt".to_string(),
            name: Some("new.txt".to_string()),
        },
    );
    assert!(
        matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::RenameFile { old_path, new_path, .. })]
                if old_path == "/tree/root/old.txt" && new_path.ends_with("new.txt")
        ),
        "a plain leaf name must still produce exactly one RenameFile call: {effects:?}"
    );
}

/// An absolute answer must never reach `Effect::Rpc(RpcCall::RenameFile)`:
/// `PathBuf::join` with an absolute argument discards `parent` entirely,
/// so an unvalidated `parent.join(new_name)` would rename the file to
/// wherever the typed text names, anywhere on the filesystem the process
/// can reach, not beside the file's own original location.
#[test]
fn tree_rename_prompt_reply_rejects_an_absolute_path() {
    let (mut m, generation) = model_with_open_tree();
    let evil = if cfg!(windows) {
        "C:\\evil.txt"
    } else {
        "/evil.txt"
    };
    let effects = update(
        &mut m,
        Msg::TreeRenamePromptReply {
            generation,
            old_path: "/tree/root/old.txt".to_string(),
            name: Some(evil.to_string()),
        },
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Rpc(RpcCall::RenameFile { .. }))),
        "an absolute path must never reach RenameFile: {effects:?}"
    );
    let entry = m
        .engine
        .messages
        .entries
        .last()
        .expect("the refusal must surface a visible notice");
    assert!(
        entry.content.iter().any(|(_, t)| t.contains("invalid")),
        "expected an \"invalid file name\" notice, got {:?}",
        entry.content
    );
}

/// A `..`-laden answer must never reach `Effect::Rpc(RpcCall::RenameFile)`
/// either: joined onto `parent`, it climbs out of the tree root the same
/// way an absolute path replaces it outright.
#[test]
fn tree_rename_prompt_reply_rejects_a_parent_traversal() {
    let (mut m, generation) = model_with_open_tree();
    let effects = update(
        &mut m,
        Msg::TreeRenamePromptReply {
            generation,
            old_path: "/tree/root/old.txt".to_string(),
            name: Some("../../etc/cron.d/evil".to_string()),
        },
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Rpc(RpcCall::RenameFile { .. }))),
        "a `..`-laden path must never reach RenameFile: {effects:?}"
    );
}

/// Nested rename (a name embedding its own directory separator) was
/// never a feature this prompt supports -- its contract is one leaf
/// name beside the original -- so a relative-but-multi-component
/// answer is refused on the same terms as an absolute or `..`-laden
/// one, not silently accepted as a move into a new subdirectory.
#[test]
fn tree_rename_prompt_reply_rejects_a_nested_relative_path() {
    let (mut m, generation) = model_with_open_tree();
    let effects = update(
        &mut m,
        Msg::TreeRenamePromptReply {
            generation,
            old_path: "/tree/root/old.txt".to_string(),
            name: Some("sub/dir/new.txt".to_string()),
        },
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Rpc(RpcCall::RenameFile { .. }))),
        "a multi-component relative path must never reach RenameFile: {effects:?}"
    );
}

/// The falsifiable check for the bridge write/focus wiring this
/// module's `tree_git_refresh_effect` exists to satisfy: with a tree
/// open, a `BufferChanged` (a write callback) must reissue
/// `Effect::TreeGitScan` against a fresh generation, not merely the
/// once-at-open refresh `toggle_tree_sidebar` already issues.
///
/// The open-time refresh is answered first: `TreeState` coalesces a
/// request that arrives while one is already in flight rather than
/// spawning a second concurrent scan (see
/// `a_write_callback_while_a_refresh_is_in_flight_coalesces_into_it`
/// for that case), so this test's reissue proof needs the in-flight
/// slot free before the write callback fires.
#[test]
fn a_buffer_write_callback_with_a_tree_open_reissues_the_git_scan() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let opened_at = m
        .tree_mut()
        .expect("tree must be open after toggling it")
        .git_generation();
    let _ = update(
        &mut m,
        Msg::TreeGitResult {
            generation: opened_at,
            status: Vec::new(),
            timed_out: false,
        },
    );

    let effects = update(
        &mut m,
        Msg::BufferChanged {
            name: "a.txt".to_string(),
            modified: false,
        },
    );
    let generation = match effects.as_slice() {
            [Effect::TreeGitScan { generation, .. }] => *generation,
            other => panic!(
                "a buffer-write callback with a tree open must reissue exactly                  one TreeGitScan, got {other:?}"
            ),
        };
    assert_ne!(
            generation, opened_at,
            "the reissued refresh must carry a fresh generation, not the              one the tree opened with"
        );
    assert_eq!(
        m.tree_mut().expect("tree still open").git_generation(),
        generation,
        "TreeState's own git_generation must track the effect it just issued"
    );
}

/// Same proof as the write-callback test above, for the focus-side
/// trigger (`GitBranchChanged`, fired on `BufEnter`/`DirChanged`/
/// `FocusGained`), with the same open-time refresh answered first for
/// the same reason.
#[test]
fn a_focus_callback_with_a_tree_open_reissues_the_git_scan() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let opened_at = m
        .tree_mut()
        .expect("tree must be open after toggling it")
        .git_generation();
    let _ = update(
        &mut m,
        Msg::TreeGitResult {
            generation: opened_at,
            status: Vec::new(),
            timed_out: false,
        },
    );

    let effects = update(
        &mut m,
        Msg::GitBranchChanged {
            branch: "main".to_string(),
        },
    );
    assert!(
            matches!(effects.as_slice(), [Effect::TreeGitScan { .. }]),
            "a focus callback with a tree open must reissue exactly one              TreeGitScan: {effects:?}"
        );
}

/// The coalescing falsification test: a write callback that
/// arrives while the open-time refresh is still in flight must issue no
/// second `Effect::TreeGitScan` -- coalescing it instead of spawning a
/// concurrent `git status` process for a reply the tree could only ever
/// act on once anyway. Its reply then re-arms exactly one more scan,
/// under a fresh generation, proving the coalesced request was
/// deduplicated rather than dropped.
#[test]
fn a_write_callback_while_a_refresh_is_in_flight_coalesces_into_it() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let in_flight = m
        .tree_mut()
        .expect("tree must be open after toggling it")
        .git_generation();

    let coalesced = update(
        &mut m,
        Msg::BufferChanged {
            name: "a.txt".to_string(),
            modified: false,
        },
    );
    assert!(
        coalesced.is_empty(),
        "a write callback while a git refresh is already in flight must \
             coalesce into it, not issue a second concurrent scan: {coalesced:?}"
    );
    assert_eq!(
        m.tree_mut().expect("tree still open").git_generation(),
        in_flight,
        "coalescing must not allocate a new generation until the \
             in-flight one answers"
    );

    let reissued = update(
        &mut m,
        Msg::TreeGitResult {
            generation: in_flight,
            status: Vec::new(),
            timed_out: false,
        },
    );
    let reissued_generation = match reissued.as_slice() {
        [Effect::TreeGitScan { generation, .. }] => *generation,
        other => panic!(
            "the in-flight refresh's reply must re-arm the coalesced \
                 request as exactly one fresh TreeGitScan, got {other:?}"
        ),
    };
    assert_ne!(
        reissued_generation, in_flight,
        "the re-armed scan must carry a generation later than the one \
             it coalesced into"
    );
}

/// The falsifiable check for the IMPORTANT liveness fix: a `git status`
/// that hits its own bound and reports `timed_out: true` must not
/// permanently wedge `git_refresh_in_flight`. `apply_git` clears the
/// flag on every reply regardless of `timed_out`, so a later write
/// callback must still be able to issue a fresh `Effect::TreeGitScan` --
/// proving the pre-fix failure mode (one hung `git status` freezing the
/// sidebar's decorations for the rest of the session) cannot recur. The
/// timed-out reply itself must also surface a notice, so the user learns
/// the decorations they see may be stale instead of silently seeing
/// nothing update.
#[test]
fn a_timed_out_git_reply_does_not_permanently_suppress_future_refreshes() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let opened_at = m
        .tree_mut()
        .expect("tree must be open after toggling it")
        .git_generation();
    let notices_before = m.engine.messages.entries.len();

    let effects = update(
        &mut m,
        Msg::TreeGitResult {
            generation: opened_at,
            status: Vec::new(),
            timed_out: true,
        },
    );
    assert!(
        m.engine.messages.entries.len() > notices_before,
        "a timed-out git status must surface a notice, not silently \
             drop the failure"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleToastExpiry { .. })),
        "the notice must actually be recorded as a message, not merely \
             counted: {effects:?}"
    );

    // The real proof the in-flight flag was cleared: `apply_git` (which
    // the handler above called unconditionally) is the only place that
    // clears it, and `request_git_refresh` returns `None` -- coalescing
    // rather than issuing a scan -- for as long as it stays set. A write
    // callback that still gets a fresh `TreeGitScan` here is only
    // possible if the timed-out reply left the flag clear, exactly like
    // an ordinary one would have.
    let reissue = update(
        &mut m,
        Msg::BufferChanged {
            name: "a.txt".to_string(),
            modified: false,
        },
    );
    assert!(
        matches!(reissue.as_slice(), [Effect::TreeGitScan { .. }]),
        "a write callback after a timed-out git reply must still be \
             able to issue a fresh TreeGitScan, proving the hang did not \
             permanently suppress future refreshes: {reissue:?}"
    );
}

/// The other half of the falsifiable check: with no tree open at all,
/// the same callbacks that must trigger a refresh while one is open
/// must issue no tree effect whatsoever -- every buffer write and
/// focus change in an ordinary editing session hits these arms, so a
/// no-tree no-op is the common case, not a corner one.
#[test]
fn bridge_callbacks_with_no_tree_open_issue_no_tree_effect() {
    let mut m = model();
    let write_effects = update(
        &mut m,
        Msg::BufferChanged {
            name: "a.txt".to_string(),
            modified: true,
        },
    );
    assert!(
            !write_effects
                .iter()
                .any(|e| matches!(e, Effect::TreeGitScan { .. })),
            "no tree is open, so a write callback must issue no tree              effect: {write_effects:?}"
        );
    let focus_effects = update(
        &mut m,
        Msg::GitBranchChanged {
            branch: "main".to_string(),
        },
    );
    assert!(
            !focus_effects
                .iter()
                .any(|e| matches!(e, Effect::TreeGitScan { .. })),
            "no tree is open, so a focus callback must issue no tree              effect: {focus_effects:?}"
        );
}

#[test]
fn notifications_history_invoke_opens_a_message_history_overlay_and_marks_dirty() {
    let mut m = model();
    m.dirty = false;
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        },
    );
    assert!(
        matches!(
            m.overlays().last().map(|o| &o.kind),
            Some(OverlayKind::MessageHistory(_))
        ),
        "invoking notifications/history must open a MessageHistory overlay: {:?}",
        m.overlays()
    );
    assert!(
        m.dirty,
        "opening the message history must mark the model dirty for a repaint"
    );
}

/// The generic `Focus::Native` `<Esc>` fallback pops whatever overlay is
/// on top without a per-kind arm of its own -- MessageHistory is the
/// first real inhabitant of that path. A pop that does not mark
/// `dirty` leaves the paint loop's `if model.dirty` gate (see
/// `view`'s runtime loop) never repainting the frame the overlay just
/// vacated, so the stale frame stays on screen until some unrelated
/// event happens to set `dirty` again.
#[test]
fn esc_through_the_generic_native_fallback_closes_message_history_and_marks_dirty() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "notifications".to_string(),
            verb: "history".to_string(),
        },
    );
    assert!(matches!(
        m.overlays().last().map(|o| &o.kind),
        Some(OverlayKind::MessageHistory(_))
    ));
    m.dirty = false;

    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: "<Esc>".into(),
        }),
    );
    assert!(
        m.overlays().is_empty(),
        "<Esc> through the generic fallback must close the MessageHistory overlay: {:?}",
        m.overlays()
    );
    assert!(
        m.dirty,
        "closing the overlay must mark the model dirty for a repaint"
    );
}
/// The escalation ladder for a wedge that may still resolve itself: the
/// sticky banner on the observation that first sees it, the modal only
/// once the wedge has outlasted `ENGINE_BUSY_MODAL_THRESHOLD`.
#[test]
fn a_wedge_raises_the_banner_first_and_the_modal_only_past_the_threshold() {
    let mut m = model();

    let effects = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: Duration::ZERO,
        },
    );
    assert!(effects.is_empty());
    assert_eq!(
        visible_texts(&m),
        vec![WedgeKind::ReadSide.notice().to_string()],
        "the banner must raise on the observation that first saw the wedge"
    );
    assert!(
        m.overlays().is_empty(),
        "no modal may open before the escalation threshold: {:?}",
        m.overlays()
    );

    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD - Duration::from_millis(1),
        },
    );
    assert!(
        m.overlays().is_empty(),
        "a wedge one millisecond short of the threshold opened the modal"
    );

    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    assert!(
        matches!(
            m.overlays().last().map(|o| &o.kind),
            Some(OverlayKind::EngineBusy(_))
        ),
        "the wedge outlasted the threshold and no modal opened: {:?}",
        m.overlays()
    );
    assert_eq!(
        visible_texts(&m),
        vec![WedgeKind::ReadSide.notice().to_string()],
        "the banner stays up underneath the modal"
    );
}

/// A model whose user turned automatic recovery off, so a dead connection
/// is surfaced and waited on rather than answered with a respawn -- the
/// only shape in which the dead-engine modal is ever seen.
fn attended_model() -> Model {
    let mut m = model();
    m.supervision.auto_restart = false;
    m
}

/// A closed connection spends no patience: the whole reason the
/// threshold exists is to let a possibly self-resolving condition
/// resolve, and this one never will.
#[test]
fn a_dead_connection_raises_banner_and_modal_on_the_same_observation() {
    let mut m = attended_model();

    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::Dead),
            observed_for: Duration::ZERO,
        },
    );

    assert_eq!(
        visible_texts(&m),
        vec![WedgeKind::Dead.notice().to_string()],
        "a dead connection must raise the banner at once"
    );
    let Some(OverlayKind::EngineBusy(state)) = m.overlays().last().map(|o| &o.kind) else {
        panic!(
            "a dead connection must escalate with no grace period: {:?}",
            m.overlays()
        );
    };
    assert!(
        !state.offers(SupervisionChoice::Interrupt),
        "no input path survives a closed connection, so Interrupt must not be offered"
    );
    assert_eq!(
        state.choices(),
        vec![SupervisionChoice::Restart, SupervisionChoice::Quit]
    );
}

/// The switch a user sets, doing the thing it names: a connection observed
/// dead is replaced without asking, and the modal that would have asked is
/// never raised.
#[test]
fn a_dead_connection_is_recovered_without_asking_unless_the_user_said_otherwise() {
    let mut unattended = model();
    let effects = update(
        &mut unattended,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::Dead),
            observed_for: Duration::ZERO,
        },
    );
    assert!(
        matches!(&effects[..], [Effect::RestartEngine]),
        "automatic recovery must respawn on the reading that saw the death: {effects:?}"
    );
    assert!(
        unattended.overlays().is_empty(),
        "nothing is being asked, so nothing may be on screen asking it: {:?}",
        unattended.overlays()
    );

    let mut attended = attended_model();
    let effects = update(
        &mut attended,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::Dead),
            observed_for: Duration::ZERO,
        },
    );
    assert!(
        effects.is_empty(),
        "a user who turned automatic recovery off must be asked first: {effects:?}"
    );
    assert!(attended.engine_busy().is_some(), "and asked on screen");
}

/// A reconnect the runtime already has scheduled owns the banner while it
/// runs: the reading that keeps arriving every couple of seconds must not
/// ask for a second replacement on top of the one already waiting out its
/// backoff, or one outage would spend the session's whole unattended budget
/// at the cadence the readout repaints on.
#[test]
fn a_scheduled_reconnect_counts_on_the_banner_and_asks_for_no_second_attempt() {
    let mut m = model();
    for attempt in 1..=5 {
        assert!(m
            .supervision
            .note_reconnect(Some(ReconnectProgress::new(attempt, 5))));
        let effects = update(
            &mut m,
            Msg::EngineLiveness {
                wedge: Some(WedgeKind::Dead),
                observed_for: Duration::ZERO,
            },
        );
        assert!(
            effects.is_empty(),
            "attempt {attempt} is already owed, so the fold must ask for no other: {effects:?}"
        );
        assert_eq!(
            visible_texts(&m),
            vec![format!("connection lost -- reconnecting ({attempt}/5)")],
            "the banner must name the attempt the reconnect is on"
        );
        assert!(
            m.overlays().is_empty(),
            "a recovery that is still running is not a question: {:?}",
            m.overlays()
        );
    }
}

/// And once the attempts run out, the failure goes back to the user through
/// supervision's own dead-engine annunciator -- the same banner and the same
/// modal an unrecoverable engine has always raised, with no state of the
/// reconnect's own left on screen.
#[test]
fn a_spent_reconnect_lands_on_the_dead_engine_banner_and_modal() {
    let mut m = model();
    assert!(m
        .supervision
        .note_reconnect(Some(ReconnectProgress::new(6, 5))));

    let effects = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::Dead),
            observed_for: Duration::ZERO,
        },
    );

    assert!(
        effects.is_empty(),
        "a sequence with evidence its recovery is not working must not start another: {effects:?}"
    );
    assert_eq!(
        visible_texts(&m),
        vec![WedgeKind::Dead.notice().to_string()],
        "the banner must stop counting attempts nobody is going to make"
    );
    let busy = m
        .engine_busy()
        .expect("a spent reconnect must ask the user");
    assert_eq!(busy.kind, WedgeKind::Dead);
    assert_eq!(
        busy.choices(),
        vec![SupervisionChoice::Restart, SupervisionChoice::Quit]
    );
}

/// A session whose engine dies every time it starts is not one automatic
/// recovery can answer, so the automatic half stops and the modal takes
/// over.
#[test]
fn unattended_recovery_gives_up_after_the_bound_and_asks_instead() {
    let mut m = model();
    for attempt in 1..=AUTOMATIC_RECOVERY_ATTEMPTS {
        let effects = update(
            &mut m,
            Msg::EngineLiveness {
                wedge: Some(WedgeKind::Dead),
                observed_for: Duration::ZERO,
            },
        );
        assert!(
            matches!(&effects[..], [Effect::RestartEngine]),
            "recovery {attempt} was refused: {effects:?}"
        );
        // the fresh engine answers, which is what ends the episode
        let _ = update(
            &mut m,
            Msg::EngineLiveness {
                wedge: None,
                observed_for: Duration::ZERO,
            },
        );
    }

    let effects = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::Dead),
            observed_for: Duration::ZERO,
        },
    );
    assert!(
        effects.is_empty(),
        "a session that has spent its silent recoveries must stop taking \
         them: {effects:?}"
    );
    assert!(
        m.engine_busy().is_some(),
        "and must ask instead of failing quietly"
    );
}

/// The one way out of a dead engine that is not a restart. Nothing reaches
/// nvim any more, including the keys that would quit it, so this is view's
/// own exit and it carries nvim's own status.
#[test]
fn quitting_a_dead_engine_leaves_with_the_status_nvim_reported() {
    let mut m = attended_model();
    let _ = m.supervision.note_engine_stop(
        ExitInfo {
            code: Some(137),
            by_signal: true,
        },
        false,
    );
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::Dead),
            observed_for: Duration::ZERO,
        },
    );

    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: QUIT_NOTATION.to_string(),
        }),
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Quit { exit_code: 137 })),
        "quitting a dead engine must carry its own exit status: {effects:?}"
    );
    assert!(!m.running, "the session must be marked over");
}

#[test]
fn a_healthy_reading_retracts_the_banner_and_closes_the_modal() {
    let mut m = attended_model();
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::Dead),
            observed_for: Duration::ZERO,
        },
    );
    assert!(!m.overlays().is_empty());

    m.dirty = false;
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: None,
            observed_for: Duration::ZERO,
        },
    );
    assert!(
        visible_texts(&m).is_empty(),
        "the banner outlived its condition"
    );
    assert!(m.overlays().is_empty(), "the modal outlived its condition");
    assert!(m.dirty, "retracting both must mark the frame for repaint");
}

/// Re-asserting an unchanged reading must be free: this message arrives
/// on every loop pass for as long as the wedge lasts, and a repaint per
/// pass is exactly what the banner's own idempotence exists to prevent.
#[test]
fn an_unchanged_wedge_reading_marks_nothing_dirty() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: Duration::from_secs(3),
        },
    );
    m.dirty = false;
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: Duration::from_millis(3_400),
        },
    );
    assert!(
        !m.dirty,
        "a reading that renders identically repainted the frame"
    );
}

#[test]
fn the_modals_readout_follows_the_wedge_it_is_showing() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    m.dirty = false;
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD + Duration::from_secs(5),
        },
    );
    let readout = m.engine_busy().map(|s| s.since.readout());
    assert_eq!(
        readout,
        Some(ENGINE_BUSY_MODAL_THRESHOLD.as_secs() + 5),
        "the modal must show the wedge's current age, not its age at open time"
    );
    assert!(m.dirty, "a changed readout must repaint");
}

/// A wedge that turns into a closed connection is a different failure
/// with a different recovery, so the modal it is offered through is
/// rebuilt rather than left showing the old choices.
#[test]
fn a_wedge_that_becomes_dead_replaces_the_modal_it_had_opened() {
    let mut m = attended_model();
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::Dead),
            observed_for: Duration::ZERO,
        },
    );
    assert_eq!(m.overlays().len(), 1, "{:?}", m.overlays());
    assert_eq!(m.engine_busy().map(|s| s.kind), Some(WedgeKind::Dead));
    assert_eq!(
        visible_texts(&m),
        vec![WedgeKind::Dead.notice().to_string()]
    );
}

#[test]
fn interrupt_sends_the_live_verified_notation_and_leaves_the_modal_open() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );

    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: SupervisionChoice::Interrupt.key().to_string(),
        }),
    );
    assert!(
        matches!(
            &effects[..],
            [Effect::Rpc(RpcCall::Input { notation })] if notation == INTERRUPT_NOTATION
        ),
        "Interrupt must send the interrupt notation, got {effects:?}"
    );
    assert!(
        !m.overlays().is_empty(),
        "an interrupt that may not land must leave the modal up to try again"
    );
}

/// A synchronous Lua wedge swallows `<C-c>` without a trace: the engine
/// never sees it, nothing changes on screen, and a user pressing it again
/// has no way to tell a key that did nothing from one that never arrived.
/// The modal holds the two facts that settle it -- an interrupt was routed
/// from here, and the wedge it was aimed at is still being reported -- so it
/// says so.
#[test]
fn an_interrupt_the_engine_never_reacted_to_is_reported_on_the_modal() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: INTERRUPT_NOTATION.to_string(),
        }),
    );
    assert!(
        !m.engine_busy()
            .expect("the modal stays up over an interrupt")
            .message()
            .contains("interrupt sent"),
        "an interrupt whose answer could still be in flight must not be \
         reported as unanswered"
    );

    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD + INTERRUPT_REACTION_WINDOW,
        },
    );
    let message = m
        .engine_busy()
        .expect("the wedge outlived the interrupt, so the modal is still up")
        .message();
    assert!(
        message.ends_with("interrupt sent 5s ago, nothing has answered since"),
        "the modal says nothing about an interrupt that changed nothing: {message}"
    );

    // and a session where nobody pressed it says nothing about one
    let mut untouched = model();
    let _ = update(
        &mut untouched,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD + INTERRUPT_REACTION_WINDOW,
        },
    );
    assert!(
        !untouched
            .engine_busy()
            .expect("modal")
            .message()
            .contains("interrupt"),
        "a modal nobody interrupted must not claim one was sent"
    );
}

#[test]
fn a_dead_connections_modal_does_not_answer_the_interrupt_key() {
    let mut m = attended_model();
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::Dead),
            observed_for: Duration::ZERO,
        },
    );

    let notation = SupervisionChoice::Interrupt.key().to_string();
    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: notation.clone(),
        }),
    );
    // not consumed and not acted on: a key this wedge offers no choice for
    // is an ordinary keystroke, and it goes where an ordinary keystroke goes
    assert!(
        matches!(&effects[..], [Effect::Rpc(RpcCall::Input { notation: sent })] if *sent == notation),
        "a disabled choice must fall through to the engine, got {effects:?}"
    );
    assert!(
        !m.overlays().is_empty(),
        "an unanswered key must not close the modal"
    );
}

/// The key on the modal reaches the respawn, from every wedge that paints
/// it -- and no other key does, so a restart is never something a user
/// arrives at by typing.
#[test]
fn the_restart_key_reaches_the_respawn_and_no_other_key_does() {
    for kind in [WedgeKind::ReadSide, WedgeKind::WriteSide, WedgeKind::Dead] {
        let mut m = attended_model();
        let _ = update(
            &mut m,
            Msg::EngineLiveness {
                wedge: Some(kind),
                observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
            },
        );
        assert!(!m.overlays().is_empty(), "{kind:?} opened no modal");
        for notation in ["R", "r", "x", "<CR>", SupervisionChoice::Interrupt.key()] {
            let effects = update(
                &mut m,
                Msg::Key(Key {
                    notation: notation.to_string(),
                }),
            );
            assert!(
                !effects.iter().any(|e| matches!(e, Effect::RestartEngine)),
                "{kind:?} reached a respawn through {notation:?}: {effects:?}"
            );
            assert!(
                m.engine_busy().is_some(),
                "{kind:?} lost its modal to {notation:?}"
            );
        }

        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: RESTART_NOTATION.to_string(),
            }),
        );
        assert!(
            effects.iter().any(|e| matches!(e, Effect::RestartEngine)),
            "{kind:?} does not restart on the key it paints for it: {effects:?}"
        );
        assert!(
            m.engine_busy().is_none(),
            "{kind:?} left the modal up over the restart it asked for"
        );
    }
}

#[test]
fn dismiss_closes_the_modal_keeps_the_banner_and_does_not_reopen() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );

    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: SupervisionChoice::Dismiss.key().to_string(),
        }),
    );
    // dismiss-and-forward: closing an annunciator the user never asked for
    // may not cost them the keystroke it took to close it
    assert!(
        matches!(&effects[..], [Effect::Rpc(RpcCall::Input { notation })] if notation == "<Esc>"),
        "Dismiss must still deliver the <Esc> to the engine: {effects:?}"
    );
    assert!(m.overlays().is_empty(), "Dismiss must close the modal");
    assert_eq!(
        visible_texts(&m),
        vec![WedgeKind::ReadSide.notice().to_string()],
        "the underlying condition has not changed, so the banner stays"
    );

    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD * 2,
        },
    );
    assert!(
        m.overlays().is_empty(),
        "the dismissed modal reopened on the next observation: {:?}",
        m.overlays()
    );
}

/// A recovered engine that wedges again is a new episode, and a modal
/// dismissed during the previous one must not silence it.
#[test]
fn a_new_wedge_episode_offers_the_modal_again() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: SupervisionChoice::Dismiss.key().to_string(),
        }),
    );
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: None,
            observed_for: Duration::ZERO,
        },
    );
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    assert!(
        matches!(
            m.overlays().last().map(|o| &o.kind),
            Some(OverlayKind::EngineBusy(_))
        ),
        "a fresh wedge must be offered its own modal: {:?}",
        m.overlays()
    );
}

/// The modal answers its own choices and takes nothing else, so a session
/// that keeps typing at a slow operation still has its keystrokes applied
/// when that operation finishes.
#[test]
fn the_modal_answers_its_choices_and_passes_every_other_key_through() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    assert!(!m.overlays().is_empty(), "the modal must be open");
    assert_eq!(
        m.focus(),
        Focus::Engine,
        "the modal must not take the keyboard from the engine it is describing"
    );

    // "i" above all: it is the key a user waiting out a slow operation
    // reaches for by reflex, and the modal binding it would answer that
    // reflex by aborting the operation they were waiting for
    for notation in ["i", "a", "x", "r"] {
        let effects = update(
            &mut m,
            Msg::Key(Key {
                notation: notation.into(),
            }),
        );
        assert!(
            matches!(&effects[..], [Effect::Rpc(RpcCall::Input { notation: sent })] if sent == notation),
            "a key the modal does not answer must reach the engine: {notation:?} gave {effects:?}"
        );
        assert!(
            !m.overlays().is_empty(),
            "and {notation:?} must not close the modal on the way past"
        );
    }

    // the one key it does answer, from the same stack position
    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: SupervisionChoice::Dismiss.key().to_string(),
        }),
    );
    assert!(
        matches!(&effects[..], [Effect::Rpc(RpcCall::Input { notation })] if notation == "<Esc>"),
        "answering the modal must cost no keystroke: {effects:?}"
    );
    assert!(m.overlays().is_empty(), "Dismiss must close the modal");
}

/// The exemption to the rule the test below pins, and what makes it safe to
/// grant: a connection that is gone offers only keys nothing else answers.
///
/// A `Dead` modal stacked over a picker paints `[<F5>] Restart` and
/// `[<C-q>] Quit view`. If the focus rule silenced those the way it silences
/// `<Esc>`, the modal would be naming keys that do nothing -- and unlike
/// every other wedge, this one cannot be dismissed and the engine under it
/// cannot be reached, so the two keys it paints are the only way out of the
/// session at all.
#[test]
fn a_focused_overlays_keys_never_collide_with_a_dead_engines_choices() {
    /// Every notation an overlay holding the keyboard acts on: the generic
    /// `<Esc>` pop, the tree's own navigation and open, and the query
    /// editing a picker applies to anything printable plus `<BS>`.
    const FOCUSED_OVERLAY_KEYS: [&str; 5] = ["<Esc>", "<CR>", "<Up>", "<Down>", "<BS>"];

    for choice in WedgeKind::Dead.choices() {
        let key = choice.key();
        assert!(
            !FOCUSED_OVERLAY_KEYS.contains(&key),
            "a dead engine offers {choice:?} on {key:?}, which an overlay \
             holding the keyboard answers itself: answering it here as well \
             would act on both"
        );
        assert!(
            key.len() > 1,
            "a dead engine offers {choice:?} on the bare key {key:?}, which \
             a picker would fold into its query"
        );
    }
}

/// The exemption, driven: a tree open when the connection dies keeps its own
/// keys, and the modal above it still answers the two that are the only way
/// out.
#[test]
fn a_dead_engines_choices_are_answered_over_an_overlay_that_holds_the_keyboard() {
    let mut m = attended_model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let tree_id = match m.overlays().last() {
        Some(overlay) if matches!(overlay.kind, OverlayKind::Tree(_)) => overlay.id,
        other => unreachable!("toggle must have opened a Tree overlay: {other:?}"),
    };
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::Dead),
            observed_for: Duration::ZERO,
        },
    );
    assert_eq!(
        m.focus(),
        Focus::Native(tree_id),
        "the annunciator took focus from the overlay the user opened"
    );

    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: RESTART_NOTATION.to_string(),
        }),
    );
    assert!(
        effects.iter().any(|e| matches!(e, Effect::RestartEngine)),
        "the only way out of a dead session was painted and not answered: \
         {effects:?}"
    );
    assert_eq!(
        m.focus(),
        Focus::Native(tree_id),
        "answering the annunciator closed the overlay underneath it"
    );
}

/// The modal opening over an overlay that does hold the keyboard must not
/// take it: a picker or tree open when a wedge is noticed keeps answering
/// its own keys, exactly as it would have with no modal on screen.
#[test]
fn a_modal_over_a_focused_overlay_leaves_that_overlays_keys_alone() {
    let mut m = model();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    let tree_id = match m.overlays().last() {
        Some(overlay) if matches!(overlay.kind, OverlayKind::Tree(_)) => overlay.id,
        other => unreachable!("toggle must have opened a Tree overlay: {other:?}"),
    };
    let _ = update(
        &mut m,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    assert_eq!(m.overlays().len(), 2, "{:?}", m.overlays());
    assert_eq!(
        m.focus(),
        Focus::Native(tree_id),
        "the annunciator took focus from the overlay the user opened"
    );

    let effects = update(
        &mut m,
        Msg::Key(Key {
            notation: "<Esc>".into(),
        }),
    );
    // the same <Esc> does to the tree exactly what it would have done with
    // no annunciator ever raised, which is the whole invariant: the modal
    // changes what is on screen, never what a key means. And it does not
    // *also* count as an answer to the annunciator: one keypress dismissing
    // two things is one of them dismissed by accident, and this modal is
    // offered once per episode -- an accidental dismissal is the whole
    // offer, spent on something the user never chose
    assert!(
        matches!(&effects[..], [Effect::TreeClose]),
        "the tree must still answer its own key: {effects:?}"
    );
    assert!(
        m.engine_busy().is_some(),
        "the <Esc> that closed the tree also closed the annunciator over it: {:?}",
        m.overlays()
    );
    assert_eq!(m.focus(), Focus::Engine);

    // and with nothing else claiming it, the very next <Esc> answers the
    // annunciator exactly as it always did
    let _ = update(
        &mut m,
        Msg::Key(Key {
            notation: "<Esc>".into(),
        }),
    );
    assert!(m.engine_busy().is_none(), "{:?}", m.overlays());
}

/// The interrupt is picked by the very key it sends, so an open modal
/// changes nothing about what reaches nvim -- only whether the episode is
/// still being announced.
#[test]
fn the_interrupt_key_puts_the_same_bytes_on_the_wire_modal_or_not() {
    let mut wedged = model();
    let _ = update(
        &mut wedged,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    assert!(wedged.engine_busy().is_some(), "the modal must be open");
    let with_modal = update(
        &mut wedged,
        Msg::Key(Key {
            notation: INTERRUPT_NOTATION.to_string(),
        }),
    );

    let mut healthy = model();
    let without_modal = update(
        &mut healthy,
        Msg::Key(Key {
            notation: INTERRUPT_NOTATION.to_string(),
        }),
    );

    assert!(
        matches!(&without_modal[..], [Effect::Rpc(RpcCall::Input { notation })] if notation == INTERRUPT_NOTATION),
        "the no-modal path must forward the interrupt unchanged: {without_modal:?}"
    );
    assert_eq!(
        format!("{with_modal:?}"),
        format!("{without_modal:?}"),
        "picking the choice must produce the identical wire input"
    );
    assert!(
        wedged.engine_busy().is_some(),
        "an interrupt that may not land must leave the modal up to try again"
    );
}

/// Every key runs the keypress bookkeeping the rest of `update` owes it,
/// including the keys the modal answers: an annunciator on screen may not
/// quietly change what a keypress means anywhere else in the model.
#[test]
fn a_key_the_modal_answers_still_ages_out_a_read_toast() {
    for notation in [
        SupervisionChoice::Interrupt.key(),
        SupervisionChoice::Dismiss.key(),
    ] {
        let mut m = model();
        let _ = update(
            &mut m,
            Msg::EngineLiveness {
                wedge: Some(WedgeKind::ReadSide),
                observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
            },
        );
        m.engine
            .messages
            .push("echomsg".to_string(), vec![(0, "written".into())], false);
        m.engine.messages.note_flush();
        assert!(
            visible_texts(&m).iter().any(|line| line == "written"),
            "{:?}",
            visible_texts(&m)
        );

        let _ = update(
            &mut m,
            Msg::Key(Key {
                notation: notation.to_string(),
            }),
        );
        assert!(
            !visible_texts(&m).iter().any(|line| line == "written"),
            "{notation:?} skipped the transient dismissal: {:?}",
            visible_texts(&m)
        );
    }
}
