//! What `nvim_ui_try_resize_grid` does to a split window, read from a live
//! engine.
//!
//! The tiled look rests on one claim: a window grid can be made smaller
//! than the layout slot nvim keeps for it, so the difference is the UI's to
//! draw a frame and a gap into. Every geometry rule the compositor follows
//! is a consequence of that claim plus three details -- that the request
//! stands until it is replaced, that `0, 0` is the only value that clears
//! it, and that a winbar row arrives on top of the height asked for.
//!
//! The connection is a bare [`EngineHandle::start`] over a child this file
//! spawns, the shape `multigrid_capture.rs` uses and for the same reason: a
//! pumped connection folds the redraw stream into a model, and the subject
//! here is the stream itself.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use view_core::events::UiEvent;
use view_engine::process::EngineConfig;
use view_engine::ui_events::decode_redraw;
use view_engine::{EngineHandle, EngineNotification};
use view_oracle::UI_EXT_OPTIONS_MULTIGRID;

/// The terminal both axes are read at. Wide and tall enough that a slot
/// halved by a `:vsplit` is still far from the framed minimum, and the two
/// axes differ so a width that reached the wrong field cannot hide.
const COLS: u16 = 80;
const ROWS: u16 = 24;

/// The ring a gapped tile spends on each axis: two cells per side.
const RING: u64 = 4;

/// How long a step's redraw traffic may keep arriving before the reader
/// calls it settled, and how long it waits for the first `flush` that says
/// the step produced traffic at all. Both host-scaled, and each is a
/// ceiling on a wait, and neither tells two events apart.
const QUIET: Duration = Duration::from_millis(80);
const FLUSH_WAIT: Duration = Duration::from_secs(10);

/// A window's layout slot, as `win_pos` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    startrow: u64,
    startcol: u64,
    width: u64,
    height: u64,
}

/// A live engine and everything its redraw stream has said so far about
/// where windows sit and how big their grids are.
struct RawUi {
    child: Child,
    handle: EngineHandle,
    notifications: Receiver<EngineNotification>,
    /// The last `grid_resize` per grid, as `(width, height)`.
    sizes: BTreeMap<u64, (u64, u64)>,
    /// The last `win_pos` per grid.
    slots: BTreeMap<u64, Slot>,
    /// The last `win_viewport_margins` top per grid.
    margins: BTreeMap<u64, u64>,
    /// How many `win_pos` events have arrived, over every grid: an inner
    /// request that re-announced a position would move this.
    win_pos_count: usize,
    /// Every `grid_line` since the last [`RawUi::forget_lines`], as
    /// `(grid, row, cells written)`.
    lines: Vec<(u64, u64, u64)>,
}

