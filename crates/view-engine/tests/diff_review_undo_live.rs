//! Live-nvim proof of the review's undo contract end to end: a whole
//! review, however many hunks it wrote and whatever the user did in
//! between, undoes as the one thing the user decided.
//!
//! This is the half no unit test can reach. `DiffReviewState` decides
//! `undojoin` per write, but whether those writes actually collapse into
//! one undo entry is nvim's answer, and the fallback path
//! (`docs/buf-set-text-wire-capture.md`'s `E790`) is exactly where the
//! composition can come apart: an accept that lands right after the user
//! pressed `u` applies un-joined, and a single `u` then retracts only that
//! hunk -- a half-applied proposal. The scenario below runs the review the
//! way the dispatch does, through `DiffReviewState`'s own effects, and
//! asserts what one `u` leaves in the buffer.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use view_core::msg::{BufferHandle, Effect, Msg, RpcCall};
use view_core::native::ai_panel::DiffReviewState;
use view_core::native::diff::{hunk, BufTextChangedEvent};
use view_engine::nvim_api::BufWriteOutcome;
use view_engine::process::{Engine, EngineConfig};

const OLD: &str = "one\ntwo\nthree\nfour\nfive\nsix\nseven\n";
const NEW: &str = "one\nTWO\nthree\nfour\nfive\nSIX\nseven\n";

/// Spawns an isolated engine with a UI attached -- load-bearing for undo
/// specifically, for the reason `buf_set_text_live.rs`'s own `spawn`
/// documents: without a UI, nvim's main loop hangs no undo boundary
/// between back-to-back API calls and every edit merges into one step
/// regardless of `undojoin`, which would make this file's assertions pass
/// for the wrong reason.
fn spawn() -> Engine {
    let engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    engine.handle.ui_attach(80, 24).expect("attach ui");
    engine
}

fn set_lines(engine: &Engine, buf: u64, first: i64, last: i64, lines: &[&str]) {
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(first),
                rmpv::Value::from(last),
                rmpv::Value::from(false),
                rmpv::Value::Array(lines.iter().map(|l| rmpv::Value::from(*l)).collect()),
            ],
        )
        .expect("set buffer lines");
}

fn text(engine: &Engine, buf: u64) -> String {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    "return table.concat(vim.api.nvim_buf_get_lines(..., 0, -1, false), '\\n')",
                ),
                rmpv::Value::Array(vec![rmpv::Value::from(buf)]),
            ],
        )
        .expect("read buffer text")
        .as_str()
        .expect("buffer text is a string")
        .to_owned()
}

fn current_buf(engine: &Engine) -> u64 {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("return vim.api.nvim_get_current_buf()"),
                rmpv::Value::Array(vec![]),
            ],
        )
        .expect("read current buffer")
        .as_u64()
        .expect("buffer handle is an integer")
}

fn changedtick(engine: &Engine, buf: u64) -> u64 {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("return vim.api.nvim_buf_get_changedtick(...)"),
                rmpv::Value::Array(vec![rmpv::Value::from(buf)]),
            ],
        )
        .expect("read changedtick")
        .as_u64()
        .expect("changedtick is an integer")
}

fn undo(engine: &Engine) {
    engine
        .handle
        .request("nvim_command", vec![rmpv::Value::from("undo")])
        .expect("issue undo");
}

/// Carries out one of the review's own effects against the live engine and
/// folds nvim's answer back into the review, exactly as `Executor::run` and
/// `update()` do between them.
fn carry_out(engine: &Engine, review: &mut DiffReviewState, effect: &Effect) {
    let Effect::Rpc(RpcCall::BufSetText {
        buf,
        edits,
        undojoin,
        expected_changedtick,
        generation,
    }) = effect
    else {
        panic!("expected a BufSetText effect, got {effect:?}")
    };
    match engine
        .handle
        .set_buf_text(*buf, edits, *undojoin, *expected_changedtick)
        .expect("the write reaches nvim")
    {
        BufWriteOutcome::Applied { changedtick } => {
            review.note_write_applied(*buf, *generation, changedtick);
        }
        BufWriteOutcome::BufferAdvanced => review.note_write_refused(*buf, *generation),
    }
}

/// Folds every buffer edit event currently queued into the review, the way
/// the loop's own `Msg::BufTextChanged` arm does. Waits for at least one,
/// then drains whatever else has already arrived.
fn fold_events(rx: &mpsc::Receiver<Msg>, review: &mut DiffReviewState) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut folded = 0;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            folded > 0 || remaining > Duration::ZERO,
            "no Msg::BufTextChanged arrived within 5s"
        );
        let received = if folded == 0 {
            rx.recv_timeout(remaining).ok()
        } else {
            rx.try_recv().ok()
        };
        let Some(msg) = received else {
            return;
        };
        if let Msg::BufTextChanged {
            buf,
            generation,
            firstline,
            lastline,
            linedata,
            changedtick,
            desynced,
        } = msg
        {
            review.apply_change(&BufTextChangedEvent {
                buf,
                generation,
                firstline,
                lastline,
                linedata,
                changedtick,
                desynced,
            });
            folded += 1;
        }
    }
}

