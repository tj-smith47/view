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

mod common;

use std::path::Path;

use std::time::Duration;

use view_core::model::Look;
use view_core::msg::{Msg, OptionValue, RpcCall};
use view_core::native::channels::{self, Channel, Scope};
use view_core::native::registry;
use view_engine::handle::EngineHandle;
use view_engine::process::Engine;
use view_native::config::NativeConfig;
use view_native::supersede::{plan, Supersession};
use view_test_support::ScratchDir;

/// The fixture's own statusline setting: a plain nvim default is already
/// `2`, so a takeover to `0` would still read as a change if the config had
/// silently failed to load. `3` is a value only this file can be
/// responsible for.
const FIXTURE_LASTSTATUS: &str = "3";

/// A fixture config directory holding an `init.lua` that claims the
/// statusline, the way a user running lualine does.
fn fixture(name: &str) -> ScratchDir {
    common::fixture(
        &format!("supersede-live-{name}"),
        &format!("vim.opt.laststatus = {FIXTURE_LASTSTATUS}\n"),
    )
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
    let engine = common::spawn_with_drained_pump(common::isolated_reading(&dir.join("init.lua")));
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
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
            Some(RpcCall::HoldOption { name, value }) => handle.hold_option(name, value).unwrap(),
            Some(RpcCall::HoldWindowOption { name, value }) => {
                handle.hold_window_option(name, value).unwrap();
            }
            Some(RpcCall::HoldNotify) => handle.hold_notify().unwrap(),
            // the attach performed it, so there is nothing to apply here
            None => {}
            other => panic!("a plan entry must ride a durable takeover call, got {other:?}"),
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

    let plan = plan(
        &NativeConfig::all_enabled(),
        registry::features(),
        Look::default(),
    );
    apply(&engine.handle, &plan);

    // the writer thread preserves order and nvim processes the stream in
    // order, so this request cannot be answered before the notification
    // ahead of it has been applied
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        "0",
        "an enabled statusline must own the status line in the live session"
    );

    // the guard's OptionSet arm, on the write nvim does report: a plain
    // `:set` of the held option, undone before anything else runs. The
    // heavy fixture covers the arm the plugin trips (a write from inside
    // another autocommand, which fires no OptionSet at all), but that file
    // is `#[ignore]`d behind the compat plugin cache -- so without this the
    // whole guard is unexercised in `task ci`
    engine
        .handle
        .eval_str("execute('set laststatus=2')")
        .unwrap();
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        "0",
        "the takeover must survive a plain :set of the option it holds"
    );

    let after = snapshot(&dir);
    assert_eq!(
        before, after,
        "supersession is runtime only: the user's config may not change"
    );
}

/// How far into the chrome-shaped option list a reading is asked for: the
/// option names nvim evaluates to draw something a user sees, spelled here
/// because nvim's own option info carries no such classification.
///
/// An explicit list and not a shape test. `nvim_get_all_options_info`
/// answers about three hundred options, nearly all of them about editing
/// behaviour, and a heuristic over their names would have to be right
/// about every one of them. The names below are the ones that put a row,
/// a column or a line of text on the screen; a name added to the engine's
/// list that belongs with them joins by being written here, which is the
/// same edit as deciding what view does with it.
const CHROME_SHAPED_OPTIONS: &[&str] = &[
    "cmdheight",
    "colorcolumn",
    "foldcolumn",
    "laststatus",
    "number",
    "relativenumber",
    "ruler",
    "rulerformat",
    "showcmd",
    "showmode",
    "showtabline",
    "signcolumn",
    "statuscolumn",
    "statusline",
    "tabline",
    "title",
    "titlestring",
    "winbar",
];

