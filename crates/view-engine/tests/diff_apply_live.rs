//! Live-nvim proof that a review's accepted hunks actually reproduce the
//! text the agent proposed. `Hunk::edits` addresses three different shapes
//! (a hunk with trailing context, one that runs to the buffer's last row,
//! and one that spans the whole buffer), each with its own row/byte-column
//! arithmetic, and none of that is provable against a pure model: only
//! nvim decides what `nvim_buf_set_text` does with those coordinates.
//!
//! Every case here applies whole hunks bottom of the buffer first, the same
//! order `DiffReviewState::accept_all` emits them in, and asserts the
//! buffer ends up holding exactly the proposal's own lines.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use view_core::msg::BufferHandle;
use view_core::native::diff::hunk::{diff, split_lines};
use view_engine::process::{Engine, EngineConfig};

/// Spawns an isolated engine with a UI attached, the same load-bearing
/// attach `buf_set_text_live.rs`'s own `spawn` documents.
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
    engine
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
        .expect("read buffer lines")
        .as_array()
        .expect("lines reply is an array")
        .iter()
        .map(|v| v.as_str().expect("line is a string").to_owned())
        .collect()
}

/// Loads `old` into the current buffer, computes the review's hunks for the
/// `old` -> `new` proposal, accepts all of them as the one batched write
/// `DiffReviewState::accept_all` emits, and returns what nvim then holds.
///
/// `old_text: None` is the new-file case: nothing is loaded, and the buffer
/// stays the single empty line nvim starts a nameless buffer as -- which is
/// exactly what `RpcCall::BufResolve` resolves such a path to.
fn accept_all(engine: &Engine, old: Option<&str>, new: &str) -> Vec<String> {
    if let Some(old) = old {
        set_lines(engine, &split_lines(old));
    } else {
        set_lines(engine, &[""]);
    }
    let mut hunks = diff(old, new);
    assert!(!hunks.is_empty(), "the fixture proposes no change");
    hunks.sort_by_key(|hunk| std::cmp::Reverse(hunk.old_range));
    let edits: Vec<view_core::msg::TextEdit> = hunks.iter().flat_map(|hunk| hunk.edits()).collect();
    engine
        .handle
        .set_buf_text(BufferHandle(0), &edits, false, None)
        .expect("apply the accepted hunks");
    get_lines(engine)
}

/// A hunk in the middle of the file: the shape with context on both sides,
/// addressed by whole rows.
#[test]
fn a_mid_file_hunk_reproduces_the_proposal() {
    let engine = spawn();
    let new = "alpha\nBETA\ngamma\n";

    let lines = accept_all(&engine, Some("alpha\nbeta\ngamma\n"), new);

    assert_eq!(lines, split_lines(new));
}

/// Two separate hunks in one proposal, applied the way accept-all emits
/// them. The upper hunk deliberately changes the buffer's line count:
/// applying it first would shift every row below it, and the lower hunk --
/// already built against the pre-accept buffer, since none of these calls
/// is reported back before the next is emitted -- would then write at rows
/// that have moved. A line-count-neutral fixture proves nothing here.
#[test]
fn two_hunks_in_one_proposal_both_land_where_they_were_computed() {
    let engine = spawn();
    let old = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
    let new = "one\nTWO\nTWO-B\nTWO-C\nthree\nfour\nfive\nSIX\nseven\n";

    let lines = accept_all(&engine, Some(old), new);

    assert_eq!(lines, split_lines(new));
}

/// A hunk running to the buffer's last row has no trailing row to address,
/// so it is written from the end of the row before it -- byte columns, not
/// character columns, which is what the multibyte lead line here proves.
#[test]
fn a_hunk_at_the_last_row_is_addressed_by_byte_columns() {
    let engine = spawn();
    let old = "héllo wörld\nlast\n";
    let new = "héllo wörld\nLAST\n";

    let lines = accept_all(&engine, Some(old), new);

    assert_eq!(lines, split_lines(new));
}

/// A hunk spanning the whole buffer has context on neither side.
#[test]
fn a_whole_buffer_hunk_reproduces_the_proposal() {
    let engine = spawn();
    let new = "entirely\ndifferent\n";

    let lines = accept_all(&engine, Some("old one\nold two\n"), new);

    assert_eq!(lines, split_lines(new));
}

/// A new file: the proposal is the whole content, written into the empty
/// buffer the path resolved to. The trailing-blank-line failure is what
/// this pins -- the buffer must hold the proposal's lines and no extra
/// empty row, since that row would be a blank line in the saved file.
#[test]
fn a_new_file_proposal_leaves_no_trailing_blank_row() {
    let engine = spawn();
    let new = "fn main() {\n    run();\n}\n";

    let lines = accept_all(&engine, None, new);

    assert_eq!(lines, split_lines(new));
}

/// A deletion hunk carries no replacement lines: the rows it names come
/// out, and the rows around them stay whole.
#[test]
fn a_deletion_hunk_removes_only_its_own_rows() {
    let engine = spawn();
    let old = "keep\ndrop me\nkeep too\n";
    let new = "keep\nkeep too\n";

    let lines = accept_all(&engine, Some(old), new);

    assert_eq!(lines, split_lines(new));
}

/// A deletion running to the buffer's last row: the rows come out and the
/// row before them keeps its own bytes.
#[test]
fn a_deletion_at_the_last_row_removes_only_its_own_rows() {
    let engine = spawn();
    let old = "keep\ndrop me\n";
    let new = "keep\n";

    let lines = accept_all(&engine, Some(old), new);

    assert_eq!(lines, split_lines(new));
}

/// A proposal that drops the file's final newline along with a real change
/// to one of its rows. `hunk::diff` collapses a trailing-newline-only
/// difference to nothing, which is the right answer for a buffer nvim
/// models as rows -- this pins that the collapse never reaches past that
/// one difference: the row change still applies, and it applies to exactly
/// the rows it named.
#[test]
fn a_proposal_dropping_the_final_newline_still_applies_its_row_change() {
    let engine = spawn();
    let old = "alpha\nbeta\ngamma\n";
    let new = "alpha\nBETA\ngamma";

    let lines = accept_all(&engine, Some(old), new);

    assert_eq!(lines, vec!["alpha", "BETA", "gamma"]);
}

/// The same pair the other way round: a buffer whose text never ended in a
/// newline, and a proposal that adds one on top of a real row change. The
/// added newline is nvim's `'endofline'` to answer, not this crate's, so
/// what must survive is the row change and nothing else -- no extra empty
/// row, which would be a blank line in the saved file.
#[test]
fn a_proposal_adding_a_final_newline_leaves_no_extra_row() {
    let engine = spawn();
    let old = "alpha\nbeta\ngamma";
    let new = "alpha\nBETA\ngamma\n";

    let lines = accept_all(&engine, Some(old), new);

    assert_eq!(lines, vec!["alpha", "BETA", "gamma"]);
}

/// An insertion at the very top of the file, where there is no leading row
/// to address from.
#[test]
fn an_insertion_at_the_first_row_reproduces_the_proposal() {
    let engine = spawn();
    let old = "second\nthird\n";
    let new = "first\nsecond\nthird\n";

    let lines = accept_all(&engine, Some(old), new);

    assert_eq!(lines, split_lines(new));
}
