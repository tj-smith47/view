//! An isolated spawn must detach its child from the host's editor
//! configuration completely, not just from the host's `XDG_*_HOME`
//! directories: the variables in `view_engine::env` reach past `--clean` to
//! redirect a child's config, runtime files, plugin manifest, Lua modules or
//! startup commands, and a child reading one of them looks exactly like a
//! child reading nothing at all.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::{PoisonError, RwLock};
use view_engine::process::{Engine, EngineConfig};

/// Orders the tests that remove or plant in the shared hermetic directories
/// (exclusive) against the isolated spawns whose funnel prepares them
/// (shared).
///
/// The preparation is a create/read/chmod sequence over paths every
/// isolated spawn in this binary shares, so a removal landing inside a
/// concurrent preparation surfaces as a spurious I/O failure from a test
/// that touched nothing. Only isolated spawns take the shared side: a
/// non-hermetic spawn never runs the preparation, and its child's `HOME` is
/// the host's, so it neither reads nor writes the shared directories.
///
/// Poisoning is ignored on both sides: the lock orders operations and
/// guards no data, so a test that panicked while holding it left nothing
/// behind for the next one to find broken.
///
/// The lock is per-test-binary, which suffices because `cargo test` -- the
/// gate's runner -- runs test binaries serially. A runner that interleaves
/// binaries (nextest) could land another binary's isolated spawn inside a
/// mutation here; that surfaces as the refusal naming the planted entry,
/// never as silent acceptance.
static HERMETIC_DIRS: RwLock<()> = RwLock::new(());

/// Runs `f` -- an isolated spawn plus everything driving its child -- with
/// the shared hermetic directories held stable.
///
/// Held for the child's whole lifetime rather than just the spawn: an
/// isolated child writes its state under the hermetic home, so an exclusive
/// section entered while a child is still alive would race that write, not
/// only the funnel's preparation.
fn with_prepared_dirs<R>(f: impl FnOnce() -> R) -> R {
    let _shared = HERMETIC_DIRS.read().unwrap_or_else(PoisonError::into_inner);
    f()
}

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

/// A listen address to plant in `NVIM_LISTEN_ADDRESS`, in whichever form
/// this platform's Neovim actually serves on and reports from
/// `serverlist()`: a unix socket under the build tree, or a named pipe on
/// Windows, which has no unix sockets at all.
///
/// The split is what keeps the pair of socket tests below honest on both
/// platforms. A unix-socket path planted on Windows could never appear in
/// `serverlist()` whether the child cleared it or not, which would leave the
/// isolation assertion passing exactly as quietly against a leaking child as
/// against a clean one -- the shape this file's own doc argues against.
fn planted_listen_address(name: &str) -> String {
    #[cfg(unix)]
    {
        let path = scratch(name);
        let _ = std::fs::remove_file(&path);
        path.display().to_string()
    }
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\view-{}-{}", name, std::process::id())
    }
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
    with_prepared_dirs(|| {
        // a spawn failure here is a leak too, not an unrelated fault: the
        // only thing separating this child from an ordinary isolated one is
        // a marker planted in variables it is supposed to be blind to, and
        // several of them (a bogus $VIMRUNTIME above all) cripple a child
        // that reads them
        let engine = Engine::spawn(planted_config())
            .expect("an isolated child failed to start with host config variables planted");
        let leaked = leaked_vars(&engine);
        assert!(
            leaked.is_empty(),
            "{leaked:?} reached the child, so an isolated spawn still answers \
             to the host's editor configuration"
        );
    });
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
    with_prepared_dirs(|| {
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
        // the variables are only the mechanism; 'runtimepath' is where a
        // leaked search path becomes sourced code, and it is what the child
        // answers for
        let rtp = engine.handle.eval_str("&runtimepath").unwrap();
        assert!(
            !rtp.contains(&planted),
            "the host's search path reached 'runtimepath' ({rtp}), so the \
             child sources plugins from it"
        );
    });
}

