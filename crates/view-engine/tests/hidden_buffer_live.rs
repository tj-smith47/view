//! Live-nvim proof of [`EngineHandle::load_hidden`]/[`EngineHandle::release_hidden`]'s
//! contract: a path the agent named becomes the buffer handle nvim itself
//! owns, the file's text is loaded (so an attach to it reports edits
//! against real content), an already-open file resolves to the buffer
//! already holding it rather than a second one, an unreadable path answers
//! `None` instead of a handle nothing can be written through, the buffer is
//! never listed, and the per-path hold this pair maintains survives exactly
//! as many `release_hidden` calls as `load_hidden` calls were made before
//! deleting it.
//!
//! [`EngineHandle::load_hidden`]: view_engine::handle::EngineHandle::load_hidden
//! [`EngineHandle::release_hidden`]: view_engine::handle::EngineHandle::release_hidden
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
        .join(format!("hidden-buffer-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    std::fs::canonicalize(root).expect("canonicalize test root")
}

/// Waits up to 5s for the next `Msg::HiddenBufferLoaded`, skipping the
/// redraw traffic the UI attach produces.
fn next_hidden_buffer_loaded(rx: &mpsc::Receiver<Msg>) -> (u64, Option<u64>, bool, u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
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
                created,
                changedtick,
            }) => return (generation, buf.map(|b| b.0), created, changedtick),
            Ok(_other) => continue,
            Err(err) => panic!("channel closed before a HiddenBufferLoaded arrived: {err}"),
        }
    }
}

