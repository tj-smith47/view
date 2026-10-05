#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::PICKER_KEYS;
use crate::events::UiEvent;
use crate::model::{Model, OverlayKind};
use crate::msg::{Effect, Key, Msg, OpenIn, RpcCall};
use crate::native::picker::{Picked, PickerItem};
use crate::update::update;

fn press(m: &mut Model, notation: &str) -> Vec<Effect> {
    update(
        m,
        Msg::Key(Key {
            notation: notation.to_string(),
        }),
    )
}

/// A model with the picker open on `verb`'s source, holding `items`.
fn picker_with(verb: &str, items: Vec<PickerItem>) -> Model {
    let mut m = Model::new();
    let _ = update(
        &mut m,
        Msg::FeatureInvoke {
            generation: None,
            feature: "picker".to_string(),
            verb: verb.to_string(),
        },
    );
    let generation = m.picker_mut().expect("the picker opens").generation();
    let _ = update(&mut m, Msg::PickerResults { generation, items });
    m.dirty = false;
    m
}

fn files() -> Model {
    picker_with(
        "files",
        vec![
            PickerItem::new("a.rs"),
            PickerItem::new("b.rs"),
            PickerItem::new("c.rs"),
        ],
    )
}

fn selected(m: &Model) -> Option<usize> {
    m.overlays().iter().find_map(|overlay| match &overlay.kind {
        OverlayKind::Picker(p) => p.view().selected,
        _ => None,
    })
}

fn picker_open(m: &Model) -> bool {
    m.overlays()
        .iter()
        .any(|overlay| matches!(overlay.kind, OverlayKind::Picker(_)))
}

/// The target and window of the one `OpenPicked` in `effects`, which has to
/// be followed by the `PickerClose` `<Esc>` sends.
fn opened(effects: &[Effect]) -> (Picked, OpenIn) {
    match effects {
        [Effect::Rpc(RpcCall::OpenPicked { target, how }), Effect::PickerClose] => {
            (target.clone(), *how)
        }
        other => panic!("expected an open and a close, got {other:?}"),
    }
}

fn preview_paths(effects: &[Effect]) -> Vec<String> {
    effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Rpc(RpcCall::PreviewBufferWindow { path, .. }) => Some(path.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn every_down_key_moves_the_selection_down_and_previews_the_new_row() {
    for key in ["<Down>", "<C-n>", "<C-j>"] {
        let mut m = files();
        let effects = press(&mut m, key);
        assert_eq!(selected(&m), Some(1), "{key}");
        assert!(m.dirty, "{key} repaints");
        let paths = preview_paths(&effects);
        assert!(
            matches!(paths.as_slice(), [path] if path.ends_with("b.rs")),
            "{key}: {effects:?}"
        );
    }
}

#[test]
fn every_up_key_moves_the_selection_up_and_previews_the_new_row() {
    for key in ["<Up>", "<C-p>", "<C-k>"] {
        let mut m = files();
        let _ = press(&mut m, "<Down>");
        let _ = press(&mut m, "<Down>");
        let effects = press(&mut m, key);
        assert_eq!(selected(&m), Some(1), "{key}");
        let paths = preview_paths(&effects);
        assert!(
            matches!(paths.as_slice(), [path] if path.ends_with("b.rs")),
            "{key}: {effects:?}"
        );
    }
}

#[test]
fn the_selection_stops_at_either_end_and_asks_for_no_preview_there() {
    let mut m = files();
    m.dirty = false;
    let at_top = press(&mut m, "<Up>");
    assert_eq!(selected(&m), Some(0));
    assert!(at_top.is_empty(), "{at_top:?}");
    assert!(!m.dirty);

    let _ = press(&mut m, "<Down>");
    let _ = press(&mut m, "<Down>");
    let at_bottom = press(&mut m, "<C-n>");
    assert_eq!(selected(&m), Some(2));
    assert!(at_bottom.is_empty(), "{at_bottom:?}");
}

#[test]
fn the_movement_keys_are_never_typed_into_the_query() {
    for key in ["<Down>", "<C-n>", "<C-j>", "<Up>", "<C-p>", "<C-k>"] {
        let mut m = files();
        let effects = press(&mut m, key);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::PickerQuery { .. })),
            "{key}: {effects:?}"
        );
        assert_eq!(m.picker_mut().unwrap().query(), "", "{key}");
    }
}

#[test]
fn a_character_still_edits_the_query() {
    let mut m = files();
    let effects = press(&mut m, "j");
    assert_eq!(m.picker_mut().unwrap().query(), "j");
    assert!(matches!(effects.as_slice(), [Effect::PickerQuery { .. }]));
}

#[test]
fn each_open_key_opens_the_selected_file_in_its_window_and_closes_the_picker() {
    for (key, want) in [
        ("<CR>", OpenIn::Current),
        ("<C-v>", OpenIn::Vertical),
        ("<C-x>", OpenIn::Horizontal),
        ("<C-t>", OpenIn::Tab),
    ] {
        let mut m = files();
        let _ = press(&mut m, "<Down>");
        let (target, how) = opened(&press(&mut m, key));
        assert_eq!(how, want, "{key}");
        let Picked::File { path, line } = target else {
            panic!("{key}: {target:?}");
        };
        assert!(path.ends_with("b.rs"), "{key}: {path}");
        assert_eq!(line, None);
        assert!(!picker_open(&m), "{key} closes the picker");
        assert!(m.dirty, "{key} repaints");
    }
}

