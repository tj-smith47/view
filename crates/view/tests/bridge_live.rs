//! Live-nvim proof of the `view_bridge` autocmd group: a real
//! `:colorscheme` in a real session crosses back as
//! `Msg::ColorSchemeChanged`, a scheme set by the user's own config is
//! observed at all (which is what registering before `nvim_ui_attach` buys),
//! and the colors that follow the announcement really do re-derive the
//! chrome a painter reads. `[ui] theme`'s own call rides the same method in
//! both directions, so its two outcomes -- the session ends up wearing the
//! named scheme, or nvim refuses a name it has no file for -- are proven
//! here beside them.
//!
//! The differential corpus cannot express any of this. Both of its legs
//! consume the same event stream, so a bridge that registered nothing would
//! leave the two sides agreeing exactly as before; the bridge's whole
//! observable is a notification that exists outside the redraw stream.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use view_core::model::Model;
use view_core::msg::{Effect, Msg, RpcCall};
use view_core::theme::{ChromeGroup, Theme};
use view_core::update::update;
use view_engine::process::Engine;
use view_engine::DamagePump;
use view_test_support::ScratchDir;

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
    pump: DamagePump,
    rx: Receiver<Msg>,
    // held only for its `Drop`: the scratch directory outlives every use
    // of `dir` above, which is why nothing in this file reads it back
    _dir: ScratchDir,
}