/// Waits up to 5s for the next `Msg::PickerBufferList`, skipping the redraw
/// traffic the UI attach produces.
fn next_picker_buffer_list(rx: &mpsc::Receiver<Msg>) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "no Msg::PickerBufferList arrived within 5s"
        );
        match rx.recv_timeout(remaining) {
            Ok(Msg::PickerBufferList { names, .. }) => return names,
            Ok(_other) => continue,
            Err(err) => panic!("channel closed before a PickerBufferList arrived: {err}"),
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

/// Unwraps a msgpack-RPC `Ext` handle (buffer/window/tabpage), whose payload
/// is itself a msgpack-packed integer -- the same shape
/// `view_engine::ui_events::decode_ext_handle` unwraps internally, but that
/// helper is `pub(crate)` and this file compiles as a separate crate.
fn decode_ext_handle(v: &rmpv::Value) -> Option<u64> {
    let rmpv::Value::Ext(_, data) = v else {
        return None;
    };
    let mut cursor = &data[..];
    rmpv::decode::read_value(&mut cursor).ok()?.as_u64()
}

/// Whether nvim's `nvim_list_bufs()` still names `buf`.
fn buf_still_listed_as_a_buffer(engine: &Engine, buf: u64) -> bool {
    engine
        .handle
        .request("nvim_list_bufs", vec![])
        .expect("list buffers")
        .as_array()
        .expect("nvim_list_bufs reply is an array")
        .iter()
        .filter_map(decode_ext_handle)
        .any(|handle| handle == buf)
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
        .load_hidden(&path.to_string_lossy(), 3)
        .expect("issue the load");

    let (generation, buf, created, tick) = next_hidden_buffer_loaded(&rx);
    assert_eq!(generation, 3, "the reply carries the caller's generation");
    assert!(created, "a never-opened path must create its buffer");
    let buf = buf.expect("an existing file resolves to a handle");
    assert!(buf > 0, "handle {buf} is not addressable");
    assert_eq!(
        tick,
        engine
            .handle
            .request(
                "nvim_exec_lua",
                vec![
                    rmpv::Value::from("return vim.api.nvim_buf_get_changedtick(...)"),
                    rmpv::Value::Array(vec![rmpv::Value::from(buf)]),
                ],
            )
            .expect("read changedtick")
            .as_u64()
            .expect("changedtick is an integer"),
        "the reply carries the buffer's own tick, which is what the \
         review's first write names"
    );
    assert_eq!(
        lines_of(&engine, buf),
        vec!["fn main() {}".to_string(), "fn other() {}".to_string()],
        "the loaded buffer was never populated from disk"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
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
        .load_hidden(&path.to_string_lossy(), 4)
        .expect("issue the load");

    let (_generation, buf, created, _tick) = next_hidden_buffer_loaded(&rx);
    assert_eq!(
        buf,
        Some(open_buf),
        "the load created a second buffer over a file already open"
    );
    assert!(
        !created,
        "a buffer a real window already holds must never read as newly created"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
    assert!(
        buf_still_listed_as_a_buffer(&engine, open_buf),
        "releasing a hold on a window-visible buffer must never delete the window's buffer"
    );
    let cur_buf = engine
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
    assert_eq!(
        cur_buf, open_buf,
        "nvim's own nvim_buf_delete does not refuse a window-visible buffer -- it \
         substitutes a fresh one into the window instead, so release_hidden must skip \
         the delete outright rather than trust a refusal that never comes"
    );
}

/// A path that names a directory is refused. nvim itself does not refuse
/// it -- `bufload` on a directory succeeds and yields a browsable listing,
/// live-observed here -- so the load has to, or a review would write its
/// hunks over the rows of a directory browser.
#[test]
fn an_unloadable_path_resolves_to_no_handle() {
    let root = scratch_root("directory");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&root.to_string_lossy(), 5)
        .expect("issue the load");

    let (generation, buf, created, _tick) = next_hidden_buffer_loaded(&rx);
    assert_eq!(generation, 5);
    assert_eq!(buf, None, "a directory answered a writable buffer handle");
    assert!(!created, "nothing was created for a refused path");
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
        .load_hidden(&path.to_string_lossy(), 6)
        .expect("issue the load");

    let (_generation, buf, created, _tick) = next_hidden_buffer_loaded(&rx);
    assert!(created, "a new-file path still creates its buffer");
    let buf = buf.expect("a file the agent proposes creating still resolves");
    assert_eq!(
        lines_of(&engine, buf),
        vec![String::new()],
        "a buffer for a file that does not exist holds one empty line"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
}

/// A hidden buffer with an unsaved edit -- an accepted hunk the panel has
/// not yet written to disk -- must survive `release_hidden` even once its
/// refcount reaches zero. Discarding it would be silent data loss: nvim
/// refuses the delete itself (unlike the window-visible case), and this
/// pins that the refusal's error, swallowed by the fire-and-forget call, is
/// never mistaken for permission to have deleted anyway.
#[test]
fn a_modified_hidden_buffer_survives_release_hidden() {
    let root = scratch_root("modified-survives");
    let path = root.join("edited.rs");
    std::fs::write(&path, "original\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 11)
        .expect("issue the load");
    let (_generation, buf, _created, _tick) = next_hidden_buffer_loaded(&rx);
    let buf = buf.expect("the fixture path resolves to a handle");

    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(0),
                rmpv::Value::from(1),
                rmpv::Value::from(false),
                rmpv::Value::Array(vec![rmpv::Value::from("edited, not yet written")]),
            ],
        )
        .expect("simulate an accepted hunk landing in the hidden buffer");

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");

    assert!(
        buf_still_listed_as_a_buffer(&engine, buf),
        "a hidden buffer with an unsaved edit must never be deleted by its release"
    );
    assert_eq!(
        lines_of(&engine, buf),
        vec!["edited, not yet written".to_string()],
        "the unsaved edit must survive the release untouched"
    );
}

/// A hidden buffer never reaches `Msg::PickerBufferList`, the picker's own
/// `Source::Buffers` enumeration -- a buffer created with `listed: true`
/// would show up here, offering the user a buffer they never opened.
#[test]
fn a_hidden_buffer_never_appears_in_the_picker_buffer_list() {
    let root = scratch_root("unlisted");
    let path = root.join("hidden.rs");
    std::fs::write(&path, "hidden content\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 7)
        .expect("issue the load");
    let (_generation, buf, _created, _tick) = next_hidden_buffer_loaded(&rx);
    let buf = buf.expect("the fixture path resolves to a handle");

    engine
        .handle
        .list_buffers(70)
        .expect("issue the buffer-list request");
    let names = next_picker_buffer_list(&rx);
    assert!(
        !names
            .iter()
            .any(|name| name == &path.to_string_lossy().into_owned()),
        "a hidden buffer's path leaked into the picker's own buffer source: {names:?}"
    );
    assert!(
        !engine
            .handle
            .request(
                "nvim_get_option_value",
                vec![
                    rmpv::Value::from("buflisted"),
                    rmpv::Value::Map(vec![(rmpv::Value::from("buf"), rmpv::Value::from(buf))]),
                ],
            )
            .expect("read buflisted")
            .as_bool()
            .expect("buflisted is a bool"),
        "a hidden buffer must never carry buflisted=true"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
}

/// Two `load_hidden` calls for the same never-opened path answer
/// `Created::Yes` then `Created::No`, both carrying the identical handle --
/// the existing-buffer lookup must run before ever calling
/// `nvim_create_buf` a second time.
#[test]
fn a_second_load_hidden_for_the_same_path_reports_created_false_with_the_same_handle() {
    let root = scratch_root("created-flag");
    let path = root.join("twice.rs");
    std::fs::write(&path, "loaded once\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 8)
        .expect("issue the first load");
    let (_g1, buf1, created1, _t1) = next_hidden_buffer_loaded(&rx);
    assert!(
        created1,
        "the first call over a never-opened path must create"
    );
    let buf1 = buf1.expect("the first load resolves to a handle");

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 9)
        .expect("issue the second load");
    let (_g2, buf2, created2, _t2) = next_hidden_buffer_loaded(&rx);
    assert!(
        !created2,
        "a second load over the same path must reuse, not recreate"
    );
    assert_eq!(
        buf2,
        Some(buf1),
        "both calls must resolve to the identical buffer handle"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the first hold");
    assert!(
        buf_still_listed_as_a_buffer(&engine, buf1),
        "one hold remains after only one of two releases"
    );
    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the second hold");
    assert!(
        !buf_still_listed_as_a_buffer(&engine, buf1),
        "the buffer must be gone once every holder has released"
    );
}

/// Two concurrent holders on the same never-opened path -- the exact shape
/// a diff review's own hold and a standalone hidden-buffer open would
/// produce together: two `load_hidden` calls, followed by one
/// `release_hidden`, leave the buffer alive; a second `release_hidden`
/// deletes it. A stub that deletes on the first `release_hidden`
/// regardless of the outstanding count would destroy a buffer a concurrent
/// holder still needs.
#[test]
fn two_concurrent_holders_on_the_same_path_leave_the_buffer_alive_until_both_release() {
    let root = scratch_root("refcount");
    let path = root.join("shared.rs");
    std::fs::write(&path, "shared\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 20)
        .expect("first holder's load");
    let (_g1, buf1, _c1, _t1) = next_hidden_buffer_loaded(&rx);
    let buf = buf1.expect("first load resolves to a handle");

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 21)
        .expect("second holder's load");
    let (_g2, buf2, _c2, _t2) = next_hidden_buffer_loaded(&rx);
    assert_eq!(buf2, Some(buf), "both holders share the identical buffer");

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("first holder releases");
    assert!(
        buf_still_listed_as_a_buffer(&engine, buf),
        "the second holder's release has not happened yet -- the buffer must survive"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("second holder releases");
    assert!(
        !buf_still_listed_as_a_buffer(&engine, buf),
        "both holders have released -- the buffer must now be gone"
    );
}
