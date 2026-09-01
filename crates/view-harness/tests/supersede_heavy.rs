//! The heavy-fixture leg of the supersession proof: a takeover asserted
//! against a real, plugin-managed session where the plugin being superseded
//! is actually running and actually wants the same surface back.
//!
//! The synthetic live test (`crates/view/tests/supersede_live.rs`) pins the
//! pair against a one-line `init.lua`. That version cannot observe the one
//! thing a plugin does that a config file never does: re-assert the option
//! later, on its own events. lualine registers `ColorScheme` and
//! `OptionSet background` autocmds that re-run its `setup()`, and `setup()`
//! sets `laststatus` back -- so a takeover that is correct at the moment it
//! is applied can still be silently undone one `:colorscheme` later, with
//! view still believing it owns the status line. That is what this file
//! exists to observe.
//!
//! Lives beside the compat harness rather than in `view`'s own tests
//! because the fixture, its pinned plugin set, and the lockfile-keyed
//! install cache are all this crate's, and because it belongs to the same
//! CI leg that already restores that cache (`task compat-supersede`, run
//! after `task compat`). Ignored by default for the same reason: a cold
//! cache makes it clone a full plugin stack from the network, which the
//! fast `task ci` legs must never do.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use view_core::msg::{Msg, RpcCall};
use view_core::native::registry;
use view_engine::handle::EngineHandle;
use view_engine::process::{Engine, EngineConfig};
use view_harness::fixture::{
    cache_root, copy_dir_recursive, fixtures_root, lockfile_cache_key, scratch_root,
};
use view_native::config::NativeConfig;
use view_native::supersede::{plan, Supersession};

/// How long a warm-cache heavy fixture is given to finish loading its
/// plugin stack, before the host's own contention is accounted for.
/// Generous rather than tight: the only failure it can hide is a fixture
/// that never loads at all, which the probe below reports as itself.
const LOAD_BOUND: Duration = Duration::from_secs(180);

/// How long the held option is given to come back after something else
/// writes it, likewise before scaling. Much tighter than [`LOAD_BOUND`]
/// because the guard's own bound is one return to nvim's main loop, and no
/// plugin install is in flight by then.
const HOLD_BOUND: Duration = Duration::from_secs(10);

/// [`LOAD_BOUND`] widened for the load this run started under.
///
/// Every millisecond of both bounds is the host's: a plugin stack loading
/// off a warm cache and an autocommand getting a turn on nvim's main loop
/// cost whatever the machine has left over, and a flat wall clock covering
/// them fails on a busy runner while saying nothing about supersession.
fn load_timeout() -> Duration {
    view_test_support::host_deadline(LOAD_BOUND)
}

/// [`HOLD_BOUND`] widened the same way.
fn hold_timeout() -> Duration {
    view_test_support::host_deadline(HOLD_BOUND)
}

/// `laststatus` as the heavy fixture's own lualine sets it: lualine's
/// `set_statusline()` writes `3` under `globalstatus` and `2` otherwise,
/// and this fixture's `opts = {}` leaves `globalstatus` at its default
/// `false`. Asserted rather than merely read back, so a run where lualine
/// silently failed to claim the surface cannot pass as a run where view
/// took it.
const LUALINE_LASTSTATUS: &str = "2";

/// The heavy fixture tree, copied per run: lazy.nvim writes its own
/// lockfile back into `stdpath("config")`, so a session pointed at the
/// committed tree could modify a checked-in file.
fn config_home(name: &str) -> PathBuf {
    let dir = scratch_root("supersede-heavy").join(format!("{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let home = dir.join("xdg_config_home");
    copy_dir_recursive(&fixtures_root().join("heavy"), &home).unwrap();
    home
}

/// The shared plugin install the compat harness maintains, keyed by the
/// same `lazy-lock.json` hash so a run here is warm exactly when a compat
/// run is.
fn data_home() -> PathBuf {
    let lockfile = fixtures_root()
        .join("heavy")
        .join("nvim")
        .join("lazy-lock.json");
    let bytes = std::fs::read(&lockfile).unwrap();
    cache_root().join(lockfile_cache_key(&bytes))
}

/// Every file under `dir` as sorted `(relative path, bytes)` pairs, walked
/// recursively: the snapshot both config-untouched assertions compare. Byte
/// equality rather than a digest, and the path listing rides along so a run
/// that left every existing file alone and wrote a new one beside them
/// fails the same assertion.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            walk(root, &path, out);
        } else {
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            out.push((name, std::fs::read(&path).unwrap()));
        }
    }
}

