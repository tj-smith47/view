//! The bare-nvim arm of a paired cell starts under the harness's own pty,
//! and that pty is the only terminal it has: every capability question it
//! asks is answered here or not at all.
//!
//! One of those questions is not optional. The pinned engine's tty startup
//! writes an OSC 11 background query with a DSR immediately behind it and
//! then blocks in `vim.wait(100, ...)` until the DSR answer lands
//! (`runtime/lua/vim/_core/defaults.lua`), so a pty that answers neither
//! adds that whole wait to the nvim side of every cold cell -- more than
//! nvim's whole startup again on this class -- and every ratio taken
//! across it reads as a win view never earned.
//!
//! What that makes falsifiable is below, in two halves. The first needs no
//! tool this tree does not already require: the same spawn under
//! [`QueryPolicy::Silent`] must be slower by most of the engine's own wait,
//! so a responder that stops answering the query fails here by name rather
//! than by a bench number nobody reads until a record run. The second is
//! the bar itself -- the harness's figure against the same engine's under a
//! real terminal on the same host -- and it needs `tmux`, so it reports its
//! own skip when the host has none.
//!
//! Both legs read `--startuptime`, which is the engine's own account of its
//! startup rather than anything this harness times, and both read it
//! through the row's own parser so a section the row would not read is not
//! read here either.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use portable_pty::CommandBuilder;
use view_bench::scenarios::startup::started_times_ms;
use view_bench::session::{BenchSession, SpawnSpec, GRID_COLS, GRID_ROWS};
use view_oracle::{PtySession, QueryPolicy};

/// How long the pinned engine blocks its own tty startup waiting for the
/// DSR answer behind its background query (`vim.wait(100, ...)` in
/// `runtime/lua/vim/_core/defaults.lua`).
const ENGINE_BACKGROUND_WAIT: Duration = Duration::from_millis(100);

/// Ceiling on the harness figure as a multiple of the same engine's figure
/// under a real terminal. A multiple rather than a duration: a loaded host
/// stretches both legs together, and what this leg is about is the gap
/// between them.
const REAL_TERMINAL_MULTIPLE: f64 = 2.0;

/// How many times each leg below is drawn, the better draw standing for
/// it: one spawn the host stretched is not a verdict about a pty, and both
/// figures here are floors a stretch can only push the wrong way.
const DRAWS_PER_LEG: usize = 2;

/// The better of `DRAWS_PER_LEG` draws of `leg`, which for a startup figure
/// is the smallest.
fn best_of(mut leg: impl FnMut(usize) -> f64) -> f64 {
    (0..DRAWS_PER_LEG).map(&mut leg).fold(f64::MAX, f64::min)
}

/// Long enough for a cold `nvim --clean` to reach `NVIM STARTED` and exit
/// on a host that is doing something else at the time; scaled, because
/// what it bounds is how fast the host is.
fn spawn_budget() -> Duration {
    view_test_support::host_deadline(Duration::from_secs(30))
}

/// `nvim --clean`, timed into `log`, exiting as soon as it has started.
///
/// `--clean` rather than a fixture config: what the engine waits on is in
/// its own runtime, ahead of any config, and a plugin set only adds the
/// host's own variance to both sides of every comparison here.
fn args_timed_into(log: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--clean"),
        OsString::from("--startuptime"),
        log.as_os_str().to_os_string(),
        OsString::from("-c"),
        OsString::from("qa"),
    ]
}

/// The editor process's own `NVIM STARTED` figure out of `log`.
fn started_ms(log: &Path) -> f64 {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let times = started_times_ms(&text).expect("the log must attribute every startup figure");
    assert_eq!(
        times.len(),
        1,
        "expected exactly one editor startup in {}, got {times:?}; log:\n{text}",
        log.display()
    );
    times[0]
}

/// The figure a bare-nvim arm writes through the funnel every bench cell
/// spawns its sides with.
fn started_through_the_bench_pty(log: &Path) -> f64 {
    let spec = SpawnSpec {
        program: PathBuf::from("nvim"),
        args: args_timed_into(log),
        env: Vec::new(),
        cwd: None,
        measured_program: None,
    };
    let mut session = BenchSession::spawn(&spec).expect("bench pty spawn");
    // the child has already asked for its own exit, so this is the reap
    // that makes the timing file complete rather than a quit request
    session.shutdown();
    started_ms(log)
}

/// The same figure from a pty that answers nothing, which is the shape the
/// harness had while the query went unanswered.
fn started_through_a_silent_pty(log: &Path) -> f64 {
    let mut cmd = CommandBuilder::new("nvim");
    for arg in args_timed_into(log) {
        cmd.arg(arg);
    }
    let mut session =
        PtySession::spawn_configured_with(cmd, GRID_COLS, GRID_ROWS, QueryPolicy::Silent)
            .expect("silent pty spawn");
    assert!(
        session.wait_for_exit(spawn_budget()).is_some(),
        "the unanswered editor never exited; screen:\n{}",
        session.screen()
    );
    started_ms(log)
}

