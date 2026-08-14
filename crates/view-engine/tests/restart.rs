//! `Engine::restart` and the swap file a crash leaves behind, against a real
//! nvim: a child killed out of band is replaced by a live one, an unsaved
//! edit comes back out of nvim's own swap file without anybody answering the
//! prompt that normally guards it, and neither teardown leaves a process
//! behind.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use std::path::{Path, PathBuf};
use std::time::Duration;
use view_engine::process::{Engine, EngineConfig};

mod common;

/// A config with no user configuration, no plugins, no shada and no swap
/// file, matching every other live test here. Built per call rather than
/// cloned: an `EngineConfig` is consumed by the spawn it describes, and a
/// restart is a second spawn.
fn cfg() -> EngineConfig {
    EngineConfig::isolated().with_shutdown_timeout(Duration::from_millis(500))
}

/// A scratch directory under the build tree -- never the system temp dir,
/// which is world-writable -- holding both the file a recovery test edits
/// and the state directory nvim writes that file's swap into.
fn scratch(name: &str) -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop();
    root.pop();
    let dir = root.join("target").join("view-engine-restart").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Where every session in this file writes its swap files, named by the test
/// rather than read back out of nvim's own state directory.
///
/// nvim resolves that state directory per platform: on unix
/// `$XDG_STATE_HOME/nvim/swap`, on Windows `$XDG_STATE_HOME/nvim-data/swap`
/// (measured on the pinned engine, which honours the variable on both and
/// differs only in the leaf it appends). A test that hard-codes one of those
/// shapes finds an empty directory on the other platform and fails on its own
/// precondition rather than on the behavior it means to measure.
fn swap_dir(dir: &Path) -> PathBuf {
    dir.join("swap")
}

/// The startup command pinning `'directory'` to [`swap_dir`].
///
/// A `--cmd`, because nvim runs those before it opens any file and so before
/// the first swap file is written; a `-c` would arrive after the buffer that
/// already made one. Written as Lua with a long-bracket string so a path
/// carrying Windows separators, spaces or commas needs no `:set` escaping.
/// The trailing `//` is nvim's own request for swap names built from the
/// file's whole path, so two same-named files never collide in one directory.
fn pin_swap_dir(dir: &Path) -> String {
    format!("lua vim.o.directory = [[{}//]]", swap_dir(dir).display())
}

/// A config with swap files enabled and every one of them written under
/// `dir`, so a recovery reads the swap this test made rather than anything
/// the host happens to hold.
fn session(dir: &Path) -> EngineConfig {
    std::fs::create_dir_all(swap_dir(dir)).unwrap();
    EngineConfig::default()
        .with_arg("--clean")
        .with_arg("--cmd")
        .with_arg(pin_swap_dir(dir))
        .with_env("XDG_STATE_HOME", dir.join("state"))
        .with_shutdown_timeout(Duration::from_millis(500))
}

/// [`session`], plus the `file` it opens as an argument.
fn editing(dir: &Path, file: &Path) -> EngineConfig {
    session(dir).with_arg(file)
}

/// The swap files nvim has written under `dir` so far.
fn swap_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(swap_dir(dir))
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default()
}

/// Replaces the current buffer's text with `text` and flushes nvim's swap
/// file, without ever writing the file to disk.
///
/// nvim flushes the swap on its own schedule ('updatetime'/'updatecount'),
/// which is exactly the pacing that bounds what a real crash recovers;
/// `:preserve` is nvim's own way to ask for that flush now, so a test built
/// on this measures the recovery rather than the flush timer.
fn write_unsaved_edit(engine: &Engine, text: &str) {
    engine
        .handle
        .request_timeout(
            "nvim_buf_set_lines",
            vec![
                0.into(),
                0.into(),
                (-1).into(),
                false.into(),
                rmpv::Value::Array(vec![text.into()]),
            ],
            Duration::from_secs(5),
        )
        .expect("a live engine answers nvim_buf_set_lines");
    engine
        .handle
        .request_timeout(
            "nvim_command",
            vec!["preserve".into()],
            Duration::from_secs(5),
        )
        .expect("a live engine answers :preserve");
}