/// The brief's own scenario: two hunks, the user editing under the second
/// one between the two accepts, a re-diff, and then one `u`.
///
/// The undo assertion is the falsifiable half. One `u` must leave the
/// buffer byte-identical to the text that stood before the first accept --
/// the user's own concurrent edit included, since that edit is not part of
/// the review and undoing it would be view retracting the user's typing.
/// A second `u` then retracts that edit, which is what proves the review's
/// two writes really did collapse into one entry rather than the first `u`
/// having retracted only the last hunk.
#[test]
fn a_whole_review_undoes_as_one_step_across_a_concurrent_edit_and_a_re_diff() {
    let mut engine = spawn();
    let buf = current_buf(&engine);
    set_lines(&engine, buf, 0, -1, &hunk::split_lines(OLD));
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let mut review = DiffReviewState::new(
        1,
        std::path::PathBuf::from("/fixture.rs"),
        7,
        hunk::diff(Some(OLD), NEW),
    );
    assert_eq!(review.hunks.len(), 2, "the fixture proposes two hunks");
    let effects = review.bind(7, Some(BufferHandle(buf)), changedtick(&engine, buf));
    let Some(Effect::Rpc(RpcCall::BufAttach { buf: attach, .. })) = effects.first() else {
        panic!("binding answers the attach, got {effects:?}")
    };
    engine
        .handle
        .buf_attach(*attach, 7)
        .expect("subscribe to the buffer under review");

    // The user types under the second hunk while the review is open. Its
    // anchor no longer matches, so that hunk goes stale and refuses to
    // apply until it is re-diffed.
    set_lines(&engine, buf, 5, 6, &["six (typed by the user)"]);
    fold_events(&rx, &mut review);
    let before_accepts = text(&engine, buf);
    assert_eq!(
        review.hunks[1].status,
        view_core::native::diff::HunkStatus::Stale,
        "the concurrent edit must stale the hunk it landed in"
    );

    let effects = review.accept(0).expect("the first hunk is still fresh");
    carry_out(&engine, &mut review, &effects[0]);
    fold_events(&rx, &mut review);
    assert!(
        review.accept(1).is_err(),
        "a stale hunk must refuse to apply before it is re-diffed"
    );

    assert!(review.re_diff(1), "the stale hunk re-diffs");
    let effects = review.accept(1).expect("the re-diffed hunk accepts");
    carry_out(&engine, &mut review, &effects[0]);
    fold_events(&rx, &mut review);
    let after_accepts = text(&engine, buf);
    assert!(
        after_accepts.contains("TWO") && after_accepts.contains("SIX"),
        "both hunks must have applied: {after_accepts}"
    );

    undo(&engine);

    assert_eq!(
        text(&engine, buf),
        before_accepts,
        "one undo must retract the whole review -- not one hunk of it, \
         which is the half-applied proposal per-hunk undojoin exists to \
         prevent -- and must not retract the user's own edit with it"
    );

    undo(&engine);
    assert_eq!(
        text(&engine, buf),
        OLD.trim_end_matches('\n'),
        "the second undo retracts the user's own edit, which proves the \
         review's two writes were one entry and not two"
    );
}

/// Two accepts in a row, with no edit event folded in between -- the real
/// case of a user pressing `a` twice faster than the reader thread's
/// events are drained.
///
/// The second write names a tick, and by then the buffer has already moved
/// past the tick the review knew when it bound: its own first write moved
/// it. What keeps the second accept from being refused for no reason is
/// nvim's answer to the first write carrying the tick it produced. This is
/// the test that fails when that answer is dropped.
#[test]
fn a_second_accept_issued_before_any_event_is_folded_still_applies() {
    let mut engine = spawn();
    let buf = current_buf(&engine);
    set_lines(&engine, buf, 0, -1, &hunk::split_lines(OLD));
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let mut review = DiffReviewState::new(
        1,
        std::path::PathBuf::from("/fixture.rs"),
        7,
        hunk::diff(Some(OLD), NEW),
    );
    let _ = review.bind(7, Some(BufferHandle(buf)), changedtick(&engine, buf));

    let first = review.accept(0).expect("the first hunk is fresh");
    carry_out(&engine, &mut review, &first[0]);
    let second = review.accept(1).expect("the second hunk is fresh");
    carry_out(&engine, &mut review, &second[0]);

    assert_eq!(
        text(&engine, buf),
        NEW.trim_end_matches('\n'),
        "both accepts must land without an event folded between them"
    );
    assert!(
        review
            .hunks
            .iter()
            .all(|hunk| hunk.status == view_core::native::diff::HunkStatus::Accepted),
        "a refused second write would have put its hunk back to stale: {:?}",
        review.hunks.iter().map(|h| h.status).collect::<Vec<_>>()
    );
    // the events for both writes are still on their way; folding them is
    // what a real loop does next, and it must not undo any of the above
    fold_events(&rx, &mut review);
    assert_eq!(text(&engine, buf), NEW.trim_end_matches('\n'));
}
