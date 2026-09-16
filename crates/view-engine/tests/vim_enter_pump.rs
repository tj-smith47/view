//! What the startup hook has in force by the time the rest of a config
//! runs, asked of a live child that is answered and attached the way a
//! session answers and attaches it.
//!
//! Three facts, and the whole of what the hook exists for. The UI is applied
//! before any other `VimEnter` autocommand runs, so the one screen startup
//! draws is drawn for the UI view attached rather than for the stand-in and
//! then again for the real one. Where a treesitter highlighter is attached,
//! the hook parses the visible lines before it returns, so the colours are
//! in that screen rather than in one after it. And the `UIEnter` nvim does
//! not fire for a UI applied that way is fired here instead, on the loop's
//! next turn, carrying the channel -- which is where nvim would have put
//! it, and late enough that a handler registered from another plugin's
//! `VimEnter` is standing when it arrives.
//!
//! Asked of a real child rather than read off the chunk's text: every one
//! of them is about what nvim's own loop does with what the chunk sends,
//! and only nvim can be asked that. The peer here answers `view_vim_enter` and
//! attaches behind the answer, which is the order a session writes them in.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use rmpv::Value;
use std::sync::mpsc;
use std::time::{Duration, Instant};
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
///
/// Screen updates are counted from a decoration provider, which is the one
/// thing a config can be told a frame went out by, and the count is read
/// inside the `UIEnter` handler. nvim performs one of its own at the end of
/// startup and the hook forces none, so a count above that one is a screen
/// view was handed twice.
///
/// The treesitter readings are taken through `package.loaded` rather than
/// `vim.treesitter.highlighter`, which is the guard the hook itself uses:
/// asking for the module is what would load it, and a fixture that loads
/// it cannot report a config that never had one. A tree counts as parsed
/// where it reports itself valid, injections included, over the range the
/// highlighter's own `on_start` asks for -- which is the range the hook
/// parses, and which reaches one line past `w$` because that function is
/// the last *completely* displayed line. `is_valid(true)` answers for the
/// root region alone, which this fixture's buffer finishes ahead of the
/// injected tree its first visible line carries, so it would report a
/// half-parsed screen as a parsed one.
///
/// The same reading taken from `on_start` is what says which frame the
/// colours went out in. This provider is registered before the fixture
/// starts the highlighter, and nvim runs providers in registration order,
/// so the reading is of the tree the frame *begins* with rather than of
/// one the highlighter's own turn has just parsed: a buffer small enough
/// to parse inside that turn would answer for the hook otherwise, and the
/// reading taken here cannot be satisfied by anything but a parse that
/// finished before the screen update started.
const PIN_CONFIG: &str = "\
_G.view_pin = { uis = -1, chan = -1, uienters = 0, chan_when_idle = -1,\n\
  safe = 0, drawn = 0, drawn_at_uienter = -1, highlighted = 0,\n\
  parsed_at_uienter = -1, coloured_draw = -1 }\n\
local function parsed()\n\
  local buf = vim.api.nvim_get_current_buf()\n\
  local hl = package.loaded['vim.treesitter.highlighter']\n\
  local h = hl and hl.active[buf]\n\
  if not h then\n\
    return -1\n\
  end\n\
  _G.view_pin.highlighted = 1\n\
  local visible = { vim.fn.line('w0') - 1, vim.fn.line('w$') + 1 }\n\
  return h.tree:is_valid(false, visible) and 1 or 0\n\
end\n\
vim.api.nvim_set_decoration_provider(\n\
  vim.api.nvim_create_namespace('view_pin'), {\n\
    on_start = function()\n\
      _G.view_pin.drawn = _G.view_pin.drawn + 1\n\
      if _G.view_pin.coloured_draw < 0 and parsed() == 1 then\n\
        _G.view_pin.coloured_draw = _G.view_pin.drawn\n\
      end\n\
    end,\n\
  })\n\
vim.api.nvim_create_autocmd('VimEnter', {\n\
  callback = function()\n\
    _G.view_pin.uis = vim.api.nvim_eval('len(nvim_list_uis())')\n\
    vim.api.nvim_create_autocmd('UIEnter', {\n\
      callback = function(args)\n\
        _G.view_pin.uienters = _G.view_pin.uienters + 1\n\
        _G.view_pin.chan = (args.data or {}).chan or 0\n\
        _G.view_pin.drawn_at_uienter = _G.view_pin.drawn\n\
        _G.view_pin.parsed_at_uienter = parsed()\n\
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
    /// Screen updates the fixture's decoration provider had seen by the
    /// time the config's `UIEnter` ran: nvim's own at the end of startup,
    /// which is the only one the hook leaves before the event.
    drawn_at_uienter: i64,
    /// Whether a treesitter highlighter was attached to the current buffer
    /// when either reading was taken, which is the branch the hook's parse
    /// turns on.
    highlighted: i64,
    /// Whether that buffer's highlighter had a parsed tree there: `1` for
    /// a parse the hook ran, `0` for one still running, and `-1` where
    /// there was no highlighter to ask about.
    parsed_at_uienter: i64,
    /// The first screen update that began with that tree already parsed,
    /// counted the same way as `drawn_at_uienter`, or `-1` where no update
    /// did. This is the frame the colours went out in.
    coloured_draw: i64,
}

