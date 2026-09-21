//! C1, against real nvim v0.12.4: the windowed palette's tile must never
//! become nvim's current or previous window. `palette_probe.lua` and
//! `search_probe.lua` (the review's own probes, kept under
//! `~/.claude/tmp/s3-task5-review/`) showed `:q` closing the tile instead
//! of the user's own window and a search running against the tile's empty
//! scratch buffer; both are the open chunk taking `curwin` on every `:`
//! and `/`, which the `enter` argument fixes (see
//! `view_engine::nvim_api::native_window::OPEN_NATIVE_WINDOW_CHUNK`'s doc).

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

const COLS: u16 = 100;
const ROWS: u16 = 30;
const QUIESCE_SILENCE: Duration = Duration::from_millis(200);
const QUIESCE_DEADLINE: Duration = Duration::from_secs(10);
/// The silence window the latency measurement settles on -- every other
/// leg in this file uses [`QUIESCE_SILENCE`] because it is quiescing
/// between steps whose correctness matters, not timing them. A stopwatch
/// that never returns before 200ms of quiet has passed cannot tell a 1ms
/// round trip from a 50ms one; it reads the floor. This window still
/// waits for real quiet on a local pty, just a shorter stretch of it.
const LATENCY_SILENCE: Duration = Duration::from_millis(5);

fn build_fixture(root: &Path) -> std::path::PathBuf {
    let dir = root.join("workspace");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "a\nhello\nc\n").unwrap();
    std::fs::write(dir.join("b.txt"), "b one\nb two\n").unwrap();
    dir
}

/// Two real windows -- `a.txt` on the left, `b.txt` (current) on the
/// right -- and the palette windowed at the bottom: the shape the
/// review's own probes used, since a single-window session cannot tell
/// "the palette took `curwin`" from "there was nothing else to take".
fn two_window_palette_session(dir: &Path) -> view_oracle::EngineSession {
    let mut engine = view_oracle::EngineSession::spawn_with_ext(
        COLS,
        ROWS,
        view_oracle::UI_EXT_OPTIONS_MULTIGRID,
    )
    .expect("EngineSession::spawn_with_ext against real nvim");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine
        .arm_and_input(&format!(
            ":cd {}<CR>:edit a.txt<CR>:vsplit b.txt<CR>",
            dir.display()
        ))
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    engine.set_surface(
        view_core::native::geometry::NativeSurface::Palette,
        view_core::native::geometry::SurfaceLayout::new(
            view_core::native::geometry::SurfacePlacement::Windowed,
            view_core::native::geometry::Anchor::Bottom,
            30,
        ),
    );
    // this driver builds its model directly rather than through a
    // `view.toml`, whose absent `[native] palette` key still means "on"
    engine.enable_palette();
    engine
}

/// `nvim_get_current_win()`, `bufname('%')` and `winnr('$')`, joined by
/// `;` -- what every leg below reads back to tell "acted on the user's own
/// window" from "acted on the palette's tile".
fn win_state(engine: &mut view_oracle::EngineSession) -> String {
    engine
        .eval_str("win_getid() . ';' . bufname('%') . ';' . winnr('$')")
        .expect("nvim answers for its own state")
        .trim()
        .to_string()
}

