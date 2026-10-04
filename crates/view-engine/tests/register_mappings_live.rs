//! Live-nvim proof of `REGISTER_MAPPINGS_CHUNK`'s restore-then-set contract
//! across two runs: a second `register_mappings` call is what a
//! `Stage::CapsUpgraded`/`Stage::ProfileFlip` reissue sends
//! (`crates/view/src/native.rs`'s `reissue_mappings`), and only a live nvim
//! can say what `maparg` reads after it.
//!
//! `nvim_api/mappings.rs`'s own unit tests pin the chunk's Lua source and
//! the wire shape of its arguments; neither says what nvim's keymap table
//! actually holds once the chunk has run twice.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::borrow::Cow;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use view_core::msg::Msg;
use view_core::native::mappings::{MappingSpec, Rhs};
use view_engine::process::{Engine, EngineConfig};

const TICK: Duration = Duration::from_millis(500);

/// The next `Msg::MappingsClaimed` on `rx`, every other message discarded.
fn next_claims(rx: &mpsc::Receiver<Msg>) -> Vec<view_core::native::mappings::MappingClaim> {
    loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::MappingsClaimed { claimed, .. }) => return claimed,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                panic!("no Msg::MappingsClaimed arrived within the deadline")
            }
        }
    }
}

fn chord(lhs: &'static str) -> MappingSpec {
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed(lhs),
        verb: "focus_left",
        rhs: Rhs::Keys("<C-w>h"),
    }
}

/// The pump and cutover are returned alongside the engine so the caller
/// keeps them alive for the test's whole body: dropped here, the pump would
/// stop routing replies before the test ever calls `register_mappings`.
fn spawn_attached() -> (
    Engine,
    u64,
    mpsc::Receiver<Msg>,
    view_engine::damage::DamagePump,
    view_engine::damage::SinkCutover,
) {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let channel = engine.api_info.channel_id;
    let (tx, rx) = mpsc::sync_channel(256);
    let (pump, cutover) = engine.start_pump(tx);
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    engine.handle.register_bridge(channel).unwrap();
    (engine, channel, rx, pump, cutover)
}

/// `:View` ends at a `|` the way nvim's own commands do: the feature gets
/// the verb before the bar, and the command after it runs.
#[test]
fn a_view_command_hands_a_trailing_bar_command_to_nvim() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    engine.handle.register_mappings(&[], channel).unwrap();
    let _ = next_claims(&rx);

    engine
        .handle
        .eval_str("execute('View ui panes tiles | vsplit')")
        .unwrap();
    let invoked = loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::FeatureInvoke { feature, verb, .. }) => break (feature, verb),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                panic!("no Msg::FeatureInvoke arrived within the deadline")
            }
        }
    };
    assert_eq!(invoked, ("ui".to_string(), "panes tiles".to_string()));
    assert_eq!(
        engine.handle.eval_str("winnr('$')").unwrap(),
        "2",
        "the vsplit after the bar never ran"
    );
}

/// The next `Msg::FeatureInvoke` on `rx`, as its feature and verb.
fn next_invoke(rx: &mpsc::Receiver<Msg>) -> (String, String) {
    loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::FeatureInvoke { feature, verb, .. }) => return (feature, verb),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                panic!("no Msg::FeatureInvoke arrived within the deadline")
            }
        }
    }
}

/// The path `:View dvr export` is given is the one a person typed: `~` is
/// the home directory, a relative path starts at nvim's current directory,
/// and a run of blanks stays in the name.
#[test]
fn an_export_path_reaches_view_as_typed_and_absolute() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    engine.handle.register_mappings(&[], channel).unwrap();
    let _ = next_claims(&rx);
    let dir = view_test_support::ScratchDir::new("export-path").unwrap();
    engine
        .handle
        .eval_str(&format!("execute('cd {}')", dir.path().display()))
        .unwrap();
    let home = engine.handle.eval_str("expand('~')").unwrap();
    let cwd = engine.handle.eval_str("getcwd()").unwrap();
    let cases = [
        ("View dvr export ~/x.vdvr", format!("export {home}/x.vdvr")),
        (
            "View dvr  export   a  b.vdvr",
            format!("export {cwd}/a  b.vdvr"),
        ),
        ("View dvr export", "export".to_owned()),
        ("View ui  panes tiles", "panes tiles".to_owned()),
    ];
    for (typed, want) in cases {
        engine
            .handle
            .eval_str(&format!("execute('{typed}')"))
            .unwrap();
        let feature = typed.split_whitespace().nth(1).unwrap().to_owned();
        assert_eq!(next_invoke(&rx), (feature, want), "{typed}");
    }
}