/// A live nvim reading the heavy fixture, with a UI attached and its
/// redraw traffic drained.
///
/// Spawned through [`EngineConfig::default`], never `isolated()`: `--clean`
/// would discard the very config this asserts against. The XDG homes are
/// the same four the compat driver sets, so this session's world is the one
/// every compat scenario already runs in.
///
/// The attach is load-bearing. An `--embed` nvim holds startup until a UI
/// attaches, so every option read before that point reads as nvim's
/// built-in default rather than as the fixture's.
fn session(config: &Path, scratch: &Path) -> Engine {
    let mut engine = Engine::spawn(
        EngineConfig::default()
            .with_env("XDG_CONFIG_HOME", config)
            .with_env("XDG_DATA_HOME", data_home())
            .with_env("XDG_STATE_HOME", scratch.join("xdg_state_home"))
            .with_env("XDG_CACHE_HOME", scratch.join("xdg_cache_home"))
            .with_env("VIEW_COMPAT_SOCK", scratch.join("sock")),
    )
    .unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    // drained for the engine's lifetime rather than dropped, per
    // `Engine::start_pump`'s contract
    std::thread::spawn(move || while rx.recv().is_ok() {});
    engine
        .handle
        .ui_attach(100, 30, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    engine
}

/// Evaluates `expr` until it answers `expected`, or `timeout` passes.
///
/// Retrying rather than asserting once: a heavy fixture is still installing
/// and sourcing plugins for seconds after attach, and a deferred
/// `nvim_eval` legitimately times out while nvim's loop is busy doing it.
fn wait_for(handle: &EngineHandle, expr: &str, expected: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last = String::from("<no reply>");
    while Instant::now() < deadline {
        if let Ok(value) = handle.eval_str(expr) {
            if value == expected {
                return value;
            }
            last = value;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("{expr} never became {expected:?} within {timeout:?}; last answer {last:?}");
}

/// Blocks until the fixture's plugin stack is loaded and lualine actually
/// owns the status line, so what follows is measured against a live
/// plugin rather than against a session that failed to install one.
fn wait_for_lualine(handle: &EngineHandle) {
    wait_for(
        handle,
        "luaeval('package.loaded.lualine ~= nil')",
        "true",
        load_timeout(),
    );
    wait_for(handle, "&statusline =~# 'lualine'", "1", load_timeout());
    wait_for(handle, "&laststatus", LUALINE_LASTSTATUS, load_timeout());
}

/// Installs a witness that records `laststatus` as it stood at the start of
/// every screen redraw from here on, and discards anything recorded before.
///
/// What a takeover actually claims is about what nvim paints, and that is
/// not the same question as what the option reads back afterwards: the
/// guard's idle arm necessarily runs *after* the write it undoes, so a
/// takeover could read as held while a frame had already been painted
/// without it. A decoration provider's `on_start` runs at the top of each
/// redraw cycle, so what it records is what that frame was drawn with --
/// the difference, observed rather than assumed away.
///
/// Rides `luaeval` because it is test scaffolding rather than product
/// behaviour, and writes no `'` so the whole chunk survives as one
/// single-quoted vimscript string.
fn install_redraw_witness(handle: &EngineHandle) {
    handle
        .eval_str(
            "luaeval('(function() \
             _G.__view_redraws = {} \
             local ns = vim.api.nvim_create_namespace([[view-redraw-witness]]) \
             vim.api.nvim_set_decoration_provider(ns, { on_start = function() \
             table.insert(_G.__view_redraws, \
             tostring(vim.api.nvim_get_option_value([[laststatus]], {}))) \
             end }) \
             return 1 end)()')",
        )
        .unwrap();
}

/// Every frame the witness has seen, as the `laststatus` each was painted
/// with. Waits for at least one: a takeover asserted over a session that
/// never redrew would pass every check here vacuously.
fn witnessed_frames(handle: &EngineHandle) -> Vec<String> {
    wait_for(
        handle,
        "luaeval('#_G.__view_redraws > 0')",
        "true",
        hold_timeout(),
    );
    handle
        .eval_str("luaeval('table.concat(_G.__view_redraws, [[,]])')")
        .unwrap()
        .split(',')
        .map(str::to_string)
        .collect()
}

/// Applies one plan against `handle` the way the runtime's executor does.
/// The executor itself lives inside the `view` bin target and is
/// unreachable from here; the mapping this mirrors is pinned by that
/// crate's own `every_supersession_entry_reaches_an_engine_op`.
fn apply(handle: &EngineHandle, plan: &[Supersession]) {
    for entry in plan {
        match &entry.rpc {
            RpcCall::HoldOption { name, value } => handle.hold_option(name, value).unwrap(),
            RpcCall::HoldNotify => handle.hold_notify().unwrap(),
            other => panic!("a plan entry must ride a durable takeover call, got {other:?}"),
        }
    }
}

#[test]
#[ignore = "heavy fixture: run via `task compat-supersede`, which has the compat plugin cache"]
fn an_enabled_statusline_takes_the_status_line_from_a_live_lualine() {
    let config = config_home("enabled");
    let scratch = config.parent().unwrap().to_path_buf();
    let committed = fixtures_root().join("heavy");
    let before_committed = snapshot(&committed);
    let before_config = snapshot(&config);

    let engine = session(&config, &scratch);
    wait_for_lualine(&engine.handle);

    let plan = plan(&NativeConfig::all_enabled(), registry::features());
    apply(&engine.handle, &plan);
    // the writer thread preserves order and nvim processes the stream in
    // order, so this request cannot be answered before the notification
    // ahead of it has been applied
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        "0",
        "an enabled statusline must take the surface from a live lualine"
    );

    // a write nvim does report: an ordinary :set, seen by the guard's
    // OptionSet arm and undone before anything else runs
    engine
        .handle
        .eval_str("execute('set laststatus=2')")
        .unwrap();
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        "0",
        "the takeover must survive a plain :set of the option it holds"
    );

    // the write nvim does NOT report: lualine re-runs its own setup() from a
    // ColorScheme autocommand, and autocommands do not nest, so that setup's
    // laststatus write fires no OptionSet at all. A takeover that does not
    // survive this is a takeover that silently lapses the first time a
    // colorscheme is applied.
    install_redraw_witness(&engine.handle);
    engine
        .handle
        .eval_str("execute('colorscheme blue')")
        .unwrap();
    let frames = witnessed_frames(&engine.handle);
    assert!(
        frames.iter().all(|held| held == "0"),
        "no frame may be painted with lualine holding the status line: {frames:?}"
    );
    // and it is still held at rest, not merely at paint time. Read behind a
    // bound rather than instantly: the guard's idle arm runs when nvim next
    // returns to its main loop, which is not ordered against this probe's
    // own reply.
    wait_for(&engine.handle, "&laststatus", "0", hold_timeout());

    assert_eq!(
        before_config,
        snapshot(&config),
        "supersession is runtime only: the session's own config tree may not change"
    );
    assert_eq!(
        before_committed,
        snapshot(&committed),
        "supersession is runtime only: the committed fixture may not change"
    );
}

#[test]
#[ignore = "heavy fixture: run via `task compat-supersede`, which has the compat plugin cache"]
fn a_disabled_statusline_leaves_a_live_lualine_holding_the_surface() {
    let config = config_home("disabled");
    let scratch = config.parent().unwrap().to_path_buf();
    let committed = fixtures_root().join("heavy");
    let before_committed = snapshot(&committed);
    let before_config = snapshot(&config);

    let engine = session(&config, &scratch);
    wait_for_lualine(&engine.handle);

    let cfg = NativeConfig::from_toml_str("[native]\nstatusline = false\n").unwrap();
    let plan = plan(&cfg, registry::features());
    assert!(
        !plan.iter().any(|s| s.feature == "statusline"),
        "a disabled statusline must contribute no takeover"
    );
    apply(&engine.handle, &plan);

    engine
        .handle
        .eval_str("execute('colorscheme blue')")
        .unwrap();
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        LUALINE_LASTSTATUS,
        "a disabled feature must leave the plugin's own setting exactly where the plugin put it"
    );

    assert_eq!(
        before_config,
        snapshot(&config),
        "a disabled feature may not touch the session's config tree either"
    );
    assert_eq!(before_committed, snapshot(&committed));
}

