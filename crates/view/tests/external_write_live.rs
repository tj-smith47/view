//! End-to-end proof of out-of-band write detection across both halves that
//! own it: `view_ai`'s filesystem watch, which nominates a path off a real
//! filesystem event, and a live nvim answering `RpcCall::Checktime` for
//! that same path. Neither crate can see the other -- the watch is
//! dependency-wise blind to the engine -- so a test in either one can only
//! pin its own half, and the question this file exists for lives exactly in
//! the seam.
//!
//! That question is whether nominating removals cries wolf. Atomic-save
//! tooling writes a temp file and renames it over the target, which raises
//! no removal at all -- the rename arrives as a move-in, a shape the watch
//! already nominated as a write. The save shape that does raise one is the
//! unlink-then-rewrite (`rm f && cat > f`, a generator clearing its output
//! before regenerating it), and its nomination really can reach the probe
//! while the path is still absent: the probe then answers `gone`, truly and
//! uselessly, about a file that is on its way back.
//!
//! What keeps that from reaching the user is the fold, not luck. A first
//! `gone` answer is not announced; it schedules a re-probe one grace period
//! later (`view_ai::FILE_GONE_GRACE`), and only a path still unreadable
//! then is spoken about. Both halves are driven here for real -- the three
//! save shapes against a live nvim (`rename` over the target, an unlink
//! followed by a rewrite, a plain `remove_file`) and, through `update()`
//! itself, what the user is told for the two that matter.
//! `docs/checktime-wire-capture.md`'s inotify section records what each of
//! the three raises at the kernel level.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use view_ai::WatchHandle;
use view_core::model::Model;
use view_core::msg::{CheckTimeOutcome, Effect, Msg, RpcCall};
use view_core::update::update;
use view_engine::process::{Engine, EngineConfig};
use view_test_support::{settle_mtime, ScratchDir};

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
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .expect("attach ui");
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

    settle_mtime();

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

    /// What the probe answers for [`Self::path`] in the batch the watch
    /// raised for it.
    fn probed(&self, request_id: u64) -> CheckTimeOutcome {
        self.probe(request_id, &self.nominated())
    }

    /// [`Self::probed`], with the nomination taken separately -- for a case
    /// that has to touch the file again between the detection and the probe,
    /// which is the whole shape of a save whose two halves land in different
    /// detection windows.
    fn probe(&self, request_id: u64, names: &[String]) -> CheckTimeOutcome {
        match self.reply(request_id, names) {
            Msg::CheckTimeReply { results, .. } => results
                .iter()
                .find(|(path, _)| path == &self.path)
                .map(|(_, outcome)| *outcome)
                .expect("the answer is positional to the batch the watch sent"),
            other => panic!("expected a checktime reply, got {other:?}"),
        }
    }

    /// The probe's whole reply, for a case that folds it through `update()`
    /// rather than reading one outcome out of it -- what the user is told
    /// is a property of the fold, not of any single outcome.
    fn reply(&self, request_id: u64, names: &[String]) -> Msg {
        self.engine
            .handle
            .checktime(request_id, names, false)
            .expect("issue the probe");
        common::drain_until(&self.engine_rx, ARRIVAL, |msg| match msg {
            Msg::CheckTimeReply { request_id: id, .. } if *id == request_id => Some(msg.clone()),
            _ => None,
        })
        .expect("the probe must answer")
    }

    /// The shell's own half of the confirmation, driven for real: the
    /// second look the fold asked for is taken against the live pair, and
    /// the reply it drove -- the one answer the fold will accept as a
    /// confirmation -- is handed back to be folded in.
    ///
    /// The probe is issued with the id and the spelling `update()` itself
    /// minted, never with one made up here: correlating on that id is what
    /// keeps an answer nobody asked for from confirming a removal.
    fn second_look(&self, model: &mut Model) -> Msg {
        let asked = update(
            model,
            Msg::ConfirmExternalRemoval {
                path: self.path.clone(),
            },
        );
        match asked.as_slice() {
            [Effect::Rpc(RpcCall::Checktime {
                request_id,
                paths,
                force: false,
            })] => self.reply(*request_id, paths),
            other => panic!("expected the second look at the path, got {other:?}"),
        }
    }

    /// What the buffer is holding, straight out of the live nvim: the only
    /// thing that tells a silent reload apart from a check that found
    /// nothing to do, since both answer
    /// [`CheckTimeOutcome::HandledSilently`].
    fn lines(&self) -> Vec<String> {
        let name = self.path.to_string_lossy().into_owned();
        self.engine
            .handle
            .preview_buffer(&name, 1)
            .expect("issue the buffer read");
        common::drain_until(&self.engine_rx, ARRIVAL, |msg| match msg {
            Msg::PickerPreviewReply { loaded, lines, .. } => {
                assert!(loaded, "the buffer this case loaded is still open");
                Some(lines.clone())
            }
            _ => None,
        })
        .expect("the read must answer")
    }
}

