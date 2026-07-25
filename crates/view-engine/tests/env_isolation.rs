//! An isolated spawn must detach its child from the host's editor
//! configuration completely, not just from the host's `XDG_*_HOME`
//! directories: the variables in `view_engine::env` reach past `--clean` to
//! redirect a child's config, runtime files, plugin manifest, Lua modules or
//! startup commands, and a child reading one of them looks exactly like a
//! child reading nothing at all.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use view_engine::process::{Engine, EngineConfig};

/// The value planted in every variable under test. Recognizable on sight in
/// a failure message, and unlike anything Neovim would derive for itself.
const MARKER: &str = "view-host-config-leak-marker";

/// Every variable that must never reach an isolated child *and* whose
/// arrival a `getenv` probe can observe, enumerated here rather than read
/// from `view_engine::env::HOST_REDIRECT_VARS`.
///
/// Deliberate duplication: an oracle that plants and probes whatever the
/// list under test happens to contain cannot fail when an entry leaves that
/// list, since the entry stops being planted at the same moment it stops
/// being cleared. Dropping a name below is how a variable is retired from
/// this contract, and doing that is a decision, not an omission.
///
/// `NVIM_LISTEN_ADDRESS` is deliberately not here: Neovim consumes it during
/// startup and unsets it, so `getenv` answers `v:null` for it whether it
/// leaked or not, and probing it this way would pass green forever. Its own
/// test below reads the effect instead.
const MUST_NOT_LEAK: &[&str] = &[
    "VIMINIT",
    "EXINIT",
    "MYVIMRC",
    "MYGVIMRC",
    "VIM",
    "VIMRUNTIME",
    "NVIM_APPNAME",
    "NVIM_RPLUGIN_MANIFEST",
    "NVIM",
    "NVIM_LOG_FILE",
    "NVIM_NOTTYFAST",
    "LUA_PATH",
    "LUA_CPATH",
];

/// The two variables an isolated spawn must point somewhere empty rather
/// than clear, duplicated here for the same reason [`MUST_NOT_LEAK`] is: a
/// test that iterates the list under test cannot notice a name leaving it.
const MUST_BE_NEUTRALIZED: &[&str] = &["XDG_CONFIG_DIRS", "XDG_DATA_DIRS"];

