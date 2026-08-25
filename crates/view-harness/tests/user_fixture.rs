//! The generated `user` fixture, driven through the row that measures it.
//!
//! The unit tests beside the generator pin what it writes; this one is the
//! only place that answers the question those cannot -- whether a real
//! terminal session over that config actually reaches the opened file.
//! Nothing else would catch a login whose plugin set leaves a message or a
//! prompt sitting where the content marker has to appear: the row's own
//! desync refusal would, but only during a measurement run nobody takes
//! until the config is already recorded.
//!
//! Not a measurement, and no number here is kept: two cold spawns per
//! side, no baseline read or written, so it needs no quiet host. Ignored
//! by default for the same reason the heavy-fixture leg is -- it needs the
//! shared plugin cache populated and the `view` binary built, neither of
//! which the fast `task ci` legs provide.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsString;
use std::path::PathBuf;

use view_bench::scenarios::{first_paint, Protocol};
use view_bench::session::{NvimSpec, SpawnSpec, ViewSpec};
use view_harness::fixture::{
    cache_root, copy_dir_recursive, generate_user_fixture, generate_user_fixture_with_stall,
    lockfile_cache_key, scratch_root, workspace_root, USER_FIXTURE, USER_FIXTURE_STALL_MS,
    USER_FIXTURE_STALL_RECEIPT,
};

/// The stall the proof plants: the fixture's own constant, so this leg and
/// the bar the gate test reasons about cannot drift apart.
const STALL_MS: u64 = USER_FIXTURE_STALL_MS;

/// Content only the opened file can supply, planted on enough lines that
/// no corner overlay could hide every one of them. The row's own marker is
/// private to the bench binary; this file plants its own rather than
/// reaching for it, since the assertion is "the buffer painted", not "that
/// exact string did".
const MARKER: &str = "VIEWUSERFIXTUREMARKER";
const MARKER_LINES: usize = 60;

/// One side's hermetic homes: a private copy of the generated fixture, and
/// a data home pointed at the shared plugin cache so the login loads the
/// plugin set the fixture was generated from.
fn side(root: &std::path::Path, tag: &str, fixture: &std::path::Path) -> SpawnSpec {
    let dir = root.join(tag);
    let config_home = dir.join("xdg_config_home");
    copy_dir_recursive(fixture, &config_home).unwrap();
    let lock = std::fs::read(fixture.join("nvim").join("lazy-lock.json")).unwrap();
    let data_home = cache_root().join(lockfile_cache_key(&lock));
    let scratch = dir.join("scratch.txt");
    let body = std::iter::repeat_n(MARKER, MARKER_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(&scratch, format!("{body}\n")).unwrap();
    let env = [
        ("XDG_CONFIG_HOME", config_home.as_os_str()),
        ("XDG_DATA_HOME", data_home.as_os_str()),
        ("XDG_STATE_HOME", dir.join("xdg_state_home").as_os_str()),
        ("XDG_CACHE_HOME", dir.join("xdg_cache_home").as_os_str()),
        ("TERM", "xterm-256color".as_ref()),
        ("COLORTERM", "truecolor".as_ref()),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), v.to_os_string()))
    .collect();
    SpawnSpec {
        program: PathBuf::new(),
        args: vec![scratch.into_os_string()],
        env,
        cwd: Some(dir),
    }
}