/// `:View dvr export` reads its path the way `:w` reads a file name: an
/// escaped blank, an escaped trailing blank, an escaped backslash before a
/// blank, an escaped `$`, an unclosed `${`, an environment variable, `~`
/// and a relative name after `:cd` each name the file `:w` writes. `HOME`
/// is pointed at a
/// scratch directory, so nothing is written outside it.
#[test]
fn an_export_path_names_the_file_w_writes() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    engine.handle.register_mappings(&[], channel).unwrap();
    let _ = next_claims(&rx);
    let dir = view_test_support::ScratchDir::new("export-like-w").unwrap();
    let home = dir.join("home");
    let cwd = dir.join("cwd");
    std::fs::create_dir_all(&home).unwrap();
    // a backslash is a separator on Windows, where `a\\ b` names ` b` in `a`
    std::fs::create_dir_all(cwd.join("a")).unwrap();
    // a single-quoted Vimscript string keeps a Windows path's backslashes
    for setup in [
        format!("execute('let $HOME = ''{}''')", home.display()),
        format!("execute('cd {}')", cwd.display()),
    ] {
        engine.handle.eval_str(&setup).unwrap();
    }
    for typed in [
        r"a\ b.vdvr",
        r"t\ ",
        r"a\\ b",
        r"\$VIEW_UNSET_EXPORT.vdvr",
        "a${VIEW_UNSET_EXPORT.vdvr",
        "$HOME/e.vdvr",
        "~/t.vdvr",
        "rel.vdvr",
    ] {
        engine
            .handle
            .eval_str(&format!("execute('View dvr export {typed}')"))
            .unwrap();
        let (_, verb) = next_invoke(&rx);
        let exported = verb.strip_prefix("export ").unwrap().to_owned();
        assert!(!std::path::Path::new(&exported).exists(), "{typed}");
        engine
            .handle
            .eval_str(&format!("execute('silent write {typed}')"))
            .unwrap();
        assert!(
            std::path::Path::new(&exported).is_file(),
            "{typed}: :w wrote elsewhere than {exported}"
        );
    }
}

/// A path naming an environment variable that is not set is refused with a
/// message naming it, and nothing reaches view, so no clip is written. A
/// name nvim never reads as a variable is no refusal: `$$x` expands `$$`.
#[test]
fn an_export_path_naming_an_unset_variable_is_refused() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    engine.handle.register_mappings(&[], channel).unwrap();
    let _ = next_claims(&rx);
    let dir = view_test_support::ScratchDir::new("export-unset").unwrap();
    engine
        .handle
        .eval_str(&format!("execute('cd {}')", dir.path().display()))
        .unwrap();
    for typed in ["$VIEW_UNSET_EXPORT/a.vdvr", "${VIEW_UNSET_EXPORT}/a.vdvr"] {
        let said = engine
            .handle
            .eval_str(&format!("execute('View dvr export {typed}')"))
            .unwrap();
        assert!(
            said.contains("view: DVR cannot export: $VIEW_UNSET_EXPORT is not set"),
            "{typed}: {said:?}"
        );
        // an export sent ahead of this one would be the next invoke
        engine
            .handle
            .eval_str("execute('View ui panes tiles')")
            .unwrap();
        assert_eq!(
            next_invoke(&rx),
            ("ui".to_owned(), "panes tiles".to_owned()),
            "{typed}"
        );
    }
    engine
        .handle
        .eval_str("execute('View dvr export $$x')")
        .unwrap();
    let (_, verb) = next_invoke(&rx);
    let cwd = engine.handle.eval_str("getcwd()").unwrap();
    let path = std::path::Path::new(verb.strip_prefix("export ").unwrap());
    assert_eq!(path.parent(), Some(std::path::Path::new(&cwd)), "{verb}");
    // a unix shell expands `$$` to its own pid, a new one on every
    // expansion, so the name is compared by shape
    if cfg!(unix) {
        let name = path.file_name().unwrap().to_str().unwrap();
        let pid = name.strip_suffix('x').unwrap_or_default();
        assert!(
            !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()),
            "{verb}"
        );
    }
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
}

