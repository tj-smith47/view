//! The differential battery for display-only speculative echo: proof that a
//! predicted glyph diverges from nvim only where the feature is allowed to,
//! and never past the settle that answers it.
//!
//! # What is compared, and against what
//!
//! Every case here ends by diffing view's own rendered screen against nvim's
//! *own* screen, read back out of the same engine that produced the redraw
//! stream (`screenstring()` over every cell). That is a different authority
//! from the corpus runner's [`ReferenceSession`](crate::ReferenceSession),
//! which re-applies the redraw stream with a second, naive applier: the
//! question a corpus entry asks is whether two appliers agree about the
//! traffic, and the question here is whether view is showing anything nvim
//! is not. A speculated glyph is precisely a cell view paints that no redraw
//! ever carried, so the side that has to be believed is nvim's screen, not a
//! second reading of the same wire.
//!
//! # The bounded exemption, and its closure
//!
//! [`masked_rows`] masks every non-`EngineGrid` layer's rows, so a row
//! carrying pending predictions is exempt from the corpus diff while they are
//! pending. That exemption is the feature working, not a hole, and each case
//! here closes it three ways:
//!
//! - the predicted row is masked *while* the layer is up, and unmasked the
//!   moment it comes down ([`SpecReport::masked_while_pending`] and
//!   [`SpecReport::masked_after_settle`]) -- the mask can never outlive the
//!   layer, because it is derived from the layer list itself;
//! - the settled diff runs over that row UNMASKED
//!   ([`SpecReport::divergences`]), so an answered prediction that left a
//!   glyph behind is a reported divergence rather than a masked one;
//! - [`SpecCase::Mispredict`] shows a glyph nvim then contradicts, proving
//!   the window closes even when the guess was wrong.
//!
//! A divergence is never retried, re-settled or explained away: whatever the
//! unmasked diff finds at the settle point is what the report carries, and
//! [`SpecReport::is_success`] is false whenever the list is non-empty.
//!
//! # What this module owns, and what comes from production code
//!
//! The state machine, the three folds that drive it
//! (`view_core::native::speculate::fold_engine_call`, `fold_redraw`,
//! `fold_expiry`) and the paint (`view_surface`'s own `Speculated` layer and
//! caret) are all production code, called here by the same names the runtime
//! calls them by. That is deliberate and it is the point: a battery holding
//! its own copy of the classification would be free to agree only with
//! itself, and no pin between two copies can make them equal. What this
//! module owns is the *schedule*: when a burst is typed, when its answer is
//! allowed to arrive, which invalidation fires between the two, and how long
//! an unanswered prediction is left alone before the age bound is expected
//! to take it.

use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use view_core::events::{UiEvent, WinHandle};
use view_core::model::Model;
use view_core::msg::{Effect, Msg, RpcCall};
use view_core::native::speculate::{
    fold_engine_call, fold_expiry, fold_redraw, SpecStamp, SPECULATION_MAX_AGE,
};
use view_core::update::update;
use view_engine::handle::EngineHandle;
use view_engine::process::{Engine, EngineConfig};
use view_engine::DamagePump;
use view_surface::{LayerKind, Surface, SurfaceCache};

use crate::parity::{diff_rows, ReferenceRows, ViewRows, RECORD_SEP};
use crate::{masked_rows, pump_rpc, raster, settle, Divergence, OracleError};

#[cfg(test)]
mod tests;

/// Terminal size every case runs at, matching the corpus runner's own fixed
/// canvas so a report line from either reads against the same geometry.
const COLS: u16 = 60;
const ROWS: u16 = 12;

/// The quiesce window every case settles on, matching the corpus runner's
/// own defaults: a settle point that meant something different here would
/// make a divergence found by one runner unreproducible in the other.
const SILENCE: Duration = Duration::from_millis(200);
const DEADLINE: Duration = Duration::from_secs(5);

/// How many lines the scrolling case's buffer is filled with: comfortably
/// more than [`ROWS`], so a window scrolled down by a few lines has content
/// to scroll to and nvim answers with a `grid_scroll` rather than a repaint
/// of an unchanged screen.
const SCROLL_BUFFER_LINES: u16 = 200;

