//! Live-nvim check of `EngineHandle::open_picked`: each window a picker key
//! names, a grep match's line, listed buffers by handle, the window a
//! sidebar was entered from, and the message an open that cannot happen
//! leaves.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::mpsc;

use rmpv::Value;
use view_core::msg::OpenIn;
use view_core::native::picker::Picked;
use view_engine::process::{Engine, EngineConfig};

fn scratch_root(suffix: &str) -> std::path::PathBuf {
    let nonce = format!(
        "{}-{}-{suffix}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tmp")
        .join(format!("open-picked-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    std::fs::canonicalize(root).expect("canonicalize test root")
}

fn spawn() -> Engine {
    let mut engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    drop(rx);
    engine
}

/// The current buffer's full name, the cursor line, and how many windows
/// and tabs there are. The request travels the same ordered stream as the
/// notify before it, so the open has run by the time this answers.
fn state(engine: &Engine) -> (String, u64, u64, u64) {
    let reply = engine
        .handle
        .request(
            "nvim_eval",
            vec![Value::from(
                "[bufname('%'), line('.'), winnr('$'), tabpagenr('$')]",
            )],
        )
        .expect("read the editor state");
    let fields = reply.as_array().expect("a list");
    (
        fields[0].as_str().unwrap_or_default().to_owned(),
        fields[1].as_u64().unwrap(),
        fields[2].as_u64().unwrap(),
        fields[3].as_u64().unwrap(),
    )
}

fn same_file(a: &str, b: &std::path::Path) -> bool {
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(std::path::Path::new(a)) == canon(b)
}

#[test]
fn a_file_opens_at_its_line_in_each_window_a_key_names() {
    let root = scratch_root("files");
    let path = root.join("a file 100% #x.txt");
    std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
    let name = path.to_string_lossy().into_owned();
    for (how, windows, tabs) in [
        (OpenIn::Current, 1, 1),
        (OpenIn::Vertical, 2, 1),
        (OpenIn::Horizontal, 2, 1),
        (OpenIn::Tab, 1, 2),
    ] {
        let engine = spawn();
        let picked = Picked::File {
            path: name.clone(),
            line: Some(3),
        };
        engine.handle.open_picked(&picked, how, false, 0).unwrap();
        let (current, line, wins, tabpages) = state(&engine);
        assert!(same_file(&current, &path), "{how:?}: {current}");
        assert_eq!(line, 3, "{how:?}");
        assert_eq!((wins, tabpages), (windows, tabs), "{how:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_line_past_the_end_lands_on_the_last_line() {
    let root = scratch_root("short");
    let path = root.join("short.txt");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    let engine = spawn();
    let picked = Picked::File {
        path: path.to_string_lossy().into_owned(),
        line: Some(40),
    };
    engine
        .handle
        .open_picked(&picked, OpenIn::Current, false, 0)
        .unwrap();
    assert_eq!(state(&engine).1, 2);
    let _ = std::fs::remove_dir_all(&root);
}

/// The current buffer's handle.
fn current_buffer(engine: &Engine) -> u64 {
    engine
        .handle
        .request("nvim_eval", vec![Value::from("bufnr('%')")])
        .expect("read the current buffer")
        .as_u64()
        .unwrap()
}

/// A listed buffer opens by its handle, and of two unnamed buffers the one
/// chosen opens. Opened by name, the first unnamed one opened for either.
#[test]
fn a_listed_buffer_and_each_of_two_unnamed_ones_open_by_handle() {
    let root = scratch_root("buffers");
    let path = root.join("listed.txt");
    std::fs::write(&path, "listed\n").unwrap();
    let name = path.to_string_lossy().into_owned();
    for (how, windows, tabs) in [
        (OpenIn::Current, 1, 1),
        (OpenIn::Vertical, 2, 1),
        (OpenIn::Horizontal, 2, 1),
        (OpenIn::Tab, 1, 2),
    ] {
        let engine = spawn();
        common::open_file(&engine.handle, &name).unwrap();
        let listed = current_buffer(&engine);
        // an empty unnamed buffer is the one `:enew` reuses, so each holds
        // a line before the next is made
        engine.handle.command("enew").unwrap();
        engine.handle.command("call setline(1, 'one')").unwrap();
        let first = current_buffer(&engine);
        engine.handle.command("enew").unwrap();
        engine.handle.command("call setline(1, 'two')").unwrap();
        let second = current_buffer(&engine);
        assert!(listed != first && first != second, "{how:?}");
        common::open_file(&engine.handle, &name).unwrap();

        let picked = Picked::Buffer { handle: second };
        engine.handle.open_picked(&picked, how, false, 0).unwrap();
        assert_eq!(current_buffer(&engine), second, "{how:?}: the second");
        let (_, _, wins, tabpages) = state(&engine);
        assert_eq!((wins, tabpages), (windows, tabs), "{how:?}");

        let picked = Picked::Buffer { handle: listed };
        engine
            .handle
            .open_picked(&picked, OpenIn::Current, false, 0)
            .unwrap();
        assert!(same_file(&state(&engine).0, &path), "{how:?}: the file");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// An engine whose routed messages the test reads.
fn spawn_routed() -> (Engine, mpsc::Receiver<view_core::msg::Msg>) {
    let mut engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    let (tx, rx) = mpsc::sync_channel(256);
    let (_pump, _cutover) = engine.start_pump(tx);
    (engine, rx)
}

/// Waits for nvim's answer to the open tagged `generation`, and checks it
/// names the window the cursor is in.
fn answered(engine: &Engine, rx: &mpsc::Receiver<view_core::msg::Msg>, generation: u64) -> bool {
    let deadline = common::rpc_poll_deadline();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(view_core::msg::Msg::PickedOpened {
                generation: g,
                window,
            }) if g == generation => {
                let current = engine
                    .handle
                    .request("nvim_eval", vec![Value::from("win_getid()")])
                    .unwrap()
                    .as_u64()
                    .map(view_core::events::WinHandle);
                assert_eq!(window, current, "the answer names another window");
                return true;
            }
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

/// nvim's message history, one line per message.
fn messages(engine: &Engine) -> String {
    engine
        .handle
        .request("nvim_exec2", vec![Value::from("messages"), opts_output()])
        .expect("read the message history")
        .as_map()
        .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("output")))
        .and_then(|(_, v)| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn opts_output() -> Value {
    Value::Map(vec![(Value::from("output"), Value::from(true))])
}

/// Every open is answered, and one that cannot happen leaves the person a
/// message saying why: a file deleted since it was listed, a buffer closed
/// since, and an open nvim refuses over unsaved changes.
#[test]
fn an_open_that_cannot_happen_is_answered_and_says_why() {
    let root = scratch_root("refused");
    let kept = root.join("kept.txt");
    let other = root.join("other.txt");
    std::fs::write(&kept, "kept\n").unwrap();
    std::fs::write(&other, "other\n").unwrap();
    let gone = root.join("gone.txt").to_string_lossy().into_owned();
    let (engine, rx) = spawn_routed();
    common::open_file(&engine.handle, &kept.to_string_lossy()).unwrap();

    let deleted = Picked::File {
        path: gone.clone(),
        line: None,
    };
    engine
        .handle
        .open_picked(&deleted, OpenIn::Current, false, 1)
        .unwrap();
    assert!(
        answered(&engine, &rx, 1),
        "the deleted file's open was never answered"
    );
    assert!(same_file(&state(&engine).0, &kept), "a deleted file opened");
    let said = messages(&engine);
    assert!(said.contains(&format!("{gone} no longer exists")), "{said}");

    engine.handle.command("enew").unwrap();
    let closed = current_buffer(&engine);
    engine.handle.command("buffer #").unwrap();
    engine
        .handle
        .command(&format!("bwipeout {closed}"))
        .unwrap();
    let wiped = Picked::Buffer { handle: closed };
    engine
        .handle
        .open_picked(&wiped, OpenIn::Vertical, false, 2)
        .unwrap();
    assert!(
        answered(&engine, &rx, 2),
        "the closed buffer's open was never answered"
    );
    assert_eq!(state(&engine).2, 1, "a closed buffer opened a split");
    assert!(messages(&engine).contains("That buffer has been closed"));

    engine.handle.command("set nohidden").unwrap();
    engine.handle.command("call setline(1, 'edited')").unwrap();
    let refused = Picked::File {
        path: other.to_string_lossy().into_owned(),
        line: None,
    };
    engine
        .handle
        .open_picked(&refused, OpenIn::Current, false, 3)
        .unwrap();
    assert!(
        answered(&engine, &rx, 3),
        "the refused open was never answered"
    );
    assert!(
        same_file(&state(&engine).0, &kept),
        "the refused open moved"
    );
    let said = messages(&engine);
    assert!(said.contains("E37: No write since last change"), "{said}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Chosen from a sidebar's window, a file opens in the window entered
/// before it, and the sidebar keeps its own buffer.
#[test]
fn a_file_chosen_from_a_sidebar_opens_in_the_window_before_it() {
    let root = scratch_root("previous");
    let path = root.join("chosen.txt");
    std::fs::write(&path, "chosen\n").unwrap();
    let (engine, rx) = spawn_routed();
    engine
        .handle
        .command("vsplit | enew | file sidebar")
        .unwrap();
    let picked = Picked::File {
        path: path.to_string_lossy().into_owned(),
        line: None,
    };
    engine
        .handle
        .open_picked(&picked, OpenIn::Current, true, 0)
        .unwrap();
    assert!(answered(&engine, &rx, 0), "the open was never answered");
    assert!(same_file(&state(&engine).0, &path));
    let sidebar = engine
        .handle
        .request("nvim_eval", vec![Value::from("bufwinnr('sidebar')")])
        .unwrap()
        .as_i64()
        .unwrap();
    assert!(sidebar > 0, "the sidebar lost its buffer");
    let _ = std::fs::remove_dir_all(&root);
}

/// An operator left pending when the open lands is dropped, so the keys
/// typed behind the open start a command of their own.
#[test]
fn an_operator_left_pending_is_dropped_by_the_open() {
    let root = scratch_root("pending");
    let path = root.join("chosen.txt");
    std::fs::write(&path, "one\ntwo\n").unwrap();
    // nvim reads no typed key before a UI attaches
    let (engine, _rx) = spawn_routed();
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS_MULTIGRID)
        .unwrap();
    engine.handle.input("d").unwrap();
    let mode = |engine: &Engine| {
        engine
            .handle
            .request("nvim_eval", vec![Value::from("mode(1)")])
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    };
    // nvim reads the typed key on a later turn of its loop than this request
    let deadline = common::rpc_poll_deadline();
    while mode(&engine) != "no" {
        assert!(
            std::time::Instant::now() < deadline,
            "the operator never went pending"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    common::open_file(&engine.handle, &path.to_string_lossy()).unwrap();
    assert!(same_file(&state(&engine).0, &path));
    assert_eq!(mode(&engine), "n");
    let _ = std::fs::remove_dir_all(&root);
}
