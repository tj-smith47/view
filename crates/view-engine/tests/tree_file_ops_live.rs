//! Live-nvim proof that the tree's create/rename/delete keys are wired
//! end-to-end, not just plumbed as far as an untriggered `RpcCall`/`Effect`
//! pair: a prior gap here left every test either constructing
//! `Msg::Tree*Reply` variants by hand or calling an `EngineHandle` method
//! directly, with no production key ever shown to reach any of it. Each
//! test here presses the real key through [`update`], executes the
//! [`view_core::msg::RpcCall`] it returns
//! against a real spawned nvim exactly the way the terminal frontend's
//! own executor would, answers the resulting blocked prompt with real
//! keystrokes, and feeds the real reply back through [`update`] -- proving
//! the whole loop, both directions, for one op per test.
//!
//! The one link this crate cannot supply itself is the executor's own
//! `Effect::TreeCreateFile`/`Effect::TreeDeleteFile` filesystem action:
//! `view-engine` has no dependency on the `view` bin crate that hosts the
//! executor (`scripts/audit-deps.sh` forbids the reverse), so that single
//! step is reproduced inline here with the executor's own documented
//! contract (`OpenOptions::create_new` refusing to overwrite; `fs::remove_file`)
//! rather than skipped -- the write's *outcome* still crosses back through
//! `update()` as a real `Msg::Tree*FileResult`, closing the loop the same
//! way a live `RpcCall` reply does. The write itself is covered against
//! truncation/overwrite regressions by `view`'s own `runtime.rs` executor
//! tests; duplicating that coverage here would test the same line twice
//! without the real-key path this file exists to prove.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use view_core::model::Model;
use view_core::msg::{DeleteConfirmOutcome, Effect, Key, Msg, RpcCall};
use view_core::native::tree::TreeEntry;
use view_core::update::update;
use view_engine::process::{Engine, EngineConfig};

fn scratch_root(nonce_suffix: &str) -> PathBuf {
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
        .join(format!("tree-file-ops-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    root
}

/// Opens a tree overlay on `root` through the real `FeatureInvoke` toggle
/// path (the same message the registered `<leader>...` mapping sends), the
/// production route into the overlay every "a"/"r"/"d" test below then
/// presses its key against.
fn open_tree(root: &std::path::Path) -> Model {
    let mut model = Model::new().with_cwd(root.to_path_buf());
    let _ = update(
        &mut model,
        Msg::FeatureInvoke {
            feature: "tree".to_string(),
            verb: "toggle".to_string(),
        },
    );
    model
}

/// Waits for the next `Msg` matching `pred` on `rx`, discarding everything
/// else (redraw traffic, unrelated replies) until it arrives or `deadline`
/// passes.
fn wait_for<T>(
    rx: &mpsc::Receiver<Msg>,
    deadline: Instant,
    mut pred: impl FnMut(&Msg) -> Option<T>,
) -> Option<T> {
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(msg) => {
                if let Some(v) = pred(&msg) {
                    return Some(v);
                }
            }
            Err(_) => break,
        }
    }
    None
}