/// The vimscript that reads nvim's own screen back, one row per record.
///
/// `screenstring` rather than `screenchar`: it returns the cell's text
/// including any composing characters, which is the same thing view's raster
/// writes into its own canvas, so the two sides are comparable without a
/// second decode step on either. The rows are joined with the record
/// separator the state probes already use, a control byte no screen cell can
/// hold.
const SCREEN_EXPR: &str = "join(map(range(1, &lines), {_, r -> \
     join(map(range(1, &columns), {_, c -> screenstring(r, c)}), '')}), nr2char(30))";

/// nvim's own report of where it is showing the caret, one-based, as the
/// authority a settled speculative caret is held against.
const CARET_EXPR: &str = "screencol()";

/// One battery case: a schedule, and the divergence class it exists to make
/// impossible.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecCase {
    /// A burst typed faster than the engine answers, whose answer is one
    /// full-line redraw covering the burst's head. Reconciliation retires
    /// the whole run, tail included, and the tail's cells must then read
    /// from the authoritative line rather than from an orphaned prediction.
    ///
    /// First, because it is the case the round trip the feature hides is
    /// actually made of: every other case is a burst plus one more thing.
    BurstTail,
    /// One character, predicted and then answered on its own. The
    /// convergence case: what a prediction claims and what the engine
    /// afterwards shows are the same cell.
    StraightLine,
    /// A burst nvim contradicts: an insert-mode abbreviation rewrites the
    /// typed text, so glyphs that were shown speculatively are not the ones
    /// the buffer ends up holding.
    Mispredict,
    /// A burst invalidated before its own answer arrives, so the redraw for
    /// it lands after the predictions it would have confirmed are already
    /// gone. The reordering case: a late batch may not resurrect anything.
    LateRedraw,
    /// A burst answered by a scroll rather than by its own cells: content
    /// moves out from under the predictions without any batch saying so cell
    /// by cell.
    Scroll,
    /// A burst answered by a full-screen clear.
    Clear,
    /// A burst answered by a resize, which is also the case where a
    /// prediction can be left naming a cell the grid no longer has.
    Resize,
    /// A burst invalidated by a paste: text reaches the buffer without any
    /// character reaching the predictor.
    Paste,
    /// A burst invalidated by a mouse click: the cursor moves without any
    /// character reaching the predictor.
    Mouse,
    /// A burst whose window moves under it in a redraw that repaints line by
    /// line instead of scrolling, so no cell in the batch answers the
    /// predicted ones. What retires them is the viewport report itself, the
    /// only thing on the wire that says the predicted coordinates now name
    /// different buffer lines.
    ViewportShift,
    /// A prediction the engine never answers at all, left to the age bound.
    /// The only case whose retirement is a clock rather than a redraw.
    AgeBound,
}

impl SpecCase {
    /// Every case a bare battery run drives, burst-tail first.
    #[must_use]
    pub const fn all() -> [Self; 11] {
        [
            Self::BurstTail,
            Self::StraightLine,
            Self::Mispredict,
            Self::LateRedraw,
            Self::Scroll,
            Self::Clear,
            Self::Resize,
            Self::Paste,
            Self::Mouse,
            Self::ViewportShift,
            Self::AgeBound,
        ]
    }

    /// The case's name on a report line and on the runner's own selector.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BurstTail => "burst-tail",
            Self::StraightLine => "straight-line",
            Self::Mispredict => "mispredict",
            Self::LateRedraw => "late-redraw",
            Self::Scroll => "scroll",
            Self::Clear => "clear",
            Self::Resize => "resize",
            Self::Paste => "paste",
            Self::Mouse => "mouse",
            Self::ViewportShift => "viewport-shift",
            Self::AgeBound => "age-bound",
        }
    }
}

/// What one case observed while its predictions were on screen.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct Painted {
    /// How many predictions were pending.
    pub pending: usize,
    /// The surface rows the `Speculated` layer occupied.
    pub rows: Vec<u16>,
    /// What the first of those rows was showing, speculated glyphs included.
    pub text: String,
    /// Whether every one of those rows was masked from the parity diff.
    pub masked: bool,
    /// The caret column view painted -- `None` for a frame carrying no
    /// caret at all, which is a state no case may settle from and so is
    /// kept distinguishable from column 0 -- and the engine's own
    /// last-known one.
    pub caret: (Option<u16>, u16),
}

/// What one case observed after its settle point.
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct Settled {
    /// Whether a `Speculated` layer is still in the frame.
    pub layer: bool,
    /// Whether any previously predicted row is still masked.
    pub masked: bool,
    /// How many predictions are still pending.
    pub pending: usize,
    /// What the first previously predicted row is showing now.
    pub text: String,
    /// The caret column view painted (see [`Painted::caret`] for why it is
    /// an option), the engine's own, and nvim's own report of where it is
    /// showing the caret.
    pub caret: (Option<u16>, u16, u16),
}

