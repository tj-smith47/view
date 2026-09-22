//! Against real nvim v0.12.4: the windowed palette carries no nvim
//! window of its own, so every leg here proves the replacement -- a tile
//! [`view_surface::render`] paints itself, off
//! `model.engine.cmdline` alone -- against the painted cells nvim actually
//! sends, not the model that describes them. Each leg types `:`, waits for
//! the round trip a real keystroke always leaves room for, reads the typed
//! text off the tile's own painted rect, checks that `winnr('$')` and
//! nvim's own grid height never moved for it, types the rest of the
//! command, and then checks the tile is gone and `eventignore` reads empty
//! again.

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
/// right -- and the palette windowed at the bottom: two windows so a leg
/// can tell "acted on the user's own current window" from "there was
/// nothing else to act on".
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
/// `;` -- what a leg reads back to tell "acted on the user's own window"
/// from "acted on the palette's tile".
fn win_state(engine: &mut view_oracle::EngineSession) -> String {
    engine
        .eval_str("win_getid() . ';' . bufname('%') . ';' . winnr('$')")
        .expect("nvim answers for its own state")
        .trim()
        .to_string()
}

/// The palette's own painted layer this frame -- the same layer
/// [`view_surface::render`] paints the tile through -- excluding the
/// notification stream's message-history overlay, which renders through
/// the same [`view_surface::LayerKind::Palette`] kind.
fn palette_layer_rect(engine: &mut view_oracle::EngineSession) -> Option<view_surface::Rect> {
    engine
        .surface()
        .layers
        .into_iter()
        .find_map(|layer| match layer.kind {
            view_surface::LayerKind::Palette(view)
                if view.title != view_core::native::palette::MESSAGE_HISTORY_TITLE =>
            {
                Some(layer.rect)
            }
            _ => None,
        })
}

/// Whether `needle` appears in the painted cells of the palette's own
/// tile this frame -- the actual glyphs [`view_oracle::EngineSession::screen_rows`]
/// renders, never the model's own copy of what it meant to draw. `None`
/// (no tile painted) reads as `false`.
///
/// Every call also asserts the band's own shape: centred across the full
/// terminal width, anchored to the bottom edge, by the same ring gap on
/// every side (`palette_rect`'s doc -- zero under `gaps = false`, two rows
/// and columns under the default `gaps = true`). A windowed palette painted
/// off to one side, narrowed, or anchored somewhere other than the bottom
/// still contains `needle` and would otherwise read as a pass here -- this
/// is the one place every leg that confirms the tile is up reads its rect.
fn tile_shows(engine: &mut view_oracle::EngineSession, needle: &str) -> bool {
    let Some(rect) = palette_layer_rect(engine) else {
        return false;
    };
    let right_margin = COLS.saturating_sub(rect.col).saturating_sub(rect.width);
    assert_eq!(
        rect.col, right_margin,
        "the windowed palette band must be centred across the full \
         terminal width, the same ring gap on its left and right"
    );
    let bottom_margin = ROWS.saturating_sub(rect.row).saturating_sub(rect.height);
    assert_eq!(
        rect.col, bottom_margin,
        "the windowed palette band must be anchored to the bottom edge, \
         the same ring gap below it as beside it"
    );
    let rows = engine.screen_rows();
    (rect.row..rect.row.saturating_add(rect.height)).any(|r| {
        rows.get(usize::from(r)).is_some_and(|row| {
            let band: String = row
                .chars()
                .skip(usize::from(rect.col))
                .take(usize::from(rect.width))
                .collect();
            band.contains(needle)
        })
    })
}

/// nvim's own window count and grid height, read together so a leg can
/// assert neither moved around its own action -- the windowed palette's
/// whole point is that nvim never sees a split or a resize for it.
fn nvim_shape(engine: &mut view_oracle::EngineSession) -> (String, String) {
    (
        engine.eval_str("winnr('$')").unwrap().trim().to_string(),
        engine.eval_str("&lines").unwrap().trim().to_string(),
    )
}

/// Presses `:` alone and waits for the round trip that paints the
/// palette's tile to settle, the way a real human's typing always leaves
/// room for (the round trip is a few milliseconds; the next keystroke is
/// tens more), before the caller types the rest of the command. Feeding a
/// whole `":only<CR>"` in one `feedkeys` burst can outrun that round trip.
///
/// Asserts the tile is up, carries the typed `:` in its own painted
/// cells, and that nvim's own window count and grid height are exactly
/// `shape_before` -- the three things every leg below needs true before
/// it types the rest of its own command.
fn open_palette_and_settle(
    engine: &mut view_oracle::EngineSession,
    shape_before: &(String, String),
) {
    engine.arm_and_input(":").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert!(
        tile_shows(engine, ":"),
        "the tile must paint the `:` a person just typed in its own cells"
    );
    assert_eq!(
        &nvim_shape(engine),
        shape_before,
        "nvim's own window count and grid height must not move for a tile \
         it never opened a window or resized for"
    );
}

