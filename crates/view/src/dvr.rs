//! The session DVR on the loop thread: records each frame the terminal was
//! sent, and paints the recorded frame the scrub shows, or a frame of a clip
//! played back, in place of the live screen.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use view_core::model::{Model, OverlayKind};
use view_core::msg::{Effect, Msg};
use view_core::native::dvr::{
    BranchPlan, DvrIoReply, DvrRequest, ExportRefusal, CLIP_HINT, SCRUB_HINT,
};
use view_proc::writer::BackgroundWriter;
use view_tui::dvr::FrameRing;
use view_tui::terminal::Term;

use crate::wake::LoopSender;
use io::{Export, IoJob};

pub(crate) mod branch;
mod clip;
mod io;

/// What view says when the ring could not keep a frame, once until it keeps
/// one again.
const REFUSED: &str = "view: DVR is skipping frames it has no room for: raise [dvr] max_mb";

/// The painted frames and the clock they are dated by.
pub(crate) struct DvrLoop {
    ring: FrameRing,
    started: Instant,
    refused_told: bool,
    /// The recorded frame on screen, the size it was painted at, whether
    /// its bar said something waits and whether the branch confirm was
    /// painted over it, so a pass that changed none of them writes nothing.
    painted: Option<(u64, (u16, u16), bool, bool)>,
    /// The thread doing the file work, `None` when the host refused it.
    io: Option<BackgroundWriter<IoJob, Infallible>>,
    /// Jobs a full queue handed back, sent again in order on the next poll.
    unsent: VecDeque<IoJob>,
    /// Shared with the export being written, which holds a snapshot of the
    /// ring. The ring records no new group past its budget until it is
    /// written, so a frame skipped meanwhile is told nothing.
    exporting: Arc<()>,
    /// The last export queued.
    pending: Option<Pending>,
    /// A confirmed branch the loop has yet to carry out.
    branch: Option<BranchPlan>,
    /// The recording, parked while `ring` holds a clip.
    live: Option<FrameRing>,
    /// The clips the file thread read, each ahead of its reply.
    clips: Receiver<FrameRing>,
}

/// An export handed to the file thread: where it is written, the flag that
/// cancels it, and the signal it sends once published or failed.
struct Pending {
    path: PathBuf,
    cancel: Arc<AtomicBool>,
    done: Receiver<()>,
}

impl Pending {
    /// Cancels the export and removes its part file. Either the writer
    /// created the part before the flag was set, and it is removed here, or
    /// the writer sees the flag after creating it and removes it itself.
    fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(io::part_path(&self.path));
    }
}

/// How long a quit waits for a clip still being written. A clip of a full
/// recording is tens of megabytes, which a local disk takes well inside it.
const EXPORT_QUIT_WAIT: Duration = Duration::from_secs(2);

impl DvrLoop {
    /// The loop's recorder, or `None` when the session is not recorded.
    /// Replies from its file thread arrive on `msg`.
    pub(crate) fn start(model: &Model, msg: LoopSender) -> Option<Self> {
        if !model.dvr.is_recording() {
            return None;
        }
        let (clips_tx, clips) = std::sync::mpsc::channel();
        let io = io::start(msg, model.remote.is_some(), clips_tx)
            .inspect_err(|e| crate::vlog::log_with("dvr", || format!("no file thread: {e}")))
            .ok();
        Some(Self::with(FrameRing::new(model.dvr.max_bytes()), io, clips))
    }

    fn with(
        ring: FrameRing,
        io: Option<BackgroundWriter<IoJob, Infallible>>,
        clips: Receiver<FrameRing>,
    ) -> Self {
        Self {
            ring,
            started: Instant::now(),
            refused_told: false,
            painted: None,
            io,
            unsent: VecDeque::new(),
            exporting: Arc::new(()),
            pending: None,
            branch: None,
            live: None,
            clips,
        }
    }

    /// The branch the person confirmed, once. The export in flight is left
    /// to finish: it writes its own snapshot of the frames on the file
    /// thread and publishes whole or not at all.
    pub(crate) fn take_branch(&mut self) -> Option<BranchPlan> {
        self.branch.take()
    }

    /// Closes the file thread. Waits up to [`EXPORT_QUIT_WAIT`] while a clip
    /// is being written, and nothing otherwise. Returns the line to print
    /// once the terminal is restored when that clip is still unfinished,
    /// its part file removed.
    pub(crate) fn finish(self) -> Option<String> {
        self.finish_within(EXPORT_QUIT_WAIT)
    }

    /// [`Self::finish`], waiting up to `wait` for the clip alone and never
    /// for file work queued behind it.
    fn finish_within(mut self, wait: Duration) -> Option<String> {
        let _ = self.io.take()?.close();
        let pending = self.pending.take()?;
        if Arc::strong_count(&self.exporting) == 1 || pending.done.recv_timeout(wait).is_ok() {
            return None;
        }
        pending.cancel();
        // the publish can land between the wait and the cancel
        if pending.path.exists() {
            return None;
        }
        Some(format!(
            "view: DVR clip {} was not finished when view quit",
            pending.path.display()
        ))
    }

    /// Resolves the scrub moves the keys asked for against the ring, and
    /// hands the recording's requests to its file thread without waiting
    /// on it. Returns what an export refused on the spot raised.
    pub(crate) fn poll(&mut self, model: &mut Model) -> Vec<Effect> {
        let mut effects = Vec::new();
        // an export queued ahead of a clip's opening copies the recording
        while let Some(request) = model.dvr.take_request() {
            match request {
                DvrRequest::Baseline(path) => match self.unsent.back_mut() {
                    Some(IoJob::Baseline(paths)) => paths.push(path),
                    _ => self.unsent.push_back(IoJob::Baseline(vec![path])),
                },
                DvrRequest::DiskCheck => self.unsent.push_back(IoJob::DiskCheck),
                DvrRequest::Export(path) => {
                    if let Err(why) = self.export(model, path) {
                        let reply = Msg::DvrIo(DvrIoReply::Refused(why));
                        effects.extend(view_core::update::update(model, reply));
                    }
                }
                DvrRequest::Branch(plan) => self.branch = Some(plan),
                DvrRequest::Play(path) => self.unsent.push_back(IoJob::Play {
                    path: model.cwd.join(path),
                    max_bytes: model.dvr.max_bytes(),
                }),
                _ => {}
            }
        }
        self.swap_clip(model);
        while let Some(step) = model.dvr.take_step() {
            if let Some(from) = model.dvr.scrub_frame() {
                model.dvr.show(self.ring.resolve(from, step));
            }
        }
        for reply in self.send_unsent() {
            effects.extend(view_core::update::update(model, Msg::DvrIo(reply)));
        }
        effects
    }

