//! Regression coverage for the self-dup collision in
//! `process::relay_stdin_fd`: forces the caller's relay descriptor onto fd
//! 3 -- `nvim_api::STDIN_RELAY_CHILD_FD` -- the one case
//! `cli_live.rs`'s own `piped_stdin_lands_in_the_first_buffer_via_the_relay_fd`
//! test never reaches, because opening its content file first claims fd 3
//! for itself and pushes the relay clone to fd 4.
//!
//! `source == STDIN_RELAY_CHILD_FD` is not theoretical: `std`'s own
//! `AsFd::try_clone_to_owned` (what `main::maybe_relay_stdin` calls on the
//! real editor's own stdin) allocates at the lowest free descriptor via
//! `F_DUPFD_CLOEXEC`, and with only stdio open at the point `engine_config`
//! runs -- before `Term::init`, before any pipe exists -- that lowest free
//! descriptor already is fd 3. A `dup2`/`dup3` of a descriptor onto itself
//! is platform-inconsistent: `dup2` is a documented no-op that leaves
//! `FD_CLOEXEC` set, so `exec` closes it anyway (silently losing the piped
//! content on x86_64/macOS), while `dup3` rejects equal descriptors
//! outright with `EINVAL` (refusing to start `view -` at all on
//! aarch64/riscv64, where rustix routes through it).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use rustix::io::FdFlags;
use view_engine::process::{Engine, EngineConfig};

#[test]
fn relay_survives_when_the_source_fd_already_is_the_child_target_fd() {
    let content = std::env::temp_dir().join(format!(
        "view-engine-stdin-relay-self-dup-{}.txt",
        std::process::id()
    ));
    std::fs::write(&content, "hello from fd 3\n").unwrap();
    let opened = std::fs::File::open(&content).unwrap();

    // Relocated off whatever low descriptor `File::open`'s own
    // lowest-free allocation happened to land on -- which can just as
    // easily be fd 3 itself as any other -- so the only fd this test's own
    // bookkeeping claims below is the one under test, never a second,
    // independently-tracked `OwnedFd` racing it for the same raw number
    // (two owners of one raw fd abort on drop, std's own IO safety check
    // firing before either side gets a chance to exercise anything).
    #[allow(unsafe_code)]
    let source: OwnedFd = rustix::io::fcntl_dupfd_cloexec(opened.as_fd(), 64).unwrap();
    drop(opened);

    // Forces the exact collision `relay_stdin_fd` must handle: an `OwnedFd`
    // whose raw value already is `STDIN_RELAY_CHILD_FD` (3). Wrapping the
    // fixed descriptor number performs no syscall of its own -- what makes
    // it a real, owned descriptor pointing at `content`'s file is the
    // `dup2` call right after, exactly mirroring `process::relay_stdin_fd`'s
    // own established pattern and its accompanying safety argument.
    #[allow(unsafe_code)]
    let mut relay_fd = unsafe { OwnedFd::from_raw_fd(3) };
    rustix::io::dup2(source.as_fd(), &mut relay_fd).unwrap();
    drop(source);

    // `dup2` to a *different* destination fd (source is 64, destination is
    // 3) unconditionally clears `FD_CLOEXEC` on arrival regardless of the
    // source's own flags, so without this line fd 3 would already be
    // `FD_CLOEXEC`-clear before `relay_stdin_fd` ever runs -- making the
    // pre-fix self-dup2(3, 3) no-op (which leaves whatever flag state was
    // already there untouched) indistinguishable from the real fix's
    // explicit clear: both would land on "already cleared" by accident,
    // and this test would pass against either. Re-arming it here is what
    // makes the assertion below actually exercise the fix: confirmed by
    // hand -- reverting `relay_stdin_fd`'s self-dup branch to a plain
    // `dup2(source_fd, &mut target)` makes this test fail (`ui_attach_with_stdin_relay`
    // or the `getline` equality below), the real fix passes.
    rustix::io::fcntl_setfd(relay_fd.as_fd(), FdFlags::CLOEXEC).unwrap();

    let cfg = EngineConfig::isolated()
        .with_arg("-")
        .with_stdin_relay(relay_fd);
    let mut engine = Engine::spawn(cfg).unwrap();
    engine.handle.ui_attach_with_stdin_relay(80, 24).unwrap();

    assert_eq!(
        engine.handle.eval_str("getline(1)").unwrap(),
        "hello from fd 3",
        "relay_stdin_fd must clear FD_CLOEXEC in place rather than \
         dup2/dup3-ing fd 3 onto itself when the caller's own stdin clone \
         already landed there, or the piped content never reaches nvim"
    );
    let _ = engine.wait_exit();
    std::fs::remove_file(&content).ok();
}
