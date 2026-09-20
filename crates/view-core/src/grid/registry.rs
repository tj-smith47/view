//! Grids addressed by the id nvim gives them, and the windows placed over
//! them.
//!
//! [`Grid`] models one grid's cells. Under `ext_multigrid` nvim addresses
//! many of them by id and announces where each one sits with its own
//! window-placement events, so the addressing and the geometry live here
//! rather than in the cell buffer. Pure data, like everything in this
//! module: no I/O, no RPC, and every wire-sourced value already saturated by
//! the decoder that produced it.

use crate::events::WinHandle;
use crate::grid::{Grid, GridDamage, GridOp};
use crate::model::Look;
use crate::native::geometry::NativeSurface;

/// A grid's identity as nvim assigns it. The global grid keeps the id the
/// engine gives it rather than a sentinel, so single-grid and multigrid
/// sessions share one code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GridId(pub u64);

/// The global grid, which nvim numbers 1 in both attach modes and never
/// destroys (`docs/multigrid-wire-capture.md`, "The grid id space"). Under
/// `ext_multigrid` it carries the chrome between windows -- statuslines,
/// separators, the ruler area -- and under single-grid it carries
/// everything, which is why it is a paintable pane in both.
pub const GLOBAL_GRID: GridId = GridId(1);

/// Where a grid sits on screen, and what kind of surface it is.
///
/// The content of a pane is an enum with one variant today because every
/// pane holds an engine window. Compositing non-engine content adds a
/// variant here rather than a parallel structure.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneKind {
    /// An ordinary window laid out by nvim's own window tree.
    Window,
    /// A floating window, drawn above the window layer at its own zindex.
    Float {
        /// nvim's own stacking order, larger is nearer the viewer.
        zindex: u32,
        /// The grid the float was anchored to.
        anchor_grid: GridId,
    },
    /// A window view opened for one of its own surfaces. Laid out by nvim
    /// like any other window, and painted by view rather than from the
    /// cells nvim sends for it.
    Native {
        /// The surface view opened the window for.
        surface: NativeSurface,
    },
    /// nvim's own message/cmdline area (`msg_set_pos`), positioned only
    /// when `ext_messages` is not attached. Its own variant rather than a
    /// `Float`: it has no anchor grid, and unlike an inactive window it is
    /// never dimmed to `NormalNC` -- nvim never treats its own message text
    /// as an unfocused window.
    Message {
        /// nvim's own stacking order, carried the same as a float's.
        zindex: u32,
    },
}

/// A grid that has been sized, placed, or both.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// The grid whose cells this pane paints.
    pub id: GridId,
    /// Where the grid's cell `(0, 0)` sits: the inner origin under gapped
    /// tiles, and the slot's own origin everywhere else.
    pub origin: (u16, u16),
    /// The layout slot nvim reported, as `(row, col, width, height)`, which
    /// is what the frame painter draws into. A pane nvim never placed as an
    /// ordinary window -- a float, the message area, the global grid --
    /// carries its origin and the grid's own size here.
    pub slot: (u16, u16, u16, u16),
    /// Which layer the pane belongs to and, for a float, how it sorts.
    pub kind: PaneKind,
    /// Whether nvim has taken the pane off screen without destroying it, per
    /// `win_hide`/`win_external_pos`.
    pub hidden: bool,
}

/// One decoded operation as nvim addresses it: cells into a named grid, or a
/// window placed over one.
///
/// Every variant names its grid, because every event on the wire does. The
/// cell operations are the `ext_linegrid` vocabulary [`Grid`] already
/// applies; the rest are the placement vocabulary `ext_multigrid` adds, with
/// field names taken from the declared parameter names in
/// `docs/multigrid-wire-capture.md`.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridEvent {
    /// A cell operation addressed to one grid.
    Cells {
        /// The grid to apply it to.
        grid: GridId,
        /// What to do to its cells.
        op: GridOp,
    },
    /// `grid_destroy`: the grid is gone, along with any placement over it.
    Destroy {
        /// The grid nvim destroyed.
        grid: GridId,
    },
    /// `win_pos`: an ordinary window occupies the slot at
    /// `(startrow, startcol)`, `width` by `height` cells.
    ///
    /// The slot is the whole box nvim's layout tree keeps for the window,
    /// which is larger than the grid it draws text into wherever view has
    /// asked for an inner size inside it.
    Window {
        /// The window's grid.
        grid: GridId,
        /// The window nvim placed, as it addresses it.
        win: WinHandle,
        /// Screen row of the slot's first row.
        startrow: u16,
        /// Screen column of the slot's first column.
        startcol: u16,
        /// Columns the slot spans.
        width: u16,
        /// Rows the slot spans.
        height: u16,
    },
    /// `win_viewport_margins`: rows of `grid` that are not viewport --
    /// what `winbar` takes off the top.
    ///
    /// nvim adds the top margin to whatever inner height view asks for, so
    /// the request has to spend it first.
    Margins {
        /// The window's grid.
        grid: GridId,
        /// Rows above the viewport.
        top: u16,
    },
    /// `win_float_pos`: a floating window sits at the position nvim already
    /// resolved from the anchor.
    Float {
        /// The float's grid, which holds its border too.
        grid: GridId,
        /// The grid the float is anchored to.
        anchor_grid: GridId,
        /// Resolved screen row, after the anchor was applied.
        screen_row: u16,
        /// Resolved screen column, after the anchor was applied.
        screen_col: u16,
        /// nvim's stacking layer for the float.
        zindex: u32,
        /// nvim's own order within one `zindex`.
        compindex: u32,
    },
    /// `win_external_pos`: the window left the layout for a UI window of its
    /// own, so it has no box on this screen.
    External {
        /// The window's grid.
        grid: GridId,
    },
    /// `win_hide`: the window is off screen but alive, as every window of a
    /// tab page that stops being current is.
    Hide {
        /// The hidden window's grid.
        grid: GridId,
    },
    /// `win_close`: the window is gone; its grid follows in the same cycle.
    Close {
        /// The closed window's grid.
        grid: GridId,
    },
    /// `msg_set_pos`: nvim's message area sits at screen row `row` (column
    /// 0, full width), above the window layer at `zindex` and, within one
    /// zindex, in `compindex` order.
    Message {
        /// The message area's grid.
        grid: GridId,
        /// Screen row of the message area's first row.
        row: u16,
        /// nvim's own stacking order, carried the same as a float's.
        zindex: u32,
        /// nvim's own order within one `zindex`.
        compindex: u32,
    },
}

impl PaneKind {
    /// Whether the pane sits in the window layer: an ordinary window or one
    /// view opened for a surface of its own. Both are laid out by nvim and
    /// both are framed as tiles; only what paints their cells differs.
    #[must_use]
    pub const fn is_window(&self) -> bool {
        matches!(self, Self::Window | Self::Native { .. })
    }

    /// The surface this pane was opened for, or `None` for a pane that is
    /// not one of view's own.
    #[must_use]
    pub const fn native_surface(&self) -> Option<NativeSurface> {
        match self {
            Self::Native { surface } => Some(*surface),
            _ => None,
        }
    }
}

/// A grid nvim has named, with the placement it has been given if any.
#[derive(Debug, Clone)]
struct Slot {
    id: GridId,
    grid: Grid,
    /// `None` for a grid nvim has sized but not placed, which is every grid
    /// between its first `win_viewport_margins` and its `win_pos`.
    placed: Option<Placement>,
    /// What `win_pos` last said about this grid's window, `None` for a
    /// grid that is not an ordinary window.
    window: Option<Window>,
}

/// What a standing inner request answers for: the slot nvim gave the
/// window, the look it was sized under, and the rows the winbar adds.
type RequestKey = ((u16, u16, u16, u16), Look, u16);

/// The layout nvim keeps for one ordinary window, and the inner size view
/// has asked for inside it.
#[derive(Debug, Clone)]
struct Window {
    /// The window nvim placed, as it addresses it.
    win: WinHandle,
    /// The slot, as `(row, col, width, height)`.
    slot: (u16, u16, u16, u16),
    /// `win_viewport_margins`'s top for this grid: rows nvim adds on top
    /// of whatever inner height it is asked for.
    margin_top: u16,
    /// The key the standing inner request answers for, `None` while this
    /// grid owes one.
    requested: Option<RequestKey>,
}

#[derive(Debug, Clone)]
struct Placement {
    origin: (u16, u16),
    kind: PaneKind,
    hidden: bool,
    /// Whether view is holding this float off the screen until it has
    /// classified what opened it. Distinct from `hidden`, which is nvim's
    /// own answer about the window and is rewritten by every placement
    /// event: this one is view's, and survives the position steps a plugin
    /// animating its window sends.
    withheld: bool,
    /// nvim's own tiebreak within one zindex; 0 for a window, which has no
    /// stacking order of its own because windows never overlap.
    compindex: u32,
}

impl Placement {
    /// Which layer the placement paints in: every float is above every
    /// window, whatever zindex nvim gave it.
    fn layer(&self) -> u8 {
        match self.kind {
            PaneKind::Window | PaneKind::Native { .. } => 0,
            PaneKind::Float { .. } | PaneKind::Message { .. } => 1,
        }
    }

