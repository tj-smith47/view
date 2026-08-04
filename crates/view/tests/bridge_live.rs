//! Live-nvim proof of the `view_bridge` autocmd group: a real
//! `:colorscheme` in a real session crosses back as
//! `Msg::ColorSchemeChanged`, a scheme set by the user's own config is
//! observed at all (which is what registering before `nvim_ui_attach` buys),
//! and the colors that follow the announcement really do re-derive the
//! chrome a painter reads.
//!
//! The differential corpus cannot express any of this. Both of its legs
//! consume the same event stream, so a bridge that registered nothing would
//! leave the two sides agreeing exactly as before; the bridge's whole
//! observable is a notification that exists outside the redraw stream.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use view_core::model::Model;
use view_core::msg::{Effect, Msg, RpcCall};
use view_core::theme::{ChromeGroup, Theme};
use view_core::update::update;
use view_engine::process::{Engine, EngineConfig};

/// How long a bridge notification is waited for. Generous because a cold
/// nvim spawn on a loaded box is the slow part; a healthy session answers in
/// milliseconds.
const ARRIVAL: Duration = Duration::from_secs(10);

/// The scheme every case switches to. A builtin, so no fixture has to ship
/// one, and visibly different from the startup default in the chrome groups
/// this asserts on.
const SCHEME: &str = "blue";

/// How long the batch behind an announcement is drained for once the
/// announcement itself has arrived. Short on purpose: the eval barrier in
/// [`Session::switch_to`] has already forced nvim back to its main loop, so
/// everything the switch produced was written ahead of that reply.
const SETTLE: Duration = Duration::from_millis(400);

