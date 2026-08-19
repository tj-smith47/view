//! Live-nvim proof of [`EngineHandle::buf_resolve`]'s contract: a path the
//! agent named becomes the buffer handle nvim itself owns, the file's text
//! is loaded (so an attach to it reports edits against real content), an
//! already-open file resolves to the buffer already holding it rather than
//! a second one, and an unreadable path answers `None` instead of a handle
//! nothing can be written through.
//!
//! [`EngineHandle::buf_resolve`]: view_engine::handle::EngineHandle::buf_resolve
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use view_core::msg::Msg;
use view_engine::process::{Engine, EngineConfig};

/// Spawns an isolated engine with a UI attached, the same load-bearing
/// attach `buf_set_text_live.rs`'s own `spawn` documents: without it nvim's
/// main loop has no idle tick between back-to-back API calls.
fn spawn() -> Engine {
    let engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    engine.handle.ui_attach(80, 24).expect("attach ui");
    engine
}

/// A per-test scratch directory under the workspace target dir, the same
/// nonce shape `open_file_live.rs` uses -- no dev-dependency on a temp-dir
/// crate for three files.
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
        .join(format!("buf-resolve-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    std::fs::canonicalize(root).expect("canonicalize test root")
}

/// Waits up to 5s for the next `Msg::BufResolved`, skipping the redraw
/// traffic the UI attach produces.
fn next_buf_resolved(rx: &mpsc::Receiver<Msg>) -> (u64, Option<u64>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "no Msg::BufResolved arrived within 5s"
        );
        match rx.recv_timeout(remaining) {
            Ok(Msg::BufResolved { generation, buf }) => return (generation, buf.map(|b| b.0)),
            Ok(_other) => continue,
            Err(err) => panic!("channel closed before a BufResolved arrived: {err}"),
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

/// A path on disk resolves to a real handle whose buffer already holds the
/// file's text. The loaded-ness is the load-bearing half: an unloaded
/// buffer reads as empty, so a review attached to it would rebase its
/// hunks against nothing and a write would land at rows that do not exist.
#[test]
fn a_file_on_disk_resolves_to_a_loaded_buffer() {
    let root = scratch_root("loaded");
    let path = root.join("resolve.rs");
    std::fs::write(&path, "fn main() {}\nfn other() {}\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .buf_resolve(&path.to_string_lossy(), 3)
        .expect("issue the resolve");

    let (generation, buf) = next_buf_resolved(&rx);
    assert_eq!(generation, 3, "the reply carries the review's generation");
    let buf = buf.expect("an existing file resolves to a handle");
    assert!(buf > 0, "handle {buf} is not addressable");
    assert_eq!(
        lines_of(&engine, buf),
        vec!["fn main() {}".to_string(), "fn other() {}".to_string()],
        "the resolved buffer was never loaded from disk"
    );
}

/// A file the user already has open resolves to the buffer already holding
/// it. A second buffer over the same file would put the review's writes
/// somewhere the user is not looking, and leave their own edits invisible
/// to its rebase.
#[test]
fn an_already_open_file_resolves_to_the_buffer_already_holding_it() {
    let root = scratch_root("already-open");
    let path = root.join("open.rs");
    std::fs::write(&path, "already open\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .open_file(&path.to_string_lossy())
        .expect("open the file the way the user would");
    let open_buf = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("return vim.api.nvim_get_current_buf()"),
                rmpv::Value::Array(vec![]),
            ],
        )
        .expect("read current buffer")
        .as_u64()
        .expect("buffer handle is an integer");

    engine
        .handle
        .buf_resolve(&path.to_string_lossy(), 4)
        .expect("issue the resolve");

    let (_generation, buf) = next_buf_resolved(&rx);
    assert_eq!(
        buf,
        Some(open_buf),
        "the resolve created a second buffer over a file already open"
    );
}

/// A path that names a directory is refused. nvim itself does not refuse
/// it -- `bufload` on a directory succeeds and yields a browsable listing,
/// live-observed here -- so the resolve has to, or a review would write
/// its hunks over the rows of a directory browser.
#[test]
fn an_unloadable_path_resolves_to_no_handle() {
    let root = scratch_root("directory");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .buf_resolve(&root.to_string_lossy(), 5)
        .expect("issue the resolve");

    let (generation, buf) = next_buf_resolved(&rx);
    assert_eq!(generation, 5);
    assert_eq!(buf, None, "a directory answered a writable buffer handle");
}

/// A file that does not exist yet resolves to a handle: that is the
/// new-file proposal's own case (`old_text: None`), where the review
/// writes the whole file into the buffer nvim will create it from. Only a
/// path that exists and is not a regular file is refused.
#[test]
fn a_not_yet_existing_file_resolves_to_an_empty_buffer() {
    let root = scratch_root("new-file");
    let path = root.join("does-not-exist.rs");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .buf_resolve(&path.to_string_lossy(), 6)
        .expect("issue the resolve");

    let (_generation, buf) = next_buf_resolved(&rx);
    let buf = buf.expect("a file the agent proposes creating still resolves");
    assert_eq!(
        lines_of(&engine, buf),
        vec![String::new()],
        "a buffer for a file that does not exist holds one empty line"
    );
}