/// The current buffer's first line, as nvim reports it.
fn first_line(engine: &Engine) -> String {
    let lines = engine
        .handle
        .request_timeout(
            "nvim_buf_get_lines",
            vec![0.into(), 0.into(), (-1).into(), false.into()],
            Duration::from_secs(5),
        )
        .expect("a live engine answers nvim_buf_get_lines");
    lines
        .as_array()
        .and_then(|lines| lines.first())
        .and_then(|line| line.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("nvim_buf_get_lines answered with {lines:?}"))
}

/// How many swap prompts this engine has answered on its user's behalf, per
/// the counter the injected `SwapExists` autocommand keeps. Read through
/// `get()`, so an engine that never met a swap file answers 0 rather than
/// failing on a variable nothing ever set.
fn swap_events(engine: &Engine) -> u64 {
    engine
        .handle
        .request_timeout(
            "nvim_eval",
            vec![rmpv::Value::from("get(g:, 'view_swap_recovered', 0)")],
            Duration::from_secs(5),
        )
        .expect("a live engine answers nvim_eval")
        .as_u64()
        .expect("the swap-prompt counter is a number")
}

/// Whether the engine's session carries view's own `SwapExists` guard.
///
/// What separates "met no swap prompt" from "had nothing installed to meet
/// one with": a count of zero answered prompts means the first only when the
/// guard that would have answered them is live.
///
/// Asked of view's group by name rather than of the event. nvim ships a
/// `SwapExists` autocommand of its own (`nvim.swapfile`), so a bare
/// `exists('#SwapExists')` answers 1 on a session that carries no view guard
/// at all and proves nothing about the counter beside it.
fn swap_guard_live(engine: &Engine) -> bool {
    engine
        .handle
        .request_timeout(
            "nvim_eval",
            vec![rmpv::Value::from(
                "exists('#view_swap_recovery#SwapExists')",
            )],
            Duration::from_secs(5),
        )
        .expect("a live engine answers nvim_eval")
        .as_u64()
        == Some(1)
}

/// Leaves behind exactly what a crash leaves: `file` holding `text` in a
/// flushed swap file, on disk holding whatever it held before, and the
/// process that made the edit killed out of band so nothing cleaned up after
/// it.
fn crash_with_unsaved_edit(dir: &Path, file: &Path, text: &str) {
    let engine = Engine::spawn(editing(dir, file)).unwrap();
    // --embed holds startup until a UI attaches, so the file is not even
    // loaded (and has no swap file) before this
    engine.handle.ui_attach(80, 24).unwrap();
    write_unsaved_edit(&engine, text);
    crash(engine);
}

/// Kills `engine`'s child out of band and reaps it, so the swap file it
/// leaves belongs to a process the next session will find gone.
///
/// The drop is what reaps: a killed child nothing waited on keeps its
/// process-table entry as a zombie, and nvim reads an owner that still has
/// one as a session that may still be editing the file.
fn crash(engine: Engine) {
    let pid = engine.pid();
    kill_out_of_band(pid);
    drop(engine);
    wait_until_gone(pid);
}

/// Kills `pid` without going through `Engine`, so the teardown a restart
/// performs meets a child that is already gone -- the shape a real crash
/// leaves behind, which the ordinary `Drop` path never produces.
///
/// Per platform for the same reason `common::pid_in_process_table` is: the
/// crash this simulates has to be a real one on the host actually running
/// the test, and neither platform's tool exists on the other.
#[cfg(unix)]
fn kill_out_of_band(pid: u32) {
    let killed = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .expect("kill must run for an out-of-band crash to be simulable");
    assert!(killed.success(), "kill -KILL {pid} failed: {killed:?}");
}

#[cfg(windows)]
fn kill_out_of_band(pid: u32) {
    let killed = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .status()
        .expect("taskkill must run for an out-of-band crash to be simulable");
    assert!(
        killed.success(),
        "taskkill /F /PID {pid} failed: {killed:?}"
    );
}

/// Waits until `pid` has left the process table, or fails: a `kill` that has
/// been sent is not yet a process that has died, and an assertion made
/// against a still-running child would prove nothing about the crash path.
fn wait_until_gone(pid: u32) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !common::pid_in_process_table(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !common::pid_in_process_table(pid),
        "pid {pid} still in the process table 5s after SIGKILL"
    );
}

