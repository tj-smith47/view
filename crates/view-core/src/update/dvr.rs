//! The session DVR's `:View dvr` verbs and the keys the scrub answers while
//! the screen shows a recorded frame.

use crate::model::{unsaved_files, Model, OverlayKind};
use crate::msg::{Effect, Msg};
use crate::native::dvr::{BranchRefusal, DvrIoReply, ExportRefusal, ScrubStep};
use crate::native::prompt::PromptState;

/// What `:View dvr` answers while recording is off.
const OFF: &str = "view: DVR is off: set [dvr] enabled = true";

/// What `:View dvr scrub` answers before the first frame is recorded.
const EMPTY: &str = "view: DVR has recorded no frame yet";

/// What a branch answers when the editor reads its input from a pipe,
/// which a replacement cannot read again.
const BRANCH_PIPED: &str = "view: DVR cannot branch while the editor reads piped input";

/// What `:View dvr play` answers with no path.
const PLAY_NO_PATH: &str = "view: DVR play needs a clip: :View dvr play PATH";

/// Every key the scrub answers, and what it does. `docs/keymaps.md`
/// carries the rendered table. Test-only: the scrub matches on the keys
/// themselves.
#[cfg(test)]
pub(crate) const DVR_KEYS: [(&str, &str); 9] = [
    ("h", "one frame back"),
    ("l", "one frame forward"),
    ("H", "one second back"),
    ("L", "one second forward"),
    ("g", "the oldest frame kept"),
    ("G", "the newest frame"),
    ("q", "back to the live screen, as `<Esc>` does"),
    (
        "b",
        "the editor replaced by one brought to this frame, after a confirm",
    ),
    ("e", "the recording written to a clip file, then live"),
];

/// Answers `:View dvr <verb>`.
pub(super) fn invoke(model: &mut Model, verb: &str) -> Vec<Effect> {
    model.dirty = true;
    if !model.dvr.is_recording() {
        return model.engine.record_native_notice(OFF.to_string(), false);
    }
    // the path is every byte after the first blank run, as typed
    let (word, path) = verb.split_once(char::is_whitespace).unwrap_or((verb, ""));
    if model.dvr.absorb_invoke(word) {
        return Vec::new();
    }
    model.dvr.mark_invoke(word);
    match word {
        "scrub" if !model.dvr.open_scrub() => {
            model.engine.record_native_notice(EMPTY.to_string(), false)
        }
        "scrub" => Vec::new(),
        "close" => {
            model.dvr.close_scrub();
            Vec::new()
        }
        "export" => export(model, path.trim_start()),
        "play" => play(model, path.trim_start()),
        _ => model
            .engine
            .record_native_notice(super::feature_invoke_notice("dvr", verb, false), false),
    }
}

/// Queues a read of the clip at `path`, which opens in the scrub once read,
/// closing the clip shown. Refused while another clip is being read.
fn play(model: &mut Model, path: &str) -> Vec<Effect> {
    if path.is_empty() {
        return model
            .engine
            .record_native_notice(PLAY_NO_PATH.to_owned(), false);
    }
    if let Some(loading) = model.dvr.loading() {
        let text = format!("view: DVR play busy: {loading} is still being read");
        return model.engine.record_native_notice(text, false);
    }
    // the loop frees the shown clip before the next read starts, so the
    // recording and one clip are the most it holds
    if model.dvr.clip().is_some() {
        model.dvr.close_scrub();
    }
    model.dvr.request_play(path.to_owned());
    Vec::new()
}

/// Closes a clip with the notice that `verb` does not run in one. `None`
/// when no clip is shown.
fn refuse_in_clip(model: &mut Model, verb: &str) -> Option<Vec<Effect>> {
    model.dvr.clip()?;
    model.dvr.close_scrub();
    model.dirty = true;
    let text = format!("view: DVR {verb} is not available in a clip");
    Some(model.engine.record_native_notice(text, false))
}

/// Closes the scrub and queues an export to `path`, or to a derived path
/// when it is empty.
fn export(model: &mut Model, path: &str) -> Vec<Effect> {
    if let Some(refused) = refuse_in_clip(model, "export") {
        return refused;
    }
    model.dvr.close_scrub();
    model.dirty = true;
    let path = (!path.is_empty()).then(|| path.to_owned());
    if model.dvr.request_export(path) {
        return Vec::new();
    }
    on_io(model, &DvrIoReply::Refused(ExportRefusal::NoFrame))
}

/// Asks to branch from frame `at`, the frame the scrub shows, which stays
/// on screen under the confirm raised once the disk check answers. A
/// refusal closes the scrub with a notice saying why that frame cannot be
/// reproduced.
fn branch(model: &mut Model, at: u64) -> Vec<Effect> {
    if let Some(refused) = refuse_in_clip(model, "branch") {
        return refused;
    }
    model.dirty = true;
    if model.stdin_relay {
        model.dvr.close_scrub();
        return model
            .engine
            .record_native_notice(BRANCH_PIPED.to_owned(), false);
    }
    let Err(refusal) = model.dvr.ask_branch(at) else {
        return Vec::new();
    };
    model.dvr.close_scrub();
    let text = match refusal {
        BranchRefusal::Dead => format!(
            "view: DVR cannot branch from frame {at}: a later branch left it \
             behind"
        ),
        BranchRefusal::PastLog(full) => format!(
            "view: DVR cannot branch from frame {at}: the input log filled at \
             frame {full}; raise [dvr] max_mb"
        ),
    };
    model.engine.record_native_notice(text, false)
}