/// A key that invokes view answers with the keys nvim matches for it, the
/// leader resolved, and a chord sending nvim keys of its own answers with
/// none.
#[test]
fn an_invoking_claim_carries_the_keys_nvim_matches() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    engine
        .handle
        .eval_str("execute('let mapleader = \" \"')")
        .unwrap();
    let specs = [
        MappingSpec {
            feature: "ai",
            lhs: Cow::Borrowed("<leader>ai"),
            verb: "toggle",
            rhs: Rhs::Invoke,
        },
        MappingSpec {
            feature: "window",
            lhs: Cow::Borrowed("<S-M-Left>"),
            verb: "close",
            rhs: Rhs::Invoke,
        },
        chord("<D-Left>"),
    ];
    engine.handle.register_mappings(&specs, channel).unwrap();
    let claimed = next_claims(&rx);
    let keys = |lhs: &str| {
        claimed
            .iter()
            .find(|c| c.lhs == lhs)
            .unwrap_or_else(|| panic!("no claim for {lhs}: {claimed:?}"))
            .keys
            .clone()
    };
    assert_eq!(keys("<leader>ai").as_deref(), Some("<Space>ai"));
    assert_eq!(keys("<S-M-Left>").as_deref(), Some("<M-S-Left>"));
    assert_eq!(keys("<D-Left>"), None);
}

/// The registration reads the user's own normal-mode keys, the leader
/// resolved, and `'timeoutlen'`, ahead of the claims in the same reply.
#[test]
fn a_registration_reads_the_users_own_keys_and_timeoutlen() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    for setup in [
        "execute('let mapleader = \" \"')",
        "execute('nnoremap <leader>fg :echo<CR>')",
        "execute('set timeoutlen=300')",
    ] {
        engine.handle.eval_str(setup).unwrap();
    }
    engine.handle.register_mappings(&[], channel).unwrap();
    let (keys, timeoutlen) = loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::UserMappingsRead {
                keys, timeoutlen, ..
            }) => break (keys, timeoutlen),
            Ok(Msg::MappingsClaimed { .. }) => panic!("the claims came before the user's keys"),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                panic!("no Msg::UserMappingsRead arrived within the deadline")
            }
        }
    };
    assert!(keys.iter().any(|k| k == "<Space>fg"), "{keys:?}");
    assert_eq!(timeoutlen, Some(Duration::from_millis(300)));
}

/// A mapping of the user's that one of view's keys replaced is no key of
/// theirs any more, since nvim runs view's.
#[test]
fn a_registration_leaves_out_the_users_keys_it_claimed() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    for setup in [
        "execute('let mapleader = \" \"')",
        "execute('nnoremap <leader>ff :echo<CR>')",
        "execute('nnoremap <leader>fg :echo<CR>')",
    ] {
        engine.handle.eval_str(setup).unwrap();
    }
    let specs = [MappingSpec {
        feature: "picker",
        lhs: Cow::Borrowed("<leader>ff"),
        verb: "files",
        rhs: Rhs::Invoke,
    }];
    engine.handle.register_mappings(&specs, channel).unwrap();
    let (keys, _) = next_user_keys(&rx);
    assert!(keys.iter().any(|k| k == "<Space>fg"), "{keys:?}");
    assert!(!keys.iter().any(|k| k == "<Space>ff"), "{keys:?}");
}

/// The next `Msg::UserMappingsRead` on `rx`, every other message discarded.
fn next_user_keys(rx: &mpsc::Receiver<Msg>) -> (Vec<String>, Option<Duration>) {
    loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::UserMappingsRead {
                keys, timeoutlen, ..
            }) => return (keys, timeoutlen),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                panic!("no Msg::UserMappingsRead arrived within the deadline")
            }
        }
    }
}

/// The command-line mappings of the next `Msg::UserMappingsRead` on `rx`,
/// as `(lhs, rhs, abbr, expr)`.
fn next_cmdline_maps(rx: &mpsc::Receiver<Msg>) -> Vec<(String, String, bool, bool)> {
    loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::UserMappingsRead { cmdline, .. }) => {
                return cmdline
                    .into_iter()
                    .map(|map| (map.lhs, map.rhs, map.abbr, map.expr))
                    .collect()
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                panic!("no Msg::UserMappingsRead arrived within the deadline")
            }
        }
    }
}

