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

mod common;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use view_core::msg::{BufferHandle, Msg, TextEdit};
use view_engine::handle::EngineError;
use view_engine::nvim_api::{BufWriteOutcome, HiddenPathRefusal};
// named only by the symlink-built canon-drift pin below, which is unix-only
#[cfg(unix)]
use view_engine::nvim_api::{hidden_buffer_key, hidden_path_refusal};
use view_engine::process::{Engine, EngineConfig};

/// Spawns an isolated engine with a UI attached, the same load-bearing
/// attach `buf_set_text_live.rs`'s own `spawn` documents: without it nvim's
/// main loop has no idle tick between back-to-back API calls.
fn spawn() -> Engine {
    let engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .expect("attach ui");
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
    let deadline = Instant::now() + common::rpc_deadline();
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

/// Reads a buffer-scoped option (`&fileformat`, `&endofline`, `&filetype`,
/// ...) via `nvim_get_option_value`, the same call the picker test already
/// uses for `buflisted`.
fn buf_option(engine: &Engine, buf: u64, name: &str) -> rmpv::Value {
    engine
        .handle
        .request(
            "nvim_get_option_value",
            vec![
                rmpv::Value::from(name),
                rmpv::Value::Map(vec![(rmpv::Value::from("buf"), rmpv::Value::from(buf))]),
            ],
        )
        .expect("read buffer-scoped option")
}

/// Waits up to 5s for the next `Msg::BufTextChanged` on `rx`, skipping the
/// redraw traffic the UI attach produces -- the same helper
/// `buf_attach_live.rs` uses.
fn next_buf_text_changed(rx: &mpsc::Receiver<Msg>) -> Msg {
    let deadline = Instant::now() + common::rpc_deadline();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "no Msg::BufTextChanged arrived within 5s"
        );
        match rx.recv_timeout(remaining) {
            Ok(msg @ Msg::BufTextChanged { .. }) => return msg,
            Ok(_other) => continue,
            Err(err) => panic!("channel closed before a BufTextChanged arrived: {err}"),
        }
    }
}

/// One buffer's own name, as nvim stores it.
fn buf_name(engine: &Engine, buf: u64) -> String {
    engine
        .handle
        .request("nvim_buf_get_name", vec![rmpv::Value::from(buf)])
        .expect("read buffer name")
        .as_str()
        .expect("a buffer name is a string")
        .to_owned()
}

/// Every buffer nvim currently holds, by name.
fn buffer_names(engine: &Engine) -> Vec<String> {
    engine
        .handle
        .request("nvim_list_bufs", vec![])
        .expect("list buffers")
        .as_array()
        .expect("nvim_list_bufs reply is an array")
        .iter()
        .filter_map(decode_ext_handle)
        .map(|handle| buf_name(engine, handle))
        .collect()
}

/// `LOAD_HIDDEN_CHUNK`'s own answer for `path`, driven directly rather than
/// through `load_hidden` -- which refuses the same spellings first, so the
/// chunk's own half of the belt-and-braces pair would otherwise never run
/// against real nvim. Answers the `buf` field, `0` being the chunk's
/// refusal.
fn load_chunk_answer(engine: &Engine, path: &str) -> u64 {
    let reply = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(view_engine::nvim_api::HIDDEN_LOAD_CHUNK),
                rmpv::Value::Array(vec![rmpv::Value::from(path)]),
            ],
        )
        .expect("run the load chunk");
    reply
        .as_map()
        .expect("the chunk answers a map")
        .iter()
        .find(|(k, _)| k.as_str() == Some("buf"))
        .and_then(|(_, v)| v.as_u64())
        .expect("the chunk's answer names a buf")
}

/// What `LOAD_HIDDEN_CHUNK`'s own `canon()` resolves `path` to, inside
/// nvim, from the identical literal the chunk itself embeds.
#[cfg(unix)]
fn canon_in_nvim(engine: &Engine, path: &str) -> String {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(view_engine::nvim_api::HIDDEN_CANON_PROBE_CHUNK),
                rmpv::Value::Array(vec![rmpv::Value::from(path)]),
            ],
        )
        .expect("run the canon probe")
        .as_str()
        .expect("canon answers a string")
        .to_owned()
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
        "nvim's own nvim_buf_delete does not refuse a window-visible buffer -- \
         it substitutes a fresh one into the window instead, so release_hidden \
         must skip the delete outright rather than trust a refusal that never \
         comes"
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

