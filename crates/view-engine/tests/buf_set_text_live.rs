//! Live-nvim proof of [`EngineHandle::set_buf_text`]'s three load-bearing
//! claims: `undojoin` genuinely links two `BufSetText` calls into one undo
//! step, row/col are 0-indexed BYTE columns rather than character columns,
//! and a stale buffer handle surfaces as an `Err` rather than a panic or a
//! silently dropped edit. See `docs/buf-set-text-wire-capture.md` for the
//! wire evidence these assertions mirror.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use view_core::msg::{BufferHandle, TextEdit};
use view_engine::handle::EngineError;
use view_engine::process::{Engine, EngineConfig};

/// Spawns an isolated engine with a UI attached and returns its handle. The
/// UI attach is load-bearing, not cosmetic: `EngineConfig::isolated()`'s
/// plain `--embed` (no `--headless`) leaves nvim's main loop with no idle
/// tick to hang an undo-sync boundary on between two back-to-back API
/// calls until a UI attaches, live-observed as every edit in an
/// un-attached session merging into one undo step regardless of
/// `undojoin` -- attaching (even headless-style, painting nothing) is what
/// restores the per-call boundary `set_buf_text`'s `undojoin` contract
/// depends on.
fn spawn() -> Engine {
    let engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    engine.handle.ui_attach(80, 24).expect("attach ui");
    engine
}

fn set_lines(engine: &Engine, lines: &[&str]) {
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(0),
                rmpv::Value::from(0),
                rmpv::Value::from(-1),
                rmpv::Value::from(false),
                rmpv::Value::Array(lines.iter().map(|l| rmpv::Value::from(*l)).collect()),
            ],
        )
        .expect("reset buffer lines");
}

fn get_lines(engine: &Engine) -> Vec<String> {
    let value = engine
        .handle
        .request(
            "nvim_buf_get_lines",
            vec![
                rmpv::Value::from(0),
                rmpv::Value::from(0),
                rmpv::Value::from(-1),
                rmpv::Value::from(false),
            ],
        )
        .expect("read buffer lines");
    value
        .as_array()
        .expect("lines reply is an array")
        .iter()
        .map(|v| v.as_str().expect("line is a string").to_owned())
        .collect()
}

fn undo(engine: &Engine) {
    engine
        .handle
        .request("nvim_command", vec![rmpv::Value::from("undo")])
        .expect("issue undo");
}

/// Two `set_buf_text` calls with `undojoin: [false, true]` join into one
/// undo step -- a single `undo` after both reverts to the state before
/// either edit ran.
#[test]
fn undojoin_true_joins_the_second_edit_onto_the_first() {
    let engine = spawn();
    set_lines(&engine, &["line1", "line2"]);

    engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[TextEdit {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 5,
                lines: vec!["LINE1".to_owned()],
            }],
            false,
        )
        .expect("first edit (undojoin: false)");
    engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[TextEdit {
                start_row: 1,
                start_col: 0,
                end_row: 1,
                end_col: 5,
                lines: vec!["LINE2".to_owned()],
            }],
            true,
        )
        .expect("second edit (undojoin: true)");

    assert_eq!(get_lines(&engine), vec!["LINE1", "LINE2"]);
    undo(&engine);
    assert_eq!(
        get_lines(&engine),
        vec!["line1", "line2"],
        "a single undo after undojoin:true must revert both edits at once"
    );
}

/// The negative control: `undojoin: [false, false]` leaves the two edits as
/// separate undo steps, so one `undo` reverts only the second.
#[test]
fn undojoin_false_keeps_the_second_edit_as_its_own_undo_step() {
    let engine = spawn();
    set_lines(&engine, &["line1", "line2"]);

    engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[TextEdit {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 5,
                lines: vec!["LINE1".to_owned()],
            }],
            false,
        )
        .expect("first edit");
    engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[TextEdit {
                start_row: 1,
                start_col: 0,
                end_row: 1,
                end_col: 5,
                lines: vec!["LINE2".to_owned()],
            }],
            false,
        )
        .expect("second edit (undojoin: false)");

    assert_eq!(get_lines(&engine), vec!["LINE1", "LINE2"]);
    undo(&engine);
    assert_eq!(
        get_lines(&engine),
        vec!["LINE1", "line2"],
        "without undojoin, one undo must revert only the second edit"
    );
}

/// `TextEdit`'s columns are 0-indexed BYTE offsets, not character
/// offsets. `"héllo"` is 5 characters but 6 bytes (`é` is a 2-byte UTF-8
/// sequence); replacing `end_col: 6` (the byte length) must consume the
/// whole word, while a caller that mistakenly used the character length
/// (`end_col: 5`) would leave the trailing byte of `é`'s encoding as a
/// stray `o` -- live-verified in `docs/buf-set-text-wire-capture.md`.
#[test]
fn text_edit_columns_are_byte_offsets_not_character_offsets() {
    let engine = spawn();
    set_lines(&engine, &["héllo wörld"]);

    engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[TextEdit {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 6, // byte length of "héllo" (h=1, é=2, l=1, l=1, o=1)
                lines: vec!["X".to_owned()],
            }],
            false,
        )
        .expect("byte-column edit");

    assert_eq!(get_lines(&engine), vec!["X wörld"]);
}

/// A `BufSetText` call against a buffer handle that no longer exists
/// (closed between an agent's proposal and the user's accept) must surface
/// as `Err`, never a panic and never a silently dropped edit.
#[test]
fn stale_buffer_handle_surfaces_as_an_error_not_a_panic() {
    let engine = spawn();

    let created = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("return vim.api.nvim_create_buf(false, true)"),
                rmpv::Value::Array(vec![]),
            ],
        )
        .expect("create scratch buffer");
    let stale = created.as_u64().expect("buffer handle is an integer");

    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("vim.api.nvim_buf_delete(..., {force = true})"),
                rmpv::Value::Array(vec![rmpv::Value::from(stale)]),
            ],
        )
        .expect("delete scratch buffer");

    let result = engine.handle.set_buf_text(
        BufferHandle(stale),
        &[TextEdit {
            start_row: 0,
            start_col: 0,
            end_row: 0,
            end_col: 0,
            lines: vec!["x".to_owned()],
        }],
        false,
    );

    match result {
        Err(EngineError::Remote(_)) => {}
        other => panic!("expected EngineError::Remote for a stale buffer handle, got {other:?}"),
    }
}