/// What one case's run observed, in the order a report line reads it.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct SpecReport {
    /// The case that produced it.
    pub case: SpecCase,
    /// The state while the predictions were painted.
    pub painted: Painted,
    /// The state after the settle point that answered them.
    pub settled: Settled,
    /// Whether the session reached quiescence before its deadline.
    pub quiesced: bool,
    /// Whether the case's own divergence class actually fired. A case whose
    /// schedule silently failed to produce the condition it exists for
    /// proves nothing, so it fails rather than passing vacuously.
    pub fired: bool,
    /// Every disagreement between view's settled screen and nvim's own,
    /// over the rows the settled frame does not mask -- the previously
    /// predicted row among them.
    pub divergences: Vec<Divergence>,
    /// How long an unanswered prediction took to retire, for the one case
    /// whose retirement is the age bound rather than a redraw.
    pub retired_after: Option<Duration>,
    /// How long the whole schedule took.
    pub elapsed: Duration,
}

impl SpecReport {
    /// Whether the case proved what it exists to prove.
    ///
    /// The reading lives beside the report rather than in whatever runs it,
    /// so a manual reproduction and a gated assertion cannot disagree about
    /// what a passing case is.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.quiesced
            && self.fired
            && self.painted.pending > 0
            && self.painted.masked
            && self
                .painted
                .caret
                .0
                .is_some_and(|col| col >= self.painted.caret.1)
            && !self.settled.layer
            && !self.settled.masked
            && self.settled.pending == 0
            && self.settled.caret.0 == Some(self.settled.caret.1)
            && self.settled.caret.0 == Some(self.settled.caret.2)
            && self.divergences.is_empty()
            && self.retirement_is_the_expected_kind()
    }

    /// Whether the case retired its predictions by the mechanism it exists
    /// to exercise, inside the bound when that mechanism is the clock.
    ///
    /// Asserted per case rather than as "if a clock retirement happened it
    /// was in time": that reading passes for a case whose clock retirement
    /// never happened at all, which is exactly the outcome
    /// [`SpecCase::AgeBound`] exists to catch. Every other case retires on a
    /// redraw, so a duration here at all would mean the schedule reached the
    /// bound instead of the answer it was waiting for.
    fn retirement_is_the_expected_kind(&self) -> bool {
        match self.case {
            SpecCase::AgeBound => self
                .retired_after
                .is_some_and(|took| took >= SPECULATION_MAX_AGE && took <= age_deadline()),
            _ => self.retired_after.is_none(),
        }
    }

    /// One case's report line, in the corpus runner's own report shape.
    #[must_use]
    pub fn report_line(&self) -> String {
        let status = if self.is_success() {
            "PARITY"
        } else {
            "DIVERGENCE"
        };
        let aged = self
            .retired_after
            .map_or(String::new(), |took| format!(", aged out after {took:?}"));
        format!(
            "oracle: speculate {} ... {status} ({} predicted, {} divergent, {}ms{aged})",
            self.case.label(),
            self.painted.pending,
            self.divergences.len(),
            self.elapsed.as_millis(),
        )
    }
}

/// How long an unanswered prediction may take to retire: the bound itself,
/// plus what it costs an observer to read the retirement.
///
/// The observation allowance is the supervision battery's own
/// ([`crate::hang::observation_slack`]) rather than a second number: both
/// are the same claim about the host, and a host that needs the one widened
/// needs the other widened by the same factor.
#[must_use]
pub fn age_deadline() -> Duration {
    SPECULATION_MAX_AGE + crate::hang::observation_slack()
}

/// A real engine driven the way the runtime drives one, with the three
/// speculation folds in the three places the runtime folds them.
struct SpecSession {
    engine: Engine,
    pump: DamagePump,
    model: Model,
    cache: SurfaceCache,
    markers: settle::QuiesceMarkers,
    /// The fixed origin every stamp in this session is measured from; two
    /// stamps from different origins say nothing together.
    origin: Instant,
    /// Which redraw kinds this session has applied, so a case can prove the
    /// invalidation class it exists for actually reached the reconciler
    /// instead of passing on a schedule that quietly did nothing.
    seen: Seen,
}

