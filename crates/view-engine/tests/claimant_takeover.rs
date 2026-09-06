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

mod common;

use rmpv::Value;
use std::sync::mpsc;
use std::time::Instant;
use view_core::msg::{Msg, TakeoverStep};
use view_engine::process::{Engine, EngineConfig};
use view_test_support::ScratchDir;

/// Whether the takeover's reply says a notifier other than nvim's own is
/// standing at `vim.notify`, run as the real batch against `dir`'s config.
fn takeover_reads_a_foreign_notifier(dir: &ScratchDir) -> bool {
    let mut engine = engine(dir);
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    engine
        .handle
        .takeover(&[TakeoverStep::DisableClaimants {
            modules: vec!["noice".to_string()],
        }])
        .unwrap();

    let deadline = Instant::now() + common::rpc_deadline();
    let mut read = None;
    while Instant::now() < deadline && read.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::NotifySinkRead { foreign }) => read = Some(foreign),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    read.expect("the takeover reply must carry a reading of vim.notify")
}

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
    write_config(name, "", "")
}

/// [`config_home`] plus a loaded `notify` module, the shape a lazy.nvim
/// config leaves behind: nvim-notify is on `package.loaded` because the
/// claimant pulled it in, and nothing ever assigned `vim.notify` to it.
fn config_home_with_notify(name: &str) -> ScratchDir {
    write_config(name, "", NOTIFY_MODULE)
}

/// [`config_home_with_notify`], except that the function the claimant saved
/// -- and so the one its `disable` restores -- is the config's own: the
/// `prologue` assigns `vim.notify` before the claimant ever runs, which is
/// a user who chose their sink rather than one who left nvim's.
fn config_home_with_custom_sink(name: &str) -> ScratchDir {
    write_config(
        name,
        "_G.view_pin_custom = function(...) end\n\
         vim.notify = _G.view_pin_custom\n",
        NOTIFY_MODULE,
    )
}

/// A loaded nvim-notify, as the claimant's own `require` leaves it. It
/// records what it was told so a notice raised through the standing sink
/// can be read back by the one that received it.
const NOTIFY_MODULE: &str = "_G.view_pin.notify = function(msg)\n\
       _G.view_pin.seen = msg\n\
     end\n\
     package.loaded['notify'] = _G.view_pin.notify\n";

fn write_config(name: &str, prologue: &str, extra: &str) -> ScratchDir {
    let dir = ScratchDir::new(&format!("claimant-takeover-{name}")).unwrap();
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "{prologue}\
             _G.view_pin = {{ orig = vim.notify, disables = 0 }}\n\
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

/// The sink repair is for the restore that lands on nvim's echo, and for no
/// other: a claimant that put the config's own `vim.notify` back has left
/// the session exactly where it would have been without the plugin, and
/// re-pointing that at nvim-notify would be view overriding the config it
/// just handed the surface back to.
#[test]
fn a_restore_that_leaves_the_configs_own_notify_is_kept_over_nvim_notify() {
    let dir = config_home_with_custom_sink("sink-edge");
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
        "orig",
        "the restore put the config's own function back, and nvim-notify \
         being loaded is not a reason to take it away"
    );
}

/// The other half of the hand-back: a session that handed the messages
/// surface back still has its own notices to place, and they go to the sink
/// the hand-back left standing rather than onto a toast stack painted over
/// the notifier that draws them.
#[test]
fn a_notice_raised_after_the_hand_back_reaches_the_sink_it_left() {
    let dir = config_home_with_notify("raised");
    let engine = engine(&dir);

    engine
        .handle
        .disable_claimants(&["noice".to_string()])
        .unwrap();
    engine
        .handle
        .raise_notice("view: took the statusline over")
        .unwrap();

    let seen = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![Value::from("return _G.view_pin.seen"), Value::Array(vec![])],
        )
        .unwrap();
    assert_eq!(
        seen.as_str(),
        Some("view: took the statusline over"),
        "the notice must arrive at the function `vim.notify` names after \
         the hand-back, whatever that function is"
    );
}

/// What the takeover reads at `vim.notify` once every one of its steps has
/// run, which is what decides whether view speaks its own notices or paints
/// them.
///
/// The pair is the whole discrimination: both configs load the claimant and
/// both have it restore nvim's own default, and they differ only in whether
/// nvim-notify is on `package.loaded` for the re-point to find. A reading
/// taken before the steps -- or of the wrong function -- answers the same
/// for both.
#[test]
fn the_takeover_reads_which_notifier_its_own_hand_back_left_standing() {
    assert!(
        !takeover_reads_a_foreign_notifier(&config_home("sink-plain")),
        "a restore that landed on nvim's own echo is not a notifier view          may hand a notice to"
    );
    assert!(
        takeover_reads_a_foreign_notifier(&config_home_with_notify("sink-notify")),
        "the re-point put nvim-notify there, and a float drawing the          messages is exactly what view must not paint over"
    );
}