#[test]
fn every_chrome_channel_the_engine_exposes_is_claimed_or_left_alone() {
    // the completeness half of the channel table, asked of the engine
    // rather than of a second list here: a plugin reaches the screen only
    // through a channel nvim exposes, so a capability or a chrome option
    // this build has never decided about is a way to draw over a surface
    // view believes it owns
    let dir = fixture("completeness");
    let engine = session(&dir);

    let ui_options = engine
        .handle
        .eval_str("join(api_info().ui_options, ' ')")
        .unwrap();
    let exposed: Vec<&str> = ui_options.split_whitespace().collect();
    for ext in view_engine::UI_EXT_OPTIONS {
        assert!(
            exposed.contains(ext),
            "the engine exposes no `{ext}`, so this half of the walk proves nothing \
             about the capabilities view attaches with: {ui_options:?}"
        );
    }
    let mut undecided: Vec<String> = Vec::new();
    for name in exposed.iter().copied() {
        if !channels::is_claimed(name) && !channels::is_yielded(name) {
            undecided.push(name.to_string());
        }
    }

    let present = engine
        .handle
        .eval_str("join(keys(luaeval('vim.api.nvim_get_all_options_info()')), ' ')")
        .unwrap();
    let present: Vec<&str> = present.split_whitespace().collect();
    for name in CHROME_SHAPED_OPTIONS {
        assert!(
            present.contains(name),
            "`{name}` is in this file's chrome list and the pinned engine has no such option"
        );
        if !channels::is_claimed(name) && !channels::is_yielded(name) {
            undecided.push((*name).to_string());
        }
    }

    let decided: Vec<String> = channels::NOT_CHROME
        .iter()
        .map(|row| format!("{}: {}", row.channel, row.why))
        .collect();
    assert!(
        undecided.is_empty(),
        "these channels can draw chrome and no surface claims them, with none of them on \
         the not-chrome list: {undecided:?}\nthe channels view leaves to the engine, each \
         with what the user gets instead:\n  {}",
        decided.join("\n  ")
    );
}

#[test]
fn every_held_option_matches_its_declared_scope() {
    // a global takeover sets and re-asserts with an empty `{}` opts table,
    // which nvim reads as the current window and buffer, so a window-local
    // option held that way would be held in one window and left to whoever
    // wrote it in every other, with nothing failing. The precondition is
    // asked of a real nvim rather than restated in a second list here
    let dir = fixture("scope");
    let engine = session(&dir);
    let plan = plan(
        &NativeConfig::all_enabled(),
        registry::features(),
        Look::default(),
    );
    assert!(!plan.is_empty(), "the all-enabled plan must not be empty");

    let declared: Vec<(&str, Scope)> = channels::CHANNELS
        .iter()
        .flat_map(|entry| entry.channels.iter())
        .filter_map(|channel| match channel {
            Channel::Hold { option, scope, .. } => Some((*option, *scope)),
            _ => None,
        })
        .collect();

    // an entry holding something other than an option has no scope to ask
    // about, so it is skipped rather than failed -- and counted, so a table
    // that stopped holding any option at all cannot leave this walk vacuous
    let mut asked = 0;
    for (name, scope) in &declared {
        asked += 1;
        // the name is interpolated into a single-quoted vimscript literal
        // below, where a `'` would close the string and the rest would be
        // evaluated as script. nvim's own option names are lowercase ASCII,
        // so the charset is asserted rather than escaped: a row that
        // violates it fails here loudly instead of running as an expression
        assert!(
            name.bytes().all(|b| b.is_ascii_lowercase()),
            "a takeover option name must be lowercase ASCII, got `{name}`"
        );
        let answered = engine
            .handle
            .eval_str(&format!(
                "luaeval('vim.api.nvim_get_option_info2(_A, {{}}).scope', '{name}')"
            ))
            .unwrap();
        let want = match scope {
            Scope::Global => "global",
            Scope::Window => "win",
        };
        assert_eq!(
            answered, want,
            "the channel table holds `{name}` at {scope:?} scope, which nvim scopes as \
             `{answered}`"
        );
    }
    assert!(
        asked > 0,
        "no channel holds an option any more, so this scope walk proved nothing"
    );
}

/// A live session reading `init.lua` from `dir`, with its pump delivered
/// to the caller: the shape a case needs when the report a hold sends back
/// is half of what it asserts.
fn reported_session(dir: &Path) -> (Engine, std::sync::mpsc::Receiver<Msg>) {
    let (engine, _pump, rx) =
        common::spawn_with_pump(common::isolated_reading(&dir.join("init.lua")), 256);
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    (engine, rx)
}

/// How long a case waits for a hold's own report. Generous: the report
/// rides the bridge behind the notification that produced it, so this is a
/// ceiling on a loaded host and never a measured span.
const REPORT_BUDGET: Duration = Duration::from_secs(10);

