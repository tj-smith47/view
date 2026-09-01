//! Live-nvim proof of [`EngineHandle::buf_attach`]/[`buf_detach`]'s
//! lifecycle: attaching subscribes to `nvim_buf_lines_event`, one edit
//! produces exactly one event bounding only the edited range, detaching
//! stops delivery, and two concurrently attached buffers with distinct
//! generations never cross-deliver -- an event for one buffer never
//! surfaces stamped with the other's generation. See
//! `docs/nvim-buf-attach-wire-capture.md` for the wire evidence these
//! assertions mirror.
//!
//! [`EngineHandle::buf_attach`]: view_engine::handle::EngineHandle::buf_attach
//! [`buf_detach`]: view_engine::handle::EngineHandle::buf_detach
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use view_core::msg::{BufferHandle, Msg};
use view_engine::process::{Engine, EngineConfig};

/// Spawns an isolated engine with a UI attached, the same load-bearing
/// attach `buf_set_text_live.rs`'s own `spawn` documents: without it,
/// nvim's main loop has no idle tick to actually process buffer edits
/// issued back-to-back with the read that follows them.
fn spawn() -> Engine {
    let engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .expect("attach ui");
    engine
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

/// Resolves the current buffer's real handle. `buf_attach`/`buf_detach`
/// must be called with this, never the bare `0` ("current buffer")
/// sentinel `nvim_buf_set_lines` and friends accept elsewhere in this
/// file: every `nvim_buf_lines_event` a `0`-sentinel attach produces still
/// names the buffer by its real, resolved number (capture #1 in
/// `docs/nvim-buf-attach-wire-capture.md`), so a generation recorded under
/// the sentinel could never be looked up by any event that attach
/// produces.
fn current_buf(engine: &Engine) -> u64 {
    // `nvim_exec_lua`, not the raw `nvim_get_current_buf` RPC method
    // directly: that method's own reply is `Buffer`-typed and crosses the
    // wire as an `Ext` value (the same shape `decode_ext_handle` unwraps in
    // the production decoder, not reachable from an external test crate),
    // while a Lua integer returned through `nvim_exec_lua` serializes as a
    // plain `Integer` -- the same idiom `create_scratch_buf` below already
    // uses for the same reason.
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("return vim.api.nvim_get_current_buf()"),
                rmpv::Value::Array(vec![]),
            ],
        )
        .expect("read current buffer handle")
        .as_u64()
        .expect("current buffer handle is an integer")
}

fn create_scratch_buf(engine: &Engine) -> u64 {
    engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("return vim.api.nvim_create_buf(false, true)"),
                rmpv::Value::Array(vec![]),
            ],
        )
        .expect("create scratch buffer")
        .as_u64()
        .expect("buffer handle is an integer")
}

/// Waits up to 5s for the next `Msg::BufTextChanged` on `rx`, skipping over
/// any other `Msg` the reader thread routes in the meantime (redraw
/// traffic from the UI attach chiefly) -- the same "wait for the one
/// message that matters" shape `redraw_live.rs`'s own tests use.
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

/// Drains `rx` for `window`, asserting no `Msg::BufTextChanged` arrives --
/// unrelated `Msg`s (redraw traffic) are skipped rather than treated as a
/// failure, the same tolerance `next_buf_text_changed` extends. A plain
/// `rx.try_recv().is_err()` only proves the channel was empty at one
/// instant; this proves it stayed empty for the whole window, which is
/// what a "no further event" claim actually needs.
fn assert_no_further_buf_text_changed(rx: &mpsc::Receiver<Msg>, window: Duration) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::BufTextChanged { .. }) => {
                panic!("a BufTextChanged arrived when none was expected")
            }
            Ok(_other) => continue,
            Err(_timeout_or_closed) => break,
        }
    }
}

/// Attaching a buffer and editing one line produces exactly one
/// `Msg::BufTextChanged` bounding that line, never the whole buffer --
/// the falsifiable check the brief states directly. `send_buffer: false`
/// means the attach itself fires no initial event, so the only event this
/// test can observe is the one the edit below produces.
#[test]
fn attach_then_one_edit_produces_one_event_bounding_only_the_edited_line() {
    let mut engine = spawn();
    set_lines(&engine, 0, &["line1", "line2", "line3"]);
    let buf = current_buf(&engine);
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .buf_attach(BufferHandle(buf), 7)
        .expect("attach to the current buffer");

    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(1),
                rmpv::Value::from(2),
                rmpv::Value::from(false),
                rmpv::Value::Array(vec![rmpv::Value::from("LINE2")]),
            ],
        )
        .expect("edit line 2");

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
    assert_eq!(generation, 7);
    assert_eq!(
        (firstline, lastline),
        (1, 2),
        "the event must bound only the edited line, never the whole buffer"
    );
    assert_eq!(linedata, vec!["LINE2".to_string()]);

    assert_no_further_buf_text_changed(&rx, Duration::from_millis(500));
}

