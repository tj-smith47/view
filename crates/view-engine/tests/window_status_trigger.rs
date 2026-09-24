//! Live-nvim proof that the bridge's `window` trigger collapses a burst of
//! events to one notification per turn of nvim's event loop, that the
//! collapse loses nothing, and that a cursor motion reaches a window's
//! status through `win_viewport` with no notification at all.
//!
//! Only a live nvim runs the trigger: `nvim_api.rs` pins the chunk's own
//! text in
//! `the_window_status_chunk_arms_every_event_of_its_group_and_defers_to_a_tick`
//! and `handle::decode`'s tests pin the wire shapes, neither of which says
//! how many times it sends them.
//!
//! Each phase below drains before the next one starts, so a message one
//! phase left on the channel is never counted as the next one's.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use view_core::events::WinHandle;
use view_core::model::{Model, WindowStatus};
use view_core::msg::Msg;
use view_core::update::update;
use view_engine::process::{Engine, EngineConfig};

/// How many times a burst fires the trigger inside one `nvim_exec_lua`
/// call. Far past any plausible per-tick redraw count, so a throttle that
/// collapsed only adjacent pairs still fails.
const BURST: usize = 50;

/// The base of the quiet window a drain calls settled.
///
/// The throttle defers by one turn of nvim's event loop, which no constant
/// states, so what the wait has to cover is the host reaching that turn and
/// the notification crossing to view's reader thread. `host_deadline`
/// scales it with the load the run started under.
const TICK: Duration = Duration::from_millis(500);

/// The `window` reports that arrive until a quiet window passes with
/// nothing on the channel. Every other message is the session's ordinary
/// redraw traffic and is discarded.
fn drain_window_status(rx: &mpsc::Receiver<Msg>) -> Vec<(WinHandle, WindowStatus)> {
    let mut seen = Vec::new();
    loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::WindowStatus { win, status }) => seen.push((win, status)),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return seen,
        }
    }
}

#[test]
fn a_cursor_burst_collapses_to_one_message_per_tick() {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let channel = engine.api_info.channel_id;
    let (tx, rx) = mpsc::sync_channel(256);
    let (_pump, _cutover) = engine.start_pump(tx);
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    engine.handle.register_bridge(channel).unwrap();
    let lua = |chunk: String| {
        engine
            .handle
            .request(
                "nvim_exec_lua",
                vec![rmpv::Value::from(chunk), rmpv::Value::Array(Vec::new())],
            )
            .unwrap();
    };

    // the registration reports every window it finds and `VimEnter` repeats
    // the sweep, which is the session's own startup and no burst
    let settled = drain_window_status(&rx);
    assert!(
        !settled.is_empty(),
        "the registration reported no window at all"
    );

    lua("local lines = {} \
         for i = 1, 60 do lines[i] = 'line ' .. i end \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)"
        .to_string());
    let _ = drain_window_status(&rx);

    // real motions, each followed by an event of the group: what the
    // collapse may not lose is the position the burst ended on
    lua(format!(
        "vim.cmd('normal! gg') for _ = 1, {BURST} do vim.cmd('normal! j') \
         vim.api.nvim_exec_autocmds('BufModifiedSet', \
         {{ buffer = vim.api.nvim_get_current_buf() }}) end"
    ));
    let moved = drain_window_status(&rx);
    assert_eq!(
        moved.len(),
        1,
        "{BURST} motions in one turn of the loop notified {} times",
        moved.len()
    );
    assert_eq!(
        moved[0].1.row,
        (BURST + 1) as u32,
        "the collapse kept a position the burst had already left"
    );

    // a second burst: a flush that never re-arms reports the first one and
    // then goes quiet for the rest of the session
    lua(format!(
        "for _ = 1, {BURST} do vim.api.nvim_exec_autocmds('WinEnter', {{}}) end"
    ));
    let again = drain_window_status(&rx);
    assert_eq!(
        again.len(),
        1,
        "a second burst notified {} times",
        again.len()
    );

    // every event of the group arms; these two carry a buffer
    for event in ["BufModifiedSet", "DiagnosticChanged"] {
        lua(format!(
            "vim.api.nvim_exec_autocmds('{event}', \
             {{ buffer = vim.api.nvim_get_current_buf() }})"
        ));
        let armed = drain_window_status(&rx);
        assert_eq!(armed.len(), 1, "{event} notified {} times", armed.len());
    }

    // two windows armed in one turn: one flush, one report each, and the
    // window each report names is its own
    // the split's own `WinEnter` is the session settling, and it is drained
    // before the burst
    lua("vim.cmd('vsplit')".to_string());
    let _ = drain_window_status(&rx);
    lua("vim.api.nvim_exec_autocmds('DiagnosticChanged', \
         { buffer = vim.api.nvim_get_current_buf() })"
        .to_string());
    let both = drain_window_status(&rx);
    assert_eq!(
        both.len(),
        2,
        "two windows armed in one turn notified {} times",
        both.len()
    );
    assert_ne!(
        both[0].0, both[1].0,
        "two windows reported under one handle: {both:?}"
    );

    // a window closed between the tick that armed it and the tick that
    // flushes it takes no other window's report with it
    let closing = both[0].0;
    lua(format!(
        "vim.api.nvim_exec_autocmds('DiagnosticChanged', \
         {{ buffer = vim.api.nvim_get_current_buf() }}) \
         vim.api.nvim_win_close({}, true)",
        closing.0
    ));
    // the count is not pinned on this drain: closing a window enters the
    // other one, which arms it again a tick later. These two assertions
    // redden a flush that reported the dead handle or stopped at it, and
    // not a missing `pcall`: `report` returns early on an invalid window,
    // so the guard the `pcall` is there for is pinned on the chunk's text
    // in `nvim_api.rs`
    let survivor = drain_window_status(&rx);
    assert!(
        survivor.iter().any(|(win, _)| *win == both[1].0),
        "the flush stopped at the closed window and the survivor lost its \
         report: {survivor:?}"
    );
    assert!(
        survivor.iter().all(|(win, _)| *win != closing),
        "a window that no longer exists reported a status: {survivor:?}"
    );

    // the session goes on: a close that left the throttle armed with a
    // dead handle in its pending set reports nothing from here on
    lua(format!(
        "for _ = 1, {BURST} do vim.api.nvim_exec_autocmds('WinEnter', {{}}) end"
    ));
    let after_close = drain_window_status(&rx);
    assert_eq!(
        after_close.len(),
        1,
        "a burst after the close notified {} times",
        after_close.len()
    );
    assert_eq!(
        after_close[0].0, both[1].0,
        "the burst after the close reported a window that is gone"
    );
}