/// Whether a report naming `channel` and `holder` arrives inside
/// [`REPORT_BUDGET`].
///
/// Drains past the other reports that channel produces rather than reading
/// the first one: a new window is written twice on its way to the screen --
/// once as a window, once as a buffer entering it -- so the holder a case
/// is about is not always the first one reported for the window.
fn reported_holding(rx: &std::sync::mpsc::Receiver<Msg>, channel: &str, holder: &str) -> bool {
    common::drain_until(rx, REPORT_BUDGET, |msg| match msg {
        Msg::ChannelHeld {
            channel: name,
            holder: found,
        } if name == channel && found == holder => Some(()),
        _ => None,
    })
    .is_some()
}

#[test]
fn a_window_local_chrome_row_is_cleared_in_every_window_and_reported() {
    // the row an `ext_*` capability cannot reach: nvim draws a window's own
    // winbar inside that window's grid, so a config that sets one puts a
    // second chrome row on the screen under a tab line view is drawing.
    // No plugin here -- the option is what a plugin would have set
    let dir = common::fixture(
        "supersede-live-winbar",
        "vim.o.winbar = '%f'\n\
         vim.api.nvim_create_autocmd('BufWinEnter', {\n\
           callback = function() vim.wo.winbar = '%t' end,\n\
         })\n",
    );
    let (engine, rx) = reported_session(&dir);
    assert!(
        !engine.handle.eval_str("&winbar").unwrap().is_empty(),
        "the fixture config never took effect, so this test could not observe a takeover"
    );

    apply(
        &engine.handle,
        &plan(
            &NativeConfig::all_enabled(),
            registry::features(),
            Look::default(),
        ),
    );

    assert_eq!(
        engine.handle.eval_str("&winbar").unwrap(),
        "",
        "the tab line's window-local channel must be empty in the window that was open"
    );
    assert!(
        reported_holding(&rx, "winbar", "%t"),
        "the hold must say what the option was set to"
    );

    // a window opened after the takeover, with the config's own autocmd
    // writing the option as the window arrives: the case a one-shot walk
    // over the window list at VimEnter answers wrongly. A window carrying
    // a buffer of its own, because a split of the same buffer never
    // announces the buffer arriving and so runs nothing the config wrote
    engine.handle.eval_str("execute('new')").unwrap();
    assert_eq!(
        engine.handle.eval_str("&winbar").unwrap(),
        "",
        "a window opened after the takeover must be held too"
    );
    assert!(
        reported_holding(&rx, "winbar", "%t"),
        "the later window's own holder must be reported"
    );
    // every window, asked of the window list rather than of whichever
    // window the split left current: a hold that walked only the new one
    // leaves the row standing in the window the user was already in
    assert_eq!(
        engine
            .handle
            .eval_str(
                "luaeval('table.concat(vim.tbl_map(function(w) \
                 return vim.wo[w].winbar end, vim.api.nvim_list_wins()), \"|\")')"
            )
            .unwrap(),
        "|",
        "the row must be empty in every window, not only the current one"
    );

    // the option written again after every window event has already
    // happened, which is what a config recomputing its row as the cursor
    // moves does: the three window events find a new window and nothing
    // else, and the row was still on the screen under a real config while
    // they were the whole guard
    engine
        .handle
        .eval_str("execute('setlocal winbar=%m')")
        .unwrap();
    assert_eq!(
        engine.handle.eval_str("&winbar").unwrap(),
        "",
        "an option written back after the window events must be held anyway"
    );
    assert!(
        reported_holding(&rx, "winbar", "%m"),
        "a holder that arrives outside the window events must be reported too"
    );
}

#[test]
fn a_global_chrome_option_under_an_externalized_surface_stays_with_view() {
    // the tab line's other two channels. `ext_tabline` is what stops nvim
    // drawing the row at all, and the options the table marks as covered by
    // it are the proof that it does: a config setting both keeps its
    // values, and nvim still draws nothing there
    let dir = common::fixture(
        "supersede-live-tabline",
        "vim.o.showtabline = 2\nvim.o.tabline = '%f'\n",
    );
    let engine = session(&dir);
    apply(
        &engine.handle,
        &plan(
            &NativeConfig::all_enabled(),
            registry::features(),
            Look::default(),
        ),
    );

    assert_eq!(engine.handle.eval_str("&showtabline").unwrap(), "2");
    assert_eq!(
        engine.handle.eval_str("&tabline").unwrap(),
        "%f",
        "an option the attach covers is left alone: nvim evaluates it only while it draws \
         the row, and view asked for that row at the attach"
    );
}

