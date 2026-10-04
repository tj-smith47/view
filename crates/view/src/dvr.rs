//! The session DVR on the loop thread: records each frame the terminal was
//! sent, and paints the recorded frame the scrub shows in place of the live
//! screen.

use std::time::Instant;

use view_core::model::{Model, OverlayKind};
use view_core::msg::Effect;
use view_core::native::dvr::SCRUB_HINT;
use view_tui::dvr::FrameRing;
use view_tui::terminal::Term;

/// What view says, once, when the ring could not keep a frame.
const REFUSED: &str = "view: DVR is skipping frames it has no room for: raise [dvr] max_mb";

/// The painted frames and the clock they are dated by.
pub(crate) struct DvrLoop {
    ring: FrameRing,
    started: Instant,
    refused_told: bool,
    /// The recorded frame on screen, the size it was painted at and whether
    /// its bar said something waits, so a pass that changed none of them
    /// writes nothing.
    painted: Option<(u64, (u16, u16), bool)>,
}

impl DvrLoop {
    /// The loop's recorder, or `None` when the session is not recorded.
    pub(crate) fn start(model: &Model) -> Option<Self> {
        model.dvr.is_recording().then(|| Self {
            ring: FrameRing::new(model.dvr.max_bytes()),
            started: Instant::now(),
            refused_told: false,
            painted: None,
        })
    }

    /// Resolves the scrub moves the keys asked for against the ring.
    pub(crate) fn poll(&mut self, model: &mut Model) {
        while let Some(step) = model.dvr.take_step() {
            if let Some(from) = model.dvr.scrub_frame() {
                model.dvr.show(self.ring.resolve(from, step));
            }
        }
    }

    /// Settles the model for one scrub pass and names the recorded frame
    /// and bar to paint. `None` on the live screen, `Some(None)` when the
    /// frame on screen is still the one to show.
    ///
    /// The pass clears `dirty` and marks no question seen: the terminal
    /// shows a recorded frame, so a prompt that opened meanwhile reads keys
    /// only once a live frame carrying it is painted.
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
        let shown = (seq, (model.term_width, model.term_height), waits);
        if self.painted == Some(shown) {
            return Some(None);
        }
        self.painted = Some(shown);
        Some(Some((seq, self.bar(seq, waits))))
    }

    /// Records the frame the live paint just wrote. Returns the notice owed
    /// the first time the ring could not keep one.
    pub(crate) fn after_paint(&mut self, term: &Term, model: &mut Model) -> Vec<Effect> {
        let seq = term.record_frame(&mut self.ring, self.started.elapsed());
        self.recorded(seq, model)
    }

    /// Notes a frame the ring kept, or raises the one-time notice when it
    /// kept none.
    fn recorded(&mut self, seq: Option<u64>, model: &mut Model) -> Vec<Effect> {
        match seq {
            Some(seq) => {
                model.dvr.note_frame(seq, self.ring.oldest().unwrap_or(seq));
                Vec::new()
            }
            None if !self.refused_told => {
                self.refused_told = true;
                model.dirty = true;
                model
                    .engine
                    .record_native_notice(REFUSED.to_string(), false)
            }
            None => Vec::new(),
        }
    }

    /// The scrub bar for frame `seq`: how far back it is and how far back
    /// the recording reaches, then the way out and the keys.
    fn bar(&self, seq: u64, waiting: bool) -> String {
        let secs = |s| self.ring.age(s).unwrap_or_default().as_secs_f64();
        let age = secs(seq);
        let reach = secs(self.ring.oldest().unwrap_or(seq));
        let flag = if waiting { WAITING } else { "" };
        // the flag goes ahead of the legend, since a narrow terminal cuts
        // the bar's end
        format!("DVR  -{age:.1}s of {reach:.1}s{flag}  {SCRUB_HINT}")
    }
}

/// What one paint pass puts on the terminal.
pub(crate) enum Draw<'a> {
    /// A recorded frame of `ring` with the scrub bar over it.
    Recorded {
        ring: &'a FrameRing,
        seq: u64,
        bar: &'a str,
    },
    /// The live screen.
    Live,
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

/// Runs one paint pass through `draw`: the frame the scrub shows while it
/// is open, the live screen otherwise.
///
/// Only a live frame settles the model through
/// [`crate::runtime::frame_reached_terminal`]. A recorded frame does not
/// carry the live screen, so a question that opened under the scrub reads
/// keys once a live frame showing it is painted.
pub(crate) fn paint_pass(
    model: &mut Model,
    dvr: Option<&mut DvrLoop>,
    draw: impl FnOnce(&mut Model, Draw<'_>) -> std::io::Result<bool>,
) -> std::io::Result<Painted> {
    if let Some(dvr) = dvr {
        match dvr.scrub_pass(model) {
            Some(None) => return Ok(Painted::Recorded(false)),
            Some(Some((seq, bar))) => {
                let ring = &dvr.ring;
                let shown = Draw::Recorded {
                    ring,
                    seq,
                    bar: &bar,
                };
                return draw(model, shown).map(Painted::Recorded);
            }
            None => {}
        }
    }
    let wrote = draw(model, Draw::Live)?;
    crate::runtime::frame_reached_terminal(model);
    Ok(Painted::Live(wrote))
}

/// What the bar adds while something on the live screen waits for an answer.
const WAITING: &str = "  ! waiting: q to answer";

/// Whether the live screen holds a question or a recovery offer the person
/// has not seen while the scrub covers it.
fn waiting(model: &Model) -> bool {
    model.ai_panel().pending_permission.is_some()
        || model
            .overlays()
            .iter()
            .any(|o| matches!(o.kind, OverlayKind::Prompt(_) | OverlayKind::EngineBusy(_)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use view_core::events::UiEvent;
    use view_core::grid::GridOp;
    use view_core::msg::{Key, Msg, RpcCall};
    use view_core::update::update;
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
        DvrLoop {
            ring: builder.finish(),
            started: Instant::now(),
            refused_told: false,
            painted: None,
        }
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
            dvr.bar(5, false),
            "DVR  -0.4s of 2.0s  q close  h/l frame  H/L 1s  g/G ends"
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
        assert!(DvrLoop::start(&Model::with_term_size(80, 24)).is_none());
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
            paint_pass(model, Some(dvr), |_, draw| {
                if let Draw::Recorded { bar: shown, .. } = draw {
                    shown.clone_into(bar);
                }
                Ok(true)
            })
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
}
