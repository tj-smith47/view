//! [`ReferenceSession`]: a real embedded engine (same transport, decode, and
//! `UI_EXT_OPTIONS` as [`crate::EngineSession`]) applying the decoded events
//! with [`RefGrid`], a deliberately naive grid applier independent of
//! `view_core::grid::Grid`. The pairing is the differential oracle: feed the
//! same input to both, and any disagreement in the resulting screen text
//! points at a bug in one applier or the other -- most usefully, in view's
//! own `update()`/`Model`/damage/render pipeline, the layer this crate's
//! sibling drivers cannot independently corroborate on their own.
//!
//! # DO-NOT-CONSOLIDATE
//!
//! RefGrid intentionally re-implements grid semantics; folding it into
//! view-core's Grid would blind the oracle to grid-apply bugs.

use std::time::Duration;

use std::collections::HashMap;

use view_core::events::{clamp_dim, saturate_u16, GridCell, UiEvent};
use view_engine::handle::EngineHandle;
use view_engine::{DamagePump, Engine, EngineConfig};

use crate::attr::{row_fingerprint, ResolvedAttr};
use crate::settle::{self, QuiesceMarkers, Settling};
use crate::OracleError;

/// One [`RefGrid`] cell: display text and the highlight group id it was
/// painted with. Distinct from `view_core::grid::Cell` by construction (see
/// this module's DO-NOT-CONSOLIDATE note): a coincidentally identical shape
/// today is not a reason to share the type, since the two are free to
/// diverge the moment either grid's own bugs need a fixture that only one
/// of them reproduces.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cell {
    text: String,
    hl_id: u64,
}

impl Cell {
    fn blank() -> Self {
        Self {
            text: " ".to_string(),
            hl_id: 0,
        }
    }
}

/// A deliberately naive `ext_linegrid` applier: row-of-cells storage
/// (`Vec<Vec<Cell>>`, not view-core's flat indexed buffer), full-row writes,
/// scroll implemented as a whole-region snapshot-then-write-back (never an
/// in-place shifted copy), no damage tracking, no compaction. Slower than
/// `view_core::grid::Grid` and not meant to be fast: the oracle only ever
/// applies one script's worth of events per test, and the entire point of
/// this type is to be obviously, inspectably correct rather than clever.
struct RefGrid {
    width: u16,
    height: u16,
    rows: Vec<Vec<Cell>>,
    cursor_row: u16,
    cursor_col: u16,
    /// Highlight definitions this side has received, keyed by the
    /// per-session `hl_id` its own `grid_line` cells reference. Independent
    /// of `view_core::hl::HlTable` for the same reason the rest of this
    /// grid is independent of `view_core::grid::Grid` (see the module's
    /// DO-NOT-CONSOLIDATE note): the oracle compares the resolved
    /// [`ResolvedAttr`] content, never the raw id, so the two sides' id
    /// spaces never have to agree. See [`crate::attr`]'s docs.
    attrs: HashMap<u64, ResolvedAttr>,
}

