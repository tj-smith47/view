//! Live-nvim check that `PREVIEW_WINDOW_CHUNK` cuts each line of a loaded
//! buffer at `PREVIEW_LINE_BYTES` on a character boundary, and that a match
//! below lines holding megabytes still arrives in its window and is marked.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::mpsc;
use std::time::Instant;

use rmpv::Value;
use view_core::msg::Msg;
use view_core::native::picker::{
    PickerItem, PickerState, Source, PREVIEW_LINE_BYTES, PREVIEW_WINDOW_LINES,
};
use view_engine::process::{Engine, EngineConfig};

fn scratch_dir() -> std::path::PathBuf {
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
        .join(format!("preview-long-lines-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    root
}

fn reply(rx: &mpsc::Receiver<Msg>, generation: u64) -> (bool, Vec<String>) {
    let deadline = Instant::now() + common::rpc_deadline();
    let mut reply = None;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::PickerPreviewReply {
                generation: got,
                loaded,
                lines,
                ..
            }) if got == generation => {
                reply = Some((loaded, lines));
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    reply.expect("a PickerPreviewReply for the request")
}

#[test]
fn a_loaded_buffers_long_lines_are_cut_and_its_match_is_marked() {
    let root = scratch_dir();
    let name = "long-lines.jsonl";
    let path = root.join(name);
    let cap = usize::try_from(PREVIEW_LINE_BYTES).expect("fits");
    let text: String = (1..=2000)
        .map(|n| match n {
            1 => format!("{}€tail\n", "x".repeat(cap - 1)),
            1500 => format!("{:a<4096}\n", "MATCH 1500 "),
            n => format!("{:a<4096}\n", format!("line {n} ")),
        })
        .collect();
    std::fs::write(&path, text).expect("write fixture");

    let mut engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    let path_str = path.to_string_lossy().into_owned();
    engine
        .handle
        .request(
            "nvim_command",
            vec![Value::from(format!("edit {path_str}"))],
        )
        .expect("open the buffer");

    engine
        .handle
        .preview_buffer_window(&path_str, 1, 1, 1)
        .expect("issue preview request");
    let (loaded, lines) = reply(&rx, 1);
    assert!(loaded, "the buffer is open");
    assert_eq!(lines, vec!["x".repeat(cap - 1)], "€ straddles the cut");

    let mut state = PickerState::open(Source::LiveGrep { root: root.clone() });
    let gen = state.generation();
    state.apply_results(gen, vec![PickerItem::grep_match(name, 1500, "MATCH")]);
    let (preview_gen, wanted) = state.refresh_preview().expect("a selection");
    engine
        .handle
        .preview_buffer_window(
            &wanted,
            state.preview_first_line(),
            PREVIEW_WINDOW_LINES,
            preview_gen,
        )
        .expect("issue preview request");
    let (loaded, lines) = reply(&rx, preview_gen);
    assert!(loaded, "the buffer is open");
    assert!(lines.iter().all(|line| line.len() <= cap));
    state.apply_preview(preview_gen, lines);
    let view = state.view();
    let (rows, marked) = view.preview_window(30);
    assert!(rows[marked.expect("the match is marked")].starts_with("MATCH 1500 "));

    let _ = std::fs::remove_dir_all(&root);
}
