//! A panic under the redirect prints where the user can read it.
//!
//! The redirect exists so nothing lands on the painted screen, and a panic
//! message is the one write that must land there anyway -- an editor that
//! dies into a log nobody was told about is a session that hung. Both the
//! hook `StderrGuard` chains and the one `restore` runs put fd 2 back
//! before the message prints; this pins the ordering, which is the half a
//! reader cannot see from the redirect alone.
//!
//! Its own test binary for the same reason its sibling has one: fd 2 and
//! the panic hook are both process state.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs::File;
use std::io::Write;
use view_test_support::ScratchDir;
use view_tui::terminal::StderrGuard;

#[test]
fn a_panic_under_the_redirect_prints_on_the_restored_terminal() {
    let dir = ScratchDir::new("tui-stderr-panic").unwrap();
    let terminal = dir.join("terminal");
    let sink = dir.join("sink");

    let real = rustix::io::fcntl_dupfd_cloexec(std::io::stderr(), 0).unwrap();
    rustix::stdio::dup2_stderr(File::create(&terminal).unwrap()).unwrap();

    // stands in for the hook the guard chains onto -- libtest's own captures
    // the message instead of writing it, which would prove nothing about
    // which descriptor a printing hook reaches
    let libtest_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|info| {
        let mut err = std::io::stderr();
        let _ = write!(err, "panicked: {}", info.location().unwrap());
        let _ = err.flush();
    }));

    let guard = StderrGuard::redirect(&File::create(&sink).unwrap()).unwrap();
    let panicked = std::panic::catch_unwind(|| panic!("engine gone")).is_err();

    std::panic::set_hook(libtest_hook);
    drop(guard);
    rustix::stdio::dup2_stderr(&real).unwrap();

    let reached_sink = std::fs::read_to_string(&sink).unwrap();
    let reached_terminal = std::fs::read_to_string(&terminal).unwrap();
    assert!(
        panicked,
        "the fixture never panicked, so nothing was printed"
    );
    assert!(
        reached_terminal.starts_with("panicked: "),
        "the panic message never reached the terminal: it printed {:?} \
         into the sink, which is the log-only world a dying session must \
         not disappear into",
        reached_sink
    );
    assert!(
        reached_sink.is_empty(),
        "the hook printed before fd 2 was handed back, so the message is in \
         the sink too: {reached_sink:?}"
    );
}