/// The startup sweep reports every window without loading `vim.diagnostic`,
/// which costs about a millisecond on nvim's startup clock, and the first
/// diagnostic a producer sets still reaches the window's report.
#[test]
fn the_startup_sweep_leaves_the_diagnostic_module_unloaded_and_counts_still_arrive() {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let channel = engine.api_info.channel_id;
    let (tx, rx) = mpsc::sync_channel(256);
    let (_pump, _cutover) = engine.start_pump(tx);
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    engine.handle.register_bridge(channel).unwrap();
    let lua = |chunk: &str| {
        engine
            .handle
            .request(
                "nvim_exec_lua",
                vec![rmpv::Value::from(chunk), rmpv::Value::Array(Vec::new())],
            )
            .unwrap()
    };

    let settled = drain_window_status(&rx);
    assert!(
        !settled.is_empty(),
        "the registration reported no window at all"
    );
    assert_eq!(
        lua("return package.loaded['vim.diagnostic'] ~= nil"),
        rmpv::Value::Boolean(false),
        "the startup sweep loaded vim.diagnostic"
    );

    lua("local d = vim.diagnostic \
         local ns = vim.api.nvim_create_namespace('view_test') \
         d.set(ns, 0, { \
           { lnum = 0, col = 0, severity = d.severity.ERROR, message = 'e' }, \
           { lnum = 0, col = 0, severity = d.severity.WARN, message = 'w' }, \
         })");
    let reported = drain_window_status(&rx);
    assert!(
        reported
            .iter()
            .any(|(_, status)| status.errors == 1 && status.warnings == 1),
        "the first diagnostic set never reached the window's report: {reported:?}"
    );
}

/// Folds every message into `model` until a quiet window passes, the way
/// the runtime loop does, and counts the `window` reports among them.
fn fold(
    rx: &mpsc::Receiver<Msg>,
    pump: &view_engine::damage::DamagePump,
    model: &mut Model,
) -> usize {
    let mut reports = 0;
    loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::RedrawReady) => {
                let _ = update(model, Msg::Redraw(pump.take_damage()));
            }
            Ok(msg @ Msg::WindowStatus { .. }) => {
                reports += 1;
                let _ = update(model, msg);
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return reports,
        }
    }
}

/// Typed motions, in normal and insert mode, move the window's position
/// with no `window` report: nvim's `win_viewport` carries the cursor, so a
/// keystroke costs no Lua callback and no notification.
#[test]
fn a_typed_motion_moves_the_window_position_with_no_report() {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let channel = engine.api_info.channel_id;
    let (tx, rx) = mpsc::sync_channel(256);
    let (pump, _cutover) = engine.start_pump(tx);
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS_MULTIGRID)
        .unwrap();
    engine.handle.register_bridge(channel).unwrap();
    let lua = |chunk: &str| {
        engine
            .handle
            .request(
                "nvim_exec_lua",
                vec![rmpv::Value::from(chunk), rmpv::Value::Array(Vec::new())],
            )
            .unwrap()
    };
    let mut model = Model::new();
    let _ = update(
        &mut model,
        Msg::Resized {
            width: 80,
            height: 24,
        },
    );

    lua("local lines = {} \
         for i = 1, 60 do lines[i] = 'line ' .. i end \
         vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)");
    let _ = fold(&rx, &pump, &mut model);
    let win = WinHandle(
        lua("return vim.api.nvim_get_current_win()")
            .as_u64()
            .unwrap(),
    );
    assert!(
        model.window_status.contains_key(&win),
        "the registration never reported the window"
    );

    let position = |model: &Model| {
        let status = &model.window_status[&win];
        (status.row, status.col)
    };
    engine.handle.input("5G$").unwrap();
    let reports = fold(&rx, &pump, &mut model);
    assert_eq!(position(&model), (5, 6), "a normal-mode motion");
    assert_eq!(reports, 0, "a normal-mode motion sent a window report");

    // the buffer is already modified, so typing fires no `BufModifiedSet`
    engine.handle.input("a!!").unwrap();
    let reports = fold(&rx, &pump, &mut model);
    assert_eq!(position(&model), (5, 9), "typing in insert mode");
    assert_eq!(reports, 0, "typing in insert mode sent a window report");
}