/// The directory refusal above still has to hold when a buffer already
/// exists for that path -- an earlier `:edit <dir>` in the same session
/// leaves nvim holding a browsable directory-listing buffer under that
/// exact name, and `LOAD_HIDDEN_CHUNK`'s existing-buffer scan must never be
/// allowed to find it before the `fs_stat` refusal ever runs. Getting the
/// order backward would resolve this call onto the directory-listing
/// buffer instead of refusing, and a review would then write its hunks over
/// the listing's own rows.
#[test]
fn a_directory_with_an_existing_buffer_is_still_refused() {
    let root = scratch_root("directory-with-buffer");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .open_file(&root.to_string_lossy())
        .expect("open the directory the way :edit would, leaving a buffer behind");

    engine
        .handle
        .load_hidden(&root.to_string_lossy(), 51)
        .expect("issue the load");

    let (generation, buf, created, _tick) = next_hidden_buffer_loaded(&rx);
    assert_eq!(generation, 51);
    assert_eq!(
        buf, None,
        "a directory that already has a buffer must still be refused, not \
         resolved onto that buffer's directory listing"
    );
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

/// The regression `bufadd`/`bufload` fixes: `nvim_buf_set_lines` populating a
/// freshly created buffer recorded that population as an undoable edit, so
/// the first `u` a user pressed after the file was later opened normally in
/// that same buffer emptied it back to nothing (see
/// `docs/hidden-buffer-wire-capture.md` capture #11). `bufload`'s own read is
/// the buffer's undo baseline instead: `:undo` right after a `load_hidden`
/// must be a no-op.
#[test]
fn undo_right_after_load_hidden_does_not_empty_the_buffer() {
    let root = scratch_root("undo-baseline");
    let path = root.join("undo.rs");
    std::fs::write(&path, "one\ntwo\nthree\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 100)
        .expect("issue the load");
    let (_generation, buf, _created, _tick) = next_hidden_buffer_loaded(&rx);
    let buf = buf.expect("the fixture path resolves to a handle");

    let seq_cur = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("return vim.fn.undotree(...).seq_cur"),
                rmpv::Value::Array(vec![rmpv::Value::from(buf)]),
            ],
        )
        .expect("read undotree().seq_cur")
        .as_u64();
    assert_eq!(
        seq_cur,
        Some(0),
        "bufload's own read must be the undo baseline, not an undoable edit"
    );

    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("vim.api.nvim_buf_call(..., function() vim.cmd('undo') end)"),
                rmpv::Value::Array(vec![rmpv::Value::from(buf)]),
            ],
        )
        .expect("issue :undo against the loaded buffer");
    assert_eq!(
        lines_of(&engine, buf),
        vec!["one".to_string(), "two".to_string(), "three".to_string()],
        "a single :undo right after load_hidden must never empty the buffer"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
}

/// `bufload` preserves `fileformat`, unlike the old `nvim_create_buf` +
/// `nvim_buf_set_lines` mechanism, which hardcoded Unix line endings
/// regardless of the source file (`docs/hidden-buffer-wire-capture.md`
/// capture #12) -- silently corrupting a CRLF file's line endings on its
/// next `:write`. This proves the round-trip: writing the loaded buffer back
/// out reproduces the exact CRLF bytes it was loaded from.
#[test]
fn a_crlf_fixture_writes_back_byte_identical() {
    let root = scratch_root("crlf");
    let path = root.join("crlf.txt");
    std::fs::write(&path, "one\r\ntwo\r\n").expect("write CRLF fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 101)
        .expect("issue the load");
    let (_generation, buf, _created, _tick) = next_hidden_buffer_loaded(&rx);
    let buf = buf.expect("the fixture path resolves to a handle");

    assert_eq!(
        buf_option(&engine, buf, "fileformat").as_str(),
        Some("dos"),
        "a CRLF source file must read as 'dos', not the hardcoded Unix default"
    );

    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("vim.api.nvim_buf_call(..., function() vim.cmd('write') end)"),
                rmpv::Value::Array(vec![rmpv::Value::from(buf)]),
            ],
        )
        .expect("write the loaded buffer back to disk");
    assert_eq!(
        std::fs::read(&path).expect("read fixture back off disk"),
        b"one\r\ntwo\r\n",
        "a CRLF file must round-trip through load_hidden + :write byte-identical"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
}

