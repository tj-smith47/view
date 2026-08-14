//! Live-nvim proof that `EngineHandle::open_file` opens hostile-character
//! filenames correctly rather than having them misparsed as ex-command
//! syntax. See `docs/tree-open-file-wire-capture.md` for the wire capture
//! this test mirrors: a space, a leading `+`, a bare `%`, and a bare `#`
//! each have special meaning to nvim's command-line parser, and an
//! unescaped `:edit` genuinely fails on at least one of them (`E499` on a
//! bare `%`) rather than silently opening the wrong file.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::mpsc;

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
        .join(format!("tree-open-file-{nonce}"));
    std::fs::create_dir_all(&root).expect("create test root");
    std::fs::canonicalize(root).expect("canonicalize test root")
}

#[test]
fn open_file_opens_hostile_character_filenames_without_misparsing_them() {
    let root = scratch_root("hostile");
    let mut cases = vec![
        "plain.txt",
        "a file.txt",
        "100%.txt",
        "#tag.txt",
        "+weird.txt",
        "+42",
        "++enc.txt",
        "-dash.txt",
        "both space and % and # and +weird.txt",
    ];
    // `|` and `\` cannot exist in Windows filenames, so these two on-disk
    // fixtures are unix-only -- the property under test is not. `cfg!`
    // rather than a `#[cfg(unix)]` extend so `mut` stays used on every
    // platform (Windows clippy fails the build on `unused_mut` otherwise).
    if cfg!(unix) {
        cases.extend(["a|b.txt", "back\\slash.txt"]);
    }

    let mut engine = Engine::spawn(EngineConfig::isolated()).expect("spawn engine");
    let (tx, rx) = mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    drop(rx); // this test drives no Msg through the pump, only raw requests

    for case in cases {
        let path = root.join(case);
        std::fs::write(&path, format!("hello from {case}")).expect("write case file");
        let path_str = path.to_string_lossy().into_owned();

        engine
            .handle
            .open_file(&path_str)
            .expect("issue open_file notify");

        // `open_file` is fire-and-forget (`notify`, no reply): the
        // follow-up `nvim_eval` below travels the same ordered RPC stream,
        // so by the time nvim replies to it the preceding notify has
        // already been dispatched and its synchronous `:edit` has already
        // run -- there is nothing async on nvim's side here to race.
        let current_name = engine
            .handle
            .request("nvim_eval", vec![rmpv::Value::from("expand('%:p')")])
            .expect("read current buffer name");
        let current_name = current_name
            .as_str()
            .expect("buffer name is a string")
            .to_owned();
        let canon =
            |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        assert_eq!(
            canon(std::path::Path::new(&current_name)),
            canon(&path),
            "open_file({case:?}) must open exactly the intended file, not \
             misparse a hostile character as ex-command syntax"
        );

        let lines = engine
            .handle
            .request(
                "nvim_buf_get_lines",
                vec![
                    rmpv::Value::from(0),
                    rmpv::Value::from(0),
                    rmpv::Value::from(-1),
                    rmpv::Value::from(false),
                ],
            )
            .expect("read buffer lines");
        let first_line = lines
            .as_array()
            .and_then(|a| a.first())
            .and_then(rmpv::Value::as_str)
            .unwrap_or_default();
        assert_eq!(
            first_line,
            format!("hello from {case}"),
            "open_file({case:?}) must open that file's own content, not \
             reuse or corrupt another buffer"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
