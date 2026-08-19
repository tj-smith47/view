//! Live-nvim proof of [`EngineHandle::set_buf_text`]'s three load-bearing
//! claims: `undojoin` genuinely links two `BufSetText` calls into one undo
//! step, row/col are 0-indexed BYTE columns rather than character columns,
//! and a stale buffer handle surfaces as an `Err` rather than a panic or a
//! silently dropped edit. See `docs/buf-set-text-wire-capture.md` for the
//! wire evidence these assertions mirror.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use view_core::msg::{BufferHandle, TextEdit};
use view_engine::handle::EngineError;
use view_engine::nvim_api::BufWriteOutcome;
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
        )
        .expect("byte-column edit");

    assert_eq!(get_lines(&engine), vec!["X wörld"]);
}

/// `TextEdit.start_col` is also a 0-indexed BYTE offset, not a character
/// offset -- the same claim `text_edit_columns_are_byte_offsets_not_character_offsets`
/// pins for `end_col`, but every edit in that test (and every other test in
/// this file) starts at `start_col: 0`, where byte and character confusion
/// is invisible. `"héllo wörld"`'s `h` is 1 byte and `é` is 2, so byte
/// offset 3 (not character offset 2) is where `llo` begins; a caller that
/// mistakenly used the character count would splice into the middle of
/// `é`'s encoding instead.
#[test]
fn text_edit_start_col_is_a_byte_offset_not_a_character_offset() {
    let engine = spawn();
    set_lines(&engine, &["héllo wörld"]);

    engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[TextEdit {
                start_row: 0,
                start_col: 3, // byte offset after "h" (1) + "é" (2)
                end_row: 0,
                end_col: 6,
                lines: vec!["LLO".to_owned()],
            }],
            false,
            None,
        )
        .expect("byte start-column edit");

    assert_eq!(get_lines(&engine), vec!["héLLO wörld"]);
}

/// A single edit spanning multiple rows must apply with `start_row` and
/// `end_row` in the order `TextEdit` declares them -- every other edit in
/// this file starts and ends on the same row, so a start/end row swap in
/// the argument-mapping code (e.g. writing `edit.end_row` under the
/// `"start_row"` key) would pass every other test here while still sending
/// nvim a backwards range.
#[test]
fn set_buf_text_applies_a_multi_row_edit_without_swapping_start_and_end_row() {
    let engine = spawn();
    set_lines(&engine, &["one", "two", "three"]);

    engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[TextEdit {
                start_row: 0,
                start_col: 1,
                end_row: 2,
                end_col: 2,
                lines: vec!["X".to_owned()],
            }],
            false,
            None,
        )
        .expect("multi-row edit");

    assert_eq!(get_lines(&engine), vec!["oXree"]);
}

/// `undojoin: true` issued right after the user pressed `u` must not drop
/// the edit. `:undojoin` throws `E790: undojoin is not allowed after undo`
/// whenever the previous action was an undo (`:help undo-joining`), live-
/// confirmed in `docs/buf-set-text-wire-capture.md`; the required fallback
/// is to apply the edit anyway, as its own unjoined undo step, since an
/// accepted diff hunk must never silently vanish just because it landed
/// right after an undo.
#[test]
fn undojoin_true_after_an_undo_falls_back_to_applying_unjoined() {
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
            None,
        )
        .expect("first edit");
    undo(&engine);
    assert_eq!(
        get_lines(&engine),
        vec!["line1", "line2"],
        "sanity: the first edit is undone before the fallback case runs"
    );

    engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[TextEdit {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 5,
                lines: vec!["LINE1-AGAIN".to_owned()],
            }],
            true,
            None,
        )
        .expect("undojoin:true right after an undo must still apply, not error out");

    assert_eq!(get_lines(&engine), vec!["LINE1-AGAIN", "line2"]);

    undo(&engine);
    assert_eq!(
        get_lines(&engine),
        vec!["line1", "line2"],
        "the fallback edit is its own undo step, since it could not join"
    );
}

/// A batch's edits apply correctly regardless of the order the caller lists
/// them in, because the executor sorts by descending `(start_row,
/// start_col)` before applying: two edits on one line, listed here in
/// ascending column order, must both land at their addressed positions
/// rather than the first edit's length change shifting the second's
/// now-stale column.
#[test]
fn set_buf_text_applies_edits_in_position_order_regardless_of_batch_order() {
    let engine = spawn();
    set_lines(&engine, &["aaa bbb ccc"]);

    engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[
                TextEdit {
                    start_row: 0,
                    start_col: 0,
                    end_row: 0,
                    end_col: 3,
                    lines: vec!["XXXX".to_owned()],
                },
                TextEdit {
                    start_row: 0,
                    start_col: 8,
                    end_row: 0,
                    end_col: 11,
                    lines: vec!["YYYY".to_owned()],
                },
            ],
            false,
            None,
        )
        .expect("ascending-order batch");

    assert_eq!(get_lines(&engine), vec!["XXXX bbb YYYY"]);
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
        None,
    );

    match result {
        Err(EngineError::Remote(_)) => {}
        other => panic!("expected EngineError::Remote for a stale buffer handle, got {other:?}"),
    }
}

/// The accept race, made unrepresentable. A write names the
/// `b:changedtick` its rows were computed against; if the buffer moved in
/// between -- the user typing while a proposal was being accepted -- nvim
/// refuses the whole batch and writes nothing at all. The check lives in
/// the same chunk as the apply for exactly this reason: no check on the
/// caller's side of the wire can close the window between itself and the
/// apply.
#[test]
fn a_write_naming_a_stale_changedtick_is_refused_and_writes_nothing() {
    let engine = spawn();
    set_lines(&engine, &["one", "two"]);
    let stamped = changedtick(&engine);

    // the user's own edit, between the moment the caller read the tick and
    // the moment its write would land
    set_lines(&engine, &["one", "two", "three"]);

    let outcome = engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[TextEdit {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 3,
                lines: vec!["ONE".to_string()],
            }],
            false,
            Some(stamped),
        )
        .expect("the refusal is an outcome, never an error");

    assert_eq!(outcome, BufWriteOutcome::BufferAdvanced);
    assert_eq!(
        get_lines(&engine),
        vec!["one".to_string(), "two".to_string(), "three".to_string()],
        "a refused write must leave the buffer byte-identical"
    );
}

/// The same call with the tick the buffer actually holds applies normally:
/// the guard refuses a stale expectation, never a current one.
#[test]
fn a_write_naming_the_current_changedtick_applies() {
    let engine = spawn();
    set_lines(&engine, &["one", "two"]);

    let outcome = engine
        .handle
        .set_buf_text(
            BufferHandle(0),
            &[TextEdit {
                start_row: 0,
                start_col: 0,
                end_row: 0,
                end_col: 3,
                lines: vec!["ONE".to_string()],
            }],
            false,
            Some(changedtick(&engine)),
        )
        .expect("apply");

    assert!(matches!(outcome, BufWriteOutcome::Applied { .. }));
    assert_eq!(
        get_lines(&engine),
        vec!["ONE".to_string(), "two".to_string()]
    );
}

fn changedtick(engine: &Engine) -> u64 {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("return vim.api.nvim_buf_get_changedtick(0)"),
                rmpv::Value::Array(vec![]),
            ],
        )
        .expect("read changedtick")
        .as_u64()
        .expect("changedtick is an integer")
}