impl Drop for RawUi {
    fn drop(&mut self) {
        // no graceful `qa!` first: this child holds nothing worth saving,
        // and a wait on an engine that never answers would hang the suite
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl RawUi {
    /// Spawns an isolated `nvim --embed`, attaches under `ext_multigrid`
    /// and reads the attach cycle.
    fn spawn() -> Self {
        let cfg = EngineConfig::isolated();
        view_engine::env::prepare_empty_search_path().unwrap();
        view_engine::env::prepare_hermetic_home().unwrap();
        let mut command = Command::new(&cfg.nvim_bin);
        command.arg("--embed").args(&cfg.extra_args);
        for (name, value) in cfg.env_plan() {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("the pinned nvim must be on PATH");
        let stdout = child.stdout.take().unwrap();
        let stdin = child.stdin.take().unwrap();
        let (handle, notifications) = EngineHandle::start(stdout, stdin);
        handle
            .ui_attach(COLS, ROWS, UI_EXT_OPTIONS_MULTIGRID)
            .unwrap();
        let mut ui = Self {
            child,
            handle,
            notifications,
            sizes: BTreeMap::new(),
            slots: BTreeMap::new(),
            margins: BTreeMap::new(),
            lines: Vec::new(),
            win_pos_count: 0,
        };
        ui.settle();
        ui
    }

    /// Drains every redraw notification the engine has queued for the step
    /// just issued, folding it into this reader's state.
    ///
    /// Every step is issued through a blocking call or followed by one, so
    /// nvim has already run it by the time this is entered and its traffic
    /// is either queued or being written. The first `flush` says a redraw
    /// cycle closed; the quiet window after it catches a second cycle the
    /// same step scheduled.
    fn settle(&mut self) {
        let deadline = Instant::now() + view_test_support::host_deadline(FLUSH_WAIT);
        let quiet = view_test_support::host_deadline(QUIET);
        let mut flushed = false;
        loop {
            let wait = if flushed {
                quiet
            } else {
                deadline.saturating_duration_since(Instant::now())
            };
            if wait.is_zero() {
                return;
            }
            match self.notifications.recv_timeout(wait) {
                Ok(notification) => {
                    if notification.method != "redraw" {
                        continue;
                    }
                    for event in decode_redraw(&notification.params) {
                        flushed |= matches!(event, UiEvent::Flush);
                        self.fold(&event);
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    fn fold(&mut self, event: &UiEvent) {
        match event {
            UiEvent::GridResize {
                grid,
                width,
                height,
            } => {
                self.sizes.insert(*grid, (*width, *height));
            }
            UiEvent::WinPos {
                grid,
                startrow,
                startcol,
                width,
                height,
                ..
            } => {
                self.win_pos_count += 1;
                self.slots.insert(
                    *grid,
                    Slot {
                        startrow: *startrow,
                        startcol: *startcol,
                        width: *width,
                        height: *height,
                    },
                );
            }
            UiEvent::WinViewportMargins { grid, top, .. } => {
                self.margins.insert(*grid, *top);
            }
            UiEvent::GridLine {
                grid, row, cells, ..
            } => {
                let written = cells.iter().map(|cell| cell.repeat.max(1)).sum();
                self.lines.push((*grid, *row, written));
            }
            _ => {}
        }
    }

    /// Runs `cmd` and reads the cycle it produced.
    fn command(&mut self, cmd: &str) {
        self.handle.command(cmd).unwrap();
        self.settle();
    }

    /// Sends an inner-size request and reads the cycle it produced. The
    /// blocking eval is what proves nvim has consumed the notification
    /// before the reader starts waiting on its traffic.
    fn request(&mut self, grid: u64, width: u16, height: u16) {
        self.handle.try_resize_grid(grid, width, height).unwrap();
        self.handle.eval_str("1").unwrap();
        self.settle();
    }

    fn forget_lines(&mut self) {
        self.lines.clear();
    }

    /// The one window grid a fresh session has: the single grid `win_pos`
    /// named that is not the global one.
    fn only_window_grid(&self) -> u64 {
        let named: Vec<u64> = self.slots.keys().copied().filter(|id| *id != 1).collect();
        assert_eq!(
            named.len(),
            1,
            "a fresh session holds one window grid; win_pos named {named:?} \
             (slots {:?})",
            self.slots
        );
        named[0]
    }

    fn slot(&self, grid: u64) -> Slot {
        *self
            .slots
            .get(&grid)
            .unwrap_or_else(|| panic!("no win_pos for grid {grid} in {:?}", self.slots))
    }

    fn size(&self, grid: u64) -> (u64, u64) {
        *self
            .sizes
            .get(&grid)
            .unwrap_or_else(|| panic!("no grid_resize for grid {grid} in {:?}", self.sizes))
    }
}

#[test]
fn a_resized_window_grid_keeps_the_slot_win_pos_reports() {
    let mut ui = RawUi::spawn();
    let grid = ui.only_window_grid();
    let before = ui.slot(grid);

    let announced = ui.win_pos_count;
    ui.request(
        grid,
        (before.width - RING) as u16,
        (before.height - RING) as u16,
    );

    let after = ui.slot(grid);
    assert_eq!(
        after, before,
        "the inner request must leave the layout slot where it was; \
         win_pos went from {before:?} to {after:?}"
    );
    assert_eq!(
        ui.win_pos_count, announced,
        "nvim re-announces no position for an inner request, so the slot \
         the compositor recorded at attach is the one it keeps drawing \
         into; it saw {} win_pos events against {announced} before",
        ui.win_pos_count
    );
    let layout = (
        ui.handle.eval_str("nvim_win_get_width(0)").unwrap(),
        ui.handle.eval_str("nvim_win_get_height(0)").unwrap(),
    );
    assert_eq!(
        layout,
        (before.width.to_string(), before.height.to_string()),
        "nvim's own layout must still hold the whole slot while the grid is \
         smaller than it; the grid went to {:?}",
        ui.size(grid)
    );
    assert!(
        ui.size(grid) < (before.width, before.height),
        "the grid must have shrunk inside that slot, or the two readings \
         above agree for the wrong reason; sizes were {:?}",
        ui.sizes
    );
}

#[test]
fn a_resized_window_grid_reports_the_requested_inner_size() {
    let mut ui = RawUi::spawn();
    let grid = ui.only_window_grid();
    let slot = ui.slot(grid);
    let (want_w, want_h) = (slot.width - RING, slot.height - RING);

    ui.forget_lines();
    ui.request(grid, want_w as u16, want_h as u16);

    assert_eq!(
        ui.size(grid),
        (want_w, want_h),
        "grid_resize must report the requested inner size; sizes were {:?}",
        ui.sizes
    );
    let widest = ui
        .lines
        .iter()
        .filter(|(id, _, _)| *id == grid)
        .map(|(_, _, written)| *written)
        .max();
    assert!(
        widest.is_some_and(|widest| widest <= want_w),
        "no grid_line for the resized grid may write past its inner width \
         {want_w}; the widest run wrote {widest:?}"
    );
}

#[test]
fn a_later_vsplit_leaves_the_stale_request_standing() {
    let mut ui = RawUi::spawn();
    let grid = ui.only_window_grid();
    let slot = ui.slot(grid);
    let (want_w, want_h) = (slot.width - RING, slot.height - RING);
    ui.request(grid, want_w as u16, want_h as u16);
    assert_eq!(ui.size(grid), (want_w, want_h));

    ui.command("vsplit");

    let halved = ui.slot(grid);
    assert!(
        halved.width < slot.width,
        "the vsplit must halve the slot under test; it went from \
         {slot:?} to {halved:?}"
    );
    assert_eq!(
        ui.size(grid),
        (want_w, want_h),
        "the request stands across a layout change, so the grid keeps the \
         size it was asked for and view owes a fresh one; sizes were {:?}",
        ui.sizes
    );
}

#[test]
fn a_zero_request_restores_the_slot_size() {
    let mut ui = RawUi::spawn();
    let grid = ui.only_window_grid();
    let slot = ui.slot(grid);
    ui.request(
        grid,
        (slot.width - RING) as u16,
        (slot.height - RING) as u16,
    );
    assert_ne!(ui.size(grid), (slot.width, slot.height));

    ui.request(grid, 0, 0);

    assert_eq!(
        ui.size(grid),
        (slot.width, slot.height),
        "a zero request is the only value that returns a grid to its slot; \
         sizes were {:?}",
        ui.sizes
    );
}

#[test]
fn a_winbar_adds_one_row_the_request_did_not_ask_for() {
    let mut ui = RawUi::spawn();
    let grid = ui.only_window_grid();
    ui.command("set winbar=spike");
    let slot = ui.slot(grid);
    let (want_w, want_h) = (slot.width - RING, slot.height - RING);

    ui.request(grid, want_w as u16, want_h as u16);

    assert_eq!(
        ui.size(grid),
        (want_w, want_h + 1),
        "the winbar row arrives on top of the height requested, so a \
         request that ignored it would overshoot the slot; sizes were {:?}",
        ui.sizes
    );
    assert_eq!(
        ui.margins.get(&grid),
        Some(&1),
        "win_viewport_margins is where that extra row is announced; \
         margins were {:?}",
        ui.margins
    );
}