    fn zindex(&self) -> u32 {
        match self.kind {
            PaneKind::Window | PaneKind::Native { .. } => 0,
            PaneKind::Float { zindex, .. } | PaneKind::Message { zindex } => zindex,
        }
    }
}

/// Beyond any real session's window count by orders of magnitude, and small
/// enough that a desynced stream naming fresh grid ids forever cannot grow
/// this unboundedly -- the same reason the decoder clamps grid dimensions.
const MAX_GRIDS: usize = 256;

/// Every grid nvim has named, the windows placed over them, and the order
/// they paint in.
///
/// The global grid is a field rather than an entry because it is the one
/// grid nvim never destroys and never places: it is always the full screen
/// at the origin, and holding it apart keeps the single-grid session's every
/// `grid_line` a branch rather than a search.
#[derive(Debug, Clone)]
pub struct GridRegistry {
    global: Grid,
    slots: Vec<Slot>,
    cursor: Option<GridId>,
    /// Window handles view opened for its own surfaces, waiting for the
    /// `win_pos` that places them. A claim outlives the placement so a
    /// window nvim re-places keeps its kind.
    claims: Vec<(WinHandle, NativeSurface)>,
    /// How window layout is drawn, which decides where a window grid's
    /// cell `(0, 0)` sits inside the slot nvim gave it.
    look: Look,
    /// Set by any event that moves, reveals or removes a box on screen.
    /// Such an event names no cells at all, and the rows a vacated box
    /// leaves behind belong to whatever was under it, so the layout
    /// changing is the one thing a per-pane row list cannot express.
    placement_dirty: bool,
}

impl GridRegistry {
    /// A registry holding nothing but an empty global grid.
    #[must_use]
    pub fn new() -> Self {
        Self {
            global: Grid::new(),
            slots: Vec::new(),
            cursor: None,
            claims: Vec::new(),
            look: Look::default(),
            placement_dirty: false,
        }
    }

    /// The global grid, for reading.
    #[must_use]
    #[inline]
    pub fn global(&self) -> &Grid {
        &self.global
    }

    /// Drains what changed since the last call, in screen rows.
    ///
    /// Per pane, not per screen: a pane's own rows are grid-local, so each
    /// one's damage is offset by the row its box starts at and the frame
    /// repaints the rows a window actually redrew rather than every row a
    /// window could have. A layout change has no rows of its own -- the box
    /// that moved uncovers whatever was beneath it -- so it collapses to
    /// the whole frame, which is also what a resized or cleared grid
    /// already reports for itself.
    ///
    /// Every grid is drained, hidden and unplaced ones included: a change
    /// left in one of their trackers would resurface as damage on some
    /// later frame that no longer needs it, exactly as
    /// [`crate::model::Model::take_paint_damage`] drains both its inputs
    /// unconditionally.
    ///
    /// The global grid's own row list is moved out rather than copied into
    /// a fresh one, so a session with no window panes -- every session the
    /// default attach opens -- costs exactly the allocation the drain it
    /// replaced cost, which was none.
    pub(crate) fn take_damage(&mut self) -> GridDamage {
        let mut full = std::mem::take(&mut self.placement_dirty);
        let global = self.global.take_dirty();
        full |= global.full;
        let mut rows = global.rows;
        let from_global = rows.len();
        for slot in &mut self.slots {
            let damage = slot.grid.take_dirty();
            let Some(placed) = slot.placed.as_ref() else {
                continue;
            };
            if placed.hidden || placed.withheld {
                continue;
            }
            full |= damage.full;
            rows.extend(
                damage
                    .rows
                    .iter()
                    .map(|row| row.saturating_add(placed.origin.0)),
            );
        }
        if full {
            return GridDamage::full();
        }
        // two side-by-side panes redraw the same screen row on the same
        // frame, and every consumer of this list scans it linearly
        if rows.len() > from_global {
            rows.sort_unstable();
            rows.dedup();
        }
        GridDamage { full: false, rows }
    }

    /// The cells of one grid, or `None` for an id nvim has never named.
    #[must_use]
    pub fn grid(&self, id: GridId) -> Option<&Grid> {
        if id == GLOBAL_GRID {
            return Some(&self.global);
        }
        self.slots
            .iter()
            .find(|slot| slot.id == id)
            .map(|s| &s.grid)
    }

    /// Every grid nvim has named, global first and the rest in ascending id
    /// order.
    ///
    /// Placement is not a filter here, unlike [`panes_in_z_order`]: a hidden
    /// window's grid still holds the cells nvim last painted into it, and a
    /// caller comparing grid content against a second applier has to see the
    /// same set of grids the wire named.
    ///
    /// [`panes_in_z_order`]: Self::panes_in_z_order
    #[must_use]
    pub fn grid_ids(&self) -> Vec<GridId> {
        let mut ids: Vec<GridId> = self.slots.iter().map(|slot| slot.id).collect();
        ids.sort_unstable();
        ids.insert(0, GLOBAL_GRID);
        ids
    }

    /// Applies one decoded grid or window operation.
    ///
    /// An operation naming a grid that has no size yet records the
    /// placement and waits: nvim may place a window before it sizes the
    /// grid behind it, and dropping the placement would lose a pane that
    /// never reappears.
    pub fn apply(&mut self, op: GridEvent) {
        // deliberately over-approximating: the arms below discard some
        // placement events (one naming the global grid, one naming a grid
        // past the ceiling), and this counts those too. The safe direction
        // -- a spare whole-frame repaint on a desynced stream, against a
        // moved window whose vacated rows nothing ever repaints
        self.placement_dirty |= !matches!(op, GridEvent::Cells { .. });
        match op {
            GridEvent::Cells { grid, op } => self.apply_cells(grid, op),
            // the global grid is never destroyed and never placed, so an
            // event naming it can only be a desynced stream; dropping the
            // screen's own surface on one is not a recoverable state
            GridEvent::Destroy { grid } if grid != GLOBAL_GRID => {
                self.slots.retain(|slot| slot.id != grid);
            }
            GridEvent::Window {
                grid,
                win,
                startrow,
                startcol,
                width,
                height,
            } if grid != GLOBAL_GRID => {
                self.place_window(grid, win, (startrow, startcol, width, height));
            }
            GridEvent::Margins { grid, top } if grid != GLOBAL_GRID => {
                self.set_margin_top(grid, top);
            }
            GridEvent::Float {
                grid,
                anchor_grid,
                screen_row,
                screen_col,
                zindex,
                compindex,
            } if grid != GLOBAL_GRID => {
                // nvim resolves a `relative = "win"` float from the anchor
                // window's slot origin, which it knows; the inner origin is
                // view's own and two cells away from it under gapped tiles,
                // so a hover would otherwise land off the text it belongs to
                let (rows, cols) = self.float_shift(anchor_grid);
                self.place(
                    grid,
                    (
                        screen_row.saturating_add(rows),
                        screen_col.saturating_add(cols),
                    ),
                    PaneKind::Float {
                        zindex,
                        anchor_grid,
                    },
                    compindex,
                );
            }
            // an external window has a UI window of its own and no box on
            // this screen, which is the same thing a hidden one has; its
            // grid stays alive either way
            GridEvent::Hide { grid } | GridEvent::External { grid } => {
                if let Some(slot) = self.slot_mut(grid) {
                    if let Some(placed) = slot.placed.as_mut() {
                        placed.hidden = true;
                    }
                }
            }
            // the grid outlives the window by one event (`win_close` is
            // always followed by `grid_destroy` in the same cycle), so the
            // pane goes now and the cells go with the destroy
            GridEvent::Close { grid } => {
                if let Some(slot) = self.slot_mut(grid) {
                    slot.placed = None;
                }
            }
            // grid 0 is nvim's own "no message grid yet" sentinel
            // (`docs/multigrid-wire-capture.md`'s `msg_set_pos` section),
            // sent once at startup before any message has claimed a real
            // grid, and carries no placement to record
            GridEvent::Message {
                grid,
                row,
                zindex,
                compindex,
            } if grid != GLOBAL_GRID && grid != GridId(0) => {
                self.place(grid, (row, 0), PaneKind::Message { zindex }, compindex);
            }
            GridEvent::Destroy { .. }
            | GridEvent::Window { .. }
            | GridEvent::Margins { .. }
            | GridEvent::Float { .. }
            | GridEvent::Message { .. } => {}
        }
    }

    /// Applies one cell operation to the grid it names.
    ///
    /// The hot path's own entry: `grid_line` is the highest-frequency event
    /// on the wire, and routing it through [`GridEvent`] would build an
    /// enum only to take it apart again on the way to the same
    /// [`Grid::apply`] the single-grid session always made.
    #[inline]
    pub(crate) fn apply_cells(&mut self, grid: GridId, op: GridOp) {
        if let GridOp::CursorGoto { .. } = op {
            self.cursor = Some(grid);
        }
        // view paints a native pane itself, so the engine's cells for the
        // scratch buffer under it are read and dropped. The cursor and the
        // size still apply: focus is read off the cursor's grid, and the
        // pane needs a size to be hit-tested and clipped.
        if matches!(op, GridOp::PutLine { .. }) && self.native_surface(grid).is_some() {
            return;
        }
        if grid == GLOBAL_GRID {
            self.global.apply(op);
        } else if let Some(slot) = self.slot_mut(grid) {
            slot.grid.apply(op);
        }
    }

