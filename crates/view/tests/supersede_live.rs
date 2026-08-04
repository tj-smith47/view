//! Live-nvim proof of the supersession pair: an enabled feature's takeover
//! actually changes the option in a real session, a disabled one changes
//! nothing, and neither run touches a byte of the user's config.
//!
//! The differential corpus cannot express this. Its entries compare view's
//! own decode against a reference applier over the same key script, and a
//! takeover is not a disagreement between two appliers: it is a session
//! option that either is or is not what the user's `init.lua` set. So the
//! pair is asserted here, against a real spawned nvim reading a real
//! fixture config, over the exact `RpcCall` a plan produces.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use view_core::msg::RpcCall;
use view_core::native::registry;
use view_engine::handle::EngineHandle;
use view_engine::process::{Engine, EngineConfig};
use view_native::config::NativeConfig;
use view_native::supersede::{plan, Supersession};

/// The fixture's own statusline setting: a plain nvim default is already
/// `2`, so a takeover to `0` would still read as a change if the config had
/// silently failed to load. `3` is a value only this file can be
/// responsible for.
const FIXTURE_LASTSTATUS: &str = "3";

/// A fixture config directory holding an `init.lua` that claims the
/// statusline, the way a user running lualine does.
fn fixture(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("view-supersede-live-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("init.lua"),
        format!("vim.opt.laststatus = {FIXTURE_LASTSTATUS}\n"),
    )
    .unwrap();
    dir
}

/// Every file under `dir` as sorted `(name, bytes)` pairs: the snapshot the
/// config-untouched assertion compares. Byte equality, not a digest of the
/// bytes, and the listing rides along so a run that left the existing files
/// alone and wrote a new one beside them fails the same assertion.
fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

/// A live nvim reading `init.lua` from `dir` and nothing else, with a UI
/// already attached: hermetic apart from the one file under test, so the
/// option this asserts on can only have come from that file.
///
/// The attach is not decoration. An `--embed` nvim holds its startup until
/// a UI attaches, so a config-sourced option read before that point reads
/// as nvim's built-in default and every assertion here would be measuring
/// the wrong session. It is also why production applies a plan after
/// `VimEnter` rather than at spawn: before the user's config has run, there
/// is nothing to supersede.
fn session(dir: &Path) -> Engine {
    let mut engine = Engine::spawn(
        EngineConfig::isolated()
            .with_arg("-u")
            .with_arg(dir.join("init.lua")),
    )
    .unwrap();
    let (tx, _rx) = std::sync::mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    engine.handle.ui_attach(80, 24).unwrap();
    engine
}

/// Applies one plan against `handle` the way the runtime's executor does.
///
/// A match rather than a call into the executor: that seam lives inside the
/// bin target and is unreachable from an integration test. The mapping it
/// mirrors is pinned separately by `runtime`'s own
/// `every_supersession_entry_reaches_an_engine_op`.
fn apply(handle: &EngineHandle, plan: &[Supersession]) {
    for entry in plan {
        match &entry.rpc {
            RpcCall::SetOption { name, value } => handle.set_option(name, value).unwrap(),
            other => panic!("a plan entry must ride an option call, got {other:?}"),
        }
    }
}

#[test]
fn an_enabled_statusline_takes_laststatus_over_without_touching_the_config() {
    let dir = fixture("enabled");
    let before = snapshot(&dir);
    let engine = session(&dir);

    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        FIXTURE_LASTSTATUS,
        "the fixture config never took effect, so this test could not observe a takeover"
    );

    let plan = plan(&NativeConfig::all_enabled(), registry::features());
    apply(&engine.handle, &plan);

    // the writer thread preserves order and nvim processes the stream in
    // order, so this request cannot be answered before the notification
    // ahead of it has been applied
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        "0",
        "an enabled statusline must own the status line in the live session"
    );
    let after = snapshot(&dir);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        before, after,
        "supersession is runtime only: the user's config may not change"
    );
}

#[test]
fn a_disabled_statusline_leaves_the_users_own_setting_alone() {
    let dir = fixture("disabled");
    let before = snapshot(&dir);
    let engine = session(&dir);

    let cfg = NativeConfig::from_toml_str("[native]\nstatusline = false\n").unwrap();
    let plan = plan(&cfg, registry::features());
    assert!(
        !plan.iter().any(|s| s.feature == "statusline"),
        "a disabled statusline must contribute no takeover"
    );
    apply(&engine.handle, &plan);

    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        FIXTURE_LASTSTATUS,
        "a disabled feature must leave the user's own setting exactly as their config set it"
    );
    let after = snapshot(&dir);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(before, after);
}

#[test]
fn the_takeover_reverses_when_the_feature_is_turned_off() {
    let dir = fixture("reversal");
    let engine = session(&dir);
    let enabled = plan(&NativeConfig::all_enabled(), registry::features());
    apply(&engine.handle, &enabled);
    assert_eq!(engine.handle.eval_str("&laststatus").unwrap(), "0");
    drop(engine);

    // the reversal a notice promises is exactly this: write the off switch,
    // restart, and the session is back to what the config alone produces --
    // no undo step, because nothing outside the session ever changed
    let off = enabled
        .iter()
        .find(|s| s.feature == "statusline")
        .expect("the statusline must be in an all-enabled plan")
        .reverses_with;
    // parsed exactly as printed, with nothing added around it: the notice
    // tells a user to write this line, so the line itself has to be a legal
    // config on its own
    let cfg = NativeConfig::from_toml_str(off).unwrap();
    let restarted = session(&dir);
    apply(&restarted.handle, &plan(&cfg, registry::features()));

    assert_eq!(
        restarted.handle.eval_str("&laststatus").unwrap(),
        FIXTURE_LASTSTATUS,
        "the registry's own off switch must restore the user's setting"
    );
    std::fs::remove_dir_all(&dir).ok();
}