/// `:q` while the palette is open must close the user's own current
/// window (`b.txt`'s), never the palette's tile.
#[test]
fn colon_q_closes_the_users_own_window_not_the_palette() {
    let work = common::ScratchPaths::new("windowed-palette-q");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    let before = win_state(&mut engine);
    let before_win = before.split(';').next().unwrap().to_string();

    open_palette_and_settle(&mut engine);
    engine.arm_and_input("q<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let after = win_state(&mut engine);
    let mut parts = after.split(';');
    let after_win = parts.next().unwrap();
    let after_buf = parts.next().unwrap();
    let after_count = parts.next().unwrap();
    assert_ne!(
        after_win, before_win,
        "`:q` must have closed the window it ran in"
    );
    assert_eq!(
        after_buf, "a.txt",
        "`:q` on `b.txt`'s window must leave `a.txt`'s standing, not the \
         palette's scratch window: {after}"
    );
    assert_eq!(
        after_count, "1",
        "no window of the user's own should survive besides a.txt's: {after}"
    );
}

/// `:w` while the palette is open must answer for the user's own buffer,
/// never E382 (`buftype=nofile`) from the palette's scratch buffer.
#[test]
fn colon_w_writes_the_users_own_buffer() {
    let work = common::ScratchPaths::new("windowed-palette-w");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    engine.arm_and_input("A modified<Esc>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    engine.arm_and_input(":w<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let errmsg = engine.eval_str("v:errmsg").unwrap();
    assert!(
        !errmsg.contains("E382"),
        "`:w` answered the palette's own unnamed scratch buffer: {errmsg}"
    );
    let saved = std::fs::read_to_string(dir.join("b.txt")).unwrap();
    assert!(
        saved.contains("modified"),
        "the user's own file must carry the write: {saved:?}"
    );
}

/// `:e <file>` while the palette is open must load the file into the
/// user's own window, never the palette's tile.
#[test]
fn colon_e_opens_into_the_users_own_window() {
    let work = common::ScratchPaths::new("windowed-palette-e");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);

    open_palette_and_settle(&mut engine);
    engine.arm_and_input("e a.txt<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine.eval_str("bufname('%')").unwrap().trim(),
        "a.txt",
        "`:e a.txt` must land in the window the user was in, not the \
         palette's own scratch window"
    );
}

/// `/pat` while the palette is open must search the user's own buffer,
/// never the palette's empty scratch buffer.
#[test]
fn search_runs_against_the_users_own_buffer() {
    let work = common::ScratchPaths::new("windowed-palette-search");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    engine.arm_and_input(":e a.txt<CR>gg<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    engine.arm_and_input("/hello<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let errmsg = engine.eval_str("v:errmsg").unwrap();
    assert!(
        !errmsg.contains("E486"),
        "the search must find `hello` in the user's own buffer, not run \
         against the palette's empty one: {errmsg}"
    );
    let cursor = engine.eval_str("line('.')").unwrap();
    assert_eq!(
        cursor.trim(),
        "2",
        "the cursor must have moved to `hello` on line 2"
    );
}

/// `:only` while the palette is open must leave the user with one window
/// of their own -- the palette's tile is gone, synchronously, before
/// `:only` even runs (see the open chunk's `CmdlineLeave` autocmd).
#[test]
fn colon_only_leaves_one_of_the_users_own_windows() {
    let work = common::ScratchPaths::new("windowed-palette-only");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);

    open_palette_and_settle(&mut engine);
    engine.arm_and_input("only<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine.eval_str("winnr('$')").unwrap().trim(),
        "1",
        "`:only` must leave exactly the user's own current window"
    );
    assert_eq!(engine.eval_str("bufname('%')").unwrap().trim(), "b.txt");
}

/// `:wincmd p` while the palette is open must still name the window that
/// was previous before the palette ever opened, never the palette's tile.
#[test]
fn wincmd_p_still_names_the_original_previous_window() {
    let work = common::ScratchPaths::new("windowed-palette-wincmd-p");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    let prev_before = engine
        .eval_str("win_getid(winnr('#'))")
        .unwrap()
        .trim()
        .to_string();

    engine.arm_and_input(":<Esc>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    engine.arm_and_input(":wincmd p<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine.eval_str("win_getid()").unwrap().trim(),
        prev_before,
        "`:wincmd p` must land on the window that was previous before the \
         palette ever opened"
    );
}

/// A counter on the four `Win*` events, taken before and after a plain `:`
/// open and `<Esc>` close: none of them may fire for either.
#[test]
fn no_win_autocmd_fires_for_the_palettes_own_open_or_close() {
    let work = common::ScratchPaths::new("windowed-palette-autocmd-count");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    engine
        .arm_and_input(
            ":let g:_c = 0<CR>:autocmd WinEnter,WinLeave,WinNew,WinClosed * let g:_c += 1<CR>",
        )
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    engine.arm_and_input(":<Esc>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine.eval_str("g:_c").unwrap().trim(),
        "0",
        "a `Win*` autocmd fired for the palette's own open or close"
    );
}

/// `q:` (the command-line window) and `input()` both raise `CmdlineShow`
/// too; neither may leave the user anywhere but their own window once
/// dismissed.
#[test]
fn cmdwin_and_input_leave_the_user_in_their_own_window() {
    let work = common::ScratchPaths::new("windowed-palette-cmdwin-input");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    let before_buf = engine.eval_str("bufname('%')").unwrap().trim().to_string();

    engine.arm_and_input("q::q<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("bufname('%')").unwrap().trim(),
        before_buf,
        "leaving the command-line window must return to the user's own \
         window"
    );

    engine.arm_and_input(":call input('x: ')<CR><CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("bufname('%')").unwrap().trim(),
        before_buf,
        "answering `input()` must leave the user in their own window"
    );
}

/// The measured cost of pressing `:` -- the median over 30 presses,
/// windowed palette on versus off -- since a windowed palette's open costs
/// a split and a restore every keystroke, unlike a floating one.
#[test]
fn the_cost_of_pressing_colon_windowed_versus_off() {
    let work = common::ScratchPaths::new("windowed-palette-latency");
    let dir = build_fixture(&work.isolated_home);

    let windowed = median_colon_latency(&mut two_window_palette_session(&dir));

    let work_off = common::ScratchPaths::new("windowed-palette-latency-off");
    let dir_off = build_fixture(&work_off.isolated_home);
    let mut off = view_oracle::EngineSession::spawn_with_ext(
        COLS,
        ROWS,
        view_oracle::UI_EXT_OPTIONS_MULTIGRID,
    )
    .expect("EngineSession::spawn_with_ext against real nvim");
    assert!(off.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    off.arm_and_input(&format!(
        ":cd {}<CR>:edit a.txt<CR>:vsplit b.txt<CR>",
        dir_off.display()
    ))
    .unwrap();
    assert!(off.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    // otherwise this measures the bare bottom-row cmdline echo, not the
    // floating palette the doc comment above names
    off.enable_palette();
    let floating = median_colon_latency(&mut off);

    eprintln!(
        "median colon-press latency: windowed palette = {windowed:?}, \
         floating palette = {floating:?}"
    );
}

/// The median wall time from sending `:` to nvim's `cmdline_show` reply
/// settling, over 30 presses, each followed by `<Esc>`.
fn median_colon_latency(engine: &mut view_oracle::EngineSession) -> Duration {
    let mut samples = Vec::with_capacity(30);
    for _ in 0..30 {
        let start = Instant::now();
        engine.arm_and_input(":<Esc>").unwrap();
        assert!(engine.quiesce(LATENCY_SILENCE, QUIESCE_DEADLINE).unwrap());
        samples.push(start.elapsed());
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

/// Presses `:` alone and waits for the round trip that opens the
/// palette's tile to settle, the way a real human's typing always leaves
/// room for (the round trip is a few milliseconds; the next keystroke is
/// tens more), before the caller types the rest of the command. Feeding a
/// whole `":only<CR>"` in one `feedkeys` burst can outrun that round trip --
/// nvim finishes running `only` before the client has even reacted to
/// `cmdline_show`, testing what the command does before the palette's
/// window exists rather than what it does once the window is open.
fn open_palette_and_settle(engine: &mut view_oracle::EngineSession) {
    engine.arm_and_input(":").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert!(
        engine.palette_is_open(),
        "the palette must be open before the rest of the command types"
    );
}