    /// Every visible pane, back to front, ready to paint.
    ///
    /// The global grid is always the first: under `ext_multigrid` it holds
    /// the chrome between windows, and under single-grid it holds the whole
    /// picture, so it is the layer every window pane paints over.
    #[must_use]
    pub fn panes_in_z_order(&self) -> Vec<Pane> {
        let (global_width, global_height) = self.global.size();
        let mut panes = vec![Pane {
            id: GLOBAL_GRID,
            origin: (0, 0),
            slot: (0, 0, global_width, global_height),
            kind: PaneKind::Window,
            hidden: false,
        }];
        let mut placed: Vec<(&Slot, &Placement)> = self
            .slots
            .iter()
            .filter_map(|slot| slot.placed.as_ref().map(|p| (slot, p)))
            .filter(|(_, p)| !p.hidden && !p.withheld)
            .collect();
        // the id is the last key so the order is total: nvim reuses no id
        // after a destroy, so two panes never tie on all four
        placed.sort_by_key(|(slot, p)| (p.layer(), p.zindex(), p.compindex, slot.id));
        panes.extend(placed.into_iter().map(|(slot, p)| Pane {
            id: slot.id,
            origin: p.origin,
            slot: slot.window.as_ref().map_or_else(
                || {
                    let (width, height) = slot.grid.size();
                    (p.origin.0, p.origin.1, width, height)
                },
                |window| window.slot,
            ),
            kind: p.kind.clone(),
            hidden: p.hidden,
        }));
        panes
    }

    /// Whether nvim is prompting out of its own message area: the cursor is
    /// parked there, one cell past the text it just drew.
    ///
    /// A message area exists only where `ext_messages` is not attached, and
    /// nvim announces it (`msg_set_pos`) at every such startup whether or
    /// not it has drawn into it -- so the placement alone says nothing and
    /// the cells are what answers. Text alone is not enough either: with
    /// `laststatus` at 0 the ruler lives in that same area, and a startup
    /// that forces its own redraws puts it there at every flush. What tells
    /// a prompt from anything else is where the cursor sits relative to the
    /// text: nvim parks a prompt's cursor on the blank cell after the
    /// prompt, and leaves the buffer's at the top-left corner while sourcing
    /// -- on the first character, or on a leading blank with nothing to its
    /// left. A `vim.fn.input()` on a session that externalized neither the
    /// cmdline nor the messages arrives this way and no other.
    ///
    /// The whole reading is one row: the cell under the cursor is blank,
    /// text lies to its left, and nothing non-blank lies to its right --
    /// every prompt ends at its cursor. The wire cursor can move while a
    /// config sources (`nvim__redraw({cursor = true})`, which plugins call
    /// from their setup), so a buffer cursor on a blank cell inside a line
    /// is a screen this has to hold, and the cells after it are what say
    /// so. A prompt exactly as wide as the grid wraps its cursor to column
    /// 0 of the next row, which is the same screen as a full-width buffer
    /// line with the cursor on the blank row below it; that prompt is left
    /// to the hold's cap rather than read as a release. And a `getchar()`
    /// wait after a message draws the text into the message grid but
    /// reports the cursor on the global grid at the message area's row, so
    /// a global-grid cursor whose row falls inside a placed `Message` pane
    /// reads as that pane's row.
    ///
    /// Without `ext_multigrid` there is no message grid to place. nvim
    /// composites its message area into the bottom of the global grid and
    /// names neither that area nor `cmdheight` on the wire (`option_set`
    /// carries no such option), so the same reading is taken off the global
    /// grid's cursor: no row of that grid is the message area, but the
    /// cursor sitting just past text is a prompt wherever it is, and a
    /// buffer -- the last row reached with `cmdheight` 0, blank end-of-
    /// buffer rows under a one-line file, a first line that opens with
    /// whitespace -- never puts it there.
    ///
    /// Read once per withheld flush; costs the cells of the cursor's row.
    #[must_use]
    pub fn message_area_has_text(&self) -> bool {
        let Some((grid, row, col)) = self.message_area_cursor() else {
            return false;
        };
        let blank = |c: u16| {
            grid.cell(row, c)
                .is_none_or(|cell| cell.text.trim().is_empty())
        };
        blank(col)
            && (0..col).any(|c| !blank(c))
            && (col.saturating_add(1)..grid.size().0).all(blank)
    }

    /// The grid the cursor's message-area reading is taken from, with the
    /// cursor local to it, or `None` when the cursor is nowhere near a
    /// message area.
    fn message_area_cursor(&self) -> Option<(&Grid, u16, u16)> {
        if self.slots.is_empty() {
            let (row, col) = self.global.cursor();
            return Some((&self.global, row, col));
        }
        let id = self.cursor?;
        let mut message_panes = self.slots.iter().filter_map(|slot| {
            slot.placed
                .as_ref()
                .filter(|p| matches!(p.kind, PaneKind::Message { .. }))
                .map(|p| (slot, p))
        });
        if id == GLOBAL_GRID {
            let (row, col) = self.global.cursor();
            return message_panes.find_map(|(slot, placed)| {
                let local = row.checked_sub(placed.origin.0)?;
                (local < slot.grid.size().1).then_some((&slot.grid, local, col))
            });
        }
        let (slot, _) = message_panes.find(|(slot, _)| slot.id == id)?;
        let (row, col) = slot.grid.cursor();
        Some((&slot.grid, row, col))
    }

    /// The grid the global screen coordinates fall inside, topmost pane
    /// first.
    ///
    /// Takes screen `(col, row)` and answers `(grid, col, row)` inside that
    /// grid -- column first on both sides, which is the order a mouse event
    /// carries and the opposite of [`Pane::origin`]'s `(row, col)`, the
    /// order nvim announces a *placement* in.
    ///
    /// The global grid answers only for a session that has no window panes
    /// at all: under `ext_multigrid` its cells are the chrome *between*
    /// windows, and a click on a separator is view's own to interpret rather
    /// than the engine's to receive.
    ///
    /// That answer is deliberately unbounded by the global grid's own size,
    /// unlike every pane's. Bounding it would make a click depend on
    /// geometry that arrives on the wire: between a terminal resize and the
    /// `grid_resize` nvim answers it with, a click in the newly exposed
    /// region would be swallowed here rather than clamped by nvim, which is
    /// what happens to it today.
    #[must_use]
    pub fn hit_test(&self, col: u16, row: u16) -> Option<(GridId, u16, u16)> {
        for pane in self.panes_in_z_order().into_iter().rev() {
            if pane.id == GLOBAL_GRID {
                continue;
            }
            if let Some(hit) = self.hit_pane(&pane, col, row) {
                return Some(hit);
            }
        }
        (!self.has_panes()).then_some((GLOBAL_GRID, col, row))
    }

    /// Where screen `(col, row)` sits inside `grid`, clamped to that grid's
    /// own box -- column first on both sides, as [`hit_test`] is.
    ///
    /// What a gesture already claimed by a grid reports while the pointer
    /// is outside it. A drag that leaves the window it started in is still
    /// that window's drag, and nvim extends the selection to the position
    /// it is handed, so the nearest cell inside the grid is the answer that
    /// keeps a selection tracking the pointer instead of stopping at the
    /// window edge.
    ///
    /// The global grid is the screen and is never clamped, for the same
    /// reason [`hit_test`] does not bound its answer.
    ///
    /// [`hit_test`]: Self::hit_test
    #[must_use]
    pub fn clamp_into(&self, grid: GridId, col: u16, row: u16) -> Option<(u16, u16)> {
        if grid == GLOBAL_GRID {
            return Some((col, row));
        }
        let (width, height) = self.grid(grid)?.size();
        let (top, left) = self
            .slots
            .iter()
            .find(|slot| slot.id == grid)
            .and_then(|slot| slot.placed.as_ref())
            .map(|placed| placed.origin)?;
        Some((
            col.saturating_sub(left).min(width.saturating_sub(1)),
            row.saturating_sub(top).min(height.saturating_sub(1)),
        ))
    }

    /// Whether nvim has placed a window of its own anywhere.
    ///
    /// False for every single-grid session and for a multigrid one before
    /// its first `win_pos` lands, which is the case a compositor answers
    /// without building the pane list at all: the global grid is the whole
    /// picture and there is no space between windows to draw in.
    #[must_use]
    pub fn has_panes(&self) -> bool {
        self.slots.iter().any(|slot| slot.placed.is_some())
    }

    /// Whether a window is showing buffer text yet.
    ///
    /// The frame a person calls the start, as distinct from the chrome view
    /// paints around it: a session reaches its first flush with the tabline
    /// and the statusline drawn over a window grid nvim has sized and not
    /// yet drawn into, and that screen carries no file. Only a window
    /// answers -- a float, a message area and an unplaced grid are all
    /// something other than the file the user opened.
    ///
    /// A session with no grid of its own is a single-grid one, where nvim
    /// composites the whole picture into the global grid and there is no
    /// window grid to ask; the global grid answers for it.
    ///
    /// Visible means both flags: a withheld pane paints no cell either, and
    /// [`place`](Self::place) carries `withheld` across a re-place that
    /// changes the kind, so a withheld float nvim re-places as a window
    /// would otherwise answer yes while showing nothing.
    ///
    /// Costs the cells of every window grid while the answer is still
    /// `false`, so a caller reads it until it turns true and never again.
    #[must_use]
    pub fn window_text_painted(&self) -> bool {
        if self.slots.is_empty() {
            return self.global.has_text();
        }
        self.slots.iter().any(|slot| {
            slot.placed.as_ref().is_some_and(|placed| {
                matches!(placed.kind, PaneKind::Window) && !placed.hidden && !placed.withheld
            }) && slot.grid.has_text()
        })
    }