/// What every leg below asserts once its command has run: the tile is off
/// the next painted frame, and `eventignore` reads exactly what it started
/// as -- never left standing from a split nvim never really made.
fn assert_palette_closed_cleanly(engine: &mut view_oracle::EngineSession) {
    assert!(
        palette_layer_rect(engine).is_none(),
        "the palette's tile is still on the painted frame after its \
         command ran"
    );
    assert_eq!(
        engine.eval_str("&eventignore").unwrap().trim(),
        "",
        "the palette's open left eventignore set for the rest of the \
         session"
    );
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
    let shape = nvim_shape(&mut engine);

    open_palette_and_settle(&mut engine, &shape);
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
    assert_palette_closed_cleanly(&mut engine);
}

/// `:w` while the palette is open must answer for the user's own buffer,
/// never E382 (`buftype=nofile`) from a palette's scratch buffer -- there
/// is none any more, but the write must still land on `b.txt`.
#[test]
fn colon_w_writes_the_users_own_buffer() {
    let work = common::ScratchPaths::new("windowed-palette-w");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    engine.arm_and_input("A modified<Esc>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    let shape = nvim_shape(&mut engine);

    open_palette_and_settle(&mut engine, &shape);
    engine.arm_and_input("w<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let errmsg = engine.eval_str("v:errmsg").unwrap();
    assert!(
        !errmsg.contains("E382"),
        "`:w` answered a scratch buffer instead of the user's own: {errmsg}"
    );
    let saved = std::fs::read_to_string(dir.join("b.txt")).unwrap();
    assert!(
        saved.contains("modified"),
        "the user's own file must carry the write: {saved:?}"
    );
    assert_palette_closed_cleanly(&mut engine);
}

/// `:e <file>` while the palette is open must load the file into the
/// user's own window, never the palette's tile.
#[test]
fn colon_e_opens_into_the_users_own_window() {
    let work = common::ScratchPaths::new("windowed-palette-e");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    let shape = nvim_shape(&mut engine);

    open_palette_and_settle(&mut engine, &shape);
    engine.arm_and_input("e a.txt<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine.eval_str("bufname('%')").unwrap().trim(),
        "a.txt",
        "`:e a.txt` must land in the window the user was in"
    );
    assert_palette_closed_cleanly(&mut engine);
}

/// `/pat` while the palette is open must search the user's own buffer,
/// never an empty scratch one.
#[test]
fn search_runs_against_the_users_own_buffer() {
    let work = common::ScratchPaths::new("windowed-palette-search");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    engine.arm_and_input(":e a.txt<CR>gg<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    let shape = nvim_shape(&mut engine);

    engine.arm_and_input("/").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert!(
        tile_shows(&mut engine, "/"),
        "the tile must paint the `/` a person just typed"
    );
    assert_eq!(&nvim_shape(&mut engine), &shape);
    engine.arm_and_input("hello<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    let errmsg = engine.eval_str("v:errmsg").unwrap();
    assert!(
        !errmsg.contains("E486"),
        "the search must find `hello` in the user's own buffer: {errmsg}"
    );
    assert_eq!(
        engine.eval_str("line('.')").unwrap().trim(),
        "2",
        "the cursor must have moved to `hello` on line 2"
    );
    assert_palette_closed_cleanly(&mut engine);
}

/// `:only` while the palette is open must leave the user with one window
/// of their own -- the tile is a paint-time overlay, never a real window
/// for `:only` to count or close.
#[test]
fn colon_only_leaves_one_of_the_users_own_windows() {
    let work = common::ScratchPaths::new("windowed-palette-only");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    let shape = nvim_shape(&mut engine);

    open_palette_and_settle(&mut engine, &shape);
    engine.arm_and_input("only<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine.eval_str("winnr('$')").unwrap().trim(),
        "1",
        "`:only` must leave exactly the user's own current window"
    );
    assert_eq!(engine.eval_str("bufname('%')").unwrap().trim(), "b.txt");
    assert_palette_closed_cleanly(&mut engine);
}

/// `:wincmd p` while the palette is open must still name the window that
/// was previous before the palette ever opened -- there is no tile window
/// left standing in nvim's own previous-window record any more.
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
    let shape = nvim_shape(&mut engine);

    open_palette_and_settle(&mut engine, &shape);
    engine.arm_and_input("<Esc>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_palette_closed_cleanly(&mut engine);

    engine.arm_and_input(":wincmd p<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine.eval_str("win_getid()").unwrap().trim(),
        prev_before,
        "`:wincmd p` must land on the window that was previous before the \
         palette ever opened"
    );
}

/// `q:` opens the command-line window, which raises `CmdlineShow` too, and
/// typing `:` again from inside it must still paint the tile over the
/// cmdwin's own buffer without nvim ever seeing a second window or a
/// resize for it.
#[test]
fn cmdwin_then_colon_inside_it_paints_the_tile_and_leaves_no_residue() {
    let work = common::ScratchPaths::new("windowed-palette-cmdwin");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    let before_buf = engine.eval_str("bufname('%')").unwrap().trim().to_string();

    engine.arm_and_input("q:").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    let shape = nvim_shape(&mut engine);

    engine.arm_and_input(":").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert!(
        tile_shows(&mut engine, ":"),
        "the tile must paint even while the command-line window is open"
    );
    assert_eq!(&nvim_shape(&mut engine), &shape);

    engine.arm_and_input("<Esc>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_palette_closed_cleanly(&mut engine);

    engine.arm_and_input(":q<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("bufname('%')").unwrap().trim(),
        before_buf,
        "leaving the command-line window must return to the user's own \
         window"
    );
    assert_palette_closed_cleanly(&mut engine);
}

/// `input()` raises `CmdlineShow` on the same terms `:` does, and must
/// leave the user in their own window once answered, with the tile up
/// and painting while it runs.
#[test]
fn input_paints_the_tile_and_leaves_the_user_in_their_own_window() {
    let work = common::ScratchPaths::new("windowed-palette-input");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    let before_buf = engine.eval_str("bufname('%')").unwrap().trim().to_string();
    let shape = nvim_shape(&mut engine);

    engine.arm_and_input(":call input('x: ')<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert!(
        tile_shows(&mut engine, "x:"),
        "the tile must paint input()'s own prompt"
    );
    assert_eq!(&nvim_shape(&mut engine), &shape);

    engine.arm_and_input("<CR>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_eq!(
        engine.eval_str("bufname('%')").unwrap().trim(),
        before_buf,
        "answering `input()` must leave the user in their own window"
    );
    assert_palette_closed_cleanly(&mut engine);
}

/// A key mapped to a whole `:...<CR>` command (a person's own `nnoremap`)
/// must run the same as typing it by hand -- the tile
/// opens, paints, and closes for a mapping-driven command exactly as it
/// does for one the user typed a character at a time.
#[test]
fn a_mapped_colon_command_paints_the_tile_and_closes_it() {
    let work = common::ScratchPaths::new("windowed-palette-mapped");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    // `<lt>CR>` types the four literal characters `<CR>` into the RHS
    // text nvim stores for the mapping, rather than letting this driver's
    // own notation parser turn it into the keypress that submits the
    // `:nnoremap` command line early -- the trailing `<CR>` is that
    // keypress
    engine
        .arm_and_input(":nnoremap Q :echo 1<lt>CR><CR>")
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    let shape = nvim_shape(&mut engine);

    engine.arm_and_input("Q").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine.eval_str("v:errmsg").unwrap().trim(),
        "",
        "the mapped command must have run cleanly"
    );
    assert_eq!(&nvim_shape(&mut engine), &shape);
    assert_palette_closed_cleanly(&mut engine);
}

/// A ring step (`:View ui cycle_surfaces`) fired while the cmdline is
/// still open must never open a second palette layer, resize anything, or
/// leave `eventignore` standing -- the tile paints off `model.engine.cmdline`
/// alone, which a ring step never touches, so this leg has nothing left to
/// race: the RPC-open mechanism that could race is gone.
#[test]
fn a_ring_step_with_the_cmdline_open_touches_neither_nvim_nor_the_tile() {
    let work = common::ScratchPaths::new("windowed-palette-ring-step");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    let shape = nvim_shape(&mut engine);

    open_palette_and_settle(&mut engine, &shape);

    engine
        .feed(view_core::msg::Msg::FeatureInvoke {
            feature: "ui".to_string(),
            verb: "cycle_surfaces".to_string(),
        })
        .expect("the ring answers its own invoke while the cmdline is open");
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine
            .surface()
            .layers
            .iter()
            .filter(|layer| matches!(
                &layer.kind,
                view_surface::LayerKind::Palette(view)
                    if view.title != view_core::native::palette::MESSAGE_HISTORY_TITLE
            ))
            .count(),
        1,
        "a ring step must never paint a second palette tile"
    );
    assert_eq!(&nvim_shape(&mut engine), &shape);
    assert_eq!(
        engine.eval_str("&eventignore").unwrap().trim(),
        "",
        "a ring step must never leave eventignore standing for the palette"
    );

    engine.arm_and_input("<Esc>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());
    assert_palette_closed_cleanly(&mut engine);
}

/// A counter on the four `Win*` events plus `BufEnter`, taken before and
/// after a plain `:` open and `<Esc>` close: none of them may fire for
/// either, since the tile is paint-time state, never a window or a buffer
/// switch.
#[test]
fn no_win_or_bufenter_autocmd_fires_for_the_palettes_own_open_or_close() {
    let work = common::ScratchPaths::new("windowed-palette-autocmd-count");
    let dir = build_fixture(&work.isolated_home);
    let mut engine = two_window_palette_session(&dir);
    engine
        .arm_and_input(
            ":let g:_c = 0<CR>:autocmd WinEnter,WinLeave,WinNew,WinClosed,BufEnter * let g:_c += 1<CR>",
        )
        .unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    engine.arm_and_input(":<Esc>").unwrap();
    assert!(engine.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE).unwrap());

    assert_eq!(
        engine.eval_str("g:_c").unwrap().trim(),
        "0",
        "a Win* or BufEnter autocmd fired for the palette's own open or \
         close"
    );
    assert_palette_closed_cleanly(&mut engine);
}

/// The measured cost of pressing `:` then `<Esc>` -- the median over 30
/// full open-close cycles, tile versus the centred overlay -- with
/// `LATENCY_SILENCE` and a no-op baseline the way
/// `the_cost_of_one_ring_step_with_all_four_surfaces_open`
/// (`close_battery.rs`) measures its own floor. `baseline` is printed for
/// context but asserted on nowhere: the three medians are the harness's
/// silence window plus one round trip, close enough together that a
/// direction-only compare against it is noise, not signal. The tile is
/// bounded against the overlay it stands beside instead, with headroom for
/// scheduling jitter, and both against the frame budget a person can feel.
#[test]
fn the_cost_of_pressing_colon_windowed_versus_off() {
    let work = common::ScratchPaths::new("windowed-palette-latency");

    let baseline = median_colon_latency(
        &common::ScratchPaths::new("windowed-palette-latency-noop"),
        |_| {},
    );
    let windowed = median_colon_latency(&work, |engine| {
        engine.set_surface(
            view_core::native::geometry::NativeSurface::Palette,
            view_core::native::geometry::SurfaceLayout::new(
                view_core::native::geometry::SurfacePlacement::Windowed,
                view_core::native::geometry::Anchor::Bottom,
                30,
            ),
        );
        engine.enable_palette();
    });
    let work_off = common::ScratchPaths::new("windowed-palette-latency-off");
    let floating = median_colon_latency(&work_off, |engine| {
        // otherwise this measures the bare bottom-row cmdline echo, not
        // the floating palette this leg names
        engine.enable_palette();
    });

    eprintln!(
        "median :<Esc> latency over 30 presses: no-op baseline = \
         {baseline:?}, windowed tile = {windowed:?}, floating overlay = \
         {floating:?}"
    );
    let frame_budget = Duration::from_millis(16);
    let headroom = Duration::from_millis(2);
    assert!(
        windowed < frame_budget,
        "windowed tile latency exceeds a frame's worth of budget: {windowed:?}"
    );
    assert!(
        floating < frame_budget,
        "floating overlay latency exceeds a frame's worth of budget: {floating:?}"
    );
    assert!(
        windowed <= floating + headroom,
        "windowed tile costs more than the floating overlay plus headroom: \
         {windowed:?} > {floating:?} + {headroom:?}"
    );
}

/// Builds a fresh two-window session against `paths`, applies `configure`
/// to it (setting the palette windowed, or leaving it at its overlay
/// default, or touching nothing at all for the no-op baseline), and
/// returns the median wall time of 30 `:<Esc>` cycles.
fn median_colon_latency(
    paths: &common::ScratchPaths,
    configure: impl FnOnce(&mut view_oracle::EngineSession),
) -> Duration {
    let dir = build_fixture(&paths.isolated_home);
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
    configure(&mut engine);

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
