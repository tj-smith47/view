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
use view_engine::EngineError;

fn fixture(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/test-fixtures")
        .join(name)
        .canonicalize()
        .expect("the test fixtures are committed alongside the crate");
    assert!(path.is_file(), "{path:?} is not a file");
    path
}

fn fake_ssh() -> PathBuf {
    fixture("fake-ssh")
}

fn stub_spec() -> RemoteSpec {
    RemoteSpec::new("view-test-host").with_ssh_bin(fake_ssh())
}

/// A scratch directory of this test's own, removed when the guard drops.
///
/// Directories only. An executable a test writes and then runs is a race in
/// a parallel test binary: a sibling test's `fork` between the write and the
/// `exec` still holds the file open, and the `exec` fails with `ETXTBSY`.
/// The programs these tests run are committed fixtures for that reason.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "view-remote-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&path).expect("a scratch directory for this test");
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A config whose child ignores the host's own editor configuration without
/// the hermetic plan. Plain `--clean` rather than
/// [`EngineConfig::isolated`], because the tests below are about what a
/// caller's own overrides do on the far side and the hermetic layer applies
/// after them: it removes the redirect variables outright, which is the
/// whole subject of the isolated test further down and would make an
/// override test assert the wrong thing.
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

/// A removal must reach the far side, because the far side has something to
/// remove: a remote editor started over ssh inherits the remote user's login
/// environment (sshd and PAM, then the login shell's own startup files), and
/// a redirect variable set there reaches it exactly as one set here would.
/// The wrapper below stands in for that login environment, which is the only
/// part of a real connection the stub client does not already reproduce.
#[test]
fn an_environment_removal_unsets_what_the_remote_login_environment_exported() {
    let spec = || RemoteSpec::new("view-test-host").with_ssh_bin(fixture("fake-ssh-login-env"));
    let base = || {
        EngineConfig::default()
            .with_arg("--clean")
            .with_arg("-n")
            .with_handshake_timeout(Duration::from_secs(10))
    };

    let inherited =
        Engine::spawn(base().with_remote(spec())).expect("a remote spawn must handshake");
    assert_eq!(
        inherited
            .handle
            .eval_str("getenv('VIEW_REMOTE_PLANTED')")
            .unwrap(),
        "from-the-remote-login-shell",
        "the far side must actually export this, or the removal below \
         proves nothing"
    );

    let removed = Engine::spawn(
        base()
            .with_env_remove("VIEW_REMOTE_PLANTED")
            .with_remote(spec()),
    )
    .expect("a remote spawn must handshake");
    assert_eq!(
        removed
            .handle
            .eval_str("string(getenv('VIEW_REMOTE_PLANTED'))")
            .unwrap(),
        "v:null",
        "a removal was accepted and dropped: the remote editor still reads a \
         variable the caller asked to be rid of, and can be redirected by it"
    );
}

/// An isolated config now reaches the far side, and this is what arrives
/// there: the login environment's redirect variable gone, the two search
/// paths pointed at something no plugin can be planted under, and the
/// remote user's own home left standing.
///
/// Read out of the started editor rather than off the command line, for the
/// reason every other test here does: the line looking right proves the
/// caller's intent, not what survived a shell's re-parse.
#[test]
fn an_isolated_remote_spawn_neutralizes_the_far_sides_login_environment() {
    let spec = || RemoteSpec::new("view-test-host").with_ssh_bin(fixture("fake-ssh-login-env"));
    let planted = Engine::spawn(
        EngineConfig::default()
            .with_arg("--clean")
            .with_arg("-n")
            .with_handshake_timeout(Duration::from_secs(10))
            .with_remote(spec()),
    )
    .expect("a remote spawn must handshake");
    assert_eq!(
        planted.handle.eval_str("getenv('NVIM_APPNAME')").unwrap(),
        "from-the-remote-login-shell",
        "the far side must actually export this, or the isolated spawn below \
         proves nothing"
    );

    let isolated = Engine::spawn(
        EngineConfig::isolated()
            .with_handshake_timeout(Duration::from_secs(10))
            .with_remote(spec()),
    )
    .expect("an isolated remote spawn must handshake");
    assert_eq!(
        isolated
            .handle
            .eval_str("string(getenv('NVIM_APPNAME'))")
            .unwrap(),
        "v:null",
        "the hermetic plan's removals did not cross: the remote editor still \
         reads a redirect variable the far side's own login shell exported"
    );
    for name in ["XDG_CONFIG_DIRS", "XDG_DATA_DIRS"] {
        assert_eq!(
            isolated
                .handle
                .eval_str(&format!("getenv('{name}')"))
                .unwrap(),
            view_engine::env::REMOTE_UNPLANTABLE_PATH,
            "{name} reached the far side as something other than the \
             substitute: a path this host prepared names an unknown directory \
             there, and an unset value selects the remote system's own"
        );
    }
    // the stub runs the "remote" command on this host, so the far side's
    // login environment is this process's own, and its `HOME` is the value
    // the exemption promises to leave alone
    let login_home = std::env::var("HOME").expect("a unix test process has a home");
    assert_eq!(
        isolated.handle.eval_str("getenv('HOME')").unwrap(),
        login_home,
        "the far side did not keep its own home; removed or replaced, the \
         remote editor resolves `~` to somewhere this host chose or to \
         nothing at all"
    );
}