fn row(lhs: &str, rhs: &str, abbr: bool, expr: bool) -> (String, String, bool, bool) {
    (lhs.to_string(), rhs.to_string(), abbr, expr)
}

/// The registration reads the user's command-line abbreviations and
/// mappings from the real engine, an `<expr>` one flagged, and a
/// `cabbrev` a config sets on `User VeryLazy` is read again on the bridge.
#[test]
fn a_registration_reads_the_users_command_line_abbreviations_and_mappings() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    for setup in [
        "execute('cabbrev vo View ai open')",
        "execute('cnoremap vv View ai open')",
        "execute('cmap <expr> ww \"View\"')",
        "execute('autocmd User VeryLazy cabbrev vl View tree toggle')",
    ] {
        engine.handle.eval_str(setup).unwrap();
    }
    engine.handle.register_mappings(&[], channel).unwrap();
    let maps = next_cmdline_maps(&rx);
    for expected in [
        row("vo", "View ai open", true, false),
        row("vv", "View ai open", false, false),
        row("ww", "\"View\"", false, true),
    ] {
        assert!(maps.contains(&expected), "{expected:?} in {maps:?}");
    }
    assert!(!maps.iter().any(|map| map.0 == "vl"), "{maps:?}");

    engine
        .handle
        .eval_str("execute('doautocmd User VeryLazy')")
        .unwrap();
    let maps = next_cmdline_maps(&rx);
    assert!(
        maps.contains(&row("vl", "View tree toggle", true, false)),
        "{maps:?}"
    );

    // a sourced file and a command typed at the prompt fire none of the
    // buffer or lazy-load events
    let dir = view_test_support::ScratchDir::new("cmdline-maps-reread").unwrap();
    let sourced = dir.join("abbrevs.vim");
    std::fs::write(&sourced, "cabbrev vs View tree toggle\n").unwrap();
    engine
        .handle
        .eval_str(&format!("execute('source {}')", sourced.display()))
        .unwrap();
    let maps = next_cmdline_maps(&rx);
    assert!(
        maps.contains(&row("vs", "View tree toggle", true, false)),
        "{maps:?}"
    );
    engine.handle.input(":cabbrev vx View ai open<CR>").unwrap();
    let maps = next_cmdline_maps(&rx);
    assert!(
        maps.contains(&row("vx", "View ai open", true, false)),
        "{maps:?}"
    );
    engine.handle.input(":cunabbrev vx<CR>").unwrap();
    let maps = next_cmdline_maps(&rx);
    assert!(!maps.iter().any(|map| map.0 == "vx"), "{maps:?}");
}

/// A mapping a config sets on `User VeryLazy`, after the registration read
/// the user's keys, is read again and sent on the bridge, and view's own
/// keys stay out of it.
#[test]
fn a_mapping_set_on_very_lazy_is_read_again() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    for setup in [
        "execute('let mapleader = \" \"')",
        "execute('autocmd User VeryLazy nnoremap <leader>fz :echo<CR>')",
        "execute('set timeoutlen=400')",
    ] {
        engine.handle.eval_str(setup).unwrap();
    }
    let specs = [MappingSpec {
        feature: "ai",
        lhs: Cow::Borrowed("<leader>ai"),
        verb: "toggle",
        rhs: Rhs::Invoke,
    }];
    engine.handle.register_mappings(&specs, channel).unwrap();
    let (first, _) = next_user_keys(&rx);
    assert!(!first.iter().any(|k| k == "<Space>fz"), "{first:?}");

    engine
        .handle
        .eval_str("execute('doautocmd User VeryLazy')")
        .unwrap();
    let (keys, timeoutlen) = next_user_keys(&rx);
    assert!(keys.iter().any(|k| k == "<Space>fz"), "{keys:?}");
    assert!(!keys.iter().any(|k| k == "<Space>ai"), "{keys:?}");
    assert_eq!(timeoutlen, Some(Duration::from_millis(400)));
}

