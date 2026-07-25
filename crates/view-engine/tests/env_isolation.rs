//! An isolated spawn must detach its child from the host's editor
//! configuration completely, not just from the host's `XDG_*_HOME`
//! directories: the variables in `view_engine::env` reach past `--clean` to
//! redirect a child's config, runtime files, plugin manifest or startup
//! commands, and a child reading one of them looks exactly like a child
//! reading nothing at all.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use view_engine::env::HOST_SEARCH_PATH_VARS;
use view_engine::process::{Engine, EngineConfig};

/// The value planted in every variable under test. Recognizable on sight in
/// a failure message, and unlike anything Neovim would derive for itself.
const MARKER: &str = "view-host-config-leak-marker";

/// Every variable that must never reach an isolated child, enumerated here
/// rather than read from `view_engine::env::HOST_REDIRECT_VARS`.
///
/// Deliberate duplication: an oracle that plants and probes whatever the
/// list under test happens to contain cannot fail when an entry leaves that
/// list, since the entry stops being planted at the same moment it stops
/// being cleared. Dropping a name below is how a variable is retired from
/// this contract, and doing that is a decision, not an omission.
const MUST_NOT_LEAK: &[&str] = &[
    "VIMINIT",
    "EXINIT",
    "MYVIMRC",
    "VIM",
    "VIMRUNTIME",
    "NVIM_APPNAME",
    "NVIM_RPLUGIN_MANIFEST",
    "NVIM",
    "NVIM_LISTEN_ADDRESS",
    "NVIM_LOG_FILE",
    "NVIM_NOTTYFAST",
];

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
        cfg.env
            .push((OsString::from(*name), OsString::from(MARKER)));
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

/// The control for the assertion above: the same probe, against a config
/// whose clearing has been removed. Without it, a probe that could never
/// see a leak in the first place would pass just as quietly as one that
/// proves the clearing works.
#[test]
fn the_leak_probe_reports_a_variable_that_is_not_cleared() {
    let mut cfg = planted_config();
    cfg.env_remove.clear();
    // MYVIMRC alone: Neovim runs $VIMINIT as an Ex command and resolves its
    // runtime files through $VIM/$VIMRUNTIME, so planting the marker in
    // those without the clearing to neutralize it would wedge or cripple
    // the child rather than demonstrate the probe
    cfg.env
        .retain(|(name, _)| name == OsString::from("MYVIMRC").as_os_str());
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
    let engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let expected = view_engine::env::empty_search_path();
    for name in HOST_SEARCH_PATH_VARS {
        let seen = engine
            .handle
            .eval_str(&format!("getenv('{name}')"))
            .unwrap();
        assert_eq!(
            seen,
            expected.display().to_string(),
            "{name} does not point at an empty directory, so the child \
             sources plugins from a system-wide default"
        );
    }
}