/// A buffer nvim's own bundled `lua` parser can highlight, made current
/// while `init.lua` is still sourcing -- so the highlighter is attached by
/// the time the hook's `VimEnter` callback runs, which is the only window
/// in which the hook can parse it.
///
/// Longer than the window, so `w0` and `w$` bound a range rather than the
/// whole buffer, and carrying a `vim.cmd([[...]])` injection on the first
/// visible line, which is the part of the answer the root region's own
/// validity does not cover.
const TREESITTER_BUFFER: &str = "\
local buf = vim.api.nvim_create_buf(false, true)\n\
local lines = { 'vim.cmd([[set number]])' }\n\
for i = 1, 200 do\n\
  lines[#lines + 1] =\n\
    ('local x%d = %d -- %s'):format(i, i, string.rep('p', 200))\n\
end\n\
vim.api.nvim_buf_set_lines(buf, 0, -1, false, lines)\n\
vim.api.nvim_set_current_buf(buf)\n";

/// The highlighter, started after the fixture has registered its own
/// decoration provider: `vim.treesitter.start` is what loads the
/// highlighter module, and the module registers its provider as it loads,
/// so starting it here is what puts the fixture's provider ahead of the
/// highlighter's in the order nvim runs them.
const START_HIGHLIGHTER: &str = "vim.treesitter.start(buf, 'lua')\n";

/// The bound the startup chunk gives a grammar whose injection query
/// carries combined injections, written here because `late_attach_cmd` is
/// private to its own crate: the two tests below stand either side of it,
/// and a chunk that moved it would leave both of them measuring the same
/// branch.
const COMBINED_PARSE_BYTES: usize = 16 * 1024;

/// A markdown buffer of a stated size, whose bundled grammar carries
/// combined injections: its injection scan is the whole document whatever
/// range is asked, so the cost rides on the file and the hook bounds it by
/// the file rather than by the window.
///
/// Returned with the byte count nvim reads off it -- every line and its
/// newline, which is what `nvim_buf_get_offset` answers -- so a test says
/// which side of the bound it stands on rather than trusting a line count
/// to land there.
fn markdown_buffer(fill: usize) -> (String, usize) {
    let mut lines = vec![
        "# heading".to_string(),
        String::new(),
        "```lua".to_string(),
        "local x = 1".to_string(),
        "```".to_string(),
        String::new(),
    ];
    for i in 0..fill {
        lines.push(format!("- item {i} {}", "p".repeat(100)));
    }
    let bytes = lines.iter().map(|line| line.len() + 1).sum();
    let table: String = lines.iter().map(|line| format!("  '{line}',\n")).collect();
    (
        format!(
            "local buf = vim.api.nvim_create_buf(false, true)\n\
             vim.api.nvim_buf_set_lines(buf, 0, -1, false, {{\n\
             {table}}})\n\
             vim.api.nvim_set_current_buf(buf)\n"
        ),
        bytes,
    )
}

