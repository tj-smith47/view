//! A thread that owns every write to one file, fed through a bounded queue
//! a caller never waits on.
//!
//! A write made on the thread that asked for it holds that thread behind the
//! disk, so a thread that must not wait on a disk hands its writes to one
//! that may. The queue is bounded because a disk that
//! stops answering would otherwise grow it for the life of the process; a
//! caller told the queue is full decides what the dropped item costs.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::time::Duration;

/// How long a quitting process waits for a writer's queued items.
///
/// What is queued at quit is a few kilobytes, which a local disk takes in a
/// few milliseconds. A writer still busy past this is stalled on a disk that
/// may never answer, such as a dead network mount, and a quit held behind it
/// is a hang.
pub const QUIT_WAIT: Duration = Duration::from_millis(250);

/// The thread that owns one file's writes, and the queue into it.
///
/// Each item is handed to the thread's `apply` in the order it was queued,
/// until `apply` fails or the writer is closed.
pub struct BackgroundWriter<T, E> {
    queue: Option<SyncSender<T>>,
    /// Answers once the thread's loop ends, with the failure that ended it.
    done: Option<Receiver<Option<E>>>,
}

/// How a writer's thread stood when a wait for it ended.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Finished<E> {
    /// Every queued item was applied.
    Done,
    /// `apply` failed on an item, and nothing queued after it was applied.
    Failed(E),
    /// The thread was still applying when the wait ran out, and is left to
    /// the process exit.
    Busy,
}

/// A closed writer's thread, to be waited on with no lock held.
#[must_use]
pub struct Closed<E>(Option<Receiver<Option<E>>>);

impl<E> Closed<E> {
    /// Waits up to `wait` for the thread to apply what was queued before
    /// the close.
    pub fn wait(self, wait: Duration) -> Finished<E> {
        let Some(done) = self.0 else {
            return Finished::Done;
        };
        match done.recv_timeout(wait) {
            Ok(Some(failed)) => Finished::Failed(failed),
            Err(RecvTimeoutError::Timeout) => Finished::Busy,
            Ok(None) | Err(RecvTimeoutError::Disconnected) => Finished::Done,
        }
    }
}

impl<T: Send + 'static, E: Send + 'static> BackgroundWriter<T, E> {
    /// Starts the thread, named `name`, with room for `capacity` items
    /// queued ahead of the one it is applying.
    ///
    /// # Errors
    ///
    /// Returns the error the host refused the thread with.
    pub fn start(
        name: &str,
        capacity: usize,
        mut apply: impl FnMut(T) -> Result<(), E> + Send + 'static,
    ) -> std::io::Result<Self> {
        let (queue, items) = mpsc::sync_channel::<T>(capacity);
        let (ended, done) = mpsc::channel();
        std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || {
                let mut failed = None;
                for item in items {
                    if let Err(e) = apply(item) {
                        failed = Some(e);
                        break;
                    }
                }
                let _ = ended.send(failed);
            })?;
        Ok(Self {
            queue: Some(queue),
            done: Some(done),
        })
    }
}

impl<T, E> BackgroundWriter<T, E> {
    /// Queues `item` for the thread without waiting.
    ///
    /// # Errors
    ///
    /// Returns [`TrySendError::Full`] with `item` when `capacity` items are
    /// already waiting, and [`TrySendError::Disconnected`] with it once the
    /// thread has ended or the writer was closed. The first disconnect
    /// closes the writer, so [`Self::is_open`] answers false from then on.
    pub fn try_send(&mut self, item: T) -> Result<(), TrySendError<T>> {
        let Some(queue) = &self.queue else {
            return Err(TrySendError::Disconnected(item));
        };
        let sent = queue.try_send(item);
        if matches!(sent, Err(TrySendError::Disconnected(_))) {
            self.queue = None;
        }
        sent
    }