impl RefGrid {
    fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            rows: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            attrs: HashMap::new(),
        }
    }

    /// Records one `hl_attr_define`'s resolved attributes under its
    /// `hl_id`, the reference-side counterpart of `view_core`'s
    /// `HlTable::attrs` insert. Grid content already stores each cell's raw
    /// `hl_id` (see [`put_line`](Self::put_line)); this table is what
    /// [`attr_row`](Self::attr_row) later resolves those ids through.
    fn define_attr(&mut self, id: u64, attr: ResolvedAttr) {
        self.attrs.insert(id, attr);
    }

    /// Rebuilds `rows` at the new size, copying the overlapping region from
    /// the old one: nvim's own `ext_linegrid` contract assumes a UI retains
    /// unpainted content across a resize (only changed lines get a fresh
    /// `GridLine`), so a naive "just wipe it" resize would diverge from a
    /// correct UI the first time a script resizes mid-run.
    fn resize(&mut self, width: u16, height: u16) {
        let mut new_rows = vec![vec![Cell::blank(); usize::from(width)]; usize::from(height)];
        for r in 0..self.height.min(height) {
            // checked access, matching view-core's Grid house style: bounds
            // here are safe by construction (r/c are already clamped to the
            // smaller of old/new dims), but a future loosened clamp should
            // degrade instead of panic
            let (Some(old_row), Some(new_row)) = (
                self.rows.get(usize::from(r)),
                new_rows.get_mut(usize::from(r)),
            ) else {
                continue;
            };
            for c in 0..self.width.min(width) {
                if let (Some(old_cell), Some(new_cell)) =
                    (old_row.get(usize::from(c)), new_row.get_mut(usize::from(c)))
                {
                    *new_cell = old_cell.clone();
                }
            }
        }
        self.width = width;
        self.height = height;
        self.rows = new_rows;
        self.cursor_row = self.cursor_row.min(height.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(width.saturating_sub(1));
    }

    fn clear(&mut self) {
        for row in &mut self.rows {
            for cell in row {
                *cell = Cell::blank();
            }
        }
    }

    fn cursor_goto(&mut self, row: u16, col: u16) {
        self.cursor_row = row.min(self.height.saturating_sub(1));
        self.cursor_col = col.min(self.width.saturating_sub(1));
    }

    /// Writes `cells` starting at `(row, col_start)`, expanding each
    /// `repeat` in place. Bounded by `self.width` regardless of how large a
    /// malformed `repeat` claims to be: the loop returns the moment `col`
    /// reaches the row's length, the same total-by-construction guard
    /// `view_core::grid::Grid::put_line` uses, arrived at independently.
    fn put_line(&mut self, row: u16, col_start: u16, cells: &[GridCell]) {
        let Some(row_cells) = self.rows.get_mut(usize::from(row)) else {
            return;
        };
        let mut col = col_start;
        for cell in cells {
            for _ in 0..cell.repeat {
                let Some(slot) = row_cells.get_mut(usize::from(col)) else {
                    return;
                };
                *slot = Cell {
                    text: cell.text.clone(),
                    hl_id: cell.hl_id,
                };
                col = col.saturating_add(1);
            }
        }
    }

    /// Scrolls `top..bot`, `left..right` by `rows` (positive moves content
    /// up toward row 0) via a whole-region snapshot: copy every cell in the
    /// region out, blank the region, then write the snapshot back at its
    /// shifted position. Never an in-place, direction-dependent shifted
    /// copy -- the naive strategy this module's docs describe -- so this
    /// code cannot reproduce an off-by-one that only an in-place shift's
    /// iteration order could produce.
    fn scroll(&mut self, top: u16, bot: u16, left: u16, right: u16, rows: i64) {
        let top = top.min(self.height);
        let bot = bot.min(self.height);
        let left = left.min(self.width);
        let right = right.min(self.width);
        if top >= bot || left >= right || rows == 0 {
            return;
        }

        // checked access throughout, matching view-core's Grid house style:
        // top/bot/left/right are already clamped above, but a future
        // loosened clamp should degrade instead of panic
        let mut snapshot = Vec::with_capacity(usize::from(bot - top));
        for r in top..bot {
            let mut row = Vec::with_capacity(usize::from(right - left));
            if let Some(src_row) = self.rows.get(usize::from(r)) {
                for c in left..right {
                    if let Some(cell) = src_row.get(usize::from(c)) {
                        row.push(cell.clone());
                    }
                }
            }
            snapshot.push(row);
        }

        for r in top..bot {
            if let Some(row_cells) = self.rows.get_mut(usize::from(r)) {
                for c in left..right {
                    if let Some(slot) = row_cells.get_mut(usize::from(c)) {
                        *slot = Cell::blank();
                    }
                }
            }
        }

        for (i, row) in snapshot.into_iter().enumerate() {
            let src_row = i64::from(top) + i as i64;
            let dst_row = src_row - rows;
            if dst_row < i64::from(top) || dst_row >= i64::from(bot) {
                // scrolled past the region edge: the whole-region blank
                // above already accounts for this row's content leaving
                continue;
            }
            let dst_row = saturate_u16(dst_row as u64);
            let Some(dst_row_cells) = self.rows.get_mut(usize::from(dst_row)) else {
                continue;
            };
            for (j, cell) in row.into_iter().enumerate() {
                if let Some(slot) = dst_row_cells.get_mut(usize::from(left) + j) {
                    *slot = cell;
                }
            }
        }
    }

    fn row_text(&self, row: u16) -> String {
        self.rows
            .get(usize::from(row))
            .map(|cells| cells.iter().map(|c| c.text.as_str()).collect())
            .unwrap_or_default()
    }

    /// Renders `row`'s per-cell highlight identity, each cell's stored
    /// `hl_id` resolved through [`attrs`](Self::attrs) into a
    /// [`ResolvedAttr`] (defaulting for `hl_id` 0 and any undefined id, the
    /// same fallback the view side's `resolve_attr` applies). The attr-parity
    /// counterpart of [`row_text`](Self::row_text).
    fn attr_row(&self, row: u16) -> String {
        self.rows
            .get(usize::from(row))
            .map(|cells| {
                row_fingerprint(cells.iter().map(|c| {
                    self.attrs
                        .get(&c.hl_id)
                        .copied()
                        .unwrap_or(ResolvedAttr::DEFAULT)
                }))
            })
            .unwrap_or_default()
    }
}

/// `UiEvent::Unknown` names a real `--clean` nvim session emits on every
/// healthy run, pinned from an empirical capture of the pinned nvim build
/// attached with the full `ext_*` set (`--clean` startup through a plain
/// insert and back to idle). [`ReferenceSession::unknown_events`] filters
/// these out so the accessor stays empty on a healthy run instead of
/// permanently drowning a genuinely new event name in the same noise; an
/// nvim upgrade that starts emitting a name outside this list fails
/// `known_unmodeled_events_match_a_live_session` loudly rather than
/// silently widening what `unknown_events` reports as novel.
const KNOWN_UNMODELED_EVENTS: &[&str] = &[
    "chdir",
    "option_set",
    "set_icon",
    "set_title",
    "update_menu",
];

/// Engine-attached headless driver applying the decoded redraw stream with
/// [`RefGrid`] instead of view's own `Model`/`Grid`: the independent second
/// opinion that parity harnesses diff against `EngineSession`'s decoded
/// screen.
pub struct ReferenceSession {
    engine: Engine,
    pump: DamagePump,
    grid: RefGrid,
    mode: String,
    /// Names of `UiEvent::Unknown` events observed, in arrival order,
    /// unfiltered: an unrecognized redraw event class is a potential
    /// divergence source the operator must see, never silently dropped
    /// (part of the quiesce protocol's contract, not just an apply-time
    /// detail). See [`unknown_events`](Self::unknown_events) for the
    /// filtered view novelty-checking code should read instead.
    unknown_events_raw: Vec<String>,
    /// This session's half of the shared quiesce protocol's state (see
    /// [`crate::settle`]).
    markers: QuiesceMarkers,
    /// The terminal size this session attached at, held separately from
    /// `grid`'s own (possibly chrome-reduced) dimensions: [`chrome_rows`](Self::chrome_rows)
    /// needs the constant full-terminal size to recompute a resize target
    /// from, the same way `view_core::model::Model::grid_target` reads
    /// `term_width`/`term_height` rather than the engine grid's current
    /// size.
    term_width: u16,
    term_height: u16,
    /// The tab count last reported by a `TablineUpdate`; `0` before the
    /// first one arrives, matching `view_core::model::EngineModel::tabline`
    /// starting `None` (both mean "no tabline reservation yet").
    tab_count: usize,
}

