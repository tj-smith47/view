//! Live-nvim proof of `MOVE_WINDOW_TO_TABPAGE_CHUNK`'s choreography: what a
//! pure `update()` test cannot see, since the whole point of the chunk is
//! state a round trip through nvim decides (a window's own handle after a
//! close, `tabpagenr('$')` after one auto-closes, where the cursor and the
//! scrolled-to line actually land).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use view_engine::process::{Engine, EngineConfig};

fn spawn_attached() -> Engine {
    let engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    engine
}

/// A window moved to a tabpage that already exists keeps its buffer,
/// cursor line/column and scrolled-to line; a lone window's own tabpage
/// closes with it, and the destination lands where that closed tabpage's
/// removal renumbers it to.
///
/// Three tabpages: the first holds the one window under test (with
/// content, a cursor position and a scroll position none of the other two
/// tabpages have any reason to share), the second and third are plain.
/// Moving that window to tabpage 3 (the third) must: leave `tabpagenr('$')`
/// at 2 (the first tabpage went with its only window), land the moved
/// window in what the removal renumbered to tabpage 2, and answer every
/// one of the readings below with what the source window held before the
/// move ran.
#[test]
fn to_tabpage_from_a_lone_window_closes_its_tabpage_and_keeps_its_view() {
    let engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    let handle = &engine.handle;

    let lines: Vec<String> = (1..=60).map(|n| format!("line {n}")).collect();
    handle
        .command(&format!(":call setline(1, {lines:?})"))
        .unwrap();
    handle.command(":40").unwrap();
    handle.command(":normal! zt").unwrap();
    handle.command(":tabnew").unwrap();
    handle.command(":tabnew").unwrap();
    handle.command(":tabfirst").unwrap();

    let win: u64 = handle.eval_str("win_getid()").unwrap().parse().unwrap();
    let buf: u64 = handle.eval_str("bufnr('%')").unwrap().parse().unwrap();
    let want_line: i64 = handle.eval_str("line('.')").unwrap().parse().unwrap();
    let want_col: i64 = handle.eval_str("col('.')").unwrap().parse().unwrap();
    let want_topline: i64 = handle
        .eval_str("winsaveview()['topline']")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(want_line, 40, "the fixture must land the cursor on line 40");

    handle.move_window_to_tabpage(win, 3).unwrap();
    // a fire-and-forget notify on the same channel is ordered ahead of the
    // request below, so this is the barrier that waits for it to finish
    let _ = handle.eval_str("1").unwrap();

    assert_eq!(
        handle.eval_str("tabpagenr('$')").unwrap(),
        "2",
        "the source tabpage (the moved window's only one) must have closed with it"
    );
    assert_eq!(
        handle
            .eval_str(&format!("len(win_findbuf({buf}))"))
            .unwrap(),
        "1",
        "the buffer must be showing in exactly the one window it moved to"
    );
    assert_eq!(
        handle
            .eval_str(&format!("win_id2tabwin(win_findbuf({buf})[0])[0]"))
            .unwrap(),
        "2",
        "the window must have landed on what the source tabpage's removal renumbered to tabpage 2"
    );
    assert_eq!(
        handle
            .eval_str(&format!("line('.', win_findbuf({buf})[0])"))
            .unwrap()
            .parse::<i64>()
            .unwrap(),
        want_line,
        "the cursor line must have crossed with the window"
    );
    assert_eq!(
        handle
            .eval_str(&format!("col('.', win_findbuf({buf})[0])"))
            .unwrap()
            .parse::<i64>()
            .unwrap(),
        want_col,
        "the cursor column must have crossed with the window"
    );
    assert_eq!(
        handle
            .eval_str(&format!("getwininfo(win_findbuf({buf})[0])[0].topline"))
            .unwrap()
            .parse::<i64>()
            .unwrap(),
        want_topline,
        "the scrolled-to line must have crossed with the window"
    );
}

