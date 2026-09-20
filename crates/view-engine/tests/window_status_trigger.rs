//! Live-nvim proof that the bridge's `window` trigger collapses a burst of
//! motions to one notification per turn of nvim's event loop.
//!
//! `CursorMoved` fires once per cursor motion, so a held-down `j` fires it
//! as fast as nvim can redraw; the throttle in
//! `REGISTER_WINDOW_STATUS_CHUNK` is what keeps that from becoming one RPC
//! notification per keystroke on the reader thread. Only a live nvim runs
//! it -- `nvim_api.rs`'s own tests pin the chunk's text and the wire shapes
//! it sends, neither of which says how many times it sends them.
//!
//! The burst is fired with `nvim_exec_autocmds` rather than typed keys
//! because the claim is about one turn of the loop: typed keys arrive over
//! however many turns nvim needs to read them, which would make the count
//! a statement about input timing instead.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use view_core::msg::Msg;
use view_engine::process::{Engine, EngineConfig};

/// How many times the burst fires the trigger inside one `nvim_exec_lua`
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

/// Counts the `window` triggers that arrive until a quiet window passes
/// with nothing on the channel. Every other message is the session's
/// ordinary redraw traffic and is discarded.
fn drain_window_status(rx: &mpsc::Receiver<Msg>) -> usize {
    let mut seen = 0;
    loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::WindowStatus { .. }) => seen += 1,
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

    // the registration reports every window it finds and `VimEnter` repeats
    // the sweep, which is the session's own startup rather than the burst
    let settled = drain_window_status(&rx);
    assert!(settled >= 1, "the registration reported no window at all");

    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(format!(
                    "for _ = 1, {BURST} do vim.api.nvim_exec_autocmds('CursorMoved', {{}}) end"
                )),
                rmpv::Value::Array(Vec::new()),
            ],
        )
        .unwrap();

    let after = drain_window_status(&rx);
    assert_eq!(
        after, 1,
        "{BURST} triggers in one turn of the loop notified {after} times"
    );
}