impl ReferenceSession {
    /// Spawns a real `nvim --embed`, attaches at `cols`x`rows` with the full
    /// `ext_*` set (via [`view_engine::EngineHandle::ui_attach`], the same
    /// call `EngineSession` and the production paint loop both use), and
    /// installs the quiesce protocol's hermetic timer pins and SafeState
    /// hook before returning. Always spawns with `--clean`, matching
    /// `EngineSession::spawn`'s own rationale: a differential oracle must be
    /// deterministic across hosts and CI, which a developer's `init.lua`
    /// cannot guarantee. Also always spawns with `-n` (no swap file): a
    /// differential run's whole point is running a second nvim instance
    /// alongside another one (`EngineSession`, or a real editor session) in
    /// the same working directory, and two unnamed-buffer swap files
    /// colliding there produces a live `E303` recovery error on whichever
    /// side loses the race, not a hang or a decode error. A short-lived
    /// oracle double has no crash to recover from, so there is nothing this
    /// trades away.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the process fails to spawn, the
    /// `ui_attach` handshake fails or times out, or the quiesce-protocol
    /// setup commands cannot be written to the connection.
    pub fn spawn(cols: u16, rows: u16) -> Result<Self, OracleError> {
        Self::spawn_configured(EngineConfig::isolated(), cols, rows)
    }

    /// Same as [`spawn`](Self::spawn), but with a caller-supplied
    /// [`EngineConfig`] (a non-default `nvim_bin`, timeout, or extra
    /// arguments) instead of [`spawn`](Self::spawn)'s own
    /// [`EngineConfig::isolated`] default.
    ///
    /// # Errors
    ///
    /// Same as [`spawn`](Self::spawn).
    pub fn spawn_configured(cfg: EngineConfig, cols: u16, rows: u16) -> Result<Self, OracleError> {
        let mut engine = Engine::spawn(cfg)?;
        engine.handle.ui_attach(cols, rows)?;
        let (sink, _unused_rx) = std::sync::mpsc::sync_channel(64);
        let (pump, _cutover) = engine.start_pump(sink);
        let session = Self {
            engine,
            pump,
            grid: RefGrid::new(),
            mode: String::new(),
            unknown_events_raw: Vec::new(),
            markers: QuiesceMarkers::default(),
            term_width: cols,
            term_height: rows,
            tab_count: 0,
        };
        settle::install_hooks(&session.engine.handle)?;
        Ok(session)
    }

    /// Forwards one encoded key `notation` via `nvim_input`, identical to
    /// [`EngineSession::input`](crate::EngineSession::input). Arms no
    /// quiesce marker: this is the poke a caller uses on a session whose
    /// state it is not trying to settle afterwards (the `<Esc>` dismissal
    /// [`crate::snapshot`] types into a blocked key-wait). Drive a script
    /// through [`arm_and_input`](Self::arm_and_input) instead.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the connection's writer thread has
    /// already exited.
    pub fn input(&mut self, notation: &str) -> Result<(), OracleError> {
        self.engine.handle.input(notation).map_err(Into::into)
    }

    /// Queues the next quiesce marker's arm command and `notation` into
    /// nvim's typeahead as one `feedkeys()` payload, in that order, and
    /// records the marker for the next [`quiesce`](Self::quiesce) call to
    /// wait on. The way a script under test must be driven; see
    /// [`crate::settle::arm_and_input`] for why the fusion into a single
    /// payload is the whole settle argument, and for the already-settled
    /// contract a caller owes it.
    ///
    /// Blocking, unlike [`input`](Self::input): the payload rides a request
    /// whose reply says nvim accepted it (the keys themselves are left for
    /// the main loop to consume).
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the connection has closed, nvim
    /// rejects the payload, or no reply arrives within the engine's eval
    /// timeout.
    pub fn arm_and_input(&mut self, notation: &str) -> Result<(), OracleError> {
        settle::arm_and_input(self, notation)
    }

    /// Waits until everything typed into this session has been fully
    /// processed, per the shared marker protocol [`crate::settle::settle`]
    /// documents in full. Returns whether the session settled before
    /// `deadline`.
    ///
    /// # Errors
    ///
    /// - [`OracleError::Engine`] if the fast state probe, the parked-state
    ///   probe, or the marker's arm call fails at the RPC layer.
    /// - [`OracleError::QuiescePerturbed`] if the marker round-trip failed
    ///   the protocol's integrity check.
    pub fn quiesce(&mut self, silence: Duration, deadline: Duration) -> Result<bool, OracleError> {
        settle::settle(self, silence, deadline)
    }

    /// Renders `RefGrid`'s current content as a plain-text screen dump: one
    /// newline-joined row per grid line, in the same shape
    /// `EngineSession::screen_text` produces for a scenario with no
    /// cmdline/message/tabline/popupmenu overlay active, so a paired test
    /// can compare the two directly.
    #[must_use]
    pub fn screen_text(&self) -> String {
        self.screen_rows().join("\n")
    }