/// The same figure under a real terminal on this host, or `None` when the
/// host has no `tmux` to provide one.
fn started_under_tmux(log: &Path) -> Option<f64> {
    if Command::new("tmux").arg("-V").output().is_err() {
        // announced rather than passed over in silence: cargo captures a
        // passing test's stderr, and a leg that never ran reads as a
        // verified claim on the checks page
        let reason = "no tmux on this host, so there is no real terminal to hold the harness's \
                      own figure against (see this file's own module docs)";
        println!(
            "skipping the_nvim_arm_starts_as_fast_under_the_bench_pty_as_under_a_real_terminal: \
             {reason}"
        );
        if std::env::var("GITHUB_ACTIONS").is_ok_and(|value| value == "true") {
            println!("::warning::nvim arm real-terminal leg skipped: {reason}");
        }
        return None;
    }
    // a socket nothing else on the host shares, so the kill below reaches
    // this server and no other session's
    let socket = format!("view-nvim-arm-{}", std::process::id());
    let pane = format!("nvim --clean --startuptime {} -c qa", log.display());
    let started = Command::new("tmux")
        .args([
            "-L",
            &socket,
            "new",
            "-d",
            "-x",
            &GRID_COLS.to_string(),
            "-y",
            &GRID_ROWS.to_string(),
            &pane,
        ])
        .status()
        .expect("tmux new-session");
    assert!(started.success(), "tmux refused the session: {started:?}");

    // the timing file is the pane's own completion signal, and reading it
    // rather than the server's lifetime keeps the wait bounded whatever
    // the pane's child does
    let deadline = Instant::now() + spawn_budget();
    let mut figure = None;
    while Instant::now() < deadline {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        // a report still being written is a section without its figure
        // yet, which the parser refuses; that is a not-done-yet here
        if let Some(time) = started_times_ms(&text)
            .ok()
            .and_then(|t| t.first().copied())
        {
            figure = Some(time);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    // output() rather than status(): the server exits with its last
    // session, so the kill routinely reports one that is already gone
    let _ = Command::new("tmux")
        .args(["-L", &socket, "kill-server"])
        .output();
    Some(figure.expect("nvim under tmux never reported its own startup"))
}

/// The whole point of answering: an unanswered pty costs the engine its own
/// wait, and the harness's pty must not be paying it.
#[test]
fn the_bench_pty_spares_the_nvim_arm_the_wait_a_silent_one_costs_it() {
    let dir = view_test_support::ScratchDir::new("nvim-arm-startup").unwrap();
    let answered_took =
        best_of(|draw| started_through_the_bench_pty(&dir.join(format!("answered-{draw}.log"))));
    let unanswered_took =
        best_of(|draw| started_through_a_silent_pty(&dir.join(format!("silent-{draw}.log"))));
    eprintln!("NVIM STARTED: bench pty {answered_took:.1} ms, silent pty {unanswered_took:.1} ms");
    assert!(
        unanswered_took - answered_took >= ENGINE_BACKGROUND_WAIT.as_secs_f64() * 1_000.0 / 2.0,
        "an unanswered pty cost the engine {unanswered_took:.1} ms against \
         the bench pty's {answered_took:.1} ms, which is less than half the \
         {ENGINE_BACKGROUND_WAIT:?} wait it is supposed to be spending -- \
         either the query is going unanswered on both, or the engine no \
         longer waits for it and this leg proves nothing"
    );
}

/// The bar the nvim arm's number is only honest under: what the harness
/// measures is what a user at a terminal would see.
#[test]
fn the_nvim_arm_starts_as_fast_under_the_bench_pty_as_under_a_real_terminal() {
    let dir = view_test_support::ScratchDir::new("nvim-arm-real-terminal").unwrap();
    let Some(real) = started_under_tmux(&dir.join("tmux-0.log")) else {
        return;
    };
    let real = (1..DRAWS_PER_LEG).fold(real, |best, draw| {
        started_under_tmux(&dir.join(format!("tmux-{draw}.log")))
            .map_or(best, |next| best.min(next))
    });
    let harness =
        best_of(|draw| started_through_the_bench_pty(&dir.join(format!("harness-{draw}.log"))));
    eprintln!("NVIM STARTED: bench pty {harness:.1} ms, tmux {real:.1} ms");
    assert!(
        harness <= real * REAL_TERMINAL_MULTIPLE,
        "the engine reached NVIM STARTED in {harness:.1} ms under the bench \
         pty against {real:.1} ms under tmux on this host, past the \
         {REAL_TERMINAL_MULTIPLE}x bar: the arm every paired cold cell \
         compares view against is not the editor a user runs"
    );
}