/// Whether the engine is at a prompt nobody can answer. `nvim_get_mode` is
/// answered on receipt even then (that is the whole reason the liveness
/// probe rides it), so a live reply is not on its own proof of a usable
/// editor -- this is.
fn blocking(engine: &Engine) -> bool {
    let mode = engine
        .handle
        .request_timeout("nvim_get_mode", vec![], Duration::from_secs(5))
        .expect("a live engine answers nvim_get_mode");
    mode.as_map()
        .and_then(|entries| {
            entries
                .iter()
                .find(|(key, _)| key.as_str() == Some("blocking"))
        })
        .and_then(|(_, value)| value.as_bool())
        .expect("nvim_get_mode always reports blocking")
}

#[test]
fn a_restart_after_a_crash_yields_a_live_engine() {
    let engine = Engine::spawn(cfg()).unwrap();
    let dead_pid = engine.pid();
    kill_out_of_band(dead_pid);

    let engine = engine
        .restart(cfg())
        .expect("a crashed engine must restart");

    assert_ne!(
        engine.pid(),
        dead_pid,
        "restart reused the dead child's pid"
    );
    let mode = engine.handle.request("nvim_get_mode", vec![]).unwrap();
    assert!(
        mode.as_map().is_some(),
        "the restarted engine answered nvim_get_mode with {mode:?}"
    );
}

/// The whole point of the flag, end to end: an edit that was never written
/// to disk comes back from nvim's own swap file, and nothing else does. This
/// is the recovery guarantee the restart claims, stated as a test -- no
/// view-side copy of the text exists to produce it from.
#[test]
fn a_restart_recovers_the_unsaved_edit_its_predecessor_left_in_swap() {
    let dir = scratch("recovers-swap");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "what is on disk\n").unwrap();

    let engine = Engine::spawn(editing(&dir, &file)).unwrap();
    // --embed holds startup until a UI attaches, so the file is not even
    // loaded (and has no swap file) before this
    engine.handle.ui_attach(80, 24).unwrap();
    write_unsaved_edit(&engine, "never written to disk");
    kill_out_of_band(engine.pid());

    let engine = engine
        .restart(editing(&dir, &file))
        .expect("a crashed engine must restart");
    engine.handle.ui_attach(80, 24).unwrap();

    assert_eq!(
        first_line(&engine),
        "never written to disk",
        "the restart did not recover the swap file's contents"
    );
    assert!(
        !blocking(&engine),
        "the recovered engine is parked at a prompt nobody can answer"
    );

    assert!(
        spawned_with(&engine, "-r"),
        "the recovering child carries no -r: {:?}",
        engine.command_line()
    );
    assert_os_agrees(&engine);
}

/// The prompt an embedded engine cannot ask: a session started over a
/// crashed one's swap file is not handed `-r` (only a restart is), so it
/// meets nvim's own `[O]pen/[E]dit/[R]ecover/[D]elete/[Q]uit` question the
/// way any other editor would -- except that this one has no terminal of its
/// own to ask through, and would park there until something answered.
#[test]
fn a_session_started_over_a_crashed_ones_swap_recovers_without_asking() {
    let dir = scratch("recovers-without-asking");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "what is on disk\n").unwrap();
    crash_with_unsaved_edit(&dir, &file, "never written to disk");

    let engine = Engine::spawn(editing(&dir, &file)).unwrap();
    assert!(
        !spawned_with(&engine, "-r"),
        "this is a plain spawn, not a restart: {:?}",
        engine.command_line()
    );
    engine.handle.ui_attach(80, 24).unwrap();

    // read first, and through an ordinary request, because that is the
    // barrier the two assertions below need: `nvim_get_mode` is answered on
    // receipt even while nvim is still starting up, so a verdict taken
    // before an ordinary request has come back is a verdict about a session
    // that had not reached the file yet
    let recovered = first_line(&engine);
    assert_eq!(
        recovered, "never written to disk",
        "the session came up on the file as it is on disk, discarding what \
         the swap held"
    );
    assert!(
        !blocking(&engine),
        "the engine is parked at a swap prompt nobody can answer"
    );
    assert_eq!(
        swap_events(&engine),
        1,
        "the swap file this test left was not met as a SwapExists event"
    );
}

