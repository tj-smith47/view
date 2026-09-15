//! What the startup hook has in force by the time the rest of a config
//! runs, asked of a live child that is answered and attached the way a
//! session answers and attaches it.
//!
//! Two facts, and the whole of what the hook exists for. The UI is applied
//! before any other `VimEnter` autocommand runs, so the one screen startup
//! draws is drawn for the UI view attached rather than for the stand-in and
//! then again for the real one. And the `UIEnter` nvim does not fire for a
//! UI applied that way is fired here instead, on the loop's next turn,
//! carrying the channel -- which is where nvim would have put it, and late
//! enough that a handler registered from another plugin's `VimEnter` is
//! standing when it arrives.
//!
//! Asked of a real child rather than read off the chunk's text: both facts
//! are about what nvim's own loop does with what the chunk sends, and only
//! nvim can be asked that. The peer here answers `view_vim_enter` and
//! attaches behind the answer, which is the order a session writes them in.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use rmpv::Value;
use std::sync::mpsc;
use std::time::Instant;
use view_core::msg::{EngineRequest, Msg, ReplyValue};
use view_engine::process::{Engine, EngineConfig};
use view_test_support::ScratchDir;

/// A config that records what the hook left it, from the same two places a
/// plugin would read them.
///
/// `nvim_list_uis()` through `nvim_eval` rather than through `vim.api`:
/// the startup chunk shims the Lua binding to answer for nvim's own
/// terminal UI while nothing has attached, which is what a plugin
/// configuring itself has to see, and vimscript's own call is the one that
/// still answers for what has actually attached.
///
/// The `UIEnter` handler is registered from inside `VimEnter` on purpose:
/// that is the window a plugin loaded by a `VimEnter` autocommand or a lazy
/// `config` has, and an event fired before it closes is an event no such
/// plugin ever sees.
const PIN_CONFIG: &str = "\
_G.view_pin =\n\
  { uis = -1, chan = -1, uienters = 0, chan_when_idle = -1, safe = 0 }\n\
vim.api.nvim_create_autocmd('VimEnter', {\n\
  callback = function()\n\
    _G.view_pin.uis = vim.api.nvim_eval('len(nvim_list_uis())')\n\
    vim.api.nvim_create_autocmd('UIEnter', {\n\
      callback = function(args)\n\
        _G.view_pin.uienters = _G.view_pin.uienters + 1\n\
        _G.view_pin.chan = (args.data or {}).chan or 0\n\
      end,\n\
    })\n\
  end,\n\
})\n\
vim.api.nvim_create_autocmd('SafeState', {\n\
  callback = function()\n\
    _G.view_pin.safe = _G.view_pin.safe + 1\n\
    if _G.view_pin.chan_when_idle < 0 then\n\
      _G.view_pin.chan_when_idle = _G.view_pin.chan\n\
    end\n\
  end,\n\
})\n";

/// What the fixture recorded, plus the channel view is talking on, read in
/// one request so no two of them can come from different moments.
///
/// `chan_when_idle` is the channel as the first `SafeState` that has one
/// reads it, which is not the first `SafeState`: the event is scheduled
/// rather than fired inline, and nvim reaches an idle transition of its own
/// before the loop turn that runs it (measured on the pinned engine, where
/// a `once = true` reading there is still `-1`). What it says is that the
/// event reaches a settled session, rather than only a session being polled.
struct Reading {
    uis_at_vim_enter: i64,
    uienters: i64,
    chan: i64,
    chan_when_idle: i64,
    channel: i64,
    safe: i64,
}

fn pin_config() -> ScratchDir {
    let dir = ScratchDir::new("vim-enter-pump").unwrap();
    std::fs::write(dir.join("init.lua"), PIN_CONFIG).unwrap();
    dir
}