/// `bufload` also reads `endofline` correctly from a source file with no
/// trailing newline, where the old mechanism left every hidden buffer
/// reading as if the source always had one (`docs/hidden-buffer-wire-capture.md`
/// capture #12). `fixendofline` re-adding a trailing newline on `:write` is
/// nvim's own default behavior, identical for a real `:edit` -- not
/// something this test asserts against.
#[test]
fn a_no_trailing_newline_fixture_reports_endofline_false() {
    let root = scratch_root("no-eol");
    let path = root.join("no-eol.txt");
    std::fs::write(&path, "a\nb").expect("write no-trailing-newline fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 102)
        .expect("issue the load");
    let (_generation, buf, _created, _tick) = next_hidden_buffer_loaded(&rx);
    let buf = buf.expect("the fixture path resolves to a handle");

    assert_eq!(
        buf_option(&engine, buf, "endofline").as_bool(),
        Some(false),
        "a source file with no trailing newline must read endofline=false, \
         not the hardcoded true the old mechanism left every hidden buffer with"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
}

/// `bufload` runs nvim's ordinary file-open autocommands, which
/// `nvim_create_buf` never triggered -- filetype detection among them. A
/// `.rs` fixture must read as `filetype=rust` once loaded.
#[test]
fn a_rust_fixture_gets_filetype_detected() {
    let root = scratch_root("filetype");
    let path = root.join("detected.rs");
    std::fs::write(&path, "fn main() {}\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 103)
        .expect("issue the load");
    let (_generation, buf, _created, _tick) = next_hidden_buffer_loaded(&rx);
    let buf = buf.expect("the fixture path resolves to a handle");

    assert_eq!(
        buf_option(&engine, buf, "filetype").as_str(),
        Some("rust"),
        "nvim_create_buf never ran filetype detection at all; bufload must"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
}

/// A buffer a real window already had open before this connection's own
/// `load_hidden` ever ran must survive release even after that window moves
/// on to a different buffer -- `win_findbuf` alone would then see nothing
/// showing it, so the engine-side `owned` gate (never set for a buffer this
/// connection did not create) is what has to refuse the delete instead.
#[test]
fn an_already_open_buffer_survives_release_even_after_its_window_moves_on() {
    let root = scratch_root("already-open-window-closed");
    let path = root.join("already-open.rs");
    std::fs::write(&path, "already open content\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .open_file(&path.to_string_lossy())
        .expect("open the file the way the user would, before any load_hidden call");
    let user_buf = engine
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
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("vim.cmd('enew')"),
                rmpv::Value::Array(vec![]),
            ],
        )
        .expect("move the window's current buffer off the user's file");

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 104)
        .expect("issue the load");
    let (_generation, buf, created, _tick) = next_hidden_buffer_loaded(&rx);
    assert_eq!(
        buf,
        Some(user_buf),
        "the load must resolve onto the user's own already-open buffer"
    );
    assert!(
        !created,
        "a buffer the user already owns must never read as newly created"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
    assert!(
        buf_still_listed_as_a_buffer(&engine, user_buf),
        "releasing a hold on a buffer this connection never created must never \
         delete it, window-visible or not"
    );
}

/// The `owned` gate's own protection zone, distinct from
/// `an_already_open_buffer_survives_release_even_after_its_window_moves_on`
/// above: that test's fixture is `buflisted=1` (opened through
/// `OPEN_FILE_CHUNK`), so `RELEASE_HIDDEN_CHUNK`'s own `buflisted` check
/// alone already refuses the delete, whether or not the engine-side `owned`
/// gate exists. This one instead builds a buffer directly through
/// `bufadd`+`bufload` -- unlisted, hidden, never opened through this
/// connection's own `open_file` -- so nothing in Lua would refuse the
/// delete on its own; only the engine's `owned` gate (never set for a
/// `load_hidden` reply that resolved onto a buffer it did not create) is
/// what keeps `RELEASE_HIDDEN_CHUNK` from ever being sent for it at all.
#[test]
fn an_unlisted_foreign_buffer_survives_release_with_no_lua_guard_protecting_it() {
    let root = scratch_root("foreign-unlisted");
    let path = root.join("foreign.rs");
    std::fs::write(&path, "foreign content\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let foreign_buf = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("local b = vim.fn.bufadd(...) vim.fn.bufload(b) return b"),
                rmpv::Value::Array(vec![rmpv::Value::from(path.to_string_lossy().into_owned())]),
            ],
        )
        .expect("build a buffer this connection's own load_hidden never made")
        .as_u64()
        .expect("buffer handle is an integer");
    assert_eq!(
        buf_option(&engine, foreign_buf, "buflisted").as_bool(),
        Some(false),
        "the fixture must be unlisted, or the buflisted belt-check alone \
         would protect it and this test would prove nothing about owned"
    );

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 200)
        .expect("issue the load");
    let (_generation, buf, created, _tick) = next_hidden_buffer_loaded(&rx);
    assert_eq!(
        buf,
        Some(foreign_buf),
        "the load must resolve onto the buffer already sitting at this path"
    );
    assert!(
        !created,
        "a buffer this connection's own load_hidden did not make must never \
         read as newly created"
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
    assert!(
        buf_still_listed_as_a_buffer(&engine, foreign_buf),
        "releasing a hold on a buffer this connection never created must \
         never delete it, even when nothing in Lua would have refused to"
    );
}

/// A buffer this connection's own `load_hidden` created can still be adopted
/// by the user afterward: a real `:edit` on the same path binds onto it and
/// flips `buflisted` 0 -> 1 without ever creating a second buffer (capture
/// #14). Once that happens, release must never delete it even after the
/// window that adopted it moves on to something else, where `win_findbuf`
/// alone would see nothing showing it. The engine-side `owned` gate alone
/// would let this delete through -- this connection really did create the
/// buffer -- so this specifically proves `RELEASE_HIDDEN_CHUNK`'s own
/// `buflisted` check.
#[test]
fn a_buffer_this_connection_created_survives_release_once_the_user_adopts_it() {
    let root = scratch_root("adopted-after-create");
    let path = root.join("adopted.rs");
    std::fs::write(&path, "adopted content\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 105)
        .expect("issue the load");
    let (_generation, buf, created, _tick) = next_hidden_buffer_loaded(&rx);
    assert!(created, "a never-opened path must create its buffer");
    let buf = buf.expect("the load resolves to a handle");

    engine
        .handle
        .open_file(&path.to_string_lossy())
        .expect("the user opens the same path, adopting this connection's own buffer");
    let opened_buf = engine
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
        opened_buf, buf,
        "the open must adopt this connection's own hidden buffer, not create a second one"
    );
    assert_eq!(
        buf_option(&engine, buf, "buflisted").as_bool(),
        Some(true),
        "the real :edit must have listed the adopted buffer"
    );

    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("vim.cmd('enew')"),
                rmpv::Value::Array(vec![]),
            ],
        )
        .expect("move the window off the adopted buffer");

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
    assert!(
        buf_still_listed_as_a_buffer(&engine, buf),
        "a buffer the user adopted after this connection created it must \
         survive release even once no window shows it"
    );
}

