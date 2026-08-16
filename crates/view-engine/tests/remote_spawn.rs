//! A remote spawn must hand the engine the same thing a local one does: a
//! child whose two pipes carry an msgpack-RPC session, reached through the
//! system `ssh` client instead of a local `nvim`.
//!
//! Every test here runs against `scripts/test-fixtures/fake-ssh`, a stand-in
//! that reproduces the one client behaviour this path turns on: the client
//! joins everything trailing the destination into a single string and hands
//! it to the remote user's shell to re-parse, rather than preserving the
//! caller's argument boundaries. A double that preserved them would run a
//! caller whose quoting is wrong and a real host would reject, so the
//! assertions below about what the far side observed are assertions about a
//! string that really was re-parsed by a shell.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};
use view_engine::process::{Engine, EngineConfig, RemoteSpec};

fn fake_ssh() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/test-fixtures/fake-ssh")
        .canonicalize()
        .expect("the stub client is committed alongside the crate");
    assert!(path.is_file(), "{path:?} is not a file");
    path
}

fn stub_spec() -> RemoteSpec {
    RemoteSpec::new("view-test-host").with_ssh_bin(fake_ssh())
}

/// A config whose child ignores the host's own editor configuration without
/// the hermetic plan, which a remote spawn refuses by construction: the
/// plan's directories are local, and the far side never receives them.
fn remote_clean() -> EngineConfig {
    EngineConfig::default()
        .with_arg("--clean")
        .with_arg("-n")
        .with_remote(stub_spec())
        .with_handshake_timeout(Duration::from_secs(10))
}

/// The stub's own fidelity, proven before anything is asserted through it:
/// a double that exec'd its trailing arguments directly would be strictly
/// more forgiving than the client it stands in for, and would pass a caller
/// whose quoting a real remote shell would break.
#[test]
fn the_stub_client_flattens_and_reparses_exactly_as_the_real_one_does() {
    let output = std::process::Command::new(fake_ssh())
        .arg("-T")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-p")
        .arg("22")
        .arg("view-test-host")
        .arg("echo")
        .arg("$((1+1))  spaced")
        .output()
        .expect("the stub client runs");
    let seen = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        seen,
        "2 spaced",
        "the stub must join its trailing arguments and let a shell re-parse \
         the result: arithmetic expands and a double space collapses. \
         Preserved argv boundaries would leave `$((1+1))  spaced` verbatim, \
         and every quoting assertion made through this stub would then be \
         vacuous. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The whole path end to end: `Engine::spawn` completes its handshake and
/// serves requests through a child reached over the client, with no change
/// to the spawn sequence beyond which program it starts.
#[test]
fn a_remote_spawn_handshakes_and_serves_requests_like_a_local_one() {
    let engine = Engine::spawn(remote_clean()).expect("a remote spawn must handshake");
    assert!(engine.api_info.channel_id >= 1);
    assert!(
        (engine.api_info.version_major, engine.api_info.version_minor) >= (0, 11),
        "nvim >= 0.11 required, found {}.{}",
        engine.api_info.version_major,
        engine.api_info.version_minor
    );
    let echoed = engine
        .handle
        .request("nvim_eval", vec![rmpv::Value::from("21 * 2")])
        .unwrap();
    assert_eq!(
        echoed.as_u64(),
        Some(42),
        "the RPC session must run over the client's pipes unchanged"
    );
}

/// The argument vector looking right proves nothing on its own: this asks
/// the process that actually started what its own environment holds, on the
/// far side of a real join-and-re-parse.
#[test]
fn an_environment_override_reaches_the_executed_remote_process() {
    let engine = Engine::spawn(remote_clean().with_env("NVIM_APPNAME", "work"))
        .expect("a remote spawn must handshake");
    assert_eq!(
        engine.handle.eval_str("getenv('NVIM_APPNAME')").unwrap(),
        "work",
        "the override did not cross the client boundary"
    );
}

/// The values that break a design passing them as separate arguments, or
/// escaping only what it judges to need escaping. Both shapes are what an
/// engine actually forwards: `view_engine::env`'s redirect variables are
/// path-shaped, and a path with a space in it is ordinary.
#[test]
fn adversarial_values_survive_the_remote_shells_reparse_byte_for_byte() {
    const SPACED: &str = "/home/a user/config/init.lua";
    const QUOTED: &str = "it's a value";
    const SHELLY: &str = "$(id); `id`; ; rm -rf /";
    let engine = Engine::spawn(
        remote_clean()
            .with_env("VIEW_REMOTE_SPACED", SPACED)
            .with_env("VIEW_REMOTE_QUOTED", QUOTED)
            .with_env("VIEW_REMOTE_SHELLY", SHELLY),
    )
    .expect("a remote spawn must handshake");
    for (name, expected) in [
        ("VIEW_REMOTE_SPACED", SPACED),
        ("VIEW_REMOTE_QUOTED", QUOTED),
        ("VIEW_REMOTE_SHELLY", SHELLY),
    ] {
        let seen = engine
            .handle
            .eval_str(&format!("getenv('{name}')"))
            .unwrap();
        assert_eq!(
            seen, expected,
            "{name} did not survive the remote shell's re-parse: a value \
             word-split, truncated at a quote, or expanded as syntax is a \
             value the remote editor never received"
        );
    }
}

/// Batch mode's guarantee, proven rather than assumed: a remote spawn that
/// cannot start an editor fails, and fails promptly. A client permitted to
/// prompt would sit on a question nothing embedded can answer, and the
/// caller would see a spawn that never returns rather than an error.
#[test]
fn a_missing_remote_editor_fails_loudly_instead_of_hanging() {
    let handshake = Duration::from_secs(10);
    let cfg = EngineConfig::default()
        .with_remote(stub_spec().with_remote_nvim_bin("/nonexistent/view-remote-nvim"))
        .with_handshake_timeout(handshake);
    let started = Instant::now();
    let refused = Engine::spawn(cfg);
    let elapsed = started.elapsed();
    let outcome = match refused {
        Ok(_) => String::from("the spawn was accepted"),
        Err(err) => err.to_string(),
    };
    assert_ne!(
        outcome, "the spawn was accepted",
        "a remote target with no editor on it must fail the spawn"
    );
    assert!(
        elapsed < handshake,
        "the failure took {elapsed:?}, which is the handshake timeout rather \
         than a reported error: the far side's own failure must reach the \
         caller, not be waited out"
    );
}
