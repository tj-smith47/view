//! fd 2 while the TUI owns the terminal, and fd 2 again once it does not.
//!
//! A whole test binary of its own because the descriptor under test is
//! process state: libtest's capture covers only the `eprint!` macros, so a
//! sibling test running in parallel would be writing to the very descriptor
//! this one redirects. `std::io::stderr()` is what the writes go through
//! here for the same reason -- it resolves fd 2 directly, which is the path
//! a system library's own diagnostic takes.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs::File;
use std::io::Write;
use view_test_support::ScratchDir;
use view_tui::terminal::StderrGuard;

fn write_stderr(text: &str) {
    let mut err = std::io::stderr();
    err.write_all(text.as_bytes()).unwrap();
    err.flush().unwrap();
}

#[test]
fn a_redirect_takes_stderr_off_the_terminal_and_hands_it_back() {
    let dir = ScratchDir::new("tui-stderr-redirect").unwrap();
    let terminal = dir.join("terminal");
    let sink = dir.join("sink");

    // the test binary's own fd 2, put back at the end so a failure's output
    // still reaches whoever is running the suite
    let real = rustix::io::fcntl_dupfd_cloexec(std::io::stderr(), 0).unwrap();
    // stands in for the terminal: a file, so what reached it is readable
    rustix::stdio::dup2_stderr(File::create(&terminal).unwrap()).unwrap();

    {
        let guard = StderrGuard::redirect(&File::create(&sink).unwrap()).unwrap();
        write_stderr("owned");
        drop(guard);
    }
    write_stderr("restored");

    rustix::stdio::dup2_stderr(&real).unwrap();

    let reached_sink = std::fs::read_to_string(&sink).unwrap();
    let reached_terminal = std::fs::read_to_string(&terminal).unwrap();
    assert_eq!(
        reached_sink, "owned",
        "a write under the guard has to reach the sink and nothing else, \
         got {reached_sink:?} in the sink and {reached_terminal:?} on the \
         terminal"
    );
    assert_eq!(
        reached_terminal, "restored",
        "the terminal has to see the write after the guard and none of the \
         writes under it -- a byte landing here mid-session paints over a \
         cell the differential painter believes it still owns; got \
         {reached_terminal:?}"
    );
}