/// The assumption every assertion against [`MUST_NOT_LEAK`] rests on: each
/// name in it is one a `getenv` probe can actually observe. Neovim consumes
/// some variables during startup and unsets them, and a name that joins that
/// group when the engine pin moves would leave the probe blind to it while
/// every assertion above stayed green forever. `NVIM_LISTEN_ADDRESS` is the
/// known member of that group and has its own effect-based test below.
///
/// A plain headless child rather than an embedded one: the marker is
/// nonsense to the editor reading it (a bogus `$VIMRUNTIME` above all), and
/// an embedded child with no UI attached has nowhere to put the startup
/// message that provokes, while a headless one prints it and exits.
#[test]
fn every_probed_variable_is_one_getenv_can_report() {
    let names = MUST_NOT_LEAK
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(",");
    let expr = format!(
        r#"echo join(map([{names}], 'v:val . "=" . (getenv(v:val) ==# "{MARKER}" ? "SEEN" : "hidden")'), " ")"#
    );
    let mut child = std::process::Command::new("nvim");
    child.args(["--clean", "-n", "--headless", "-c", &expr, "-c", "qa!"]);
    for name in MUST_NOT_LEAK {
        child.env(name, MARKER);
    }
    // the marker is a relative path to the child, and several of these
    // variables name a file it *writes* ($NVIM_LOG_FILE above all), so
    // without a working directory of its own the child drops a file named
    // after the marker into the source tree
    let cwd = scratch("probe-cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    child.current_dir(&cwd);
    let out = child.output().unwrap();
    // both streams: which one a headless editor writes `:echo` to is its
    // choice, and a startup message provoked by the markers lands on the
    // other one
    let reported = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    for name in MUST_NOT_LEAK {
        assert!(
            reported.contains(&format!("{name}=SEEN")),
            "{name} is planted but the probe cannot see it, so every \
             assertion about it clearing passes whether it clears or not; \
             the child reported: {reported}"
        );
    }
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
    with_prepared_dirs(|| {
        let planted = planted_listen_address("host-listen.sock");
        let cfg = EngineConfig::isolated().with_env("NVIM_LISTEN_ADDRESS", &planted);
        let engine = Engine::spawn(cfg).unwrap();
        let servers = engine.handle.eval_str("string(serverlist())").unwrap();
        assert!(
            !servers.contains(&planted),
            "the child listens at the host's own address ({servers}), so \
             every measured session answers to whoever holds it"
        );
    });
}

/// The control for the socket assertion above, and the reason it is not
/// written as a `getenv` probe: the same planted address does reach a child
/// nothing clears it for, and reaches it as a listening socket rather than
/// as a readable variable.
#[test]
fn the_socket_probe_reports_an_address_that_is_not_cleared() {
    let planted = planted_listen_address("uncleared-listen.sock");
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
    assert!(
        servers.contains(&planted),
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

/// An isolated spawn must *establish* the search path its plan points a
/// child at, not merely name it: the preparation is where the emptiness
/// refusal runs, so a spawn that stopped calling it would keep every
/// plan-shaped assertion green while never refusing anything.
///
/// The directories are removed first, because one left behind by any
/// earlier spawn would satisfy the assertion whether or not this spawn
/// prepared it, and under [`HERMETIC_DIRS`]'s exclusive side, because the
/// removal ruins any preparation it lands in the middle of. The search path
/// carries the whole discrimination: nothing but the preparation ever
/// creates it, and the mode it leaves is one `create_dir_all` never yields.
/// The home's existence proves nothing by itself -- this spawn's own child
/// establishes a state directory under its `HOME` moments after starting --
/// so the home preparation is pinned by the refusal test below instead.
#[test]
fn an_isolated_spawn_establishes_the_directories_its_plan_points_at() {
    let _exclusive = HERMETIC_DIRS
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    let home = view_engine::env::hermetic_home();
    let empty = view_engine::env::empty_search_path();
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&empty);
    let engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    assert!(
        home.is_dir(),
        "the spawn pointed its child's HOME at {} without establishing it",
        home.display()
    );
    assert!(
        empty.is_dir(),
        "the spawn pointed its child's search path at {} without \
         establishing it, so the emptiness refusal never ran",
        empty.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&empty).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o500,
            "the spawn left the search path writable, so a plugin can be \
             planted in it between spawns"
        );
    }
    drop(engine);
}

/// An isolated spawn must *run* the plant refusal against the home its plan
/// points a child's `HOME` at: the directory existing proves nothing about
/// who prepared it (see the test above), but only the preparation can
/// refuse, so a planted credential file turning the spawn itself into a
/// refusal that names the plant is the one observation a child cannot fake.
#[test]
fn an_isolated_spawn_refuses_a_home_holding_a_planted_credential() {
    let _exclusive = HERMETIC_DIRS
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    let home = view_engine::env::hermetic_home();
    std::fs::create_dir_all(&home).unwrap();
    // unplanted on every exit path, panics included: a `.netrc` left behind
    // refuses every isolated spawn in every later test
    struct Unplant(PathBuf);
    impl Drop for Unplant {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _plant = Unplant(home.join(".netrc"));
    std::fs::write(home.join(".netrc"), "machine github.com").unwrap();
    let spawned = Engine::spawn(EngineConfig::isolated());
    // the map drops a wrongly-spawned engine, which shuts its child down
    // before the failure is reported
    let refused = spawned.map(drop).expect_err(
        "an isolated spawn ran against a hermetic home holding a planted \
         credential file, so the refusal never ran",
    );
    assert!(
        refused.to_string().contains(".netrc"),
        "the refusal does not name the planted credential file: {refused}"
    );
}
