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
use view_core::msg::{EngineRequest, Msg, ReplyValue, TakeoverStep};
use view_engine::process::{Engine, EngineConfig};
use view_test_support::ScratchDir;

/// What the takeover's one reply delivered, as the messages a real pump
/// routes out of it: its reading of `vim.notify` and whatever nvim said
/// while it was starting.
///
/// Both, rather than the reading alone, because they arrive from the same
/// reply and a raise inside the batch loses all of it -- a pin that watched
/// only the reading could not tell a wrong answer from a lost one.
struct TakeoverReply {
    foreign: Option<bool>,
    startup: Option<String>,
    /// The hand-back step's own answer: the modules whose `disable` ran.
    handed_back: Option<Vec<String>>,
}

/// Runs the real takeover batch against `dir`'s config and collects what
/// its reply routed.
fn takeover_reply(dir: &ScratchDir) -> TakeoverReply {
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
    let mut reply = TakeoverReply {
        foreign: None,
        startup: None,
        handed_back: None,
    };
    while Instant::now() < deadline
        && (reply.foreign.is_none() || reply.startup.is_none() || reply.handed_back.is_none())
    {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::NotifySinkRead { foreign }) => reply.foreign = Some(foreign),
            Ok(Msg::StartupMessages { text }) => reply.startup = Some(text),
            Ok(Msg::ClaimantsHandedBack { modules }) => reply.handed_back = Some(modules),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    reply
}