/// The redraw kinds a case's schedule may be waiting on.
#[derive(Debug, Clone, Default)]
struct Seen {
    scroll: bool,
    clear: bool,
    resize: bool,
    line_on_predicted_row: bool,
    /// Whether a window ever reported a `topline` different from the one it
    /// reported before. Tracked here as well as in the state machine, and
    /// deliberately so: this is the observation a case asserts against, and
    /// reading it out of the code under test would make the assertion
    /// circular.
    viewport_moved: bool,
    /// The last `topline` each window reported, for the comparison above.
    toplines: Vec<(WinHandle, u64)>,
}

impl SpecSession {
    /// Spawns an isolated engine, attaches a UI and installs the quiesce
    /// protocol's hooks.
    fn spawn() -> Result<Self, OracleError> {
        let mut engine = Engine::spawn(EngineConfig::isolated())?;
        let (sink, _unused_rx) = sync_channel(64);
        let (pump, _cutover) = engine.start_pump(sink);
        engine.handle.ui_attach(COLS, ROWS)?;
        settle::install_hooks(&engine.handle)?;
        Ok(Self {
            engine,
            pump,
            model: Model::with_term_size(COLS, ROWS),
            cache: SurfaceCache::new(),
            markers: settle::QuiesceMarkers::default(),
            origin: Instant::now(),
            seen: Seen::default(),
        })
    }

    /// Now, as elapsed time since this session's origin.
    fn now(&self) -> SpecStamp {
        SpecStamp::new(self.origin.elapsed())
    }

    /// Applies one drained batch the way the runtime's pass does: reconcile
    /// against the batch, apply it, route its effects back -- including the
    /// follow-up messages a buffer write answers with, which carry a
    /// refusal the model has to see -- then check the age bound.
    fn apply(&mut self, events: Vec<UiEvent>) {
        self.note(&events);
        fold_redraw(&mut self.model, &events);
        let effects = update(&mut self.model, Msg::Redraw(events));
        let now = self.now();
        pump_rpc(
            &self.engine.handle,
            &mut self.model,
            effects,
            |model, effects| {
                for effect in effects {
                    if let Effect::Rpc(call) = effect {
                        fold_engine_call(model, call, now);
                    }
                }
            },
        );
        fold_expiry(&mut self.model, now);
    }

    /// Records which redraw kinds a batch carried.
    fn note(&mut self, events: &[UiEvent]) {
        for event in events {
            match event {
                UiEvent::GridScroll { .. } => self.seen.scroll = true,
                UiEvent::GridClear { .. } => self.seen.clear = true,
                UiEvent::GridResize { .. } => self.seen.resize = true,
                UiEvent::GridLine { row, .. } => {
                    let cursor_row = self.model.engine.grid().cursor().0;
                    self.seen.line_on_predicted_row |= *row == u64::from(cursor_row);
                }
                UiEvent::WinViewport { win, topline, .. } => {
                    match self.seen.toplines.iter_mut().find(|(seen, _)| seen == win) {
                        Some((_, last)) => {
                            self.seen.viewport_moved |= *last != *topline;
                            *last = *topline;
                        }
                        None => self.seen.toplines.push((*win, *topline)),
                    }
                }
                _ => {}
            }
        }
    }

    /// Folds every key in `keys` into the prediction it produces, sending
    /// none of them.
    ///
    /// A burst typed faster than the engine answers reports the same cursor
    /// for every keystroke in it, which is the condition the whole feature
    /// exists for; holding the send until the fold is done is how a schedule
    /// reproduces that without needing a slow link to produce it.
    fn predict_keys(&mut self, keys: &[&str]) {
        for key in keys {
            self.predict_only(key);
        }
    }

    /// Folds one keystroke into whatever prediction it produces without
    /// sending it: the schedule for an answer that never comes.
    fn predict_only(&mut self, notation: &str) {
        let call = RpcCall::Input {
            notation: notation.to_string(),
        };
        let now = self.now();
        fold_engine_call(&mut self.model, &call, now);
    }

    /// Sends `keys` as one payload, folding nothing: how a case scripts its
    /// setup, and how it delivers a burst whose predictions are already
    /// folded.
    ///
    /// Exactly one of these may ride between two settle points. The payload
    /// carries the quiesce protocol's own marker ahead of the keys, and a
    /// second payload queued behind an unconsumed marker replaces the hook
    /// the first one is still waiting on -- which leaves that marker's echo
    /// to arrive with nothing expecting it, and a message on screen no
    /// script asked for.
    fn script(&mut self, keys: &str) -> Result<(), OracleError> {
        settle::arm_and_input(self, keys)
    }

