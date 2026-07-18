//! End-to-end proof that the wired `view` binary starts nvim, paints typed
//! text into a real terminal, and exits cleanly on `:q!`, driven through an
//! actual pty rather than an in-process mock. The real quiesce protocol
//! (deterministic "redraw settled" signaling instead of sleeps) arrives in
//! P3; this is a smoke test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Locates the `view` binary next to this crate's own target directory,
/// building it first if it is not already there.
///
/// `view` is a bin-only crate with no library target, so Cargo's
/// `CARGO_BIN_EXE_<name>` mechanism is unavailable: Cargo only sets that
/// variable for binaries reachable via a package's own dependency graph, and
/// it refuses to add a lib-less crate as a dependency at all (confirmed by
/// attempting exactly that: `cargo add view -p view-oracle --dev` succeeds
/// but emits "ignoring invalid dependency `view` which is missing a lib
/// target", and `env!("CARGO_BIN_EXE_view")` then fails to compile). Falls
/// back to locating the workspace `target/<profile>/view` executable
/// directly, building it on demand so this test is self-sufficient under a
/// direct `cargo test -p view-oracle` as well as under `task test`.
fn view_bin_path() -> PathBuf {
    let profile_dir = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path.push("target");
    path.push(profile_dir);
    path.push("view");
    if !path.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = std::process::Command::new(cargo)
            .args(["build", "-p", "view"])
            .status()
            .expect("failed to invoke cargo build -p view");
        assert!(status.success(), "cargo build -p view failed");
    }
    path
}

/// Spawns a background thread that forwards every chunk read from `reader`
/// onto the returned channel, so the test thread can poll with a bounded
/// timeout instead of blocking on a single `read` call that may return only
/// part of the child's output.
fn spawn_reader(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0_u8; 65536];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

#[test]
fn view_paints_typed_text_in_a_pty() {
    let view_bin = view_bin_path();

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let pid = std::process::id();
    let scratch = std::env::temp_dir().join(format!("view-oracle-smoke-{pid}.txt"));
    let isolated_home = std::env::temp_dir().join(format!("view-oracle-home-{pid}"));
    std::fs::create_dir_all(&isolated_home).unwrap();

    let mut cmd = CommandBuilder::new(view_bin);
    cmd.arg(&scratch);
    // isolate from any real nvim user config on the host running this test:
    // a dashboard plugin or custom keymap on a bare "i" would make the
    // typed-text assertion below nondeterministic
    for var in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
    ] {
        cmd.env(var, isolated_home.join(var.to_lowercase()));
    }

    let mut child = pair.slave.spawn_command(cmd).unwrap();
    let reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    // the slave fd must not outlive the child's own copy, or the master
    // never sees EOF once the child exits
    drop(pair.slave);

    let rx = spawn_reader(reader);
    let mut parser = vt100::Parser::new(24, 80, 0);

    // wait for nvim's startup redraw before typing, same as a human would
    let startup_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < startup_deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                parser.process(&chunk);
                if !parser.screen().contents().trim().is_empty() {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    writer.write_all(b"ihello from view").unwrap();
    writer.flush().unwrap();

    let type_deadline = Instant::now() + Duration::from_secs(5);
    let mut found = false;
    while Instant::now() < type_deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                parser.process(&chunk);
                if parser.screen().contents().contains("hello from view") {
                    found = true;
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(
        found,
        "screen never showed typed text; last screen:\n{}",
        parser.screen().contents()
    );

    writer.write_all(b"\x1b:q!\r").unwrap();
    writer.flush().unwrap();
    let _ = child.wait();

    let _ = std::fs::remove_file(&scratch);
    let _ = std::fs::remove_dir_all(&isolated_home);
}
