//! Live-nvim check that a preview request for a line deep in a large file
//! returns the window around that line, and answers `loaded: false` for a
//! file no buffer holds so the disk read takes over.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::mpsc::{self, Receiver};
use std::time::Instant;

use rmpv::Value;
use view_core::msg::Msg;
use view_engine::process::{Engine, EngineConfig};

fn reply(rx: &Receiver<Msg>, generation: u64) -> Option<(bool, Vec<String>)> {
    let deadline = Instant::now() + common::rpc_deadline();
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::PickerPreviewReply {
                generation: g,
                loaded,
                lines,
                ..
            }) if g == generation => return Some((loaded, lines)),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    None
}

#[test]
fn a_match_deep_in_a_large_file_previews_the_window_around_it() {
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tmp")
        .join(format!("preview-window-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    let text: String = (1..=10_000).map(|n| format!("line {n}\n")).collect();
    let open = root.join("open.txt");
    let closed = root.join("closed.txt");
    std::fs::write(&open, &text).expect("write open file");
    std::fs::write(&closed, &text).expect("write closed file");

    let mut engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let open_str = open.to_string_lossy().into_owned();
    engine
        .handle
        .request(
            "nvim_command",
            vec![Value::from(format!("edit {open_str}"))],
        )
        .expect("open buffer");

    engine
        .handle
        .preview_buffer_window(&open_str, 4500, 1000, 1)
        .expect("issue preview request");
    let (loaded, lines) = reply(&rx, 1).expect("a reply for the open buffer");
    assert!(loaded, "the open buffer answers from memory");
    assert_eq!(lines.len(), 1000);
    assert_eq!(lines[0], "line 4500");
    assert_eq!(lines[500], "line 5000");

    let closed_str = closed.to_string_lossy().into_owned();
    engine
        .handle
        .preview_buffer_window(&closed_str, 4500, 1000, 2)
        .expect("issue preview request");
    let (loaded, lines) = reply(&rx, 2).expect("a reply for the closed file");
    assert!(!loaded, "a file no buffer holds falls back to disk");
    assert!(lines.is_empty(), "{} lines", lines.len());

    let _ = std::fs::remove_dir_all(&root);
}
