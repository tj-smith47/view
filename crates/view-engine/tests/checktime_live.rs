//! Live-nvim proof of [`EngineHandle::checktime`]'s three-way contract, the
//! falsifiable check `docs/checktime-wire-capture.md` was captured to pin:
//! an out-of-band write to a path with no loaded buffer raises nothing: an
//! unmodified buffer facing a genuine external change reloads silently; a
//! modified buffer facing one raises the conflict signal (`found: true,
//! fired: true`); and a self-write (nvim's own, indistinguishable on the
//! wire from `AiFsWrite`'s own mechanism) is a no-op even with local edits
//! layered on top of it afterward. Two final cases prove `force: true`
//! drives the explicit reload behind the user's own "discard local edits"
//! answer, and that a re-read which raises is reported as a reload that did
//! not finish rather than as a completed discard.
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
