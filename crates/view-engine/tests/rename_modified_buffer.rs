//! Live-nvim proof that renaming a file through `RpcCall::RenameFile` never
//! orphans an open, modified buffer: the falsifiable check the file tree's
//! rename support exists to satisfy. A rename that only moved the file on
//! disk (`std::fs::rename`, off the loop, bypassing nvim entirely) would
//! leave the buffer still named after the now-nonexistent old path, with
//! the next `:w` from it recreating the file there instead of saving to the
//! new one -- this test fails loudly for exactly that regression.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::mpsc;
use std::time::Instant;

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
    let deadline = Instant::now() + common::rpc_deadline();
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

/// The scenario `RENAME_CHUNK`'s snapshot-before-rename fix exists for.
///
/// Reaching it live took investigation beyond the ordinary "open a file
/// through a symlinked directory" case this test started as: the pinned
/// engine turns out to eagerly and *permanently* resolve a buffer's name to
/// its realpath the moment every path component exists, whether the buffer
/// is created via `:edit` or `nvim_buf_set_name` -- confirmed live, and
/// unaffected by the underlying file later being deleted or moved (the name
/// is stored once, never re-derived on a later `nvim_buf_get_name` call).
/// That eagerness means a buffer opened the ordinary way, through an
/// already-existing symlinked directory, never actually carries an
/// unresolved symlink component for `RENAME_CHUNK`'s bug to lose -- both
/// the pre-fix and post-fix chunk resolve to the same already-canonical
/// string regardless of which side of the rename the canon runs on.
///
/// The gap is reachable only when the buffer is named for a path *before*
/// its symlinked parent directory exists: `vim.uv.fs_realpath` cannot
/// resolve a symlink component that is not there yet, so nvim falls back to
/// storing the literal, unresolved path -- and never revisits that once the
/// symlink is later created. That is exactly the state this test builds:
/// the buffer is named while `link` does not exist, the symlink is created
/// afterward, and only then does the rename run. Pre-fix, `canon()` on the
/// buffer's still-literal name is computed AFTER the rename, when the old
/// path no longer resolves to anything on disk (the file has moved), so it
/// falls back to the same unresolved literal string -- which never equals
/// `wanted` (resolved from `old_path` while the file still existed) and the
/// retarget silently skips this buffer. Snapshotting the canon before the
/// rename runs, while the literal old path still resolves through the
/// symlink to the real file, closes that gap: reverting to a post-rename
/// canon computation must make this test fail.
#[cfg(unix)]
#[test]
fn a_rename_through_a_symlinked_directory_still_follows_the_open_buffer() {
    // canonicalize immediately: `scratch_root` builds this from
    // `CARGO_MANIFEST_DIR`, which carries literal ".." components, and
    // resolving realpath on a ".."-laden path lands on a live kernel
    // dentry-cache race right after a same-directory rename (confirmed by
    // isolating the variable live: identical steps with a clean, dot-dot-free
    // root never mis-resolve, while the raw CARGO_MANIFEST_DIR-derived root
    // does, nondeterministically) -- a real environment hazard unrelated to
    // the symlink-aliasing bug this test exists to catch, so it must be
    // eliminated for the test to be a reliable disconfirm.
    let root = std::fs::canonicalize(scratch_root("symlink")).expect("canonicalize scratch root");
    let real_dir = root.join("real");
    std::fs::create_dir_all(&real_dir).expect("create real dir");
    let link_dir = root.join("link"); // deliberately not created yet
    let old_path = link_dir.join("old.txt");
    let new_path = link_dir.join("new.txt");
    std::fs::write(real_dir.join("old.txt"), "line one\n").expect("write original file");

    let mut engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    let old_str = old_path.to_string_lossy().into_owned();
    engine
        .handle
        .request("nvim_command", vec![Value::from("enew")])
        .expect("create a scratch buffer");
    engine
        .handle
        .request(
            "nvim_buf_set_name",
            vec![Value::from(0), Value::from(old_str.clone())],
        )
        .expect("name the buffer before its symlinked parent exists");
    engine
        .handle
        .request(
            "nvim_buf_set_lines",
            vec![
                Value::from(0),
                Value::from(0),
                Value::from(-1),
                Value::from(false),
                Value::Array(vec![
                    Value::from("line one"),
                    Value::from("unsaved second line"),
                ]),
            ],
        )
        .expect("seed unsaved buffer content");

    let stored_name = engine
        .handle
        .request("nvim_buf_get_name", vec![Value::from(0)])
        .expect("read buffer name before the symlink exists");
    assert_eq!(
        stored_name.as_str(),
        Some(old_str.as_str()),
        "the buffer name must still be the literal, unresolved symlinked path \
         while its parent directory does not exist yet -- if this assertion \
         fails, the pinned engine's own resolution behavior has changed and \
         this test no longer exercises the gap RENAME_CHUNK's fix closes"
    );

    std::os::unix::fs::symlink(&real_dir, &link_dir).expect("create the symlinked directory");
    assert!(
        old_path.exists(),
        "the symlinked path must now resolve to the real file"
    );

    let new_str = new_path.to_string_lossy().into_owned();
    engine
        .handle
        .rename_file(&old_str, &new_str, 42)
        .expect("issue rename request");

    let ok = wait_for_rename_reply(&rx, 42).expect("no TreeRenameReply within 5s");
    assert!(
        ok,
        "renaming through a symlinked directory onto a fresh path must succeed"
    );

    let buf_name = engine
        .handle
        .request("nvim_buf_get_name", vec![Value::from(0)])
        .expect("read buffer name after rename");
    let buf_name = buf_name
        .as_str()
        .expect("buffer name is a string")
        .to_owned();
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    assert_eq!(
        canon(std::path::Path::new(&buf_name)),
        canon(&new_path),
        "the buffer must follow the rename onto the new, symlink-aliased path -- a \
         retarget whose canon is computed from a post-rename buffer name (the \
         pre-fix ordering) can no longer resolve the now-renamed-away old path and \
         silently leaves this buffer orphaned instead"
    );

    let after_modified = engine
        .handle
        .request("nvim_eval", vec![Value::from("&modified")])
        .expect("read modified flag after rename");
    assert_eq!(
        after_modified,
        Value::from(1),
        "the modified flag must survive a rename through a symlinked directory too"
    );

    assert!(
        !real_dir.join("old.txt").exists(),
        "the real file must no longer exist at its old name"
    );
    assert!(
        real_dir.join("new.txt").exists(),
        "the real file must exist at its new name"
    );

    let _ = std::fs::remove_dir_all(&root);
}