/// `buf_detach` stops delivery: an edit issued after detaching produces no
/// `Msg::BufTextChanged`, proven by an edit that would otherwise be
/// unmistakable (a distinct line count change) never showing up within a
/// bounded wait.
#[test]
fn detach_stops_further_events() {
    let mut engine = spawn();
    set_lines(&engine, 0, &["line1", "line2"]);
    let buf = current_buf(&engine);
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .buf_attach(BufferHandle(buf), 3)
        .expect("attach to the current buffer");
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(0),
                rmpv::Value::from(1),
                rmpv::Value::from(false),
                rmpv::Value::Array(vec![rmpv::Value::from("EDITED1")]),
            ],
        )
        .expect("edit before detach");
    let _ = next_buf_text_changed(&rx);

    engine
        .handle
        .buf_detach(BufferHandle(buf))
        .expect("detach from the current buffer");
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(1),
                rmpv::Value::from(2),
                rmpv::Value::from(false),
                rmpv::Value::Array(vec![rmpv::Value::from("EDITED2")]),
            ],
        )
        .expect("edit after detach");

    // 500ms comfortably exceeds a local nvim's round trip for one small
    // edit; this alone only proves this connection's own decode gate
    // dropped the event locally, not that nvim itself stopped sending --
    // see `buf_detach_stops_delivery_on_the_wire_itself` below for that
    // stronger claim.
    assert_no_further_buf_text_changed(&rx, Duration::from_millis(500));
}

/// `detach_stops_further_events` above proves no `Msg::BufTextChanged`
/// reaches the consumer after `buf_detach` -- but `buf_detach` clears this
/// connection's own attach-generation entry proactively, before its
/// `nvim_buf_detach` notify even reaches the writer thread, so
/// `decode_buf_lines_event`'s map-miss gate would drop every subsequent
/// event on the LOCAL side regardless of whether nvim itself ever stopped
/// sending them. A notify that silently never left this process (a dropped
/// `nvim_buf_detach` send) would be indistinguishable from a genuine detach
/// under that test alone.
///
/// This test closes that gap: after a real `buf_detach`, it re-arms the
/// local entry with `note_buf_attach_for_test` -- local bookkeeping only,
/// no wire effect -- so a still-live wire subscription (exactly what a
/// dropped notify would leave behind) would decode and reach `rx`
/// faithfully. Only a connection nvim genuinely detached stays silent with
/// the local gate artificially held open.
#[test]
fn buf_detach_stops_delivery_on_the_wire_itself() {
    let mut engine = spawn();
    set_lines(&engine, 0, &["line1", "line2"]);
    let buf = current_buf(&engine);
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .buf_attach(BufferHandle(buf), 11)
        .expect("attach to the current buffer");
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(0),
                rmpv::Value::from(1),
                rmpv::Value::from(false),
                rmpv::Value::Array(vec![rmpv::Value::from("EDITED1")]),
            ],
        )
        .expect("edit before detach");
    let _ = next_buf_text_changed(&rx);

    engine
        .handle
        .buf_detach(BufferHandle(buf))
        .expect("detach from the current buffer");
    // re-arm the local decode gate ONLY -- no `nvim_buf_attach` notify --
    // so a still-live wire subscription would decode and be delivered
    engine.handle.note_buf_attach_for_test(buf, 99);
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf),
                rmpv::Value::from(1),
                rmpv::Value::from(2),
                rmpv::Value::from(false),
                rmpv::Value::Array(vec![rmpv::Value::from("EDITED2")]),
            ],
        )
        .expect("edit after detach, with the local gate re-armed");

    assert_no_further_buf_text_changed(&rx, Duration::from_millis(500));
}