    /// Renders `RefGrid`'s current content as one `String` per row, in row
    /// order: the row-indexed form [`crate::compare`] needs, matching
    /// [`EngineSession::screen_rows`](crate::EngineSession::screen_rows)'s
    /// shape on the view side so a [`crate::masked_rows`] row index lines up
    /// against both sides' row vectors identically.
    ///
    /// Prepends [`chrome_rows`](Self::chrome_rows) empty placeholder rows
    /// ahead of `grid`'s own content: `view_surface::render` offsets its
    /// `EngineGrid` layer down by the same count to make room for the
    /// tabline (see that function's doc comment), so without a matching
    /// placeholder here every row from the tabline down would be off by
    /// one between the two sides. The placeholder's own text is never
    /// compared -- row 0 is exactly the row [`crate::masked_rows`] excludes
    /// whenever the tabline is showing -- only its presence as an
    /// index-shifting slot matters.
    #[must_use]
    pub fn screen_rows(&self) -> Vec<String> {
        let chrome = self.chrome_rows();
        let mut rows: Vec<String> = (0..chrome).map(|_| String::new()).collect();
        rows.extend((0..self.grid.height).map(|r| self.grid.row_text(r)));
        rows
    }

    /// Renders `RefGrid`'s per-cell highlight identity as one string per row,
    /// row-indexed identically to [`screen_rows`](Self::screen_rows) (same
    /// chrome-placeholder offset): the attr-parity counterpart of the text
    /// dump that [`crate::compare`] diffs against the view side's
    /// `attr_rows`. Each cell's `hl_id` is resolved through this session's
    /// own highlight table, so the two sides' per-session id assignments
    /// never enter the comparison (see [`crate::attr`]'s docs).
    #[must_use]
    pub fn attr_rows(&self) -> Vec<String> {
        let chrome = self.chrome_rows();
        let mut rows: Vec<String> = (0..chrome).map(|_| String::new()).collect();
        rows.extend((0..self.grid.height).map(|r| self.grid.attr_row(r)));
        rows
    }

    /// Captures this side's [`crate::Screen`] -- glyph rows plus per-cell
    /// highlight rows -- for [`crate::compare`], the reference-side
    /// counterpart of [`EngineSession::screen`](crate::EngineSession::screen).
    #[must_use]
    pub fn screen(&self) -> crate::Screen {
        crate::Screen {
            rows: self.screen_rows(),
            attr_rows: self.attr_rows(),
        }
    }

    /// Evaluates `expr` against the real engine, identical to
    /// [`EngineSession::eval_str`](crate::EngineSession::eval_str).
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the request fails, nvim rejects
    /// the expression, or the reply times out.
    pub fn eval_str(&mut self, expr: &str) -> Result<String, OracleError> {
        self.engine.handle.eval_str(expr).map_err(Into::into)
    }

    /// Reads nvim's current mode name and blocked flag via the fast
    /// `nvim_get_mode` probe, identical in contract to
    /// `EngineHandle::get_mode`: answered even in the blocked key-wait
    /// states where [`eval_str`](Self::eval_str) would be deferred, and
    /// the probe [`quiesce`](Self::quiesce)'s settle-path choice reads.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the request fails, the reply
    /// times out, or the reply shape is malformed.
    pub fn get_mode(&mut self) -> Result<(String, bool), OracleError> {
        self.engine.handle.get_mode().map_err(Into::into)
    }

    /// The current cursor `(row, col)`, as last set by `GridCursorGoto`.
    #[must_use]
    pub fn cursor(&self) -> (u16, u16) {
        (self.grid.cursor_row, self.grid.cursor_col)
    }

    /// The current mode name, as last set by `ModeChange`.
    #[must_use]
    pub fn mode(&self) -> &str {
        &self.mode
    }

    /// Names of every `UiEvent::Unknown` event observed so far that are
    /// *not* in [`KNOWN_UNMODELED_EVENTS`], in arrival order. A real
    /// `--clean` nvim session emits several event names view-engine's
    /// decoder does not model as structured `UiEvent` variants on every
    /// healthy run (`option_set`, `chdir`, `set_title`, and the rest
    /// of `KNOWN_UNMODELED_EVENTS`); returning those unfiltered here would
    /// make this accessor permanently non-empty and so operationally inert
    /// as a novelty signal. Filtering them out means a genuinely new event
    /// name -- one an nvim upgrade started emitting that this list has not
    /// been updated to expect -- is the only thing that ever shows up here,
    /// and an empty result is what a healthy run looks like. See
    /// [`raw_unknown_events`](Self::raw_unknown_events) for the unfiltered
    /// log.
    #[must_use]
    pub fn unknown_events(&self) -> Vec<&str> {
        self.unknown_events_raw
            .iter()
            .map(String::as_str)
            .filter(|name| !KNOWN_UNMODELED_EVENTS.contains(name))
            .collect()
    }

    /// The unfiltered arrival-order log [`unknown_events`](Self::unknown_events)
    /// filters against [`KNOWN_UNMODELED_EVENTS`]. Kept as a separate
    /// accessor for tests and diagnostics that want to see every
    /// `UiEvent::Unknown` name a run actually produced, known-unmodeled or
    /// not (e.g. pinning that `KNOWN_UNMODELED_EVENTS` still matches a live
    /// session).
    #[must_use]
    pub fn raw_unknown_events(&self) -> &[String] {
        &self.unknown_events_raw
    }