    /// Waits for everything typed to be processed, applying every batch that
    /// arrives on the way.
    fn quiesce(&mut self) -> Result<bool, OracleError> {
        settle::settle(self, SILENCE, DEADLINE)
    }

    /// The current frame, through the same cached renderer the runtime
    /// paints from.
    fn surface(&mut self) -> Surface {
        self.cache.render(&self.model).clone()
    }

    /// `surface` as one string per canvas row, speculated glyphs painted
    /// exactly where the terminal painter puts them.
    ///
    /// Takes the surface rather than rendering one, for the reason
    /// [`masked_rows`] states in its own doc: one `render()` feeds both the
    /// mask and the row text, so the two can never be a frame apart.
    fn rows(&self, surface: &Surface) -> Vec<String> {
        raster::screen_rows(surface, self.model.engine.grid())
    }

    /// The rows the `Speculated` layer currently occupies, in surface
    /// coordinates.
    fn speculated_rows(surface: &Surface) -> Vec<u16> {
        surface
            .layers
            .iter()
            .filter(|layer| matches!(layer.kind, LayerKind::Speculated(_)))
            .flat_map(|layer| {
                let start = layer.rect.row;
                start..start.saturating_add(layer.rect.height)
            })
            .collect()
    }

    /// Whether nothing speculative is left in the frame.
    fn speculation_gone(&mut self) -> bool {
        let surface = self.surface();
        Self::speculated_rows(&surface).is_empty()
    }

    /// What this session is showing while its predictions are up.
    fn painted(&mut self) -> Painted {
        let pending = self.model.speculate.pending().len();
        let surface = self.surface();
        let rows = Self::speculated_rows(&surface);
        let mask = masked_rows(&surface);
        let masked = !rows.is_empty() && rows.iter().all(|row| mask.contains(row));
        let caret = (
            surface.cursor.map(|spec| spec.col),
            self.model.engine.grid().cursor().1,
        );
        let text = self
            .rows(&surface)
            .get(usize::from(rows.first().copied().unwrap_or(0)))
            .cloned()
            .unwrap_or_default();
        Painted {
            pending,
            rows,
            text,
            masked,
            caret,
        }
    }

    /// The surface row `surface` paints the engine grid's first row on, so a
    /// grid row read off nvim's own screen lines up with the row view
    /// painted it on.
    ///
    /// Read off the `EngineGrid` layer's own position rather than derived as
    /// "however many rows the canvas has that the grid does not": that
    /// subtraction is only the same number while every reserved row sits
    /// above the grid, and it silently misaligns every row by one the day a
    /// frame reserves a row below it.
    fn grid_origin(surface: &Surface) -> usize {
        surface
            .layers
            .iter()
            .find(|layer| matches!(layer.kind, LayerKind::EngineGrid))
            .map_or(0, |layer| usize::from(layer.rect.row))
    }

    /// nvim's own screen, row by row, with placeholders prepended so its
    /// indices line up against view's rows.
    fn nvim_rows(&mut self, surface: &Surface) -> Result<Vec<String>, OracleError> {
        let origin = Self::grid_origin(surface);
        let raw = self.engine.handle.eval_str(SCREEN_EXPR)?;
        let mut rows: Vec<String> = (0..origin).map(|_| String::new()).collect();
        rows.extend(raw.split(RECORD_SEP).map(str::to_string));
        Ok(rows)
    }

    /// nvim's own report of the caret column, zero-based to match the grid's.
    ///
    /// A reply that will not parse is an error rather than a default,
    /// because this is the authority a settled caret is judged against: a
    /// silent column 0 would let a case pass on an answer nobody read.
    fn nvim_caret_col(&mut self) -> Result<u16, OracleError> {
        let raw = self.engine.handle.eval_str(CARET_EXPR)?;
        let col: u16 = raw
            .trim()
            .parse()
            .map_err(|_| OracleError::Parse(format!("malformed screencol reply: {raw:?}")))?;
        Ok(col.saturating_sub(1))
    }