/// An atomic save, driven for real: a temp file written beside the target
/// and renamed over it, which is how editors, formatters and most agent
/// write tooling replace a file. The rename raises no removal -- it arrives
/// as a move-in, which the watch nominates as the write it is -- and the
/// probe re-stats the path, finds an ordinary readable file, and leaves nvim
/// to reload the buffer itself with nothing said to the user.
///
/// Removal forwarding is not what this row turns on, then. It is here
/// because the commonest save shape there is must never reach `FileGone`,
/// whichever arm of the watch's filter nominated it.
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
    assert_eq!(
        case.lines(),
        vec!["changed-externally".to_string()],
        "silence is only right if the buffer actually took the new content"
    );
}

/// The save shape that does raise a removal: the target is unlinked and then
/// written again, which is what a generator clearing its output before
/// regenerating it does. Driven with the two halves in different detection
/// windows -- the nomination is taken while the path is genuinely absent,
/// and the file is put back before the probe runs -- because that is the
/// only ordering in which the removal reaches the probe as its own
/// nomination rather than riding along with the create that follows it.
///
/// The favourable ordering of the two, and the reason removals can be
/// forwarded at all: a nomination naming a path that really was gone when
/// the watch saw it still answers `HandledSilently`, because the probe
/// re-stats every path it is handed. The ordering where the probe wins the
/// race instead -- answering `gone` about a file on its way back -- is what
/// `a_save_that_unlinks_before_rewriting_never_says_anything` drives, and
/// there the fold's own confirmation is what keeps it off the screen.
#[test]
fn a_save_that_unlinks_before_rewriting_reloads_rather_than_reporting_it_gone() {
    let case = watched("extwrite-unlink-rewrite");
    std::fs::remove_file(&case.path).expect("unlink the target out of band");
    let nominated = case.nominated();
    std::fs::write(&case.path, "changed-externally\n").expect("write the target again");

    assert_eq!(
        case.probe(3, &nominated),
        CheckTimeOutcome::HandledSilently,
        "the path the watch nominated is readable again by the time it is probed"
    );
    assert_eq!(
        case.lines(),
        vec!["changed-externally".to_string()],
        "silence is only right if the buffer actually took the new content"
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

/// The whole point of confirming a `gone` answer, driven end to end: an
/// ordinary save whose unlink is nominated on its own, whose rewrite lands
/// inside the grace period, and which therefore says nothing to the user at
/// all -- not a notice, and not a notice retracted a moment later, which is
/// still a notice their eye caught.
#[test]
fn a_save_that_unlinks_before_rewriting_never_says_anything() {
    let case = watched("extwrite-grace-save");
    let mut model = Model::new();
    std::fs::remove_file(&case.path).expect("unlink the target out of band");
    let nominated = case.nominated();

    let effects = update(&mut model, case.reply(4, &nominated));
    assert!(
        matches!(effects.as_slice(), [Effect::ReprobeExternalWrite { .. }]),
        "the answer taken while the path is absent asks for a second stat, got {effects:?}"
    );
    assert!(
        model.engine.messages.entries.is_empty(),
        "and says nothing meanwhile: {:?}",
        model.engine.messages.entries
    );

    // on disk before the second look by construction, never raced against
    // it with sleeps a loaded host reorders; the grace's size and its being
    // spent are pinned in `the_grace_outlasts_the_absence_a_save_can_leave`
    // and `the_re_probe_waits_out_the_save_before_looking_again`
    std::fs::write(&case.path, "changed-externally\n").expect("write the target again");
    let confirming = case.second_look(&mut model);
    let _ = update(&mut model, confirming);

    assert!(
        model.engine.messages.entries.is_empty(),
        "an ordinary save must never reach the user at all: {:?}",
        model.engine.messages.entries
    );
    assert_eq!(
        case.lines(),
        vec!["changed-externally".to_string()],
        "silence is only right if the buffer actually took the new content"
    );
}

/// The other side of the same grace: a file an agent really removed is
/// still unreadable when the re-probe stats it, and that is what the user
/// is told. Confirming delays the notice; it must not lose it.
///
/// The notice's own end is driven here too, off the same live pair: a
/// removal the agent undoes minutes later leaves a notice asserting
/// something that has stopped being true, and the answer that reads the
/// path again is what takes it down.
#[test]
fn a_removed_watched_file_is_announced_once_the_grace_confirms_it() {
    let case = watched("extwrite-grace-removed");
    let mut model = Model::new();
    std::fs::remove_file(&case.path).expect("remove the file out of band");
    let nominated = case.nominated();

    let _ = update(&mut model, case.reply(6, &nominated));
    assert!(
        model.engine.messages.entries.is_empty(),
        "nothing is said on the strength of one answer: {:?}",
        model.engine.messages.entries
    );

    let confirming = case.second_look(&mut model);
    let _ = update(&mut model, confirming);

    let entry = model
        .engine
        .messages
        .entries
        .last()
        .expect("a file that stayed gone must leave the user a notice");
    let text: String = entry.content.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(
        text,
        format!(
            "view: file {} is no longer a readable file on disk -- nothing \
             was reloaded, and the buffer still holds the content it last read",
            case.path.display()
        )
    );

    std::fs::write(&case.path, "restored\n").expect("put the file back");
    let _ = update(&mut model, case.reply(8, &nominated));
    assert!(
        model.engine.messages.entries.is_empty(),
        "an answer that read the path takes the notice down: {:?}",
        model.engine.messages.entries
    );
}