/// A window moved off a tabpage that keeps another window leaves that
/// tabpage standing, so nothing renumbers: three tabpages before and after,
/// the moved window on tabpage 2 where it was sent, the one it shared a
/// tabpage with still on tabpage 1, and the moved window's buffer, cursor
/// and scrolled-to line crossing with it.
#[test]
fn to_tabpage_moves_the_window_and_keeps_its_view() {
    let engine = spawn_attached();
    let handle = &engine.handle;

    let lines: Vec<String> = (1..=60).map(|n| format!("line {n}")).collect();
    handle
        .command(&format!(":call setline(1, {lines:?})"))
        .unwrap();
    handle.command(":40").unwrap();
    handle.command(":normal! zt").unwrap();
    handle.command(":vnew").unwrap();
    let stays: u64 = handle.eval_str("win_getid()").unwrap().parse().unwrap();
    handle.command(":wincmd l").unwrap();
    handle.command(":tabnew").unwrap();
    handle.command(":tabnew").unwrap();
    handle.command(":tabfirst").unwrap();
    handle.command(":wincmd l").unwrap();
    assert_eq!(
        handle.eval_str("tabpagewinnr(1, '$')").unwrap(),
        "2",
        "the fixture must leave two windows on the source tabpage"
    );

    let win: u64 = handle.eval_str("win_getid()").unwrap().parse().unwrap();
    let buf: u64 = handle.eval_str("bufnr('%')").unwrap().parse().unwrap();
    let want_line: i64 = handle.eval_str("line('.')").unwrap().parse().unwrap();
    let want_col: i64 = handle.eval_str("col('.')").unwrap().parse().unwrap();
    let want_topline: i64 = handle
        .eval_str("winsaveview()['topline']")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(want_line, 40, "the fixture must land the cursor on line 40");

    handle.move_window_to_tabpage(win, 2).unwrap();
    let _ = handle.eval_str("1").unwrap();

    assert_eq!(
        handle.eval_str("tabpagenr('$')").unwrap(),
        "3",
        "the source tabpage kept a window, so no tabpage closed and none opened"
    );
    assert_eq!(
        handle
            .eval_str(&format!("win_id2tabwin({stays})[0]"))
            .unwrap(),
        "1",
        "the window left behind must still stand on the source tabpage"
    );
    assert_eq!(
        handle.eval_str("tabpagewinnr(1, '$')").unwrap(),
        "1",
        "the source tabpage must hold only the window left behind"
    );
    assert_eq!(
        handle
            .eval_str(&format!("len(win_findbuf({buf}))"))
            .unwrap(),
        "1",
        "the buffer must be showing in exactly the one window it moved to"
    );
    let moved = format!("win_findbuf({buf})[0]");
    assert_eq!(
        handle
            .eval_str(&format!("win_id2tabwin({moved})[0]"))
            .unwrap(),
        "2",
        "the window must have landed on the tabpage it was sent to"
    );
    assert_eq!(
        handle
            .eval_str(&format!("line('.', {moved})"))
            .unwrap()
            .parse::<i64>()
            .unwrap(),
        want_line,
        "the cursor line must have crossed with the window"
    );
    assert_eq!(
        handle
            .eval_str(&format!("col('.', {moved})"))
            .unwrap()
            .parse::<i64>()
            .unwrap(),
        want_col,
        "the cursor column must have crossed with the window"
    );
    assert_eq!(
        handle
            .eval_str(&format!("getwininfo({moved})[0].topline"))
            .unwrap()
            .parse::<i64>()
            .unwrap(),
        want_topline,
        "the scrolled-to line must have crossed with the window"
    );
}

/// A destination past `tabpagenr('$')` creates one at the end. The move is
/// neither refused nor clamped to the last existing tabpage.
#[test]
fn to_tabpage_past_the_last_tabpage_creates_one() {
    let engine = spawn_attached();
    let handle = &engine.handle;

    handle.command(":tabnew").unwrap();
    handle.command(":tabfirst").unwrap();
    assert_eq!(handle.eval_str("tabpagenr('$')").unwrap(), "2");

    let win: u64 = handle.eval_str("win_getid()").unwrap().parse().unwrap();
    let buf: u64 = handle.eval_str("bufnr('%')").unwrap().parse().unwrap();

    handle.move_window_to_tabpage(win, 5).unwrap();
    let _ = handle.eval_str("1").unwrap();

    // tab 1 had two windows before the move (the original was split by
    // nothing here -- it is nvim's own second, already-open tabpage that
    // keeps tabpagenr('$') from dropping to 1), so a fresh tabpage is
    // created at the end for the destination past the count
    assert_eq!(
        handle.eval_str("tabpagenr('$')").unwrap(),
        "2",
        "moving the only window off tab 1 must close tab 1, and the new tab \
         created past the count is the tabpage that survives it"
    );
    assert_eq!(
        handle
            .eval_str(&format!("len(win_findbuf({buf}))"))
            .unwrap(),
        "1",
        "the buffer must be showing in exactly the one window the new tabpage opened for it"
    );
}
