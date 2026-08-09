//! Live-nvim proof of the three CLI mechanisms the differential corpus
//! cannot express, because each is about what a real child process does
//! with its arguments, environment, and file descriptors rather than about
//! decoding a key script:
//!
//! - `:cq N` really propagates as the child's own exit code, which is what
//!   `Engine::wait_exit` (and `runtime::run`'s `Effect::Quit` mapping) reads.
//! - `--clean` really suppresses a user's own config, the way `nvim --clean`
//!   itself defines the flag -- distinct from `EngineConfig::isolated`,
//!   which this crate reserves for the oracle/measurement matrix.
//! - The stdin relay really lands piped content in nvim's first buffer via
//!   `stdin_fd`, the mechanism `ui_attach_with_stdin_relay` arms.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use view_engine::process::{Engine, EngineConfig};
use view_test_support::ScratchDir;

/// A fixture `nvim/init.lua` under its own `XDG_CONFIG_HOME`, so a run that
/// does not pass `--clean` sources it as an ordinary user config would, and
/// a run that does pass `--clean` proves the flag suppressed it -- not that
/// nothing pointed at it in the first place.
///
/// A global variable, not an option: nvim's own option defaults are
/// version-dependent (this pin's `laststatus` default is already `2`, the
/// same value an earlier draft of this fixture tried to distinguish
/// `--clean` with), while a global variable nobody but this file's
/// `init.lua` sets can only exist because that file ran.
fn config_home(name: &str) -> ScratchDir {
    let dir = ScratchDir::new(&format!("cli-live-{name}"));
    std::fs::create_dir_all(dir.join("nvim")).unwrap();
    std::fs::write(
        dir.join("nvim").join("init.lua"),
        "vim.g.view_config_probe = 1\n",
    )
    .unwrap();
    dir
}

/// The engine config `view`'s own `--clean` flag produces, per
/// `main::engine_config`: `EngineConfig::default()` plus the bare
/// `--clean` argument, never `EngineConfig::isolated`. Reproduced here
/// rather than calling into the bin crate, which an integration test
/// cannot link against; `main.rs`'s own
/// `clean_appends_only_the_clean_flag_never_isolateds_extra_n_or_hermetic_env`
/// pins that this is what it builds.
fn clean_cfg(config_home: &std::path::Path) -> EngineConfig {
    EngineConfig::default()
        .with_arg("--clean")
        .with_env("XDG_CONFIG_HOME", config_home)
}

fn unclean_cfg(config_home: &std::path::Path) -> EngineConfig {
    EngineConfig::default().with_env("XDG_CONFIG_HOME", config_home)
}

#[test]
fn without_clean_the_fixture_config_is_sourced() {
    let dir = config_home("sanity");
    let engine = Engine::spawn(unclean_cfg(&dir)).unwrap();
    engine.handle.ui_attach(80, 24).unwrap();
    assert_eq!(
        engine
            .handle
            .eval_str("exists('g:view_config_probe')")
            .unwrap(),
        "1",
        "the fixture's own init.lua never took effect, so --clean's test below proves nothing"
    );
}

#[test]
fn clean_suppresses_the_users_own_config() {
    let dir = config_home("clean");
    let engine = Engine::spawn(clean_cfg(&dir)).unwrap();
    engine.handle.ui_attach(80, 24).unwrap();
    assert_eq!(
        engine
            .handle
            .eval_str("exists('g:view_config_probe')")
            .unwrap(),
        "0",
        "--clean must suppress the fixture's init.lua, and nothing else \
         could have set g:view_config_probe"
    );
}

// `clean_never_carries_isolateds_extra_n_or_hermetic_environment` used to
// live here, re-spelling `main::engine_config`'s `--clean` shape against a
// test-local `clean_cfg` helper this file cannot assert is what production
// actually builds (an integration test cannot link against `main.rs`'s
// private items). `main.rs`'s own
// `clean_appends_only_the_clean_flag_never_isolateds_extra_n_or_hermetic_env`
// asserts the identical shape against the real `engine_config`, so this
// duplicate closed rather than moved.

#[test]
fn cq_propagates_as_the_childs_own_exit_code() {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    engine.handle.ui_attach(80, 24).unwrap();
    // `command`, not `input`/`feed_keys`: those two queue typeahead and
    // return before nvim has actually run it, which raced `wait_exit`'s own
    // `qa!` below into exiting the process first with code 0 on an earlier
    // run of this test. `command("cq 3")` runs synchronously and never
    // answers because the process exits mid-request, so its own `Err` is
    // the barrier: it only returns once the exit has genuinely already
    // happened.
    let barrier = engine.handle.command("cq 3");
    assert!(
        barrier.is_err(),
        "cq 3 must end the connection without a reply, got {barrier:?}"
    );
    let info = engine.wait_exit();
    assert_eq!(
        info.code,
        Some(3),
        ":cq 3 must reach the caller as the child's real exit status, \
         which runtime::run's Effect::Quit mapping and this process's own \
         exit code both read verbatim"
    );
    assert!(!info.by_signal, "a normal :cq exit is not a signal death");
}

#[cfg(unix)]
#[test]
fn piped_stdin_lands_in_the_first_buffer_via_the_relay_fd() {
    use std::os::fd::AsFd;

    // A regular file rather than an actual OS pipe: what `with_stdin_relay`
    // dup2s onto the child's fd 3 is any readable descriptor, and a file
    // exercises the identical dup2 path `ls | view -` would without this
    // crate depending on a pipe-construction crate just for a test.
    let dir = ScratchDir::new("cli-live-stdin-relay");
    let content = dir.join("stdin.txt");
    std::fs::write(
        &content,
        "hello from the pipe
",
    )
    .unwrap();
    let source = std::fs::File::open(&content).unwrap();

    let cfg = EngineConfig::isolated()
        .with_arg("-")
        .with_stdin_relay(source.as_fd().try_clone_to_owned().unwrap());
    let mut engine = Engine::spawn(cfg).unwrap();
    engine.handle.ui_attach_with_stdin_relay(80, 24).unwrap();

    assert_eq!(
        engine.handle.eval_str("getline(1)").unwrap(),
        "hello from the pipe",
        "the relayed fd's content must land in buffer 1, the way nvim's \
         own ui-startup-stdin documents stdin_fd behaving"
    );
    let _ = engine.wait_exit();
}