/// A fixture config directory whose `init.lua` runs `extra` once nvim
/// sources it.
fn fixture(name: &str, extra: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("view-bridge-live-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("init.lua"), extra).unwrap();
    dir
}

/// A live nvim reading the fixture's `init.lua` and nothing else, with the
/// bridge registered and a UI attached, so what crosses back from a
/// colorscheme change is observable as the `Msg` the runtime loop would see.
///
/// The registration order here is production's, not a convenience: the
/// bridge is registered BEFORE `ui_attach`, which is the window in which
/// nvim services requests on this connection but has not yet begun sourcing
/// the user's config.
struct Session {
    engine: Engine,
    pump: view_engine::DamagePump,
    rx: Receiver<Msg>,
    dir: PathBuf,
}

impl Session {
    fn start(name: &str, init: &str) -> Self {
        let dir = fixture(name, init);
        let mut engine = Engine::spawn(
            EngineConfig::isolated()
                .with_arg("-u")
                .with_arg(dir.join("init.lua")),
        )
        .unwrap();
        let (tx, rx): (SyncSender<Msg>, Receiver<Msg>) = std::sync::mpsc::sync_channel(1024);
        let (pump, _cutover) = engine.start_pump(tx);
        engine
            .handle
            .register_bridge(engine.api_info.channel_id)
            .unwrap();
        engine.handle.ui_attach(80, 24).unwrap();
        Self {
            engine,
            pump,
            rx,
            dir,
        }
    }

    fn eval(&self, expr: &str) -> String {
        self.engine.handle.eval_str(expr).unwrap()
    }

    /// Switches colorscheme the way a user would, and waits for nvim to be
    /// back at its main loop -- so anything the switch notified is already
    /// on the wire ahead of this reply.
    fn switch_to(&self, scheme: &str) {
        self.eval(&format!("execute('colorscheme {scheme}')"));
    }

    /// The first `Msg` the pump delivers that `want` answers for, within
    /// `budget`, with every message seen along the way applied to `model`
    /// exactly as the runtime loop would apply it.
    ///
    /// Folding through `update` rather than filtering past it is the point:
    /// the highlight state a switch produces arrives as ordinary redraw
    /// traffic, and a test that only watched for the announcement could not
    /// tell whether the colors behind it ever landed.
    fn wait_for<T>(
        &self,
        model: &mut Model,
        budget: Duration,
        want: impl Fn(&Msg) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + budget;
        let mut found = None;
        loop {
            // once the announcement is in hand the wait shortens to the
            // batch behind it: the colors follow the message, so returning
            // on the message alone would sample the theme the user left
            let left = if found.is_some() {
                SETTLE
            } else {
                deadline.saturating_duration_since(Instant::now())
            };
            match self.rx.recv_timeout(left) {
                Ok(msg) => {
                    // the pump answers with a token, exactly as it does in
                    // production; the loop is what drains it into the batch
                    let msg = match msg {
                        Msg::RedrawReady => Msg::Redraw(self.pump.take_damage()),
                        other => other,
                    };
                    if found.is_none() {
                        found = want(&msg);
                    }
                    for eff in update(model, msg) {
                        self.carry_out(eff);
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return found,
            }
        }
    }

    /// Runs the one effect this harness has to honour for the model's theme
    /// to settle the way production's does: the background probe that
    /// disambiguates a `default_colors_set` whose background is zero.
    fn carry_out(&self, eff: Effect) {
        if let Effect::Rpc(RpcCall::GetDefaultHl { generation }) = eff {
            self.engine.handle.probe_default_hl(generation).unwrap();
        }
    }

    /// The name carried by the next colorscheme announcement, with the
    /// traffic around it applied to `model`.
    fn switch_name(&self, model: &mut Model) -> Option<String> {
        self.wait_for(model, ARRIVAL, |msg| match msg {
            Msg::ColorSchemeChanged { name } => Some(name.clone()),
            _ => None,
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// A model at the size the session attaches at, so the redraw traffic it is
/// fed describes the grid it has.
fn model() -> Model {
    Model::with_term_size(80, 24)
}

/// Every chrome group's resolved colors, the whole surface a painter reads
/// per frame.
fn chrome(model: &Model) -> Vec<(ChromeGroup, view_core::theme::ResolvedStyle)> {
    let theme = Theme::from_hl(model.engine.hl());
    ChromeGroup::ALL
        .into_iter()
        .map(|group| (group, theme.chrome(group)))
        .collect()
}

/// Every event the `view_bridge` group registers, as the chunk asks nvim
/// for them. Registration is a notify, so nvim reports nothing back about
/// whether the chunk ran to completion.
const TRIGGERS: [&str; 5] = [
    "ColorScheme",
    "DiagnosticChanged",
    "BufEnter",
    "DirChanged",
    "FocusGained",
];

/// Registration is a notify over a chunk that creates three autocmds in
/// sequence, so a chunk aborting partway -- a later `nvim_create_autocmd`
/// rejected, a typo in an event name -- leaves the earlier registrations
/// working and the later ones simply absent, with no error anywhere and no
/// failing test: `ColorScheme` keeps announcing while a consumer of one of
/// the others goes quietly stale forever. Asserting the substrings appear in
/// the chunk cannot see that, because the chunk is not what nvim ended up
/// holding. This asks nvim.
#[test]
fn every_trigger_the_chunk_asks_for_is_registered_with_nvim() {
    let session = Session::start("registered", "");

    let registered =
        session.eval("join(map(nvim_get_autocmds({'group': 'view_bridge'}), 'v:val.event'), ',')");
    let held: Vec<&str> = registered.split(',').collect();

    for trigger in TRIGGERS {
        assert!(
            held.contains(&trigger),
            "the view_bridge group is missing {trigger}: nvim holds {held:?}"
        );
    }
}

/// The bridge's own observable, end to end: a real `:colorscheme` in a live
/// session becomes a typed message on the runtime's channel. Nothing in the
/// redraw stream states this -- a highlight batch looks identical whether
/// one plugin redefined a group or the whole scheme changed -- so this is
/// the assertion a missing registration fails.
#[test]
fn a_live_colorscheme_change_arrives_as_a_typed_message() {
    let session = Session::start("switch", "");
    let mut m = model();

    session.switch_to(SCHEME);

    assert_eq!(
        session.switch_name(&mut m).as_deref(),
        Some(SCHEME),
        "the ColorScheme autocmd must notify view with the scheme it switched to"
    );
    assert_eq!(
        session.eval("g:colors_name"),
        SCHEME,
        "the session must actually be wearing the scheme it announced"
    );
}

/// The reason the registration goes before `nvim_ui_attach`: a user whose
/// config sets a colorscheme fires `ColorScheme` while nvim sources it, and
/// nvim cannot begin sourcing until attach returns. A registration made
/// after attach misses that switch outright, which is exactly the case a
/// cold-start theme cache exists to serve.
#[test]
fn a_colorscheme_set_by_the_users_own_config_is_still_observed() {
    let session = Session::start("from-config", &format!("vim.cmd.colorscheme('{SCHEME}')\n"));
    let mut m = model();

    assert_eq!(
        session.switch_name(&mut m).as_deref(),
        Some(SCHEME),
        "a scheme set during config sourcing must still reach view"
    );
}

/// The switch has to reach the colors a painter reads, not just the message
/// log. `view-tui` derives its chrome from the live highlight table every
/// frame, so this asserts against the same `Theme::chrome` accessor the
/// painters call, over the traffic a real switch produced.
#[test]
fn the_colors_a_painter_reads_change_when_the_scheme_does() {
    let session = Session::start("re-derive", "");
    let mut m = model();

    // the startup batch is what establishes the chrome the switch has to
    // move: sampling before it lands would compare a scheme against
    // built-in defaults rather than against the scheme it replaced
    session.switch_to("default");
    assert!(
        session.switch_name(&mut m).is_some(),
        "the baseline switch must be observed before its colors can be sampled"
    );
    let before = chrome(&m);

    session.switch_to(SCHEME);
    assert_eq!(session.switch_name(&mut m).as_deref(), Some(SCHEME));
    let after = chrome(&m);

    let moved: Vec<ChromeGroup> = before
        .iter()
        .zip(&after)
        .filter(|((_, was), (_, now))| was != now)
        .map(|((group, _), _)| *group)
        .collect();
    assert!(
        !moved.is_empty(),
        "no chrome group's colors moved across a real colorscheme change: before={before:?} after={after:?}"
    );
}