/// Opening a file raises `FileType`, `BufEnter` and `BufWinEnter`, and the
/// user's keys are walked once for the three. Each walk reads every global
/// normal-mode map, so one per event is paid on every buffer switch.
#[test]
fn opening_a_file_walks_the_users_keys_once() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    engine.handle.register_mappings(&[], channel).unwrap();
    let _ = next_user_keys(&rx);
    // counts the walks from outside the chunk, since nvim_get_keymap('n')
    // is read nowhere else in it once the registration has returned
    engine
        .handle
        .eval_str(
            "execute('lua local get = vim.api.nvim_get_keymap; \
             vim.g.walks = 0; \
             vim.api.nvim_get_keymap = function(m) \
             if m == \"n\" then vim.g.walks = vim.g.walks + 1 end \
             return get(m) end')",
        )
        .unwrap();
    let dir = view_test_support::ScratchDir::new("keys-walk").unwrap();
    let file = dir.join("walked.lua");
    engine
        .handle
        .eval_str(&format!("execute('edit {}')", file.display()))
        .unwrap();
    engine
        .handle
        .eval_str("execute('lua vim.wait(200, function() return false end)')")
        .unwrap();
    assert_eq!(engine.handle.eval_str("&filetype").unwrap(), "lua");
    assert_eq!(engine.handle.eval_str("g:walks").unwrap(), "1");
}

/// A change to `'timeoutlen'` or `'timeout'` is read again and sent, with
/// `None` while nvim waits for good.
#[test]
fn a_change_to_the_mapping_timeout_is_read_again() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    engine.handle.register_mappings(&[], channel).unwrap();
    let _ = next_user_keys(&rx);
    engine
        .handle
        .eval_str("execute('set timeoutlen=700')")
        .unwrap();
    let (_, timeoutlen) = next_user_keys(&rx);
    assert_eq!(timeoutlen, Some(Duration::from_millis(700)));
    engine.handle.eval_str("execute('set notimeout')").unwrap();
    let (_, timeoutlen) = next_user_keys(&rx);
    assert_eq!(timeoutlen, None);
}

/// An idle moment compares the maps nvim holds with the ones last read,
/// walks nothing more where they are the same, and reads a buffer mapping
/// a deferred callback set.
#[test]
fn an_idle_moment_reads_a_mapping_set_late() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    engine
        .handle
        .eval_str("execute('let mapleader = \" \"')")
        .unwrap();
    engine.handle.register_mappings(&[], channel).unwrap();
    let _ = next_user_keys(&rx);
    engine
        .handle
        .eval_str(
            "execute('lua local get = vim.api.nvim_get_keymap; \
             vim.g.walks = 0; \
             vim.api.nvim_get_keymap = function(m) \
             if m == \"n\" then vim.g.walks = vim.g.walks + 1 end \
             return get(m) end')",
        )
        .unwrap();
    let idle = || {
        for step in [
            "execute('doautocmd CursorHold')",
            "execute('lua vim.wait(100, function() return false end)')",
        ] {
            engine.handle.eval_str(step).unwrap();
        }
    };
    idle();
    assert_eq!(engine.handle.eval_str("g:walks").unwrap(), "1");

    engine
        .handle
        .eval_str(
            "execute('lua vim.defer_fn(function() \
             vim.keymap.set(\"n\", \"<leader>hs\", \":echo<CR>\", \
             { buffer = 0 }) end, 10)')",
        )
        .unwrap();
    engine
        .handle
        .eval_str("execute('lua vim.wait(100, function() return false end)')")
        .unwrap();
    idle();
    let (keys, _) = next_user_keys(&rx);
    assert!(keys.iter().any(|k| k == "<Space>hs"), "{keys:?}");
}

/// A plugin lazy.nvim loads on a tick after a file open's walk has run
/// raises its own `User LazyLoad`, and the map it set is read by a walk of
/// its own.
#[test]
fn a_lazy_load_after_a_file_open_walks_the_users_keys_again() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    engine
        .handle
        .eval_str("execute('let mapleader = \" \"')")
        .unwrap();
    engine.handle.register_mappings(&[], channel).unwrap();
    let _ = next_user_keys(&rx);
    let dir = view_test_support::ScratchDir::new("keys-rewalk").unwrap();
    let file = dir.join("opened.lua");
    engine
        .handle
        .eval_str(&format!("execute('edit {}')", file.display()))
        .unwrap();
    // lets the open's scheduled walk run before the map exists, so only a
    // later walk can read it
    engine
        .handle
        .eval_str("execute('lua vim.wait(200, function() return false end)')")
        .unwrap();
    for later in [
        "execute('nnoremap <leader>lz :echo<CR>')",
        "execute('doautocmd User LazyLoad')",
    ] {
        engine.handle.eval_str(later).unwrap();
    }
    let (keys, _) = next_user_keys(&rx);
    assert!(keys.iter().any(|k| k == "<Space>lz"), "{keys:?}");
}