const START_MARKDOWN_HIGHLIGHTER: &str = "vim.treesitter.start(buf, 'markdown')\n";

fn pin_config() -> ScratchDir {
    config(PIN_CONFIG)
}

/// The same fixture with a highlighted buffer standing before it.
fn highlighted_pin_config() -> ScratchDir {
    highlighted_config(TREESITTER_BUFFER, START_HIGHLIGHTER)
}

/// The same, over a grammar the hook bounds by the file, with the bytes
/// that grammar's buffer holds.
fn highlighted_markdown_config(fill: usize) -> (ScratchDir, usize) {
    let (buffer, bytes) = markdown_buffer(fill);
    (
        highlighted_config(&buffer, START_MARKDOWN_HIGHLIGHTER),
        bytes,
    )
}

fn highlighted_config(buffer: &str, start_highlighter: &str) -> ScratchDir {
    let lua = format!("{buffer}{PIN_CONFIG}{start_highlighter}");
    let provider = lua.find("nvim_set_decoration_provider").unwrap();
    let highlighter = lua.find("vim.treesitter.start").unwrap();
    assert!(
        provider < highlighter,
        "the fixture's provider has to register before the highlighter's \
         own, or its on_start reads a tree the highlighter's own turn has \
         just parsed and a frame that carried no colours reports one that \
         did"
    );
    config(&lua)
}

fn config(lua: &str) -> ScratchDir {
    let dir = ScratchDir::new("vim-enter-pump").unwrap();
    std::fs::write(dir.join("init.lua"), lua).unwrap();
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
                     pin.chan_when_idle, channel, pin.safe, \
                     pin.drawn_at_uienter, pin.highlighted, \
                     pin.parsed_at_uienter, pin.coloured_draw }",
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
        drawn_at_uienter: at(6),
        highlighted: at(7),
        parsed_at_uienter: at(8),
        coloured_draw: at(9),
    }
}

