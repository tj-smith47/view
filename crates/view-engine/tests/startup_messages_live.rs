//! Live-nvim proof that what the child said before view had a UI still
//! reaches view.
//!
//! A late attach means the user's config is sourced with no UI on the
//! channel, so every `echomsg`, deprecation warning and plugin complaint
//! raised during startup goes to nvim's own message history and to no
//! screen at all -- there is no screen yet. What view owes the user is that
//! those lines are still readable afterwards, and the takeover's own reply
//! is where they cross: it asks nvim what it said in the same request that
//! performs the takeover, so the answer costs no extra round trip on
//! `VimEnter`.
//!
//! Asked of a real child rather than of the chunk's text: the question is
//! whether nvim's history actually holds a line raised that early, and only
//! nvim can be asked that.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::mpsc;
use std::time::Instant;
use view_core::msg::{Msg, TakeoverStep};
use view_engine::process::{Engine, EngineConfig};
use view_test_support::ScratchDir;

/// The marker a config raises while nvim is still sourcing it -- before
/// `VimEnter`, and so before any attach view performs.
const MARKER: &str = "PRE-ATTACH-MARKER";

#[test]
fn a_line_raised_before_the_attach_comes_back_with_the_takeover_reply() {
    let dir = ScratchDir::new("startup-messages-live").unwrap();
    std::fs::write(
        dir.join("init.lua"),
        format!("vim.api.nvim_echo({{ {{ '{MARKER}' }} }}, true, {{}})\n"),
    )
    .unwrap();
    let mut engine = Engine::spawn(
        EngineConfig::isolated()
            .with_arg("-u")
            .with_arg(dir.join("init.lua"))
            .with_late_attach(120, 40),
    )
    .unwrap();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine.handle.takeover(&[TakeoverStep::HoldNotify]).unwrap();

    let deadline = Instant::now() + common::rpc_deadline();
    let mut said = None;
    while Instant::now() < deadline && said.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::StartupMessages { text }) => said = Some(text),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let said = said.expect("the takeover reply must carry nvim's own startup messages");
    assert!(
        said.contains(MARKER),
        "a line raised while the config was sourcing must survive the \
         attach that follows it: {said:?}"
    );
}
