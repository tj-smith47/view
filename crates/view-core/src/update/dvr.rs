//! The session DVR's `:View dvr` verbs and the keys the scrub answers while
//! the screen shows a recorded frame.

use crate::model::Model;
use crate::msg::{Effect, Msg};
use crate::native::dvr::ScrubStep;

/// What `:View dvr` answers while recording is off.
const OFF: &str = "view: DVR is off: set [dvr] enabled = true";

/// What `:View dvr scrub` answers before the first frame is recorded.
const EMPTY: &str = "view: DVR has recorded no frame yet";

/// Every key the scrub answers, and what it does. `docs/keymaps.md`
/// carries the rendered table. Test-only: the scrub matches on the keys
/// themselves.
#[cfg(test)]
pub(crate) const DVR_KEYS: [(&str, &str); 7] = [
    ("h", "one frame back"),
    ("l", "one frame forward"),
    ("H", "one second back"),
    ("L", "one second forward"),
    ("g", "the oldest frame kept"),
    ("G", "the newest frame"),
    ("q", "back to the live screen, as `<Esc>` does"),
];

/// Answers `:View dvr <verb>`.
pub(super) fn invoke(model: &mut Model, verb: &str) -> Vec<Effect> {
    model.dirty = true;
    if !model.dvr.is_recording() {
        return model.engine.record_native_notice(OFF.to_string(), false);
    }
    model.dvr.mark_invoke();
    match verb {
        "scrub" if !model.dvr.open_scrub() => {
            model.engine.record_native_notice(EMPTY.to_string(), false)
        }
        "scrub" => Vec::new(),
        "close" => {
            model.dvr.close_scrub();
            Vec::new()
        }
        _ => model
            .engine
            .record_native_notice(super::feature_invoke_notice("dvr", verb, false), false),
    }
}

/// Takes every key, paste and click while the scrub is open, so none
/// reaches the engine. `None` on the live screen and for any other message.
pub(super) fn scrub_input(model: &mut Model, msg: &Msg) -> Option<Vec<Effect>> {
    model.dvr.scrub_frame()?;
    match msg {
        Msg::Key(key) => scrub_key(model, &key.notation),
        Msg::Paste(_) | Msg::Mouse(_) => {}
        _ => return None,
    }
    Some(Vec::new())
}

/// Moves the scrub cursor or closes the scrub. Any other key does nothing.
fn scrub_key(model: &mut Model, notation: &str) {
    let step = match notation {
        "h" => ScrubStep::Frames(-1),
        "l" => ScrubStep::Frames(1),
        "H" => ScrubStep::Seconds(-1),
        "L" => ScrubStep::Seconds(1),
        "g" => ScrubStep::Oldest,
        "G" => ScrubStep::Newest,
        "q" | "<Esc>" => {
            model.dvr.close_scrub();
            model.dirty = true;
            return;
        }
        _ => return,
    };
    model.dvr.step(step);
    model.dirty = true;
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
        assert_eq!(named.len(), 7, "{named:?}");
        for k in named {
            assert!(
                DVR_KEYS.iter().any(|(notation, _)| *notation == k),
                "the bar names {k}, which the scrub does not answer"
            );
        }
    }
}
