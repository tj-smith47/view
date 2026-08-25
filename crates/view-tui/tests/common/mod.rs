//! A pty on descriptor 0, shared by the integration tests that need
//! crossterm's process-wide reader pointed at a terminal they control.
//!
//! Descriptor 0 is process state and crossterm's reader binds to it once,
//! so a test file using this holds exactly one test: a second terminal
//! opened later in the same process is a terminal the reader never sees.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::ffi::OsStrExt;

/// Opens a pty, puts its slave end on descriptor 0 in raw mode, and returns
/// `(master, slave)` for the caller to write into and poll.
///
/// Raw mode carries a written burst through byte for byte: a canonical-mode
/// line discipline would hold every byte back until a newline that an escape
/// sequence never contains, and would echo them into the master besides.
pub fn stdin_pty() -> (rustix::fd::OwnedFd, std::fs::File) {
    let master =
        rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)
            .unwrap();
    rustix::pty::grantpt(&master).unwrap();
    rustix::pty::unlockpt(&master).unwrap();
    let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
    let slave = std::fs::File::options()
        .read(true)
        .write(true)
        .open(std::ffi::OsStr::from_bytes(name.as_bytes()))
        .unwrap();
    let mut attrs = rustix::termios::tcgetattr(&slave).unwrap();
    attrs.make_raw();
    rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &attrs).unwrap();
    rustix::stdio::dup2_stdin(&slave).unwrap();
    (master, slave)
}

/// Blocks until `fd` has something to read, up to two seconds.
// each test file compiles this module for itself, so a helper the file at
// hand does not call is dead code only in that one binary
#[allow(dead_code)]
pub fn wait_readable(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    let mut fds = [rustix::event::PollFd::from_borrowed_fd(
        fd,
        rustix::event::PollFlags::IN,
    )];
    let timeout = rustix::event::Timespec {
        tv_sec: 2,
        tv_nsec: 0,
    };
    matches!(rustix::event::poll(&mut fds, Some(&timeout)), Ok(n) if n > 0)
}

/// A [`ReplySource`](view_tui::tiers::ReplySource) over descriptor 0, for a
/// test driving a real [`Probe`](view_tui::tiers::Probe) against the pty
/// [`stdin_pty`] put there.
///
/// Polls before it reads because that slave is in raw mode with `VMIN=1`,
/// so a read with nothing queued would block past the probe's own window.
#[allow(dead_code)]
pub struct SlaveSource;

#[allow(dead_code)]
impl view_tui::tiers::ReplySource for SlaveSource {
    fn next_chunk(&mut self, budget: std::time::Duration) -> Option<Vec<u8>> {
        use std::os::fd::AsFd;
        let stdin = std::io::stdin();
        let mut fds = [rustix::event::PollFd::from_borrowed_fd(
            stdin.as_fd(),
            rustix::event::PollFlags::IN,
        )];
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: i64::try_from(budget.as_nanos()).unwrap_or(i64::MAX),
        };
        if !matches!(rustix::event::poll(&mut fds, Some(&timeout)), Ok(n) if n > 0) {
            return Some(Vec::new());
        }
        let mut buf = [0_u8; 256];
        match rustix::io::read(stdin.as_fd(), &mut buf) {
            Ok(n) => Some(buf[..n].to_vec()),
            Err(_) => None,
        }
    }
}