/// nvim's own default mappings are among the user's keys, and a surface
/// with a window of its own passes them on the way a tile does. Its buffer
/// is not modifiable, so a default that edits (`&` repeating the last `:s`,
/// the `gcc` comment toggle) changes nothing there, and the buffer the
/// last `:s` ran in is left alone too.
#[test]
fn a_default_mapping_typed_in_a_surface_window_changes_no_buffer() {
    let (engine, _channel, _rx, _pump, _cutover) = spawn_attached();
    for setup in [
        "execute('call setline(1, \"axa\")')",
        "execute('s/x/y/')",
        "execute('call setline(1, \"axa\")')",
    ] {
        engine.handle.eval_str(setup).unwrap();
    }
    let win = engine
        .handle
        .open_native_window_sync(
            view_core::native::geometry::NativeSurface::Tree,
            view_core::msg::WinSplit::Left,
            30,
            true,
        )
        .unwrap()
        .expect("the surface window opened");
    // text in the surface buffer for the defaults to match, put there
    // around its own modifiable setting, which is what is under test
    engine
        .handle
        .eval_str(
            "execute('lua local m = vim.bo.modifiable; \
             vim.bo.modifiable = true; \
             vim.api.nvim_buf_set_lines(0, 0, -1, false, {\"axa\"}); \
             vim.bo.modifiable = m; vim.bo.commentstring = \"#%s\"')",
        )
        .unwrap();
    let surface_lines = || {
        engine
            .handle
            .eval_str("join(getline(1, '$'), '|')")
            .unwrap()
    };
    assert_eq!(surface_lines(), "axa");
    for keys in ["&", "gcc"] {
        engine.handle.feed_keys(keys).unwrap();
        engine.handle.eval_str("1").unwrap();
    }
    assert_eq!(
        engine.handle.eval_str("win_getid()").unwrap(),
        win.0.to_string(),
        "the keys ran outside the surface window"
    );
    assert_eq!(surface_lines(), "axa");
    assert_eq!(engine.handle.eval_str("&modifiable").unwrap(), "0");
    assert_eq!(
        engine
            .handle
            .eval_str("join(getbufline(1, 1, '$'), '|')")
            .unwrap(),
        "axa"
    );
}

/// A second registration that reissues the same chord must not report it as
/// taken from a user: the previous run's own claim is not a user mapping,
/// so the reissue's own claim for the same key must answer
/// `had_user_mapping: false`.
#[test]
fn a_reissue_claims_no_key_from_itself() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    let specs = [chord("<D-Left>")];

    engine.handle.register_mappings(&specs, channel).unwrap();
    let first = next_claims(&rx);
    let first_claim = first
        .iter()
        .find(|c| c.lhs == "<D-Left>")
        .expect("the first run must claim <D-Left>");
    assert!(
        !first_claim.had_user_mapping,
        "nothing was mapped there before the first run: {first:?}"
    );

    engine.handle.register_mappings(&specs, channel).unwrap();
    let second = next_claims(&rx);
    let second_claim = second
        .iter()
        .find(|c| c.lhs == "<D-Left>")
        .expect("the reissue must claim <D-Left>");
    assert!(
        !second_claim.had_user_mapping,
        "a reissue over its own prior registration is not a user mapping taken: {second:?}"
    );
}

/// A registration that lands on a user's own mapping, then a reissue that
/// drops the chord (a flip to the editor profile), must give the user's
/// mapping back -- `maparg` after the second run reads what the user
/// wrote.
#[test]
fn a_flip_gives_back_the_user_mapping_it_took() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    let user_rhs = ":echo 'mine'<CR>";
    engine
        .handle
        .request(
            "nvim_set_keymap",
            vec![
                rmpv::Value::from("n"),
                rmpv::Value::from("<D-Left>"),
                rmpv::Value::from(user_rhs),
                rmpv::Value::Map(vec![(
                    rmpv::Value::from("noremap"),
                    rmpv::Value::from(true),
                )]),
            ],
        )
        .expect("planting the user's own mapping");

    let specs = [chord("<D-Left>")];
    engine.handle.register_mappings(&specs, channel).unwrap();
    let claimed = next_claims(&rx);
    let claim = claimed
        .iter()
        .find(|c| c.lhs == "<D-Left>")
        .expect("the first run must claim <D-Left>");
    assert!(
        claim.had_user_mapping,
        "the user's own mapping was there before the first run: {claimed:?}"
    );

    // the flip to the editor profile: no chords in the reissue's specs
    engine.handle.register_mappings(&[], channel).unwrap();
    let _ = next_claims(&rx);

    let read_back = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("return vim.fn.maparg(..., 'n')"),
                rmpv::Value::Array(vec![rmpv::Value::from("<D-Left>")]),
            ],
        )
        .expect("reading maparg back after the flip");
    assert_eq!(
        read_back.as_str(),
        Some(user_rhs),
        "the flip must give the user's own mapping back, read: {read_back:?}"
    );
}