/// Raises the confirm a pending branch waits on, or the notice a reply
/// from the DVR's file work carries.
pub(super) fn on_io(model: &mut Model, reply: &DvrIoReply) -> Vec<Effect> {
    match reply {
        DvrIoReply::ClipLoaded { path, .. } => {
            model.dvr.open_clip(path.clone());
            model.dirty = true;
        }
        DvrIoReply::Failed { verb: "play", .. } => model.dvr.end_load(),
        _ => {}
    }
    if let DvrIoReply::DiskChecked {
        changed,
        unverifiable,
    } = reply
    {
        if let Some(at) = model.dvr.take_asked() {
            let state = PromptState::dvr_branch_prompt(
                at,
                &unsaved_files(&model.buffers),
                changed,
                model.dvr.restarts_before(at),
                *unverifiable,
            );
            // beneath a prompt the engine waits on, which keeps its answer
            // and is the question the live screen has to show
            if let Some(OverlayKind::Prompt(_)) = model.focused_overlay().map(|ov| &ov.kind) {
                model.dvr.close_scrub();
                model.insert_overlay_beneath_top(state.overlay_box(), OverlayKind::Prompt(state));
            } else {
                model.push_overlay(state.overlay_box(), OverlayKind::Prompt(state));
            }
            model.dirty = true;
            return Vec::new();
        }
    }
    let Some(text) = reply.notice() else {
        return Vec::new();
    };
    model.dirty = true;
    model.engine.record_native_notice(text, false)
}

/// Closes every view overlay and drops any held keys, returning what each
/// close owes the executor. A replayed launch reaches the frame from a
/// screen with none of them open.
pub(super) fn close_view_surfaces(model: &mut Model) -> Vec<Effect> {
    let _ = model.submit_hold.take_held();
    let mut effects = Vec::new();
    if model.close_tree() {
        effects.push(Effect::TreeClose);
    }
    while let Some(overlay) = model.pop_focused_overlay() {
        if matches!(overlay.kind, OverlayKind::Picker(_)) {
            effects.push(Effect::PickerClose);
        }
    }
    model.dirty = true;
    effects
}

/// Takes every key, paste and click while the scrub is open, so none
/// reaches the engine. `None` on the live screen, for a key while the
/// branch confirm answers keys over the frame it names, and for any other
/// message.
pub(super) fn scrub_input(model: &mut Model, msg: &Msg) -> Option<Vec<Effect>> {
    let shown = model.dvr.scrub_frame()?;
    match msg {
        Msg::Key(_) if model.branch_confirm_focused() => None,
        Msg::Key(key) => Some(scrub_key(model, &key.notation, shown)),
        Msg::Paste(_) | Msg::Mouse(_) => Some(Vec::new()),
        _ => None,
    }
}