#[test]
fn a_replaced_notify_is_put_back_for_a_session_that_draws_the_messages() {
    // the replaced-global channel, set the way a config sets it and with no
    // plugin involved
    let dir = common::fixture(
        "supersede-live-notify",
        "vim.notify = function() end\nvim.g.view_foreign_notify = true\n",
    );
    let engine = session(&dir);
    apply(
        &engine.handle,
        &plan(
            &NativeConfig::all_enabled(),
            registry::features(),
            Look::default(),
        ),
    );

    assert_eq!(
        engine
            .handle
            .eval_str("luaeval('vim.notify == _G.view_notify_hold and 1 or 0')")
            .unwrap(),
        "1",
        "the message surface's replaced global must be back at the hold view installed"
    );
}

/// What nvim answers about `option`: the type it takes, and the value nvim
/// itself left there.
///
/// The second half is the reading a hold refuses to report. A value nvim
/// wrote names no holder, and a notice about it is a box on every launch.
fn nvim_stock(handle: &EngineHandle, option: &str) -> (String, String) {
    let answer = handle
        .eval_str(&format!(
            "luaeval('(function() \
             local i = vim.api.nvim_get_option_info2(\"{option}\", {{}}) \
             return i.type .. \"=\" .. tostring(i.default) end)()')"
        ))
        .unwrap();
    let (kind, stock) = answer.split_once('=').unwrap();
    (kind.to_string(), stock.to_string())
}

/// A value of `option`'s own type that is neither what nvim left there nor
/// what the table holds the channel at, in nvim's own spelling of it: the
/// only reading a hold has anything to report about.
///
/// `None` for a flag, which has two readings and neither names a holder:
/// one is nvim's own, and the other is the user switching the channel off.
fn populated(kind: &str, stock: &str, held_at: Option<&str>) -> Option<String> {
    match kind {
        "boolean" => None,
        "number" => ('0'..='9')
            .map(|digit| digit.to_string())
            .find(|value| value != stock && Some(value.as_str()) != held_at),
        _ => Some("view-pin-held".to_string()),
    }
}

/// A channel value as nvim spells it back, which is what a case compares a
/// hold's own value against.
/// Resolved through the same look [`hold`] issues the value under, so a
/// look-keyed channel is compared against the leg that was actually set.
fn spelled(value: channels::ChannelValue) -> String {
    match value.wire(Look::default()) {
        OptionValue::Int(n) => n.to_string(),
        OptionValue::Bool(b) => u8::from(b).to_string(),
        OptionValue::Str(s) => s,
    }
}

/// The chunk name a replaced global's holder is loaded under, which is
/// what `debug.getinfo` answers about it and so what the report spells.
const REPLACEMENT_SOURCE: &str = "@view-pin-held";

/// The hold `surface` declares, which is the call that reads the surface's
/// covered channels beside its own option.
fn hold_of(
    surface: view_core::native::surfaces::Surface,
) -> Option<(&'static str, Scope, channels::ChannelValue)> {
    channels::channels(surface)
        .iter()
        .find_map(|channel| match channel {
            Channel::Hold {
                option,
                scope,
                value,
            } => Some((*option, *scope, *value)),
            _ => None,
        })
}

/// The hold of `option` under the default look, issued over the call its
/// scope names.
fn hold(handle: &EngineHandle, option: &str, scope: Scope, value: channels::ChannelValue) {
    let value = value.wire(Look::default());
    match scope {
        Scope::Global => handle.hold_option(option, &value).unwrap(),
        Scope::Window => handle.hold_window_option(option, &value).unwrap(),
    }
}