/// Two different spellings of the identical file must share one refcount
/// entry rather than racing each other's cleanup: both resolve to the same
/// buffer, and the buffer survives until both spellings' holds have
/// released.
///
/// Unix-only, from measurement rather than from principle: on windows the
/// scratch root is a verbatim (`\\?\`) path, where a doubled separator is
/// not a respelling of one file but a second path. `canonicalize` collapses
/// it and nvim's own `canon()` does not, so the two spellings came back as
/// two buffers (buf 2 and buf 3) on windows-msvc -- a divergence about that
/// spelling, not about the refcount this asserts. The new-file variant
/// below keeps the same property on both platforms, since its lexical
/// fallback answers for both spellings.
#[cfg(unix)]
#[test]
fn two_spellings_of_the_same_path_share_one_hold() {
    let root = scratch_root("dual-spelling");
    let path = root.join("shared-spelling.rs");
    std::fs::write(&path, "same file\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let canonical = path.to_string_lossy().into_owned();
    // a doubled separator rather than a `.` component: windows normalizes
    // the component away while pushing onto the verbatim prefix the scratch
    // root carries, which would leave two identical strings to compare
    let respelled = format!(
        "{}{sep}{sep}shared-spelling.rs",
        root.to_string_lossy(),
        sep = std::path::MAIN_SEPARATOR
    );
    assert_ne!(
        canonical, respelled,
        "the two spellings must actually differ as strings for this test to prove anything"
    );

    engine
        .handle
        .load_hidden(&canonical, 106)
        .expect("first spelling's load");
    let (_g1, buf1, _c1, _t1) = next_hidden_buffer_loaded(&rx);
    let buf = buf1.expect("first load resolves to a handle");

    engine
        .handle
        .load_hidden(&respelled, 107)
        .expect("second spelling's load");
    let (_g2, buf2, _c2, _t2) = next_hidden_buffer_loaded(&rx);
    assert_eq!(
        buf2,
        Some(buf),
        "both spellings must resolve to the identical buffer"
    );

    engine
        .handle
        .release_hidden(&canonical)
        .expect("first spelling releases");
    assert!(
        buf_still_listed_as_a_buffer(&engine, buf),
        "one hold remains -- the two spellings must share a single refcount \
         entry, not race each other's cleanup"
    );

    engine
        .handle
        .release_hidden(&respelled)
        .expect("second spelling releases");
    assert!(
        !buf_still_listed_as_a_buffer(&engine, buf),
        "both spellings' holds have released -- the buffer must now be gone"
    );
}

/// The same sharing `two_spellings_of_the_same_path_share_one_hold` proves
/// for a file that already exists on disk, for one that does not yet exist
/// -- the new-file proposal's own case. `std::fs::canonicalize` fails for
/// both spellings here (neither exists), so `canonical_hidden_key` falls
/// back to a lexical normalization rather than a filesystem-backed one; if
/// that fallback left the doubled separator uncollapsed the two spellings
/// would key two separate holds over the one buffer nvim resolves both onto
/// (nvim's own `canon()` falls back to `fnamemodify(p, ':p')`, which does
/// collapse it), and the first `release_hidden` would delete that buffer
/// while the second spelling's hold still thinks it owns it.
#[test]
fn two_spellings_of_a_not_yet_existing_path_share_one_hold() {
    let root = scratch_root("dual-spelling-new-file");
    let path = root.join("brand-new.rs");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let canonical = path.to_string_lossy().into_owned();
    let respelled = format!(
        "{}{sep}{sep}brand-new.rs",
        root.to_string_lossy(),
        sep = std::path::MAIN_SEPARATOR
    );
    assert_ne!(
        canonical, respelled,
        "the two spellings must actually differ as strings for this test to prove anything"
    );
    assert!(
        std::fs::canonicalize(&path).is_err(),
        "the fixture must not exist on disk, or this test would exercise the \
         canonicalize-succeeds path instead of its fallback"
    );

    engine
        .handle
        .load_hidden(&canonical, 200)
        .expect("first spelling's load");
    let (_g1, buf1, c1, _t1) = next_hidden_buffer_loaded(&rx);
    assert!(c1, "the first load over a never-proposed path must create");
    let buf = buf1.expect("first load resolves to a handle");

    engine
        .handle
        .load_hidden(&respelled, 201)
        .expect("second spelling's load");
    let (_g2, buf2, c2, _t2) = next_hidden_buffer_loaded(&rx);
    assert!(
        !c2,
        "the second spelling must reuse the buffer the first spelling created"
    );
    assert_eq!(
        buf2,
        Some(buf),
        "both spellings of the not-yet-existing path must resolve to the identical buffer"
    );

    engine
        .handle
        .release_hidden(&canonical)
        .expect("first spelling releases");
    assert!(
        buf_still_listed_as_a_buffer(&engine, buf),
        "one hold remains -- an unnormalized fallback key would let this \
         release delete the buffer the second spelling's hold still needs"
    );

    engine
        .handle
        .release_hidden(&respelled)
        .expect("second spelling releases");
    assert!(
        !buf_still_listed_as_a_buffer(&engine, buf),
        "both spellings' holds have released -- the buffer must now be gone"
    );
}

/// The live-probe case `docs/hidden-buffer-wire-capture.md` case 15
/// measured directly against `bufadd`, and the one the original
/// shared-buffer-deleted bug actually reproduced on: a symlinked directory
/// with a leaf that does not exist yet, and -- critically -- no `.`/`..`
/// component anywhere in either spelling. `fnamemodify(p, ':p')` (what
/// `LOAD_HIDDEN_CHUNK`'s own `canon()` falls back to in its scan loop) never
/// resolves the symlink for a dot-free spelling like this, but `bufadd`'s
/// own identity check has no such gate and resolves it anyway -- so
/// `canonical_hidden_key`'s fallback must match `bufadd`, not
/// `fnamemodify`, or the two spellings below key two separate holds over
/// the one buffer nvim itself resolves both onto, and the first
/// `release_hidden` deletes it out from under the second spelling's still-
/// live hold.
#[test]
#[cfg(unix)]
fn two_spellings_through_a_symlinked_directory_with_no_dot_component_share_one_hold() {
    let root = scratch_root("dual-spelling-symlink-no-dot");
    let real_dir = root.join("real");
    std::fs::create_dir_all(&real_dir).expect("create real dir");
    let link_dir = root.join("link");
    std::os::unix::fs::symlink(&real_dir, &link_dir).expect("create symlink");

    let via_real = real_dir.join("brand-new.rs").to_string_lossy().into_owned();
    let via_link = link_dir.join("brand-new.rs").to_string_lossy().into_owned();
    assert_ne!(
        via_real, via_link,
        "the two spellings must actually differ as strings for this test to prove anything"
    );
    assert!(
        !via_link.contains("/./") && !via_link.contains("/../"),
        "the symlinked spelling must carry no '.'/'..' component -- that is \
         exactly the case fnamemodify(':p') leaves unresolved but bufadd does \
         not"
    );
    assert!(
        std::fs::canonicalize(link_dir.join("brand-new.rs")).is_err(),
        "the fixture must not exist on disk, or this test exercises the \
         canonicalize-succeeds path instead of its fallback"
    );

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&via_real, 210)
        .expect("real spelling's load");
    let (_g1, buf1, c1, _t1) = next_hidden_buffer_loaded(&rx);
    assert!(c1, "the first load over a never-proposed path must create");
    let buf = buf1.expect("first load resolves to a handle");

    engine
        .handle
        .load_hidden(&via_link, 211)
        .expect("symlinked spelling's load");
    let (_g2, buf2, c2, _t2) = next_hidden_buffer_loaded(&rx);
    assert!(
        !c2,
        "the symlinked spelling must reuse the buffer the real spelling created"
    );
    assert_eq!(
        buf2,
        Some(buf),
        "both spellings must resolve to the identical buffer"
    );

    engine
        .handle
        .release_hidden(&via_real)
        .expect("real spelling releases");
    assert!(
        buf_still_listed_as_a_buffer(&engine, buf),
        "one hold remains -- the symlinked spelling's hold must not have been \
         orphaned by a key that failed to match the real spelling's"
    );

    engine
        .handle
        .release_hidden(&via_link)
        .expect("symlinked spelling releases");
    assert!(
        !buf_still_listed_as_a_buffer(&engine, buf),
        "both spellings' holds have released -- the buffer must now be gone"
    );
}