/// Every chord's `with_super` spelling beside its `with_alt` spelling, both
/// registered in the one session: `no_two_chords_share_a_spelling_under_
/// either_modifier` (`chords.rs`) pins this over the table alone, and this
/// is nvim's own answer once the 92 rows are real keymaps. Each of the 92
/// must claim (nothing was mapped there before), `maparg` must answer
/// non-empty for each, and `keytrans` must read 92 distinct forms -- proof
/// that no two of the table's spellings collapse to the same key once nvim
/// normalizes them.
#[test]
fn every_chord_spelling_registers_as_its_own_key() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();

    // built with `DesktopChord::lhs`, the same call `view-native`'s
    // `chord_plan` makes to respell a derived row under a settled modifier
    // -- view-engine cannot depend on view-native (dependency direction),
    // so this reads the production spelling function directly, past the
    // compiled-in `with_super`/`with_alt` fields.
    use view_core::native::chords::DesktopModifier;
    let mut specs = Vec::new();
    for chord in view_core::native::chords::desktop_chords() {
        specs.push(MappingSpec {
            feature: chord.feature,
            lhs: Cow::Borrowed(chord.lhs(DesktopModifier::Super)),
            verb: chord.verb,
            rhs: chord.rhs,
        });
        specs.push(MappingSpec {
            feature: chord.feature,
            lhs: Cow::Borrowed(chord.lhs(DesktopModifier::Alt)),
            verb: chord.verb,
            rhs: chord.rhs,
        });
    }
    let want = specs.len();
    assert_eq!(
        want,
        2 * view_core::native::chords::DESKTOP_CHORD_COUNT,
        "one super spelling and one alt spelling per chord"
    );

    engine.handle.register_mappings(&specs, channel).unwrap();
    let claimed = next_claims(&rx);
    let claimed_lhs: std::collections::BTreeSet<&str> =
        claimed.iter().map(|c| c.lhs.as_str()).collect();
    assert_eq!(
        claimed_lhs.len(),
        want,
        "every one of the {want} spellings must claim its own key: {claimed:?}"
    );
    for claim in &claimed {
        assert!(
            !claim.had_user_mapping,
            "a fresh session had nothing mapped under {}: {claimed:?}",
            claim.lhs
        );
    }

    let lhs_array = rmpv::Value::Array(
        specs
            .iter()
            .map(|s| rmpv::Value::from(s.lhs.as_ref()))
            .collect(),
    );
    let report = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    "local lhs = ...\n\
                     local hits, forms = {}, {}\n\
                     for i, l in ipairs(lhs) do\n\
                       hits[i] = vim.fn.maparg(l, 'n') ~= ''\n\
                       local raw =\n\
                         vim.api.nvim_replace_termcodes(l, true, true, true)\n\
                       forms[i] = vim.fn.keytrans(raw)\n\
                     end\n\
                     return { hits, forms }",
                ),
                rmpv::Value::Array(vec![lhs_array]),
            ],
        )
        .expect("reading maparg and keytrans for all 92 spellings");
    let pair = report.as_array().expect("the chunk returns [hits, forms]");
    let hits = pair[0].as_array().expect("hits crosses as an array");
    let forms = pair[1].as_array().expect("forms crosses as an array");
    assert_eq!(hits.len(), want);
    assert!(
        hits.iter().all(|h| h.as_bool() == Some(true)),
        "every one of the {want} spellings must answer maparg: {hits:?}"
    );
    let distinct: std::collections::BTreeSet<&str> =
        forms.iter().filter_map(rmpv::Value::as_str).collect();
    assert_eq!(
        distinct.len(),
        want,
        "the {want} spellings must read {want} distinct keytrans forms, read {forms:?}"
    );
}
