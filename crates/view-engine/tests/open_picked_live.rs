//! Live-nvim check of `EngineHandle::open_picked_target`: each window a
//! picker key names, a grep match's line, and listed buffers by handle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

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
        engine.handle.open_picked_target(&picked, how).unwrap();
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
        .open_picked_target(&picked, OpenIn::Current)
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
        engine.handle.open_file(&name).unwrap();
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
        engine.handle.open_file(&name).unwrap();

        let picked = Picked::Buffer { handle: second };
        engine.handle.open_picked_target(&picked, how).unwrap();
        assert_eq!(current_buffer(&engine), second, "{how:?}: the second");
        let (_, _, wins, tabpages) = state(&engine);
        assert_eq!((wins, tabpages), (windows, tabs), "{how:?}");

        let picked = Picked::Buffer { handle: listed };
        engine
            .handle
            .open_picked_target(&picked, OpenIn::Current)
            .unwrap();
        assert!(same_file(&state(&engine).0, &path), "{how:?}: the file");
    }
    let _ = std::fs::remove_dir_all(&root);
}