#[test]
fn pressing_a_creates_a_file_through_the_real_prompt_and_rescans() {
    let root = scratch_root("create");
    let mut model = open_tree(&root);

    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: "a".into(),
        }),
    );
    let generation = match effects.as_slice() {
        [Effect::Rpc(RpcCall::TreeCreatePrompt { generation })] => *generation,
        other => panic!("\"a\" on an open tree must issue exactly one TreeCreatePrompt: {other:?}"),
    };

    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    engine
        .handle
        .tree_create_prompt(generation)
        .expect("issue the real create prompt RPC");

    // the chunk blocks nvim's main loop inside vim.fn.input(); nothing
    // replies until these keystrokes answer it, matching the probe file's
    // own confirmed wire behavior
    for ch in "new-file.txt".chars() {
        engine.handle.input(&ch.to_string()).unwrap();
    }
    engine.handle.input("<CR>").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let (reply_gen, name) = wait_for(&rx, deadline, |msg| match msg {
        Msg::TreeCreatePromptReply { generation, name } => Some((*generation, name.clone())),
        _ => None,
    })
    .expect("no TreeCreatePromptReply within 5s");
    assert_eq!(reply_gen, generation);
    assert_eq!(name.as_deref(), Some("new-file.txt"));

    let effects = update(
        &mut model,
        Msg::TreeCreatePromptReply {
            generation: reply_gen,
            name,
        },
    );
    let (path, create_generation) = match effects.as_slice() {
        [Effect::TreeCreateFile { path, generation }] => (path.clone(), *generation),
        other => panic!("a real filename reply must issue exactly one TreeCreateFile: {other:?}"),
    };
    assert_eq!(path, root.join("new-file.txt"));

    // reproduces the executor's own Effect::TreeCreateFile contract
    // (OpenOptions::create_new, never a truncating write) rather than
    // skipping the write: view-engine cannot depend on the executor that
    // normally performs it (see this file's module doc)
    let ok = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .is_ok();
    assert!(ok, "the target must not already exist");
    assert!(path.exists(), "the file must exist on disk after create");

    let effects = update(
        &mut model,
        Msg::TreeCreateFileResult {
            generation: create_generation,
            ok,
        },
    );
    assert!(
        matches!(effects.as_slice(), [Effect::TreeScan { .. }]),
        "a successful create must trigger an unconditional rescan: {effects:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn pressing_r_renames_a_file_through_the_real_prompt_and_the_real_rename_reply() {
    let root = scratch_root("rename");
    let old_path = root.join("old.txt");
    std::fs::write(&old_path, "content\n").unwrap();

    let mut model = open_tree(&root);
    let tree = model.tree_mut().expect("tree overlay must be open");
    let scan_generation = tree.generation();
    tree.apply_scan(
        scan_generation,
        vec![TreeEntry::new(PathBuf::from("old.txt"), false, 0)],
    );

    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: "r".into(),
        }),
    );
    let (generation, old_path_str, current_name) = match effects.as_slice() {
        [Effect::Rpc(RpcCall::TreeRenamePrompt {
            generation,
            old_path,
            current_name,
        })] => (*generation, old_path.clone(), current_name.clone()),
        other => {
            panic!("\"r\" on a selected file must issue exactly one TreeRenamePrompt: {other:?}")
        }
    };
    assert_eq!(old_path_str, old_path.to_string_lossy());
    assert_eq!(current_name, "old.txt");

    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    engine
        .handle
        .tree_rename_prompt(&old_path_str, &current_name, generation)
        .expect("issue the real rename prompt RPC");

    // the prompt is prefilled with current_name (vim.fn.input's `default`);
    // clearing it first proves the wire path actually carries typed input
    // rather than the reply merely echoing the prefill back unexamined
    for _ in 0..current_name.chars().count() {
        engine.handle.input("<BS>").unwrap();
    }
    for ch in "new.txt".chars() {
        engine.handle.input(&ch.to_string()).unwrap();
    }
    engine.handle.input("<CR>").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let (reply_gen, reply_old_path, name) = wait_for(&rx, deadline, |msg| match msg {
        Msg::TreeRenamePromptReply {
            generation,
            old_path,
            name,
        } => Some((*generation, old_path.clone(), name.clone())),
        _ => None,
    })
    .expect("no TreeRenamePromptReply within 5s");
    assert_eq!(reply_gen, generation);
    assert_eq!(reply_old_path, old_path_str);
    assert_eq!(name.as_deref(), Some("new.txt"));

    let effects = update(
        &mut model,
        Msg::TreeRenamePromptReply {
            generation: reply_gen,
            old_path: reply_old_path,
            name,
        },
    );
    let (rename_old, rename_new, rename_generation) = match effects.as_slice() {
        [Effect::Rpc(RpcCall::RenameFile {
            old_path,
            new_path,
            generation,
        })] => (old_path.clone(), new_path.clone(), *generation),
        other => panic!("a real new-name reply must issue exactly one RenameFile: {other:?}"),
    };
    let new_path = root.join("new.txt");
    assert_eq!(rename_new, new_path.to_string_lossy());

    // the real RpcCall::RenameFile, driven through the production
    // EngineHandle wrapper -- proving TreeRenameReply live rather than
    // synthesized by constructing the Msg variant by hand
    engine
        .handle
        .rename_file(&rename_old, &rename_new, rename_generation)
        .expect("issue the real rename RPC");

    let rename_deadline = Instant::now() + Duration::from_secs(5);
    let ok = wait_for(&rx, rename_deadline, |msg| match msg {
        Msg::TreeRenameReply { generation, ok } if *generation == rename_generation => Some(*ok),
        _ => None,
    })
    .expect("no TreeRenameReply within 5s");
    assert!(ok, "renaming onto a fresh path must succeed");

    let effects = update(
        &mut model,
        Msg::TreeRenameReply {
            generation: rename_generation,
            ok,
        },
    );
    assert!(
        matches!(effects.as_slice(), [Effect::TreeScan { .. }]),
        "a successful rename must trigger an unconditional rescan: {effects:?}"
    );
    assert!(!old_path.exists(), "the old path must be gone");
    assert!(new_path.exists(), "the new path must exist");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn pressing_d_deletes_a_file_through_the_real_confirm_and_rescans() {
    let root = scratch_root("delete");
    let target = root.join("doomed.txt");
    std::fs::write(&target, "bye\n").unwrap();

    let mut model = open_tree(&root);
    let tree = model.tree_mut().expect("tree overlay must be open");
    let scan_generation = tree.generation();
    tree.apply_scan(
        scan_generation,
        vec![TreeEntry::new(PathBuf::from("doomed.txt"), false, 0)],
    );

    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: "d".into(),
        }),
    );
    let (generation, path_str) = match effects.as_slice() {
        [Effect::Rpc(RpcCall::TreeDeleteConfirm { generation, path })] => {
            (*generation, path.clone())
        }
        other => {
            panic!("\"d\" on a selected file must issue exactly one TreeDeleteConfirm: {other:?}")
        }
    };
    assert_eq!(path_str, target.to_string_lossy());

    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    engine
        .handle
        .tree_delete_confirm(&path_str, generation)
        .expect("issue the real delete confirm RPC");

    // vim.fn.confirm's first choice ("&Yes") answers to plain "y"
    engine.handle.input("y").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let (reply_gen, reply_path, outcome) = wait_for(&rx, deadline, |msg| match msg {
        Msg::TreeDeleteConfirmReply {
            generation,
            path,
            outcome,
        } => Some((*generation, path.clone(), *outcome)),
        _ => None,
    })
    .expect("no TreeDeleteConfirmReply within 5s");
    assert_eq!(reply_gen, generation);
    assert_eq!(reply_path, path_str);
    assert_eq!(
        outcome,
        DeleteConfirmOutcome::Confirmed,
        "answering Yes on a file with no loaded buffer must confirm the delete"
    );

    let effects = update(
        &mut model,
        Msg::TreeDeleteConfirmReply {
            generation: reply_gen,
            path: reply_path,
            outcome,
        },
    );
    let (delete_path, delete_generation) = match effects.as_slice() {
        [Effect::TreeDeleteFile { path, generation }] => (path.clone(), *generation),
        other => panic!("a confirmed delete must issue exactly one TreeDeleteFile: {other:?}"),
    };
    assert_eq!(delete_path, target);

    // reproduces the executor's own Effect::TreeDeleteFile contract: same
    // rationale as pressing_a's inline write above
    let ok = std::fs::remove_file(&delete_path).is_ok();
    assert!(ok, "the target must have existed to delete");
    assert!(!delete_path.exists(), "the file must be gone after delete");

    let effects = update(
        &mut model,
        Msg::TreeDeleteFileResult {
            generation: delete_generation,
            ok,
        },
    );
    assert!(
        matches!(effects.as_slice(), [Effect::TreeScan { .. }]),
        "a successful delete must trigger an unconditional rescan: {effects:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The delete-with-open-buffer proof `TREE_DELETE_CONFIRM_CHUNK`'s
/// `bufloaded` check exists for: a file with a loaded, unsaved-modified
/// buffer must never be deleted out from under it, even on an explicit
/// "Yes" -- the chunk refuses before it ever offers the confirm prompt at
/// all, so this test sends no answering keystroke and would hang on a
/// missing reply if that refusal did not happen engine-side.
#[test]
fn pressing_d_on_a_file_with_a_loaded_modified_buffer_refuses_the_delete_and_records_a_notice() {
    let root = scratch_root("delete-buffer-open");
    let target = root.join("open.txt");
    std::fs::write(&target, "line one\n").unwrap();

    let mut model = open_tree(&root);
    let tree = model.tree_mut().expect("tree overlay must be open");
    let scan_generation = tree.generation();
    tree.apply_scan(
        scan_generation,
        vec![TreeEntry::new(PathBuf::from("open.txt"), false, 0)],
    );

    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: "d".into(),
        }),
    );
    let (generation, path_str) = match effects.as_slice() {
        [Effect::Rpc(RpcCall::TreeDeleteConfirm { generation, path })] => {
            (*generation, path.clone())
        }
        other => {
            panic!("\"d\" on a selected file must issue exactly one TreeDeleteConfirm: {other:?}")
        }
    };
    assert_eq!(path_str, target.to_string_lossy());

    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);

    // load the buffer and dirty it, unsaved -- the exact case the
    // bufloaded check exists to catch
    engine
        .handle
        .request(
            "nvim_command",
            vec![rmpv::Value::from(format!("edit {path_str}"))],
        )
        .expect("open buffer");
    engine
        .handle
        .request(
            "nvim_command",
            vec![rmpv::Value::from("normal! Gounsaved second line")],
        )
        .expect("modify buffer without saving");
    let before_modified = engine
        .handle
        .request("nvim_eval", vec![rmpv::Value::from("&modified")])
        .expect("read modified flag before delete confirm");
    assert_eq!(
        before_modified,
        rmpv::Value::from(1),
        "the buffer must genuinely be modified before the refusal this test proves"
    );

    engine
        .handle
        .tree_delete_confirm(&path_str, generation)
        .expect("issue the real delete confirm RPC");

    let deadline = Instant::now() + Duration::from_secs(5);
    let (reply_gen, reply_path, outcome) = wait_for(&rx, deadline, |msg| match msg {
        Msg::TreeDeleteConfirmReply {
            generation,
            path,
            outcome,
        } => Some((*generation, path.clone(), *outcome)),
        _ => None,
    })
    .expect(
        "no TreeDeleteConfirmReply within 5s -- the chunk must resolve without \
         waiting on a confirm answer when it refuses outright",
    );
    assert_eq!(reply_gen, generation);
    assert_eq!(reply_path, path_str);
    assert_eq!(
        outcome,
        DeleteConfirmOutcome::BufferOpen,
        "a loaded buffer on the target path must refuse the delete outright"
    );

    let effects = update(
        &mut model,
        Msg::TreeDeleteConfirmReply {
            generation: reply_gen,
            path: reply_path,
            outcome,
        },
    );
    assert!(
        matches!(effects.as_slice(), [Effect::ScheduleToastExpiry { .. }]),
        "a refused delete must surface exactly one notice through the choke \
         point, no TreeDeleteFile: {effects:?}"
    );
    let notice = model
        .engine
        .messages
        .entries
        .last()
        .expect("the refusal must reach the message surface");
    assert_eq!(
        notice.content,
        vec![(0, "view: buffer open -- close it first".to_string())]
    );

    assert!(target.exists(), "the file must survive a refused delete");
    let after_modified = engine
        .handle
        .request("nvim_eval", vec![rmpv::Value::from("&modified")])
        .expect("read modified flag after the refused delete");
    assert_eq!(
        after_modified,
        rmpv::Value::from(1),
        "the buffer's unsaved edit must survive a refused delete untouched"
    );

    let _ = std::fs::remove_dir_all(&root);
}