    /// Shows the clip just opened in place of the recording, which is
    /// parked, and brings the recording back once the clip is closed.
    fn swap_clip(&mut self, model: &mut Model) {
        if model.dvr.take_opened_clip() {
            if let Ok(clip) = self.clips.try_recv() {
                let shown = std::mem::replace(&mut self.ring, clip);
                self.live.get_or_insert(shown);
                self.painted = None;
            }
        }
        if model.dvr.clip().is_none() {
            if let Some(live) = self.live.take() {
                self.ring = live;
                self.painted = None;
            }
        }
    }

    /// Hands the queued jobs to the file thread until its queue is full.
    /// Returns the replies owed on the spot for jobs no thread can take: a
    /// disk check is answered unverifiable and a clip read refused.
    fn send_unsent(&mut self) -> Vec<DvrIoReply> {
        let mut owed = Vec::new();
        while let Some(job) = self.unsent.pop_front() {
            let job = match self.io.as_mut() {
                Some(io) => match io.try_send(job) {
                    Ok(()) => continue,
                    Err(TrySendError::Full(job)) => {
                        self.unsent.push_front(job);
                        break;
                    }
                    Err(TrySendError::Disconnected(job)) => job,
                },
                None => job,
            };
            match job {
                IoJob::DiskCheck => owed.push(DvrIoReply::DiskChecked {
                    changed: Vec::new(),
                    unverifiable: true,
                }),
                IoJob::Play { .. } => owed.push(DvrIoReply::Failed {
                    verb: "play",
                    reason: "its file thread is not running".to_owned(),
                }),
                _ => {}
            }
        }
        owed
    }

    /// Queues the export of the recording to `path`, which nvim made
    /// absolute, or to a derived name in the directory view was started in.
    /// The ring's groups are shared with the job, its open group copied,
    /// and the input log copied once.
    fn export(&mut self, model: &Model, path: Option<String>) -> Result<(), ExportRefusal> {
        if Arc::strong_count(&self.exporting) > 1 {
            return Err(ExportRefusal::Busy);
        }
        let Some(io) = self.io.as_mut().filter(|io| io.is_open()) else {
            return Err(ExportRefusal::NoWriter);
        };
        if self.ring.newest().is_none() {
            return Err(ExportRefusal::NoFrame);
        }
        let frames = self.ring.snapshot().ok_or(ExportRefusal::OverBudget)?;
        let name = path.map_or_else(
            || {
                let secs = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                PathBuf::from(format!("view-dvr-{secs}.vdvr"))
            },
            PathBuf::from,
        );
        let path = model.cwd.join(name);
        let cancel = Arc::new(AtomicBool::new(false));
        let (done_tx, done) = std::sync::mpsc::sync_channel(1);
        let job = IoJob::Export(Export {
            path: path.clone(),
            frames,
            inputs: clip::Inputs::new(model.dvr.inputs()),
            markers: model.dvr.markers().to_vec(),
            dead: model.dvr.dead().to_vec(),
            held: Arc::clone(&self.exporting),
            cancel: Arc::clone(&cancel),
            done: done_tx,
        });
        match io.try_send(job) {
            Ok(()) => {
                self.pending = Some(Pending { path, cancel, done });
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(ExportRefusal::Queued),
            Err(TrySendError::Disconnected(_)) => Err(ExportRefusal::NoWriter),
        }
    }

    /// Settles the model for one scrub pass and names the recorded frame
    /// and bar to paint. `None` on the live screen, `Some(None)` when the
    /// frame on screen is still the one to show.
    ///
    /// The pass clears `dirty` and marks no question seen: the terminal
    /// shows a recorded frame, so a prompt that opened meanwhile reads keys
    /// only once a frame carrying it is painted. The branch confirm is the
    /// one question a recorded frame carries.
    fn scrub_pass(&mut self, model: &mut Model) -> Option<Option<(u64, String)>> {
        let Some(seq) = model.dvr.scrub_frame() else {
            self.painted = None;
            return None;
        };
        model.dirty = false;
        // the live screen's damage is spent here, since the frame that
        // closes the scrub repaints everything
        let _ = model.take_paint_damage();
        let waits = waiting(model);
        let confirming = model.branch_confirm_focused();
        let shown = (
            seq,
            (model.term_width, model.term_height),
            waits,
            confirming,
        );
        if self.painted == Some(shown) {
            return Some(None);
        }
        if self.painted.is_none() {
            crate::vlog::log_with("dvr", || {
                let (oldest, newest) = (self.ring.oldest(), self.ring.newest());
                let frames = newest.zip(oldest).map_or(0, |(n, o)| n - o + 1);
                let reach = oldest.and_then(|o| self.ring.age(o)).unwrap_or_default();
                format!(
                    "scrub frames={frames} held={} reach={:.1}s",
                    self.ring.held(),
                    reach.as_secs_f64()
                )
            });
        }
        self.painted = Some(shown);
        let clip = model
            .dvr
            .clip()
            .map(|path| (path, model.dvr.clip_left_out()));
        let bar = self.bar(seq, waits, clip, confirming);
        Some(Some((seq, bar)))
    }

    /// Records the frame the live paint just wrote. Returns the notice owed
    /// the first time the ring could not keep one.
    pub(crate) fn after_paint(&mut self, term: &Term, model: &mut Model) -> Vec<Effect> {
        let seq = term.record_frame(&mut self.ring, self.started.elapsed());
        crate::vlog::log_with("dvr", || format!("frame {}", self.ring.last_capture()));
        self.recorded(seq, model)
    }

    /// Notes a frame the ring kept, or raises the one-time notice when it
    /// kept none.
    fn recorded(&mut self, seq: Option<u64>, model: &mut Model) -> Vec<Effect> {
        match seq {
            Some(seq) => {
                self.refused_told = false;
                model.dvr.note_frame(seq, self.ring.oldest().unwrap_or(seq));
                Vec::new()
            }
            None if !self.refused_told && Arc::strong_count(&self.exporting) == 1 => {
                self.refused_told = true;
                model.dirty = true;
                model
                    .engine
                    .record_native_notice(REFUSED.to_string(), false)
            }
            None => Vec::new(),
        }
    }

    /// The scrub bar for frame `seq`: the clip's file name when `clip`
    /// names one, how far back the frame is and how far back the frames
    /// reach, the way out and the keys, then how many of the clip's oldest
    /// frames were left out.
    fn bar(
        &self,
        seq: u64,
        waiting: bool,
        clip: Option<(&str, usize)>,
        confirming: bool,
    ) -> String {
        let secs = |s| self.ring.age(s).unwrap_or_default().as_secs_f64();
        let age = secs(seq);
        let reach = secs(self.ring.oldest().unwrap_or(seq));
        let flag = if waiting { WAITING } else { "" };
        let (head, hint, cut) = match clip {
            Some((path, left_out)) => {
                let name = std::path::Path::new(path)
                    .file_name()
                    .map_or_else(|| path.into(), |n| n.to_string_lossy());
                let cut = match left_out {
                    0 => String::new(),
                    n => format!("  ({n} oldest frames past [dvr] max_mb not loaded)"),
                };
                (format!("CLIP {name}"), CLIP_HINT, cut)
            }
            None => ("DVR".to_owned(), SCRUB_HINT, String::new()),
        };
        if confirming {
            // the confirm over the frame names its own keys
            return format!("{head}  -{age:.1}s of {reach:.1}s{flag}{cut}");
        }
        // the flag and the legend go ahead of the count, since a narrow
        // terminal cuts the bar's end
        format!("{head}  -{age:.1}s of {reach:.1}s{flag}  {hint}{cut}")
    }
}

/// A loop dropped without [`DvrLoop::finish`], on an error or a panic,
/// cancels the clip in flight and removes its part file without waiting.
impl Drop for DvrLoop {
    fn drop(&mut self) {
        let in_flight = Arc::strong_count(&self.exporting) > 1;
        if let Some(pending) = self.pending.as_ref().filter(|_| in_flight) {
            pending.cancel();
        }
    }
}

/// What a paint pass drew, and whether it wrote bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Painted {
    Recorded(bool),
    Live(bool),
}