    /// Applies one decoded event to `RefGrid` (or to this session's
    /// cursor/mode/unknown-event bookkeeping). Exhaustive over every
    /// `UiEvent` variant with no wildcard arm: `UiEvent` is not
    /// `#[non_exhaustive]` specifically so a new variant fails this match
    /// at compile time instead of silently doing nothing.
    ///
    /// Per the ext-event policy: `Cmdline*`/`Msg*`/`Tabline*`/`Popupmenu*`
    /// content is consumed and discarded for grid purposes. With the full
    /// `ext_*` set attached (matching `EngineSession`'s own attach), nvim
    /// never paints that content into the grid at all, so both sides'
    /// grids are already structurally equalized on it; ext-layer
    /// correctness is covered by state probes and view's own unit suites,
    /// not by this oracle's grid diff.
    ///
    /// `clamp_dim`/`saturate_u16` (from `view_core::events`, the same
    /// helpers `EngineSession`'s own decode path normalizes through) sit
    /// upstream of both `RefGrid` and `view_core::grid::Grid`: a wrong
    /// clamp there would be common-mode between the two appliers and so
    /// invisible to this module's differential, which only ever proves the
    /// two sides agree, not that either normalized correctly in the first
    /// place.
    fn apply(&mut self, ev: UiEvent) {
        match ev {
            UiEvent::GridResize { width, height, .. } => {
                self.grid.resize(clamp_dim(width), clamp_dim(height));
            }
            UiEvent::GridLine {
                row,
                col_start,
                cells,
                ..
            } => {
                self.grid
                    .put_line(saturate_u16(row), saturate_u16(col_start), &cells);
            }
            UiEvent::GridCursorGoto { row, col, .. } => {
                self.grid.cursor_goto(saturate_u16(row), saturate_u16(col));
            }
            UiEvent::GridScroll {
                top,
                bot,
                left,
                right,
                rows,
                ..
            } => {
                self.grid.scroll(
                    saturate_u16(top),
                    saturate_u16(bot),
                    saturate_u16(left),
                    saturate_u16(right),
                    rows,
                );
            }
            UiEvent::GridClear { .. } => {
                self.grid.clear();
            }
            UiEvent::ModeChange { mode, .. } => {
                self.mode = mode;
            }
            UiEvent::Unknown { name } => {
                self.unknown_events_raw.push(name);
            }
            // tab count is tracked (not discarded, unlike the rest of this
            // arm's chrome events) purely to re-derive the same
            // one-row-for-the-tabline reservation decision
            // `view_core::model::Model::chrome_rows`/`grid_target` makes in
            // production: a correctly-behaved UI resizes its grid down by
            // one row the moment a second tab opens, so this session's own
            // nvim process must receive that same `nvim_ui_try_resize` or
            // its grid stays a row taller than `EngineSession`'s -- not a
            // grid-apply bug this differential exists to catch, just the
            // UI-attach-policy plumbing every `ext_tabline`-attached UI
            // (including this one) must carry out for the two sides to stay
            // comparable at all. See this fn's doc comment's ext-event
            // policy paragraph for why the tabline's *content* is still
            // never painted into `grid`.
            UiEvent::TablineUpdate { tabs, .. } => {
                let before = self.chrome_rows();
                self.tab_count = tabs.len();
                let after = self.chrome_rows();
                if before != after {
                    let target_height = self.term_height.saturating_sub(after);
                    let _ = self
                        .engine
                        .handle
                        .try_resize(self.term_width, target_height);
                }
            }
            // recorded (not discarded like the rest of this arm's ext
            // content) because a cell's own `hl_id` -- already stored by
            // put_line -- is meaningless without the table that resolves it
            // to rendered attributes, and attr parity compares that
            // resolved content cell-for-cell (see the attr module's docs)
            UiEvent::HlAttrDefine {
                id,
                fg,
                bg,
                bold,
                italic,
                underline,
                reverse,
            } => {
                self.grid.define_attr(
                    id,
                    ResolvedAttr {
                        fg,
                        bg,
                        bold,
                        italic,
                        underline,
                        reverse,
                    },
                );
            }
            // discarded for grid purposes: no cell content of theirs ever
            // reaches the grid while ext_hlstate/ext_cmdline/ext_messages/
            // ext_tabline/ext_popupmenu are attached (see this fn's doc
            // comment's ext-event policy paragraph)
            UiEvent::DefaultColorsSet { .. }
            | UiEvent::WinViewport { .. }
            | UiEvent::HlGroupSet { .. }
            | UiEvent::Flush
            | UiEvent::ModeInfoSet { .. }
            | UiEvent::CmdlineShow { .. }
            | UiEvent::CmdlinePos { .. }
            | UiEvent::CmdlineHide
            | UiEvent::MsgShow { .. }
            | UiEvent::MsgClear
            | UiEvent::MsgShowmode { .. }
            | UiEvent::MsgShowcmd { .. }
            | UiEvent::MsgRuler { .. }
            | UiEvent::PopupmenuShow { .. }
            | UiEvent::PopupmenuSelect { .. }
            | UiEvent::PopupmenuHide
            | UiEvent::MouseOn
            | UiEvent::MouseOff
            // no cell content either: bytes bound for a terminal this
            // session does not have (it attaches without `stdout_tty`, so
            // nvim never sends one here in the first place)
            | UiEvent::UiSend { .. } => {}
        }
    }

    /// Terminal rows reserved for the tabline: `1` once more than one tab
    /// is open, `0` otherwise. Mirrors
    /// `view_core::model::Model::chrome_rows`'s threshold exactly (bare
    /// nvim's own default `showtabline` rule), re-derived here rather than
    /// imported so this session's UI-attach-policy decision comes from its
    /// own bookkeeping (`tab_count`), the same independence
    /// [`RefGrid`]'s grid-apply logic keeps from `view_core::grid::Grid`.
    fn chrome_rows(&self) -> u16 {
        if self.tab_count > 1 {
            1
        } else {
            0
        }
    }
}

impl Settling for ReferenceSession {
    fn handle(&self) -> &EngineHandle {
        &self.engine.handle
    }

    fn take_damage(&self) -> Vec<UiEvent> {
        self.pump.take_damage()
    }

    fn apply_batch(&mut self, events: Vec<UiEvent>) -> Result<(), OracleError> {
        for ev in events {
            self.apply(ev);
        }
        Ok(())
    }