/// Moves the scrub cursor off frame `shown`, closes the scrub, exports the
/// recording or branches from `shown`. Any other key does nothing, and
/// while a branch waits on its disk check only closing acts.
fn scrub_key(model: &mut Model, notation: &str, shown: u64) -> Vec<Effect> {
    if model.dvr.is_asking() && !matches!(notation, "q" | "<Esc>") {
        return Vec::new();
    }
    let step = match notation {
        "h" => ScrubStep::Frames(-1),
        "l" => ScrubStep::Frames(1),
        "H" => ScrubStep::Seconds(-1),
        "L" => ScrubStep::Seconds(1),
        "g" => ScrubStep::Oldest,
        "G" => ScrubStep::Newest,
        "e" => return export(model, ""),
        "b" => return branch(model, shown),
        "q" | "<Esc>" => {
            model.dvr.close_scrub();
            model.dirty = true;
            return Vec::new();
        }
        _ => return Vec::new(),
    };
    model.dvr.step(step);
    model.dirty = true;
    Vec::new()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::msg::{Key, MouseInput, RpcCall};
    use crate::native::dvr::{DvrRequest, Marker};
    use crate::update::update;

    fn key(notation: &str) -> Msg {
        Msg::Key(Key {
            notation: notation.to_owned(),
        })
    }

    fn invoke_msg(verb: &str) -> Msg {
        Msg::FeatureInvoke {
            generation: None,
            feature: "dvr".to_owned(),
            verb: verb.to_owned(),
        }
    }

    fn recorded() -> Model {
        let mut m = Model::with_term_size(80, 24);
        m.engine.mode.current = "normal".to_owned();
        m.dvr.enable(1 << 20);
        m.dvr.note_frame(9, 2);
        m
    }

    fn reaches_engine(effects: &[Effect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, Effect::Rpc(RpcCall::Input { .. })))
    }

    #[test]
    fn scrub_opens_on_the_newest_frame_and_steps_through_the_ring() {
        let mut m = recorded();
        let _ = update(&mut m, invoke_msg("scrub"));
        assert_eq!(m.dvr.scrub_frame(), Some(9));
        assert_eq!(m.dvr.markers(), [(9, Marker::Invoke)]);
        for k in ["h", "h", "H", "L", "g", "l", "G"] {
            let _ = update(&mut m, key(k));
        }
        let steps: Vec<_> = std::iter::from_fn(|| m.dvr.take_step()).collect();
        assert_eq!(
            steps,
            [
                ScrubStep::Newest,
                ScrubStep::Frames(-2),
                ScrubStep::Seconds(0),
                ScrubStep::Oldest,
                ScrubStep::Frames(1),
                ScrubStep::Newest,
            ]
        );
        m.dvr.show(4);
        assert_eq!(m.dvr.scrub_frame(), Some(4));
    }

    #[test]
    fn scrub_keys_never_reach_the_engine() {
        let mut m = recorded();
        let before = m.dvr.inputs().count();
        let _ = update(&mut m, invoke_msg("scrub"));
        let mut effects = Vec::new();
        for k in ["h", "i", "x", ":", "<CR>", " ", "f", "f", "<C-w>"] {
            effects.extend(update(&mut m, key(k)));
        }
        effects.extend(update(&mut m, Msg::Paste("text".to_owned())));
        effects.extend(update(
            &mut m,
            Msg::Mouse(MouseInput {
                button: "left".to_owned(),
                action: "press".to_owned(),
                modifier: String::new(),
                row: 1,
                col: 1,
            }),
        ));
        assert!(effects.is_empty(), "{effects:?}");
        assert_eq!(
            m.dvr.inputs().count(),
            before,
            "no swallowed input is logged"
        );
        assert!(m.dvr.scrub_frame().is_some());
        let _ = update(&mut m, key("q"));
        assert!(reaches_engine(&update(&mut m, key("j"))));
    }

    #[test]
    fn closing_scrub_repaints_the_live_screen_in_full() {
        for close in [key("q"), key("<Esc>")] {
            let mut m = recorded();
            let _ = update(&mut m, invoke_msg("scrub"));
            m.dirty = false;
            let _ = update(&mut m, close);
            assert!(m.dvr.scrub_frame().is_none());
            assert!(m.dvr.take_step().is_none(), "no move outlives the scrub");
            assert!(m.dirty, "the live frame that closes the scrub is owed");
        }
        let mut m = recorded();
        m.dvr.open_scrub();
        m.dirty = false;
        let _ = update(&mut m, invoke_msg("close"));
        assert!(m.dvr.scrub_frame().is_none());
        assert!(m.dirty);
    }

    #[test]
    fn a_disabled_dvr_refuses_every_verb_in_one_notice() {
        for verb in ["scrub", "close", "rewind"] {
            let mut m = Model::with_term_size(80, 24);
            let _ = update(&mut m, invoke_msg(verb));
            let raised = format!("{:?}", m.engine.messages.entries);
            assert_eq!(raised.matches("DVR is off").count(), 1, "{verb}: {raised}");
            assert!(
                raised.contains("set [dvr] enabled = true"),
                "{verb}: {raised}"
            );
            assert!(m.dvr.scrub_frame().is_none());
            assert!(scrub_input(&mut m, &key("h")).is_none());
        }
    }

    #[test]
    fn every_scrub_key_moves_or_closes_and_no_other_key_does() {
        for (notation, _) in DVR_KEYS.iter().chain(&[("<Esc>", "")]) {
            let mut m = recorded();
            let _ = update(&mut m, invoke_msg("scrub"));
            assert_eq!(m.dvr.take_step(), Some(ScrubStep::Newest));
            let _ = update(&mut m, key(notation));
            let ended = m.dvr.scrub_frame().is_none() || m.dvr.is_asking();
            assert!(
                ended != m.dvr.take_step().is_some(),
                "{notation} neither moved, closed nor branched from the scrub"
            );
        }
        let mut m = recorded();
        let _ = update(&mut m, invoke_msg("scrub"));
        let _ = m.dvr.take_step();
        let _ = update(&mut m, key("j"));
        assert!(m.dvr.scrub_frame().is_some() && m.dvr.take_step().is_none());
    }

    #[test]
    fn the_bar_names_only_keys_the_scrub_answers() {
        let named: Vec<&str> = crate::native::dvr::SCRUB_HINT
            .split("  ")
            .filter_map(|hint| hint.split(' ').next())
            .flat_map(|keys| keys.split('/'))
            .collect();
        assert_eq!(named.len(), 9, "{named:?}");
        for k in named {
            assert!(
                DVR_KEYS.iter().any(|(notation, _)| *notation == k),
                "the bar names {k}, which the scrub does not answer"
            );
        }
    }

    fn exports(m: &mut Model) -> Vec<Option<String>> {
        std::iter::from_fn(|| m.dvr.take_request())
            .filter_map(|r| match r {
                crate::native::dvr::DvrRequest::Export(path) => Some(path),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn export_from_the_scrub_or_the_command_queues_a_clip() {
        let mut m = recorded();
        let _ = update(&mut m, invoke_msg("scrub"));
        let effects = update(&mut m, key("e"));
        assert!(effects.is_empty(), "{effects:?}");
        assert!(
            m.dvr.scrub_frame().is_none(),
            "the export shows the live screen"
        );
        assert!(m.dirty);
        let _ = update(&mut m, invoke_msg("export"));
        let _ = update(&mut m, invoke_msg("export   /w/clips/a  b.vdvr "));
        let _ = update(&mut m, invoke_msg("export\t/w/c.vdvr"));
        assert_eq!(
            exports(&mut m),
            [
                None,
                None,
                Some("/w/clips/a  b.vdvr ".to_owned()),
                Some("/w/c.vdvr".to_owned())
            ]
        );
    }

    #[test]
    fn every_export_reply_is_told_in_its_own_words() {
        use crate::native::dvr::ExportRefusal;
        let mut fresh = Model::with_term_size(80, 24);
        fresh.dvr.enable(1 << 20);
        let _ = update(&mut fresh, invoke_msg("export"));
        assert!(exports(&mut fresh).is_empty());
        let mut told = vec![format!("{:?}", fresh.engine.messages.entries)];
        let replies = [
            DvrIoReply::Exported {
                path: "/w/view-dvr-1.vdvr".to_owned(),
                cut: 0,
            },
            DvrIoReply::Exported {
                path: "/w/view-dvr-2.vdvr".to_owned(),
                cut: 3,
            },
            DvrIoReply::Refused(ExportRefusal::Busy),
            DvrIoReply::Refused(ExportRefusal::OverBudget),
            DvrIoReply::Refused(ExportRefusal::NoWriter),
            DvrIoReply::Failed {
                verb: "export",
                reason: "/w/x.vdvr already exists, left as it is".to_owned(),
            },
            DvrIoReply::Refused(ExportRefusal::Queued),
        ];
        for reply in replies {
            let mut m = recorded();
            m.dirty = false;
            let _ = update(&mut m, Msg::DvrIo(reply.clone()));
            assert!(m.dirty, "{reply:?}");
            told.push(format!("{:?}", m.engine.messages.entries));
        }
        for (i, text) in told.iter().enumerate() {
            assert!(text.contains("DVR"), "{text}");
            assert!(!text.contains("no room for") && !text.contains("DVR is off"));
            assert!(
                told.iter().skip(i + 1).all(|other| other != text),
                "{text} is told twice"
            );
        }
        assert!(told[1].contains("DVR clip written: /w/view-dvr-1.vdvr"));
        assert!(told[2].contains('3'), "{}", told[2]);
        assert!(told[2].contains("3 symbols cut"), "{}", told[2]);
        assert!(told[4].contains("raise [dvr] max_mb"), "{}", told[4]);
        assert!(
            told[6].contains("/w/x.vdvr already exists, left as it is"),
            "{}",
            told[6]
        );
        assert!(told[7].contains("file work is queued"), "{}", told[7]);
        let mut m = recorded();
        let quiet = DvrIoReply::DiskChecked {
            changed: Vec::new(),
            unverifiable: false,
        };
        assert!(update(&mut m, Msg::DvrIo(quiet)).is_empty());
    }

    fn disk_checks(m: &mut Model) -> usize {
        std::iter::from_fn(|| m.dvr.take_request())
            .filter(|r| matches!(r, DvrRequest::DiskCheck))
            .count()
    }

    fn checked(m: &mut Model, changed: &[&str], unverifiable: bool) {
        let reply = DvrIoReply::DiskChecked {
            changed: changed.iter().map(|p| (*p).to_owned()).collect(),
            unverifiable,
        };
        let _ = update(m, Msg::DvrIo(reply));
    }

    /// Opens the scrub, steps back `back` frames and presses `b`.
    fn branch_back(m: &mut Model, back: u64) {
        let _ = update(m, invoke_msg("scrub"));
        let shown = m.dvr.scrub_frame().unwrap() - back;
        m.dvr.show(shown);
        let _ = update(m, key("b"));
    }

    fn prompt_text(m: &Model) -> Option<String> {
        m.overlays().iter().rev().find_map(|ov| match &ov.kind {
            crate::model::OverlayKind::Prompt(p) => p.dvr_branch_at().map(|_| format!("{p:?}")),
            _ => None,
        })
    }

    fn told_once(m: &Model, words: &str) -> bool {
        format!("{:?}", m.engine.messages.entries)
            .matches(words)
            .count()
            == 1
    }

    #[test]
    fn branch_refuses_under_stdin_relay() {
        let mut m = recorded();
        m.stdin_relay = true;
        branch_back(&mut m, 2);
        assert!(m.dvr.scrub_frame().is_none(), "b leaves the scrub");
        assert_eq!(disk_checks(&mut m), 0);
        assert!(told_once(&m, "piped"), "{:?}", m.engine.messages.entries);
        checked(&mut m, &[], false);
        assert!(prompt_text(&m).is_none());
    }

    /// Confirms a branch from frame `at` and settles it the way the loop
    /// does once the fresh engine is up, then paints frames up to `upto`.
    fn branched(m: &mut Model, at: u64, upto: u64) {
        // opened directly: a scrub invoked with no keys behind it would
        // count as one the replay invokes again
        m.dvr.open_scrub();
        m.dvr.show(at);
        let _ = update(m, key("b"));
        checked(m, &[], false);
        m.note_frame_painted();
        let _ = update(m, key("y"));
        let plan = std::iter::from_fn(|| m.dvr.take_request())
            .find_map(|r| match r {
                DvrRequest::Branch(plan) => Some(plan),
                _ => None,
            })
            .expect("y queues the branch");
        assert_eq!(plan.at_frame, at);
        m.dvr.branched(plan.at_frame, plan.replay);
        let _ = m.takes_attach();
        for msg in crate::update::due_replay(m) {
            let _ = update(m, msg);
        }
        for seq in m.dvr.markers().last().map_or(0, |(f, _)| *f) + 1..=upto {
            m.dvr.note_frame(seq, 2);
        }
    }

    #[test]
    fn branch_refuses_a_dead_frame() {
        let mut m = recorded();
        for (frame, k) in [(3, "i"), (5, "a"), (7, "b"), (9, "c")] {
            m.dvr.note_frame(frame, 2);
            let _ = update(&mut m, key(k));
        }
        branched(&mut m, 6, 12);
        assert_eq!(m.dvr.dead(), std::slice::from_ref(&(7..=9)));
        assert_eq!(
            m.dvr.inputs().map(|i| i.body.to_vec()).collect::<Vec<_>>(),
            [b"i".to_vec(), b"a".to_vec(), vec![80, 0, 24, 0]],
            "the log keeps what was folded before frame 6, once, then the size \
             the branch went on at"
        );
        assert!(m.dvr.markers().iter().any(|(_, k)| *k == Marker::Branch));
        let _ = update(&mut m, invoke_msg("scrub"));
        m.dvr.show(8);
        let _ = update(&mut m, key("b"));
        assert_eq!(disk_checks(&mut m), 0);
        assert!(told_once(&m, "frame 8"), "{:?}", m.engine.messages.entries);
        let _ = update(&mut m, invoke_msg("scrub"));
        m.dvr.show(11);
        let _ = update(&mut m, key("b"));
        assert_eq!(disk_checks(&mut m), 1, "a frame after the dead run is live");
    }

    #[test]
    fn branch_prompt_names_unsaved_and_changed_files() {
        use crate::model::BufferEntry;
        let mut m = recorded();
        let _ = update(
            &mut m,
            Msg::BufferList {
                buffers: vec![
                    BufferEntry::new(1, "a.rs".into(), true, true).with_path("/w/a.rs".into()),
                    BufferEntry::new(2, "b.rs".into(), false, false).with_path("/w/b.rs".into()),
                    BufferEntry::new(3, "c.rs".into(), true, false).with_path("/w/c.rs".into()),
                ],
            },
        );
        branch_back(&mut m, 2);
        assert_eq!(disk_checks(&mut m), 1);
        assert!(
            prompt_text(&m).is_none(),
            "the prompt waits for the disk check"
        );
        checked(&mut m, &["/w/c.toml"], false);
        let text = prompt_text(&m).unwrap();
        assert!(text.contains("frame 7"), "{text}");
        assert!(text.contains("Unsaved: /w/a.rs, /w/c.rs."), "{text}");
        assert!(!text.contains("b.rs"), "{text}");
        assert!(text.contains("Changed on disk: /w/c.toml."), "{text}");
        assert!(!text.contains("not checked"), "{text}");
        let _ = update(&mut m, key("<Esc>"));

        let mut remote = recorded();
        branch_back(&mut remote, 1);
        checked(&mut remote, &[], true);
        let text = prompt_text(&remote).unwrap();
        assert!(text.contains("Disk not checked"), "{text}");
        assert!(
            !text.contains("Unsaved") && !text.contains("Changed"),
            "{text}"
        );
        remote.note_frame_painted();
        let logged = remote.dvr.inputs().count();
        let _ = update(&mut remote, key("n"));
        assert!(prompt_text(&remote).is_none());
        assert!(!remote.dvr.is_branching(), "n cancels");
        assert_eq!(
            remote.dvr.inputs().count(),
            logged,
            "a later replay would type the answer into the engine"
        );
    }

    #[test]
    fn the_chosen_frame_stays_on_screen_under_the_branch_confirm() {
        for (answer, branches) in [("y", true), ("n", false), ("<Esc>", false)] {
            let mut m = recorded();
            branch_back(&mut m, 2);
            let at = m.dvr.scrub_frame();
            assert_eq!(at, Some(7), "{answer}: b keeps the frame it was pressed on");
            let _ = update(&mut m, key("h"));
            assert_eq!(
                m.dvr.scrub_frame(),
                at,
                "{answer}: the frame holds for the check"
            );
            checked(&mut m, &[], false);
            assert!(prompt_text(&m).is_some(), "{answer}");
            assert_eq!(m.dvr.scrub_frame(), at, "{answer}: and under the confirm");
            m.note_recorded_frame_painted();
            let _ = update(&mut m, key(answer));
            assert!(
                prompt_text(&m).is_none(),
                "{answer}: the key answers the confirm"
            );
            assert!(
                m.dvr.scrub_frame().is_none(),
                "{answer}: the answer closes it"
            );
            assert_eq!(m.dvr.is_branching(), branches, "{answer}");
        }
    }

    #[test]
    fn a_click_or_paste_under_the_branch_confirm_never_reaches_the_engine() {
        let mut m = recorded();
        branch_back(&mut m, 2);
        checked(&mut m, &[], false);
        m.note_recorded_frame_painted();
        let logged = m.dvr.inputs().count();
        let mut effects = update(&mut m, Msg::Paste("text".to_owned()));
        effects.extend(update(
            &mut m,
            Msg::Mouse(MouseInput {
                button: "left".to_owned(),
                action: "press".to_owned(),
                modifier: String::new(),
                row: 1,
                col: 1,
            }),
        ));
        assert!(effects.is_empty(), "{effects:?}");
        assert_eq!(m.dvr.inputs().count(), logged, "nothing is recorded");
        assert!(prompt_text(&m).is_some(), "the confirm stays up");
        assert_eq!(m.dvr.scrub_frame(), Some(7), "over the frame it names");
    }

    #[test]
    fn backing_out_during_the_disk_check_cancels_the_confirm() {
        for close in ["q", "<Esc>"] {
            let mut m = recorded();
            branch_back(&mut m, 2);
            let _ = update(&mut m, key(close));
            assert!(m.dvr.scrub_frame().is_none(), "{close}");
            assert!(!m.dvr.is_asking(), "{close}: the ask ends with the scrub");
            checked(&mut m, &[], false);
            assert!(prompt_text(&m).is_none(), "{close}: no confirm on live");

            let _ = update(&mut m, invoke_msg("scrub"));
            let _ = m.dvr.take_step();
            checked(&mut m, &[], false);
            assert!(prompt_text(&m).is_none(), "{close}: nor on a new scrub");
            let _ = update(&mut m, key("h"));
            assert!(m.dvr.take_step().is_some(), "{close}: the scrub moves");
        }
    }

    #[test]
    fn only_closing_acts_while_the_disk_check_runs() {
        let mut m = recorded();
        branch_back(&mut m, 2);
        assert_eq!(disk_checks(&mut m), 1);
        let _ = std::iter::from_fn(|| m.dvr.take_step()).count();
        let mut effects = Vec::new();
        for k in ["e", "b", "h", "l", "g", "G"] {
            effects.extend(update(&mut m, key(k)));
        }
        assert!(effects.is_empty(), "{effects:?}");
        assert!(m.dvr.take_step().is_none(), "no move while asking");
        assert!(exports(&mut m).is_empty(), "no export while asking");
        assert_eq!(disk_checks(&mut m), 0, "no second check");
        assert_eq!(m.dvr.scrub_frame(), Some(7));
        for close in ["q", "<Esc>"] {
            let mut m = recorded();
            branch_back(&mut m, 2);
            let _ = update(&mut m, key(close));
            assert!(m.dvr.scrub_frame().is_none(), "{close} closes");
        }
    }

    #[test]
    fn a_confirm_under_a_prompt_the_engine_waits_on_closes_the_scrub() {
        let mut m = recorded();
        branch_back(&mut m, 2);
        let engine =
            PromptState::external_write_conflict_prompt("/w/a.rs".into(), "Reload?".into());
        m.push_overlay(engine.overlay_box(), OverlayKind::Prompt(engine));
        checked(&mut m, &[], false);
        assert!(
            m.dvr.scrub_frame().is_none(),
            "the live prompt is the one on screen"
        );
    }

    #[test]
    fn branch_across_an_engine_restart_asks_first() {
        let mut m = recorded();
        m.dvr.note_frame(4, 2);
        m.dvr.note_restart();
        m.dvr.note_frame(9, 2);
        branch_back(&mut m, 3);
        checked(&mut m, &[], false);
        let text = prompt_text(&m).unwrap();
        assert!(text.contains("restarted once"), "{text}");

        let mut before = recorded();
        before.dvr.note_frame(4, 2);
        before.dvr.note_restart();
        before.dvr.note_frame(9, 2);
        branch_back(&mut before, 6);
        checked(&mut before, &[], false);
        let text = prompt_text(&before).unwrap();
        assert!(!text.contains("restarted"), "{text}");
    }

    #[test]
    fn replayed_dvr_keys_are_absorbed() {
        let mut m = recorded();
        for k in [" ", "f", "v"] {
            let _ = update(&mut m, key(k));
        }
        let _ = update(&mut m, invoke_msg("scrub"));
        let _ = update(&mut m, key("q"));
        m.dvr.note_frame(10, 2);
        let _ = update(&mut m, key("x"));
        m.dvr.note_frame(11, 2);
        branched(&mut m, 11, 11);
        assert_eq!(
            m.dvr.inputs().count(),
            5,
            "the replay is not logged twice, and the closing size is"
        );
        let _ = update(&mut m, invoke_msg("scrub"));
        assert!(
            m.dvr.scrub_frame().is_none(),
            "the scrub the replayed keys open again is absorbed"
        );
        let _ = update(&mut m, invoke_msg("scrub"));
        assert!(
            m.dvr.scrub_frame().is_some(),
            "a scrub asked for afresh opens"
        );
    }

    /// Stages a branch from frame `at` that waits for the new engine's
    /// attach, the state between the replacement and its `VimEnter`.
    fn staged(m: &mut Model, at: u64) {
        m.dvr.open_scrub();
        m.dvr.show(at);
        let _ = update(m, key("b"));
        checked(m, &[], false);
        m.note_frame_painted();
        let _ = update(m, key("y"));
        let plan = std::iter::from_fn(|| m.dvr.take_request())
            .find_map(|r| match r {
                DvrRequest::Branch(plan) => Some(plan),
                _ => None,
            })
            .expect("y queues the branch");
        m.dvr.branched(plan.at_frame, plan.replay);
        m.rearm_attach();
    }

    #[test]
    fn a_replay_waits_for_the_attach() {
        let mut m = recorded();
        let _ = update(&mut m, key("x"));
        m.dvr.note_frame(10, 2);
        staged(&mut m, 10);
        assert!(crate::update::due_replay(&mut m).is_empty());
        assert!(m.dvr.has_replay());
        let _ = m.takes_attach();
        let replay = crate::update::due_replay(&mut m);
        assert!(format!("{:?}", replay[0]).contains("\"x\""), "{replay:?}");
        assert!(!m.dvr.has_replay());
        assert!(crate::update::due_replay(&mut m).is_empty(), "once");
    }

    #[test]
    fn keys_typed_while_a_replay_is_owed_land_after_it() {
        let mut m = recorded();
        let _ = update(&mut m, key("x"));
        m.dvr.note_frame(10, 2);
        staged(&mut m, 10);
        let mut early = update(&mut m, key("i"));
        early.extend(update(&mut m, Msg::Paste("p".to_owned())));
        assert!(early.is_empty(), "{early:?}");
        assert_eq!(
            m.dvr.inputs().count(),
            1,
            "nothing typed meanwhile is logged"
        );
        let _ = m.takes_attach();
        let mut sent = Vec::new();
        for msg in crate::update::due_replay(&mut m) {
            for effect in update(&mut m, msg) {
                if let Effect::Rpc(RpcCall::Input { notation }) = effect {
                    sent.push(notation);
                }
            }
        }
        assert_eq!(sent, ["x", "i"], "the replay reaches the engine first");
        let logged: Vec<_> = m.dvr.inputs().map(|i| i.kind).collect();
        use crate::native::dvr::InputKind;
        assert_eq!(
            logged,
            [
                InputKind::Key,
                InputKind::Resized,
                InputKind::Key,
                InputKind::Paste
            ],
            "the log reads in the order the engine got them"
        );
    }

    fn loaded(path: &str) -> Msg {
        Msg::DvrIo(DvrIoReply::ClipLoaded {
            path: path.to_owned(),
            left_out: 0,
        })
    }

    fn plays(m: &mut Model) -> Vec<String> {
        std::iter::from_fn(|| m.dvr.take_request())
            .filter_map(|r| match r {
                DvrRequest::Play(path) => Some(path),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_play_while_a_clip_loads_is_refused_naming_it() {
        let mut m = recorded();
        let _ = update(&mut m, invoke_msg("play /w/a.vdvr"));
        let _ = update(&mut m, invoke_msg("play /w/b.vdvr"));
        assert_eq!(plays(&mut m), ["/w/a.vdvr"], "one clip is read at a time");
        assert!(
            told_once(&m, "view: DVR play busy: /w/a.vdvr is still being read"),
            "{:?}",
            m.engine.messages.entries
        );
        let _ = update(&mut m, loaded("/w/a.vdvr"));
        let _ = update(&mut m, invoke_msg("play /w/c.vdvr"));
        assert_eq!(plays(&mut m), ["/w/c.vdvr"], "a loaded clip ends the load");
        let failed = DvrIoReply::Failed {
            verb: "play",
            reason: "/w/c.vdvr: not a file".to_owned(),
        };
        let _ = update(&mut m, Msg::DvrIo(failed));
        let _ = update(&mut m, invoke_msg("play /w/d.vdvr"));
        assert_eq!(plays(&mut m), ["/w/d.vdvr"], "a failed read ends the load");
    }

    #[test]
    fn a_play_closes_the_clip_open_before_it_queues() {
        let mut m = recorded();
        let _ = update(&mut m, loaded("/w/a.vdvr"));
        let _ = update(&mut m, invoke_msg("play /w/b.vdvr"));
        assert!(m.dvr.clip().is_none(), "the open clip is closed first");
        assert_eq!(plays(&mut m), ["/w/b.vdvr"]);
    }

    #[test]
    fn a_clip_cut_to_fit_says_how_many_oldest_frames_were_left_out() {
        let mut m = recorded();
        let cut = DvrIoReply::ClipLoaded {
            path: "/w/a.vdvr".to_owned(),
            left_out: 40,
        };
        let _ = update(&mut m, Msg::DvrIo(cut));
        assert_eq!(m.dvr.clip(), Some("/w/a.vdvr"));
        assert!(
            told_once(
                &m,
                "view: DVR clip /w/a.vdvr: 40 oldest frames past [dvr] max_mb \
                 were not loaded"
            ),
            "{:?}",
            m.engine.messages.entries
        );
        let mut whole = recorded();
        let _ = update(&mut whole, loaded("/w/a.vdvr"));
        assert!(!format!("{:?}", whole.engine.messages.entries).contains("not loaded"));
    }

    #[test]
    fn a_verb_run_after_the_input_log_filled_is_never_owed_to_a_branch() {
        let mut m = recorded();
        let _ = update(&mut m, key("x"));
        m.dvr.note_frame(10, 2);
        // the input that fills the log is the first after frame 10, so
        // every logged input comes before it
        let _ = update(&mut m, Msg::Paste("p".repeat(4 << 20)));
        assert_eq!(m.dvr.overflowed_at(), Some(10));
        m.dvr.note_frame(12, 2);
        let _ = update(&mut m, invoke_msg("scrub"));
        let _ = update(&mut m, key("q"));
        branched(&mut m, 10, 12);
        let _ = update(&mut m, invoke_msg("scrub"));
        assert!(
            m.dvr.scrub_frame().is_some(),
            "the first scrub asked for after the branch opens"
        );
    }

    #[test]
    fn play_queues_a_clip_and_a_loaded_clip_opens_in_the_scrub() {
        let mut m = recorded();
        let _ = update(&mut m, invoke_msg("play"));
        assert!(
            told_once(&m, "DVR play needs a clip"),
            "{:?}",
            m.engine.messages.entries
        );
        let _ = update(&mut m, invoke_msg("play /w/a b.vdvr"));
        let plays: Vec<_> = std::iter::from_fn(|| m.dvr.take_request())
            .filter_map(|r| match r {
                DvrRequest::Play(path) => Some(path),
                _ => None,
            })
            .collect();
        assert_eq!(plays, ["/w/a b.vdvr"]);
        assert!(
            m.dvr.clip().is_none(),
            "nothing opens before the clip is read"
        );
        m.dirty = false;
        let _ = update(&mut m, loaded("/w/a b.vdvr"));
        assert!(m.dirty);
        assert_eq!(m.dvr.clip(), Some("/w/a b.vdvr"));
        assert!(m.dvr.scrub_frame().is_some());
        assert_eq!(m.dvr.take_step(), Some(ScrubStep::Newest));
        assert!(m.dvr.take_opened_clip());
        assert!(!m.dvr.take_opened_clip(), "once");
        let _ = update(&mut m, key("q"));
        assert!(m.dvr.clip().is_none() && m.dvr.scrub_frame().is_none());
    }

    #[test]
    fn a_clip_refuses_branch_and_export() {
        for refused in [key("b"), key("e"), invoke_msg("export /w/b.vdvr")] {
            let mut m = recorded();
            let _ = update(&mut m, loaded("/w/a.vdvr"));
            let _ = update(&mut m, refused.clone());
            let queued: Vec<_> = std::iter::from_fn(|| m.dvr.take_request()).collect();
            assert!(queued.is_empty(), "{refused:?}: {queued:?}");
            assert!(
                told_once(&m, "not available in a clip"),
                "{refused:?}: {:?}",
                m.engine.messages.entries
            );
            assert!(m.dvr.clip().is_none(), "the refusal shows the live screen");
        }
    }

    #[test]
    fn a_replayed_verb_that_never_fires_swallows_one_verb_of_its_word() {
        let mut m = recorded();
        let _ = update(&mut m, invoke_msg("scrub"));
        let _ = update(&mut m, key("q"));
        let _ = update(&mut m, key("x"));
        m.dvr.note_frame(10, 2);
        branched(&mut m, 10, 10);
        let _ = update(&mut m, key("j"));
        let _ = update(&mut m, invoke_msg("scrub"));
        assert!(m.dvr.scrub_frame().is_none(), "the owed scrub is swallowed");
        assert!(exports(&mut m).is_empty(), "nothing is written");
        let _ = update(&mut m, invoke_msg("scrub"));
        assert!(
            m.dvr.scrub_frame().is_some(),
            "the next scrub asked for opens"
        );
    }

    #[test]
    fn a_replayed_export_is_absorbed_however_late_it_arrives() {
        let mut m = recorded();
        let _ = update(&mut m, key("x"));
        let _ = update(&mut m, invoke_msg("export a.vdvr"));
        let _ = exports(&mut m);
        m.dvr.note_frame(10, 2);
        branched(&mut m, 10, 10);
        let _ = update(
            &mut m,
            Msg::Resized {
                width: 100,
                height: 30,
            },
        );
        let _ = update(&mut m, key("j"));
        let _ = update(&mut m, invoke_msg("export a.vdvr"));
        assert!(
            exports(&mut m).is_empty(),
            "the replayed export runs nothing"
        );
        let _ = update(&mut m, invoke_msg("export b.vdvr"));
        assert_eq!(exports(&mut m), [Some("b.vdvr".to_owned())]);
    }

    #[test]
    fn a_verb_of_another_word_runs_while_one_is_owed() {
        let mut m = recorded();
        let _ = update(&mut m, invoke_msg("export a.vdvr"));
        let _ = exports(&mut m);
        let _ = update(&mut m, key("x"));
        m.dvr.note_frame(10, 2);
        branched(&mut m, 10, 10);
        let _ = update(&mut m, invoke_msg("scrub"));
        assert!(
            m.dvr.scrub_frame().is_some(),
            "a scrub opens past an owed export"
        );
        let _ = update(&mut m, key("q"));
        let _ = update(&mut m, invoke_msg("export c.vdvr"));
        assert_eq!(
            exports(&mut m),
            [Some("c.vdvr".to_owned())],
            "the mismatch dropped what was owed"
        );
    }
}
