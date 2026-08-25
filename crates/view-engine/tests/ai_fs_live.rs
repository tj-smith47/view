//! Live-nvim proof of [`EngineHandle::ai_fs_read`]/[`EngineHandle::ai_fs_write`]'s
//! contract: an agent's read answers from the buffer nvim holds rather than
//! from the file on disk, a windowed read costs only the lines it names, an
//! agent's write is visible in the buffer and lands on disk without this
//! process ever writing a byte itself, repeated reads of never-opened files
//! leave nvim's buffer count where they found it, and two overlapping reads
//! of the same never-opened path both answer correctly with the buffer
//! outliving the first of them.
//!
//! [`EngineHandle::ai_fs_read`]: view_engine::handle::EngineHandle::ai_fs_read
//! [`EngineHandle::ai_fs_write`]: view_engine::handle::EngineHandle::ai_fs_write
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use view_core::msg::{BufferHandle, Msg};
use view_core::native::ai_event::FsError;
use view_engine::process::{Engine, EngineConfig};

/// Spawns an isolated engine with a UI attached, the same load-bearing
/// attach `hidden_buffer_live.rs`'s own `spawn` documents: without it nvim's
/// main loop has no idle tick between back-to-back API calls.
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
        .join(format!("ai-fs-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    std::fs::canonicalize(root).expect("canonicalize test root")
}

fn next_hidden_buffer_loaded(rx: &mpsc::Receiver<Msg>) -> (u64, Option<u64>, u64) {
    let deadline = Instant::now() + common::rpc_deadline();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "no Msg::HiddenBufferLoaded arrived within 5s"
        );
        match rx.recv_timeout(remaining) {
            Ok(Msg::HiddenBufferLoaded {
                generation,
                buf,
                changedtick,
                ..
            }) => return (generation, buf.map(|b| b.0), changedtick),
            Ok(_other) => continue,
            Err(err) => panic!("channel closed before a HiddenBufferLoaded arrived: {err}"),
        }
    }
}

fn next_read_reply(rx: &mpsc::Receiver<Msg>) -> (u64, Result<String, FsError>) {
    let deadline = Instant::now() + common::rpc_deadline();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "no Msg::AiFsReadReply arrived within 5s"
        );
        match rx.recv_timeout(remaining) {
            Ok(Msg::AiFsReadReply { request_id, result }) => return (request_id, result),
            Ok(_other) => continue,
            Err(err) => panic!("channel closed before an AiFsReadReply arrived: {err}"),
        }
    }
}

