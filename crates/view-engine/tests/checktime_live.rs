//! Live-nvim proof of [`EngineHandle::checktime`]'s three-way contract, the
//! falsifiable check `docs/checktime-wire-capture.md` was captured to pin:
//! an out-of-band write to a path with no loaded buffer raises nothing: an
//! unmodified buffer facing a genuine external change reloads silently; a
//! modified buffer facing one raises the conflict signal (`found: true,
//! fired: true`); and a self-write (nvim's own, indistinguishable on the
//! wire from `AiFsWrite`'s own mechanism) is a no-op even with local edits
//! layered on top of it afterward. Three final cases prove `force: true`
//! drives the explicit reload behind the user's own "discard local edits"
//! answer, that a re-read which raises is reported as a reload that did not
//! finish rather than as a completed discard, and that a file no longer on
//! disk is never "reloaded" over the user's own edits.
//!
//! [`EngineHandle::checktime`]: view_engine::nvim_api::EngineHandle::checktime
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use view_core::msg::{CheckTimeOutcome, Msg};
use view_engine::process::{Engine, EngineConfig};

/// Spawns an isolated engine with a UI attached -- load-bearing the same way
/// `ai_fs_live.rs`'s own `spawn` documents, and doubly so here:
/// `docs/checktime-wire-capture.md` case 0 is the proof that an unhandled
/// `:checktime` against a modified buffer BLOCKS the whole connection only
/// when a UI is attached, which is exactly the shape `CHECKTIME_CHUNK`
/// exists to make safe. A `--headless`-shaped test here would never have
/// caught a regression that dropped the chunk's own `FileChangedShell`
/// guard.
fn spawn() -> Engine {
    let engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    engine.handle.ui_attach(80, 24).expect("attach ui");
    engine
}

fn scratch_root(nonce_suffix: &str) -> std::path::PathBuf {
    let nonce = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
        nonce_suffix
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tmp")
        .join(format!("checktime-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    std::fs::canonicalize(root).expect("canonicalize test root")
}

fn next_hidden_buffer_loaded(rx: &mpsc::Receiver<Msg>) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "no Msg::HiddenBufferLoaded arrived within 5s"
        );
        match rx.recv_timeout(remaining) {
            Ok(Msg::HiddenBufferLoaded { buf, .. }) => {
                return buf.expect("the path resolves to a handle").0;
            }
            Ok(_other) => continue,
            Err(err) => panic!("channel closed before a HiddenBufferLoaded arrived: {err}"),
        }
    }
}

struct CheckTimeReply {
    request_id: u64,
    results: Vec<(std::path::PathBuf, CheckTimeOutcome)>,
}

impl CheckTimeReply {
    /// The single entry a one-path call answers with.
    fn only(&self) -> CheckTimeOutcome {
        assert_eq!(self.results.len(), 1, "expected a one-path reply");
        self.results[0].1
    }
}

fn next_checktime_reply(rx: &mpsc::Receiver<Msg>) -> CheckTimeReply {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "no Msg::CheckTimeReply arrived within 5s"
        );
        match rx.recv_timeout(remaining) {
            Ok(Msg::CheckTimeReply {
                request_id,
                results,
            }) => {
                return CheckTimeReply {
                    request_id,
                    results,
                };
            }
            Ok(_other) => continue,
            Err(err) => panic!("channel closed before a CheckTimeReply arrived: {err}"),
        }
    }
}

fn lines_of(engine: &Engine, buf: u64) -> Vec<String> {
    engine
        .handle
        .request(
            "nvim_buf_get_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(0),
                rmpv::Value::from(-1),
                rmpv::Value::from(false),
            ],
        )
        .expect("read buffer lines")
        .as_array()
        .expect("lines reply is an array")
        .iter()
        .map(|v| v.as_str().expect("line is a string").to_owned())
        .collect()
}

fn is_modified(engine: &Engine, buf: u64) -> bool {
    engine
        .handle
        .request(
            "nvim_buf_get_option",
            vec![rmpv::Value::from(buf), rmpv::Value::from("modified")],
        )
        .expect("read modified flag")
        .as_bool()
        .expect("modified is a bool")
}

