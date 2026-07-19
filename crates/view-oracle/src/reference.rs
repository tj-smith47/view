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

use std::time::{Duration, Instant};

use view_core::events::{clamp_dim, saturate_u16, GridCell, UiEvent};
use view_engine::{DamagePump, Engine, EngineConfig};

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
}

impl RefGrid {
    fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            rows: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
        }
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
}

/// Vimscript augroup name [`ReferenceSession::install_quiesce_hooks`]
/// registers once at spawn time and every [`ReferenceSession::quiesce`] call
/// re-arms; kept as a named constant so the setup command and the per-call
/// marker command cannot drift apart into two different group names.
const QUIESCE_AUGROUP: &str = "ViewOracleQuiesce";

/// Wraps `cmd` as a single `<Cmd>...<CR>` key-notation segment: an Ex
/// command executed via `nvim_input` without leaving the current mode or
/// waiting for a reply, the mechanism every quiesce-protocol command in this
/// module rides on (see [`ReferenceSession::quiesce`]'s doc comment for why
/// this, and not `nvim_command`/`nvim_eval`, is what proves ordering).
fn cmd_key(cmd: &str) -> String {
    format!("<Cmd>{cmd}<CR>")
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
    "msg_showmode",
    "option_set",
    "set_icon",
    "set_title",
    "update_menu",
    "win_viewport",
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
    next_marker_seq: u64,
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
        Self::spawn_configured(
            EngineConfig {
                extra_args: vec!["--clean".into(), "-n".into()],
                ..EngineConfig::default()
            },
            cols,
            rows,
        )
    }

    /// Same as [`spawn`](Self::spawn), but with a caller-supplied
    /// [`EngineConfig`] (a non-default `nvim_bin`, timeout, or extra
    /// arguments) instead of [`spawn`](Self::spawn)'s own `--clean` + `-n`
    /// default.
    ///
    /// # Errors
    ///
    /// Same as [`spawn`](Self::spawn).
    pub fn spawn_configured(cfg: EngineConfig, cols: u16, rows: u16) -> Result<Self, OracleError> {
        let mut engine = Engine::spawn(cfg)?;
        engine.handle.ui_attach(cols, rows)?;
        let (sink, _unused_rx) = std::sync::mpsc::sync_channel(64);
        let (pump, _cutover) = engine.start_pump(sink);
        let mut session = Self {
            engine,
            pump,
            grid: RefGrid::new(),
            mode: String::new(),
            unknown_events_raw: Vec::new(),
            next_marker_seq: 0,
            term_width: cols,
            term_height: rows,
            tab_count: 0,
        };
        session.install_quiesce_hooks()?;
        Ok(session)
    }

    /// Pins `timeoutlen`/`updatetime` far outside any run's real duration (a
    /// mapping timeout or a `CursorHold` firing mid-script would inject
    /// nondeterministic redraw noise into the quiesce silence window) and
    /// creates the empty [`QUIESCE_AUGROUP`] each [`quiesce`](Self::quiesce)
    /// call re-arms with a fresh `SafeState` hook. Sent via `nvim_input`
    /// (fire-and-forget, no reply awaited) rather than a synchronous
    /// `nvim_command` request: everything this protocol depends on is typed
    /// through the same typeahead queue real test input rides, and mixing
    /// in a request/reply round-trip here would be exactly the ordering
    /// hazard [`quiesce`](Self::quiesce)'s doc comment explains why to
    /// avoid.
    fn install_quiesce_hooks(&mut self) -> Result<(), OracleError> {
        let setup = format!(
            "{}{}{}",
            cmd_key("set timeoutlen=86400000 updatetime=86400000"),
            cmd_key(&format!("augroup {QUIESCE_AUGROUP}")),
            cmd_key("autocmd!"),
        ) + &cmd_key("augroup END");
        self.engine.handle.input(&setup)?;
        Ok(())
    }

    /// Forwards one encoded key `notation` via `nvim_input`, identical to
    /// [`EngineSession::input`](crate::EngineSession::input).
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Engine`] if the connection's writer thread has
    /// already exited.
    pub fn input(&mut self, notation: &str) -> Result<(), OracleError> {
        self.engine.handle.input(notation).map_err(Into::into)
    }

    /// Waits for input driven via [`input`](Self::input) to be fully
    /// processed, using nvim's own idle signal rather than any RPC-reply
    /// ordering: an `nvim_eval`/`nvim_command` round-trip only proves the
    /// channel consumed prior messages, not that the *processing* those
    /// messages queued (redraw bursts, autocmd cascades) has finished,
    /// since `nvim_input` queues keys into nvim's typeahead buffer and
    /// returns before they are processed.
    ///
    /// The settle signal is a `SafeState` autocmd (fires when nvim's main
    /// loop is idle with no pending input), re-armed on every call as a
    /// `++once` hook in [`QUIESCE_AUGROUP`] that `echom`s a sequence-tagged
    /// marker string. The marker command is written as a single
    /// `nvim_input` call, landing in the exact same typeahead FIFO as every
    /// key driven by prior [`input`](Self::input) calls on this connection
    /// -- so by the time nvim dequeues and processes *this* keystroke,
    /// every earlier one is guaranteed already processed, without relying
    /// on the engine's RPC scheduler to preserve that order (it does not
    /// need to: typeahead order is the guarantee, not RPC order). A stale
    /// marker from a pre-input idle period cannot satisfy this: its
    /// sequence number was baked in by an earlier call and will not match.
    ///
    /// The marker alone is not sufficient: nvim can be genuinely idle while
    /// a still-pending timer (e.g. a deferred `timer_start` mapping) has
    /// not yet fired, so seeing the marker only proves processing *through*
    /// the point this call's own keystroke was queued, not that every
    /// later async burst has landed. `silence` is the backstop: after the
    /// marker is observed, this keeps draining and applying events, and
    /// only returns `true` once a full `silence`-length window has elapsed
    /// with nothing new arriving. A burst that lands inside that window
    /// (the deferred timer firing) resets the window, so quiescence is
    /// never declared while one is still in flight. The whole wait is
    /// bounded by `deadline`; returns `false` if it elapses first, whether
    /// or not the marker was ever seen.
    ///
    /// Every drained event is applied to this session's `RefGrid`
    /// regardless of whether it precedes or follows the marker, and every
    /// `UiEvent::Unknown` observed is recorded (see
    /// [`unknown_events`](Self::unknown_events)) rather than silently
    /// dropped.
    #[must_use]
    pub fn quiesce(&mut self, silence: Duration, deadline: Duration) -> bool {
        let start = Instant::now();
        self.next_marker_seq += 1;
        let marker = format!("VIEW_ORACLE_QUIESCE:{}", self.next_marker_seq);
        let arm = cmd_key(&format!(
            "autocmd! {QUIESCE_AUGROUP} SafeState * ++once echom '{marker}'"
        ));
        if self.engine.handle.input(&arm).is_err() {
            return false;
        }

        let mut marker_seen = false;
        let mut quiet_since: Option<Instant> = None;
        loop {
            let events = self.pump.take_damage();
            if !events.is_empty() {
                for ev in events {
                    if !marker_seen {
                        if let UiEvent::MsgShow { content, .. } = &ev {
                            marker_seen = content.iter().any(|(_, text)| text.contains(&marker));
                        }
                    }
                    self.apply(ev);
                }
                quiet_since = Some(Instant::now());
            }
            if marker_seen {
                if let Some(quiet_since) = quiet_since {
                    if quiet_since.elapsed() >= silence {
                        return true;
                    }
                }
            }
            if start.elapsed() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
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
    /// healthy run (`option_set`, `win_viewport`, `set_title`, and the rest
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
            // discarded for grid purposes: no cell content of theirs ever
            // reaches the grid while ext_hlstate/ext_cmdline/ext_messages/
            // ext_tabline/ext_popupmenu are attached (see this fn's doc
            // comment's ext-event policy paragraph)
            UiEvent::HlAttrDefine { .. }
            | UiEvent::DefaultColorsSet { .. }
            | UiEvent::HlGroupSet { .. }
            | UiEvent::Flush
            | UiEvent::ModeInfoSet { .. }
            | UiEvent::CmdlineShow { .. }
            | UiEvent::CmdlinePos { .. }
            | UiEvent::CmdlineHide
            | UiEvent::MsgShow { .. }
            | UiEvent::MsgClear
            | UiEvent::PopupmenuShow { .. }
            | UiEvent::PopupmenuSelect { .. }
            | UiEvent::PopupmenuHide
            | UiEvent::MouseOn
            | UiEvent::MouseOff => {}
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
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
        let mut engine_side =
            EngineSession::spawn(60, 12).expect("EngineSession::spawn against real nvim");
        let mut reference_side =
            ReferenceSession::spawn(60, 12).expect("ReferenceSession::spawn against real nvim");

        // EngineSession's pump_until_flush returns on the first Flush it
        // sees, which is otherwise indistinguishable from a still-pending
        // startup batch: draining startup traffic first (the same guard
        // pump_until_flush_returns_false_at_the_deadline_when_no_flush_arrives
        // uses in driver_legs.rs) is what ReferenceSession's quiesce needs
        // no equivalent for, since each call arms a freshly
        // sequence-numbered marker at call time: the marker the later,
        // post-input quiesce call below arms is issued after the real
        // input is queued, so a stale pre-input idle period can never
        // satisfy it.
        while engine_side.pump_until_flush(Duration::from_millis(500)) {}
        assert!(reference_side.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE));

        engine_side
            .input("ihello world<Esc>")
            .expect("input against EngineSession");
        reference_side
            .input("ihello world<Esc>")
            .expect("input against ReferenceSession");

        // pump_until_flush is a "wait for at least one Flush" primitive,
        // not a "wait until settled" one: a multi-key notation like this
        // one can land across more than one Flush, so the first call alone
        // can observe a still-mid-edit screen. Looping it until a full
        // window passes with nothing new is the same drain-to-quiet pattern
        // used above for startup traffic; ReferenceSession's quiesce needs
        // no such caller-side loop because its own silence backstop already
        // does this internally.
        assert!(
            engine_side.pump_until_flush(QUIESCE_DEADLINE),
            "EngineSession never observed a Flush; screen:\n{}",
            engine_side.screen_text()
        );
        while engine_side.pump_until_flush(Duration::from_millis(500)) {}
        assert!(
            reference_side.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE),
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
        let mut engine_side =
            EngineSession::spawn(60, 12).expect("EngineSession::spawn against real nvim");
        let mut reference_side =
            ReferenceSession::spawn(60, 12).expect("ReferenceSession::spawn against real nvim");

        while engine_side.pump_until_flush(Duration::from_millis(500)) {}

        engine_side
            .input("ihello world<Esc>")
            .expect("input against EngineSession");
        reference_side
            .input("ihello world<Esc>")
            .expect("input against ReferenceSession");
        assert!(engine_side.pump_until_flush(QUIESCE_DEADLINE));
        assert!(reference_side.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE));

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
        let mut reference_side =
            ReferenceSession::spawn(60, 12).expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE));

        reference_side
            .input("ihello world<Esc>")
            .expect("input against ReferenceSession");
        assert!(reference_side.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE));

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
        let mut reference_side =
            ReferenceSession::spawn(60, 12).expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE));

        reference_side
            .input("ihello world<Esc>")
            .expect("input against ReferenceSession");
        assert!(reference_side.quiesce(QUIESCE_SILENCE, QUIESCE_DEADLINE));

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

    /// `quiesce` must return `false` at `deadline` rather than hang while a
    /// blocking `:sleep` is still pending: no `SafeState` fires (nvim's
    /// main loop is not idle, it is inside the sleep) and no marker is ever
    /// observed, so this proves the deadline bound, not the marker path.
    #[test]
    fn quiesce_returns_false_at_the_deadline_during_a_pending_sleep() {
        let mut reference_side =
            ReferenceSession::spawn(20, 6).expect("ReferenceSession::spawn against real nvim");
        assert!(
            reference_side.quiesce(Duration::from_millis(50), Duration::from_secs(2)),
            "initial startup quiesce should settle before the sleep is even queued"
        );

        reference_side
            .input("<Cmd>sleep 10<CR>")
            .expect("input against ReferenceSession");

        let settled = reference_side.quiesce(Duration::from_millis(100), Duration::from_secs(1));
        assert!(
            !settled,
            "quiesce reported settled while nvim was still inside a blocking :sleep 10"
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
        let mut reference_side =
            ReferenceSession::spawn(20, 6).expect("ReferenceSession::spawn against real nvim");
        assert!(reference_side.quiesce(silence, Duration::from_secs(2)));

        reference_side
            .input(&format!(
                "<Cmd>call timer_start({TIMER_DELAY_MS}, {{-> setline(1, 'DELAYED')}})<CR>"
            ))
            .expect("input against ReferenceSession");

        let before = Instant::now();
        let settled = reference_side.quiesce(silence, Duration::from_secs(5));
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
