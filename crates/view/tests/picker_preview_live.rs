//! Live-nvim proof of the picker preview pane's load-bearing contract: a
//! path with a modified-but-unsaved buffer must preview that buffer's
//! in-memory content, never the still-unmodified file on disk (see
//! `docs/picker-preview-wire-capture.md` case 3, and the crate's "nvim owns
//! all buffer text" hard rule). A disk-read-only implementation passes
//! every other picker test and only this one -- driven against a real
//! engine, not a fixture -- can catch it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use view_core::msg::Msg;
use view_engine::process::{Engine, EngineConfig};

/// How long a preview reply is waited for. Generous for the same reason
/// `bridge_live.rs::ARRIVAL` is: a cold nvim spawn on a loaded box is the
/// slow part.
const ARRIVAL: Duration = Duration::from_secs(10);

/// A live nvim with no UI attach needed -- buffer content is not
/// redraw-derived state, the same conclusion
/// `docs/picker-preview-wire-capture.md`'s capture method reaches.
struct Session {
    engine: Engine,
    rx: Receiver<Msg>,
    dir: PathBuf,
}

impl Session {
    /// `-n` (no swapfile): every isolated spawn in this test binary shares
    /// one hermetic HOME (`crate::env::hermetic_home`), so concurrent
    /// `:edit` calls across this file's tests race to create the same
    /// `.local/state/nvim/swap` directory -- exactly the kind of shared,
    /// incidental state a swapfile has no bearing on for a test that
    /// never crashes nvim and never needs recovery.
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "view-picker-preview-live-{name}-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut engine = Engine::spawn(EngineConfig::isolated().with_arg("-n")).unwrap();
        let (tx, rx): (SyncSender<Msg>, Receiver<Msg>) = std::sync::mpsc::sync_channel(64);
        let _pump = engine.start_pump(tx);
        Self { engine, rx, dir }
    }

    fn command(&self, cmd: &str) {
        self.engine.handle.command(cmd).unwrap();
    }

    /// Requests a preview for `path` at `generation`, the same call
    /// `RpcCall::PreviewBuffer`'s executor arm makes in production.
    fn preview(&self, path: &str, generation: u64) {
        self.engine.handle.preview_buffer(path, generation).unwrap();
    }

    /// The first `Msg::PickerPreviewReply` the pump delivers, within
    /// `ARRIVAL`.
    fn wait_for_reply(&self) -> Option<Msg> {
        let deadline = Instant::now() + ARRIVAL;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(msg @ Msg::PickerPreviewReply { .. }) => return Some(msg),
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// The load-bearing case: a buffer opened, then modified without `:write`,
/// must preview the in-memory content. A disk-read-only implementation
/// would instead see the still-unmodified file this test also writes to
/// disk, so a regression here fails by returning the disk content instead.
#[test]
fn a_modified_unsaved_buffer_previews_its_modified_content_not_the_disk_file() {
    let session = Session::start("modified");
    let path = session.dir.join("target.txt");
    std::fs::write(&path, "disk line one\ndisk line two").unwrap();
    let path_str = path.to_string_lossy().into_owned();

    session.command(&format!("edit {}", path_str.replace(' ', "\\ ")));
    session.command(
        "call setline(1, ['modified line one', 'modified line two', 'modified line three'])",
    );

    session.preview(&path_str, 1);
    let reply = session
        .wait_for_reply()
        .expect("a PreviewBuffer request must answer with Msg::PickerPreviewReply");

    match reply {
        Msg::PickerPreviewReply {
            generation,
            path: replied_path,
            loaded,
            lines,
        } => {
            assert_eq!(generation, 1);
            assert_eq!(replied_path, path_str);
            assert!(loaded, "a path with an open buffer must reply loaded: true");
            assert_eq!(
                lines,
                vec![
                    "modified line one".to_string(),
                    "modified line two".to_string(),
                    "modified line three".to_string(),
                ],
                "the preview must reflect the modified in-memory buffer, not the \
                 still-unmodified file on disk"
            );
        }
        other => panic!("expected Msg::PickerPreviewReply, got {other:?}"),
    }
}

/// The baseline a disk-read-only implementation would also pass: an
/// unmodified, freshly opened buffer's content matches the file verbatim.
/// Kept alongside the load-bearing case above (not as a replacement for
/// it) since this alone cannot distinguish a correct RPC-backed preview
/// from a buggy disk-only one -- see
/// `docs/picker-preview-wire-capture.md`'s case 2.
#[test]
fn an_unmodified_buffer_previews_the_same_content_the_file_holds() {
    let session = Session::start("unmodified");
    let path = session.dir.join("target.txt");
    std::fs::write(&path, "disk line one\ndisk line two").unwrap();
    let path_str = path.to_string_lossy().into_owned();

    session.command(&format!("edit {}", path_str.replace(' ', "\\ ")));

    session.preview(&path_str, 2);
    let reply = session
        .wait_for_reply()
        .expect("a PreviewBuffer request must answer with Msg::PickerPreviewReply");

    match reply {
        Msg::PickerPreviewReply { loaded, lines, .. } => {
            assert!(loaded);
            assert_eq!(
                lines,
                vec!["disk line one".to_string(), "disk line two".to_string()]
            );
        }
        other => panic!("expected Msg::PickerPreviewReply, got {other:?}"),
    }
}

/// A path with no buffer open must reply `loaded: false` with no lines --
/// the signal the caller uses to fall back to a disk read
/// (`Effect::PickerPreviewFallback`), never invented placeholder content.
#[test]
fn a_path_with_no_open_buffer_replies_not_loaded() {
    let session = Session::start("no-buffer");
    let path = session.dir.join("never-opened.txt");
    std::fs::write(&path, "disk line one").unwrap();
    let path_str = path.to_string_lossy().into_owned();

    session.preview(&path_str, 3);
    let reply = session
        .wait_for_reply()
        .expect("a PreviewBuffer request must answer with Msg::PickerPreviewReply");

    match reply {
        Msg::PickerPreviewReply { loaded, lines, .. } => {
            assert!(
                !loaded,
                "a path with no open buffer must reply loaded: false"
            );
            assert!(lines.is_empty());
        }
        other => panic!("expected Msg::PickerPreviewReply, got {other:?}"),
    }
}