    /// The grid nvim last placed the cursor in.
    #[must_use]
    pub fn cursor_grid(&self) -> Option<GridId> {
        self.cursor
    }

    /// Where `grid` currently sits on screen: `(0, 0)` for the global grid,
    /// the placed origin for a visible pane, `None` for a grid nvim has
    /// never placed or has since hidden -- the same visibility
    /// [`panes_in_z_order`](Self::panes_in_z_order) filters to.
    #[must_use]
    pub fn pane_origin(&self, grid: GridId) -> Option<(u16, u16)> {
        if grid == GLOBAL_GRID {
            return Some((0, 0));
        }
        self.slots
            .iter()
            .find(|slot| slot.id == grid)
            .and_then(|slot| slot.placed.as_ref())
            .filter(|placed| !placed.hidden)
            .map(|placed| placed.origin)
    }

    /// The grid the cursor is in, and its position local to that grid.
    ///
    /// Falls back to the global grid when no visible pane currently owns
    /// the cursor: every single-grid session, a multigrid one before its
    /// first window claims the cursor, and one where the window that had it
    /// has since been hidden.
    #[must_use]
    pub fn cursor_local(&self) -> (GridId, u16, u16) {
        if let Some(id) = self.cursor.filter(|&id| id != GLOBAL_GRID) {
            if self.pane_origin(id).is_some() {
                if let Some(grid) = self.grid(id) {
                    let (row, col) = grid.cursor();
                    return (id, row, col);
                }
            }
        }
        let (row, col) = self.global.cursor();
        (GLOBAL_GRID, row, col)
    }

    /// The cursor's screen position: the owning pane's origin plus its
    /// position inside that pane's own grid, so a caller never has to
    /// special-case which grid currently owns the cursor.
    #[must_use]
    pub fn cursor_pos(&self) -> (u16, u16) {
        let (id, row, col) = self.cursor_local();
        let (orow, ocol) = self.pane_origin(id).unwrap_or((0, 0));
        (row.saturating_add(orow), col.saturating_add(ocol))
    }

    /// Drops every grid but the global one, and every placement with them,
    /// for a connection being replaced.
    ///
    /// Grid ids are per-connection allocations, so a replacement's are
    /// somebody else's numbers -- both halves: a pane left behind names a
    /// window that no longer exists and nothing will ever destroy it, and
    /// the cells behind it are a dead session's. The global grid's own
    /// cells stay, so the last frame survives the restart the way it always
    /// has.
    pub(crate) fn forget_grids(&mut self) {
        self.slots.clear();
        self.cursor = None;
        // every box those grids held is gone from the screen at once, and
        // no cell op will ever name the rows they occupied
        self.placement_dirty = true;
    }

    /// Where `(col, row)` lands inside `pane`, if it lands inside it at all.
    fn hit_pane(&self, pane: &Pane, col: u16, row: u16) -> Option<(GridId, u16, u16)> {
        let (width, height) = self.grid(pane.id)?.size();
        let (top, left) = pane.origin;
        let (in_row, in_col) = (row.checked_sub(top)?, col.checked_sub(left)?);
        (in_col < width && in_row < height).then_some((pane.id, in_col, in_row))
    }

    /// Records a placement over `grid`, creating the grid if nvim has named
    /// it here first, and un-hiding it: a hidden window comes back through a
    /// bare `win_pos` with no paired "show" event of its own.
    fn place(&mut self, grid: GridId, origin: (u16, u16), kind: PaneKind, compindex: u32) {
        if let Some(slot) = self.slot_mut(grid) {
            let withheld = slot.placed.as_ref().is_some_and(|placed| placed.withheld);
            slot.placed = Some(Placement {
                origin,
                kind,
                hidden: false,
                withheld,
                compindex,
            });
        }
    }

    /// Records the slot nvim gave `grid`'s window and places the grid at
    /// the origin the current look puts inside it.
    fn place_window(&mut self, grid: GridId, win: WinHandle, slot: (u16, u16, u16, u16)) {
        let look = self.look;
        if let Some(entry) = self.slot_mut(grid) {
            let margin_top = entry.window.as_ref().map_or(0, |window| window.margin_top);
            let requested = entry
                .window
                .as_ref()
                .filter(|window| window.slot == slot)
                .and_then(|window| window.requested);
            entry.window = Some(Window {
                win,
                slot,
                margin_top,
                requested,
            });
        }
        let origin = inner_origin(look, slot, self.margin_top(grid));
        let kind = self.window_kind(win);
        // a grid_line that beat the claim here left engine cells standing
        // under a pane view paints itself
        if matches!(kind, PaneKind::Native { .. }) && self.native_surface(grid).is_none() {
            if let Some(entry) = self.slot_mut(grid) {
                entry.grid.apply(GridOp::Clear);
            }
        }
        self.place(grid, origin, kind, 0);
    }

    /// The kind a `win_pos` for `win` places: a native pane where view
    /// claimed the handle, an ordinary window otherwise.
    fn window_kind(&self, win: WinHandle) -> PaneKind {
        self.claims
            .iter()
            .find(|(handle, _)| *handle == win)
            .map_or(PaneKind::Window, |(_, surface)| PaneKind::Native {
                surface: *surface,
            })
    }

    /// Binds the window handle `nvim_open_win` answered with to the surface
    /// view opened it for, so the next `win_pos` for it places a `Native`
    /// pane rather than a `Window` one.
    pub fn claim_native_window(&mut self, win: WinHandle, surface: NativeSurface) {
        if let Some(entry) = self.claims.iter_mut().find(|(handle, _)| *handle == win) {
            entry.1 = surface;
            return;
        }
        self.claims.push((win, surface));
    }

    /// Forgets the claim on `win`, for a window that has been closed.
    pub fn release_native_window(&mut self, win: WinHandle) {
        self.claims.retain(|(handle, _)| *handle != win);
    }

    /// How many window handles view holds a surface claim on.
    ///
    /// Test-only: `native_window` answers from the placed pane and so reads
    /// the same either way, which leaves a claim never released invisible
    /// to every other question the registry can be asked.
    #[cfg(test)]
    pub(crate) fn native_claims(&self) -> usize {
        self.claims.len()
    }

    /// The surface of the `PaneKind::Native` pane `cursor_grid()` names, or
    /// `None` when the cursor sits in an ordinary window or grid 1.
    #[must_use]
    pub fn native_pane_focus(&self) -> Option<NativeSurface> {
        self.native_surface(self.cursor_grid()?)
    }

    /// The window view opened for `surface`, as nvim addresses it, or
    /// `None` while no pane of that surface is placed.
    #[must_use]
    pub fn native_window(&self, surface: NativeSurface) -> Option<WinHandle> {
        self.slots
            .iter()
            .find(|slot| {
                slot.placed
                    .as_ref()
                    .is_some_and(|placed| placed.kind.native_surface() == Some(surface))
            })
            .and_then(|slot| slot.window.as_ref())
            .map(|window| window.win)
    }

    /// The surface `grid`'s pane was placed for, if it is a native one.
    #[must_use]
    pub(crate) fn native_surface(&self, grid: GridId) -> Option<NativeSurface> {
        self.slots
            .iter()
            .find(|slot| slot.id == grid)
            .and_then(|slot| slot.placed.as_ref())
            .and_then(|placed| match placed.kind {
                PaneKind::Native { surface } => Some(surface),
                _ => None,
            })
    }

    /// Records `grid`'s top margin, re-placing its window when the margin
    /// moves the inner origin or what the grid owes.
    ///
    /// A grid nvim has neither sized nor placed is not a grid: nvim sends
    /// margins for a window's grid before the split that would size it is
    /// settled, and the grid it settles on can be a different one, which it
    /// never destroys the abandoned one for. Creating a slot here left that
    /// one standing for the life of the session.
    fn set_margin_top(&mut self, grid: GridId, top: u16) {
        let look = self.look;
        let Some(entry) = self.slots.iter_mut().find(|slot| slot.id == grid) else {
            return;
        };
        let Some(window) = entry.window.as_mut() else {
            return;
        };
        if window.margin_top == top {
            return;
        }
        window.margin_top = top;
        let slot = window.slot;
        let win = window.win;
        let origin = inner_origin(look, slot, top);
        let kind = self.window_kind(win);
        self.place(grid, origin, kind, 0);
    }

    /// `grid`'s top margin, 0 for a grid with no window or no winbar.
    fn margin_top(&self, grid: GridId) -> u16 {
        self.slots
            .iter()
            .find(|slot| slot.id == grid)
            .and_then(|slot| slot.window.as_ref())
            .map_or(0, |window| window.margin_top)
    }