impl Painted {
    /// Whether the pass wrote bytes to the terminal.
    pub(crate) fn wrote(self) -> bool {
        matches!(self, Self::Recorded(true) | Self::Live(true))
    }
}

/// Runs one paint pass on `target`: `recorded` paints the frame the scrub
/// shows, with its bar, while the scrub is open, and `live` paints the live
/// screen otherwise.
///
/// Only a live frame settles the model through
/// [`crate::runtime::frame_reached_terminal`]. A recorded frame does not
/// carry the live screen, so a question that opened under the scrub reads
/// keys once a live frame showing it is painted, and `recorded` sees the
/// model read-only. The branch confirm is painted over the recorded frame,
/// and reads keys once that frame is written, or once a pass finds the
/// frame gone from the ring.
pub(crate) fn paint_pass<T>(
    model: &mut Model,
    dvr: Option<&mut DvrLoop>,
    target: &mut T,
    live: impl FnOnce(&mut T, &mut Model) -> std::io::Result<bool>,
    recorded: impl FnOnce(&mut T, &Model, &FrameRing, u64, &str) -> std::io::Result<bool>,
) -> std::io::Result<Painted> {
    if let Some(dvr) = dvr {
        match dvr.scrub_pass(model) {
            Some(None) => return Ok(Painted::Recorded(false)),
            Some(Some((seq, bar))) => {
                let wrote = recorded(target, model, &dvr.ring, seq, &bar)?;
                // a frame the ring no longer keeps paints nothing, and a
                // confirm left unshown would read no key, `<Esc>` included
                model.note_recorded_frame_painted();
                return Ok(Painted::Recorded(wrote));
            }
            None => {}
        }
    }
    let wrote = live(target, model)?;
    crate::runtime::frame_reached_terminal(model);
    Ok(Painted::Live(wrote))
}

/// What the bar adds while something on the live screen waits for an answer.
const WAITING: &str = "  ! waiting: q to answer";