/// Blocks until a plugin in this fixture actually owns `vim.notify`, so what
/// follows is measured against a live patcher rather than against the engine
/// default nobody contested.
///
/// The discriminator is the function's own `source`, which the pin answers
/// `vim/_core/editor` for its default (`docs/vim-notify-takeover-wire-capture.md`);
/// anything else is somebody's replacement. noice installs its router from a
/// deferred load and nvim-notify's setup patches the global directly, so this
/// is a wait rather than an assertion.
fn wait_for_a_plugin_owning_notify(handle: &EngineHandle) {
    wait_for(
        handle,
        "luaeval('package.loaded.notify ~= nil')",
        "true",
        load_timeout(),
    );
    wait_for(
        handle,
        "luaeval('tostring(debug.getinfo(vim.notify).source ~= [[vim/_core/editor]])')",
        "true",
        load_timeout(),
    );
}

/// Whether `marker` reached nvim-notify's own history: the plugin's record of
/// a message it rendered, which outlives the float it drew for it.
///
/// Read from the plugin rather than from the screen because that is the fact
/// a takeover changes -- a message the plugin never saw cannot be in it --
/// and because the float times out on its own while the history does not.
fn in_notify_history(handle: &EngineHandle, marker: &str) -> String {
    handle
        .eval_str(&format!(
            "luaeval('(function() \
             for _, n in ipairs(require([[notify]]).history()) do \
             if table.concat(n.message, [[\\n]]):find([[{marker}]]) then \
             return [[in-history]] end end return [[absent]] end)()')"
        ))
        .unwrap()
}