    fn markers(&mut self) -> &mut QuiesceMarkers {
        &mut self.markers
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::time::Instant;

    use super::*;
    use crate::testenv;
    use crate::EngineSession;

    const QUIESCE_SILENCE: Duration = Duration::from_millis(200);
    const QUIESCE_DEADLINE: Duration = Duration::from_secs(5);

    /// The falsifiable check this whole module exists for: the same script
    /// driven into a real engine-attached session and a `RefGrid`-applied
    /// reference session must agree on the resulting screen text. Trims
    /// trailing blank lines and per-line trailing whitespace; not the full
    /// comparison mask a richer region-diff would use.
    fn buffer_region(text: &str) -> Vec<&str> {
        let trimmed: Vec<&str> = text.lines().map(|l| l.trim_end()).collect();
        let last_nonblank = trimmed.iter().rposition(|l| !l.is_empty());
        match last_nonblank {
            Some(i) => trimmed[..=i].to_vec(),
            None => Vec::new(),
        }
    }

    #[test]
    fn reference_and_engine_sessions_agree_on_a_plain_insert() {
        let mut engine_side = testenv::spawning(|| EngineSession::spawn(60, 12))
            .expect("EngineSession::spawn against real nvim");
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(60, 12))
            .expect("ReferenceSession::spawn against real nvim");

        assert!(engine_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce EngineSession"));
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        engine_side
            .arm_and_input("ihello world<Esc>")
            .expect("input against EngineSession");
        reference_side
            .arm_and_input("ihello world<Esc>")
            .expect("input against ReferenceSession");

        assert!(
            engine_side
                .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
                .expect("quiesce EngineSession"),
            "EngineSession never quiesced; screen:\n{}",
            engine_side.screen_text()
        );
        let settled = reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession");
        assert!(
            settled,
            "ReferenceSession never quiesced; screen:\n{}",
            reference_side.screen_text()
        );

        // the raw (unfiltered) log stays informative here even though this
        // test doesn't assert on it: see unknown_events_is_empty_on_a_
        // healthy_session and known_unmodeled_events_match_a_live_session
        // below for the assertions this event stream backs.
        println!(
            "raw unknown redraw events observed: {:?}",
            reference_side.raw_unknown_events()
        );

        let engine_text = engine_side.screen_text();
        let reference_text = reference_side.screen_text();
        let engine_region = buffer_region(&engine_text);
        let reference_region = buffer_region(&reference_text);
        assert_eq!(
            engine_region, reference_region,
            "EngineSession and ReferenceSession disagree on buffer-region text\n\
             engine:\n{engine_text}\nreference:\n{reference_text}"
        );
        assert!(
            reference_region.iter().any(|l| l.contains("hello world")),
            "typed text never showed up on either side's screen"
        );
    }

    /// Non-tautology: corrupting one `RefGrid` cell after quiescing must
    /// make the same comparison fail, proving the check above can actually
    /// detect a real disagreement rather than vacuously passing no matter
    /// what either side rendered.
    #[test]
    fn corrupted_ref_grid_cell_fails_the_comparison() {
        let mut engine_side = testenv::spawning(|| EngineSession::spawn(60, 12))
            .expect("EngineSession::spawn against real nvim");
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(60, 12))
            .expect("ReferenceSession::spawn against real nvim");

        assert!(engine_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce EngineSession"));

        engine_side
            .arm_and_input("ihello world<Esc>")
            .expect("input against EngineSession");
        reference_side
            .arm_and_input("ihello world<Esc>")
            .expect("input against ReferenceSession");
        assert!(engine_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce EngineSession"));
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        reference_side.grid.rows[0][0] = Cell {
            text: "Z".to_string(),
            hl_id: 0,
        };

        let engine_text = engine_side.screen_text();
        let reference_text = reference_side.screen_text();
        let engine_region = buffer_region(&engine_text);
        let reference_region = buffer_region(&reference_text);
        assert_ne!(
            engine_region, reference_region,
            "corrupting a RefGrid cell must break the comparison, not pass vacuously"
        );
    }

    /// The operational contract `unknown_events` exists for: a healthy
    /// `--clean` startup plus a plain insert must report zero genuinely
    /// novel event names, so any future non-empty result is real signal
    /// rather than the permanent background noise the raw log carries.
    #[test]
    fn unknown_events_is_empty_on_a_healthy_session() {
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(60, 12))
            .expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        reference_side
            .arm_and_input("ihello world<Esc>")
            .expect("input against ReferenceSession");
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        assert!(
            reference_side.unknown_events().is_empty(),
            "unknown_events() reported a genuinely novel event name on a healthy \
             run: {:?} (raw log: {:?})",
            reference_side.unknown_events(),
            reference_side.raw_unknown_events()
        );
    }

    /// Pins `KNOWN_UNMODELED_EVENTS` against what the live pinned nvim
    /// build actually emits, in both directions: an nvim upgrade that
    /// starts emitting a name outside this list fails here (that name
    /// would otherwise slip straight through
    /// `unknown_events_is_empty_on_a_healthy_session` unnoticed as
    /// known-unmodeled noise), and a stale entry this build no longer
    /// emits fails here too, rather than sitting in the const forever
    /// unverified.
    #[test]
    fn known_unmodeled_events_match_a_live_session() {
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(60, 12))
            .expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        reference_side
            .arm_and_input("ihello world<Esc>")
            .expect("input against ReferenceSession");
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        let observed: std::collections::BTreeSet<&str> = reference_side
            .raw_unknown_events()
            .iter()
            .map(String::as_str)
            .collect();
        let known: std::collections::BTreeSet<&str> =
            KNOWN_UNMODELED_EVENTS.iter().copied().collect();
        assert_eq!(
            observed, known,
            "KNOWN_UNMODELED_EVENTS is stale against a live nvim run: an entry \
             present in `observed` but missing from `known` means nvim now emits \
             an event name view-engine does not model yet (a real divergence \
             risk to investigate, not just re-pin); an entry present in `known` \
             but missing from `observed` means nvim stopped emitting one this \
             const still expects"
        );
    }

