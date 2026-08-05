//! Live-nvim proof of the "user's `g:clipboard` wins" contract:
//! `EngineHandle::register_clipboard` must leave an already-set
//! `g:clipboard` untouched, and must install view's own provider only when
//! the user's config left it unset. Neither half of this can be a pure
//! `view-core` unit test: the precedence check itself runs Lua-side inside
//! the injected chunk (see `view_engine::nvim_api::REGISTER_CLIPBOARD_CHUNK`)
//! and never crosses back to Rust as data, so the only way to prove it holds
//! is to run it against a real spawned engine and read `g:clipboard` back.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rmpv::Value;
use view_engine::process::{Engine, EngineConfig};

#[test]
fn an_existing_g_clipboard_survives_registration() {
    let engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let channel_id = engine.api_info.channel_id;
    engine
        .handle
        .request(
            "nvim_command",
            vec![Value::from("let g:clipboard = {'name': 'user-provider'}")],
        )
        .unwrap();

    engine.handle.register_clipboard(channel_id).unwrap();

    // the notify above and this request share one writer-thread stream, and
    // nvim services one connection's traffic in order, so by the time this
    // reply arrives the exec_lua chunk has already run (same ordering
    // argument as `register_bridge`'s own doc comment)
    let got = engine
        .handle
        .request("nvim_eval", vec![Value::from("g:clipboard.name")])
        .unwrap();
    assert_eq!(
        got.as_str(),
        Some("user-provider"),
        "an existing g:clipboard must be left untouched, got {got:?}"
    );
}

#[test]
fn an_unset_g_clipboard_becomes_views_provider() {
    let engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let channel_id = engine.api_info.channel_id;

    engine.handle.register_clipboard(channel_id).unwrap();

    let got = engine
        .handle
        .request("nvim_eval", vec![Value::from("g:clipboard.name")])
        .unwrap();
    assert_eq!(
        got.as_str(),
        Some("view"),
        "an unset g:clipboard must become view's own provider, got {got:?}"
    );
}