/// Runs `chunk` as a statement inside nvim and answers `"1"`.
///
/// `eval_str` is a request, so the answer is the receipt that the chunk ran
/// before the next assertion reads what it did. Written with `[[...]]` Lua
/// strings so the whole thing survives as one single-quoted vimscript
/// literal, the same way the redraw witness above does.
fn run_lua(handle: &EngineHandle, chunk: &str) {
    let answer = handle
        .eval_str(&format!("luaeval('(function() {chunk} return 1 end)()')"))
        .unwrap();
    assert_eq!(answer, "1", "the chunk did not run: {chunk}");
}

/// The takeover's own version of the lualine evidence above: `vim.notify` is
/// a Lua global, and assigning one fires no autocommand at all, so
/// `SafeState` is the whole guard rather than the backstop half of one. The
/// plugin that patches it here is the fixture's own, and the message sent
/// afterwards proves where the call actually went -- not merely which
/// function the global holds.
#[test]
#[ignore = "heavy fixture: run via `task compat-supersede`, which has the compat plugin cache"]
fn the_takeover_reasserts_after_a_plugin_repatches_it() {
    let config = config_home("notify");
    let scratch = config.parent().unwrap().to_path_buf();
    let committed = fixtures_root().join("heavy");
    let before_committed = snapshot(&committed);
    let before_config = snapshot(&config);

    let engine = session(&config, &scratch);
    wait_for_a_plugin_owning_notify(&engine.handle);

    // the control, before view touches anything: a vim.notify call in this
    // fixture reaches nvim-notify and is recorded by it. Without this the
    // assertion after the takeover would pass against a fixture whose
    // plugins never handled a notification in the first place
    run_lua(&engine.handle, "vim.notify([[pre-takeover-marker]])");
    wait_for(
        &engine.handle,
        "luaeval('(function() for _, n in ipairs(require([[notify]]).history()) do \
         if table.concat(n.message, [[\\n]]):find([[pre%-takeover%-marker]]) then \
         return [[in-history]] end end return [[absent]] end)()')",
        "in-history",
        hold_timeout(),
    );
    run_lua(&engine.handle, "_G.__plugin_notify = vim.notify");

    let plan = plan(&NativeConfig::all_enabled(), registry::features());
    apply(&engine.handle, &plan);
    assert_eq!(
        engine
            .handle
            .eval_str("luaeval('tostring(vim.notify ~= _G.__plugin_notify)')")
            .unwrap(),
        "true",
        "an enabled notifications must take vim.notify from the live plugin"
    );
    run_lua(&engine.handle, "_G.__view_notify = vim.notify");

    // the re-patch nothing reports. noice installs its router from a
    // deferred load that runs after VimEnter -- after the plan -- and
    // nvim-notify's documented setup is this same assignment from init.lua;
    // a takeover that does not survive it silently lapses on an ordinary
    // config rather than an exotic one
    run_lua(
        &engine.handle,
        "_G.__seen = nil \
         _G.__repatched = function(msg) _G.__seen = msg end \
         vim.notify = _G.__repatched",
    );
    // read behind a bound rather than instantly: the guard runs when nvim
    // next returns to its main loop, which is not ordered against this probe
    wait_for(
        &engine.handle,
        "luaeval('tostring(vim.notify == _G.__view_notify)')",
        "true",
        hold_timeout(),
    );

    // and the message that follows goes to view, not to either claimant
    run_lua(&engine.handle, "vim.notify([[post-takeover-marker]])");
    assert_eq!(
        engine
            .handle
            .eval_str("luaeval('tostring(_G.__seen)')")
            .unwrap(),
        "nil",
        "the function that re-patched vim.notify must not receive the next message"
    );
    assert_eq!(
        in_notify_history(&engine.handle, "post%-takeover%-marker"),
        "absent",
        "a message view owns must not reach nvim-notify, which would draw its own float over view's chrome"
    );
    assert_eq!(
        in_notify_history(&engine.handle, "pre%-takeover%-marker"),
        "in-history",
        "the control message must still be in the plugin's history: supersession is runtime only, and it takes nothing away retroactively"
    );

    assert_eq!(
        before_config,
        snapshot(&config),
        "supersession is runtime only: the session's own config tree may not change"
    );
    assert_eq!(
        before_committed,
        snapshot(&committed),
        "supersession is runtime only: the committed fixture may not change"
    );
}

