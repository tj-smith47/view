//! Live-nvim proof that the bridge's `buffers` trigger reports the listed
//! set, replaces it whole each time, and collapses a burst to one
//! report per turn of nvim's event loop.
//!
//! Only a live nvim runs the chunk: `nvim_api.rs`'s
//! `the_buffers_chunk_arms_every_event_of_its_group_and_defers_to_a_tick`
//! greps its text, `handle::decode`'s tests pin the wire shape, and the
//! close battery seeds the list from Rust. A typo in `vim.bo[buf].buflisted`
//! or a payload field swapped against `decode_buffer_entries` would pass
//! every one of them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use view_core::model::BufferEntry;
use view_core::msg::Msg;
use view_engine::process::{Engine, EngineConfig};

/// How many buffers a burst steps through. Past any plausible per-tick
/// count, so a throttle that collapsed only adjacent pairs still fails.
const BURST: usize = 4;

/// The base of the quiet window a drain calls settled, scaled with the load
/// the run started under the way `window_status_trigger.rs` scales it.
const TICK: Duration = Duration::from_millis(500);

/// The reports that arrive until a quiet window passes with nothing on the
/// channel. Every other message is the session's ordinary redraw traffic.
fn drain(rx: &mpsc::Receiver<Msg>) -> Vec<Vec<BufferEntry>> {
    let mut seen = Vec::new();
    loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::BufferList { buffers }) => seen.push(buffers),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return seen,
        }
    }
}

/// The names one report carries, each with whether it is the current one.
fn names(report: &[BufferEntry]) -> Vec<(&str, bool)> {
    report
        .iter()
        .map(|entry| (entry.name.as_str(), entry.current))
        .collect()
}

#[test]
fn the_buffers_trigger_reports_the_listed_set_once_per_tick() {
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

    // the registration reports the set it finds, which is the one unnamed
    // buffer a session starts with
    let settled = drain(&rx);
    assert!(
        !settled.is_empty(),
        "the registration reported no buffer at all"
    );

    lua("vim.cmd('edit a.rs') vim.cmd('edit b.rs')".to_string());
    let opened = drain(&rx);
    let last = opened.last().expect("opening two files reported nothing");
    assert_eq!(
        names(last),
        // nvim reuses the empty unnamed buffer for the first :edit
        vec![("a.rs", false), ("b.rs", true)],
        "the report names {:?}",
        names(last)
    );

    // the set replaces what view held: a wiped buffer is gone from it
    lua("vim.cmd('bwipeout')".to_string());
    let wiped = drain(&rx);
    let last = wiped.last().expect("a wipe reported nothing");
    assert!(
        names(last).iter().all(|(name, _)| *name != "b.rs"),
        "the wiped buffer is still on the list: {:?}",
        names(last)
    );

    // an unsaved buffer carries its marker, so the row draws one without
    // waiting for the user to change buffers
    lua("vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'edited' })".to_string());
    let touched = drain(&rx);
    let last = touched.last().expect("a modification reported nothing");
    assert!(
        last.iter().any(|entry| entry.modified),
        "nothing on the list reads as modified: {last:?}"
    );

    // a burst of buffer switches in one turn of the loop: one report, and
    // the one it sends names the buffer the burst ended on
    lua(format!(
        "for index = 1, {BURST} do vim.cmd('edit burst' .. index .. '.rs') end"
    ));
    let burst = drain(&rx);
    assert_eq!(
        burst.len(),
        1,
        "{BURST} switches in one turn reported {} times",
        burst.len()
    );
    let last = burst.last().expect("a burst of switches reported nothing");
    assert_eq!(
        last.iter()
            .find(|entry| entry.current)
            .map(|entry| entry.name.as_str()),
        Some(format!("burst{BURST}.rs").as_str()),
        "the report names a buffer the burst had already left: {:?}",
        names(last)
    );

    // a rename and an unlisting change what the row says without adding or
    // removing a buffer, and each arms its own report
    for chunk in [
        "vim.cmd('file renamed.rs')",
        "vim.cmd('setlocal nobuflisted')",
    ] {
        let before = drain(&rx);
        assert!(before.is_empty(), "the session had not settled");
        lua(chunk.to_string());
        assert!(
            !drain(&rx).is_empty(),
            "`{chunk}` armed no report, so the row stays stale"
        );
    }
}
