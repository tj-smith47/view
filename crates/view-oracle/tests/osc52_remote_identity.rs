//! Byte-for-byte proof that a `"+y` yank's OSC 52 clipboard escape is
//! identical whether the engine answering it is local or reached over the
//! stub-ssh remote transport (`view_oracle::remote`).
//!
//! `Effect::Osc52Copy` (`crates/view-core/src/msg.rs`) is emitted alongside
//! `Effect::ClipboardWrite` on every copy, unconditionally
//! (`crates/view-core/src/update/mod.rs`'s `EngineRequest::ClipboardSet`
//! arm) -- never gated on a local display being present -- and
//! `Term::write_osc52` (`crates/view-tui/src/terminal.rs:205-212,538-540`)
//! is a pure function of the register and yanked text it is handed. Nothing
//! in that path reads the transport the engine was reached over, so the
//! escape a real `view` process writes to its own pty is exactly what this
//! test holds one engine's output against the other's.
//!
//! Driven through the real `view` binary end to end, not through
//! `view-oracle`'s `EngineSession` (which drives the pinned nvim directly
//! and has no runtime loop to exercise `drain_osc52`/`Term::write_osc52` at
//! all): `PtySession`'s raw-output capture is what lets the literal escape
//! bytes a real terminal would receive be read back and compared, which is
//! the only way to prove the claim rather than assume it from the `Effect`
//! layer down.
//!
//! # Why a `PATH` substitution, not `RemoteSpec::with_ssh_bin`
//!
//! `view --remote` always resolves a bare `ssh` on `PATH`
//! (`RemoteSpec::new`'s own default) -- the CLI exposes no flag to name a
//! different client the way `view_oracle::remote::stub_config` does for a
//! Rust-constructed `RemoteSpec`. Since `Engine::spawn`'s remote half
//! applies no environment of its own to the client `Command` (deliberately;
//! see `remote_command`'s doc in `crates/view-engine/src/process.rs`), the
//! `ssh` a spawned `view` process runs inherits `view`'s own `PATH`
//! unchanged, so leading that `PATH` with a directory holding an `ssh` that
//! is really `view_oracle::remote::stub_client()` reaches the identical
//! committed stand-in through the one seam the CLI does expose.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

/// The destination `--remote` is given. Parsed and discarded by the stub the
/// same way `view_oracle::remote::STUB_TARGET` is (that constant is private
/// to its own module), so this only has to look like a hostname.
const REMOTE_TARGET: &str = "view-oracle-osc52-stub-host";

/// The line typed into the buffer before it is yanked. Distinct from every
/// other oracle fixture's marker text so a raw-output grep for it (were one
/// ever added) could not cross-match another test's session.
const MARKER: &str = "view-osc52-oracle-marker";

/// Symlinks a directory's `ssh` at [`view_oracle::remote::stub_client`] under
/// `scratch`, and returns a `PATH` value with that directory leading the
/// caller's own -- see this file's module doc for why a `PATH` lead is the
/// substitution the CLI's own remote flags leave room for.
fn stub_ssh_path(scratch: &std::path::Path) -> std::ffi::OsString {
    let dir = scratch.join("stub-ssh-bin");
    std::fs::create_dir_all(&dir).expect("the stub ssh bin directory must be creatable");
    let link = dir.join("ssh");
    if link.symlink_metadata().is_err() {
        std::os::unix::fs::symlink(view_oracle::remote::stub_client(), &link)
            .expect("the stub ssh symlink must be creatable");
    }
    let mut joined = std::ffi::OsString::from(dir.as_os_str());
    joined.push(":");
    joined.push(std::env::var_os("PATH").unwrap_or_default());
    joined
}

/// The first complete OSC 52 escape (`ESC ] 5 2 ; ... ESC \`) found in
/// `raw`, or `None` if the session never wrote one.
///
/// A byte-pattern search rather than a `vt100`-screen read: OSC 52 leaves no
/// cell on the parsed screen (`:help clipboard-osc52` is invisible output by
/// design), so the literal bytes are the only place this claim can be
/// checked at all.
fn extract_osc52(raw: &[u8]) -> Option<&[u8]> {
    const PREFIX: &[u8] = b"\x1b]52;";
    const TERMINATOR: &[u8] = b"\x1b\\";
    let start = raw.windows(PREFIX.len()).position(|w| w == PREFIX)?;
    let rel_end = raw[start..]
        .windows(TERMINATOR.len())
        .position(|w| w == TERMINATOR)?;
    Some(&raw[start..start + rel_end + TERMINATOR.len()])
}

