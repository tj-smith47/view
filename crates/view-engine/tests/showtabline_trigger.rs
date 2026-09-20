//! Live-nvim proof that the bridge relays nvim's own `showtabline`: the
//! value in force when the registration runs, and every change after it.
//!
//! Only a live nvim runs the chunk. `nvim_api.rs` greps its text and
//! `handle::decode`'s tests pin the wire shape, and an option name
//! misspelled in the Lua, or a payload nvim answers as something other
//! than a number, passes both.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use view_core::msg::Msg;
use view_engine::process::{Engine, EngineConfig};

/// The base of the quiet window a drain calls settled, scaled with the
/// load the run started under the way `buffers_trigger.rs` scales it.
const TICK: Duration = Duration::from_millis(500);

/// The readings that arrive until a quiet window passes with nothing on
/// the channel. Every other message is the session's ordinary redraw
/// traffic.
fn drain(rx: &mpsc::Receiver<Msg>) -> Vec<u8> {
    let mut seen = Vec::new();
    loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::ShowTablineChanged { value }) => seen.push(value),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return seen,
        }
    }
}

#[test]
fn the_bridge_reports_showtabline_at_registration_and_on_every_change() {
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
            .unwrap();
    };

    // the registration reads the option itself: a session that set it in
    // its own config is heard without waiting for the user to set it again
    let seeded = drain(&rx);
    assert_eq!(
        seeded.last().copied(),
        Some(1),
        "the registration reported {seeded:?} where nvim's own default is 1"
    );

    // the row is always up at 2 and never up at 0, so both crossings have
    // to reach view
    for asked in [2_u8, 0, 1] {
        lua(&format!("vim.cmd('set showtabline={asked}')"));
        let reported = drain(&rx);
        assert_eq!(
            reported.last().copied(),
            Some(asked),
            "setting the option to {asked} reported {reported:?}"
        );
    }
}
