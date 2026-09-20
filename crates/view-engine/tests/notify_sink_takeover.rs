//! Live-nvim proof of the reading view's message surface rests on: who is
//! standing at `vim.notify` when the takeover runs, and who is standing
//! there once the session has settled.
//!
//! A notifier a config installs is a renderer drawing the message area
//! view took, and the only thing that tells one from nvim's own default is
//! a reading taken in the child. Read wrong, view paints its toasts over a
//! float that plugin is already drawing, with nothing in the history to say
//! why.
//!
//! Asked of a real child rather than of the chunk's text: what has to be
//! true is the answer nvim gives, before any UI exists and again after the
//! attach, and only nvim can be asked that.
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
}

/// Runs the real takeover batch against `dir`'s config and collects what
/// its reply routed.
fn takeover_reply(dir: &ScratchDir) -> TakeoverReply {
    let mut engine = engine(dir);
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    engine.handle.takeover(&[]).unwrap();

    let deadline = Instant::now() + common::rpc_deadline();
    let mut reply = TakeoverReply {
        foreign: None,
        startup: None,
    };
    while Instant::now() < deadline && (reply.foreign.is_none() || reply.startup.is_none()) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::NotifySinkRead { foreign }) => reply.foreign = Some(foreign),
            Ok(Msg::StartupMessages { text }) => reply.startup = Some(text),
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

/// A config that leaves `vim.notify` where nvim put it.
fn config_home(name: &str) -> ScratchDir {
    write_config(name, "", "")
}

/// [`config_home`] with a notifier standing at `vim.notify`, which is what
/// a config that sets one up leaves behind.
fn config_home_with_notify(name: &str) -> ScratchDir {
    write_config(name, "", NOTIFY_MODULE)
}

/// A notifier as a plugin's own module leaves it: a callable table rather
/// than a plain function, because `debug.getinfo` raises on one, and a stub
/// that was a plain function let five green pins ride over a reading that
/// could not be taken at all against the real thing.
const NOTIFY_MODULE: &str = "_G.view_pin.notify = setmetatable({}, {\n\
       __call = function(_, msg)\n\
         _G.view_pin.seen = msg\n\
       end,\n\
     })\n\
     vim.notify = _G.view_pin.notify\n";

/// The same callable table, assigned only once the UI is there: a
/// notifier's own documented lazy spec is `event = "UIEnter"`, which is
/// after the takeover has already answered.
const LATE_NOTIFY_SINK: &str = "_G.view_pin.late = setmetatable({}, {\n\
       __call = function() end,\n\
     })\n\
     vim.api.nvim_create_autocmd('UIEnter', {\n\
       callback = function()\n\
         vim.notify = _G.view_pin.late\n\
       end,\n\
     })\n";

fn write_config(name: &str, prologue: &str, extra: &str) -> ScratchDir {
    let dir = ScratchDir::new(&format!("notify-sink-{name}")).unwrap();
    std::fs::write(
        dir.join("init.lua"),
        format!(
            "{prologue}\
             vim.api.nvim_echo({{ {{ 'view-pin-startup' }} }}, true, {{}})\n\
             _G.view_pin = {{ orig = vim.notify }}\n\
             {extra}"
        ),
    )
    .unwrap();
    dir
}

/// The child a real session spawns, reading the fixture config: late
/// attach, so the config sets itself up over a startup no UI is attached
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