    /// How much a float anchored to `anchor` has to move to sit over the
    /// text it belongs to, as `(rows, cols)`.
    fn float_shift(&self, anchor: GridId) -> (u16, u16) {
        if anchor == GLOBAL_GRID {
            return (0, 0);
        }
        let Some(window) = self
            .slots
            .iter()
            .find(|slot| slot.id == anchor)
            .and_then(|slot| slot.window.as_ref())
        else {
            return (0, 0);
        };
        let (row, col, width, height) = window.slot;
        let origin = inner_origin(self.look, (row, col, width, height), window.margin_top);
        (origin.0.saturating_sub(row), origin.1.saturating_sub(col))
    }

    /// How window layout is drawn, and whether this call changed it.
    ///
    /// A look change moves every window grid's origin and what every one of
    /// them owes nvim, so the whole frame repaints and every standing
    /// request is stale at once.
    pub fn set_look(&mut self, look: Look) -> bool {
        if self.look == look {
            return false;
        }
        self.look = look;
        for index in 0..self.slots.len() {
            let Some(window) = self.slots.get(index).and_then(|slot| slot.window.as_ref()) else {
                continue;
            };
            let (slot, margin_top) = (window.slot, window.margin_top);
            let origin = inner_origin(look, slot, margin_top);
            if let Some(entry) = self.slots.get_mut(index) {
                if let Some(placed) = entry.placed.as_mut() {
                    placed.origin = origin;
                }
            }
        }
        self.placement_dirty = true;
        true
    }

    /// How window layout is currently drawn.
    #[must_use]
    pub fn look(&self) -> Look {
        self.look
    }

    /// The window nvim last placed on `grid`, as it addresses it.
    #[must_use]
    pub fn window_handle(&self, grid: GridId) -> Option<WinHandle> {
        self.slots
            .iter()
            .find(|slot| slot.id == grid)
            .and_then(|slot| slot.window.as_ref())
            .map(|window| window.win)
    }

    /// The slot nvim placed `win`'s window in, as
    /// `(row, col, width, height)`.
    #[must_use]
    pub fn window_slot(&self, win: WinHandle) -> Option<(u16, u16, u16, u16)> {
        self.slots
            .iter()
            .filter(|slot| slot.placed.as_ref().is_some_and(|p| !p.hidden))
            .find_map(|slot| {
                slot.window
                    .as_ref()
                    .filter(|window| window.win == win)
                    .map(|window| window.slot)
            })
    }

    /// The grid rows every placed window's frame edges stand on under
    /// `look`, which are the rows a tile's own status segments are painted
    /// in.
    #[must_use]
    pub fn window_edge_rows(&self, look: crate::model::Look) -> Vec<u16> {
        let mut rows: Vec<u16> = self
            .slots
            .iter()
            .filter(|slot| slot.placed.as_ref().is_some_and(|p| !p.hidden))
            .filter_map(|slot| slot.window.as_ref())
            .flat_map(|window| look.edge_rows(window.slot))
            .collect();
        // a gapless tile answers its one lattice row twice, and two tiles
        // stacked in a column share the row between them
        rows.sort_unstable();
        rows.dedup();
        rows
    }

    /// Marks a row of the global grid changed, for chrome view paints over
    /// that grid itself.
    ///
    /// A tile's frame edge stands on one of those rows, and what it says
    /// comes from view's own bridge rather than from a redraw event, so
    /// nothing else in the frame's damage covers it.
    pub fn mark_global_row(&mut self, row: u16) {
        self.global.mark_row(row);
    }

