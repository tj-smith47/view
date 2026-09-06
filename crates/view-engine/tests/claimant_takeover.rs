//! Live-nvim proof of the one ordering the surface hand-back rests on: a
//! claimant is turned off *before* view's own `vim.notify` goes in.
//!
//! A plugin that took `vim.notify` saved what was there and puts it back
//! when it is disabled (noice's `source/notify.lua` is the shipped case),
//! so the call that clears the surface is also a call that overwrites
//! whatever was installed before it. Sent in the wrong order, the takeover
//! silently hands `vim.notify` to whatever the user's config left there --
//! nothing errors, nothing is logged, and every message the session raises
//! goes to a renderer view believes it superseded.
//!
//! Asked of a real child rather than of the chunk's text: what has to be
//! true is that nvim runs `require('noice').disable()` and that the
//! assignment made after it is the one still standing, and only nvim can be
//! asked that.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rmpv::Value;
use view_engine::process::{Engine, EngineConfig};
use view_test_support::ScratchDir;

/// A config that installs a stand-in for the claimant view supersedes: a
/// loaded `noice` module whose `disable` records the call and restores the
/// `vim.notify` it replaced, which is what the shipped plugin's own
/// `disable` does.
///
/// A stand-in rather than noice itself: this pin is about view's own call
/// order, and it must fail for view's reason on a machine with no plugin
/// manager, no network and no plugin cache. What noice actually does when
/// disabled is `compat/scenarios/noice.toml`'s subject.
///
/// The two functions are kept in a global table so the probe can name which
/// one is standing, rather than inferring it from a side effect: `orig` is
/// what a restore puts back, `claimed` is the claimant's own.
fn config_home(name: &str) -> ScratchDir {
    write_config(name, "")
}

/// [`config_home`] plus a loaded `notify` module, the shape a lazy.nvim
/// config leaves behind: nvim-notify is on `package.loaded` because the
/// claimant pulled it in, and nothing ever assigned `vim.notify` to it.
fn config_home_with_notify(name: &str) -> ScratchDir {
    write_config(
        name,
        "_G.view_pin.notify = function(...) end\n\
         package.loaded['notify'] = _G.view_pin.notify\n",
    )
}

fn write_config(name: &str, extra: &str) -> ScratchDir {
    let dir = ScratchDir::new(&format!("claimant-takeover-{name}")).unwrap();
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "_G.view_pin = {{ orig = vim.notify, disables = 0 }}\n\
             _G.view_pin.claimed = function(...) end\n\
             vim.notify = _G.view_pin.claimed\n\
             package.loaded['noice'] = {{\n\
               disable = function()\n\
                 _G.view_pin.disables = _G.view_pin.disables + 1\n\
                 vim.notify = _G.view_pin.orig\n\
               end,\n\
             }}\n\
             {extra}"
        ),
    )
    .unwrap();
    dir
}

/// The child a real session spawns, reading the fixture config: late
/// attach, so the claimant sets itself up over a startup no UI is attached
/// for, exactly as it does under `view`.
///
/// `-u` behind [`EngineConfig::isolated`]'s own `--clean` rather than a
/// `XDG_CONFIG_HOME` of its own: both write nvim's one "which config"
/// field and the later argument wins (verified against the pinned engine),
/// so the child keeps every hermetic root an isolated spawn owns -- its own
/// state, data and swap directories -- and still sources exactly this
/// fixture.
fn engine(dir: &ScratchDir) -> Engine {
    Engine::spawn(
        EngineConfig::isolated()
            .with_arg("-u")
            .with_arg(dir.join("init.lua"))
            .with_late_attach(120, 40),
    )
    .unwrap()
}

/// Who owns `vim.notify` in `engine` right now: `claimed`, `orig`, or
/// `other` for a function neither the fixture nor its stand-in installed --
/// which, after a takeover, is view's own.
fn notify_owner(engine: &Engine) -> String {
    let answer = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                Value::from(
                    "local pin = _G.view_pin \
                     if vim.notify == pin.claimed then return 'claimed' end \
                     if vim.notify == pin.orig then return 'orig' end \
                     if vim.notify == pin.notify then return 'notify' end \
                     return 'other'",
                ),
                Value::Array(vec![]),
            ],
        )
        .unwrap();
    answer.as_str().unwrap_or_default().to_string()
}