/// Who owns `vim.notify` in `engine` right now: `orig`, `notify`, or
/// `other` for a function the fixture never installed -- which, after a
/// hold, is view's own.
fn notify_owner(engine: &Engine) -> String {
    let answer = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                Value::from(
                    "local pin = _G.view_pin \
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

/// The reading itself, over the pair that is the whole discrimination: two
/// configs that differ only in whether a notifier of their own is standing
/// at `vim.notify`. A reading taken of the wrong function answers the same
/// for both.
#[test]
fn the_takeover_reads_which_notifier_is_standing() {
    assert!(
        !takeover_reads_a_foreign_notifier(&config_home("sink-plain")),
        "nvim's own echo is not a notifier view may hand a notice to"
    );
    assert!(
        takeover_reads_a_foreign_notifier(&config_home_with_notify("sink-notify")),
        "a notifier drawing the messages in a float of its own is exactly \
         what view must not paint over"
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
/// The fixture's own notifier stands at `vim.notify` before the hold runs,
/// so a reading taken ahead of the step, or of the wrong function, answers
/// `true` here.
#[test]
fn a_session_holding_its_own_notify_reads_the_sink_as_views() {
    let dir = config_home_with_notify("sink-held");
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

/// A notifier module is a table with a `__call` metamethod, and LuaJIT's
/// `debug.getinfo` raises on one. Raised inside the takeover's single
/// batch, that error degraded the whole reply -- the reading, the claims
/// and the startup messages together -- so a session under that shape went
/// on painting toasts over the float the plugin was already drawing, with
/// nothing in the history to say why.
#[test]
fn a_callable_table_notifier_leaves_the_rest_of_the_takeover_reply_standing() {
    let reply = takeover_reply(&config_home_with_notify("sink-table"));
    assert_eq!(
        reply.foreign,
        Some(true),
        "a callable table is what a notifier's own module is, and a reading \
         that cannot place a value must not call it nvim's own"
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
/// notifier a config installs on `UIEnter` -- a notifier plugin's own
/// documented lazy spec, and the shape the user's config uses -- is never
/// the one it saw. The session reads the sink again at every idle
/// transition until it finds one, which is what this is about.
///
/// The watcher here is the one production arms, from the startup `--cmd`
/// chunk, rather than a second one this test installs: what is being
/// pinned is the reading a shipped session takes.
#[test]
fn a_notifier_installed_at_the_attach_is_read_after_the_takeover() {
    let dir = write_config("late-sink", "", LATE_NOTIFY_SINK);
    let mut engine = engine(&dir);
    let (tx, rx) = mpsc::sync_channel(256);
    let (_pump, cutover) = engine.start_pump(tx);
    engine.handle.takeover(&[]).unwrap();

    // nvim parks inside `VimEnter` until this is answered, which is what
    // orders the readings below: the takeover answers before any idle
    // transition can read the sink. The request is staged when it lands
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
    // nothing forces the turn the watcher answers on: the child reaches its
    // own idle transition once the attach is applied, and the wait below
    // carries the deadline. An nvim-side sleep here would run inside the
    // hook's own `vim.wait` for that attach, ahead of what it waits for
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

/// The registration's `:` reading, carried through the takeover's own reply
/// -- the path a real session takes, where the claim report is a table
/// nested inside the batch's answer rather than the batch's whole result.
///
/// Live rather than decoded from a canned reply, and through the takeover
/// rather than the standalone call, because the two halves that could
/// disagree are nvim's encoding of the nested table and the decoder's
/// reading of it, and only a real child produces the first.
#[test]
fn the_takeover_reply_carries_the_registrations_colon_reading() {
    for (extra, mapped) in [
        ("", false),
        ("vim.keymap.set('n', ':', ':', { silent = true })\n", true),
    ] {
        let dir = write_config(
            if mapped { "colon-mapped" } else { "colon-free" },
            "",
            extra,
        );
        let mut engine = engine(&dir);
        let (tx, rx) = mpsc::sync_channel(64);
        let (_pump, _cutover) = engine.start_pump(tx);
        let channel_id = engine.api_info.channel_id;
        engine
            .handle
            .takeover(&[TakeoverStep::RegisterMappings {
                specs: view_core::native::mappings::default_maps().to_vec(),
                channel_id,
            }])
            .unwrap();

        let deadline = Instant::now() + common::rpc_deadline();
        let mut read = None;
        while Instant::now() < deadline && read.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(Msg::MappingsClaimed {
                    claimed,
                    colon_mapped,
                }) => read = Some((claimed, colon_mapped)),
                Ok(_) => {}
                Err(_) => break,
            }
        }

        let (claimed, colon_mapped) =
            read.expect("the takeover must route the registration's claim report");
        assert_eq!(
            claimed.len(),
            view_core::native::mappings::default_maps().len(),
            "the claims must survive the nesting the reading was added inside"
        );
        assert_eq!(
            colon_mapped, mapped,
            "the takeover's nested reply lost the `:` reading"
        );
    }
}
