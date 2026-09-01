//! Live-nvim proof of [`EngineHandle::checktime`]'s contract, the
//! falsifiable check `docs/checktime-wire-capture.md` was captured to pin.
//!
//! The ordinary rows first: an out-of-band write to a path with no loaded
//! buffer raises nothing; an unmodified buffer facing a genuine external
//! change reloads silently; a modified buffer facing one raises the
//! conflict signal (`found: true, fired: true`); and a self-write (nvim's
//! own, indistinguishable on the wire from `AiFsWrite`'s own mechanism) is
//! a no-op even with local edits layered on top of it afterward.
//!
//! Then `force: true`, the explicit reload behind the user's own "discard
//! local edits" answer: it takes the external content, a re-read that
//! raises is reported as a reload that did not finish rather than as a
//! completed discard, and a path that stops being a readable file -- gone,
//! dangling, a directory, a pipe, a socket, a device, or swapped mid-reload
//! -- is never "reloaded" over the user's own edits, while an ordinary
//! symlink to a file still is.
//!
//! And last the watcher's own unforced probe over those same unreadable
//! shapes, which reaches them with no user in the loop at all: it answers
//! them rather than reading them, and one of them costs the rest of its
//! batch nothing.
//!
//! [`EngineHandle::checktime`]: view_engine::nvim_api::EngineHandle::checktime
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
// the whole file drives a live engine through unix-only facts: a named
// pipe, a mode bit, and a descriptor the relay dups
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::time::Instant;

use view_core::msg::{CheckTimeOutcome, Msg};
use view_engine::process::{Engine, EngineConfig};
use view_test_support::{settle_mtime, ScratchDir};

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
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .expect("attach ui");
    engine
}

/// A scratch directory that removes itself when the test that made it ends.
///
/// Cleanup is the type's job rather than each test's because the fixtures
/// here are not all ordinary files: a named pipe or a socket left under
/// `target/` blocks anything that later opens files for reading there --
/// forever, and one more per run. A test that forgot the call would put one
/// back.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn join(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A listener bound at a short path under the system temp dir, with the
/// asked-for path symlinked to it. `sun_path` caps a bindable socket path
/// near 104 bytes and the scratch root's nonce alone puts a path under it
/// at that edge on CI runners, so the bind never happens at the scratch
/// path itself. The probe stats through the link, so the path reads as a
/// socket either way -- the same reach the device case gets from its
/// `/dev/null` symlink. The socket file outlives the listener, because the
/// path must keep reading as a socket after the binder returns, so it is
/// the returned directory guard rather than the listener that decides how
/// long it lives -- nothing else here would ever remove it, and one socket
/// per case per run is what fills a temp root over a week of test runs.
#[cfg(unix)]
fn socket_at(path: &std::path::Path) -> (std::os::unix::net::UnixListener, ScratchDir) {
    static NONCE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = ScratchDir::new(&format!("ct-sock-{n}")).expect("a directory for the socket");
    let short = dir.join("s.sock");
    let listener =
        std::os::unix::net::UnixListener::bind(&short).expect("bind a socket at a short path");
    std::os::unix::fs::symlink(&short, path).expect("point the path at the socket");
    (listener, dir)
}

fn scratch_root(nonce_suffix: &str) -> Scratch {
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
    Scratch(std::fs::canonicalize(root).expect("canonicalize test root"))
}

/// The cleanup itself, which nothing else here would notice the loss of:
/// every other test passes just as well with a pipe left behind, and the
/// cost lands on whatever opens files under `target/` next -- once per run,
/// forever.
#[test]
#[cfg(unix)]
fn a_scratch_root_takes_its_blocking_fixtures_with_it() {
    let pipe;
    {
        let root = scratch_root("scratch-drop");
        pipe = root.join("case_scratch_pipe.txt");
        let made = std::process::Command::new("mkfifo")
            .arg(&pipe)
            .status()
            .expect("run mkfifo");
        assert!(made.success(), "mkfifo refused to create the pipe");
        assert!(pipe.exists(), "the fixture must exist to be worth removing");
    }
    assert!(
        !pipe.exists(),
        "a named pipe left under target/ blocks every later reader of it"
    );
}

fn next_hidden_buffer_loaded(rx: &mpsc::Receiver<Msg>) -> u64 {
    let deadline = Instant::now() + common::rpc_deadline();
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Msg::HiddenBufferLoaded { buf, .. }) => {
                return buf.expect("the path resolves to a handle").0;
            }
            Ok(_other) => continue,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("no Msg::HiddenBufferLoaded arrived within 5s")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the engine channel closed before a HiddenBufferLoaded arrived")
            }
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