/// How many times the fixture's `disable` has been called.
fn disables(engine: &Engine) -> i64 {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                Value::from("return _G.view_pin.disables"),
                Value::Array(vec![]),
            ],
        )
        .unwrap()
        .as_i64()
        .unwrap_or(-1)
}

#[test]
fn the_hand_back_runs_the_claimants_own_disable_and_view_holds_notify_behind_it() {
    let dir = config_home("ordered");
    let engine = engine(&dir);
    assert_eq!(
        notify_owner(&engine),
        "claimed",
        "the fixture never took vim.notify, so this pin proves nothing"
    );

    engine
        .handle
        .disable_claimants(&["noice".to_string()])
        .unwrap();
    engine.handle.hold_notify().unwrap();

    assert_eq!(
        disables(&engine),
        1,
        "the hand-back must call the loaded claimant's own disable"
    );
    assert_eq!(
        notify_owner(&engine),
        "other",
        "view's notify must be the one left standing behind the restore"
    );
}

/// The same two calls in the other order, which is what the ordering rule
/// costs when it is broken: the claimant's restore lands on top of view's
/// hold and the session is left with the function the config had.
#[test]
fn a_hand_back_sent_behind_the_notify_hold_takes_the_hold_back_off() {
    let dir = config_home("reversed");
    let engine = engine(&dir);

    engine.handle.hold_notify().unwrap();
    assert_eq!(
        notify_owner(&engine),
        "other",
        "the hold itself must land, or the order below proves nothing"
    );
    engine
        .handle
        .disable_claimants(&["noice".to_string()])
        .unwrap();

    assert_eq!(
        notify_owner(&engine),
        "orig",
        "a restore behind the hold is what the takeover's order exists to \
         prevent"
    );
}

/// A module nobody loaded is a plugin that claimed nothing, and requiring
/// it in order to turn it off would load the plugin the session decided
/// against.
#[test]
fn a_claimant_that_never_loaded_is_left_alone() {
    let dir = config_home("absent");
    let engine = engine(&dir);

    engine
        .handle
        .disable_claimants(&["view_pin_absent_module".to_string()])
        .unwrap();
    engine.handle.hold_notify().unwrap();

    assert_eq!(
        notify_owner(&engine),
        "other",
        "an absent module must not take the takeover down with it"
    );
    assert_eq!(disables(&engine), 0);
}

/// A session that handed the messages back still leaves a notify sink: the
/// claimant's restore puts nvim's own echo back, and where the config
/// loaded nvim-notify that is not where its notifications went before view
/// existed. Left alone it is a hit-enter prompt on the first multi-line
/// message of the launch.
///
/// No `hold_notify` here, deliberately: this is the
/// `[native] notifications = false` session, the one shape where nothing
/// of view's follows the hand-back.
#[test]
fn a_hand_back_with_no_hold_behind_it_ends_at_the_configs_own_notify() {
    let dir = config_home_with_notify("sink");
    let engine = engine(&dir);
    assert_eq!(
        notify_owner(&engine),
        "claimed",
        "the fixture never took vim.notify, so this pin proves nothing"
    );

    engine
        .handle
        .disable_claimants(&["noice".to_string()])
        .unwrap();

    assert_eq!(disables(&engine), 1);
    assert_eq!(
        notify_owner(&engine),
        "notify",
        "a hand-back must end in the sink the config would have used \
         without the claimant, never in nvim's echo area"
    );
}

/// And view's own hold still wins when it is issued, so the sink above is
/// never the state a `notifications = true` session ends in.
#[test]
fn view_own_hold_still_outranks_the_sink_the_hand_back_leaves() {
    let dir = config_home_with_notify("sink-held");
    let engine = engine(&dir);

    engine
        .handle
        .disable_claimants(&["noice".to_string()])
        .unwrap();
    engine.handle.hold_notify().unwrap();

    assert_eq!(
        notify_owner(&engine),
        "other",
        "the takeover's own notify is issued behind the hand-back and is \
         the one left standing"
    );
}