/// The bound on whose swap may be recovered: an owner that is still editing.
/// Recovering there would hand a second session the first one's unsaved work
/// while the first goes on editing it, and the two would part with divergent
/// copies of one file and nothing on screen to say either happened. nvim
/// decides that case itself (`W325`, edit the file as it is on disk), and a
/// guard that answered anyway would overrule it.
#[test]
fn a_session_started_over_a_live_engines_swap_leaves_the_owner_its_work() {
    let dir = scratch("live-owner-swap");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "what is on disk\n").unwrap();

    let owner = Engine::spawn(editing(&dir, &file)).unwrap();
    // --embed holds startup until a UI attaches, so the file is not even
    // loaded (and has no swap file) before this
    owner.handle.ui_attach(80, 24).unwrap();
    write_unsaved_edit(&owner, "the owner is still editing this");
    assert!(
        !swap_files(&dir).is_empty(),
        "the owner flushed no swap file, so the second session below would \
         meet no prompt at all"
    );

    let second = Engine::spawn(editing(&dir, &file)).unwrap();
    second.handle.ui_attach(80, 24).unwrap();

    assert_eq!(
        first_line(&second),
        "what is on disk",
        "the second session recovered a swap its owner is still editing"
    );
    assert!(
        swap_guard_live(&second),
        "the session carries no SwapExists guard, so a count of zero \
         answered prompts says nothing"
    );
    assert_eq!(
        swap_events(&second),
        0,
        "view answered a prompt nvim had already decided for a live owner"
    );
    assert!(
        !blocking(&second),
        "the second engine is parked at a swap prompt nobody can answer"
    );
    assert_eq!(
        first_line(&owner),
        "the owner is still editing this",
        "the owner lost the edit it never wrote"
    );
}

/// The migrated vimrc nvim passes through verbatim, and the one line in it
/// that reaches this guard: a bare `autocmd!` claims the autocommands from
/// there on, and would delete an ungrouped guard along with everything else.
/// The group is what survives it, so a recovery that would have hung the
/// session still happens under a config that asked for no autocommands at
/// all.
#[test]
fn a_vimrc_clearing_every_autocommand_leaves_the_swap_guard_standing() {
    let dir = scratch("vimrc-clears-autocmds");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "what is on disk\n").unwrap();
    let vimrc = dir.join("init.vim");
    // the second line is what proves `-u` was honoured at all: a vimrc nvim
    // never sourced would leave the guard standing for the wrong reason
    std::fs::write(&vimrc, "autocmd!\nlet g:vimrc_ran = 1\n").unwrap();
    crash_with_unsaved_edit(&dir, &file, "never written to disk");

    let engine = Engine::spawn(
        session(&dir)
            .with_arg("-u")
            .with_arg(&vimrc)
            .with_arg(&file),
    )
    .unwrap();
    engine.handle.ui_attach(80, 24).unwrap();

    let sourced = engine
        .handle
        .request_timeout(
            "nvim_eval",
            vec![rmpv::Value::from("get(g:, 'vimrc_ran', 0)")],
            Duration::from_secs(5),
        )
        .expect("a live engine answers nvim_eval")
        .as_u64();
    assert_eq!(
        sourced,
        Some(1),
        "nvim never sourced the vimrc, so this test cleared no autocommands"
    );
    assert!(
        swap_guard_live(&engine),
        "the vimrc's autocmd! deleted the swap guard"
    );
    assert_eq!(
        first_line(&engine),
        "never written to disk",
        "the session came up on the file as it is on disk, discarding what \
         the swap held"
    );
    assert_eq!(
        swap_events(&engine),
        1,
        "the swap file this test left was not met as a SwapExists event"
    );
    assert!(
        !blocking(&engine),
        "the engine is parked at a swap prompt nobody can answer"
    );
}

/// The window a startup flag cannot reach: the swap file is met by an open
/// the user asks for long after the session came up, where `-r` (an argument
/// nvim reads once) has nothing left to say. The autocommand covers it
/// because it lives for the session, not for its startup.
#[test]
fn a_file_opened_mid_session_over_a_swap_recovers_without_parking_the_editor() {
    let dir = scratch("mid-session-swap");
    let file = dir.join("notes.txt");
    std::fs::write(&file, "what is on disk\n").unwrap();
    crash_with_unsaved_edit(&dir, &file, "typed after the last write");

    // no file argument at all, so this session meets nothing at startup
    let engine = Engine::spawn(session(&dir)).unwrap();
    engine.handle.ui_attach(80, 24).unwrap();
    engine
        .handle
        .request_timeout(
            "nvim_command",
            vec![rmpv::Value::from(format!("edit {}", file.display()))],
            Duration::from_secs(5),
        )
        .expect("an open over a swap file must return, not park inside :edit");

    assert!(
        !blocking(&engine),
        "the engine is parked at a swap prompt nobody can answer"
    );
    assert_eq!(
        swap_events(&engine),
        1,
        "the swap file this test left was not met as a SwapExists event"
    );
    assert_eq!(
        first_line(&engine),
        "typed after the last write",
        "the open discarded what the swap held"
    );
}

