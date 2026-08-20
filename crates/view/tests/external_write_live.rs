//! End-to-end proof of out-of-band write detection across both halves that
//! own it: `view_ai`'s filesystem watch, which nominates a path off a real
//! filesystem event, and a live nvim answering `RpcCall::Checktime` for
//! that same path. Neither crate can see the other -- the watch is
//! dependency-wise blind to the engine -- so a test in either one can only
//! pin its own half, and the question this file exists for lives exactly in
//! the seam.
//!
//! That question is whether nominating removals cries wolf. Atomic-save
//! tooling writes a temp file and renames it over the target, which emits a
//! removal; forwarding removals would be a bad trade if it made view
//! announce a vanished file every time an agent saved one. It does not, and
//! the reason is structural rather than lucky: the watch only nominates,
//! and the probe re-stats every nominated path before answering. Both
//! shapes are driven for real here rather than argued -- a `rename` over
//! the target, and a `remove_file` -- and what the probe answers for each
//! is asserted.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use view_ai::WatchHandle;
use view_core::msg::{CheckTimeOutcome, Msg};
use view_engine::process::{Engine, EngineConfig};
use view_test_support::ScratchDir;

/// How long a detection or a reply is waited for. Generous because a cold
/// nvim spawn and the watch's own registration walk are the slow parts; a
/// healthy pair answers in milliseconds.
const ARRIVAL: Duration = Duration::from_secs(10);

/// Everything one case needs live at once: a watched scratch root, a real
/// engine holding the file open, and the two channels the halves answer on.
struct Watched {
    engine: Engine,
    engine_rx: Receiver<Msg>,
    watch: WatchHandle,
    watch_rx: Receiver<Msg>,
    path: PathBuf,
    root: PathBuf,
    // held only for its `Drop`: the scratch directory outlives every use of
    // `root` above, which is why nothing here reads it back
    _dir: ScratchDir,
}

impl Drop for Watched {
    fn drop(&mut self) {
        self.watch.stop();
    }
}

/// Opens `<scratch>/watched.txt` as a loaded buffer in a live engine and
/// starts the real watch over the scratch root.
///
/// The buffer is left unmodified, which is the state most open files are in
/// and the one that reaches nvim's own re-read: an ordinary save answers
/// `HandledSilently` because nvim reloaded it itself, so a `FileGone` on
/// that same path could only have come from the watch's nomination being
/// mistaken for a verdict.
fn watched(label: &str) -> Watched {
    let dir = ScratchDir::new(label).expect("scratch root");
    let root = std::fs::canonicalize(dir.join(".")).expect("canonicalize the scratch root");
    let path = root.join("watched.txt");
    std::fs::write(&path, "original\n").expect("write fixture");

    let (engine, _pump, engine_rx) = common::spawn_with_pump(EngineConfig::isolated(), 64);
    engine.handle.ui_attach(80, 24).expect("attach ui");
    let name = path.to_string_lossy().into_owned();
    engine.handle.load_hidden(&name, 1).expect("issue the load");
    common::drain_until(&engine_rx, ARRIVAL, |msg| match msg {
        Msg::HiddenBufferLoaded { buf, .. } => Some(buf.expect("the path resolves").0),
        _ => None,
    })
    .expect("the buffer must load before anything touches the file");

    let (watch_tx, watch_rx) = channel::<Msg>();
    let watch = view_ai::spawn_watch(&root, move |msg| {
        let _ = watch_tx.send(msg);
    })
    .expect("watch must start against a real, writable scratch root");
    assert!(watch.wait_until_watching(ARRIVAL), "the walk must finish");

    // coarse filesystem mtime resolution can otherwise leave the fixture's
    // own write and the out-of-band one that follows inside the same clock
    // tick, which nvim's own check cannot tell apart
    std::thread::sleep(Duration::from_millis(1100));

    Watched {
        engine,
        engine_rx,
        watch,
        watch_rx,
        path,
        root,
        _dir: dir,
    }
}

impl Watched {
    /// The batch the watch raised for [`Self::path`], spelled the way the
    /// watch spelled it -- never re-derived here, so what reaches nvim is
    /// literally what the detection nominated.
    fn nominated(&self) -> Vec<String> {
        let want: &Path = &self.path;
        common::drain_until(&self.watch_rx, ARRIVAL, |msg| match msg {
            Msg::ExternalWritesDetected { paths } if paths.iter().any(|p| p == want) => {
                Some(paths.clone())
            }
            _ => None,
        })
        .expect("the change must reach a batch naming it")
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
    }

    /// What the probe answers for [`Self::path`] in that batch.
    fn probed(&self, request_id: u64) -> CheckTimeOutcome {
        let names = self.nominated();
        self.engine
            .handle
            .checktime(request_id, &names, false)
            .expect("issue the probe");
        let results = common::drain_until(&self.engine_rx, ARRIVAL, |msg| match msg {
            Msg::CheckTimeReply {
                request_id: id,
                results,
            } if *id == request_id => Some(results.clone()),
            _ => None,
        })
        .expect("the probe must answer");
        results
            .iter()
            .find(|(path, _)| path == &self.path)
            .map(|(_, outcome)| *outcome)
            .expect("the answer is positional to the batch the watch sent")
    }
}

/// An atomic save, driven for real: a temp file written beside the target
/// and renamed over it, which is how editors, formatters and most agent
/// write tooling replace a file. The rename emits a removal, and the
/// nomination that follows it is answered by re-stat'ing the path -- which
/// finds an ordinary readable file, so nvim reloads the buffer itself and
/// the user is told nothing, exactly as before removals were forwarded.
///
/// The falsifiable half of "forwarding removals does not cry wolf": a
/// `FileGone` here would be view announcing a vanished file every time an
/// agent saved one.
#[test]
fn an_atomic_save_over_a_watched_file_reloads_rather_than_reporting_it_gone() {
    let case = watched("extwrite-atomic");
    let temp = case.root.join("watched.txt.tmp");
    std::fs::write(&temp, "changed-externally\n").expect("write the temp file");
    std::fs::rename(&temp, &case.path).expect("rename it over the target");

    assert_eq!(
        case.probed(1),
        CheckTimeOutcome::HandledSilently,
        "a save that put the file back is a file the probe can still read"
    );
}

/// The shape the removal nomination exists for, driven end to end: an agent
/// removes a file the user has open. No create and no modify follows it, so
/// before removals were forwarded this produced no message of any kind and
/// the user was told nothing until something unrelated touched the path.
#[test]
fn a_removed_watched_file_reaches_the_probe_and_answers_gone() {
    let case = watched("extwrite-removed");
    std::fs::remove_file(&case.path).expect("remove the file out of band");

    assert_eq!(
        case.probed(2),
        CheckTimeOutcome::FileGone { modified: false },
        "the buffer is holding what it last read off a path that no longer answers"
    );
}
