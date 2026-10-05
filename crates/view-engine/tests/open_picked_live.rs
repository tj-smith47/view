//! Live-nvim check of `EngineHandle::open_picked`: each window a picker key
//! names, a grep match's line, listed buffers by handle, the window a
//! sidebar was entered from, and the message an open that cannot happen
//! leaves.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::mpsc;

use rmpv::Value;
use view_core::events::WinHandle;
use view_core::msg::{OpenIn, WinSplit};
use view_core::native::geometry::NativeSurface;
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
        engine.handle.open_picked(&picked, how, &[], 0).unwrap();
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
        .open_picked(&picked, OpenIn::Current, &[], 0)
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
        engine.handle.open_picked(&picked, how, &[], 0).unwrap();
        assert_eq!(current_buffer(&engine), second, "{how:?}: the second");
        let (_, _, wins, tabpages) = state(&engine);
        assert_eq!((wins, tabpages), (windows, tabs), "{how:?}");

        let picked = Picked::Buffer { handle: listed };
        engine
            .handle
            .open_picked(&picked, OpenIn::Current, &[], 0)
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
        .open_picked(&deleted, OpenIn::Current, &[], 1)
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
        .open_picked(&wiped, OpenIn::Vertical, &[], 2)
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
        .open_picked(&refused, OpenIn::Current, &[], 3)
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
    let claimed = [current_window(&engine)];
    engine
        .handle
        .open_picked(&picked, OpenIn::Current, &claimed, 0)
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

/// A sidebar view opened on `split`, entered.
fn sidebar(engine: &Engine, surface: NativeSurface, split: WinSplit) -> WinHandle {
    engine
        .handle
        .open_native_window_sync(surface, split, 30, true)
        .unwrap()
        .expect("the sidebar opened")
}

/// The window the cursor is in.
fn current_window(engine: &Engine) -> WinHandle {
    WinHandle(
        engine
            .handle
            .request("nvim_eval", vec![Value::from("win_getid()")])
            .unwrap()
            .as_u64()
            .unwrap(),
    )
}

/// Whether `win` still shows the scratch buffer view opened it with.
fn keeps_its_buffer(engine: &Engine, win: WinHandle, surface: NativeSurface) -> bool {
    let filetype = engine
        .handle
        .request(
            "nvim_eval",
            vec![Value::from(format!(
                "getbufvar(winbufnr({}), '&filetype')",
                win.0
            ))],
        )
        .unwrap();
    filetype.as_str() == Some(&format!("view-{}", surface.id()))
}