/// Writes buffer `buf` to disk without switching the editor's own current
/// buffer to it -- a bare `nvim_command("write")` operates on whichever
/// buffer nvim currently has focused, never the hidden one
/// [`resolve`](resolve) loaded, so this is the only way to exercise "nvim's
/// own write" against a buffer this test never displays.
fn write_buf(engine: &Engine, buf: u64) {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    "local buf = ...\nvim.api.nvim_buf_call(buf, function() vim.cmd('write') end)",
                ),
                rmpv::Value::Array(vec![rmpv::Value::from(buf)]),
            ],
        )
        .expect("write the hidden buffer");
}

fn set_lines(engine: &Engine, buf: u64, lines: &[&str]) {
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(0),
                rmpv::Value::from(-1),
                rmpv::Value::from(false),
                rmpv::Value::Array(lines.iter().map(|l| rmpv::Value::from(*l)).collect()),
            ],
        )
        .expect("set buffer lines");
}

/// Coarse filesystem mtime resolution can leave two writes inside the same
/// clock tick indistinguishable to nvim's own check -- the same reason
/// `docs/checktime-wire-capture.md`'s own capture method sleeps between an
/// initial write and the "external" one in every case that needs two
/// distinct disk mtimes.
fn settle_mtime() {
    std::thread::sleep(Duration::from_millis(1100));
}

fn resolve(engine: &Engine, rx: &mpsc::Receiver<Msg>, path: &str) -> u64 {
    engine.handle.load_hidden(path, 1).expect("issue the load");
    next_hidden_buffer_loaded(rx)
}