#[test]
fn every_channel_kind_that_can_be_held_reports_the_holder_it_found() {
    // the class the window-local hold's own report belongs to: a surface
    // that changes hands with nothing said leaves the user no way to learn
    // which switch gives it back, and that is a property of every channel
    // rather than of the one whose report was written first. No plugin in
    // any of these -- the channel is populated the way a config populates
    // it
    let mut options: Vec<&str> = Vec::new();
    let mut globals: Vec<&str> = Vec::new();
    let mut covered: Vec<&str> = Vec::new();
    let mut flags: Vec<&str> = Vec::new();
    let mut capabilities: Vec<&str> = Vec::new();
    for entry in channels::CHANNELS {
        for channel in entry.channels {
            match *channel {
                Channel::Hold {
                    option,
                    scope,
                    value,
                } => {
                    if options.contains(&option) {
                        continue;
                    }
                    options.push(option);
                    let dir = common::fixture(&format!("supersede-live-held-{option}"), "");
                    let (engine, rx) = reported_session(&dir);
                    let (kind, stock) = nvim_stock(&engine.handle, option);
                    let held =
                        populated(&kind, &stock, Some(&spelled(value))).unwrap_or_else(|| {
                            panic!("no value of `{option}`'s own type can name a holder")
                        });
                    // written after the attach rather than from the fixture
                    // config: nvim zeroes some of these itself when a UI
                    // takes the capability beside them, and a case that let
                    // it do so would assert on a channel nothing had
                    // populated
                    engine
                        .handle
                        .eval_str(&format!("execute('set {option}={held}')"))
                        .unwrap();
                    assert_eq!(
                        engine.handle.eval_str(&format!("&{option}")).unwrap(),
                        held,
                        "`{option}` never took the value this case is about to hold over"
                    );
                    hold(&engine.handle, option, scope, value);
                    assert!(
                        reported_holding(&rx, option, &held),
                        "the hold of `{option}` said nothing about the value it displaced"
                    );
                }
                Channel::Covered { option, by } => {
                    // a capability is nvim's answer to `nvim_ui_attach`, and
                    // no config writes one, so it has no holder to find
                    if option.starts_with("ext_") {
                        if !capabilities.contains(&option) {
                            capabilities.push(option);
                        }
                        continue;
                    }
                    if covered.contains(&option) || flags.contains(&option) {
                        continue;
                    }
                    let Some((held_option, scope, value)) = hold_of(entry.surface) else {
                        panic!("`{option}` is covered by `{by}` and no hold of the same surface reads it")
                    };
                    let dir = common::fixture(&format!("supersede-live-covered-{option}"), "");
                    let (engine, rx) = reported_session(&dir);
                    let (kind, stock) = nvim_stock(&engine.handle, option);
                    let Some(claimed) = populated(&kind, &stock, None) else {
                        flags.push(option);
                        continue;
                    };
                    covered.push(option);
                    engine
                        .handle
                        .eval_str(&format!("execute('set {option}={claimed}')"))
                        .unwrap();
                    hold(&engine.handle, held_option, scope, value);
                    assert!(
                        reported_holding(&rx, option, &claimed),
                        "the hold of `{held_option}` said nothing about what was drawing `{option}`"
                    );
                }
                Channel::Replaced(global) => {
                    if globals.contains(&global) {
                        continue;
                    }
                    globals.push(global);
                    let dir = common::fixture(
                        &format!("supersede-live-held-{}", global.replace('.', "-")),
                        &format!(
                            "{global} = assert(load('', '{REPLACEMENT_SOURCE}'))
"
                        ),
                    );
                    let (engine, rx) = reported_session(&dir);
                    apply(
                        &engine.handle,
                        &plan(
                            &NativeConfig::all_enabled(),
                            registry::features(),
                            Look::default(),
                        ),
                    );
                    assert!(
                        reported_holding(&rx, global, REPLACEMENT_SOURCE),
                        "the hold of `{global}` said nothing about the function it displaced"
                    );
                }
                // an attach takes its surface at `nvim_ui_attach`, where
                // nvim stops drawing it and leaves no holder to name; a
                // float is read off the window list and reported by the
                // scan that finds it
                Channel::Attach(_) | Channel::Float(_) => {}
            }
        }
    }
    assert_eq!(
        (
            options.len(),
            globals.len(),
            covered.len(),
            flags.len(),
            capabilities.len()
        ),
        (3, 1, 4, 3, 1),
        "held {options:?}, replaced {globals:?}, read beside {covered:?}, \
         flags with no holder to name {flags:?}, capabilities {capabilities:?}"
    );
}