/// Chosen from a sidebar entered from another sidebar, a file opens in the
/// ordinary window, whether view's claims or nvim's own record of the
/// sidebars names them.
#[test]
fn a_file_chosen_from_a_sidebar_entered_from_another_opens_in_the_ordinary_window() {
    let root = scratch_root("two-sidebars");
    let path = root.join("chosen.txt");
    std::fs::write(&path, "chosen\n").unwrap();
    for forget_record in [false, true] {
        let (engine, rx) = spawn_routed();
        let ordinary = current_window(&engine);
        let agent = sidebar(&engine, NativeSurface::Agent, WinSplit::Right);
        let tree = sidebar(&engine, NativeSurface::Tree, WinSplit::Left);
        let claimed = if forget_record {
            engine
                .handle
                .command("let g:view_native_windows = {}")
                .unwrap();
            vec![agent, tree]
        } else {
            Vec::new()
        };
        let picked = Picked::File {
            path: path.to_string_lossy().into_owned(),
            line: None,
        };
        engine
            .handle
            .open_picked(&picked, OpenIn::Current, &claimed, 0)
            .unwrap();
        assert!(answered(&engine, &rx, 0), "the open was never answered");
        assert_eq!(current_window(&engine), ordinary, "{forget_record}");
        assert!(same_file(&state(&engine).0, &path), "{forget_record}");
        assert!(keeps_its_buffer(&engine, agent, NativeSurface::Agent));
        assert!(keeps_its_buffer(&engine, tree, NativeSurface::Tree));
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// With every ordinary window closed, a file or a buffer chosen from a
/// sidebar opens in a new window beside it, and each sidebar keeps its
/// buffer.
#[test]
fn with_only_sidebars_on_screen_a_choice_opens_in_a_new_window() {
    let root = scratch_root("only-sidebars");
    let path = root.join("chosen.txt");
    std::fs::write(&path, "chosen\n").unwrap();
    let file = Picked::File {
        path: path.to_string_lossy().into_owned(),
        line: None,
    };
    for (picked, how) in [
        (None, OpenIn::Current),
        (None, OpenIn::Horizontal),
        (Some(()), OpenIn::Current),
    ] {
        let (engine, rx) = spawn_routed();
        let ordinary = current_window(&engine);
        engine.handle.command("call setline(1, 'kept')").unwrap();
        let kept = current_buffer(&engine);
        let agent = sidebar(&engine, NativeSurface::Agent, WinSplit::Right);
        let tree = sidebar(&engine, NativeSurface::Tree, WinSplit::Left);
        engine
            .handle
            .command(&format!("call nvim_win_close({}, v:true)", ordinary.0))
            .unwrap();
        let target = match picked {
            None => file.clone(),
            Some(()) => Picked::Buffer { handle: kept },
        };
        engine.handle.open_picked(&target, how, &[], 0).unwrap();
        assert!(answered(&engine, &rx, 0), "{how:?}: never answered");
        let (_, _, wins, _) = state(&engine);
        assert_eq!(wins, 3, "{target:?} {how:?}: one new window");
        let now = current_window(&engine);
        assert!(now != agent && now != tree, "{target:?} {how:?}");
        match picked {
            None => assert!(same_file(&state(&engine).0, &path), "{how:?}"),
            Some(()) => assert_eq!(current_buffer(&engine), kept),
        }
        assert!(keeps_its_buffer(&engine, agent, NativeSurface::Agent));
        assert!(keeps_its_buffer(&engine, tree, NativeSurface::Tree));
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// The name of the buffer `win` shows.
fn buffer_in(engine: &Engine, win: WinHandle) -> String {
    engine
        .handle
        .request(
            "nvim_eval",
            vec![Value::from(format!("bufname(winbufnr({}))", win.0))],
        )
        .unwrap()
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// Builds, left of and above the ordinary window the engine starts in, a
/// `nofile` sidebar a plugin could have docked, a help window, a terminal,
/// a `winfixbuf` window and a `nofile` window view claims, entering each in
/// turn, so every one of them comes before the ordinary window in layout
/// order and has been entered after it. Returns the ordinary window, the
/// four special ones and the claimed one, the cursor in the claimed one.
fn special_windows(engine: &Engine) -> (WinHandle, Vec<WinHandle>, WinHandle) {
    let ordinary = current_window(engine);
    engine.handle.command("file ordinary").unwrap();
    let mut special = Vec::new();
    for build in [
        "topleft vnew | setlocal buftype=nofile winfixwidth | file plugintree",
        "topleft help",
        "topleft new | terminal",
        "topleft vnew | file fixed | setlocal winfixbuf",
        "topleft vnew | setlocal buftype=nofile | file claimed",
    ] {
        engine.handle.command(build).unwrap();
        special.push(current_window(engine));
    }
    let claimed = special.pop().unwrap();
    (ordinary, special, claimed)
}

/// Chosen from a window view claims, a file opens in the one ordinary
/// window, whatever comes before it in the layout: a sidebar a plugin
/// docked, a help window, a terminal and a `winfixbuf` window each keep
/// their buffer.
#[test]
fn a_file_chosen_from_a_claimed_window_skips_every_special_window() {
    let root = scratch_root("special");
    let path = root.join("chosen.txt");
    std::fs::write(&path, "chosen\n").unwrap();
    let (engine, rx) = spawn_routed();
    let (ordinary, special, claimed) = special_windows(&engine);
    let before: Vec<String> = special.iter().map(|w| buffer_in(&engine, *w)).collect();
    let picked = Picked::File {
        path: path.to_string_lossy().into_owned(),
        line: None,
    };
    engine
        .handle
        .open_picked(&picked, OpenIn::Current, &[claimed], 0)
        .unwrap();
    assert!(answered(&engine, &rx, 0), "the open was never answered");
    assert_eq!(current_window(&engine), ordinary, "{:?}", state(&engine));
    assert!(same_file(&state(&engine).0, &path));
    let after: Vec<String> = special.iter().map(|w| buffer_in(&engine, *w)).collect();
    assert_eq!(after, before, "a special window lost its buffer");
    assert_eq!(buffer_in(&engine, claimed), "claimed");
    let _ = std::fs::remove_dir_all(&root);
}

/// With no ordinary window left, a file chosen from a claimed window opens
/// in a new window, and every other window keeps its buffer.
#[test]
fn with_only_special_windows_left_a_choice_opens_in_a_new_window() {
    let root = scratch_root("special-only");
    let path = root.join("chosen.txt");
    std::fs::write(&path, "chosen\n").unwrap();
    let (engine, rx) = spawn_routed();
    let (ordinary, special, claimed) = special_windows(&engine);
    engine
        .handle
        .command(&format!("call nvim_win_close({}, v:true)", ordinary.0))
        .unwrap();
    let windows = state(&engine).2;
    let before: Vec<String> = special.iter().map(|w| buffer_in(&engine, *w)).collect();
    let picked = Picked::File {
        path: path.to_string_lossy().into_owned(),
        line: None,
    };
    engine
        .handle
        .open_picked(&picked, OpenIn::Current, &[claimed], 0)
        .unwrap();
    assert!(answered(&engine, &rx, 0), "the open was never answered");
    let (current, _, now, _) = state(&engine);
    assert_eq!(now, windows + 1, "one new window");
    assert!(same_file(&current, &path), "{current}");
    let opened = current_window(&engine);
    assert!(!special.contains(&opened) && opened != claimed);
    let after: Vec<String> = special.iter().map(|w| buffer_in(&engine, *w)).collect();
    assert_eq!(after, before, "a special window lost its buffer");
    assert_eq!(buffer_in(&engine, claimed), "claimed");
    let _ = std::fs::remove_dir_all(&root);
}

/// Chosen while the cursor is in a float view does not claim, a file opens
/// in that float, as `:edit` there would.
#[test]
fn a_file_chosen_from_an_unclaimed_float_opens_in_the_float() {
    let root = scratch_root("float");
    let path = root.join("chosen.txt");
    std::fs::write(&path, "chosen\n").unwrap();
    let (engine, rx) = spawn_routed();
    let ordinary = current_window(&engine);
    engine
        .handle
        .command(
            "call nvim_open_win(nvim_create_buf(v:true, v:false), v:true, \
             {'relative': 'editor', 'row': 2, 'col': 2, \
             'width': 20, 'height': 5})",
        )
        .unwrap();
    let float = current_window(&engine);
    assert_ne!(float, ordinary);
    let picked = Picked::File {
        path: path.to_string_lossy().into_owned(),
        line: None,
    };
    engine
        .handle
        .open_picked(&picked, OpenIn::Current, &[], 0)
        .unwrap();
    assert!(answered(&engine, &rx, 0), "the open was never answered");
    assert_eq!(current_window(&engine), float);
    assert!(same_file(&state(&engine).0, &path));
    let _ = std::fs::remove_dir_all(&root);
}

/// Chosen from a sidebar entered from another sidebar, a file opens in the
/// ordinary window the cursor was in last, which comes after another
/// ordinary window in the layout.
#[test]
fn a_file_chosen_from_a_sidebar_opens_in_the_window_last_edited_in() {
    let root = scratch_root("recent");
    let path = root.join("chosen.txt");
    std::fs::write(&path, "chosen\n").unwrap();
    let (engine, rx) = spawn_routed();
    let last = current_window(&engine);
    engine.handle.command("vsplit | wincmd l").unwrap();
    assert_eq!(
        current_window(&engine),
        last,
        "vsplit puts the new window left"
    );
    let agent = sidebar(&engine, NativeSurface::Agent, WinSplit::Right);
    let tree = sidebar(&engine, NativeSurface::Tree, WinSplit::Left);
    let picked = Picked::File {
        path: path.to_string_lossy().into_owned(),
        line: None,
    };
    engine
        .handle
        .open_picked(&picked, OpenIn::Current, &[agent, tree], 0)
        .unwrap();
    assert!(answered(&engine, &rx, 0), "the open was never answered");
    assert_eq!(current_window(&engine), last);
    assert!(same_file(&state(&engine).0, &path));
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