/// The disconfirm, and the bound on what the autocommand is allowed to do: a
/// crash with nothing unsaved still leaves a swap file behind (nvim writes
/// one the moment a buffer loads, not when it is first modified), but it
/// holds nothing the file on disk does not. nvim answers that case itself --
/// no `SwapExists` event is raised at all -- so a session that meets one
/// must show no sign of a recovery it never performed.
#[test]
fn a_crash_with_nothing_unsaved_raises_no_swap_event_at_all() {
    let dir = scratch("clean-crash");
    let file = dir.join("doc.txt");
    std::fs::write(&file, "what is on disk\n").unwrap();

    let engine = Engine::spawn(editing(&dir, &file)).unwrap();
    engine.handle.ui_attach(80, 24).unwrap();
    // proves the buffer really loaded, so the swap file asserted below is
    // this session's rather than the trace of a startup that never got there
    assert_eq!(first_line(&engine), "what is on disk");
    crash(engine);
    assert!(
        !swap_files(&dir).is_empty(),
        "the crash left no swap file, so this test cannot tell a swap that \
         raises no event from one that was never there"
    );

    let engine = Engine::spawn(editing(&dir, &file)).unwrap();
    engine.handle.ui_attach(80, 24).unwrap();

    assert!(
        swap_guard_live(&engine),
        "the session carries no SwapExists guard, so a count of zero \
         answered prompts says nothing"
    );
    assert_eq!(
        swap_events(&engine),
        0,
        "a swap file holding nothing unsaved was answered as a recovery"
    );
    assert_eq!(
        first_line(&engine),
        "what is on disk",
        "the session came up holding something other than the file on disk"
    );
    assert!(
        !blocking(&engine),
        "the engine is parked at a swap prompt nobody can answer"
    );
}

