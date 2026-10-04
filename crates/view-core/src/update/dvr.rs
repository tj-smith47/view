//! The session DVR's `:View dvr` verbs and the keys the scrub answers while
//! the screen shows a recorded frame.

use crate::model::Model;
use crate::msg::{Effect, Msg};
use crate::native::dvr::{DvrIoReply, ExportRefusal, ScrubStep};

/// What `:View dvr` answers while recording is off.
const OFF: &str = "view: DVR is off: set [dvr] enabled = true";

/// What `:View dvr scrub` answers before the first frame is recorded.
const EMPTY: &str = "view: DVR has recorded no frame yet";

/// Every key the scrub answers, and what it does. `docs/keymaps.md`
/// carries the rendered table. Test-only: the scrub matches on the keys
/// themselves.
#[cfg(test)]
pub(crate) const DVR_KEYS: [(&str, &str); 8] = [
    ("h", "one frame back"),
    ("l", "one frame forward"),
    ("H", "one second back"),
    ("L", "one second forward"),
    ("g", "the oldest frame kept"),
    ("G", "the newest frame"),
    ("q", "back to the live screen, as `<Esc>` does"),
    ("e", "the recording written to a clip file, then live"),
];

/// Answers `:View dvr <verb>`.
pub(super) fn invoke(model: &mut Model, verb: &str) -> Vec<Effect> {
    model.dirty = true;
    if !model.dvr.is_recording() {
        return model.engine.record_native_notice(OFF.to_string(), false);
    }
    model.dvr.mark_invoke();
    // the path is every byte after the first blank run, as typed
    let (word, path) = verb.split_once(char::is_whitespace).unwrap_or((verb, ""));
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
        _ => model
            .engine
            .record_native_notice(super::feature_invoke_notice("dvr", verb, false), false),
    }
}

/// Closes the scrub and queues an export to `path`, or to a derived path
/// when it is empty.
fn export(model: &mut Model, path: &str) -> Vec<Effect> {
    model.dvr.close_scrub();
    model.dirty = true;
    let path = (!path.is_empty()).then(|| path.to_owned());
    if model.dvr.request_export(path) {
        return Vec::new();
    }
    on_io(model, &DvrIoReply::Refused(ExportRefusal::NoFrame))
}

/// Raises the notice a reply from the DVR's file work carries.
pub(super) fn on_io(model: &mut Model, reply: &DvrIoReply) -> Vec<Effect> {
    let Some(text) = reply.notice() else {
        return Vec::new();
    };
    model.dirty = true;
    model.engine.record_native_notice(text, false)
}

/// Takes every key, paste and click while the scrub is open, so none
/// reaches the engine. `None` on the live screen and for any other message.
pub(super) fn scrub_input(model: &mut Model, msg: &Msg) -> Option<Vec<Effect>> {
    model.dvr.scrub_frame()?;
    match msg {
        Msg::Key(key) => Some(scrub_key(model, &key.notation)),
        Msg::Paste(_) | Msg::Mouse(_) => Some(Vec::new()),
        _ => None,
    }
}

/// Moves the scrub cursor, closes the scrub or exports the recording. Any
/// other key does nothing.
fn scrub_key(model: &mut Model, notation: &str) -> Vec<Effect> {
    let step = match notation {
        "h" => ScrubStep::Frames(-1),
        "l" => ScrubStep::Frames(1),
        "H" => ScrubStep::Seconds(-1),
        "L" => ScrubStep::Seconds(1),
        "g" => ScrubStep::Oldest,
        "G" => ScrubStep::Newest,
        "e" => return export(model, ""),
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
    use crate::native::dvr::Marker;
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
            let closed = m.dvr.scrub_frame().is_none();
            assert!(
                closed != m.dvr.take_step().is_some(),
                "{notation} neither moved nor closed the scrub"
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
        assert_eq!(named.len(), 8, "{named:?}");
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
}