/// Spawns `view` against `home`'s isolated `XDG_*` directories, with
/// `--remote REMOTE_TARGET` and `remote_ssh_path` leading its own `PATH`
/// when `remote_ssh_path` is given, or a plain local session otherwise.
/// Arms raw-output recording immediately, before anything drains the
/// session, per [`view_oracle::PtySession::record_raw_output`]'s own
/// contract.
fn spawn_session(
    bin: &std::path::Path,
    home: &std::path::Path,
    remote_ssh_path: Option<&std::ffi::OsStr>,
) -> view_oracle::PtySession {
    let mut cmd = portable_pty::CommandBuilder::new(bin);
    common::isolate_xdg_native_off(&mut cmd, home);
    if let Some(path) = remote_ssh_path {
        cmd.env("PATH", path);
        cmd.arg("--remote");
        cmd.arg(REMOTE_TARGET);
    }
    let mut session = view_oracle::PtySession::spawn_configured(cmd, 80, 24)
        .unwrap_or_else(|err| panic!("PtySession::spawn_configured against {bin:?}: {err}"));
    session.record_raw_output();
    session
}

/// Types [`MARKER`] into the buffer and yanks the whole line into `"+`, the
/// register nvim's own `g:clipboard` provider `view` installs both
/// [`Effect::ClipboardWrite`] and [`Effect::Osc52Copy`] from
/// (`crates/view-core/src/update/mod.rs`).
fn drive_yank(session: &mut view_oracle::PtySession) {
    assert!(
        session.wait_for("~", Duration::from_secs(10)),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );
    session
        .send(format!("i{MARKER}\x1b").as_bytes())
        .expect("typed marker must reach the session");
    assert!(
        session.wait_for(MARKER, Duration::from_secs(10)),
        "typed marker never appeared on screen; screen:\n{}",
        session.screen()
    );
    session
        .send(b"0\"+yy")
        .expect("the yank keys must reach the session");
}

/// Polls `session`'s raw output for an OSC 52 escape until one appears or
/// `timeout` elapses. A single-line yank prints no cmdline message under
/// nvim's default `'report'`, so there is no on-screen signal to
/// `wait_for` instead (the same reasoning `clipboard_roundtrip.rs` states
/// for its own independent-reader poll).
fn wait_for_osc52(session: &mut view_oracle::PtySession, timeout: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(bytes) = extract_osc52(session.raw_output()) {
            return Some(bytes.to_vec());
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn quit(session: &mut view_oracle::PtySession) {
    let _ = session.send(b"\x1b:q!\r");
    let _ = session.wait_for_exit(Duration::from_secs(5));
}

#[test]
fn osc52_clipboard_escape_is_byte_identical_local_vs_stub_remote() {
    assert!(
        view_oracle::remote::stub_available(),
        "no stand-in ssh client on this host ({}); nothing here can drive the \
         remote leg (see view_oracle::remote's module doc)",
        view_oracle::remote::stub_client().display()
    );

    let bin = common::view_bin_path();

    // the local-engine yank, characterizing the escape a normal session
    // writes, before the remote leg below is held against it
    let local_paths = common::ScratchPaths::new("osc52-identity-local");
    let mut local = spawn_session(&bin, &local_paths.isolated_home, None);
    drive_yank(&mut local);
    let local_osc52 = wait_for_osc52(&mut local, Duration::from_secs(5)).unwrap_or_else(|| {
        panic!(
            "the local session never wrote an OSC 52 escape; raw output:\n{}",
            String::from_utf8_lossy(local.raw_output())
        )
    });
    quit(&mut local);

    const REGISTER_PREFIX: &[u8] = b"\x1b]52;c;";
    assert!(
        local_osc52.starts_with(REGISTER_PREFIX),
        "a `\"+yy` yank must select OSC 52's clipboard code 'c' \
         (`write_osc52_bytes`'s register mapping), got {local_osc52:?}"
    );

    // the same yank through the stub-ssh remote path, held against the
    // local bytes captured above
    let remote_paths = common::ScratchPaths::new("osc52-identity-remote");
    let ssh_path = stub_ssh_path(&remote_paths.isolated_home);
    let mut remote = spawn_session(&bin, &remote_paths.isolated_home, Some(&ssh_path));
    drive_yank(&mut remote);
    let remote_osc52 = wait_for_osc52(&mut remote, Duration::from_secs(10)).unwrap_or_else(|| {
        panic!(
            "the stub-remote session never wrote an OSC 52 escape; raw output:\n{}",
            String::from_utf8_lossy(remote.raw_output())
        )
    });
    quit(&mut remote);

    assert_eq!(
        remote_osc52, local_osc52,
        "the stub-remote engine's OSC 52 escape diverged from the local one: \
         local {local_osc52:?}, remote {remote_osc52:?}"
    );
}