/// The falsifier for the flag's condition, and the reason it has one: `-r`
/// with no file to act on lists every swap file it can find, parks the
/// engine at the prompt that acknowledges the list, and exits. A restart
/// with nothing to recover must therefore carry no flag at all, and must
/// still be an editor once a UI attaches.
#[test]
fn a_restart_with_no_file_to_recover_carries_no_flag_and_stays_usable() {
    let engine = Engine::spawn(cfg()).unwrap();
    kill_out_of_band(engine.pid());
    let engine = engine
        .restart(cfg())
        .expect("a crashed engine must restart");

    assert!(
        !spawned_with(&engine, "-r"),
        "a restart with no file to recover must not ask nvim to list swap \
         files: {:?}",
        engine.command_line()
    );
    assert_os_agrees(&engine);

    engine.handle.ui_attach(80, 24).unwrap();
    assert!(
        !blocking(&engine),
        "the restarted engine is parked at a prompt nobody can answer"
    );
    let sum = engine
        .handle
        .request_timeout(
            "nvim_eval",
            vec![rmpv::Value::from("6 * 7")],
            Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(
        sum.as_u64(),
        Some(42),
        "the restarted engine cannot evaluate"
    );
}

/// Two crashes in a row, the second arriving immediately after the recovery
/// from the first: every child this test starts is reaped, so no zombie
/// survives either teardown.
#[test]
fn a_second_crash_restarts_again_and_leaves_no_zombie() {
    let engine = Engine::spawn(cfg()).unwrap();
    let first = engine.pid();
    kill_out_of_band(first);
    let engine = engine.restart(cfg()).expect("the first restart must work");

    let second = engine.pid();
    kill_out_of_band(second);
    let engine = engine.restart(cfg()).expect("the second restart must work");

    let third = engine.pid();
    assert!(
        first != second && second != third && first != third,
        "three spawns must be three processes: {first}, {second}, {third}"
    );
    engine.handle.request("nvim_get_mode", vec![]).unwrap();
    drop(engine);

    for pid in [first, second, third] {
        wait_until_gone(pid);
    }
}

/// A restart whose replacement cannot start at all reports the failure
/// rather than handing back an engine that is not there. The old child is
/// already gone by then, which is why the caller owes the user a report
/// instead of a silent retry.
#[test]
fn a_restart_that_cannot_come_back_up_reports_it() {
    let engine = Engine::spawn(cfg()).unwrap();
    let dead_pid = engine.pid();
    kill_out_of_band(dead_pid);

    let failed = engine
        .restart(cfg().with_nvim_bin("view-no-such-nvim-binary"))
        .err();

    assert!(
        matches!(failed, Some(view_engine::EngineError::Io(_))),
        "a restart that cannot spawn must report it, got {failed:?}"
    );
    wait_until_gone(dead_pid);
}

/// A wedge is a fact about the engine that was there, never about the one
/// that replaced it. Carrying the old watch across the restart would carry
/// its unanswered probes and their accumulated silence with it, and the
/// replacement -- healthy, answering, seconds old -- would read as wedged on
/// the very first observation, re-raising the banner and the modal it just
/// recovered from.
#[test]
fn a_restart_never_inherits_the_dead_engines_wedge() {
    let (tx, _rx) = std::sync::mpsc::sync_channel(64);
    let mut engine = Engine::spawn(cfg()).unwrap();
    let _ = engine.start_pump(tx.clone());
    // a probe that is genuinely outstanding: the engine's reply reaches the
    // pump's sink, and nothing in this test folds it back through
    // `record_ack`, so the watch keeps waiting for it exactly as a loop
    // stalled past the threshold would
    engine.heartbeat.prober().tick(&engine.handle).unwrap();
    let wedged_at = std::time::Instant::now() + view_engine::HEARTBEAT_WEDGE_THRESHOLD;
    assert_eq!(
        engine.heartbeat.observe_at(false, wedged_at),
        view_engine::Liveness::Wedged,
        "the test never reached the wedge it means to restart out of"
    );

    kill_out_of_band(engine.pid());
    let mut engine = engine.restart(cfg()).expect("a wedged engine must restart");
    let _ = engine.start_pump(tx);

    assert_eq!(
        engine.heartbeat.observe_at(false, wedged_at),
        view_engine::Liveness::Alive,
        "the restarted engine inherited its predecessor's wedge"
    );
    // an armed watch always names a deadline, so the reading that separates
    // a fresh cadence from an inherited wedge is which one: nothing
    // outstanding arms the whole prospective window (one probe interval
    // before the next probe is even owed, plus the threshold it would then
    // have to go unanswered for), while a carried-over wedge would arm the
    // threshold's own look-again and nothing longer
    let armed = engine
        .heartbeat
        .poll_deadline()
        .expect("an armed watch owes the loop a wakeup");
    assert!(
        armed > view_engine::HEARTBEAT_WEDGE_THRESHOLD,
        "the restarted engine owes an answer to a probe it was never sent: {armed:?}"
    );
}

/// Whether the engine's child was handed `flag`, according to the command
/// line the spawn itself recorded.
///
/// The authority on every platform: `Engine::command_line` is read off the
/// `Command` that was spawned, so an assertion made against it is an
/// assertion about what the child actually received rather than about a
/// rule re-derived from the config a second time.
fn spawned_with(engine: &Engine, flag: &str) -> bool {
    engine.command_line().iter().any(|arg| arg == flag)
}

/// Cross-checks the recorded command line against the one the OS reports for
/// the running child, so a recording that stopped matching reality is caught
/// rather than believed. Linux only: `/proc` is the one place a spawned
/// process's argv is readable without cooperation from the process.
#[cfg(target_os = "linux")]
fn assert_os_agrees(engine: &Engine) {
    let pid = engine.pid();
    let raw = std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_else(|e| panic!("/proc/{pid}/cmdline unreadable: {e}"));
    assert!(
        !raw.is_empty(),
        "pid {pid} has an empty command line: it is a zombie, not a running engine"
    );
    let os: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    let recorded: Vec<String> = engine
        .command_line()
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        os, recorded,
        "the OS reports a command line the spawn did not record"
    );
}

/// The cross-check is a Linux-only extra, never the assertion itself: every
/// test above asserts through [`spawned_with`], which answers on every
/// platform.
#[cfg(not(target_os = "linux"))]
fn assert_os_agrees(_engine: &Engine) {}