/// Whether the takeover's reply says a notifier other than nvim's own is
/// standing at `vim.notify`, run as the real batch against `dir`'s config.
fn takeover_reads_a_foreign_notifier(dir: &ScratchDir) -> bool {
    takeover_reply(dir)
        .foreign
        .expect("the takeover reply must carry a reading of vim.notify")
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
///
/// A callable table rather than a plain function, because that is what
/// nvim-notify's module actually is: `debug.getinfo` raises on one, and a
/// stub that was a plain function let five green pins ride over a reading
/// that could not be taken at all against the real plugin.
const NOTIFY_MODULE: &str = "_G.view_pin.notify = setmetatable({}, {\n\
       __call = function(_, msg)\n\
         _G.view_pin.seen = msg\n\
       end,\n\
     })\n\
     package.loaded['notify'] = _G.view_pin.notify\n";

/// The same callable table, never placed on `package.loaded` and assigned
/// only once the UI is there: nvim-notify's own documented lazy spec is
/// `event = "UIEnter"`, which is after the takeover has already answered.
const LATE_NOTIFY_SINK: &str = "_G.view_pin.late = setmetatable({}, {\n\
       __call = function() end,\n\
     })\n\
     vim.api.nvim_create_autocmd('UIEnter', {\n\
       callback = function()\n\
         vim.notify = _G.view_pin.late\n\
       end,\n\
     })\n";

fn write_config(name: &str, prologue: &str, extra: &str) -> ScratchDir {
    let dir = ScratchDir::new(&format!("claimant-takeover-{name}")).unwrap();
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "{prologue}\
             vim.api.nvim_echo({{ {{ 'view-pin-startup' }} }}, true, {{}})\n\
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

/// A session that owns notifications reads its own sink as view's.
///
/// `HoldNotify` is the step every owning session sends, and what it leaves
/// at `vim.notify` is view's own function -- not the engine's default, and
/// so not something a source comparison can place. Read as foreign, the
/// line a triage read starts from says the session is speaking through a
/// plugin's notifier when it is speaking through none.
///
/// The fixture's own `claimed` function stands at `vim.notify` before the
/// hold runs, so a reading taken ahead of the step, or of the wrong
/// function, answers `true` here.
#[test]
fn a_session_holding_its_own_notify_reads_the_sink_as_views() {
    let dir = config_home("sink-held");
    let mut engine = engine(&dir);
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    engine.handle.takeover(&[TakeoverStep::HoldNotify]).unwrap();

    let deadline = Instant::now() + common::rpc_deadline();
    let mut foreign = None;
    while Instant::now() < deadline && foreign.is_none() {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Msg::NotifySinkRead { foreign: read }) => foreign = Some(read),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert_eq!(
        foreign,
        Some(false),
        "the function view itself installed is view's sink, never a foreign one"
    );
    assert_eq!(
        notify_owner(&engine),
        "other",
        "the hold must actually be standing for that reading to be about it"
    );
}

/// nvim-notify's module is a table with a `__call` metamethod, and LuaJIT's
/// `debug.getinfo` raises on one. Raised inside the takeover's single
/// batch, that error degraded the whole reply -- the reading, the claims
/// and the startup messages together -- so a session under the shape the
/// hand-back most often leaves went on painting toasts over the float the
/// plugin was already drawing, with nothing in the history to say why.
#[test]
fn a_callable_table_notifier_leaves_the_rest_of_the_takeover_reply_standing() {
    let reply = takeover_reply(&config_home_with_notify("sink-table"));
    assert_eq!(
        reply.foreign,
        Some(true),
        "the re-point put nvim-notify's own callable table at vim.notify, \
         and a reading that cannot place a value must not call it nvim's own"
    );
    assert!(
        reply
            .startup
            .as_deref()
            .unwrap_or_default()
            .contains("view-pin-startup"),
        "what nvim said at startup rides the same reply, and a raise inside \
         the batch took it down with the reading: {:?}",
        reply.startup
    );
}

/// The reading the takeover takes is taken before any UI exists, so a
/// notifier a config installs on `UIEnter` -- nvim-notify's own documented
/// lazy spec, and the shape the user's config uses -- is never the one it
/// saw. The claimant probe already re-asks a question whose answer moves at
/// every idle transition, and this is the other one.
///
/// The probe here is the one production arms, from the startup `--cmd`
/// chunk, rather than a second one this test installs: what is being
/// pinned is the reading a shipped session takes.
#[test]
fn a_notifier_installed_at_the_attach_is_read_after_the_takeover() {
    let dir = write_config("late-sink", "", LATE_NOTIFY_SINK);
    let mut engine = engine(&dir);
    let (tx, rx) = mpsc::sync_channel(256);
    let (_pump, cutover) = engine.start_pump(tx);
    engine
        .handle
        .takeover(&[TakeoverStep::DisableClaimants {
            modules: vec!["noice".to_string()],
        }])
        .unwrap();

    // nvim parks inside `VimEnter` until this is answered, which is what
    // orders the readings below: the takeover restores `vim.notify` before
    // any idle transition can read it. The request is staged when it lands
    // ahead of the sink, so the presink is read for it first -- a test that
    // waited only on the channel left nvim parked for its own lifetime
    let mut entered = cutover.presink.into_iter().find_map(|msg| match msg {
        Msg::EngineRequest(EngineRequest::VimEnter { token }) => Some(token),
        _ => None,
    });
    let mut first = None;
    let deadline = Instant::now() + common::rpc_deadline_for(3);
    while Instant::now() < deadline && (entered.is_none() || first.is_none()) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::NotifySinkRead { foreign }) if first.is_none() => first = Some(foreign),
            Ok(Msg::EngineRequest(EngineRequest::VimEnter { token })) => entered = Some(token),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    assert_eq!(
        first,
        Some(false),
        "the takeover's own reading must land on nvim's echo first, or the \
         reading below is not the second one"
    );
    if let Some(token) = entered.take() {
        engine.handle.reply(token, ReplyValue::Nil).unwrap();
    }

    engine
        .handle
        .ui_attach(120, 40, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    // the probe answers at an idle transition, which a reply to a request
    // is not: this puts nvim back through its own main loop
    engine.handle.eval_str("execute('sleep 100m')").unwrap();
    assert_eq!(
        wait_for_sink_read(&rx, |foreign| foreign),
        Some(true),
        "the notifier UIEnter installed is one only a reading taken after \
         the attach can see"
    );
}

/// The first `Msg::NotifySinkRead` `want` accepts, within three engine
/// round trips, with everything else the pump delivers drained past.
fn wait_for_sink_read(rx: &mpsc::Receiver<Msg>, want: impl Fn(bool) -> bool) -> Option<bool> {
    let deadline = Instant::now() + common::rpc_deadline_for(3);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::NotifySinkRead { foreign }) if want(foreign) => return Some(foreign),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// The hand-back's own answer, carried out of the same reply: the module
/// whose `disable` actually ran, named.
///
/// Nothing else in a session can tell a plugin that turned itself off from
/// one that never received the call. A disabled module is still on
/// `package.loaded`, so the later claimant probe reads both cases
/// identically, and the notice worded off that reading told every user the
/// plugin was "still loaded" -- a success reported as a failure.
#[test]
fn the_takeover_reply_names_the_module_whose_disable_ran() {
    let dir = config_home("handed-back");
    let handed_back = takeover_reply(&dir)
        .handed_back
        .expect("the takeover reply must carry the hand-back's own answer");
    assert_eq!(
        handed_back,
        vec!["noice".to_string()],
        "the step ran this module's own disable, so its answer names it"
    );
}
