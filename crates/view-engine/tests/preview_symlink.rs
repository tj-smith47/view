//! Live-nvim proof that a symlinked candidate path still resolves to a
//! modified-but-unsaved buffer's in-memory content: `PREVIEW_CHUNK`
//! canonicalizes both sides of its name comparison (see
//! `view_engine::nvim_api`'s doc on the constant) specifically so a picker
//! candidate reached through a symlink is not silently treated as "no
//! buffer open", which would otherwise fall back to a stale on-disk read.
//! `#[cfg(unix)]`-gated: creating a symlink on Windows needs developer mode
//! or an elevated process, the same reason `stdin_relay_self_dup.rs` and
//! `shutdown.rs`'s unix-only cases are gated -- not a claim this crate's
//! path canonicalization is unix-specific, only that this particular proof
//! needs a filesystem feature Windows CI cannot grant unprompted.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::mpsc;
use std::time::Instant;

use rmpv::Value;
use view_core::msg::Msg;
use view_engine::process::{Engine, EngineConfig};

#[test]
fn a_symlinked_candidate_path_still_finds_its_modified_buffer() {
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
        .join(format!("preview-symlink-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    let real_path = root.join("real.txt");
    let link_path = root.join("link.txt");
    std::fs::write(&real_path, "original on disk\n").expect("write real file");
    std::os::unix::fs::symlink(&real_path, &link_path).expect("create symlink");

    let mut engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    // opens the buffer through the *symlink* path, then modifies it
    // in-memory without saving -- the load-bearing state this whole preview
    // feature exists to surface (see `docs/picker-preview-wire-capture.md`)
    let link_str = link_path.to_string_lossy().into_owned();
    engine
        .handle
        .request(
            "nvim_command",
            vec![Value::from(format!("edit {link_str}"))],
        )
        .expect("open buffer via symlink path");
    engine
        .handle
        .request(
            "nvim_command",
            vec![Value::from("normal! ggIunsaved via symlink ")],
        )
        .expect("modify buffer in place");

    // requests the preview through the *real, resolved* path -- literally
    // unequal to the symlink path nvim's buffer is named after, so this
    // only matches if `PREVIEW_CHUNK` canonicalizes both sides
    let real_str = real_path.to_string_lossy().into_owned();
    engine
        .handle
        .preview_buffer(&real_str, 1)
        .expect("issue preview request");

    let deadline = Instant::now() + common::rpc_deadline();
    let mut reply = None;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::PickerPreviewReply {
                generation: 1,
                loaded,
                lines,
                ..
            }) => {
                reply = Some((loaded, lines));
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    let (loaded, lines) = reply.expect("no PickerPreviewReply within 5s");
    assert!(
        loaded,
        "a candidate path reached through a symlink must still find its \
         open buffer, not answer loaded=false and fall back to disk"
    );
    assert!(
        lines.iter().any(|l| l.contains("unsaved via symlink")),
        "the preview must carry the buffer's in-memory, unsaved edit, not \
         the original on-disk content: {lines:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