#[test]
fn a_grep_match_opens_its_file_at_its_line() {
    for (key, want) in [
        ("<CR>", OpenIn::Current),
        ("<C-v>", OpenIn::Vertical),
        ("<C-x>", OpenIn::Horizontal),
        ("<C-t>", OpenIn::Tab),
    ] {
        let mut m = picker_with(
            "grep",
            vec![
                PickerItem::grep_match("src/a.rs", 3, "first"),
                PickerItem::grep_match("src/b.rs", 42, "second"),
            ],
        );
        let _ = press(&mut m, "<C-j>");
        let (target, how) = opened(&press(&mut m, key));
        assert_eq!(how, want, "{key}");
        let Picked::File { path, line } = target else {
            panic!("{key}: {target:?}");
        };
        assert!(path.ends_with("src/b.rs"), "{key}: {path}");
        assert_eq!(line, Some(42), "{key}");
    }
}

/// The buffers picker opened on nvim's list of `(handle, name)` pairs, its
/// corpus handed back as the matcher's results unfiltered.
fn buffers(listed: &[(u64, &str)]) -> Model {
    let mut m = picker_with("buffers", Vec::new());
    let generation = m.picker_mut().unwrap().generation();
    let buffers = listed
        .iter()
        .map(|&(handle, name)| (handle, name.to_string()))
        .collect();
    let effects = update(
        &mut m,
        Msg::PickerBuffers {
            generation,
            buffers,
        },
    );
    let items = match effects.as_slice() {
        [Effect::PickerQuery {
            resolved: Some(items),
            ..
        }] => items.clone(),
        other => panic!("expected the corpus for the matcher, got {other:?}"),
    };
    let _ = update(&mut m, Msg::PickerResults { generation, items });
    m
}

#[test]
fn a_buffer_opens_by_its_handle_in_each_window() {
    for (key, want) in [
        ("<CR>", OpenIn::Current),
        ("<C-v>", OpenIn::Vertical),
        ("<C-x>", OpenIn::Horizontal),
        ("<C-t>", OpenIn::Tab),
    ] {
        let mut m = buffers(&[(4, "/work/notes.md"), (9, "")]);
        let (target, how) = opened(&press(&mut m, key));
        assert_eq!(how, want, "{key}");
        assert_eq!(target, Picked::Buffer { handle: 4 }, "{key}");
    }
}

/// Two unnamed buffers read alike in the list, and the second one chosen
/// is the one that opens. Opened by name, either reached the first.
#[test]
fn the_second_of_two_unnamed_buffers_opens_that_buffer() {
    let mut m = buffers(&[(3, ""), (5, "/work/notes.md"), (8, "")]);
    let _ = press(&mut m, "<Down>");
    let _ = press(&mut m, "<Down>");
    let (target, _) = opened(&press(&mut m, "<CR>"));
    assert_eq!(target, Picked::Buffer { handle: 8 });
}

#[test]
fn an_open_key_on_no_results_does_nothing_and_the_picker_stays() {
    for key in ["<CR>", "<C-v>", "<C-x>", "<C-t>"] {
        let mut m = picker_with("files", Vec::new());
        let effects = press(&mut m, key);
        assert!(effects.is_empty(), "{key}: {effects:?}");
        assert!(picker_open(&m), "{key}");
    }
}

/// A confirm prompt stacked over the picker holds the focus, so the
/// picker's keys reach the prompt and the picker beneath stays as it was.
#[test]
fn the_picker_keys_reach_a_prompt_stacked_over_it() {
    let mut m = files();
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
    assert!(matches!(
        m.overlays().last().map(|o| &o.kind),
        Some(OverlayKind::Prompt(_))
    ));
    m.note_frame_painted();

    for key in ["<Down>", "<C-j>", "<C-v>", "<C-t>"] {
        let effects = press(&mut m, key);
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                Effect::Rpc(RpcCall::OpenPicked { .. } | RpcCall::PreviewBufferWindow { .. })
            )),
            "{key}: {effects:?}"
        );
        assert_eq!(selected(&m), Some(0), "{key}");
    }
    let effects = press(&mut m, "<CR>");
    assert!(
        matches!(effects.as_slice(), [Effect::Rpc(RpcCall::Input { notation })] if notation == "<CR>"),
        "{effects:?}"
    );
    assert!(picker_open(&m));
}

/// The walk behind `PICKER_KEYS`: every key it documents, pressed on a
/// picker whose selection has a result on either side, changes something.
#[test]
fn every_documented_picker_key_answers_a_real_keystroke() {
    for (key, what) in PICKER_KEYS {
        let mut m = files();
        let _ = press(&mut m, "<Down>");
        m.dirty = false;
        let effects = press(&mut m, key);
        assert!(
            !effects.is_empty() || m.dirty,
            "`{key}` ({what}) is documented but does nothing when pressed"
        );
    }
}
