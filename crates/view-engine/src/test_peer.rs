//! Endpoints for a peer that can be made to stop reading.
//!
//! A connection whose peer parks inside a write is the one state several
//! of this workspace's guarantees are about -- that a caller never waits on
//! the writer thread, and that a backlog going nowhere is noticed and said
//! out loud -- and no real nvim can be asked to enter it on command. The
//! sink and source live here, beside the connection they wedge, rather than
//! once per test module: two crates need the same parked peer, and a
//! fixture copied per crate drifts until the two "identical" wedges are no
//! longer the same state.

use std::io::{Read, Write};
use std::sync::mpsc;

/// Seconds a test may wait to observe the writer thread reach the sink.
///
/// Arming rather than measurement: until the thread is provably inside a
/// write that cannot finish, there is no stall, and anything timed from
/// before that says nothing.
pub const PARKED_WRITE_ARM_SECS: u64 = 30;

/// A write sink that announces entering a write and then stays inside it
/// until released, standing in for a pipe whose peer stopped reading.
pub struct ParkedSink {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl ParkedSink {
    /// A sink, the signal it announces each entered write on, and the
    /// release that frees it.
    ///
    /// Dropping the release frees the write in progress and every later
    /// one, so a run that has taken its reading -- or one abandoning the
    /// attempt on a failed assertion -- can let the writer thread finish
    /// instead of leaving it parked for the rest of the suite.
    #[must_use]
    pub fn new() -> (Self, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, entered_rx) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        (Self { entered, release }, entered_rx, release_tx)
    }
}

impl Write for ParkedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.entered.send(());
        let _ = self.release.recv();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A read source that yields nothing and does not end, so a connection's
/// reader thread cannot close the connection out from under a test whose
/// subject is the write side.
pub struct IdleSource(mpsc::Receiver<()>);

impl IdleSource {
    /// A source and the sender whose drop ends it: the source reads as an
    /// ordinary end of stream from then on, retiring the reader thread.
    #[must_use]
    pub fn new() -> (Self, mpsc::Sender<()>) {
        let (tx, rx) = mpsc::channel();
        (Self(rx), tx)
    }
}

impl Read for IdleSource {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        let _ = self.0.recv();
        Ok(0)
    }
}
