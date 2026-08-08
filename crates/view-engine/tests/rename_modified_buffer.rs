//! Live-nvim proof that renaming a file through `RpcCall::RenameFile` never
//! orphans an open, modified buffer: the falsifiable check the file tree's
//! rename support exists to satisfy. A rename that only moved the file on
//! disk (`std::fs::rename`, off the loop, bypassing nvim entirely) would
//! leave the buffer still named after the now-nonexistent old path, with
//! the next `:w` from it recreating the file there instead of saving to the
//! new one -- this test fails loudly for exactly that regression.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use rmpv::Value;
use view_core::msg::Msg;
use view_engine::process::{Engine, EngineConfig};

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
        .join(format!("tree-rename-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    root
}

fn wait_for_rename_reply(rx: &mpsc::Receiver<Msg>, generation: u64) -> Option<bool> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Msg::TreeRenameReply { generation: g, ok }) if g == generation => {
                return Some(ok);
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    None
}

#[test]
fn a_rename_with_an_unsaved_modified_buffer_follows_it_and_keeps_the_modified_flag() {
    let root = scratch_root("modified");
    let old_path = root.join("old.txt");
    let new_path = root.join("new.txt");
    std::fs::write(&old_path, "line one\n").expect("write original file");

    let mut engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let old_str = old_path.to_string_lossy().into_owned();
    engine
        .handle
        .request("nvim_command", vec![Value::from(format!("edit {old_str}"))])
        .expect("open buffer");
    engine
        .handle
        .request(
            "nvim_command",
            vec![Value::from("normal! Gounsaved second line")],
        )
        .expect("modify buffer without saving");

    let before_modified = engine
        .handle
        .request("nvim_eval", vec![Value::from("&modified")])
        .expect("read modified flag before rename");
    assert_eq!(
        before_modified,
        Value::from(1),
        "the buffer must genuinely be modified before the rename this test proves"
    );

    let new_str = new_path.to_string_lossy().into_owned();
    engine
        .handle
        .rename_file(&old_str, &new_str, 1)
        .expect("issue rename request");

    let ok = wait_for_rename_reply(&rx, 1).expect("no TreeRenameReply within 5s");
    assert!(ok, "renaming onto a fresh path must succeed");

    let buf_name = engine
        .handle
        .request("nvim_buf_get_name", vec![Value::from(0)])
        .expect("read buffer name after rename");
    let buf_name = buf_name
        .as_str()
        .expect("buffer name is a string")
        .to_owned();
    // canonicalize both sides: on some hosts /tmp is itself a symlink
    // (e.g. to /private/tmp), and nvim's buffer name is realpath-resolved
    // by nvim_buf_set_name's own bookkeeping while `new_path` here is not
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    assert_eq!(
        canon(std::path::Path::new(&buf_name)),
        canon(&new_path),
        "the buffer must follow the rename onto the new path, not still \
         name the old, now-nonexistent one"
    );

    let after_modified = engine
        .handle
        .request("nvim_eval", vec![Value::from("&modified")])
        .expect("read modified flag after rename");
    assert_eq!(
        after_modified,
        Value::from(1),
        "the modified flag must survive the rename -- an RPC-routed rename \
         that lost it would silently make the unsaved edit look saved"
    );

    let lines = engine
        .handle
        .request(
            "nvim_buf_get_lines",
            vec![
                Value::from(0),
                Value::from(0),
                Value::from(-1),
                Value::from(false),
            ],
        )
        .expect("read buffer content after rename");
    let lines: Vec<String> = lines
        .as_array()
        .expect("lines reply is an array")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("unsaved second line")),
        "the unsaved edit must survive the rename verbatim: {lines:?}"
    );

    assert!(
        !old_path.exists(),
        "the old path must no longer exist on disk after a successful rename"
    );
    assert!(
        new_path.exists(),
        "the new path must exist on disk after a successful rename"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_rename_onto_an_existing_destination_is_refused_and_orphans_nothing() {
    let root = scratch_root("collision");
    let old_path = root.join("a.txt");
    let existing_path = root.join("b.txt");
    std::fs::write(&old_path, "aaa\n").expect("write source file");
    std::fs::write(&existing_path, "bbb\n").expect("write existing destination file");

    let mut engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let old_str = old_path.to_string_lossy().into_owned();
    let existing_str = existing_path.to_string_lossy().into_owned();
    engine
        .handle
        .rename_file(&old_str, &existing_str, 7)
        .expect("issue rename request");

    let ok = wait_for_rename_reply(&rx, 7).expect("no TreeRenameReply within 5s");
    assert!(
        !ok,
        "a rename onto an existing destination must be refused, never \
         silently overwrite it"
    );
    assert!(old_path.exists(), "the source file must be left in place");
    assert_eq!(
        std::fs::read_to_string(&existing_path).expect("read destination"),
        "bbb\n",
        "the destination file's original content must be untouched"
    );

    let _ = std::fs::remove_dir_all(&root);
}