    /// What this session is showing after the settle point, and every
    /// disagreement with nvim's own screen over the rows the settled frame
    /// does not mask.
    fn settled(&mut self, predicted: &[u16]) -> Result<(Settled, Vec<Divergence>), OracleError> {
        let surface = self.surface();
        let layer = !Self::speculated_rows(&surface).is_empty();
        let mask = masked_rows(&surface);
        let masked = predicted.iter().any(|row| mask.contains(row));
        let caret_painted = surface.cursor.map(|spec| spec.col);
        let caret_engine = self.model.engine.grid().cursor().1;
        let caret_nvim = self.nvim_caret_col()?;
        let view_rows = self.rows(&surface);
        let nvim_rows = self.nvim_rows(&surface)?;
        let mut divergences = Vec::new();
        diff_rows(
            ViewRows(&view_rows),
            ReferenceRows(&nvim_rows),
            &mask,
            &mut divergences,
            |row, view, reference| Divergence::Grid {
                row,
                view: view.to_string(),
                reference: reference.to_string(),
            },
        );
        let text = predicted
            .first()
            .and_then(|row| view_rows.get(usize::from(*row)))
            .cloned()
            .unwrap_or_default();
        Ok((
            Settled {
                layer,
                masked,
                pending: self.model.speculate.pending().len(),
                text,
                caret: (caret_painted, caret_engine, caret_nvim),
            },
            divergences,
        ))
    }

    /// Drains and applies whatever has arrived without waiting for anything.
    fn drain(&mut self) {
        let events = self.pump.take_damage();
        if events.is_empty() {
            let now = self.now();
            fold_expiry(&mut self.model, now);
        } else {
            self.apply(events);
        }
    }
}

impl settle::Settling for SpecSession {
    fn handle(&self) -> &EngineHandle {
        &self.engine.handle
    }

    fn take_damage(&self) -> Vec<UiEvent> {
        self.pump.take_damage()
    }

    fn apply_batch(&mut self, events: Vec<UiEvent>) {
        self.apply(events);
    }

    fn markers(&mut self) -> &mut settle::QuiesceMarkers {
        &mut self.markers
    }
}

/// Runs one case against a real pinned engine and reports what it observed.
///
/// # Errors
///
/// Returns [`OracleError`] if the engine cannot be spawned or driven, or if
/// a probe this battery reads its authority from fails. A case that runs but
/// disagrees is a report with divergences, never an error: the difference
/// between "could not ask" and "asked and the answer was wrong" is the whole
/// contract of a differential runner.
pub fn run_case(case: SpecCase) -> Result<SpecReport, OracleError> {
    let start = Instant::now();
    let mut session = SpecSession::spawn()?;
    let quiesced_start = session.quiesce()?;
    // the cached renderer is warmed before any prediction exists, so every
    // case afterwards decides frame reuse against a frame that predates its
    // own speculation rather than capturing from a cold cache
    let _ = session.surface();

    let outcome = match case {
        SpecCase::BurstTail => burst_tail(&mut session)?,
        SpecCase::StraightLine => straight_line(&mut session)?,
        SpecCase::Mispredict => mispredict(&mut session)?,
        SpecCase::LateRedraw => late_redraw(&mut session)?,
        SpecCase::Scroll => scroll(&mut session)?,
        SpecCase::Clear => clear(&mut session)?,
        SpecCase::Resize => resize(&mut session)?,
        SpecCase::Paste => paste(&mut session)?,
        SpecCase::Mouse => mouse(&mut session)?,
        SpecCase::ViewportShift => viewport_shift(&mut session)?,
        SpecCase::AgeBound => age_bound(&mut session)?,
    };

    let quiesced_end = session.quiesce()?;
    let (settled, divergences) = session.settled(&outcome.painted.rows)?;
    Ok(SpecReport {
        case,
        painted: outcome.painted,
        settled,
        quiesced: quiesced_start && quiesced_end,
        fired: outcome.fired,
        divergences,
        retired_after: outcome.retired_after,
        elapsed: start.elapsed(),
    })
}

/// What one case's schedule observed, before the settle point the runner
/// takes over at.
struct Outcome {
    painted: Painted,
    /// Whether the case's own divergence class actually fired.
    fired: bool,
    /// How long an unanswered prediction took to retire, for the cases whose
    /// retirement is the clock rather than a redraw.
    retired_after: Option<Duration>,
}

impl Outcome {
    /// An outcome for a case a redraw is expected to answer.
    const fn answered(painted: Painted, fired: bool) -> Self {
        Self {
            painted,
            fired,
            retired_after: None,
        }
    }
}