/// Two buffers attached with distinct generations never cross-deliver:
/// editing buffer A produces an event stamped with A's own generation,
/// never B's, and vice versa -- the disconfirm the brief requires for
/// concurrent diff-review sessions on different buffers.
#[test]
fn two_concurrently_attached_buffers_never_cross_deliver_generations() {
    let mut engine = spawn();
    let buf_a = current_buf(&engine);
    let buf_b = create_scratch_buf(&engine);
    set_lines(&engine, buf_a, &["a-line1", "a-line2"]);
    set_lines(&engine, buf_b, &["b-line1", "b-line2"]);
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .buf_attach(BufferHandle(buf_a), 101)
        .expect("attach buffer A");
    engine
        .handle
        .buf_attach(BufferHandle(buf_b), 202)
        .expect("attach buffer B");

    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf_a),
                rmpv::Value::from(0),
                rmpv::Value::from(1),
                rmpv::Value::from(false),
                rmpv::Value::Array(vec![rmpv::Value::from("A-EDITED")]),
            ],
        )
        .expect("edit buffer A");
    let Msg::BufTextChanged {
        buf, generation, ..
    } = next_buf_text_changed(&rx)
    else {
        unreachable!("next_buf_text_changed only returns this variant")
    };
    assert_eq!(buf, BufferHandle(buf_a));
    assert_eq!(
        generation, 101,
        "buffer A's event must carry A's own generation, never B's"
    );

    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                rmpv::Value::from(buf_b),
                rmpv::Value::from(0),
                rmpv::Value::from(1),
                rmpv::Value::from(false),
                rmpv::Value::Array(vec![rmpv::Value::from("B-EDITED")]),
            ],
        )
        .expect("edit buffer B");
    let Msg::BufTextChanged {
        buf, generation, ..
    } = next_buf_text_changed(&rx)
    else {
        unreachable!("next_buf_text_changed only returns this variant")
    };
    assert_eq!(buf, BufferHandle(buf_b));
    assert_eq!(
        generation, 202,
        "buffer B's event must carry B's own generation, never A's"
    );
}

fn scratch_file(nonce_suffix: &str, contents: &str) -> std::path::PathBuf {
    let nonce = format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
        nonce_suffix
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/tmp");
    std::fs::create_dir_all(&root).expect("create test root");
    let path = root.join(format!("buf-attach-edit-bang-{nonce}.txt"));
    std::fs::write(&path, contents).expect("seed scratch file");
    path
}

/// Waits up to 5s for the next `Msg::BufDetached` on `rx`, skipping over any
/// other `Msg` in the meantime -- the `Msg::BufDetached` analogue of
/// `next_buf_text_changed`.
fn next_buf_detached(rx: &mpsc::Receiver<Msg>) -> Msg {
    let deadline = Instant::now() + common::rpc_deadline();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "no Msg::BufDetached arrived within 5s"
        );
        match rx.recv_timeout(remaining) {
            Ok(msg @ Msg::BufDetached { .. }) => return msg,
            Ok(_other) => continue,
            Err(err) => panic!("channel closed before a BufDetached arrived: {err}"),
        }
    }
}

/// `:edit!` forcing a reload of an attached buffer is nvim deciding to
/// detach this connection's subscription unasked -- section 7 of
/// `docs/nvim-buf-attach-wire-capture.md` captures the raw
/// `nvim_buf_detach_event` this produces with no `nvim_buf_detach` request
/// ever sent by this connection. This test proves `crate::handle` surfaces
/// that as `Msg::BufDetached`, the vocabulary a rebase state machine needs
/// to learn its subscription died -- distinct from a self-initiated
/// `buf_detach`, which produces no `Msg` at all (`detach_stops_further_events`
/// above).
#[test]
fn nvim_initiated_detach_via_edit_bang_produces_msg_buf_detached() {
    let mut engine = spawn();
    let path = scratch_file("reload", "disk-line1\ndisk-line2\n");
    engine
        .handle
        .request(
            "nvim_command",
            vec![rmpv::Value::from(format!("edit {}", path.display()))],
        )
        .expect("open the scratch file");
    let buf = current_buf(&engine);
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    engine
        .handle
        .buf_attach(BufferHandle(buf), 42)
        .expect("attach to the reloaded-later buffer");

    // rewrite the file out from under nvim, then force a reload -- nvim
    // itself decides to detach this buffer's subscription, unasked
    std::fs::write(&path, "disk-line1-CHANGED\ndisk-line2\n").expect("mutate file on disk");
    engine
        .handle
        .request("nvim_command", vec![rmpv::Value::from("edit!")])
        .expect("force reload");

    let Msg::BufDetached {
        buf: event_buf,
        generation,
    } = next_buf_detached(&rx)
    else {
        unreachable!("next_buf_detached only returns this variant")
    };
    assert_eq!(event_buf, BufferHandle(buf));
    assert_eq!(generation, 42);

    let _ = std::fs::remove_file(&path);
}