/// Case 1: no loaded buffer for the changed path raises nothing -- neither
/// a silent reload nor a conflict, since there is nothing to reload or
/// conflict with.
#[test]
fn no_loaded_buffer_answers_found_false() {
    let root = scratch_root("no-buffer");
    let path = root.join("untouched.txt");
    std::fs::write(&path, "content\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .checktime(1, std::slice::from_ref(&name), false)
        .expect("issue the probe");
    let reply = next_checktime_reply(&rx);
    assert_eq!(reply.request_id, 1);
    assert_eq!(
        reply.only(),
        CheckTimeOutcome::NoBuffer,
        "no buffer names this path"
    );
}

/// Case 2: a loaded, UNMODIFIED buffer facing a genuine external change is
/// reloaded SILENTLY -- `found: true, fired: false`, and the buffer's own
/// text visibly takes on the new disk content, proving the "silent reload"
/// leg needs no extra logic beyond calling `:checktime`.
#[test]
fn unmodified_buffer_reloads_silently_on_external_change() {
    let root = scratch_root("silent-reload");
    let path = root.join("case2.txt");
    std::fs::write(&path, "original\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let buf = resolve(&engine, &rx, &name);
    assert!(!is_modified(&engine, buf));

    settle_mtime();
    std::fs::write(&path, "changed-externally\n").expect("external write");

    engine
        .handle
        .checktime(2, std::slice::from_ref(&name), false)
        .expect("issue the probe");
    let reply = next_checktime_reply(&rx);
    assert_eq!(
        reply.only(),
        CheckTimeOutcome::HandledSilently,
        "an unmodified buffer's own reload must not raise FileChangedShell"
    );
    assert_eq!(
        lines_of(&engine, buf),
        vec!["changed-externally".to_string()]
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// Case 3: a loaded, MODIFIED buffer facing a genuine external change
/// raises the conflict signal (`found: true, fired: true`) and leaves the
/// buffer's local edit completely untouched -- the case
/// `Msg::CheckTimeReply`'s own doc names as the one that must raise the
/// conflict prompt.
#[test]
fn modified_buffer_raises_conflict_and_keeps_the_local_edit() {
    let root = scratch_root("conflict");
    let path = root.join("case3.txt");
    std::fs::write(&path, "original\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let buf = resolve(&engine, &rx, &name);
    set_lines(&engine, buf, &["local-edit"]);
    assert!(is_modified(&engine, buf));

    settle_mtime();
    std::fs::write(&path, "changed-externally\n").expect("external write");

    engine
        .handle
        .checktime(3, std::slice::from_ref(&name), false)
        .expect("issue the probe");
    let reply = next_checktime_reply(&rx);
    assert_eq!(
        reply.only(),
        CheckTimeOutcome::Conflict,
        "a genuine external change against a modified buffer must fire"
    );
    assert_eq!(
        lines_of(&engine, buf),
        vec!["local-edit".to_string()],
        "the conflict case must never touch the buffer's own local edit"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// Case 4/5: nvim's own write (the same mechanism `AiFsWrite` and a bare
/// user `:w` both use) followed immediately by `:checktime` is a no-op --
/// `fired: false` -- and stays a no-op even with an unrelated local edit
/// layered on top afterward. This is the crux disconfirm for self-write
/// suppression: nothing in this crate filters the watcher's own detection
/// of nvim's own write, and this test is the proof that no such filter is
/// needed, because nvim's own mtime bookkeeping already answers `fired:
/// false` for both shapes.
#[test]
fn a_self_write_then_a_local_edit_never_raises_a_false_conflict() {
    let root = scratch_root("self-write");
    let path = root.join("case5.txt");
    std::fs::write(&path, "original\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let buf = resolve(&engine, &rx, &name);

    // nvim's own write: the same disk-write effect `AiFsWrite`'s own
    // chunk and a bare `:w` both produce, issued against the hidden
    // buffer specifically since it is never the editor's current one.
    write_buf(&engine, buf);

    engine
        .handle
        .checktime(4, std::slice::from_ref(&name), false)
        .expect("issue the probe immediately after the self-write");
    let immediate = next_checktime_reply(&rx);
    assert_eq!(
        immediate.only(),
        CheckTimeOutcome::HandledSilently,
        "nvim's own write must never read back as an external change"
    );

    // now layer an unrelated local edit on top, the race the disconfirm
    // targets: an agent's routed write immediately followed by unrelated
    // typing must not read as a conflict either.
    set_lines(&engine, buf, &["original", "more-local-edits"]);
    assert!(is_modified(&engine, buf));

    engine
        .handle
        .checktime(5, std::slice::from_ref(&name), false)
        .expect("issue the probe after the local edit");
    let after_edit = next_checktime_reply(&rx);
    assert_eq!(
        after_edit.only(),
        CheckTimeOutcome::HandledSilently,
        "the local edit alone changes nothing on disk, so FileChangedShell must not fire"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// `force: true` drives the user's own "reload, discard local edits"
/// answer: issued against an already-conflicted buffer, it discards the
/// local edit and takes the external content -- proving the explicit
/// `:edit!` path actually reloads, unlike a second plain `:checktime`
/// (`docs/checktime-wire-capture.md` case 7's rejected "clear modified,
/// checktime again" idea, which this test would also catch: a regression
/// that swapped `force: true`'s handling back to a second `:checktime`
/// call would leave `lines_of` still showing the stale local edit).
#[test]
fn force_true_discards_the_local_edit_and_takes_the_external_content() {
    let root = scratch_root("force-reload");
    let path = root.join("case_force.txt");
    std::fs::write(&path, "original\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let buf = resolve(&engine, &rx, &name);
    set_lines(&engine, buf, &["local-edit"]);

    settle_mtime();
    std::fs::write(&path, "changed-externally\n").expect("external write");

    // establish the conflict shape first, the same way the user would have
    // seen the prompt before answering "reload"
    engine
        .handle
        .checktime(6, std::slice::from_ref(&name), false)
        .expect("issue the probe");
    let conflict = next_checktime_reply(&rx);
    assert_eq!(conflict.only(), CheckTimeOutcome::Conflict);
    assert_eq!(lines_of(&engine, buf), vec!["local-edit".to_string()]);

    engine
        .handle
        .checktime(7, std::slice::from_ref(&name), true)
        .expect("issue the forced reload");
    let forced = next_checktime_reply(&rx);
    assert_eq!(forced.request_id, 7);
    // NOT `Conflict`: the reply to the user's own "Reload" answer must be
    // structurally distinguishable from the fresh conflict that prompted
    // it, or folding it re-opens the same prompt over a buffer that no
    // longer has local edits -- forever, since only "Keep local" escapes.
    assert_eq!(
        forced.only(),
        CheckTimeOutcome::Reloaded,
        "a forced reload must never read back as a fresh conflict"
    );
    assert_eq!(
        lines_of(&engine, buf),
        vec!["changed-externally".to_string()],
        "force: true must discard the local edit and take the external content"
    );
    assert!(
        !is_modified(&engine, buf),
        "a freshly `:edit!`-reloaded buffer must not read as modified"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// The destructive answer that did not happen. A `BufReadPost` autocmd that
/// raises makes the chunk's own `pcall` around `:edit!` fail, which is the
/// live shape `docs/checktime-wire-capture.md` case 7a captured: `ok` comes
/// back false, the local edit is still in the buffer, and the user is owed a
/// notice rather than silence -- silence here reads as "your edits were
/// discarded", the exact opposite of what happened.
#[test]
fn a_forced_reload_that_raises_reports_failure_rather_than_a_completed_discard() {
    let root = scratch_root("force-fails");
    let path = root.join("case_force_fails.txt");
    std::fs::write(&path, "original\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let buf = resolve(&engine, &rx, &name);
    set_lines(&engine, buf, &["local-edit"]);

    settle_mtime();
    std::fs::write(&path, "changed-externally\n").expect("external write");

    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    "vim.api.nvim_create_autocmd('BufReadPost', \
                     { buffer = ..., callback = function() error('reload refused') end })",
                ),
                rmpv::Value::Array(vec![rmpv::Value::from(buf)]),
            ],
        )
        .expect("register the raising autocmd");

    engine
        .handle
        .checktime(9, std::slice::from_ref(&name), true)
        .expect("issue the forced reload");
    let forced = next_checktime_reply(&rx);
    assert_eq!(forced.request_id, 9);
    assert_eq!(
        forced.only(),
        CheckTimeOutcome::ReloadFailed,
        "a `:edit!` that raised must not read back as a completed discard"
    );
    // deliberately no assertion on the buffer's content: this autocmd
    // raises after `:edit!` has already read the file, so the external
    // content is what survives here, while a failure earlier in the re-read
    // would leave the local edit. The wire says only that the reload did
    // not finish, which is exactly what the recorded notice may claim.
    engine.handle.release_hidden(&name).expect("release");
}

/// The destructive answer against a file that is gone. `:edit!` on a missing
/// path is a *success* in nvim -- it opens a new, empty file -- so reloading
/// anyway answers `ok = true`, empties the buffer, and leaves one `:w`
/// between the user and a file recreated empty, with nothing said. The whole
/// sequence is the ordinary one this feature exists for: an agent removes a
/// file the user has unsaved edits in, the conflict prompt opens, the user
/// answers "reload". `docs/checktime-wire-capture.md` case 7e.
#[test]
fn a_forced_reload_of_a_deleted_file_reloads_nothing_and_keeps_the_buffer() {
    let row = forced_reload_after("force-deleted", |path| {
        std::fs::remove_file(path).expect("remove the file out of band");
    });

    assert_eq!(
        row.probe,
        CheckTimeOutcome::Conflict,
        "a removal must open the prompt the way an external rewrite does, \
         or the user never reaches the answer this case is about"
    );
    assert_eq!(
        row.outcome,
        CheckTimeOutcome::FileGone,
        "a reload of a file that is gone must not read back as a completed one"
    );
    assert_eq!(
        row.lines,
        vec!["local-edit".to_string()],
        "the buffer is the only copy left -- it must not be emptied"
    );
    assert!(
        row.modified,
        "the edits were never discarded, so the buffer must still read modified"
    );
    assert!(
        !row.path.exists(),
        "a forced reload must not recreate the file it could not read"
    );
}

/// The window the guard cannot close from in front, driven deterministically
/// by a `BufReadPre` autocmd that swaps a directory in after the chunk has
/// stat'ed the path and while `:edit!` is already running -- the microseconds
/// an agent's `rm -r && mkdir` would have to win by chance.
///
/// This shape is the one that gets through, and `pcall` is blind to it.
/// A path that merely *vanishes* in that window is caught by nvim itself
/// (`E200: *ReadPre autocommands made the file unreadable`), but a directory
/// is something `:edit!` will happily open: it succeeds, hands the buffer a
/// netrw listing in place of the user's unsaved edits, and clears `modified`.
/// On `pcall` alone that reads back as `ok = true` -- a completed discard,
/// reported to the user with silence.
///
/// The stat taken after the reload is the whole defence. It is deliberately
/// not `FileGone`: by now `:edit!` has run, and `FileGone`'s notice promises
/// the buffer still holds the user's edits, which here it no longer does.
#[test]
fn a_path_that_stops_being_a_file_mid_reload_is_not_reported_as_a_completed_one() {
    let root = scratch_root("force-raced");
    let path = root.join("case_force_raced.txt");
    std::fs::write(&path, "original\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let buf = resolve(&engine, &rx, &name);
    set_lines(&engine, buf, &["local-edit"]);

    settle_mtime();
    std::fs::write(&path, "changed-externally\n").expect("external write");

    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    "local b, p = ...\n\
                     vim.api.nvim_create_autocmd('BufReadPre', { buffer = b, \
                     callback = function() \
                     vim.uv.fs_unlink(p) vim.uv.fs_mkdir(p, 493) end })",
                ),
                rmpv::Value::Array(vec![
                    rmpv::Value::from(buf),
                    rmpv::Value::from(name.clone()),
                ]),
            ],
        )
        .expect("register the swapping autocmd");

    engine
        .handle
        .checktime(12, std::slice::from_ref(&name), true)
        .expect("issue the forced reload");
    let forced = next_checktime_reply(&rx);
    assert_eq!(forced.request_id, 12);
    assert_eq!(
        forced.only(),
        CheckTimeOutcome::ReloadFailed,
        "a reload whose path stopped being a file under it must not read back \
         as a completed discard, however well `:edit!` itself went"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// A path that now holds a directory. Reachable only through the window
/// between the guard's stat and the reload (a file-to-directory swap raises
/// no `FileChangedShell` of its own, so it opens no prompt), but the outcome
/// if it is reached is the same silent emptying a deleted file used to get.
#[test]
fn a_forced_reload_of_a_path_that_became_a_directory_reloads_nothing() {
    let row = forced_reload_after("force-dir", |path| {
        std::fs::remove_file(path).expect("remove the file out of band");
        std::fs::create_dir(path).expect("put a directory in its place");
    });

    assert_eq!(
        row.probe,
        CheckTimeOutcome::HandledSilently,
        "nvim raises no `FileChangedShell` for a directory in a file's place, \
         which is what keeps this row off the prompt entirely"
    );
    assert_eq!(row.outcome, CheckTimeOutcome::FileGone);
    assert_eq!(
        row.lines,
        vec!["local-edit".to_string()],
        "a directory has no content to reload, so the buffer must be left alone"
    );
}

/// A symlink whose target is gone. `fs_stat` follows symlinks, so this
/// answers exactly as a deleted file does -- which is the point: the guard
/// asks what `:edit!` would be able to read, not what the path itself is.
#[test]
fn a_forced_reload_of_a_dangling_symlink_reloads_nothing() {
    let row = forced_reload_after("force-dangling", |path| {
        std::fs::remove_file(path).expect("remove the file out of band");
        std::os::unix::fs::symlink(path.with_extension("nowhere"), path)
            .expect("point the path at nothing");
    });

    assert_eq!(row.probe, CheckTimeOutcome::Conflict);
    assert_eq!(row.outcome, CheckTimeOutcome::FileGone);
    assert_eq!(row.lines, vec!["local-edit".to_string()]);
}

/// The row the guard exists for. `:edit!` on a named pipe blocks reading it
/// and never returns -- inside `nvim_exec_lua`, on nvim's single-threaded
/// main loop, taking the RPC connection with it (capture doc case 7e, driven
/// there under a bounded harness). `rm f && mkfifo f` is one shell command
/// and raises `FileChangedShell` on the modified buffer, so the user reaches
/// this by answering an ordinary conflict prompt.
///
/// Safe to run here: the guard means `:edit!` never executes, and a
/// regression that let it through fails on `next_checktime_reply`'s own 5s
/// deadline rather than hanging -- `Engine`'s `Drop` then `SIGKILL`s the
/// wedged child, so a broken guard costs this test five seconds, not the
/// suite.
#[test]
#[cfg(unix)]
fn a_forced_reload_of_a_path_that_became_a_pipe_reloads_nothing() {
    let row = forced_reload_after("force-fifo", |path| {
        std::fs::remove_file(path).expect("remove the file out of band");
        let made = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("run mkfifo");
        assert!(made.success(), "mkfifo refused to create the pipe");
    });

    assert_eq!(
        row.probe,
        CheckTimeOutcome::Conflict,
        "`rm f && mkfifo f` opens the ordinary prompt, which is how a user \
         reaches the answer that would otherwise wedge the editor"
    );
    assert_eq!(row.outcome, CheckTimeOutcome::FileGone);
    assert_eq!(
        row.lines,
        vec!["local-edit".to_string()],
        "a pipe must never be read into the buffer, let alone block on"
    );
}

/// The other side of the guard: an ordinary symlink to a real file must
/// still reload, or "is this a file" would have cost every symlinked path in
/// a project its forced reload. `fs_stat` follows the link, so it does.
#[test]
#[cfg(unix)]
fn a_forced_reload_through_a_symlink_to_a_file_still_reloads() {
    let row = forced_reload_after("force-symlink", |path| {
        let target = path.with_extension("target");
        std::fs::write(&target, "changed-externally\n").expect("write the link target");
        std::fs::remove_file(path).expect("remove the file out of band");
        std::os::unix::fs::symlink(&target, path).expect("point the path at the target");
    });

    assert_eq!(row.probe, CheckTimeOutcome::Conflict);
    assert_eq!(
        row.outcome,
        CheckTimeOutcome::Reloaded,
        "a symlink to a real file is a file to `:edit!`, and must still reload"
    );
    assert_eq!(row.lines, vec!["changed-externally".to_string()]);
    assert!(
        !row.modified,
        "a completed reload leaves the buffer unmodified"
    );
}

/// What one row of the capture doc's case-7e table reads back as: the
/// unforced probe's answer, then the forced reload's, then what the buffer
/// and the path were left holding.
struct ForcedReload {
    probe: CheckTimeOutcome,
    outcome: CheckTimeOutcome,
    lines: Vec<String>,
    modified: bool,
    path: std::path::PathBuf,
}

/// Loads a buffer on a fresh file, layers a local edit on it, lets `mutate`
/// change what the path is out of band, then walks the sequence the user
/// walks: the unforced probe that decides whether a prompt opens at all,
/// followed by the forced reload behind their own "discard local edits"
/// answer.
///
/// The probe leg is not scaffolding for the forced one. It is the half of
/// the table that says which of these shapes a user can even reach, and
/// whether reaching it costs them a prompt they never asked for.
fn forced_reload_after(nonce: &str, mutate: impl FnOnce(&std::path::Path)) -> ForcedReload {
    let root = scratch_root(nonce);
    let path = root.join("case_forced.txt");
    std::fs::write(&path, "original\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let buf = resolve(&engine, &rx, &name);
    set_lines(&engine, buf, &["local-edit"]);

    settle_mtime();
    mutate(&path);

    engine
        .handle
        .checktime(10, std::slice::from_ref(&name), false)
        .expect("issue the probe");
    let probed = next_checktime_reply(&rx);
    assert_eq!(probed.request_id, 10);
    let probe = probed.only();

    engine
        .handle
        .checktime(11, std::slice::from_ref(&name), true)
        .expect("issue the forced reload");
    let forced = next_checktime_reply(&rx);
    assert_eq!(forced.request_id, 11);
    let outcome = forced.only();
    let lines = lines_of(&engine, buf);
    let modified = is_modified(&engine, buf);
    engine.handle.release_hidden(&name).expect("release");
    ForcedReload {
        probe,
        outcome,
        lines,
        modified,
        path,
    }
}