/// The child a real session spawns, reading the fixture config.
///
/// `-u` behind [`EngineConfig::isolated`]'s own `--clean`, on the same
/// grounds as `claimant_takeover.rs`'s: both write nvim's one "which
/// config" field and the later argument wins, so the child keeps every
/// hermetic root an isolated spawn owns and still sources exactly this
/// file.
fn engine(dir: &ScratchDir) -> Engine {
    Engine::spawn(
        EngineConfig::isolated()
            .with_arg("-u")
            .with_arg(dir.join("init.lua"))
            .with_late_attach(120, 40),
    )
    .unwrap()
}

fn reading(engine: &Engine) -> Reading {
    let answer = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                Value::from(
                    "local channel = -1 \
                     for _, chan in ipairs(vim.api.nvim_list_chans()) do \
                     if chan.stream == 'stdio' then channel = chan.id end \
                     end \
                     local pin = _G.view_pin \
                     return { pin.uis, pin.uienters, pin.chan, \
                     pin.chan_when_idle, channel, pin.safe }",
                ),
                Value::Array(vec![]),
            ],
        )
        .unwrap();
    let read = answer.as_array().expect("the pin answers with a list");
    let at = |index: usize| read[index].as_i64().unwrap_or(-1);
    Reading {
        uis_at_vim_enter: at(0),
        uienters: at(1),
        chan: at(2),
        chan_when_idle: at(3),
        channel: at(4),
        safe: at(5),
    }
}

/// The `VimEnter` request the child is parked inside, whether it landed
/// before the sink was attached or after.
fn vim_enter_token(
    presink: Vec<Msg>,
    rx: &mpsc::Receiver<Msg>,
) -> Option<view_core::msg::ReplyToken> {
    let staged = presink.into_iter().find_map(|msg| match msg {
        Msg::EngineRequest(EngineRequest::VimEnter { token }) => Some(token),
        _ => None,
    });
    if staged.is_some() {
        return staged;
    }
    let deadline = Instant::now() + common::rpc_deadline_for(3);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::EngineRequest(EngineRequest::VimEnter { token })) => return Some(token),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    None
}

#[test]
fn the_hook_has_the_ui_in_force_before_the_rest_of_startup_runs() {
    let dir = pin_config();
    let mut engine = engine(&dir);
    let (tx, rx) = mpsc::sync_channel(256);
    let (_pump, cutover) = engine.start_pump(tx);

    let token = vim_enter_token(cutover.presink, &rx)
        .expect("the child never asked view_vim_enter, so nothing here was measured");
    engine.handle.reply(token, ReplyValue::Nil).unwrap();
    engine
        .handle
        .ui_attach(120, 40, view_engine::UI_EXT_OPTIONS)
        .unwrap();

    // the event the hook schedules runs on the loop, which answering a
    // request is not: this puts the child back through its own
    engine.handle.eval_str("execute('sleep 100m')").unwrap();
    let deadline = Instant::now() + common::rpc_deadline_for(3);
    let mut read = reading(&engine);
    while Instant::now() < deadline && read.chan_when_idle < 0 {
        read = reading(&engine);
    }

    assert_eq!(
        read.uis_at_vim_enter, 1,
        "the attach view wrote behind its answer must be applied before the \
         config's own VimEnter runs, or the screen startup draws is drawn \
         for a UI that is not there and drawn again for the one that arrives"
    );
    assert_eq!(
        read.uienters, 1,
        "exactly one UIEnter: nvim fires none for a UI applied inside the \
         hook's wait, and a second one from here would run every non-once \
         UIEnter autocommand twice"
    );
    assert_eq!(
        read.chan, read.channel,
        "the event carries the channel in its own data, which is the only \
         place nvim_exec_autocmds can put it -- and the handler reading it \
         was registered from the config's own VimEnter, which is a window \
         an event fired inside view's hook has already closed"
    );
    assert_eq!(
        read.chan_when_idle, read.channel,
        "the event must reach a settled session, not only a polled one; \
         the child took {} idle transitions",
        read.safe
    );
}