/// The other half of the pair: with `notifications` off, `vim.notify` is
/// left exactly where the fixture's plugins put it, so a call still reaches
/// nvim-notify and still draws its own float.
#[test]
#[ignore = "heavy fixture: run via `task compat-supersede`, which has the compat plugin cache"]
fn a_disabled_notifications_leaves_vim_notify_with_the_plugin() {
    let config = config_home("notify-disabled");
    let scratch = config.parent().unwrap().to_path_buf();
    let before_config = snapshot(&config);

    let engine = session(&config, &scratch);
    wait_for_a_plugin_owning_notify(&engine.handle);
    run_lua(&engine.handle, "_G.__plugin_notify = vim.notify");

    let cfg = NativeConfig::from_toml_str("[native]\nnotifications = false\n").unwrap();
    let plan = plan(&cfg, registry::features());
    assert!(
        !plan.iter().any(|s| s.rpc == RpcCall::HoldNotify),
        "a disabled notifications must contribute no takeover"
    );
    apply(&engine.handle, &plan);

    assert_eq!(
        engine
            .handle
            .eval_str("luaeval('tostring(vim.notify == _G.__plugin_notify)')")
            .unwrap(),
        "true",
        "a disabled feature must leave vim.notify exactly where the plugin put it"
    );
    run_lua(&engine.handle, "vim.notify([[disabled-marker]])");
    wait_for(
        &engine.handle,
        "luaeval('(function() for _, n in ipairs(require([[notify]]).history()) do \
         if table.concat(n.message, [[\\n]]):find([[disabled%-marker]]) then \
         return [[in-history]] end end return [[absent]] end)()')",
        "in-history",
        hold_timeout(),
    );

    assert_eq!(
        before_config,
        snapshot(&config),
        "a disabled feature may not touch the session's config tree either"
    );
}