/// Drives the session until nothing is pending or the age deadline passes,
/// reporting how long that took as measured from `predicted_at`.
///
/// The loop is the runtime's own per-pass shape: drain whatever arrived,
/// check the bound, and go round again. What it must never do is settle,
/// which would make the retirement a redraw's doing rather than the clock's.
fn wait_out_the_bound(session: &mut SpecSession, predicted_at: Instant) -> Option<Duration> {
    while predicted_at.elapsed() <= age_deadline() {
        session.drain();
        if session.model.speculate.pending().is_empty() {
            return Some(predicted_at.elapsed());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    None
}

/// Enters insert mode on an empty buffer and waits for the mode change to
/// arrive: nothing may be predicted before the engine has said what mode it
/// is in, since the mode is read off the model rather than assumed.
fn enter_insert(session: &mut SpecSession) -> Result<(), OracleError> {
    session.script("i")?;
    let _ = session.quiesce()?;
    Ok(())
}

/// The burst the round trip is actually made of, answered by one full-line
/// redraw: every prediction in the run retires together, tail included.
fn burst_tail(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    enter_insert(session)?;
    session.predict_keys(&["a", "b", "c", "d", "e", "f"]);
    let painted = session.painted();
    session.script("abcdef")?;
    let _ = session.quiesce()?;
    // the tail retires because the run covering the head reaches it, so the
    // line the batch carried is what the case has to have seen
    Ok(Outcome::answered(
        painted,
        session.seen.line_on_predicted_row,
    ))
}

/// One character, predicted and then answered on its own.
fn straight_line(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    enter_insert(session)?;
    session.predict_keys(&["z"]);
    let painted = session.painted();
    let converged = painted.text.starts_with('z');
    session.script("z")?;
    Ok(Outcome::answered(painted, converged))
}

/// A burst nvim contradicts: `teh ` is typed and shown, and an insert-mode
/// abbreviation makes the buffer hold `the ` instead. The glyphs at the two
/// transposed columns are predictions the engine never confirms.
fn mispredict(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    session.script("<Cmd>iabbrev teh the<CR>")?;
    let _ = session.quiesce()?;
    enter_insert(session)?;
    session.predict_keys(&["t", "e", "h", "."]);
    let painted = session.painted();
    let shown = painted.text.clone();
    session.script("teh.")?;
    let _ = session.quiesce()?;
    let row = usize::from(painted.rows.first().copied().unwrap_or(0));
    let surface = session.surface();
    let settled = session.rows(&surface).get(row).cloned().unwrap_or_default();
    // the case is only evidence if the guess really was wrong: a shown row
    // that already matched the settled one would prove nothing about a
    // mispredicted glyph surviving
    let mispredicted = shown.starts_with("teh.") && settled.starts_with("the.");
    Ok(Outcome::answered(painted, mispredicted))
}

/// A burst invalidated before its own answer arrives: the redraw it would
/// have been confirmed by lands after the predictions are already gone.
fn late_redraw(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    enter_insert(session)?;
    session.predict_keys(&["a", "b", "c"]);
    let painted = session.painted();
    let epoch = session.model.speculate.epoch();
    // nothing has been sent yet, so the invalidation lands ahead of the
    // redraw that would have confirmed the burst rather than after it
    session.predict_only("<Esc>");
    let discarded = session.model.speculate.pending().is_empty()
        && session.model.speculate.epoch() > epoch
        && session.speculation_gone();
    session.script("abc<Esc>")?;
    let _ = session.quiesce()?;
    Ok(Outcome::answered(painted, discarded))
}

/// A burst answered by a scroll: content moves out from under the
/// predictions without any batch resending their cells.
fn scroll(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    fill_and_park(session)?;
    session.predict_keys(&["q", "r"]);
    let painted = session.painted();
    // the scroll rides behind the burst in one payload, so the batch that
    // answers the predictions is the one that moved the content. A
    // single-line scroll rather than a viewport jump: nvim only reaches for
    // its scroll optimization when the region really did shift by a line it
    // can move, and repaints line by line otherwise
    session.script("qr<Cmd>execute \"normal! \\<lt>C-e>\"<CR>")?;
    let _ = session.quiesce()?;
    Ok(Outcome::answered(painted, session.seen.scroll))
}

/// Fills the buffer with more lines than the window shows, parks the cursor
/// in the middle of it and enters insert mode: the starting position for
/// every case whose schedule moves the window under a typing burst.
fn fill_and_park(session: &mut SpecSession) -> Result<(), OracleError> {
    session.script(&format!(
        "<Cmd>call setline(1, map(range(1, {SCROLL_BUFFER_LINES}), {{_, v -> 'line ' . v}}))<CR>"
    ))?;
    let _ = session.quiesce()?;
    session.script("<Cmd>normal! 100G<CR>")?;
    let _ = session.quiesce()?;
    enter_insert(session)
}

/// A burst answered by a full-screen clear.
fn clear(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    enter_insert(session)?;
    session.predict_keys(&["q", "r"]);
    let painted = session.painted();
    session.script("qr<Cmd>mode<CR>")?;
    let _ = session.quiesce()?;
    Ok(Outcome::answered(painted, session.seen.clear))
}

/// A burst answered by a resize, which is also where a prediction can be
/// left naming a cell the grid no longer has.
fn resize(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    enter_insert(session)?;
    session.predict_keys(&["q", "r"]);
    let painted = session.painted();
    session.script("qr")?;
    // out of band rather than in the payload: a resize is not something a
    // user types, and it is the one invalidation that arrives at the engine
    // by a path no keystroke shares
    session.engine.handle.try_resize(COLS - 6, ROWS - 2)?;
    let _ = session.quiesce()?;
    Ok(Outcome::answered(painted, session.seen.resize))
}

/// A burst invalidated by a paste: text reaches the buffer without any
/// character reaching the predictor.
fn paste(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    enter_insert(session)?;
    session.predict_keys(&["q", "r"]);
    let painted = session.painted();
    let call = RpcCall::Paste {
        text: "pasted".to_string(),
    };
    let now = session.now();
    fold_engine_call(&mut session.model, &call, now);
    let discarded = session.model.speculate.pending().is_empty() && session.speculation_gone();
    session.script("qr")?;
    session.engine.handle.paste("pasted")?;
    let _ = session.quiesce()?;
    Ok(Outcome::answered(painted, discarded))
}

/// A burst invalidated by a mouse click: the cursor moves without any
/// character reaching the predictor.
fn mouse(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    session.script("<Cmd>set mouse=a | call setline(1, ['alpha', 'bravo', 'charlie'])<CR>")?;
    let _ = session.quiesce()?;
    enter_insert(session)?;
    session.predict_keys(&["q", "r"]);
    let painted = session.painted();
    let call = RpcCall::InputMouse {
        button: "left".to_string(),
        action: "press".to_string(),
        modifier: String::new(),
        row: 2,
        col: 3,
    };
    let now = session.now();
    fold_engine_call(&mut session.model, &call, now);
    let discarded = session.model.speculate.pending().is_empty() && session.speculation_gone();
    session.script("qr")?;
    session
        .engine
        .handle
        .input_mouse("left", "press", "", 2, 3)?;
    let _ = session.quiesce()?;
    Ok(Outcome::answered(painted, discarded))
}

/// A burst answered by a window that jumps: nvim repaints the moved
/// viewport line by line rather than scrolling, and those lines need not
/// reach the predicted columns, so no cell in the batch answers them.
///
/// The retirement here is the viewport report itself, which is the only
/// thing in the batch that says the predicted coordinates are now over
/// different buffer lines. That makes this an ordinary event-driven
/// retirement like the scroll and clear cases -- it must land at the settle
/// point, not a second later on the age bound.
fn viewport_shift(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    fill_and_park(session)?;
    session.predict_keys(&["q", "r"]);
    let painted = session.painted();
    session.script("qr<Cmd>call winrestview({'topline': winsaveview().topline + 3})<CR>")?;
    let _ = session.quiesce()?;
    // the schedule proves nothing unless the window really did move: a
    // winrestview that landed on the same topline would leave an ordinary
    // per-cell retirement wearing this case's name
    Ok(Outcome::answered(painted, session.seen.viewport_moved))
}

/// A prediction the engine never answers, left to the age bound.
///
/// The keystroke is folded but never sent, which is a schedule no real
/// session produces and exactly the condition the bound exists for: a redraw
/// that never comes cannot be what retires a glyph.
fn age_bound(session: &mut SpecSession) -> Result<Outcome, OracleError> {
    enter_insert(session)?;
    let predicted_at = Instant::now();
    session.predict_only("k");
    let painted = session.painted();
    let retired_after = wait_out_the_bound(session, predicted_at);
    Ok(Outcome {
        painted,
        fired: retired_after.is_some(),
        retired_after,
    })
}