    /// Every grid holding an ordinary window, in ascending id order.
    #[must_use]
    pub fn window_grids(&self) -> Vec<GridId> {
        let mut ids: Vec<GridId> = self
            .slots
            .iter()
            .filter(|slot| slot.window.is_some())
            .map(|slot| slot.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// The inner request `grid` owes, or `None` when nothing has changed
    /// since the last request this registry answered for it under this
    /// `look` and this top margin.
    ///
    /// Keyed on `(slot, look, margin_top)` rather than the slot alone: a
    /// gaps flip changes what every window owes while leaving slots whose
    /// neighbours absorb the ring change exactly where they were, and a
    /// slot-only key would drop the re-send that flip exists to make.
    ///
    /// Every `nvim_ui_try_resize_grid` costs a full redraw of the window it
    /// names, so the guard is what keeps an attach from relaying out a
    /// screen nvim has already drawn.
    #[must_use]
    pub fn pending_inner_request(&mut self, grid: GridId, look: Look) -> Option<(u16, u16)> {
        let entry = self.slots.iter_mut().find(|slot| slot.id == grid)?;
        let window = entry.window.as_mut()?;
        let key = (window.slot, look, window.margin_top);
        if window.requested == Some(key) {
            return None;
        }
        window.requested = Some(key);
        let (_, _, width, height) = window.slot;
        Some(look.inner_request((width, height), window.margin_top))
    }

    /// Whether view is holding `grid`'s float off the screen.
    ///
    /// Distinct from "absent from [`panes_in_z_order`]", which a hidden or
    /// unplaced grid answers the same way: a reader working out which pane
    /// owned a cell has to tell view's own hold apart from nvim's.
    ///
    /// [`panes_in_z_order`]: Self::panes_in_z_order
    #[must_use]
    pub fn float_withheld(&self, grid: GridId) -> bool {
        self.slots
            .iter()
            .find(|slot| slot.id == grid)
            .and_then(|slot| slot.placed.as_ref())
            .is_some_and(|placed| placed.withheld)
    }

    /// Holds `grid`'s float off the screen, or gives it back, and answers
    /// whether this call changed anything.
    ///
    /// A withheld pane paints no cell and contributes no damage rows, the
    /// same two exclusions a hidden one gets -- so the cells underneath it
    /// belong to whatever was there, and the frame that lifts or applies
    /// the flag repaints them (`placement_dirty`, since the box appearing
    /// or vanishing names no rows of its own).
    pub fn withhold_float(&mut self, grid: GridId, withheld: bool) -> bool {
        let Some(slot) = self.slot_mut(grid) else {
            return false;
        };
        let Some(placed) = slot.placed.as_mut() else {
            return false;
        };
        if placed.withheld == withheld {
            return false;
        }
        placed.withheld = withheld;
        self.placement_dirty = true;
        true
    }

    /// The slot for `id`, created empty if nvim has not named it before.
    fn slot_mut(&mut self, id: GridId) -> Option<&mut Slot> {
        if let Some(index) = self.slots.iter().position(|slot| slot.id == id) {
            return self.slots.get_mut(index);
        }
        if self.slots.len() >= MAX_GRIDS {
            return None;
        }
        self.slots.push(Slot {
            id,
            grid: Grid::new(),
            placed: None,
            window: None,
        });
        self.slots.last_mut()
    }
}

/// Where a window grid's cell `(0, 0)` sits inside the slot nvim gave it.
///
/// A slot too small to frame is drawn bare, so its grid fills the slot and
/// sits at its origin, which is also what gapless tiles and `"nvim"` mode
/// answer.
fn inner_origin(look: Look, slot: (u16, u16, u16, u16), margin_top: u16) -> (u16, u16) {
    let (row, col, width, height) = slot;
    if look.inner_request((width, height), margin_top) == (0, 0) {
        return (row, col);
    }
    let (rows, cols) = look.inset();
    (row.saturating_add(rows), col.saturating_add(cols))
}

impl Default for GridRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::model::{Panes, MIN_FRAMED_SLOT};

    /// Places `grid`'s window over the whole slot its grid already fills,
    /// which is the shape every `win_pos` takes while no inner size has
    /// been asked for.
    fn window(registry: &mut GridRegistry, grid: GridId, startrow: u16, startcol: u16) {
        let (width, height) = registry.grid(grid).map_or((0, 0), Grid::size);
        registry.apply(GridEvent::Window {
            grid,
            win: WinHandle(grid.0),
            startrow,
            startcol,
            width,
            height,
        });
    }

    #[test]
    fn a_placement_before_its_resize_is_retained() {
        let mut registry = GridRegistry::new();
        window(&mut registry, GridId(2), 0, 41);
        registry.apply(GridEvent::Cells {
            grid: GridId(2),
            op: GridOp::Resize {
                width: 39,
                height: 23,
            },
        });
        let panes = registry.panes_in_z_order();
        assert!(
            panes
                .iter()
                .any(|p| p.id == GridId(2) && p.origin == (0, 41)),
            "the placement that arrived before the resize was dropped: {panes:?}"
        );
        assert_eq!(
            registry.grid(GridId(2)).map(Grid::size),
            Some((39, 23)),
            "the resize that followed the placement did not reach the grid"
        );
    }

    /// Both shapes the capture records: a closed window sends `win_close`
    /// then `grid_destroy`, while a closed tab page sends `grid_destroy`
    /// alone, with the placement still standing when it arrives.
    #[test]
    fn grid_destroy_removes_the_pane_and_its_placement() {
        for closed in [false, true] {
            let mut registry = GridRegistry::new();
            resize(&mut registry, GridId(4), 40, 23);
            window(&mut registry, GridId(4), 0, 0);
            if closed {
                registry.apply(GridEvent::Close { grid: GridId(4) });
            }
            registry.apply(GridEvent::Destroy { grid: GridId(4) });
            assert_eq!(
                ids(&registry),
                vec![GLOBAL_GRID],
                "a destroyed grid still has a pane (win_close first: {closed})"
            );
            assert!(registry.grid(GridId(4)).is_none());
        }
    }

    #[test]
    fn hidden_grids_are_not_painted_but_are_not_forgotten() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GridId(5), 29, 19);
        window(&mut registry, GridId(5), 0, 41);
        registry.apply(GridEvent::Hide { grid: GridId(5) });
        assert_eq!(ids(&registry), vec![GLOBAL_GRID]);
        assert_eq!(
            registry.grid(GridId(5)).map(Grid::size),
            Some((29, 19)),
            "a hidden grid's cells were dropped, and nvim resends none of them"
        );
        // a hidden window comes back through a bare `win_pos`; the wire
        // carries no paired "show"
        window(&mut registry, GridId(5), 0, 41);
        assert_eq!(ids(&registry), vec![GLOBAL_GRID, GridId(5)]);
    }

    #[test]
    fn floats_sort_above_windows_by_zindex() {
        let mut registry = GridRegistry::new();
        for id in [GridId(2), GridId(6)] {
            resize(&mut registry, id, 10, 5);
            window(&mut registry, id, 0, 0);
        }
        for (id, zindex) in [(GridId(9), 200), (GridId(7), 50)] {
            resize(&mut registry, id, 4, 2);
            registry.apply(GridEvent::Float {
                grid: id,
                anchor_grid: GLOBAL_GRID,
                screen_row: 1,
                screen_col: 1,
                zindex,
                compindex: 1,
            });
        }
        assert_eq!(
            ids(&registry),
            vec![GLOBAL_GRID, GridId(2), GridId(6), GridId(7), GridId(9)],
            "floats must paint after every window, lowest zindex first"
        );
    }

    #[test]
    fn a_message_grid_becomes_a_pane_but_grid_zero_names_none() {
        let mut registry = GridRegistry::new();
        // nvim's own startup sentinel: no message grid exists yet
        registry.apply(GridEvent::Message {
            grid: GridId(0),
            row: 23,
            zindex: 0,
            compindex: 0,
        });
        assert_eq!(
            ids(&registry),
            vec![GLOBAL_GRID],
            "grid 0 must record no placement and allocate no slot"
        );

        // the global grid is the other id the guard refuses: nvim never
        // positions its message area onto the canvas view already paints
        // whole, and a placement recorded over grid 1 would make the whole
        // screen a pane inside itself
        registry.apply(GridEvent::Message {
            grid: GLOBAL_GRID,
            row: 23,
            zindex: 200,
            compindex: 0,
        });
        assert_eq!(
            ids(&registry),
            vec![GLOBAL_GRID],
            "the global grid must record no placement of its own"
        );
        assert!(
            !registry.has_panes(),
            "neither sentinel may turn a single-grid session into a composited one"
        );

        resize(&mut registry, GridId(3), 80, 1);
        registry.apply(GridEvent::Message {
            grid: GridId(3),
            row: 23,
            zindex: 200,
            compindex: 0,
        });
        let panes = registry.panes_in_z_order();
        let message = panes
            .iter()
            .find(|pane| pane.id == GridId(3))
            .expect("the message grid must become a paintable pane");
        assert_eq!(message.origin, (23, 0));
        assert_eq!(message.kind, PaneKind::Message { zindex: 200 });
        assert!(
            panes.iter().position(|p| p.id == GridId(3)).unwrap()
                > panes.iter().position(|p| p.id == GLOBAL_GRID).unwrap(),
            "the message area paints after the global grid, same as a float"
        );
    }

    #[test]
    fn an_op_naming_an_unknown_grid_is_recorded_not_dropped() {
        let mut registry = GridRegistry::new();
        registry.apply(GridEvent::Cells {
            grid: GridId(8),
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("x".into(), 0, 1)],
            },
        });
        assert!(
            registry.grid(GridId(8)).is_some(),
            "a grid nvim named only by writing to it was dropped"
        );
        registry.apply(GridEvent::Hide { grid: GridId(11) });
        assert!(registry.grid(GridId(11)).is_some());
    }

    #[test]
    fn single_grid_sessions_produce_exactly_one_pane() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        registry.apply(GridEvent::Cells {
            grid: GLOBAL_GRID,
            op: GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("h".into(), 0, 1), ("i".into(), 0, 1)],
            },
        });
        registry.apply(GridEvent::Cells {
            grid: GLOBAL_GRID,
            op: GridOp::CursorGoto { row: 0, col: 2 },
        });
        assert_eq!(ids(&registry), vec![GLOBAL_GRID]);
        assert_eq!(registry.cursor_grid(), Some(GLOBAL_GRID));
        assert_eq!(registry.global().row_text(0).trim_end(), "hi");
        // the identity translation the mouse path relies on while nvim has
        // placed no window of its own
        assert_eq!(registry.hit_test(3, 1), Some((GLOBAL_GRID, 3, 1)));
        // and unbounded: a click that arrives before the grid_resize
        // answering a terminal that just grew is nvim's to clamp
        assert_eq!(registry.hit_test(80, 0), Some((GLOBAL_GRID, 80, 0)));
        assert_eq!(registry.clamp_into(GLOBAL_GRID, 80, 0), Some((80, 0)));
    }

    /// The cursor's screen position under multigrid: pane origin plus its
    /// position inside that pane's own grid, not the global grid's own
    /// (stale, unset) cursor field -- the bug a session with any window
    /// placed away from the screen's own origin would otherwise show.
    #[test]
    fn a_windows_cursor_resolves_through_its_own_pane_origin() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        resize(&mut registry, GridId(4), 39, 23);
        window(&mut registry, GridId(4), 0, 41);
        registry.apply(GridEvent::Cells {
            grid: GridId(4),
            op: GridOp::CursorGoto { row: 2, col: 5 },
        });
        assert_eq!(registry.cursor_grid(), Some(GridId(4)));
        assert_eq!(registry.pane_origin(GridId(4)), Some((0, 41)));
        assert_eq!(registry.cursor_local(), (GridId(4), 2, 5));
        assert_eq!(
            registry.cursor_pos(),
            (2, 46),
            "the pane's own column (41) plus its local cursor column (5)"
        );
    }

    /// A window that goes away without a `grid_cursor_goto` naming a new
    /// one leaves `cursor_grid` pointing at a pane that no longer answers
    /// `pane_origin`, and the fallback is the global grid rather than a
    /// stale screen position nobody would repaint.
    #[test]
    fn a_hidden_cursor_pane_falls_back_to_the_global_grid() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        resize(&mut registry, GridId(4), 39, 23);
        window(&mut registry, GridId(4), 0, 41);
        registry.apply(GridEvent::Cells {
            grid: GridId(4),
            op: GridOp::CursorGoto { row: 2, col: 5 },
        });
        registry.apply(GridEvent::Hide { grid: GridId(4) });
        assert_eq!(registry.pane_origin(GridId(4)), None);
        assert_eq!(registry.cursor_local(), (GLOBAL_GRID, 0, 0));
        assert_eq!(registry.cursor_pos(), (0, 0));
    }

    /// Before any `grid_cursor_goto` has named a window grid -- the first
    /// frame of a multigrid session, and every single-grid one -- the
    /// cursor is the global grid's own, unmodified by any pane origin.
    #[test]
    fn no_pane_cursor_yet_reads_the_global_grid_directly() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        registry.apply(GridEvent::Cells {
            grid: GLOBAL_GRID,
            op: GridOp::CursorGoto { row: 3, col: 7 },
        });
        assert_eq!(registry.cursor_local(), (GLOBAL_GRID, 3, 7));
        assert_eq!(registry.cursor_pos(), (3, 7));
    }

    #[test]
    fn a_point_outside_a_pane_clamps_to_that_panes_nearest_cell() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        resize(&mut registry, GridId(2), 39, 23);
        window(&mut registry, GridId(2), 0, 41);
        // inside the pane, clamping is the translation hit_test already made
        assert_eq!(registry.clamp_into(GridId(2), 41, 3), Some((0, 3)));
        // left of it, and below it
        assert_eq!(registry.clamp_into(GridId(2), 12, 3), Some((0, 3)));
        assert_eq!(registry.clamp_into(GridId(2), 79, 23), Some((38, 22)));
        assert_eq!(registry.clamp_into(GridId(9), 0, 0), None);
    }

    #[test]
    fn hit_test_names_the_topmost_pane_in_its_own_coordinates() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        resize(&mut registry, GridId(4), 40, 23);
        window(&mut registry, GridId(4), 0, 0);
        resize(&mut registry, GridId(2), 39, 23);
        window(&mut registry, GridId(2), 0, 41);
        resize(&mut registry, GridId(7), 22, 5);
        registry.apply(GridEvent::Float {
            grid: GridId(7),
            anchor_grid: GLOBAL_GRID,
            screen_row: 2,
            screen_col: 4,
            zindex: 50,
            compindex: 1,
        });
        assert_eq!(registry.hit_test(41, 3), Some((GridId(2), 0, 3)));
        assert_eq!(registry.hit_test(5, 3), Some((GridId(7), 1, 1)));
        // column 40 is the separator: it is the global grid's cell, and no
        // window's
        assert_eq!(registry.hit_test(40, 10), None);
    }

    #[test]
    fn the_grid_ceiling_refuses_the_next_id_and_keeps_every_earlier_one() {
        let mut registry = GridRegistry::new();
        let last = u64::try_from(MAX_GRIDS).unwrap_or(u64::MAX) + 1;
        // ids are sparse and never reused, so a desynced stream naming a
        // fresh one forever is the shape the ceiling exists for
        for id in 2..=last {
            resize(&mut registry, GridId(id), 4, 2);
        }
        assert!(registry.grid(GridId(2)).is_some());
        assert_eq!(
            registry.grid(GridId(last)).map(Grid::size),
            Some((4, 2)),
            "the last grid inside the ceiling was refused"
        );
        resize(&mut registry, GridId(9999), 4, 2);
        assert!(
            registry.grid(GridId(9999)).is_none(),
            "the ceiling admitted a grid past MAX_GRIDS"
        );
    }

    #[test]
    fn every_named_grid_is_listed_whether_or_not_it_is_on_screen() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GridId(4), 40, 11);
        resize(&mut registry, GridId(2), 39, 23);
        registry.apply(GridEvent::Hide { grid: GridId(4) });

        assert_eq!(
            registry.grid_ids(),
            vec![GLOBAL_GRID, GridId(2), GridId(4)],
            "a hidden grid still holds cells and must stay listed"
        );

        registry.apply(GridEvent::Destroy { grid: GridId(4) });
        assert_eq!(
            registry.grid_ids(),
            vec![GLOBAL_GRID, GridId(2)],
            "a destroyed grid is gone from the listing"
        );
    }

    /// The three answers the drain owes a compositor: a grid with no box on
    /// screen contributes nothing (and does not hoard its rows for a later
    /// frame that no longer needs them), a placement change repaints the
    /// frame it rearranged, and a placed grid's rows arrive where its box
    /// actually sits.
    #[test]
    fn damage_is_reported_per_pane_in_screen_rows() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        resize(&mut registry, GridId(5), 40, 5);
        let _ = registry.take_damage();

        put(&mut registry, GridId(5), 1);
        let unplaced = registry.take_damage();
        assert!(
            !unplaced.full && unplaced.rows.is_empty(),
            "a grid with no box on screen damaged the frame: {unplaced:?}"
        );

        window(&mut registry, GridId(5), 12, 0);
        assert!(
            registry.take_damage().full,
            "a window appeared and the rows it covered were never repainted"
        );

        put(&mut registry, GridId(5), 1);
        assert_eq!(
            registry.take_damage().rows,
            vec![13],
            "a pane's row reached the frame in its own coordinates, not the screen's"
        );

        registry.apply(GridEvent::Hide { grid: GridId(5) });
        let _ = registry.take_damage();
        put(&mut registry, GridId(5), 2);
        let hidden = registry.take_damage();
        assert!(
            !hidden.full && hidden.rows.is_empty(),
            "a hidden pane damaged the frame: {hidden:?}"
        );
    }

    /// The startup shape the two milestone lines are read off, in the
    /// order a live session sends it: nvim sizes a window grid at the
    /// attach and draws nothing into it, view paints its chrome over that,
    /// and the file arrives a whole first screen update later.
    ///
    /// Disconfirm: answering off the global grid, off any placed grid, off
    /// a grid that has only been sized, or off either visibility flag alone
    /// turns the third line back into the second one.
    #[test]
    fn only_a_visible_window_holding_text_reads_as_content() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        resize(&mut registry, GridId(3), 80, 24);
        assert!(
            !registry.window_text_painted(),
            "a window grid nvim has sized and not drawn into is the chrome frame"
        );

        registry.apply(GridEvent::Message {
            grid: GridId(4),
            row: 23,
            zindex: 200,
            compindex: 0,
        });
        resize(&mut registry, GridId(4), 80, 1);
        put(&mut registry, GridId(4), 0);
        registry.apply(GridEvent::Float {
            grid: GridId(5),
            anchor_grid: GLOBAL_GRID,
            screen_row: 1,
            screen_col: 1,
            zindex: 50,
            compindex: 1,
        });
        resize(&mut registry, GridId(5), 20, 5);
        put(&mut registry, GridId(5), 0);
        assert!(
            !registry.window_text_painted(),
            "a message area and a float are not the file the user opened"
        );

        window(&mut registry, GridId(6), 0, 0);
        resize(&mut registry, GridId(6), 80, 23);
        put(&mut registry, GridId(6), 0);
        registry.apply(GridEvent::Hide { grid: GridId(6) });
        assert!(
            !registry.window_text_painted(),
            "a window nvim has taken off screen shows the user nothing"
        );

        registry.withhold_float(GridId(5), true);
        window(&mut registry, GridId(5), 0, 0);
        assert!(
            !registry.window_text_painted(),
            "a withheld float re-placed as a window keeps the hold and paints no cell"
        );

        window(&mut registry, GridId(2), 0, 0);
        resize(&mut registry, GridId(2), 80, 23);
        assert!(
            !registry.window_text_painted(),
            "the window is placed and nvim has still drawn nothing into it"
        );
        put(&mut registry, GridId(2), 0);
        assert!(
            registry.window_text_painted(),
            "the file the user opened reached the window"
        );
    }

    /// Without `ext_multigrid` there is no window grid to ask, and nvim
    /// composites the file into the global grid along with everything else.
    #[test]
    fn a_single_grid_session_reads_its_content_off_the_global_grid() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GLOBAL_GRID, 80, 24);
        assert!(
            !registry.window_text_painted(),
            "a sized and undrawn global grid carries no file either"
        );
        put(&mut registry, GLOBAL_GRID, 0);
        assert!(
            registry.window_text_painted(),
            "the single-grid session's whole picture is the global grid"
        );
    }

    fn put(registry: &mut GridRegistry, grid: GridId, row: u16) {
        registry.apply(GridEvent::Cells {
            grid,
            op: GridOp::PutLine {
                row,
                col_start: 0,
                cells: vec![("x".into(), 0, 1)],
            },
        });
    }

    fn resize(registry: &mut GridRegistry, grid: GridId, width: u16, height: u16) {
        registry.apply(GridEvent::Cells {
            grid,
            op: GridOp::Resize { width, height },
        });
    }

    fn ids(registry: &GridRegistry) -> Vec<GridId> {
        registry
            .panes_in_z_order()
            .into_iter()
            .map(|pane| pane.id)
            .collect()
    }

    fn tiles(gaps: bool) -> Look {
        Look::new(Panes::Tiles, gaps)
    }

    /// Places `grid`'s window in `slot` without touching the grid's own
    /// size, which is the shape a `win_pos` takes before the `grid_resize`
    /// that answers it.
    fn window_slot(registry: &mut GridRegistry, grid: GridId, slot: (u16, u16, u16, u16)) {
        let (startrow, startcol, width, height) = slot;
        registry.apply(GridEvent::Window {
            grid,
            win: WinHandle(grid.0),
            startrow,
            startcol,
            width,
            height,
        });
    }

    fn pane_of(registry: &GridRegistry, grid: GridId) -> Pane {
        registry
            .panes_in_z_order()
            .into_iter()
            .find(|pane| pane.id == grid)
            .expect("the grid is placed")
    }

    #[test]
    fn a_window_event_carries_the_slot_and_the_handle() {
        let mut registry = GridRegistry::new();
        assert!(registry.set_look(tiles(true)));
        window_slot(&mut registry, GridId(2), (3, 5, 40, 20));
        let pane = pane_of(&registry, GridId(2));
        assert_eq!(pane.slot, (3, 5, 40, 20), "the slot nvim reported");
        assert_eq!(pane.origin, (5, 7), "the inner origin the look puts in it");
        assert_eq!(registry.window_handle(GridId(2)), Some(WinHandle(2)));
    }

    #[test]
    fn a_viewport_margin_is_kept_for_the_grid_it_names() {
        let mut registry = GridRegistry::new();
        registry.set_look(tiles(true));
        window_slot(&mut registry, GridId(2), (0, 0, 40, 20));
        window_slot(&mut registry, GridId(3), (0, 40, 40, 20));
        assert_eq!(
            registry.pending_inner_request(GridId(2), tiles(true)),
            Some((36, 16))
        );
        registry.apply(GridEvent::Margins {
            grid: GridId(2),
            top: 1,
        });
        assert_eq!(
            registry.pending_inner_request(GridId(2), tiles(true)),
            Some((36, 15)),
            "the winbar row comes out of the height the window is asked for"
        );
        assert_eq!(
            registry.pending_inner_request(GridId(3), tiles(true)),
            Some((36, 16)),
            "the neighbour has no winbar and owes its whole inner height"
        );
    }

    #[test]
    fn a_gapped_slot_requests_four_cells_less_at_the_inset_origin() {
        let mut registry = GridRegistry::new();
        registry.set_look(tiles(true));
        window_slot(&mut registry, GridId(2), (4, 6, 40, 20));
        assert_eq!(
            registry.pending_inner_request(GridId(2), tiles(true)),
            Some((36, 16))
        );
        assert_eq!(pane_of(&registry, GridId(2)).origin, (6, 8));
    }

    #[test]
    fn a_gapless_slot_requests_nothing_and_fills_its_slot() {
        let mut registry = GridRegistry::new();
        registry.set_look(tiles(false));
        window_slot(&mut registry, GridId(2), (4, 6, 40, 20));
        assert_eq!(
            registry.pending_inner_request(GridId(2), tiles(false)),
            Some((0, 0))
        );
        assert_eq!(pane_of(&registry, GridId(2)).origin, (4, 6));
        assert!(tiles(false).frames((40, 20), 0));
    }

    #[test]
    fn a_winbar_takes_its_row_out_of_the_height_request() {
        let mut registry = GridRegistry::new();
        registry.set_look(tiles(true));
        window_slot(&mut registry, GridId(2), (0, 0, 40, 20));
        registry.apply(GridEvent::Margins {
            grid: GridId(2),
            top: 1,
        });
        assert_eq!(
            registry.pending_inner_request(GridId(2), tiles(true)),
            Some((36, 15))
        );
    }

    #[test]
    fn a_slot_under_the_framed_minimum_is_drawn_bare() {
        let look = tiles(true);
        let (min_width, min_height) = MIN_FRAMED_SLOT;
        for slot in [(0, 0, min_width - 1, 20), (0, 0, 40, min_height - 1)] {
            let mut registry = GridRegistry::new();
            registry.set_look(look);
            window_slot(&mut registry, GridId(2), slot);
            assert_eq!(
                registry.pending_inner_request(GridId(2), look),
                Some((0, 0)),
                "a slot of {slot:?} has no room for a ring"
            );
            assert_eq!(pane_of(&registry, GridId(2)).origin, (slot.0, slot.1));
            assert!(!look.frames((slot.2, slot.3), 0));
        }
        assert!(look.frames((min_width, min_height), 0));
    }

    #[test]
    fn a_five_row_slot_with_a_winbar_is_drawn_bare() {
        let look = tiles(true);
        let mut registry = GridRegistry::new();
        registry.set_look(look);
        window_slot(&mut registry, GridId(2), (0, 0, 40, 5));
        registry.apply(GridEvent::Margins {
            grid: GridId(2),
            top: 1,
        });
        assert_eq!(
            registry.pending_inner_request(GridId(2), look),
            Some((0, 0))
        );
        assert_eq!(pane_of(&registry, GridId(2)).origin, (0, 0));
        assert!(!look.frames((40, 5), 1));
        assert!(look.frames((40, 6), 1), "one more row and the ring fits");
    }

    #[test]
    fn nvim_mode_requests_nothing_and_frames_nothing() {
        let look = Look::new(Panes::Nvim, true);
        let mut registry = GridRegistry::new();
        window_slot(&mut registry, GridId(2), (4, 6, 40, 20));
        assert_eq!(look.ring(), 0);
        assert_eq!(
            registry.pending_inner_request(GridId(2), look),
            Some((0, 0))
        );
        assert_eq!(pane_of(&registry, GridId(2)).origin, (4, 6));
        assert!(!look.frames((40, 20), 0));
    }

    #[test]
    fn an_unchanged_slot_sends_no_second_request() {
        let mut registry = GridRegistry::new();
        registry.set_look(tiles(true));
        window_slot(&mut registry, GridId(2), (0, 0, 40, 20));
        assert_eq!(
            registry.pending_inner_request(GridId(2), tiles(true)),
            Some((36, 16))
        );
        assert_eq!(registry.pending_inner_request(GridId(2), tiles(true)), None);
        window_slot(&mut registry, GridId(2), (0, 0, 40, 20));
        assert_eq!(
            registry.pending_inner_request(GridId(2), tiles(true)),
            None,
            "nvim re-announcing the same slot changes nothing the window owes"
        );
    }

    #[test]
    fn a_gaps_flip_on_an_unmoved_slot_still_requests() {
        let mut registry = GridRegistry::new();
        registry.set_look(tiles(true));
        window_slot(&mut registry, GridId(2), (0, 0, 40, 20));
        assert_eq!(
            registry.pending_inner_request(GridId(2), tiles(true)),
            Some((36, 16))
        );
        assert!(registry.set_look(tiles(false)));
        assert_eq!(
            registry.pending_inner_request(GridId(2), tiles(false)),
            Some((0, 0)),
            "the slot is where it was and the look changed"
        );
    }

    #[test]
    fn a_flip_to_nvim_mode_clears_every_standing_request() {
        let mut registry = GridRegistry::new();
        registry.set_look(tiles(true));
        window_slot(&mut registry, GridId(2), (0, 0, 40, 20));
        window_slot(&mut registry, GridId(3), (0, 40, 40, 20));
        for grid in registry.window_grids() {
            assert_eq!(
                registry.pending_inner_request(grid, tiles(true)),
                Some((36, 16))
            );
        }
        let nvim = Look::new(Panes::Nvim, true);
        assert!(registry.set_look(nvim));
        let cleared: Vec<_> = registry
            .window_grids()
            .into_iter()
            .map(|grid| registry.pending_inner_request(grid, nvim))
            .collect();
        assert_eq!(cleared, vec![Some((0, 0)), Some((0, 0))]);
    }

    #[test]
    fn a_float_anchored_to_a_gapped_window_paints_at_the_inner_origin() {
        let mut registry = GridRegistry::new();
        registry.set_look(tiles(true));
        window_slot(&mut registry, GridId(2), (4, 6, 40, 20));
        resize(&mut registry, GridId(7), 10, 3);
        registry.apply(GridEvent::Float {
            grid: GridId(7),
            anchor_grid: GridId(2),
            screen_row: 5,
            screen_col: 7,
            zindex: 50,
            compindex: 1,
        });
        assert_eq!(
            pane_of(&registry, GridId(7)).origin,
            (7, 9),
            "nvim placed the float from the slot origin, two cells out from \
             the text it hovers"
        );
    }

    /// Places a window nvim opened for a surface view claimed the handle
    /// of, which is the whole shape of a windowed surface's placement.
    fn claimed(registry: &mut GridRegistry, grid: GridId, surface: NativeSurface) {
        registry.claim_native_window(WinHandle(grid.0), surface);
        resize(registry, grid, 30, 20);
        window(registry, grid, 0, 0);
    }

    #[test]
    fn a_claimed_window_handle_places_a_native_pane() {
        let mut registry = GridRegistry::new();
        claimed(&mut registry, GridId(2), NativeSurface::Tree);
        let panes = registry.panes_in_z_order();
        assert!(
            panes.iter().any(|pane| pane.id == GridId(2)
                && pane.kind
                    == PaneKind::Native {
                        surface: NativeSurface::Tree
                    }),
            "the claimed handle did not place a native pane: {panes:?}"
        );
        registry.apply_cells(GridId(2), GridOp::CursorGoto { row: 0, col: 0 });
        assert_eq!(
            registry.native_pane_focus(),
            Some(NativeSurface::Tree),
            "the cursor in the native pane did not name its surface"
        );
    }

    #[test]
    fn a_native_panes_grid_line_paints_nothing() {
        let mut registry = GridRegistry::new();
        claimed(&mut registry, GridId(2), NativeSurface::Tree);
        registry.apply_cells(
            GridId(2),
            GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("scratch".to_string(), 0, 1)],
            },
        );
        assert_eq!(
            registry.grid(GridId(2)).map(Grid::has_text),
            Some(false),
            "the engine's cells for the scratch buffer reached the grid"
        );
        assert_eq!(
            registry.grid(GridId(2)).map(Grid::size),
            Some((30, 20)),
            "the native pane lost the size its resize gave it"
        );
    }

    #[test]
    fn an_unclaimed_window_handle_still_places_an_ordinary_pane() {
        let mut registry = GridRegistry::new();
        resize(&mut registry, GridId(2), 30, 20);
        window(&mut registry, GridId(2), 0, 0);
        registry.apply_cells(
            GridId(2),
            GridOp::PutLine {
                row: 0,
                col_start: 0,
                cells: vec![("buffer".to_string(), 0, 1)],
            },
        );
        let panes = registry.panes_in_z_order();
        assert!(
            panes
                .iter()
                .any(|pane| pane.id == GridId(2) && pane.kind == PaneKind::Window),
            "an unclaimed window was placed as something other than a window: {panes:?}"
        );
        assert_eq!(
            registry.native_pane_focus(),
            None,
            "an ordinary window named a surface"
        );
        assert_eq!(
            registry
                .grid(GridId(2))
                .map(|grid| grid.row_text(0).trim_end().to_string()),
            Some("buffer".to_string()),
            "an ordinary window's cells were dropped"
        );
    }

    /// nvim names a grid in a margins event before the split that would
    /// size it has settled, and settles on a different grid without
    /// destroying the first. A slot made here would outlive the session.
    #[test]
    fn margins_for_a_grid_nvim_never_sized_leave_no_slot_behind() {
        let mut registry = GridRegistry::new();
        registry.apply(GridEvent::Margins {
            grid: GridId(5),
            top: 1,
        });
        assert!(
            registry.grid(GridId(5)).is_none(),
            "a grid nvim never sized or placed was recorded anyway"
        );
        assert_eq!(
            registry.grid_ids(),
            vec![GLOBAL_GRID],
            "the margins event left a slot behind"
        );
    }
}