#[test]
fn a_hold_that_finds_nvims_own_value_names_nobody() {
    // the notice on every launch: nvim's own `laststatus` is 2 and its own
    // `cmdheight` is 1, so a hold reporting whatever it displaced announced
    // a takeover from nvim itself before a config had claimed anything
    let dir = common::fixture("supersede-live-stock", "");
    let (engine, rx) = reported_session(&dir);
    for (option, scope) in [
        ("laststatus", Scope::Global),
        ("cmdheight", Scope::Global),
        ("winbar", Scope::Window),
    ] {
        hold(
            &engine.handle,
            option,
            scope,
            channels::ChannelValue::Int(0),
        );
    }
    let stray = common::drain_until(&rx, Duration::from_secs(2), |msg| match msg {
        Msg::ChannelHeld { channel, holder } => Some(format!("{channel} = {holder}")),
        _ => None,
    });
    assert!(
        stray.is_none(),
        "a hold of an untouched config found a holder to name: {stray:?}"
    );
}

/// Every row of nvim's own screen, as one string per row.
///
/// `screenstring` cell by cell rather than any buffer read: what is asked
/// is what nvim put on the screen, which is where a chrome row a plugin
/// wrote and a chrome row view draws would both appear.
fn screen_rows(engine: &Engine) -> Vec<String> {
    let joined = engine
        .handle
        .eval_str(
            "luaeval('(function() vim.cmd(\"redraw\") local out = {} \
             for r = 1, vim.o.lines do local s = {} \
             for c = 1, vim.o.columns do s[#s + 1] = vim.fn.screenstring(r, c) end \
             out[#out + 1] = table.concat(s) end return table.concat(out, \"\\n\") end)()')",
        )
        .unwrap();
    joined.lines().map(str::to_string).collect()
}

#[test]
fn a_held_chrome_row_leaves_no_second_copy_of_the_file_name_on_the_grid() {
    // the duplication the window-local hold exists to prevent, read off
    // nvim's own screen: a config that draws the file name in a chrome row
    // of its own puts it there a second time under the row view draws
    let dir = common::fixture("supersede-live-winbar-grid", "vim.o.winbar = '%f'\n");
    let engine = session(&dir);
    engine
        .handle
        .eval_str("execute('edit view_pin_chrome.txt')")
        .unwrap();

    let before = screen_rows(&engine)
        .iter()
        .filter(|line| line.contains("view_pin_chrome.txt"))
        .count();
    assert!(
        before > 0,
        "the fixture never drew the name, so this case could not observe the hold"
    );

    apply(
        &engine.handle,
        &plan(
            &NativeConfig::all_enabled(),
            registry::features(),
            Look::default(),
        ),
    );

    let after: Vec<String> = screen_rows(&engine)
        .into_iter()
        .filter(|line| line.contains("view_pin_chrome.txt"))
        .collect();
    assert!(
        after.is_empty(),
        "the chrome rows view draws are view's own, so nvim's grid must carry no \
         copy of the name: {after:?}"
    );
}

#[test]
fn a_float_parked_over_the_command_line_is_claimed_at_the_geometry_nvim_reports() {
    // the float channel of the command line, opened the way a plugin opens
    // one and measured from nvim's own window config rather than from a
    // rect written here
    let dir = common::fixture("supersede-live-float", "");
    let engine = session(&dir);
    let size = engine
        .handle
        .eval_str("join([&lines, &columns], ' ')")
        .unwrap();
    let (lines, columns) = size
        .split_once(' ')
        .map(|(l, c)| {
            (
                l.parse::<u16>().expect("nvim reports a row count"),
                c.parse::<u16>().expect("nvim reports a column count"),
            )
        })
        .expect("the eval answers both");

    let opened = engine
        .handle
        .eval_str(&format!(
            "luaeval('(function() local buf = vim.api.nvim_create_buf(false, true) \
             local win = vim.api.nvim_open_win(buf, false, {{ relative = \"editor\", \
             row = {row}, col = 0, width = 20, height = 2, style = \"minimal\" }}) \
             local cfg = vim.api.nvim_win_get_config(win) \
             return table.concat({{ cfg.row, cfg.col, cfg.width, cfg.height, cfg.anchor }}, \
             \" \") end)()')",
            row = lines - 2
        ))
        .unwrap();
    let parts: Vec<&str> = opened.split_whitespace().collect();
    let [row, col, width, height, anchor] = parts.as_slice() else {
        panic!("nvim must report the window's own rect: {opened:?}")
    };
    assert_eq!(
        *anchor, "NW",
        "the rule below reads the anchor nvim reports"
    );
    let row: f64 = row.parse().expect("nvim reports the row as a number");
    let col: f64 = col.parse().expect("nvim reports the column as a number");
    let width: u16 = width.parse().expect("nvim reports the width");
    let height: u16 = height.parse().expect("nvim reports the height");

    let mut model = view_core::model::Model::with_term_size(columns, lines);
    model.attach_surfaces(vec![view_core::native::ext::Ext::Cmdline]);
    let _ = view_core::update::update(
        &mut model,
        Msg::Redraw(vec![view_core::events::UiEvent::GridResize {
            grid: 1,
            width: u64::from(columns),
            height: u64::from(lines),
        }]),
    );
    let _ = view_core::update::update(
        &mut model,
        Msg::Redraw(vec![view_core::events::UiEvent::CmdlineShow {
            content: vec![(0, "e pre".to_string())],
            pos: 5,
            firstc: ":".to_string(),
            prompt: String::new(),
            indent: 0,
            level: 1,
        }]),
    );

    let anchor = view_core::native::surfaces::FloatAnchor::NorthWest;
    assert_eq!(
        view_core::native::surfaces::claims_at(
            row as i64, col as i64, width, height, anchor, &model
        ),
        Some(view_core::native::surfaces::Surface::Cmdline),
        "a window nvim parked on the command line's own rows claims that surface"
    );
    assert_eq!(
        view_core::native::surfaces::claims_at(
            row as i64 - 3,
            col as i64,
            width,
            height,
            anchor,
            &model
        ),
        None,
        "the same window three rows higher is over the buffer, and claims nothing"
    );
}