    /// Regression pin for the operator-pending false divergence
    /// (`corpus/fuzz-21-7.toml`, found by seeded fuzz): a script ending
    /// with a bare `y` leaves nvim waiting for a motion, a state
    /// `SafeState` never fires in, and the old unconditional marker keys
    /// were consumed as that motion -- aborting the operator back to
    /// normal mode and setting `'[`/`']` marks the engine side never got.
    /// `quiesce` must settle without typing anything, leaving the session
    /// in the exact operator-pending state the script produced.
    #[test]
    fn quiesce_leaves_an_operator_pending_state_untouched() {
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(60, 12))
            .expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        reference_side
            .arm_and_input("y")
            .expect("input against ReferenceSession");
        assert!(
            reference_side
                .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
                .expect("quiesce ReferenceSession"),
            "quiesce must settle on an operator-pending final state, not time out"
        );

        let (mode, blocking) = reference_side
            .get_mode()
            .expect("get_mode against ReferenceSession");
        assert_eq!(
            mode, "no",
            "the pending operator must survive quiesce untouched"
        );
        assert!(
            !blocking,
            "operator-pending is a typeahead wait, not a blocked prompt"
        );
        // eval still answers in operator-pending mode (only blocking
        // states defer it), so the marks probe can prove no aborted
        // operator ever ran
        let marks = reference_side
            .eval_str("join(map(getmarklist(bufnr('%')), 'v:val.mark'), ',')")
            .expect("eval_str against ReferenceSession");
        assert!(
            !marks.contains("'["),
            "quiesce's own keys completed/aborted the pending operator: marks {marks:?}"
        );
    }

    /// The blocked-wait sibling of the operator-pending pin above
    /// (`corpus/fuzz-42-6.toml`'s quiesce half): a script ending with a
    /// bare `t` leaves nvim blocked waiting for the motion's character
    /// argument. `SafeState` cannot fire there and a typed marker key
    /// would become that argument, so `quiesce` must settle on the
    /// silence window alone and leave the wait in place for the snapshot
    /// layer to capture.
    #[test]
    fn quiesce_settles_on_a_blocked_char_wait_without_typing() {
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(60, 12))
            .expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        reference_side
            .arm_and_input("ihello<Esc>0t")
            .expect("input against ReferenceSession");
        assert!(
            reference_side
                .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
                .expect("quiesce ReferenceSession"),
            "quiesce must settle on a blocked key-wait, not time out"
        );

        let (mode, blocking) = reference_side
            .get_mode()
            .expect("get_mode against ReferenceSession");
        assert!(
            blocking,
            "the pending character-argument wait must survive quiesce untouched"
        );
        assert_eq!(mode, "n", "nvim reports normal mode while blocked on t");
        assert!(
            reference_side.screen_text().contains("hello"),
            "the typed text never reached the reference grid:\n{}",
            reference_side.screen_text()
        );
    }

    /// Regression pin for the stalled-typeahead marker race: `:sleep`
    /// stalls nvim's main loop past the quiet window while the trailing `y`
    /// sits unprocessed in typeahead, and the fast probe keeps reporting
    /// the pre-stall `("n", false)` throughout. A marker armed *behind*
    /// that script would land in the stalled typeahead and execute as the
    /// freshly-started operator's input; armed ahead of it by
    /// [`ReferenceSession::arm_and_input`], nvim's own "there is typeahead"
    /// rule holds the hook until the sleep ends and the `y` is consumed, so
    /// the session settles in the operator-pending state the script
    /// actually produced -- no aborted operator, no spurious `'[`/`']`
    /// marks, and no fabricated mode/marks divergence against an engine
    /// side that never had either.
    #[test]
    fn quiesce_survives_a_script_that_stalls_the_main_loop_mid_typeahead() {
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(60, 12))
            .expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        reference_side
            .arm_and_input(":sleep 1500m<CR>y")
            .expect("input against ReferenceSession");

        assert!(
            reference_side
                .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
                .expect("quiesce ReferenceSession"),
            "quiesce must settle a stalled script's real final state"
        );
        let (mode, _) = reference_side
            .get_mode()
            .expect("get_mode against ReferenceSession");
        assert_eq!(
            mode, "no",
            "the operator the stalled typeahead's trailing y started must survive quiesce"
        );
        let marks = reference_side
            .eval_str("join(map(getmarklist(bufnr('%')), 'v:val.mark'), ',')")
            .expect("eval_str against ReferenceSession");
        assert!(
            !marks.contains("'["),
            "quiesce's own keys completed/aborted the pending operator: marks {marks:?}"
        );
    }

    /// Regression pin for a script that ends parked in a half-typed command
    /// nothing else can see: `"0` leaves a register prefix waiting for the
    /// operator that never comes, which suppresses `SafeState` (so the marker
    /// can never arrive) while `nvim_get_mode` still reports plain unblocked
    /// normal mode (so there is nothing for the fast probe to wait on
    /// either). Settling it requires nvim's own `state()`, and the state must
    /// survive: the register prefix is the script's real final state, so a
    /// settle that completed or aborted it would be reading a state the
    /// script never produced.
    #[test]
    fn quiesce_settles_a_script_parked_in_a_dangling_register_prefix() {
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(60, 12))
            .expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        reference_side
            .arm_and_input("ihello<Esc>\"0")
            .expect("input against ReferenceSession");

        let started = Instant::now();
        assert!(
            reference_side
                .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
                .expect("quiesce ReferenceSession"),
            "a dangling register prefix must settle, not run out the deadline"
        );
        assert!(
            started.elapsed() < QUIESCE_DEADLINE,
            "settled only by exhausting the deadline"
        );
        let flags = reference_side
            .eval_str("state()")
            .expect("eval_str against ReferenceSession");
        assert!(
            flags.contains('o'),
            "quiesce's own probing consumed the pending register prefix: state {flags:?}"
        );

        // a second call with nothing typed since: the arm command is keys,
        // so a settle that armed before recognising the wait would hand the
        // register prefix its register name and cancel it
        assert!(
            reference_side
                .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
                .expect("second quiesce ReferenceSession"),
            "a repeat settle on a parked session must settle again"
        );
        let flags = reference_side
            .eval_str("state()")
            .expect("eval_str against ReferenceSession");
        assert!(
            flags.contains('o'),
            "a repeat settle armed into the pending key-wait: state {flags:?}"
        );
    }

    /// The integrity backstop, still live: a session that moves *after*
    /// nvim's `SafeState` hook has already proved its typeahead empty had
    /// input this protocol cannot account for, so no comparison against the
    /// resulting state can be trusted. Constructed deterministically -- a
    /// one-shot timer feeds a mode-changing key strictly inside the silence
    /// window, so the marker fires in normal mode and the fast probe then
    /// observes insert mode -- because the perturbation this catches must
    /// never be reachable only by racing.
    #[test]
    fn quiesce_fails_loudly_when_the_session_moves_after_proving_itself_idle() {
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(20, 6))
            .expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side
            .quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE)
            .expect("quiesce ReferenceSession"));

        reference_side
            .arm_and_input("<Cmd>call timer_start(150, {-> feedkeys('i')})<CR>")
            .expect("input against ReferenceSession");

        match reference_side.quiesce(Duration::from_millis(600), QUIESCE_DEADLINE) {
            Err(OracleError::QuiescePerturbed { armed, observed }) => {
                assert_eq!(armed, "n", "the marker fired in normal mode");
                assert_eq!(
                    observed, "i",
                    "the timer's feedkeys moved the session after the marker proved it idle"
                );
            }
            other => unreachable!(
                "a session moving after its marker fired must fail loudly as \
                 QuiescePerturbed, got {other:?}"
            ),
        }
    }

    /// `quiesce` must return `Ok(false)` at `deadline` rather than hang while a
    /// blocking `:sleep` is still pending: no `SafeState` fires (nvim's
    /// main loop is not idle, it is inside the sleep) and no marker is ever
    /// observed, so this proves the deadline bound, not the marker path.
    #[test]
    fn quiesce_returns_false_at_the_deadline_during_a_pending_sleep() {
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(20, 6))
            .expect("ReferenceSession::spawn against real nvim");
        assert!(
            reference_side
                .quiesce(Duration::from_millis(50), Duration::from_secs(2))
                .expect("quiesce ReferenceSession"),
            "initial startup quiesce should settle before the sleep is even queued"
        );

        reference_side
            .arm_and_input("<Cmd>sleep 10<CR>")
            .expect("input against ReferenceSession");

        let settled = reference_side
            .quiesce(Duration::from_millis(100), Duration::from_secs(1))
            .expect("quiesce ReferenceSession");
        assert!(
            !settled,
            "quiesce reported settled while nvim was still inside a blocking :sleep 10"
        );
        assert!(
            reference_side.markers.is_armed(),
            "a marker that never fired must be handed back, or a retry queues a \
             second arm behind whatever is still draining"
        );
    }

    /// Determinism pin: a one-shot `timer_start` deliberately delays a
    /// mapping's redraw burst by `TIMER_DELAY`, chosen strictly less than
    /// `silence`. The marker's own `SafeState` can legitimately fire before
    /// the timer runs (nvim is genuinely idle while only a future timer is
    /// pending), so this proves the *silence backstop*, not the marker
    /// step, is what keeps `quiesce` from settling before the delayed
    /// burst: the timer's redraw resets the silence window, so settling
    /// only happens once a full `silence`-length gap follows it. A
    /// deterministic construction (`TIMER_DELAY < silence`, not equal or
    /// close), not a race on scheduling.
    #[test]
    fn quiesce_settles_only_after_a_delayed_timer_burst_not_before_it() {
        const TIMER_DELAY_MS: u64 = 80;
        let silence = Duration::from_millis(300);
        let mut reference_side = testenv::spawning(|| ReferenceSession::spawn(20, 6))
            .expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side
            .quiesce(silence, Duration::from_secs(2))
            .expect("quiesce ReferenceSession"));

        reference_side
            .arm_and_input(&format!(
                "<Cmd>call timer_start({TIMER_DELAY_MS}, {{-> setline(1, 'DELAYED')}})<CR>"
            ))
            .expect("input against ReferenceSession");

        let before = Instant::now();
        let settled = reference_side
            .quiesce(silence, Duration::from_secs(5))
            .expect("quiesce ReferenceSession");
        let elapsed = before.elapsed();

        assert!(settled, "quiesce never settled after the delayed burst");
        assert!(
            elapsed >= Duration::from_millis(TIMER_DELAY_MS),
            "quiesce settled ({elapsed:?}) before the delayed timer ({TIMER_DELAY_MS}ms) could have fired"
        );
        assert!(
            reference_side.screen_text().contains("DELAYED"),
            "the delayed timer's own mutation never showed up in the reference screen:\n{}",
            reference_side.screen_text()
        );
    }
}