impl Session {
    fn start(name: &str, init: &str) -> Self {
        let dir = common::fixture(&format!("bridge-live-{name}"), init);
        let cfg = common::isolated_reading(&dir.join("init.lua"));
        let (engine, pump, rx) = common::spawn_with_pump(cfg, 1024);
        engine
            .handle
            .register_bridge(engine.api_info.channel_id)
            .unwrap();
        engine
            .handle
            .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
            .unwrap();
        Self {
            engine,
            pump,
            rx,
            _dir: dir,
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

    /// Every float filetype the bridge reports within `budget`, in arrival
    /// order. Not folded through `update`: what this observes is the wire
    /// the watcher writes, and the model would only add a second thing that
    /// could be wrong.
    fn float_filetypes(&self, budget: Duration) -> Vec<String> {
        let deadline = Instant::now() + budget;
        let mut seen = Vec::new();
        while let Ok(msg) = self
            .rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if let Msg::FloatObserved(float) = msg {
                seen.push(float.filetype);
            }
        }
        seen
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
const TRIGGERS: [&str; 14] = [
    "ColorScheme",
    "DiagnosticChanged",
    "BufEnter",
    "DirChanged",
    "FocusGained",
    "CmdlineEnter",
    "CmdlineChanged",
    "ModeChanged",
    "CursorHold",
    "CursorHoldI",
    "WinEnter",
    "WinClosed",
    "OptionSet",
    "VimEnter",
];

/// Registration is a notify over a chunk that creates its autocmds in
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

/// A `ttimeoutlen` the user's own `init.lua` sets reaches the reader.
///
/// This is the ordinary way the option is set, and it is the one way
/// neither of the other two relays covers: the bridge is registered before
/// nvim sources the config, and `OptionSet` does not fire for what the
/// config assigns during startup. Only the `VimEnter` relay carries it, and
/// a session that never heard it would read every unfinished key code at
/// nvim's default however long its user had asked for.
#[test]
fn an_escape_timing_set_by_the_users_own_config_reaches_the_reader() {
    let session = Session::start("ttimeout", "vim.o.ttimeoutlen = 300\n");
    let mut m = model();
    let tuned = Duration::from_millis(300);
    // recorded rather than matched on the first arrival: the relay at
    // registration necessarily carries the pre-config value, so what this
    // asserts is that the tuned one follows it, and the record is what
    // names the value nvim did relay when it does not
    let heard = std::cell::RefCell::new(Vec::new());
    let armed = session.wait_for(&mut m, ARRIVAL, |msg| match msg {
        Msg::EscapeTimeout(within) => {
            heard.borrow_mut().push(*within);
            (*within == tuned).then_some(*within)
        }
        _ => None,
    });
    assert_eq!(
        armed,
        Some(tuned),
        "the config's own ttimeoutlen never reached the reader; nvim relayed {:?}",
        heard.borrow()
    );
}

/// `ttimeout` off is no wait at all at the reader.
///
/// nvim's two ways of saying "read a half-arrived key code at once" --
/// `nottimeout`, and a negative `ttimeoutlen` -- are resolved in the
/// chunk's own Lua, so no Rust-side test can reach them: what a session
/// relays for a config that switched the wait off is only observable from
/// a session that has one. A reader that took the raw `ttimeoutlen` here
/// would hold every unfinished run for a wait its user turned off.
#[test]
fn an_escape_timing_switched_off_reaches_the_reader_as_no_wait() {
    let session = Session::start("nottimeout", "vim.o.ttimeout = false\n");
    let mut m = model();
    // recorded rather than matched on the first arrival: the relay at
    // registration carries the pre-config wait, so what this asserts is
    // that the zero follows it, and the record names what nvim did relay
    // when it does not
    let heard = std::cell::RefCell::new(Vec::new());
    let armed = session.wait_for(&mut m, ARRIVAL, |msg| match msg {
        Msg::EscapeTimeout(within) => {
            heard.borrow_mut().push(*within);
            (*within == Duration::ZERO).then_some(*within)
        }
        _ => None,
    });
    assert_eq!(
        armed,
        Some(Duration::ZERO),
        "`nottimeout` never reached the reader as a zero wait; nvim relayed {:?}",
        heard.borrow()
    );
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

/// What `[ui] theme` actually does, against a real engine: the call view
/// issues at `VimEnter` puts nvim in the named scheme, and view learns the
/// name through the same autocmd a user's own `:colorscheme` fires. There is
/// one palette in the session and this is the proof nvim owns it -- nothing
/// view holds decided `g:colors_name`.
#[test]
fn a_named_theme_puts_the_live_session_in_that_colorscheme() {
    let session = Session::start("ui-theme-named", "");
    let mut m = model();

    session
        .engine
        .handle
        .colorscheme(SCHEME)
        .expect("the call `[ui] theme` issues must reach a live engine");

    assert_eq!(
        session.switch_name(&mut m).as_deref(),
        Some(SCHEME),
        "the scheme view asked for must be announced back like any other switch"
    );
    assert_eq!(
        session.eval("g:colors_name"),
        SCHEME,
        "and the session must actually be wearing it"
    );
}

/// The other half, and the one no unit test can settle: whether nvim's own
/// refusal really is a refusal this chunk catches. A name nvim cannot find
/// comes back as the typed message -- not as an anonymous `E185` in a
/// session that is still sourcing plugins, and not as a switch that never
/// happened.
#[test]
fn a_theme_nvim_cannot_find_reports_itself_by_name() {
    let session = Session::start("ui-theme-missing", "");
    let mut m = model();
    let missing = "view-nonexistent-scheme";

    session.engine.handle.colorscheme(missing).unwrap();

    let reported = session.wait_for(&mut m, ARRIVAL, |msg| match msg {
        Msg::ColorSchemeMissing { name } => Some(name.clone()),
        _ => None,
    });
    assert_eq!(
        reported.as_deref(),
        Some(missing),
        "a scheme nvim has no runtime file for must report itself by name"
    );
    assert_ne!(
        // `get`, not the bare variable: a session that never loaded a scheme
        // has no `g:colors_name` at all, and reading one directly is `E121`
        // -- which would make this assertion fail on the very outcome it is
        // asserting for
        session.eval("get(g:, 'colors_name', '')"),
        missing,
        "and nothing may claim the session is wearing a scheme it refused"
    );
}

/// A float opener and a `CursorHold` that only ever fires when this test
/// asks for one: an `updatetime` this large means no idle timer can arm a
/// scan the test did not schedule, which is what keeps the round below
/// measuring the chunk's own state machine.
const FLOAT_INIT: &str = "vim.o.updatetime = 100000\n\
function _G.view_test_float(ft)\n\
  local buf = vim.api.nvim_create_buf(false, true)\n\
  vim.bo[buf].filetype = ft\n\
  vim.api.nvim_open_win(buf, false, { relative = 'editor', row = 1,\n\
    col = 1, width = 8, height = 2, style = 'minimal' })\n\
end\n";

/// The trailing edge of the throttle, live. The leading edge alone loses the
/// last keystroke of a burst: the arming events inside the running window
/// are absorbed, and the float the absorbed one summons appears 61 ms later
/// -- after the scheduled scan has already walked the windows and found
/// nothing. Nothing re-arms, so a user who types `:e pre` and stops to read
/// the menu is never told the menu is covering the command line.
///
/// The compat harness cannot catch this: `Step::Send` waits out 200 ms of
/// screen silence, so every key it types is spaced wider than the throttle
/// window -- the case that always worked.
///
/// The round is only evidence if the leading scan really did run before the
/// second float existed, which the `view_test_a` sighting ahead of the first
/// `view_test_b` one is what proves. A host stalled long enough to run that
/// scan late produces a round where they arrive together; that round is
/// discarded and the dispatch repeated with a wider gap, rather than read as
/// a pass.
#[test]
fn a_float_opened_after_the_leading_scan_is_still_reported() {
    let session = Session::start("trailing-scan", FLOAT_INIT);
    session.eval("execute('lua _G.view_test_float(\"view_test_a\")')");

    let mut rounds = Vec::new();
    for gap in [250_u64, 500, 1000] {
        let _ =
            session.float_filetypes(view_test_support::host_deadline(Duration::from_millis(300)));
        // the leading edge, then an event inside its window with nothing for
        // that scan to find, then the float it stands for -- opened from
        // inside nvim so the gap is measured against the same loop the
        // throttle's own timer runs on
        session.eval("execute('doautocmd CursorHold')");
        session.eval("execute('doautocmd CursorHold')");
        session.eval(&format!(
            "execute('lua vim.defer_fn(function() \
             _G.view_test_float(\"view_test_b\") end, {gap})')"
        ));

        // scaled, because what the round needs is for nvim's deferred float
        // and the scan behind it to have happened, and a host that stalled
        // this process stalled that engine with it: an unscaled window turns
        // a slow round into a report that the trailing scan never ran
        let seen = session.float_filetypes(view_test_support::host_deadline(
            Duration::from_millis(gap + 800),
        ));
        let first_b = seen.iter().position(|ft| ft == "view_test_b");
        let first_a = seen.iter().position(|ft| ft == "view_test_a");
        let leading_scan_ran_first = match (first_a, first_b) {
            (Some(a), Some(b)) => a < b,
            (Some(_), None) => true,
            _ => false,
        };
        if leading_scan_ran_first {
            assert!(
                first_b.is_some(),
                "the float opened {gap} ms after the arming burst was never \
                 reported: the absorbed event owes a trailing scan, and the \
                 walk this round did produce was {seen:?}"
            );
            return;
        }
        rounds.push(seen);
    }
    panic!("no round observed the leading scan ahead of the second float; the host stalled the deferred scan past every gap tried: {rounds:?}");
}

/// The whole probe against a real engine: what `package.loaded` answers,
/// when it answers it, and that the answer decodes back into the message
/// `update` acts on. A fixture that requires nothing and one that stands a
/// module in the registry, so a pass cannot come from the probe answering
/// the same thing either way.
#[test]
fn the_claimant_probe_answers_what_the_session_actually_loaded() {
    for (name, init, expected) in [
        ("claimants-bare", "", Vec::new()),
        (
            "claimants-loaded",
            "package.loaded['noice'] = { probed = true }\n",
            vec!["noice".to_string()],
        ),
    ] {
        let session = Session::start(name, init);
        session
            .engine
            .handle
            .probe_claimants(session.engine.api_info.channel_id)
            .unwrap();
        // the probe fires on the first idle transition, which an eval
        // barrier is not: this forces nvim through its main loop
        session.eval("execute('sleep 100m')");
        let mut model = model();
        let probed = session
            .wait_for(&mut model, ARRIVAL, |msg| match msg {
                Msg::ClaimantsProbed(loaded) => Some(loaded.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name}: the probe never answered"));
        assert_eq!(probed, expected, "{name}");
    }
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

/// The reading a restart takes off the engine it is replacing, proven
/// against a real session on both settings of the option that decides it.
///
/// A session holding a swap file reports it, paired with the buffer's own
/// name, and the path it reports is on disk -- which is the exact condition
/// `EngineConfig::recovering_recorded` gates nvim's `-r` on. A session with
/// no swap file, which is what the user's own `swapfile = false` leaves and
/// what `-n` leaves here, reports none: a restart that passed `-r` anyway
/// would hand its replacement a recovery that produces no buffer, and
/// `create_windows` ends the child through `getout(1)` before `VimEnter`.
///
/// `updatecount` rather than `swapfile` is what the on-case sets, and it is
/// set from `-c` rather than from the init: `-n` zeroes the option after
/// the config has been sourced, and nvim decides a buffer's swap from that
/// number at `ml_open`.
///
/// The `eval` barrier is the ordering: nvim writes its reply after the
/// autocommand that sent the notification, and the reader thread takes the
/// stream in order, so a reply in hand means the report ahead of it is
/// already recorded.
#[test]
fn the_swap_file_a_session_holds_is_recorded_only_while_there_is_one() {
    for (name, swapping) in [("swaps-on", true), ("swaps-off", false)] {
        let dir = common::fixture(&format!("bridge-live-{name}"), "");
        let mut cfg = common::isolated_reading(&dir.join("init.lua"));
        if swapping {
            cfg = cfg.with_arg("-c").with_arg("set updatecount=200");
        }
        let (engine, _pump, _rx) = common::spawn_with_pump(cfg, 1024);
        engine
            .handle
            .register_bridge(engine.api_info.channel_id)
            .unwrap();
        engine
            .handle
            .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
            .unwrap();

        let notes = dir.join("notes.md");
        std::fs::write(&notes, "one\ntwo\n").unwrap();
        engine
            .handle
            .eval_str(&format!(
                "execute('edit {}')",
                notes.to_string_lossy().replace('\'', "''")
            ))
            .unwrap();

        let recorded = engine.handle.recorded_swaps();
        if !swapping {
            assert!(
                recorded.is_empty(),
                "{name}: a session with no swap files must name none, got {recorded:?}"
            );
            continue;
        }
        let (buffer, swap) = recorded
            .iter()
            .find(|(buffer, _)| std::path::Path::new(buffer) == notes)
            .unwrap_or_else(|| panic!("{name}: the open buffer is missing from {recorded:?}"));
        assert!(
            std::path::Path::new(swap).exists(),
            "{name}: {buffer} reported the swap file {swap}, which is not on disk"
        );
    }
}