/// The other direction of the same plan, asserted so it cannot be mistaken
/// for coverage nobody checked: a variable the far side's login environment
/// exports that no list in `view_engine::env` names reaches the remote
/// editor intact.
///
/// A command line can only remove names somebody enumerated, and no
/// enumeration written here can be complete about a shell this host has
/// never seen -- `EngineConfig::env_plan` says so, and this is that sentence
/// as an observation. A change that closed the class (neutralization running
/// on the far side rather than riding a command line) fails here, which is
/// the point: the boundary moves deliberately, not by accident.
#[test]
fn a_far_side_variable_no_list_names_survives_an_isolated_remote_spawn() {
    let isolated = Engine::spawn(
        EngineConfig::isolated()
            .with_handshake_timeout(Duration::from_secs(10))
            .with_remote(
                RemoteSpec::new("view-test-host").with_ssh_bin(fixture("fake-ssh-login-env")),
            ),
    )
    .expect("an isolated remote spawn must handshake");
    assert_eq!(
        isolated
            .handle
            .eval_str("getenv('VIEW_REMOTE_PLANTED')")
            .unwrap(),
        "from-the-remote-login-shell",
        "a far-side variable outside every list here no longer reaches the \
         remote editor: either the plan gained a way to neutralize what it \
         cannot enumerate, in which case its documentation is now wrong, or \
         the fixture stopped planting one"
    );
}

/// The local path carries an `OsStr` to its child untouched, and the remote
/// path must not be less faithful. A filename that is not valid UTF-8 is
/// ordinary on a POSIX filesystem; a lossy decode would open, and on write
/// create, a different path than the caller named, silently.
///
/// Observed on the far side of a real join-and-re-parse rather than in the
/// argument vector: the probe records what the executed process itself
/// received. It speaks no RPC, so the spawn fails afterwards, which is not
/// what this test is about.
#[test]
fn a_non_utf8_argument_and_value_reach_the_executed_process_byte_for_byte() {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let scratch = Scratch::new("raw-bytes");
    let out = scratch.0.join("observed");
    let probe = fixture("remote-probe");

    // a latin-1 e-acute, in a filename and in a path-shaped value: both are
    // the shape `view_engine::env`'s redirect variables actually carry
    let file = OsString::from_vec(b"caf\xe9.md".to_vec());
    let value = OsString::from_vec(b"/home/caf\xe9/config/init.lua".to_vec());
    let cfg = EngineConfig::default()
        .with_env("VIEW_PROBE_OUT", &out)
        .with_env("VIEW_PROBE_VALUE", &value)
        .with_arg(&file)
        .with_handshake_timeout(Duration::from_secs(5))
        .with_remote(stub_spec().with_remote_nvim_bin(probe.display().to_string()));
    // the probe answers no handshake; what it does before exiting is the
    // subject here
    let _ = Engine::spawn(cfg);

    let observed = std::fs::read(&out).expect("the probe must have run on the far side");
    for needle in [value.as_bytes(), file.as_bytes()] {
        assert!(
            observed
                .windows(needle.len())
                .any(|window| window == needle),
            "{:?} did not reach the executed process unchanged; it observed \
             {:?}",
            std::ffi::OsStr::from_bytes(needle),
            std::ffi::OsStr::from_bytes(&observed)
        );
    }
    assert!(
        !observed
            .windows(3)
            .any(|window| window == [0xef, 0xbf, 0xbd]),
        "a replacement character reached the far side, so the remote editor \
         was pointed somewhere the caller did not name; it observed {:?}",
        std::ffi::OsStr::from_bytes(&observed)
    );
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
        // the variant, not its rendering: a timeout here means the failure
        // was waited out rather than reported, which is the hang this
        // disproves and is invisible in a message
        Err(EngineError::Timeout { method, timeout }) => {
            format!("the spawn waited out {timeout:?} for {method}")
        }
        Err(err) => err.to_string(),
    };
    assert!(
        !outcome.starts_with("the spawn was accepted") && !outcome.starts_with("the spawn waited"),
        "a remote target with no editor on it must fail the spawn, and fail \
         by report rather than by timeout: {outcome}"
    );
    // bounded well below the handshake timeout on purpose: a bound of the
    // timeout itself would pass a regression that hangs until just short of
    // it, which is the failure this test exists to catch
    let prompt = Duration::from_secs(2);
    assert!(
        elapsed < prompt && prompt < handshake,
        "the failure took {elapsed:?}: the far side's own refusal must reach \
         the caller immediately, not be waited for"
    );
}