fn next_write_reply(rx: &mpsc::Receiver<Msg>) -> (u64, Result<(), FsError>) {
    let deadline = Instant::now() + common::rpc_deadline();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "no Msg::AiFsWriteReply arrived within 5s"
        );
        match rx.recv_timeout(remaining) {
            Ok(Msg::AiFsWriteReply { request_id, result }) => return (request_id, result),
            Ok(_other) => continue,
            Err(err) => panic!("channel closed before an AiFsWriteReply arrived: {err}"),
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

fn buffer_count(engine: &Engine) -> usize {
    engine
        .handle
        .request("nvim_list_bufs", vec![])
        .expect("list buffers")
        .as_array()
        .expect("nvim_list_bufs reply is an array")
        .len()
}

/// Loads `path` into a hidden buffer and answers the handle and tick the
/// resolve reported, the same two values core carries from
/// `Msg::HiddenBufferLoaded` into the read or write that follows it.
fn resolve(engine: &Engine, rx: &mpsc::Receiver<Msg>, path: &str, generation: u64) -> (u64, u64) {
    engine
        .handle
        .load_hidden(path, generation)
        .expect("issue the load");
    let (seen, buf, tick) = next_hidden_buffer_loaded(rx);
    assert_eq!(seen, generation, "the resolve carries its own generation");
    (buf.expect("the path resolves to a handle"), tick)
}

/// Buffer truth: an agent reading a file the user has unsaved edits in gets
/// the edits, not the stale bytes on disk. This is the whole reason the
/// capability is worth advertising -- an agent's own direct-disk read can
/// never see them, and acting on the stale text is how an agent silently
/// reverts the user's work.
#[test]
fn a_read_answers_from_the_buffer_not_from_the_stale_file_on_disk() {
    let root = scratch_root("buffer-truth");
    let path = root.join("truth.rs");
    std::fs::write(&path, "on disk\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let (buf, _tick) = resolve(&engine, &rx, &name, 1);
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(0),
                rmpv::Value::from(-1),
                rmpv::Value::from(false),
                rmpv::Value::Array(vec![rmpv::Value::from("edited, never saved")]),
            ],
        )
        .expect("dirty the buffer");

    engine
        .handle
        .ai_fs_read(7, BufferHandle(buf), None, None)
        .expect("issue the read");

    let (request_id, result) = next_read_reply(&rx);
    assert_eq!(request_id, 7, "the answer names the request that asked");
    assert_eq!(
        result.expect("the read answers"),
        "edited, never saved\n",
        "the answer came from disk, so every unsaved edit was invisible"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("the file is still there"),
        "on disk\n",
        "a read must not have written anything"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// A windowed read is the agent asking for those lines only. The window is
/// applied inside nvim rather than by reading the whole buffer and slicing
/// afterwards, which is what keeps a read of one function out of a 50k-line
/// file from costing 50k lines over the wire.
#[test]
fn a_windowed_read_answers_only_the_lines_the_window_names() {
    let root = scratch_root("window");
    let path = root.join("window.rs");
    std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let (buf, _tick) = resolve(&engine, &rx, &name, 1);
    engine
        .handle
        .ai_fs_read(1, BufferHandle(buf), Some(2), Some(2))
        .expect("issue the windowed read");

    let (_, result) = next_read_reply(&rx);
    assert_eq!(
        result.expect("the windowed read answers"),
        "two\nthree\n",
        "the wire's `line` is 1-based and `limit` is a line count"
    );

    engine
        .handle
        .ai_fs_read(2, BufferHandle(buf), Some(5), None)
        .expect("issue the tail read");
    let (_, tail) = next_read_reply(&rx);
    assert_eq!(
        tail.expect("the tail read answers"),
        "five\n",
        "a window reaching the last line keeps the file's own trailing \
         newline"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// A file with no trailing newline reads back without one. nvim's line
/// list is identical for a file ending `"a\n"` and one ending `"a"`, so the
/// `eol` flag beside it is the only thing that tells them apart -- and an
/// agent handed a byte the file does not hold writes it back, changing a
/// file it was only asked to read.
#[test]
fn a_file_with_no_trailing_newline_reads_back_without_one() {
    let root = scratch_root("no-eol");
    let path = root.join("noeol.rs");
    std::fs::write(&path, "no terminator").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let (buf, _tick) = resolve(&engine, &rx, &name, 1);
    engine
        .handle
        .ai_fs_read(1, BufferHandle(buf), None, None)
        .expect("issue the read");

    let (_, result) = next_read_reply(&rx);
    assert_eq!(
        result.expect("the read answers"),
        "no terminator",
        "the answer invented a trailing newline the file does not hold"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// The write half of the same terminator contract: `eol = false` must reach
/// disk with no trailing newline. `fixendofline` is what nvim consults to
/// decide whether to *add* a missing final newline, so hard-coding either
/// option to `true` would silently append a byte the agent never sent --
/// which the agent's next read returns, and which it then believes it
/// authored (capture case 7).
#[test]
fn a_write_with_no_trailing_newline_reaches_disk_without_one() {
    let root = scratch_root("write-no-eol");
    let path = root.join("terminator.txt");
    std::fs::write(&path, "before\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let (buf, tick) = resolve(&engine, &rx, &name, 1);
    engine
        .handle
        .ai_fs_write(
            1,
            BufferHandle(buf),
            &["no".to_owned(), "trailing".to_owned()],
            false,
            tick,
        )
        .expect("issue the write");

    let (_, result) = next_write_reply(&rx);
    result.expect("the write is accepted");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read the written file"),
        "no\ntrailing",
        "the write appended a terminator the agent did not send"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// ACP's "The Client MUST create the file if it doesn't exist," and the
/// directory above it: creating a file in a new directory is an ordinary
/// thing an agent does, and the directory is not a second decision to put
/// in front of the user. The buffer count still returns to baseline, so a
/// creating write leaks no more than a read does.
#[test]
fn a_write_to_a_path_with_neither_file_nor_directory_creates_both() {
    let root = scratch_root("create");
    let path = root.join("no_such_dir").join("fresh.txt");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    let baseline = buffer_count(&engine);

    assert!(!path.exists(), "the fixture must not exist yet");
    let (buf, tick) = resolve(&engine, &rx, &name, 1);
    engine
        .handle
        .ai_fs_write(
            1,
            BufferHandle(buf),
            &["created".to_owned(), "by the agent".to_owned()],
            true,
            tick,
        )
        .expect("issue the write");

    let (_, result) = next_write_reply(&rx);
    result.expect("a write to a path with no file behind it must create it");
    // a buffer with no file behind it is written in nvim's default
    // fileformat for the platform, which is dos on windows
    let eol = if cfg!(windows) { "\r\n" } else { "\n" };
    assert_eq!(
        std::fs::read_to_string(&path).expect("the file now exists"),
        format!("created{eol}by the agent{eol}")
    );

    engine.handle.release_hidden(&name).expect("release");
    assert_eq!(buffer_count(&engine), baseline);
}

/// A CRLF file survives a read-modify-write byte for byte. The agent both
/// sees and sends LF only; the carriage returns ride on `fileformat`, which
/// `bufload` detects and which the write chunk deliberately never sets
/// (capture case 12) -- a `content` string cannot express a file's line
/// terminators, so deciding them from one would rewrite every CRLF file an
/// agent touched.
#[test]
fn a_crlf_file_survives_an_agent_read_modify_write_byte_for_byte() {
    let root = scratch_root("crlf");
    let path = root.join("dos.txt");
    std::fs::write(&path, "alpha\r\nbravo\r\ncharlie\r\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let (buf, tick) = resolve(&engine, &rx, &name, 1);
    engine
        .handle
        .ai_fs_read(1, BufferHandle(buf), None, None)
        .expect("issue the read");
    let (_, read) = next_read_reply(&rx);
    assert_eq!(
        read.expect("the read answers"),
        "alpha\nbravo\ncharlie\n",
        "the agent must see LF only, never the file's own carriage returns"
    );

    engine
        .handle
        .ai_fs_write(
            2,
            BufferHandle(buf),
            &["alpha".to_owned(), "bravo".to_owned(), "CHARLIE".to_owned()],
            true,
            tick,
        )
        .expect("issue the write");
    let (_, written) = next_write_reply(&rx);
    written.expect("the write is accepted");

    assert_eq!(
        std::fs::read(&path).expect("read the written file"),
        b"alpha\r\nbravo\r\nCHARLIE\r\n",
        "the write converted the file's line terminators"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// An agent's write goes through nvim: the buffer holds the new text
/// immediately, and the file on disk holds it too -- this process never
/// opens the file, so nvim's own `endofline`/`fixendofline` decide the
/// trailing newline exactly as they would for a write the user typed.
#[test]
fn a_write_lands_in_the_buffer_and_on_disk_through_nvim() {
    let root = scratch_root("write");
    let path = root.join("written.rs");
    std::fs::write(&path, "before\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let (buf, tick) = resolve(&engine, &rx, &name, 1);
    engine
        .handle
        .ai_fs_write(
            4,
            BufferHandle(buf),
            &["after".to_owned(), "and more".to_owned()],
            true,
            tick,
        )
        .expect("issue the write");

    let (request_id, result) = next_write_reply(&rx);
    assert_eq!(request_id, 4);
    result.expect("the write is accepted");
    assert_eq!(
        lines_of(&engine, buf),
        vec!["after".to_string(), "and more".to_string()],
        "the buffer must hold the agent's text the moment the write answers"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read the written file"),
        "after\nand more\n",
        "the write never reached disk"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// The tick guard: a write against a buffer that moved since the resolve is
/// refused rather than applied over whatever the user did in between. The
/// refusal is answered, not dropped -- an agent can retry a stale tick, and
/// can do nothing at all with a request that never comes back.
#[test]
fn a_write_against_a_moved_buffer_is_refused_and_says_so() {
    let root = scratch_root("stale-tick");
    let path = root.join("moved.rs");
    std::fs::write(&path, "before\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let (buf, tick) = resolve(&engine, &rx, &name, 1);
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(0),
                rmpv::Value::from(-1),
                rmpv::Value::from(false),
                rmpv::Value::Array(vec![rmpv::Value::from("the user typed this")]),
            ],
        )
        .expect("move the buffer under the agent");

    engine
        .handle
        .ai_fs_write(9, BufferHandle(buf), &["agent text".to_owned()], true, tick)
        .expect("issue the stale write");

    let (_, result) = next_write_reply(&rx);
    assert!(result.is_err(), "a stale write must not be applied");
    assert_eq!(
        lines_of(&engine, buf),
        vec!["the user typed this".to_string()],
        "the refused write overwrote the user's own edit anyway"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("the file is still there"),
        "before\n",
        "a refused write must not touch disk either"
    );

    engine.handle.release_hidden(&name).expect("release");
}

/// Twenty reads of twenty never-opened files leave nvim's buffer count
/// where they found it. A hold that outlived its read would pin one buffer
/// per file the agent ever looked at, which over a long session is every
/// file in the project.
#[test]
fn twenty_reads_of_never_opened_files_leave_the_buffer_count_flat() {
    let root = scratch_root("flat-count");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    let baseline = buffer_count(&engine);

    for index in 0..20u64 {
        let path = root.join(format!("file-{index}.rs"));
        std::fs::write(&path, format!("line {index}\n")).expect("write fixture");
        let name = path.to_string_lossy().into_owned();

        let (buf, _tick) = resolve(&engine, &rx, &name, index);
        engine
            .handle
            .ai_fs_read(index, BufferHandle(buf), None, None)
            .expect("issue the read");
        let (_, result) = next_read_reply(&rx);
        assert_eq!(result.expect("the read answers"), format!("line {index}\n"));
        engine.handle.release_hidden(&name).expect("release");

        assert_eq!(
            buffer_count(&engine),
            baseline,
            "read {index} left a buffer behind"
        );
    }
}

/// Two overlapping reads of the same never-opened path -- the second issued
/// before the first has answered -- both answer correctly, and the buffer
/// survives until the second release. Under a rule that let whichever call
/// created the buffer delete it, the first completion would delete the
/// buffer the second is still reading through.
#[test]
fn two_overlapping_reads_of_one_path_both_answer_and_share_one_buffer() {
    let root = scratch_root("overlap");
    let path = root.join("shared.rs");
    std::fs::write(&path, "shared\n").expect("write fixture");
    let name = path.to_string_lossy().into_owned();

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    let baseline = buffer_count(&engine);

    let (first_buf, _) = resolve(&engine, &rx, &name, 1);
    let (second_buf, _) = resolve(&engine, &rx, &name, 2);
    assert_eq!(
        first_buf, second_buf,
        "a second hold on the same path must reuse the one buffer"
    );

    engine
        .handle
        .ai_fs_read(1, BufferHandle(first_buf), None, None)
        .expect("issue the first read");
    engine
        .handle
        .ai_fs_read(2, BufferHandle(second_buf), None, None)
        .expect("issue the second read");

    for _ in 0..2 {
        let (_, result) = next_read_reply(&rx);
        assert_eq!(result.expect("both reads answer"), "shared\n");
    }

    engine.handle.release_hidden(&name).expect("first release");
    assert_eq!(
        buffer_count(&engine),
        baseline + 1,
        "the buffer was deleted while a second holder still had it"
    );
    engine.handle.release_hidden(&name).expect("second release");
    assert_eq!(
        buffer_count(&engine),
        baseline,
        "the last release must delete the buffer"
    );
}

/// A read against a handle nvim no longer knows answers a refusal rather
/// than throwing. The agent is owed an answer either way, and a thrown Lua
/// error would arrive as a reply nothing correlates.
#[test]
fn a_read_against_an_invalid_handle_answers_a_refusal() {
    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .ai_fs_read(3, BufferHandle(9999), None, None)
        .expect("issue the read");

    let (request_id, result) = next_read_reply(&rx);
    assert_eq!(request_id, 3);
    assert!(
        matches!(result, Err(FsError::NotFound)),
        "an unusable handle must answer as a path that named nothing"
    );
}