/// The deadline the pipe and socket rows lean on: a chunk that blocked
/// nvim's main loop answers nothing, and this is what turns that into a
/// five-second failure rather than a wedged suite. Timeout and disconnect
/// are told apart deliberately -- a blocked engine and a dead one are
/// different bugs, and one diagnostic covering both sends the next reader
/// looking in the wrong place.
fn next_checktime_reply(rx: &mpsc::Receiver<Msg>) -> CheckTimeReply {
    let deadline = Instant::now() + common::rpc_deadline();
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
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
            Err(mpsc::RecvTimeoutError::Timeout) => panic!(
                "no Msg::CheckTimeReply arrived within 5s -- nvim's main loop \
                 is blocked inside the chunk"
            ),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the engine channel closed before a CheckTimeReply arrived")
            }
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
                    "vim.api.nvim_create_autocmd('BufReadPost', { buffer = \
                     ..., callback = function() error('reload refused') end })",
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
/// between the user and a file recreated empty, with nothing said.
/// `docs/checktime-wire-capture.md` cases 7e and 10.
///
/// An agent removing a file the user has unsaved edits in arrives at the
/// *probe* row below: removals are nominated like writes, so the answer is
/// the notice rather than a prompt whose only offer is a reload that cannot
/// happen. The forced row is reached the one way left -- the path stops
/// being a file between a prompt opening and the user answering it -- and
/// is driven because that window is real.
#[test]
fn a_forced_reload_of_a_deleted_file_reloads_nothing_and_keeps_the_buffer() {
    let row = forced_reload_after("force-deleted", |path| {
        std::fs::remove_file(path).expect("remove the file out of band");
    });

    assert_eq!(
        row.probe,
        CheckTimeOutcome::FileGone { modified: true },
        "the probe answers the removal itself rather than opening a prompt \
         whose only offer is a reload that cannot happen"
    );
    assert_eq!(
        row.outcome,
        CheckTimeOutcome::FileGone { modified: true },
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
        !row.path_existed,
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

/// A path that now holds a directory -- the shape `:edit!` is *glad* to
/// open, handing the buffer a netrw listing in place of the user's unsaved
/// edits and calling it a completed discard. A file-to-directory swap
/// raises no `FileChangedShell` of its own, so nothing about it announces
/// itself; the stat is the only thing that notices.
#[test]
fn a_forced_reload_of_a_path_that_became_a_directory_reloads_nothing() {
    let row = forced_reload_after("force-dir", |path| {
        std::fs::remove_file(path).expect("remove the file out of band");
        std::fs::create_dir(path).expect("put a directory in its place");
    });

    assert_eq!(
        row.probe,
        CheckTimeOutcome::FileGone { modified: true },
        "nvim raises no `FileChangedShell` for a directory in a file's place, \
         so before the stat governed the probe too this row said nothing at all"
    );
    assert_eq!(row.outcome, CheckTimeOutcome::FileGone { modified: true });
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
#[cfg(unix)]
fn a_forced_reload_of_a_dangling_symlink_reloads_nothing() {
    let row = forced_reload_after("force-dangling", |path| {
        std::fs::remove_file(path).expect("remove the file out of band");
        std::os::unix::fs::symlink(path.with_extension("nowhere"), path)
            .expect("point the path at nothing");
    });

    assert_eq!(row.probe, CheckTimeOutcome::FileGone { modified: true });
    assert_eq!(row.outcome, CheckTimeOutcome::FileGone { modified: true });
    assert_eq!(row.lines, vec!["local-edit".to_string()]);
}

/// The row the guard exists for. `:edit!` on a named pipe blocks reading it
/// and never returns -- inside `nvim_exec_lua`, on nvim's single-threaded
/// main loop, taking the RPC connection with it (capture doc case 7e, driven
/// there under a bounded harness). `rm f && mkfifo f` is one shell command,
/// so nothing exotic is needed to reach it.
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
        CheckTimeOutcome::FileGone { modified: true },
        "`rm f && mkfifo f` is one shell command, and neither half of the \
         round trip it starts may read the pipe"
    );
    assert_eq!(row.outcome, CheckTimeOutcome::FileGone { modified: true });
    assert_eq!(
        row.lines,
        vec!["local-edit".to_string()],
        "a pipe must never be read into the buffer, let alone block on"
    );
}

/// A path that now holds a unix socket, and one that now holds a character
/// device. Neither is readable as a file, and both are creatable by an
/// ordinary user -- a socket by binding one, a device by pointing a symlink
/// at `/dev/null` -- so neither needs root to reach. The predicate rejects
/// them for the same reason it rejects a pipe, and driving them is what
/// closes the enumeration of what `fs_stat` can answer rather than leaving
/// three of its types argued for and untested.
#[test]
#[cfg(unix)]
fn a_forced_reload_of_a_path_that_became_a_socket_or_a_device_reloads_nothing() {
    let row = forced_reload_after("force-socket", |path| {
        std::fs::remove_file(path).expect("remove the file out of band");
        let (_socket, _socket_dir) = socket_at(path);
    });
    assert_eq!(row.probe, CheckTimeOutcome::FileGone { modified: true });
    assert_eq!(row.outcome, CheckTimeOutcome::FileGone { modified: true });
    assert_eq!(row.lines, vec!["local-edit".to_string()]);

    let row = forced_reload_after("force-chardev", |path| {
        std::fs::remove_file(path).expect("remove the file out of band");
        std::os::unix::fs::symlink("/dev/null", path).expect("point the path at a device");
    });
    assert_eq!(row.probe, CheckTimeOutcome::FileGone { modified: true });
    assert_eq!(row.outcome, CheckTimeOutcome::FileGone { modified: true });
    assert_eq!(row.lines, vec!["local-edit".to_string()]);
}

/// The reachable half of the pipe hazard, and the reason the stat sits above
/// the force split rather than inside it. On an UNMODIFIED buffer
/// `:checktime` performs the re-read itself, without consulting
/// `FileChangedShell` at all, so it blocks on the pipe exactly as `:edit!`
/// would -- and it gets there with no user in the loop: the watcher issues
/// this call by itself for any write under a trusted root, and an unmodified
/// buffer is the state most open files are in.
///
/// A modified buffer is what hid this: its handler sets `v:fcs_choice = ''`,
/// which short-circuits nvim's read before anything is opened, which is why
/// every forced row above answers rather than hanging
/// (`docs/checktime-wire-capture.md` case 10a).
///
/// Bounded the same way the forced pipe row is: a regression fails on
/// `next_checktime_reply`'s 5s deadline, and `Engine`'s `Drop` `SIGKILL`s
/// the wedged child.
#[test]
#[cfg(unix)]
fn an_unforced_probe_of_a_path_that_became_a_pipe_answers_without_reading_it() {
    let root = scratch_root("probe-fifo");
    let path = root.join("case_probe_fifo.txt");
    std::fs::write(&path, "original\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let buf = resolve(&engine, &rx, &name);

    settle_mtime();
    std::fs::remove_file(&path).expect("remove the file out of band");
    let made = std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo refused to create the pipe");

    engine
        .handle
        .checktime(13, std::slice::from_ref(&name), false)
        .expect("issue the probe");
    let probe = next_checktime_reply(&rx);
    assert_eq!(probe.request_id, 13);
    assert_eq!(
        probe.only(),
        CheckTimeOutcome::FileGone { modified: false },
        "the watcher's own probe must answer a pipe rather than read it"
    );
    assert_eq!(
        lines_of(&engine, buf),
        vec!["original".to_string()],
        "nothing was read, so the buffer holds what it last read"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// One unreadable path must not cost its batch. `:checktime` against a
/// socket or a device raises `E321` rather than blocking, and an unprotected
/// `vim.cmd` takes the whole Lua chunk down with it -- which the caller
/// degrades to `NoBuffer` for every path in the call, so a genuine conflict
/// on a sibling is swallowed and the user never sees the prompt. The chunk
/// is deliberately batched (`docs/checktime-wire-capture.md` case 8), so
/// "one path in the call" is the ordinary shape, not a corner.
///
/// The raise also skipped the augroup cleanup on the line after it, leaving
/// a one-shot `FileChangedShell` autocmd armed to set `fcs_choice =
/// 'reload'` on some later, unrelated change. The quiet path last in the
/// batch is what makes that visible: its autocmd never fires, so it is
/// still armed when the call ends, and only the cleanup takes it down --
/// a path whose autocmd fired has already deleted itself and would report
/// a clean group either way (case 10b).
#[test]
#[cfg(unix)]
fn an_unforced_probe_answers_every_path_in_a_batch_around_an_unreadable_one() {
    let root = scratch_root("probe-batch");
    let conflict = root.join("case_batch_conflict.txt");
    let socket = root.join("case_batch_socket.txt");
    let device = root.join("case_batch_device.txt");
    let quiet = root.join("case_batch_quiet.txt");
    for path in [&conflict, &socket, &device, &quiet] {
        std::fs::write(path, "original\n").expect("write fixture");
    }
    let names: Vec<String> = [&conflict, &socket, &device, &quiet]
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let conflict_buf = resolve(&engine, &rx, &names[0]);
    for name in &names[1..] {
        let _ = resolve(&engine, &rx, name);
    }
    set_lines(&engine, conflict_buf, &["local-edit"]);

    settle_mtime();
    std::fs::write(&conflict, "changed-externally\n").expect("external write");
    std::fs::remove_file(&socket).expect("remove the socket's fixture");
    let (_listener, _socket_dir) = socket_at(&socket);
    std::fs::remove_file(&device).expect("remove the device's fixture");
    std::os::unix::fs::symlink("/dev/null", &device).expect("point the path at a device");

    engine
        .handle
        .checktime(14, &names, false)
        .expect("issue the batched probe");
    let probe = next_checktime_reply(&rx);
    assert_eq!(probe.request_id, 14);
    assert_eq!(
        probe.results.len(),
        4,
        "the reply is positional -- a short array is the whole batch lost"
    );
    assert_eq!(
        probe.results[0].1,
        CheckTimeOutcome::Conflict,
        "a genuine conflict must survive an unreadable path sharing its call"
    );
    assert_eq!(
        probe.results[1].1,
        CheckTimeOutcome::FileGone { modified: false }
    );
    assert_eq!(
        probe.results[2].1,
        CheckTimeOutcome::FileGone { modified: false }
    );
    assert_eq!(
        probe.results[3].1,
        CheckTimeOutcome::HandledSilently,
        "a path nothing touched must still be answered on its own terms"
    );
    assert_eq!(
        probe_augroup_count(&engine),
        0,
        "the chunk's one-shot autocmd must never outlive the call that made it"
    );

    for name in &names {
        engine.handle.release_hidden(name).expect("release");
    }
}

/// How many autocmds the chunk's own scoped augroup still holds. Zero unless
/// the cleanup was skipped: the group is created and deleted inside a single
/// call, so anything left in it fires on a change nobody asked about.
///
/// Asked only by the two unix-gated batch probes around it.
#[cfg(unix)]
fn probe_augroup_count(engine: &Engine) -> i64 {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    "local ok, found = pcall(vim.api.nvim_get_autocmds, \
                     { group = 'view_checktime_probe' })\n\
                     if not ok then return 0 end\n\
                     return #found",
                ),
                rmpv::Value::Array(Vec::new()),
            ],
        )
        .expect("count the probe augroup")
        .as_i64()
        .expect("the count is an integer")
}

/// The one unreadable shape `st.type == 'file'` cannot reject, and the only
/// thing the probe's own `pcall` is left to catch: a regular file the
/// editor may not *open*. `:checktime` on an unmodified buffer performs the
/// re-read itself, the open is refused, and nvim raises `E321` out of the
/// command -- past a stat that had nothing wrong to report
/// (`docs/checktime-wire-capture.md` case 10d).
///
/// Unprotected, that raise ends the whole Lua chunk, and both losses of the
/// batch test above come back from a path the guard cannot see: every entry
/// degrades to `NoBuffer`, and the one-shot `FileChangedShell` autocmd
/// stays armed to set `fcs_choice = 'reload'` on some later, unrelated
/// change. The untouched path last in the call is what makes the leak
/// observable, for the reason that test's own doc gives.
///
/// Mode bits are advisory to a privileged process, so the child is dropped
/// to an unprivileged uid whenever the suite itself runs as root, and the
/// refusal is read back through the child before the probe is issued: a
/// fixture the editor could open would leave this passing while proving
/// nothing.
#[test]
#[cfg(unix)]
fn a_probe_of_a_file_the_editor_may_not_open_keeps_the_rest_of_its_batch() {
    let root = scratch_root("probe-unreadable");
    let unreadable = root.join("case_unreadable.txt");
    let quiet = root.join("case_unreadable_quiet.txt");
    for path in [&unreadable, &quiet] {
        std::fs::write(path, "original\n").expect("write fixture");
    }
    let names: Vec<String> = [&unreadable, &quiet]
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    let mut engine = spawn_unprivileged(&root);
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let buf = resolve(&engine, &rx, &names[0]);
    let _ = resolve(&engine, &rx, &names[1]);

    settle_mtime();
    std::fs::write(&unreadable, "changed-externally\n").expect("external write");
    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))
        .expect("deny reads on the fixture");

    assert_eq!(
        stat_type_seen_by(&engine, &names[0]),
        "file",
        "the whole point of this row is that the guard has nothing to reject"
    );
    assert!(
        !can_be_opened_by(&engine, &names[0]),
        "the editor must actually be refused, or nothing here is exercised"
    );

    engine
        .handle
        .checktime(15, &names, false)
        .expect("issue the batched probe");
    let probe = next_checktime_reply(&rx);
    assert_eq!(probe.request_id, 15);
    assert_eq!(
        probe.results.len(),
        2,
        "an unprotected raise answers no array at all -- the whole call lost"
    );
    assert_eq!(
        probe.results[0].1,
        CheckTimeOutcome::HandledSilently,
        "a refused re-read read nothing and changed nothing the user must be told about"
    );
    assert_eq!(
        probe.results[1].1,
        CheckTimeOutcome::HandledSilently,
        "a path nothing touched must still be answered on its own terms"
    );
    assert_eq!(
        probe_augroup_count(&engine),
        0,
        "the chunk's one-shot autocmd must never outlive the call that made it"
    );
    assert!(
        !lines_of(&engine, buf).contains(&"changed-externally".to_string()),
        "nothing was readable, so nothing may have been read into the buffer"
    );

    std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644))
        .expect("restore the fixture's mode");
    for name in &names {
        engine.handle.release_hidden(name).expect("release");
    }
}