/// Whether the live screen holds a question or a recovery offer the person
/// has not seen while the scrub covers it.
fn waiting(model: &Model) -> bool {
    model.ai_panel().pending_permission.is_some()
        || model.overlays().iter().any(|o| match &o.kind {
            // the branch confirm is painted over the recorded frame
            OverlayKind::Prompt(p) => p.dvr_branch_at().is_none(),
            OverlayKind::EngineBusy(_) => true,
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use view_core::events::UiEvent;
    use view_core::grid::GridOp;
    use view_core::msg::{Key, Msg, RpcCall};
    use view_core::update::update;
    use view_proc::writer::QUIT_WAIT;
    use view_tui::dvr::RingBuilder;

    use super::*;

    const MAX: usize = 1 << 20;

    /// A loop over six frames painted 400 ms apart.
    fn recorded(model: &mut Model) -> DvrLoop {
        model.dvr.enable(MAX);
        let mut builder = RingBuilder::new(MAX);
        for at in 0..6u64 {
            builder
                .push_key(at * 400_000, (4, 2), None, std::iter::empty())
                .unwrap();
        }
        model.dvr.note_frame(6, 1);
        DvrLoop::with(builder.finish(), None, std::sync::mpsc::channel().1)
    }

    fn press(model: &mut Model, dvr: &mut DvrLoop, notation: &str) -> Option<u64> {
        let _ = update(
            model,
            Msg::Key(Key {
                notation: notation.to_owned(),
            }),
        );
        dvr.poll(model);
        model.dvr.scrub_frame()
    }

    #[test]
    fn scrub_opens_on_the_newest_frame_and_steps_through_the_ring() {
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        let _ = update(
            &mut model,
            Msg::FeatureInvoke {
                generation: None,
                feature: "dvr".to_owned(),
                verb: "scrub".to_owned(),
            },
        );
        dvr.poll(&mut model);
        assert_eq!(model.dvr.scrub_frame(), Some(6));
        assert_eq!(press(&mut model, &mut dvr, "h"), Some(5));
        assert_eq!(
            dvr.bar(5, false, None, false),
            "DVR  -0.4s of 2.0s  q close  h/l frame  H/L 1s  g/G ends  b branch  e export"
        );
        assert_eq!(press(&mut model, &mut dvr, "H"), Some(2));
        assert_eq!(press(&mut model, &mut dvr, "g"), Some(1));
        assert_eq!(press(&mut model, &mut dvr, "h"), Some(1));
        assert_eq!(press(&mut model, &mut dvr, "L"), Some(4));
        assert_eq!(press(&mut model, &mut dvr, "G"), Some(6));
        assert_eq!(press(&mut model, &mut dvr, "l"), Some(6));
        assert_eq!(press(&mut model, &mut dvr, "q"), None);
    }

    #[test]
    fn a_session_with_the_dvr_off_runs_no_recorder() {
        let (tx, _rx) = std::sync::mpsc::sync_channel(1);
        let off = Model::with_term_size(80, 24);
        assert!(DvrLoop::start(&off, LoopSender::new(tx)).is_none());
    }

    fn open_scrub(model: &mut Model, dvr: &mut DvrLoop) {
        let _ = update(
            model,
            Msg::FeatureInvoke {
                generation: None,
                feature: "dvr".to_owned(),
                verb: "scrub".to_owned(),
            },
        );
        dvr.poll(model);
    }

    fn answers_l(effects: &[Effect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, Effect::Rpc(RpcCall::Input { notation }) if notation == "l"))
    }

    #[test]
    fn a_prompt_that_opens_while_scrubbing_waits_for_a_live_frame() {
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        open_scrub(&mut model, &mut dvr);
        let mut bar = String::new();
        let pass = |model: &mut Model, dvr: &mut DvrLoop, bar: &mut String| {
            paint_pass(
                model,
                Some(dvr),
                bar,
                |_, _| Ok(true),
                |bar, _, _, _, shown| {
                    shown.clone_into(bar);
                    Ok(true)
                },
            )
            .unwrap()
        };
        model.dirty = true;
        assert_eq!(
            pass(&mut model, &mut dvr, &mut bar),
            Painted::Recorded(true)
        );
        let _ = update(
            &mut model,
            Msg::Redraw(vec![
                UiEvent::MsgShow {
                    kind: "confirm".into(),
                    content: vec![(0, "W12: Warning: File \"a.rs\" has changed".into())],
                    replace_last: false,
                },
                UiEvent::Flush,
            ]),
        );
        let _ = update(
            &mut model,
            Msg::Redraw(vec![
                UiEvent::CmdlineShow {
                    content: vec![],
                    pos: 0,
                    firstc: String::new(),
                    prompt: "[O]K, (L)oad File: ".into(),
                    indent: 0,
                    level: 1,
                },
                UiEvent::Flush,
            ]),
        );
        assert_eq!(
            pass(&mut model, &mut dvr, &mut bar),
            Painted::Recorded(true)
        );
        assert!(
            bar.find(WAITING).is_some_and(|at| at + WAITING.len() <= 80),
            "the bar is repainted to say a prompt waits, inside 80 columns: {bar}"
        );
        assert!(!model.dirty);

        // `q` then `l` in one drained batch, before any live frame
        let mut effects = update(&mut model, key("q"));
        effects.extend(update(&mut model, key("l")));
        assert!(!answers_l(&effects), "{effects:?}");

        model.dirty = true;
        assert_eq!(pass(&mut model, &mut dvr, &mut bar), Painted::Live(true));
        let answer = update(&mut model, key("l"));
        assert!(answers_l(&answer), "{answer:?}");
    }

    #[test]
    fn the_branch_confirm_is_painted_over_the_frame_it_names_and_answered_there() {
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        open_scrub(&mut model, &mut dvr);
        let _ = press(&mut model, &mut dvr, "h");
        let _ = press(&mut model, &mut dvr, "b");
        let checked = DvrIoReply::DiskChecked {
            changed: Vec::new(),
            unverifiable: false,
        };
        let _ = update(&mut model, Msg::DvrIo(checked));
        assert!(model.branch_confirm_focused());
        let _ = update(&mut model, key("y"));
        assert!(
            !model.dvr.is_branching(),
            "a key before the confirm is painted"
        );
        let mut painted = None;
        let pass = paint_pass(
            &mut model,
            Some(&mut dvr),
            &mut painted,
            |_, _| Ok(true),
            |painted, _, _, seq, bar| {
                *painted = Some((seq, bar.to_owned()));
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(pass, Painted::Recorded(true));
        let (seq, bar) = painted.unwrap();
        assert_eq!(seq, 5, "the frame b was pressed on");
        assert_eq!(bar, "DVR  -0.4s of 2.0s", "the confirm names its own keys");
        let _ = update(&mut model, key("y"));
        assert!(model.dvr.is_branching());
        assert!(model.dvr.scrub_frame().is_none());
    }

    #[test]
    fn a_branch_confirm_over_a_frame_no_longer_kept_still_answers_esc() {
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        open_scrub(&mut model, &mut dvr);
        let _ = press(&mut model, &mut dvr, "b");
        let checked = DvrIoReply::DiskChecked {
            changed: Vec::new(),
            unverifiable: false,
        };
        let _ = update(&mut model, Msg::DvrIo(checked));
        let pass = paint_pass(
            &mut model,
            Some(&mut dvr),
            &mut (),
            |(), _| Ok(true),
            |(), _, _, _, _| Ok(false),
        )
        .unwrap();
        assert_eq!(pass, Painted::Recorded(false));
        let _ = update(&mut model, key("<Esc>"));
        assert!(!model.branch_confirm_focused(), "<Esc> answers the confirm");
        assert!(model.dvr.scrub_frame().is_none(), "and returns to live");
    }

    #[test]
    fn a_scrub_pass_paints_only_what_changed() {
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        model.dirty = true;
        assert_eq!(dvr.scrub_pass(&mut model), None, "the live screen");
        assert!(model.dirty, "a live pass settles nothing here");
        open_scrub(&mut model, &mut dvr);
        model.engine.apply_grid(GridOp::Resize {
            width: 80,
            height: 22,
        });
        assert!(model.take_paint_damage().full);
        model.engine.apply_grid(GridOp::PutLine {
            row: 2,
            col_start: 0,
            cells: vec![("x".into(), 0, 1)],
        });
        assert!(matches!(dvr.scrub_pass(&mut model), Some(Some((6, _)))));
        let spent = model.take_paint_damage();
        assert!(!spent.full && spent.rows.is_empty(), "{spent:?}");
        assert_eq!(dvr.scrub_pass(&mut model), Some(None), "cached");
        model.term_width = 100;
        assert!(matches!(dvr.scrub_pass(&mut model), Some(Some((6, _)))));
        let _ = press(&mut model, &mut dvr, "h");
        assert!(matches!(dvr.scrub_pass(&mut model), Some(Some((5, _)))));
        let _ = press(&mut model, &mut dvr, "q");
        assert_eq!(dvr.scrub_pass(&mut model), None);
        assert_eq!(dvr.painted, None);
    }

    #[test]
    fn a_refused_frame_is_told_once_and_a_kept_one_is_noted() {
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        assert!(dvr.recorded(Some(4), &mut model).is_empty());
        let _ = dvr.recorded(None, &mut model);
        let _ = dvr.recorded(None, &mut model);
        let raised = format!("{:?}", model.engine.messages.entries);
        assert_eq!(raised.matches("no room for").count(), 1, "{raised}");
        // a kept frame ends the run, and the next refusal is a new one
        let _ = dvr.recorded(Some(4), &mut model);
        let _ = dvr.recorded(None, &mut model);
        // a frame skipped while an export holds the ring is told nothing
        let _ = dvr.recorded(Some(4), &mut model);
        let export = Arc::clone(&dvr.exporting);
        let _ = dvr.recorded(None, &mut model);
        drop(export);
        let raised = format!("{:?}", model.engine.messages.entries);
        assert_eq!(raised.matches("no room for").count(), 2, "{raised}");
        // the frame noted is the one a scrub opens on, before any move
        let _ = update(
            &mut model,
            Msg::FeatureInvoke {
                generation: None,
                feature: "dvr".to_owned(),
                verb: "scrub".to_owned(),
            },
        );
        assert_eq!(model.dvr.scrub_frame(), Some(4));
    }

    fn key(notation: &str) -> Msg {
        Msg::Key(Key {
            notation: notation.to_owned(),
        })
    }

    fn export(model: &mut Model, dvr: &mut DvrLoop, path: &str) {
        let _ = update(
            model,
            Msg::FeatureInvoke {
                generation: None,
                feature: "dvr".to_owned(),
                verb: format!("export {path}"),
            },
        );
        let _ = dvr.poll(model);
    }

    /// Every message's text as the person reads it, one per line; a path
    /// in it stays as displayed, where `Debug` doubles a backslash.
    fn said(model: &Model) -> String {
        model
            .engine
            .messages
            .entries
            .iter()
            .map(|entry| {
                entry
                    .content()
                    .iter()
                    .map(|(_, text)| text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn told(model: &Model, words: &str) -> usize {
        said(model).matches(words).count()
    }

    #[test]
    fn export_never_blocks_the_loop_when_the_writer_is_full() {
        let _watchdog = view_test_support::watchdog();
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (gate, gate_rx) = std::sync::mpsc::channel::<()>();
        let mut io = BackgroundWriter::start("dvr-io-test", 1, move |_: IoJob| {
            let _ = entered_tx.send(());
            let _ = gate_rx.recv();
            Ok::<(), Infallible>(())
        })
        .unwrap();
        // the thread holds one job and the queue the next
        io.try_send(IoJob::DiskCheck).ok().unwrap();
        entered.recv().unwrap();
        io.try_send(IoJob::DiskCheck).ok().unwrap();
        dvr.io = Some(io);
        // a send that waited on the blocked thread would never return here
        export(&mut model, &mut dvr, "a.vdvr");
        assert_eq!(told(&model, "export busy: file work is queued"), 1);
        assert_eq!(
            Arc::strong_count(&dvr.exporting),
            1,
            "the refused job is dropped"
        );
        // a clip still being written refuses the next export in its own words
        let writing = Arc::clone(&dvr.exporting);
        export(&mut model, &mut dvr, "b.vdvr");
        assert_eq!(told(&model, "export busy: the last clip"), 1);
        assert_eq!(told(&model, "export busy"), 2);
        drop(writing);
        drop(gate);
    }

    #[test]
    fn a_quit_during_a_slow_write_leaves_no_file_and_says_so() {
        let _watchdog = view_test_support::watchdog();
        let dir = view_test_support::ScratchDir::new("dvr-quit-export").unwrap();
        let mut model = Model::with_term_size(80, 24);
        let (tx, _rx) = std::sync::mpsc::sync_channel(4);
        let mut idle = recorded(&mut model);
        idle.io =
            Some(io::start(LoopSender::new(tx), false, std::sync::mpsc::channel().0).unwrap());
        assert_eq!(idle.finish(), None, "no clip in flight, nothing to say");

        let mut dvr = recorded(&mut model);
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (gate, gate_rx) = std::sync::mpsc::channel::<()>();
        let writer = BackgroundWriter::start("dvr-io-test", 1, move |job: IoJob| {
            if let IoJob::Export(export) = job {
                // the clip half written when the process quits
                let _ = std::fs::write(io::part_path(&export.path), b"VIEWDVR\0");
                let _ = entered_tx.send(());
                let _ = gate_rx.recv();
            }
            Ok::<(), Infallible>(())
        })
        .unwrap();
        dvr.io = Some(writer);
        let path = dir.join("a.vdvr");
        export(&mut model, &mut dvr, &path.display().to_string());
        entered.recv().unwrap();
        assert_eq!(
            dvr.finish(),
            Some(format!(
                "view: DVR clip {} was not finished when view quit",
                path.display()
            ))
        );
        let left = io::listed(dir.path());
        assert!(left.is_empty(), "{left:?}");
        drop(gate);
    }

    /// A quit whose wait ends while the clip is still queued behind other
    /// file work leaves neither a part file nor a clip once the writer
    /// reaches it.
    #[test]
    fn an_export_queued_behind_slow_work_and_cancelled_by_a_quit_leaves_nothing() {
        let _watchdog = view_test_support::watchdog();
        let dir = view_test_support::ScratchDir::new("dvr-quit-queued").unwrap();
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (gate, gate_rx) = std::sync::mpsc::channel::<()>();
        let (wrote_tx, wrote) = std::sync::mpsc::channel();
        let writer = BackgroundWriter::start("dvr-io-test", 1, move |job: IoJob| {
            match job {
                IoJob::Export(export) => {
                    let _ = wrote_tx.send(io::write(export));
                }
                _ => {
                    let _ = entered_tx.send(());
                    let _ = gate_rx.recv();
                }
            }
            Ok::<(), Infallible>(())
        })
        .unwrap();
        dvr.io = Some(writer);
        dvr.unsent.push_back(IoJob::DiskCheck);
        assert!(dvr.send_unsent().is_empty());
        entered.recv().unwrap();
        let path = dir.join("a.vdvr");
        export(&mut model, &mut dvr, &path.display().to_string());
        assert!(dvr.finish_within(Duration::ZERO).is_some());
        drop(gate);
        let reply = wrote
            .recv_timeout(view_test_support::host_deadline(QUIT_WAIT))
            .unwrap();
        assert!(matches!(reply, DvrIoReply::Failed { .. }), "{reply:?}");
        let left = io::listed(dir.path());
        assert!(left.is_empty(), "{left:?}");
    }

    /// A quit once the clip is written returns without waiting on the file
    /// work queued behind it: a whole-queue wait blocks on the gate until
    /// the watchdog fails the test.
    #[test]
    fn a_quit_after_the_clip_is_written_never_waits_on_work_behind_it() {
        let _watchdog = view_test_support::watchdog();
        let dir = view_test_support::ScratchDir::new("dvr-quit-behind").unwrap();
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (gate, gate_rx) = std::sync::mpsc::channel::<()>();
        let writer = BackgroundWriter::start("dvr-io-test", 4, move |job: IoJob| {
            match job {
                IoJob::Export(export) => {
                    let _ = io::write(export);
                }
                _ => {
                    let _ = entered_tx.send(());
                    let _ = gate_rx.recv();
                }
            }
            Ok::<(), Infallible>(())
        })
        .unwrap();
        dvr.io = Some(writer);
        let path = dir.join("a.vdvr");
        export(&mut model, &mut dvr, &path.display().to_string());
        dvr.unsent.push_back(IoJob::DiskCheck);
        assert!(dvr.send_unsent().is_empty());
        // the thread reaches the disk check only once the clip is written
        entered.recv().unwrap();
        // no end to the wait, so only a quit that skips the queue returns
        assert_eq!(dvr.finish_within(Duration::MAX), None);
        assert!(path.is_file());
        drop(gate);
    }

    /// A loop dropped on an error or a panic, with no `finish`, cancels
    /// the clip in flight and removes its part file.
    #[test]
    fn a_loop_dropped_without_finish_cancels_its_clip_and_removes_the_part() {
        let _watchdog = view_test_support::watchdog();
        let dir = view_test_support::ScratchDir::new("dvr-drop-export").unwrap();
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (gate, gate_rx) = std::sync::mpsc::channel::<()>();
        let (seen_tx, seen) = std::sync::mpsc::channel();
        let writer = BackgroundWriter::start("dvr-io-test", 1, move |job: IoJob| {
            if let IoJob::Export(export) = job {
                let _ = std::fs::write(io::part_path(&export.path), b"VIEWDVR\0");
                let _ = entered_tx.send(());
                let _ = gate_rx.recv();
                let _ = seen_tx.send(export.cancel.load(Ordering::SeqCst));
            }
            Ok::<(), Infallible>(())
        })
        .unwrap();
        dvr.io = Some(writer);
        export(
            &mut model,
            &mut dvr,
            &dir.join("a.vdvr").display().to_string(),
        );
        entered.recv().unwrap();
        drop(dvr);
        let left = io::listed(dir.path());
        assert!(left.is_empty(), "{left:?}");
        drop(gate);
        let cancelled = seen
            .recv_timeout(view_test_support::host_deadline(QUIT_WAIT))
            .unwrap();
        assert!(cancelled, "the writer is told to stop");
    }

    #[test]
    fn a_disk_check_with_no_file_thread_is_answered_unverifiable() {
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        dvr.unsent
            .push_back(IoJob::Baseline(vec!["a.rs".to_owned()]));
        dvr.unsent.push_back(IoJob::DiskCheck);
        assert_eq!(
            dvr.send_unsent(),
            [DvrIoReply::DiskChecked {
                changed: Vec::new(),
                unverifiable: true,
            }]
        );
        assert!(dvr.unsent.is_empty());
    }

    #[test]
    fn an_export_with_no_file_thread_says_so() {
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        export(&mut model, &mut dvr, "a.vdvr");
        assert_eq!(told(&model, "file thread is not running"), 1);
    }

    #[test]
    fn an_export_from_the_loop_writes_the_clip_and_names_it() {
        let dir = view_test_support::ScratchDir::new("dvr-loop-export").unwrap();
        let mut model = Model::with_term_size(80, 24);
        model.cwd = dir.path().to_path_buf();
        let (mut dvr, rx) = wired(&mut model);
        export(&mut model, &mut dvr, "a.vdvr");
        let reply = rx
            .recv_timeout(view_test_support::host_deadline(QUIT_WAIT))
            .unwrap();
        assert!(
            matches!(reply, Msg::DvrIo(DvrIoReply::Exported { .. })),
            "{reply:?}"
        );
        let _ = update(&mut model, reply);
        assert_eq!(told(&model, "DVR clip written"), 1);
        let written = std::fs::read(dir.join("a.vdvr")).unwrap();
        assert_eq!(&written[..10], b"VIEWDVR\0\x01\x00");
        assert_eq!(
            Arc::strong_count(&dvr.exporting),
            1,
            "released once written"
        );
    }

    /// A branch confirmed while a clip is being written leaves the clip to
    /// the file thread, which writes the frames it was handed whole.
    #[test]
    fn a_branch_leaves_an_export_in_flight_to_finish() {
        let dir = view_test_support::ScratchDir::new("dvr-loop-branch-export").unwrap();
        let mut model = Model::with_term_size(80, 24);
        model.cwd = dir.path().to_path_buf();
        let mut dvr = recorded(&mut model);
        for notation in ["i", "a", "<Esc>"] {
            let key = Msg::Key(Key {
                notation: notation.to_owned(),
            });
            let _ = update(&mut model, key);
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        dvr.io = Some(io::start(LoopSender::new(tx), false, std::sync::mpsc::channel().0).unwrap());
        export(&mut model, &mut dvr, "a.vdvr");
        let scrub = Msg::FeatureInvoke {
            generation: None,
            feature: "dvr".to_owned(),
            verb: "scrub".to_owned(),
        };
        let _ = update(&mut model, scrub);
        let _ = press(&mut model, &mut dvr, "h");
        let _ = press(&mut model, &mut dvr, "b");
        let checked = Msg::DvrIo(DvrIoReply::DiskChecked {
            changed: Vec::new(),
            unverifiable: false,
        });
        let _ = update(&mut model, checked);
        model.note_frame_painted();
        let _ = update(
            &mut model,
            Msg::Key(Key {
                notation: "y".to_owned(),
            }),
        );
        let _ = dvr.poll(&mut model);
        let plan = dvr.take_branch().expect("the loop carries the branch");
        assert!(dvr.take_branch().is_none(), "once");
        model.dvr.branched(plan.at_frame, plan.replay);
        assert_eq!(model.dvr.inputs().count(), 0, "the branch cut the log");
        let reply = std::iter::from_fn(|| {
            rx.recv_timeout(view_test_support::host_deadline(QUIT_WAIT))
                .ok()
        })
        .find(|reply| !matches!(reply, Msg::DvrIo(DvrIoReply::DiskChecked { .. })))
        .unwrap();
        assert!(
            matches!(reply, Msg::DvrIo(DvrIoReply::Exported { .. })),
            "{reply:?}"
        );
        let written = std::fs::read(dir.join("a.vdvr")).unwrap();
        assert_eq!(&written[..10], b"VIEWDVR\0\x01\x00");
        let clip = clip::read::decode(&mut written.as_slice(), MAX).unwrap();
        assert_eq!(clip.inputs.len(), 3, "the clip holds the log as queued");
    }

    /// A ring of `frames` frames painted a second apart.
    fn clip_ring(frames: u64) -> FrameRing {
        let mut builder = RingBuilder::new(MAX);
        for at in 0..frames {
            builder
                .push_key(at * 1_000_000, (4, 2), None, std::iter::empty())
                .unwrap();
        }
        builder.finish()
    }

    /// Writes a clip of [`clip_ring`]'s frames to `path`.
    fn write_clip(path: &std::path::Path, frames: u64) {
        let mut ring = clip_ring(frames);
        let mut bytes = Vec::new();
        let snapshot = ring.snapshot().unwrap();
        clip::encode(&mut bytes, &snapshot, &clip::Inputs::default(), &[], &[]).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    /// A loop over [`recorded`]'s frames with its file thread running, and
    /// the channel the thread's replies arrive on.
    fn wired(model: &mut Model) -> (DvrLoop, std::sync::mpsc::Receiver<Msg>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let (clips_tx, clips) = std::sync::mpsc::channel();
        let mut dvr = recorded(model);
        dvr.clips = clips;
        dvr.io = Some(io::start(LoopSender::new(tx), false, clips_tx).unwrap());
        (dvr, rx)
    }

    /// Invokes `:View dvr play PATH` and folds the file thread's reply.
    fn play(
        model: &mut Model,
        dvr: &mut DvrLoop,
        rx: &std::sync::mpsc::Receiver<Msg>,
        path: &std::path::Path,
    ) {
        let _ = update(
            model,
            Msg::FeatureInvoke {
                generation: None,
                feature: "dvr".to_owned(),
                verb: format!("play {}", path.display()),
            },
        );
        let _ = dvr.poll(model);
        let reply = rx
            .recv_timeout(view_test_support::host_deadline(QUIT_WAIT))
            .unwrap();
        let _ = update(model, reply);
        let _ = dvr.poll(model);
    }

    #[test]
    fn play_opens_a_clip_in_scrub_and_close_returns_live() {
        let dir = view_test_support::ScratchDir::new("dvr-play").unwrap();
        let path = dir.join("a.vdvr");
        write_clip(&path, 3);
        let mut model = Model::with_term_size(80, 24);
        let (mut dvr, rx) = wired(&mut model);
        open_scrub(&mut model, &mut dvr);
        for _ in 0..3 {
            let _ = press(&mut model, &mut dvr, "h");
        }
        assert!(matches!(dvr.scrub_pass(&mut model), Some(Some((3, _)))));
        play(&mut model, &mut dvr, &rx, &path);
        assert_eq!(model.dvr.scrub_frame(), Some(3), "the clip's newest frame");
        assert_eq!(
            dvr.scrub_pass(&mut model),
            Some(Some((
                3,
                "CLIP a.vdvr  -0.0s of 2.0s  q close  h/l frame  H/L 1s  g/G ends".to_owned()
            ))),
            "the clip's frame is painted over the recording's of the same number"
        );
        assert_eq!(press(&mut model, &mut dvr, "h"), Some(2));
        assert_eq!(press(&mut model, &mut dvr, "g"), Some(1));
        assert_eq!(press(&mut model, &mut dvr, "q"), None);
        assert_eq!(dvr.ring.newest(), Some(6), "the recording is back");
        open_scrub(&mut model, &mut dvr);
        assert_eq!(model.dvr.scrub_frame(), Some(6));
        assert!(dvr.bar(6, false, None, false).starts_with("DVR  "));
    }

    #[test]
    fn play_reports_an_unreadable_file() {
        let dir = view_test_support::ScratchDir::new("dvr-play-bad").unwrap();
        let garbage = dir.join("b.vdvr");
        std::fs::write(&garbage, b"not a clip").unwrap();
        let empty = dir.join("e.vdvr");
        write_clip(&empty, 0);
        let mut model = Model::with_term_size(80, 24);
        let (mut dvr, rx) = wired(&mut model);
        let cases = [
            (garbage, "not a view DVR clip"),
            (empty, "holds no frame"),
            (dir.join("missing.vdvr"), "missing.vdvr: "),
        ];
        for (path, why) in cases {
            play(&mut model, &mut dvr, &rx, &path);
            let said = said(&model);
            let line = format!("DVR play failed: {}", path.display());
            assert!(said.contains(&line), "{said}");
            assert!(said.contains(why), "{why}: {said}");
            assert!(model.dvr.scrub_frame().is_none());
            assert!(dvr.clips.try_recv().is_err(), "no frames were handed over");
        }
        let mut idle = recorded(&mut model);
        let _ = update(
            &mut model,
            Msg::FeatureInvoke {
                generation: None,
                feature: "dvr".to_owned(),
                verb: "play /w/a.vdvr".to_owned(),
            },
        );
        let _ = idle.poll(&mut model);
        assert_eq!(
            told(&model, "play failed: its file thread is not running"),
            1
        );
    }

    /// Invokes `:View dvr TEXT` and polls the loop once.
    fn verb(model: &mut Model, dvr: &mut DvrLoop, text: &str) {
        let _ = update(
            model,
            Msg::FeatureInvoke {
                generation: None,
                feature: "dvr".to_owned(),
                verb: text.to_owned(),
            },
        );
        let _ = dvr.poll(model);
    }

    /// A play asked for while a clip is being read is refused by name, and
    /// the file thread reads one clip.
    #[test]
    fn a_second_play_while_one_loads_is_refused_and_one_read_runs() {
        let _watchdog = view_test_support::watchdog();
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        let (read_tx, read) = std::sync::mpsc::channel();
        let (gate, gate_rx) = std::sync::mpsc::channel::<()>();
        let writer = BackgroundWriter::start("dvr-io-test", 1, move |job: IoJob| {
            if let IoJob::Play { .. } = job {
                let _ = read_tx.send(());
                let _ = gate_rx.recv();
            }
            Ok::<(), Infallible>(())
        })
        .unwrap();
        dvr.io = Some(writer);
        verb(&mut model, &mut dvr, "play /w/a.vdvr");
        read.recv().unwrap();
        verb(&mut model, &mut dvr, "play /w/b.vdvr");
        assert_eq!(
            told(&model, "play busy: /w/a.vdvr is still being read"),
            1,
            "{:?}",
            model.engine.messages.entries
        );
        drop(gate);
        let mut io = dvr.io.take().unwrap();
        assert_eq!(
            io.finish_within(view_test_support::host_deadline(QUIT_WAIT)),
            view_proc::writer::Finished::Done
        );
        assert_eq!(read.try_iter().count(), 0, "the second clip is never read");
    }

    /// Playing a clip while one is open brings the recording back before
    /// the next clip is read, so the loop holds the recording and one clip.
    #[test]
    fn a_play_with_a_clip_open_brings_the_recording_back_first() {
        let dir = view_test_support::ScratchDir::new("dvr-play-two").unwrap();
        let (first, second) = (dir.join("a.vdvr"), dir.join("b.vdvr"));
        write_clip(&first, 3);
        write_clip(&second, 4);
        let mut model = Model::with_term_size(80, 24);
        let (mut dvr, rx) = wired(&mut model);
        play(&mut model, &mut dvr, &rx, &first);
        assert_eq!(dvr.ring.newest(), Some(3), "the first clip is shown");
        verb(&mut model, &mut dvr, &format!("play {}", second.display()));
        assert_eq!(
            dvr.ring.newest(),
            Some(6),
            "the recording is back before the next clip is read"
        );
        assert!(dvr.live.is_none(), "the first clip's frames are freed");
        let reply = rx
            .recv_timeout(view_test_support::host_deadline(QUIT_WAIT))
            .unwrap();
        let _ = update(&mut model, reply);
        let _ = dvr.poll(&mut model);
        assert_eq!(dvr.ring.newest(), Some(4), "the second clip is shown");
        assert_eq!(press(&mut model, &mut dvr, "q"), None);
        assert_eq!(
            dvr.ring.newest(),
            Some(6),
            "the recording parked under it is whole"
        );
    }

    /// Makes a named pipe at `path`, answering the error the volume gives.
    #[cfg(unix)]
    fn mkfifo(path: &std::path::Path) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: `path` is a NUL-terminated buffer that outlives the
        // call, which retains nothing.
        #[allow(unsafe_code)]
        let made = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
        if made == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// A named pipe is refused without being opened, so the file thread
    /// goes on to the export queued behind it.
    #[cfg(unix)]
    #[test]
    fn playing_a_named_pipe_is_refused_and_an_export_behind_it_completes() {
        let _watchdog = view_test_support::watchdog();
        let dir = view_test_support::ScratchDir::new("dvr-play-fifo").unwrap();
        let fifo = dir.join("p.vdvr");
        match mkfifo(&fifo) {
            Err(e) if io::lacks_the_operation(&e) => {
                eprintln!("skipped: this volume refuses a named pipe ({e})");
                return;
            }
            made => made.unwrap(),
        }
        let mut model = Model::with_term_size(80, 24);
        let (mut dvr, rx) = wired(&mut model);
        verb(&mut model, &mut dvr, &format!("play {}", fifo.display()));
        let clip = dir.join("after.vdvr");
        export(&mut model, &mut dvr, &clip.display().to_string());
        for _ in 0..2 {
            let reply = rx
                .recv_timeout(view_test_support::host_deadline(QUIT_WAIT))
                .expect("the file thread answers the play and the export");
            let _ = update(&mut model, reply);
        }
        let refused = format!("DVR play failed: {}: not a file", fifo.display());
        assert_eq!(
            told(&model, &refused),
            1,
            "{:?}",
            model.engine.messages.entries
        );
        assert_eq!(told(&model, "DVR clip written"), 1);
        assert!(clip.is_file());
    }

    #[test]
    fn a_clip_longer_than_max_mb_holds_names_the_oldest_frames_left_out() {
        let dir = view_test_support::ScratchDir::new("dvr-play-cut").unwrap();
        let path = dir.join("long.vdvr");
        write_clip(&path, 40);
        let mut model = Model::with_term_size(80, 24);
        let (mut dvr, rx) = wired(&mut model);
        // room for a few one-frame groups of the forty
        model.dvr.enable(clip_ring(1).held() * 4);
        play(&mut model, &mut dvr, &rx, &path);
        assert_eq!(dvr.ring.newest(), Some(40), "the newest end is kept");
        let oldest = dvr.ring.oldest().unwrap();
        assert!(oldest > 1, "the clip was cut, its oldest frame {oldest}");
        let bar = dvr.scrub_pass(&mut model).flatten().map(|(_, bar)| bar);
        let bar = bar.expect("the clip is not shown");
        let cut = format!(
            "  ({} oldest frames past [dvr] max_mb not loaded)",
            oldest - 1
        );
        assert!(bar.starts_with("CLIP long.vdvr  -"), "{bar}");
        assert!(bar.ends_with(&cut), "{bar}");
        assert_eq!(told(&model, "not loaded"), 0, "a notice under the clip");
    }

    /// The clip bar of a cut clip under the name view gives an export,
    /// read at 80 columns, still says how to close it.
    #[test]
    fn a_cut_clips_bar_keeps_its_close_key_on_an_80_column_terminal() {
        let mut model = Model::with_term_size(80, 24);
        let dvr = recorded(&mut model);
        let bar = dvr.bar(6, false, Some(("/w/view-dvr-1759700000.vdvr", 40)), false);
        let shown: String = bar.chars().take(80).collect();
        assert!(shown.contains("q close"), "{shown}");
        assert!(bar.contains("40 oldest frames"), "{bar}");
    }

    #[test]
    fn a_clip_bar_names_the_scrub_keys_but_branch_and_export() {
        let mut model = Model::with_term_size(80, 24);
        let dvr = recorded(&mut model);
        assert_eq!(
            dvr.bar(6, false, Some(("/w/clips/a.vdvr", 0)), false),
            "CLIP a.vdvr  -0.0s of 2.0s  q close  h/l frame  H/L 1s  g/G ends"
        );
        assert!(dvr
            .bar(6, false, None, false)
            .ends_with("g/G ends  b branch  e export"));
    }

    fn paint_live(model: &mut Model, term: &mut Term, dvr: &mut DvrLoop, row: u16) {
        model.engine.apply_grid(GridOp::PutLine {
            row,
            col_start: 0,
            cells: vec![("x".into(), 0, 1)],
        });
        let damage = model.take_paint_damage();
        let surface = view_surface::render(model);
        term.probe_paint(model, &surface, &damage).unwrap();
        let _ = dvr.after_paint(term, model);
    }

    #[test]
    fn the_recording_is_kept_whole_while_a_clip_is_open() {
        let mut model = Model::with_term_size(20, 6);
        model.engine.apply_grid(GridOp::Resize {
            width: 20,
            height: 6,
        });
        model.dvr.enable(MAX);
        let mut term = Term::frame_probe(model.caps);
        let (clips_tx, clips) = std::sync::mpsc::channel();
        let mut dvr = DvrLoop::with(FrameRing::new(MAX), None, clips);
        paint_live(&mut model, &mut term, &mut dvr, 0);
        paint_live(&mut model, &mut term, &mut dvr, 1);
        assert_eq!(dvr.ring.newest(), Some(2));
        clips_tx.send(clip_ring(5)).unwrap();
        let loaded = DvrIoReply::ClipLoaded {
            path: "/w/a.vdvr".to_owned(),
            left_out: 0,
        };
        let _ = update(&mut model, Msg::DvrIo(loaded));
        let _ = dvr.poll(&mut model);
        assert_eq!(model.dvr.scrub_frame(), Some(5));
        let _ = press(&mut model, &mut dvr, "q");
        assert_eq!(dvr.ring.newest(), Some(2), "the recording was kept whole");
        paint_live(&mut model, &mut term, &mut dvr, 2);
        assert_eq!(dvr.ring.newest(), Some(3), "a frame after close joins it");
        open_scrub(&mut model, &mut dvr);
        assert_eq!(model.dvr.scrub_frame(), Some(3));
    }

    /// A quit while a clip is being read waits for nothing.
    #[test]
    fn a_quit_while_a_clip_loads_waits_for_nothing() {
        let _watchdog = view_test_support::watchdog();
        let mut model = Model::with_term_size(80, 24);
        let mut dvr = recorded(&mut model);
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (gate, gate_rx) = std::sync::mpsc::channel::<()>();
        let writer = BackgroundWriter::start("dvr-io-test", 1, move |job: IoJob| {
            if let IoJob::Play { .. } = job {
                let _ = entered_tx.send(());
                let _ = gate_rx.recv();
            }
            Ok::<(), Infallible>(())
        })
        .unwrap();
        dvr.io = Some(writer);
        verb(&mut model, &mut dvr, "play a.vdvr");
        entered.recv().unwrap();
        assert_eq!(Arc::strong_count(&dvr.exporting), 1, "a read is no export");
        assert_eq!(dvr.finish_within(Duration::MAX), None);
        drop(gate);
    }
}