#[test]
fn a_disabled_statusline_leaves_the_users_own_setting_alone() {
    let dir = fixture("disabled");
    let before = snapshot(&dir);
    let engine = session(&dir);

    let cfg = NativeConfig::from_toml_str("[native]\nstatusline = false\n").unwrap();
    let plan = plan(&cfg, registry::features(), Look::default());
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
    assert_eq!(before, after);
}

/// Tiles keeps `laststatus = 2` with the statusline off, and the release a
/// flip back to `panes = "nvim"` sends puts the config's own value back and
/// takes the guard down with it. A gaps-only flip re-issues the same hold
/// while it is already in force (the `before[name] == nil` guard in
/// `HOLD_OPTION_CHUNK`), and a hold issued again after a release has to
/// stash the value then in force.
#[test]
fn a_disabled_statusline_under_tiles_is_held_at_two_until_released() {
    let dir = fixture("tiles-disabled");
    let before = snapshot(&dir);
    let engine = session(&dir);
    let cfg = NativeConfig::from_toml_str("[native]\nstatusline = false\n").unwrap();
    apply(
        &engine.handle,
        &plan(
            &cfg,
            registry::features(),
            Look::new(view_core::model::Panes::Tiles, true),
        ),
    );
    assert_eq!(engine.handle.eval_str("&laststatus").unwrap(), "2");

    engine
        .handle
        .hold_option("laststatus", &OptionValue::Int(2))
        .unwrap();
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        "2",
        "a second hold while the first is still in force must not move the value"
    );
    engine.handle.release_option("laststatus").unwrap();
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        FIXTURE_LASTSTATUS,
        "the release must put back the value the config set, not the value \
         the second hold stashed"
    );
    engine
        .handle
        .eval_str("execute('set laststatus=1')")
        .unwrap();
    engine
        .handle
        .hold_option("laststatus", &OptionValue::Int(2))
        .unwrap();
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        "2",
        "a hold issued after a release must still stash and hold the value"
    );
    engine.handle.release_option("laststatus").unwrap();
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        "1",
        "the release after a re-hold must put back the value that was in \
         force when that hold was issued"
    );
    engine
        .handle
        .eval_str("execute('set laststatus=4')")
        .unwrap();
    assert_eq!(
        engine.handle.eval_str("&laststatus").unwrap(),
        "4",
        "the second release must take the guard down too"
    );
    assert_eq!(before, snapshot(&dir));
}

#[test]
fn the_takeover_reverses_when_the_feature_is_turned_off() {
    let dir = fixture("reversal");
    let engine = session(&dir);
    let enabled = plan(
        &NativeConfig::all_enabled(),
        registry::features(),
        Look::default(),
    );
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
    apply(
        &restarted.handle,
        &plan(&cfg, registry::features(), Look::default()),
    );

    assert_eq!(
        restarted.handle.eval_str("&laststatus").unwrap(),
        FIXTURE_LASTSTATUS,
        "the registry's own off switch must restore the user's setting"
    );
}