/// `BufAttach`/`BufSetText` operate unbranched on a `load_hidden`-resolved
/// handle: nothing about it (unlisted, file-backed, `bufload`-populated) is
/// special-cased anywhere else in the RPC surface, so attaching to it and
/// writing through `set_buf_text` must behave exactly like any other real
/// buffer -- an applied write at the named `changedtick`, and a correctly
/// generation-stamped `Msg::BufTextChanged` for the edit.
#[test]
fn buf_attach_and_set_buf_text_operate_unbranched_on_a_hidden_buffer() {
    let root = scratch_root("attach-and-write");
    let path = root.join("attach.rs");
    std::fs::write(&path, "line1\nline2\n").expect("write fixture");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&path.to_string_lossy(), 108)
        .expect("issue the load");
    let (_generation, buf, _created, tick) = next_hidden_buffer_loaded(&rx);
    let buf = buf.expect("the fixture path resolves to a handle");

    engine
        .handle
        .buf_attach(BufferHandle(buf), 12)
        .expect("attach to the hidden buffer");

    let outcome = engine
        .handle
        .set_buf_text(
            BufferHandle(buf),
            &[TextEdit {
                start_row: 1,
                start_col: 0,
                end_row: 1,
                end_col: 5,
                lines: vec!["LINE2".to_string()],
            }],
            false,
            Some(tick),
        )
        .expect("apply the edit against the hidden buffer");
    assert!(
        matches!(outcome, BufWriteOutcome::Applied { .. }),
        "a write at the buffer's own just-loaded changedtick must apply, got {outcome:?}"
    );

    let Msg::BufTextChanged {
        buf: event_buf,
        generation,
        firstline,
        lastline,
        linedata,
        ..
    } = next_buf_text_changed(&rx)
    else {
        unreachable!("next_buf_text_changed only returns this variant")
    };
    assert_eq!(event_buf, BufferHandle(buf));
    assert_eq!(
        generation, 12,
        "the event must carry this attach's own generation"
    );
    assert_eq!((firstline, lastline), (1, 2));
    assert_eq!(linedata, vec!["LINE2".to_string()]);
    assert_eq!(
        lines_of(&engine, buf),
        vec!["line1".to_string(), "LINE2".to_string()]
    );

    engine
        .handle
        .release_hidden(&path.to_string_lossy())
        .expect("release the one hold this test took");
}