    /// Whether an item queued now can still reach the thread, so a caller
    /// can skip building one that cannot.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.queue.is_some()
    }

    /// Closes the queue, and answers the thread to wait for. A second
    /// close answers one with nothing to wait for.
    pub fn close(&mut self) -> Closed<E> {
        self.queue = None;
        Closed(self.done.take())
    }

    /// Closes the queue and waits up to `wait` for the thread to apply what
    /// was queued before it.
    pub fn finish_within(&mut self, wait: Duration) -> Finished<E> {
        self.close().wait(wait)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Every queued item is applied in order before the wait answers.
    ///
    /// Disconfirm: `wait` answering before the thread ends reads the list
    /// before the last item lands.
    #[test]
    fn a_finished_writer_applied_every_item_in_order() {
        let applied = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&applied);
        let mut writer = BackgroundWriter::<u32, ()>::start("test-writer", 4, move |n| {
            std::thread::sleep(Duration::from_millis(5));
            seen.lock().unwrap().push(n);
            Ok(())
        })
        .unwrap();
        for n in 0..3 {
            writer.try_send(n).unwrap();
        }
        assert_eq!(
            writer.finish_within(Duration::from_secs(30)),
            Finished::Done
        );
        assert_eq!(*applied.lock().unwrap(), vec![0, 1, 2]);
        assert!(!writer.is_open());
    }

    /// A full queue hands the item back at once, and a failed apply ends
    /// the thread and comes back from the wait.
    ///
    /// Disconfirm: an unbounded channel queues the third item, and a
    /// thread that keeps applying after a failure answers the wait `Done`.
    #[test]
    fn a_full_queue_refuses_and_a_failure_ends_the_writer() {
        let (release, held) = mpsc::channel::<()>();
        let (entered_tx, entered) = mpsc::channel::<()>();
        let mut writer = BackgroundWriter::<u32, u32>::start("test-writer", 1, move |n| {
            let _ = entered_tx.send(());
            let _ = held.recv();
            Err(n)
        })
        .unwrap();
        writer.try_send(1).unwrap();
        // the thread has taken the first item and parked in apply, which
        // leaves room for exactly one more
        entered.recv().unwrap();
        writer.try_send(2).unwrap();
        assert!(matches!(writer.try_send(3), Err(TrySendError::Full(3))));
        drop(release);
        assert_eq!(
            writer.finish_within(Duration::from_secs(30)),
            Finished::Failed(1)
        );
    }

    /// Sends once when dropped, which marks the end of the thread that
    /// owned it.
    struct Gone(mpsc::Sender<()>);

    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    /// A send that finds the thread ended closes the writer.
    ///
    /// Disconfirm: `try_send` leaving the queue in place on a disconnect
    /// answers `is_open` true after it.
    #[test]
    fn a_send_to_an_ended_thread_closes_the_writer() {
        let (gone_tx, gone) = mpsc::channel::<()>();
        let guard = Gone(gone_tx);
        let mut writer = BackgroundWriter::<u32, u32>::start("test-writer", 1, move |n| {
            // the closure owns the guard, so it drops once the thread's
            // loop has ended and let go of its queue
            let _held = &guard;
            Err(n)
        })
        .unwrap();
        writer.try_send(1).unwrap();
        gone.recv().unwrap();
        assert!(writer.is_open(), "nothing has told the writer yet");
        assert!(matches!(
            writer.try_send(2),
            Err(TrySendError::Disconnected(2))
        ));
        assert!(!writer.is_open());
    }

    /// A thread stuck in apply holds the wait for `wait` and no longer.
    ///
    /// Disconfirm: a wait with no timeout never returns.
    #[test]
    fn a_stalled_writer_is_left_busy_after_the_wait() {
        let (release, held) = mpsc::channel::<()>();
        let mut writer = BackgroundWriter::<u32, ()>::start("test-writer", 1, move |_| {
            let _ = held.recv();
            Ok(())
        })
        .unwrap();
        writer.try_send(0).unwrap();
        assert_eq!(
            writer.finish_within(Duration::from_millis(20)),
            Finished::Busy
        );
        drop(release);
    }

    /// Every file in the workspace that runs a writer thread goes through
    /// this one, so a change to how a writer degrades is made once.
    ///
    /// Disconfirm: restoring a quit wait of its own in `view`'s native
    /// session, or a done channel of its own in the redraw log, names the
    /// file.
    #[test]
    fn no_crate_carries_a_writer_thread_of_its_own() {
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let own = crates.parent().unwrap().join(file!());
        let needles = [
            "RECORD_QUIT_WAIT",
            "REDRAW_LOG_QUIT_WAIT",
            "Receiver<Option<WriteError>>",
            "mpsc::channel::<RecordWrite>",
        ];
        let mut found = Vec::new();
        let mut dirs = vec![crates.to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n != "target") {
                        dirs.push(path);
                    }
                } else if path.extension().is_some_and(|e| e == "rs") && path != own {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for needle in needles {
                        if text.contains(needle) {
                            found.push(format!("{}: {needle}", path.display()));
                        }
                    }
                }
            }
        }
        assert!(
            found.is_empty(),
            "a writer thread of its own; use view_proc::writer: {found:?}"
        );
    }
}