#[test]
#[ignore = "generated user fixture: run via `task user-fixture`, which has the compat plugin cache and a built view"]
fn the_first_paint_row_reaches_the_opened_file_through_the_generated_login() {
    let fixture = generate_user_fixture().expect(
        "the shared plugin cache must be populated (task compat) before the generated login \
         can be built from it",
    );
    let view_bin = workspace_root().join("target").join("debug").join("view");
    assert!(
        view_bin.exists(),
        "{} is missing; build it first (cargo build -p view)",
        view_bin.display()
    );

    let root = scratch_root("user-fixture-test").join(format!("{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let mut view_spec = side(&root, "view", &fixture);
    let nvim_spec = side(&root, "nvim", &fixture);
    // --nvim-bin must precede the positional: view forwards every token
    // after the first positional to nvim verbatim
    view_spec
        .args
        .splice(0..0, [OsString::from("--nvim-bin"), OsString::from("nvim")]);
    let mut nvim_spec = nvim_spec;
    nvim_spec.program = PathBuf::from("nvim");
    view_spec.program = view_bin;

    // two spawns per side with no warmup: enough for the row to produce
    // every statistic it reports, few enough that this stays a functional
    // check rather than a measurement
    let protocol = Protocol {
        samples: 2,
        warmup: 0,
        ..Protocol::default()
    };
    let outcome = first_paint::run(
        ViewSpec(&view_spec),
        NvimSpec(&nvim_spec),
        &protocol,
        view_surface::SHELL_PLACEHOLDER,
        MARKER,
    )
    .unwrap_or_else(|err| {
        panic!("the {USER_FIXTURE} fixture never reached the opened file: {err}")
    });

    assert!(
        outcome.gated_shell_visible_cold_ms > 0.0,
        "the startup shell must reach the terminal before the login finishes"
    );
    assert!(
        outcome.gated_marker_cold_ms > outcome.gated_shell_visible_cold_ms,
        "the file's content must arrive after the shell that stands in for it: shell {} ms, \
         marker {} ms",
        outcome.gated_shell_visible_cold_ms,
        outcome.gated_marker_cold_ms
    );
    println!(
        "{USER_FIXTURE}: shell visible {:.1} ms, marker {:.1} ms over {} cold spawns per side",
        outcome.gated_shell_visible_cold_ms, outcome.gated_marker_cold_ms, protocol.samples
    );
    std::fs::remove_dir_all(&root).ok();
}

/// The stall the gate proof runs on has to actually run. A spec entry the
/// plugin manager declined, a plugin directory outside the runtimepath, a
/// `plugin/` file never sourced: each leaves a run that looks exactly like
/// an ordinary one, and the proof would then read a slowdown that was
/// never applied.
///
/// Serialized with the row test above by `--test-threads=1`, since the
/// knob it sets is process-wide and the generated fixture is one tree.
#[test]
#[ignore = "generated user fixture: run via `task user-fixture`, which has the compat plugin cache and a built view"]
fn the_stall_knob_reaches_a_real_session() {
    let fixture = generate_user_fixture_with_stall(STALL_MS)
        .expect("the shared plugin cache must be populated");

    let root = scratch_root("user-fixture-stall").join(format!("{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok();
    let spec = side(&root, "nvim", &fixture);
    // headless rather than through the row: what this asserts is that the
    // stall loaded, which the receipt answers exactly, and a pty adds only
    // the boundary timing the row already covers
    let status = std::process::Command::new("nvim")
        .arg("--headless")
        .args(&spec.args)
        .arg("-c")
        .arg("qa!")
        .envs(spec.env.iter().cloned())
        .current_dir(spec.cwd.clone().unwrap())
        .status()
        .expect("spawning the pinned nvim over the stalled login");
    assert!(status.success(), "the stalled login exited {status}");

    // stdpath("state") is the `nvim` subdirectory of the state home, which
    // is where the stall plugin writes and where a reader must look
    let receipt = root
        .join("nvim")
        .join("xdg_state_home")
        .join("nvim")
        .join(USER_FIXTURE_STALL_RECEIPT);
    assert_eq!(
        std::fs::read_to_string(&receipt).unwrap_or_default().trim(),
        STALL_MS.to_string(),
        "the stall plugin left no receipt at {}, so the spec entry never loaded",
        receipt.display()
    );

    // an unstalled generation must drop the stall rather than leave the
    // previous one's in the tree for every later run to pay
    let plain = generate_user_fixture().unwrap();
    assert!(!plain.join("nvim").join("slow-init").exists());
    std::fs::remove_dir_all(&root).ok();
}