/// An empty proposal path resolves onto nvim's own `[No Name]` buffer --
/// the user's scratch buffer, live-confirmed as buffer 1 with an empty name
/// (`docs/hidden-buffer-wire-capture.md` case 17). A review bound to it
/// would attach to it and write its hunks into it. Both ends refuse: the
/// engine before the request is even built, and the chunk itself when
/// driven directly.
#[test]
fn an_empty_path_is_refused_at_both_ends_rather_than_bound_to_nvims_no_name_buffer() {
    let mut engine = spawn();
    let (tx, _rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    for blank in ["", "   "] {
        let err = engine
            .handle
            .load_hidden(blank, 300)
            .expect_err("a blank path must refuse rather than resolve");
        assert!(
            matches!(
                err,
                EngineError::UnusablePath {
                    reason: HiddenPathRefusal::Blank,
                    ..
                }
            ),
            "a blank path must refuse as unusable, not as a lost engine: {err:?}"
        );
        assert_eq!(
            load_chunk_answer(&engine, blank),
            0,
            "nvim's own [No Name] buffer must never be returned as a hidden-buffer hit"
        );
    }

    assert!(
        buf_still_listed_as_a_buffer(&engine, 1),
        "the fixture only proves anything while nvim's unnamed buffer 1 still exists"
    );
    assert_eq!(
        buf_name(&engine, 1),
        "",
        "buffer 1 must be the unnamed scratch buffer the empty path would have matched"
    );
}

/// A relative proposal path resolves against nvim's cwd, which `:cd` moves
/// and view's own process never observes (case 19) -- two authorities for
/// one buffer identity. `docs/acp-v1-wire-capture.md`'s `Diff` schema
/// documents `path` as "The absolute file path being modified," so the
/// spelling is off-contract and is refused rather than resolved against
/// either cwd.
#[test]
fn a_relative_path_is_refused_rather_than_keyed_against_this_processs_cwd() {
    let mut engine = spawn();
    let (tx, _rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    for relative in ["rel.rs", "./rel.rs", "sub/rel.rs"] {
        let err = engine
            .handle
            .load_hidden(relative, 301)
            .expect_err("a relative path must refuse");
        assert!(
            matches!(
                err,
                EngineError::UnusablePath {
                    reason: HiddenPathRefusal::Relative,
                    ..
                }
            ),
            "a relative path must refuse as unusable: {err:?}"
        );
        engine
            .handle
            .release_hidden(relative)
            .expect("a refused path's release is the same no-op an unheld path gets");
    }
}

/// A trailing separator is a second, distinct nvim buffer over the same
/// file (case 18): `bufadd` keeps the separator, the hold key drops it, and
/// the two spellings shared one hold over two buffers. Refused at both ends
/// rather than normalized -- a trailing separator names a directory, and a
/// directory is already refused, but the existing `fs_stat` refusal never
/// fires for a leaf that does not exist.
#[test]
fn a_trailing_separator_is_refused_rather_than_keyed_onto_a_second_buffer() {
    let root = scratch_root("trailing-separator");
    let bare = root.join("nope.rs").to_string_lossy().into_owned();
    let with_separator = format!("{bare}/");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .load_hidden(&bare, 302)
        .expect("the bare spelling loads");
    let (_g, buf, created, _t) = next_hidden_buffer_loaded(&rx);
    assert!(created, "the bare spelling creates the one buffer");
    let buf = buf.expect("the bare spelling resolves to a handle");

    let err = engine
        .handle
        .load_hidden(&with_separator, 303)
        .expect_err("the trailing-separator spelling must refuse");
    assert!(
        matches!(
            err,
            EngineError::UnusablePath {
                reason: HiddenPathRefusal::TrailingSeparator,
                ..
            }
        ),
        "a trailing separator must refuse as unusable: {err:?}"
    );
    assert_eq!(
        load_chunk_answer(&engine, &with_separator),
        0,
        "the chunk itself must refuse it too -- fs_stat cannot, the leaf does not exist"
    );
    assert!(
        !buffer_names(&engine).iter().any(|n| n.ends_with('/')),
        "no second buffer may exist over the same file: {:?}",
        buffer_names(&engine)
    );

    engine
        .handle
        .release_hidden(&with_separator)
        .expect("a refused path's release is a no-op");
    assert!(
        buf_still_listed_as_a_buffer(&engine, buf),
        "a refused spelling must never decrement the bare spelling's hold"
    );

    engine
        .handle
        .release_hidden(&bare)
        .expect("the one real hold releases");
    assert!(
        !buf_still_listed_as_a_buffer(&engine, buf),
        "the bare spelling's own hold reached zero and must have deleted its buffer"
    );
}

/// The chunk refuses both separator characters unconditionally -- Lua has no
/// portable separator predicate -- so the Rust gate must refuse both too, on
/// every platform. On Unix a trailing backslash names an ordinary readable
/// file that `bufadd` binds without complaint (case 21), which is what makes
/// the disagreement observable: a platform-keyed Rust gate let this spelling
/// take a hold, reach the wire, and come back `buf = 0`.
///
/// Unix-only because a file whose name ends in a backslash cannot be created
/// on Windows -- there the two ends agree trivially, since `is_separator`
/// answers for `\` as well.
#[test]
#[cfg(unix)]
fn a_trailing_backslash_is_refused_here_exactly_as_the_chunk_refuses_it() {
    let root = scratch_root("trailing-backslash");
    let bare = root.join("nope.rs").to_string_lossy().into_owned();
    let with_backslash = format!("{bare}\\");
    std::fs::write(&with_backslash, "a real file on this platform\n")
        .expect("a trailing backslash is an ordinary filename on unix");

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let err = engine
        .handle
        .load_hidden(&with_backslash, 320)
        .expect_err("the trailing-backslash spelling must refuse");
    assert!(
        matches!(
            err,
            EngineError::UnusablePath {
                reason: HiddenPathRefusal::TrailingSeparator,
                ..
            }
        ),
        "a trailing backslash must refuse as unusable: {err:?}"
    );
    // the answer this request would produce, not an idle channel: the same
    // receiver carries the attach's own redraw traffic (see
    // `next_hidden_buffer_loaded`), so a contended run has unrelated
    // messages queued here and an emptiness check reads one of them as an
    // answer
    assert!(
        !std::iter::from_fn(|| rx.try_recv().ok())
            .any(|msg| matches!(msg, Msg::HiddenBufferLoaded { .. })),
        "a refused spelling never reaches the wire, so nothing may answer it"
    );
    assert_eq!(
        load_chunk_answer(&engine, &with_backslash),
        0,
        "the chunk refuses it too -- the two ends must refuse the identical set"
    );
    assert!(
        !buffer_names(&engine).iter().any(|n| n.ends_with('\\')),
        "no buffer may exist over the refused spelling: {:?}",
        buffer_names(&engine)
    );

    engine
        .handle
        .release_hidden(&with_backslash)
        .expect("a refused path's release is the same no-op an unheld path gets");

    engine
        .handle
        .load_hidden(&bare, 321)
        .expect("the bare spelling is still usable");
    let (_g, buf, created, _t) = next_hidden_buffer_loaded(&rx);
    assert!(created, "the refusal left no hold the bare spelling reuses");
    let buf = buf.expect("the bare spelling resolves to a handle");
    engine
        .handle
        .release_hidden(&bare)
        .expect("the one real hold releases");
    assert!(
        !buf_still_listed_as_a_buffer(&engine, buf),
        "the bare spelling's own hold reached zero and must have deleted its buffer"
    );
}

/// Two implementations of one algorithm: `canonical_hidden_key` on this
/// side of the wire and `LOAD_HIDDEN_CHUNK`'s own `canon()` on nvim's. They
/// must answer identically for every spelling in the divergent set
/// (`docs/hidden-buffer-wire-capture.md` cases 15, 16 and 20) or the scan
/// misses a reuse the key shares, or shares a key the scan splits. Nothing
/// but this test keeps them in agreement.
///
/// Unix-only for the same reason
/// `two_spellings_through_a_symlinked_directory_...` is: the divergent set
/// is built from symlinked directories, and the `/`-joined spelling the two
/// sides agree on here is a POSIX one. What matters on every platform --
/// two spellings nvim resolves onto one buffer share one hold -- is pinned
/// by the not-yet-existing-path reuse test above, which is not gated.
#[test]
#[cfg(unix)]
fn the_hold_key_answers_exactly_what_the_load_chunks_own_canon_answers() {
    let root = scratch_root("canon-drift-pin");
    let real_dir = root.join("real");
    std::fs::create_dir_all(&real_dir).expect("create real dir");
    let link_dir = root.join("link");
    std::os::unix::fs::symlink(&real_dir, &link_dir).expect("create symlink");
    std::fs::write(real_dir.join("exists.rs"), "x\n").expect("write fixture");

    let r = root.to_string_lossy().into_owned();
    let spellings = [
        format!("{r}/real/nope.rs"),
        format!("{r}/link/nope.rs"),
        format!("{r}/link/./nope.rs"),
        format!("{r}/real//nope.rs"),
        format!("{r}/real/./nope.rs"),
        format!("{r}/real/../real/nope.rs"),
        format!("{r}/link/sub/../nope.rs"),
        format!("{r}/real/exists.rs"),
        format!("{r}/link/exists.rs"),
        "/../a".to_string(),
    ];

    let engine = spawn();
    for spelling in &spellings {
        assert_eq!(
            hidden_path_refusal(spelling),
            None,
            "the divergent set must be spellings that actually reach a hold: {spelling}"
        );
        assert_eq!(
            hidden_buffer_key(spelling),
            canon_in_nvim(&engine, spelling),
            "the Rust hold key and the chunk's own canon() disagree on {spelling}"
        );
    }
}

/// The scenario the `canon()` fix was made for, which no test covered: a
/// foreign, unlisted buffer already sitting at the real spelling, then one
/// `load_hidden` through the symlinked one. The old `canon()` missed the
/// match, fell through to `bufadd` (whose `created = true` is
/// unconditional), and handed this connection ownership of a buffer it
/// never made -- which `release_hidden` then deleted, with neither Lua
/// belt-check applying to an unlisted buffer no window shows.
#[test]
#[cfg(unix)]
fn a_foreign_buffer_reached_through_a_symlinked_spelling_is_neither_owned_nor_deleted() {
    let root = scratch_root("foreign-through-symlink");
    let real_dir = root.join("real");
    std::fs::create_dir_all(&real_dir).expect("create real dir");
    let link_dir = root.join("link");
    std::os::unix::fs::symlink(&real_dir, &link_dir).expect("create symlink");
    let real_path = real_dir.join("foreign.rs");
    let via_link = link_dir.join("foreign.rs").to_string_lossy().into_owned();
    assert!(
        !via_link.contains("/./") && !via_link.contains("/../"),
        "the symlinked spelling must carry no '.'/'..' component -- that is \
         exactly the case the old canon() left unresolved"
    );
    assert!(
        std::fs::canonicalize(&real_path).is_err(),
        "the leaf must not exist on disk: with the file there, fs_realpath \
         resolves both spellings outright and the parent-only fallback -- the \
         half that was broken, and the only half this test exists to pin -- \
         never runs at all"
    );

    let mut engine = spawn();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let foreign_buf = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("local b = vim.fn.bufadd(...) vim.fn.bufload(b) return b"),
                rmpv::Value::Array(vec![rmpv::Value::from(
                    real_path.to_string_lossy().into_owned(),
                )]),
            ],
        )
        .expect("build a buffer this connection's own load_hidden never made")
        .as_u64()
        .expect("buffer handle is an integer");
    assert_eq!(
        buf_option(&engine, foreign_buf, "buflisted").as_bool(),
        Some(false),
        "the fixture must be unlisted, or the buflisted belt-check alone would \
         protect it and this test would prove nothing about owned"
    );

    engine
        .handle
        .load_hidden(&via_link, 304)
        .expect("the symlinked spelling loads");
    let (_g, buf, created, _t) = next_hidden_buffer_loaded(&rx);
    assert_eq!(
        buf,
        Some(foreign_buf),
        "the symlinked spelling must resolve onto the buffer already at the real one"
    );
    assert!(
        !created,
        "a buffer this connection never made must never read as newly created"
    );

    engine
        .handle
        .release_hidden(&via_link)
        .expect("release the one hold this test took");
    assert!(
        buf_still_listed_as_a_buffer(&engine, foreign_buf),
        "a foreign buffer reached through a symlinked spelling must survive release"
    );
}