/// A scratch directory under the build tree (never the system temp dir,
/// which is world-writable), holding this file's planted paths.
fn scratch(name: &str) -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    let dir = root.join("target").join("view-env-isolation");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// Asks the child which of [`MUST_NOT_LEAK`] carry [`MARKER`], returning
/// their names.
///
/// The comparison is against the planted value rather than against the
/// variable merely existing: Neovim derives and exports several of these
/// (`$VIM`, `$VIMRUNTIME`) for itself during startup, so their presence
/// proves nothing while their *value* distinguishes what the host handed
/// down from what the child computed.
fn leaked_vars(engine: &Engine) -> Vec<String> {
    let names = MUST_NOT_LEAK
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(",");
    let expr = format!(r#"join(map([{names}], 'getenv(v:val) ==# "{MARKER}" ? v:val : ""'), ",")"#);
    engine
        .handle
        .eval_str(&expr)
        .unwrap()
        .split(',')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// An isolated config with every host variable planted in the environment
/// the child would otherwise inherit them from.
fn planted_config() -> EngineConfig {
    let mut cfg = EngineConfig::isolated();
    for name in MUST_NOT_LEAK {
        cfg = cfg.with_env(*name, MARKER);
    }
    cfg
}

#[test]
fn an_isolated_child_reads_none_of_the_hosts_config_variables() {
    // a spawn failure here is a leak too, not an unrelated fault: the only
    // thing separating this child from an ordinary isolated one is a marker
    // planted in variables it is supposed to be blind to, and several of
    // them (a bogus $VIMRUNTIME above all) cripple a child that reads them
    let engine = Engine::spawn(planted_config())
        .expect("an isolated child failed to start with host config variables planted");
    let leaked = leaked_vars(&engine);
    assert!(
        leaked.is_empty(),
        "{leaked:?} reached the child, so an isolated spawn still answers \
         to the host's editor configuration"
    );
}

/// The control for the assertion above: the same probe, against a child
/// nothing clears anything for. Without it, a probe that could never see a
/// leak in the first place would pass just as quietly as one that proves the
/// clearing works.
#[test]
fn the_leak_probe_reports_a_variable_that_is_not_cleared() {
    // MYVIMRC alone, and a non-hermetic config rather than an isolated one
    // with its plan stripped (which cannot be done, by design): Neovim runs
    // $VIMINIT as an Ex command and resolves its runtime files through
    // $VIM/$VIMRUNTIME, so planting the marker in those without the clearing
    // to neutralize it would wedge or cripple the child rather than
    // demonstrate the probe
    let cfg = EngineConfig::default()
        .with_arg("--clean")
        .with_arg("-n")
        .with_env("MYVIMRC", MARKER);
    let engine = Engine::spawn(cfg).unwrap();
    assert_eq!(
        leaked_vars(&engine),
        vec!["MYVIMRC".to_string()],
        "the probe cannot see an uncleared variable, so it proves nothing \
         about a cleared one"
    );
}

#[test]
fn an_isolated_child_searches_no_system_wide_config_directory() {
    let planted = scratch("host-search-path");
    let planted = planted.display().to_string();
    let mut cfg = EngineConfig::isolated();
    for name in MUST_BE_NEUTRALIZED {
        cfg = cfg.with_env(*name, &planted);
    }
    let engine = Engine::spawn(cfg).unwrap();
    for name in MUST_BE_NEUTRALIZED {
        let seen = engine
            .handle
            .eval_str(&format!("getenv('{name}')"))
            .unwrap();
        assert_ne!(
            seen, planted,
            "{name} still names the host's own search path, so the child \
             sources whatever plugins it holds"
        );
    }
    // the variables are only the mechanism; 'runtimepath' is where a leaked
    // search path becomes sourced code, and it is what the child answers for
    let rtp = engine.handle.eval_str("&runtimepath").unwrap();
    assert!(
        !rtp.contains(&planted),
        "the host's search path reached 'runtimepath' ({rtp}), so the child \
         sources plugins from it"
    );
}

/// `NVIM_LISTEN_ADDRESS` cannot be probed the way [`MUST_NOT_LEAK`] is:
/// Neovim consumes it at startup and unsets it, so `getenv` reports nothing
/// for it in a leaking child and a clean one alike. What a leak produces is
/// a server socket at an address the host chose, which `serverlist()`
/// reports, and which fails outright if something already holds that
/// address: the startup message that failure emits is precisely the one that
/// parks an embedded child's `qa!` in `wait_return` with no UI to answer it.
#[test]
fn an_isolated_child_opens_no_server_socket_at_a_host_chosen_address() {
    let planted = scratch("host-listen.sock");
    let _ = std::fs::remove_file(&planted);
    let cfg = EngineConfig::isolated().with_env("NVIM_LISTEN_ADDRESS", &planted);
    let engine = Engine::spawn(cfg).unwrap();
    let servers = engine.handle.eval_str("string(serverlist())").unwrap();
    let planted = planted.display().to_string();
    assert!(
        !servers.contains(&planted),
        "the child listens at the host's own address ({servers}), so every \
         measured session answers to whoever holds it"
    );
}

/// The control for the socket assertion above, and the reason it is not
/// written as a `getenv` probe: the same planted address does reach a child
/// nothing clears it for, and reaches it as a listening socket rather than
/// as a readable variable.
#[test]
fn the_socket_probe_reports_an_address_that_is_not_cleared() {
    let planted = scratch("uncleared-listen.sock");
    let _ = std::fs::remove_file(&planted);
    let cfg = EngineConfig::default()
        .with_arg("--clean")
        .with_arg("-n")
        .with_env("NVIM_LISTEN_ADDRESS", &planted);
    let engine = Engine::spawn(cfg).unwrap();
    let servers = engine.handle.eval_str("string(serverlist())").unwrap();
    let getenv = engine
        .handle
        .eval_str("string(getenv('NVIM_LISTEN_ADDRESS'))")
        .unwrap();
    let planted_str = planted.display().to_string();
    assert!(
        servers.contains(&planted_str),
        "the probe cannot see an uncleared listen address ({servers}), so it \
         proves nothing about a cleared one"
    );
    assert_eq!(
        getenv, "v:null",
        "getenv can see this variable after all, so the effect-based probe \
         above is answering a question a plain leak probe could have"
    );
    drop(engine);
    let _ = std::fs::remove_file(&planted);
}