/// The fixture read once the child has been back round its own loop, which
/// is where the scheduled event runs and where `SafeState` fires.
///
/// The pause gives the child room to take a turn of its own -- answering a
/// request is not one, so a reading taken straight after the reply can
/// precede the event it is about -- and the loop below is the guarantee,
/// not the pause. `chan_when_idle` takes a value only from a `SafeState`
/// running after the `UIEnter` handler has recorded a channel, so
/// re-reading until it is non-negative exits on exactly the transition
/// this function is named for; the pause only saves most of those reads.
/// Host-side rather than asked of the child (`execute('sleep 100m')`),
/// because the hook's own `vim.wait` for the attach services this channel:
/// an nvim-side sleep arriving while the hook is inside that wait runs
/// there, ahead of the attach the wait is for. The poll between readings is
/// paced rather than tight -- a child that never schedules the event would
/// otherwise spend the whole deadline being asked again as fast as a
/// shared host allows, which is load the other tests on that host pay for.
fn settled(engine: &Engine) -> Reading {
    std::thread::sleep(Duration::from_millis(100));
    let deadline = Instant::now() + common::rpc_deadline_for(3);
    let mut read = reading(engine);
    while Instant::now() < deadline && read.chan_when_idle < 0 {
        std::thread::sleep(Duration::from_millis(10));
        read = reading(engine);
    }
    read
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

/// The engine answered and attached the way a session does it, left parked
/// at the point the hook's own work is done.
fn answered(engine: &mut Engine) -> mpsc::Receiver<Msg> {
    let (tx, rx) = mpsc::sync_channel(256);
    let (_pump, cutover) = engine.start_pump(tx);
    let token = vim_enter_token(cutover.presink, &rx)
        .expect("the child never asked view_vim_enter, so nothing here was measured");
    engine.handle.reply(token, ReplyValue::Nil).unwrap();
    engine
        .handle
        .ui_attach(120, 40, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    rx
}

/// With a highlighter attached, the hook parses the visible lines before
/// it returns, so the tree is valid when nvim draws its own first frame at
/// the end of startup and the colours are in the frame the file's text is.
///
/// Left to that frame the parse starts inside it, and its continuations
/// land the colours behind whatever `UIEnter` sets off.
#[test]
fn the_highlighters_parse_is_finished_before_nvims_first_frame() {
    let dir = highlighted_pin_config();
    let mut engine = engine(&dir);
    let _rx = answered(&mut engine);

    let read = settled(&engine);

    assert_eq!(
        read.highlighted, 1,
        "the fixture attaches a highlighter while init.lua is sourcing, so \
         one must be active here or this test measured nothing"
    );
    assert_eq!(
        read.coloured_draw, 1,
        "the colours must be in nvim's own screen update at the end of \
         startup, which is the first: the tree has to be parsed before that \
         update begins, or the colours land in a frame after it, behind the \
         plugin loads UIEnter sets off, which reads as a second paint"
    );
    assert_eq!(
        read.drawn_at_uienter, 1,
        "one screen update before UIEnter, nvim's own: the hook draws none \
         of its own, and a second frame here would be the recolour the \
         parse exists to prevent"
    );
    assert_eq!(
        read.parsed_at_uienter, 1,
        "and the tree is still parsed by the time the config reaches \
         UIEnter"
    );
}

/// A grammar whose injection query carries combined injections is parsed
/// inline while the buffer is under the bound that grammar gets: the scan
/// is the whole document however few lines the hook asks for, so the cost
/// rides on the file, and a file this size still finishes inside the
/// frame nvim draws at the end of startup.
#[test]
fn a_combined_injection_grammar_under_its_own_bound_is_parsed_inline() {
    let (dir, bytes) = highlighted_markdown_config(36);
    assert!(
        bytes < COMBINED_PARSE_BYTES,
        "the fixture has to stand under the bound to measure the branch \
         this test is about, and it holds {bytes} bytes"
    );
    let mut engine = engine(&dir);
    let _rx = answered(&mut engine);

    let read = settled(&engine);

    assert_eq!(
        read.highlighted, 1,
        "the fixture attaches a markdown highlighter while init.lua is \
         sourcing, so one must be active here or this test measured nothing"
    );
    assert_eq!(
        read.coloured_draw, 1,
        "the colours must be in nvim's own screen update at the end of \
         startup: a combined-injection grammar under the bound is parsed \
         inline like any other, or this buffer keeps the uncoloured first \
         frame the bound exists to buy it out of"
    );
}

/// The same grammar over that bound is left to the asynchronous path: the
/// injection scan is the whole document, so a file this size costs more
/// than the frame it would have to finish inside.
///
/// The reading that says the hook skipped it is the first screen update
/// beginning with an unparsed tree, which is what the highlighter's own
/// first slice then finds and what every startup looked like before this
/// hook parsed anything.
#[test]
fn a_combined_injection_grammar_over_its_own_bound_takes_the_async_path() {
    let (dir, bytes) = highlighted_markdown_config(290);
    assert!(
        bytes > COMBINED_PARSE_BYTES,
        "the fixture has to stand over the bound to measure the branch \
         this test is about, and it holds {bytes} bytes"
    );
    let mut engine = engine(&dir);
    let _rx = answered(&mut engine);

    let read = settled(&engine);

    assert_eq!(
        read.highlighted, 1,
        "the fixture attaches a markdown highlighter while init.lua is \
         sourcing, so one must be active here or this test measured nothing"
    );
    assert_ne!(
        read.coloured_draw, 1,
        "nvim's own screen update at the end of startup must begin with an \
         unparsed tree: a first frame carrying colours here is the inline \
         parse having run on a file whose injection scan costs more than \
         the frame"
    );
}

/// A buffer with no highlighter is parsed by nothing: there is no
/// highlighter to ask about, so the hook's block goes straight to the
/// event it schedules.
///
/// The frame is nvim's own at the end of startup either way, and the
/// reading is taken inside the event's own handler, which is the last
/// moment at which "before the event" and "after it" are still different
/// answers.
#[test]
fn a_buffer_with_no_highlighter_is_drawn_once_and_never_parsed() {
    let dir = pin_config();
    let mut engine = engine(&dir);
    let _rx = answered(&mut engine);

    let read = settled(&engine);

    assert_eq!(
        read.highlighted, 0,
        "a --clean child opening no file has no highlighter, which is the \
         branch this test is about"
    );
    assert_eq!(
        read.parsed_at_uienter, -1,
        "there is no highlighter to read a tree from, here or in the hook"
    );
    assert_eq!(
        read.drawn_at_uienter, 1,
        "one screen update before UIEnter, nvim's own at the end of \
         startup: the hook forces none, here or where it parses"
    );
    assert_eq!(
        read.uienters, 1,
        "the hook returned and its scheduled event ran, so the skipped \
         parse cost the startup nothing it did not already spend"
    );
}

#[test]
fn the_hook_has_the_ui_in_force_before_the_rest_of_startup_runs() {
    let dir = pin_config();
    let mut engine = engine(&dir);
    let _rx = answered(&mut engine);

    let read = settled(&engine);

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

/// The other spawn shape, where the guard that fires nothing is the whole
/// of what is being read.
///
/// A relayed stdin keeps nvim's own pre-startup attach barrier
/// ([`EngineConfig::attaches_late`] drops `--headless` for it), so the
/// child parks before it sources `init.lua` at all and the UI is in force
/// by the time the hook runs. nvim fires its own `UIEnter` once `VimEnter`
/// returns, and a second one from here would run every non-`once` handler
/// twice -- which is what the reading taken on the callback's first line
/// exists to prevent, and what nothing else in this tree reads.
///
/// Unix only, because the relay is: the descriptor is handed to the child
/// by `dup2` between `fork` and `exec`.
#[cfg(unix)]
#[test]
fn a_spawn_that_kept_nvims_own_attach_barrier_gets_one_uienter() {
    let dir = pin_config();
    let dev_null = std::fs::File::open("/dev/null").expect("/dev/null always opens");
    let mut engine = Engine::spawn(
        EngineConfig::isolated()
            .with_arg("-u")
            .with_arg(dir.join("init.lua"))
            .with_late_attach(120, 40)
            .with_stdin_relay(dev_null.into()),
    )
    .unwrap();
    let (tx, rx) = mpsc::sync_channel(256);
    let (_pump, cutover) = engine.start_pump(tx);

    // the attach first, and not behind the answer: this child sources
    // nothing until a UI is there, so waiting for `view_vim_enter` before
    // attaching would wait for an event the barrier is holding back
    engine
        .handle
        .ui_attach(120, 40, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    let token = vim_enter_token(cutover.presink, &rx)
        .expect("the child never asked view_vim_enter, so nothing here was measured");
    engine.handle.reply(token, ReplyValue::Nil).unwrap();

    let read = settled(&engine);

    assert_eq!(
        read.uis_at_vim_enter, 1,
        "the barrier holds startup until a UI attaches, so this child runs \
         its config with one in force"
    );
    assert_eq!(
        read.uienters, 1,
        "nvim fires its own UIEnter for a UI the barrier waited for, so the \
         hook must fire nothing: two events run every non-once UIEnter \
         autocommand twice"
    );
    assert_eq!(
        read.chan, 0,
        "nvim's own event puts the channel in v:event, which a callback \
         reads as no args.data at all -- so a chan read here is view's \
         event, fired where nvim had already fired one"
    );
}
