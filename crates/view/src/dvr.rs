//! The session DVR on the loop thread: records each frame the terminal was
//! sent, and paints the recorded frame the scrub shows in place of the live
//! screen.

use std::time::Instant;

use view_core::model::Model;
use view_core::msg::Effect;
use view_tui::dvr::FrameRing;
use view_tui::terminal::Term;

/// What view says, once, when the ring could not keep a frame.
const REFUSED: &str = "view: DVR is skipping frames it has no room for: raise [dvr] max_mb";

/// The painted frames and the clock they are dated by.
pub(crate) struct DvrLoop {
    ring: FrameRing,
    started: Instant,
    refused_told: bool,
    /// The recorded frame on screen and the size it was painted at, so a
    /// pass that changed neither writes nothing.
    painted: Option<(u64, (u16, u16))>,
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

    /// Paints the frame the scrub shows, with the scrub bar on its last
    /// row. `None` on the live screen; otherwise whether bytes were written.
    pub(crate) fn paint_scrub(
        &mut self,
        term: &mut Term,
        model: &mut Model,
    ) -> std::io::Result<Option<bool>> {
        let Some(seq) = model.dvr.scrub_frame() else {
            self.painted = None;
            return Ok(None);
        };
        // the live screen's damage is spent here, since the frame that
        // closes the scrub repaints everything
        let _ = model.take_paint_damage();
        let shown = (seq, (model.term_width, model.term_height));
        if self.painted == Some(shown) {
            return Ok(Some(false));
        }
        let wrote = term.draw_recorded(model, &self.ring, seq, &self.bar(seq))?;
        self.painted = Some(shown);
        Ok(Some(wrote))
    }

    /// Records the frame the live paint just wrote. Returns the notice owed
    /// the first time the ring could not keep one.
    pub(crate) fn after_paint(&mut self, term: &Term, model: &mut Model) -> Vec<Effect> {
        match term.record_frame(&mut self.ring, self.started.elapsed()) {
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

    /// The scrub bar for frame `seq`.
    fn bar(&self, seq: u64) -> String {
        let age = self.ring.age(seq).unwrap_or_default().as_secs_f64();
        let oldest = self.ring.oldest().unwrap_or(seq);
        format!(
            "DVR  -{age:.1}s  frame {seq} (oldest {oldest})  h/l frame  H/L 1s  g/G ends  q close"
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use view_core::msg::{Key, Msg};
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
            dvr.bar(5),
            "DVR  -0.4s  frame 5 (oldest 1)  h/l frame  H/L 1s  g/G ends  q close"
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
}