/// [`spawn`], with the child dropped to the overflow uid when the suite runs
/// as root. `nvim_bin` is a shell wrapper rather than an argument list
/// because [`EngineConfig::extra_args`] lands *after* `--embed`, which is
/// too late to name a program to run.
#[cfg(unix)]
fn spawn_unprivileged(root: &Scratch) -> Engine {
    let mut config = EngineConfig::isolated();
    if running_as_root() {
        let wrapper = root.join("nvim-unprivileged");
        std::fs::write(
            &wrapper,
            "#!/bin/sh\nexec setpriv --reuid=65534 --regid=65534 --clear-groups nvim \"$@\"\n",
        )
        .expect("write the privilege-dropping wrapper");
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
            .expect("make the wrapper executable");
        config = config.with_nvim_bin(&wrapper);
    }
    let engine = Engine::spawn(config).expect("spawn engine");
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .expect("attach ui");
    engine
}

#[cfg(unix)]
fn running_as_root() -> bool {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .is_some_and(|uid| uid.trim() == "0")
}

/// What `vim.uv.fs_stat` answers for `path` inside the child -- the same
/// call the chunk's own guard makes, asked from outside it.
#[cfg(unix)]
fn stat_type_seen_by(engine: &Engine, path: &str) -> String {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    "local p = ...\nlocal st = vim.uv.fs_stat(p)\nreturn st and st.type or 'none'",
                ),
                rmpv::Value::Array(vec![rmpv::Value::from(path)]),
            ],
        )
        .expect("stat the fixture through the child")
        .as_str()
        .expect("the stat type is a string")
        .to_owned()
}

/// Whether the child can open `path` for reading at all -- the question the
/// stat cannot answer, and the one this row turns on.
#[cfg(unix)]
fn can_be_opened_by(engine: &Engine, path: &str) -> bool {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    // the mode argument is `fs_open`'s create mode, which a
                    // read-only open never reaches -- 0 rather than a file
                    // mode that would read as the permissions this asks about
                    concat!(
                        "local p = ...\n",
                        "local fd = vim.uv.fs_open(p, 'r', 0)\n",
                        "if fd == nil then return false end\n",
                        "vim.uv.fs_close(fd)\n",
                        "return true",
                    ),
                ),
                rmpv::Value::Array(vec![rmpv::Value::from(path)]),
            ],
        )
        .expect("try the fixture open through the child")
        .as_bool()
        .expect("the open result is a bool")
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
    /// Read before the scratch directory removes itself, so the "a reload
    /// must not recreate what it could not read" assertion still has
    /// something to fail on.
    path_existed: bool,
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
    let path_existed = path.symlink_metadata().is_ok();
    engine.handle.release_hidden(&name).expect("release");
    ForcedReload {
        probe,
        outcome,
        lines,
        modified,
        path_existed,
    }
}
