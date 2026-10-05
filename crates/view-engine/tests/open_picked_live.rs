//! Live-nvim check of `EngineHandle::open_picked`: each window a picker key
//! names, a grep match's line, a listed buffer by its name, and an unnamed
//! one.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc;

use rmpv::Value;
use view_core::msg::OpenIn;
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
        engine
            .handle
            .open_picked(&name, Some(3), false, how)
            .unwrap();
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
    engine
        .handle
        .open_picked(&path.to_string_lossy(), Some(40), false, OpenIn::Current)
        .unwrap();
    assert_eq!(state(&engine).1, 2);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_listed_buffer_and_an_unnamed_one_open_by_name() {
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
        engine.handle.command("enew").unwrap();
        engine.handle.open_picked(&name, None, true, how).unwrap();
        let (current, _, wins, tabpages) = state(&engine);
        assert!(same_file(&current, &path), "{how:?}: {current}");
        assert_eq!((wins, tabpages), (windows, tabs), "{how:?}");

        engine.handle.open_picked("", None, true, how).unwrap();
        assert_eq!(state(&engine).0, "", "{how:?}: the unnamed buffer");
    }
    let _ = std::fs::remove_dir_all(&root);
}
