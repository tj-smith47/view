//! The surfaces view draws itself, what it does when a plugin draws over
//! one of them, and the arithmetic that decides whether a floating window
//! is doing exactly that.
//!
//! The set view claims is fixed and small, so the conflict class is
//! decidable generically -- for a plugin nobody has tested, and without
//! naming one. What a float actually carries is recorded live in
//! `docs/surface-float-wire-capture.md`, and two of its findings shape
//! everything here:
//!
//! - **Rect overlap with what view paints answers backwards.** Read
//!   against view's own palette box, nvim-cmp's cmdline menu (grid rows
//!   26..27) misses it entirely while telescope's picker (rows 1..26)
//!   covers it whole -- so the float that claims a surface looks innocent
//!   and the negative control looks guilty. A claim is against the region
//!   the *engine* leaves for the surface view took over, which is why
//!   [`claims`] measures against the grid's own edges and never against a
//!   painted overlay.
//! - **Geometry is the weak axis; the session's own state is the strong
//!   one.** So each rule below is a conjunction: a rect that lands where a
//!   surface lives *and* a state only that surface produces. The
//!   command-line rule fires only while a command line is actually open,
//!   which is what keeps a picker whose lowest chrome window sits one row
//!   above the menu's bottom edge silent.
//!
//! Nothing here names a plugin, does I/O, or allocates per observation:
//! [`claims`] is integer arithmetic over one rect and the grid's size.

use crate::model::Model;
use crate::native::ext::Ext;

/// One surface a session can externalize, plus the buffer grid nvim keeps
/// for itself -- the whole vocabulary a claim can be about.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Surface {
    /// The command line, rendered by view as the palette.
    Cmdline,
    /// The completion popup that feeds the palette.
    Popupmenu,
    /// Messages, rendered by view as toasts and the message history.
    Messages,
    /// The tab line.
    Tabline,
    /// The buffer grid, which view never draws over: nvim owns it, and so
    /// does anything that wants to float above it.
    Grid,
}

/// What a claim on a surface means for view.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Policy {
    /// view keeps drawing it, so a second renderer on the same cells is a
    /// conflict the user is told about once, with the line that resolves
    /// it.
    Own,
    /// view does not draw it at all, so drawing there claims nothing.
    Yield,
    /// view takes what the claimant drew into its own chrome rather than
    /// letting two renderers stack -- what already happens to a
    /// cmdline-sourced popupmenu, whose rows are folded into the palette
    /// instead of painted as a second menu (`view_surface`'s
    /// `consumed_by_palette`).
    Absorb,
}

/// One row of the ownership table: what view does with `surface`, and the
/// `view.toml` line that hands it back.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnedSurface {
    /// The surface this row is about.
    pub surface: Surface,
    /// The `ext_*` capability whose attachment decides whether view draws
    /// this surface at all, or `None` for a surface no attach carries.
    pub ext: Option<Ext>,
    /// What view does with a claim on it.
    pub policy: Policy,
    /// How a notice names it to a user, who never sees an `ext_*` key.
    pub label: &'static str,
    /// The `[native]` line that returns it, or `None` when no switch
    /// reaches this surface today -- a notice about such a surface says
    /// what happened and stops, rather than naming a setting that does not
    /// exist.
    pub remedy: Option<&'static str>,
}

/// Every surface, in the order a notice lists them when one identity
/// claims more than one. Data rather than a `match`, so the set stays
/// enumerable: the notice, the ownership gate and the policy all read this
/// one table, and a surface added to [`Surface`] without a row here fails
/// `every_surface_has_exactly_one_row`.
pub const SURFACES: &[OwnedSurface] = &[
    OwnedSurface {
        surface: Surface::Cmdline,
        ext: Some(Ext::Cmdline),
        policy: Policy::Own,
        label: "the command line",
        remedy: Some("[native] palette = false"),
    },
    OwnedSurface {
        surface: Surface::Popupmenu,
        ext: Some(Ext::Popupmenu),
        policy: Policy::Absorb,
        label: "the completion menu",
        remedy: Some("[native] palette = false"),
    },
    OwnedSurface {
        surface: Surface::Messages,
        ext: Some(Ext::Messages),
        policy: Policy::Own,
        label: "the message area",
        remedy: Some("[native] notifications = false"),
    },
    OwnedSurface {
        surface: Surface::Tabline,
        ext: Some(Ext::Tabline),
        policy: Policy::Own,
        label: "the tab line",
        remedy: None,
    },
    OwnedSurface {
        surface: Surface::Grid,
        ext: None,
        policy: Policy::Yield,
        label: "the buffer grid",
        remedy: None,
    },
];

/// `surface`'s row of [`SURFACES`], or `None` for a variant the table has
/// no row for -- which `every_surface_has_exactly_one_row` denies, so a
/// caller reading `None` is reading a table that has already failed its
/// own walk rather than a case it must invent an answer for.
#[must_use]
pub fn row(surface: Surface) -> Option<&'static OwnedSurface> {
    SURFACES.iter().find(|row| row.surface == surface)
}

/// Which corner of a float its `row`/`col` name.
///
/// Load-bearing rather than decoration: an `NE`-anchored window at
/// `col = 100` on a 100-column grid has its *right* edge there and covers
/// columns 50..99 at width 50, so a consumer reading `col` without the
/// anchor places nvim-notify's toast off the grid entirely (the wire
/// capture's own warning).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FloatAnchor {
    /// `row`/`col` are the top-left corner.
    NorthWest,
    /// `row`/`col` are the top-right corner.
    NorthEast,
    /// `row`/`col` are the bottom-left corner.
    SouthWest,
    /// `row`/`col` are the bottom-right corner.
    SouthEast,
}

impl FloatAnchor {
    /// nvim's own spelling, as `nvim_win_get_config` answers it. An
    /// unrecognized spelling reads as [`FloatAnchor::NorthWest`], which is
    /// nvim's own default for a window that names no anchor.
    #[must_use]
    pub fn from_wire(anchor: &str) -> Self {
        match anchor {
            "NE" => Self::NorthEast,
            "SW" => Self::SouthWest,
            "SE" => Self::SouthEast,
            _ => Self::NorthWest,
        }
    }
}

/// One floating window as the engine-side watcher saw it, in the grid's
/// own cells.
///
/// Identity is carried by name, never by number: window and namespace ids
/// are per-session allocations that name something else on the next run
/// (the wire capture measures three runs handing the same six namespaces
/// six different numbers), so `win`/`buf` are here for a consumer acting on
/// *this* observation and never for remembering a float by.
///
/// Deliberately not `#[non_exhaustive]`, unlike everything else in this
/// module: `view-engine`'s bridge decoder is what fills it in, and the
/// build breaking there is exactly what a new field should cost -- a field
/// the wire never carries is a field decoded from nothing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FloatSighting {
    /// The window handle, valid only for this observation.
    pub win: u64,
    /// The buffer handle behind it, valid only for this observation.
    pub buf: u64,
    /// The anchor corner's row, which may be negative or off-grid.
    pub row: i64,
    /// The anchor corner's column, which may be negative or off-grid.
    pub col: i64,
    /// Width in cells.
    pub width: u16,
    /// Height in cells.
    pub height: u16,
    /// Which corner `row`/`col` name.
    pub anchor: FloatAnchor,
    /// The window's `zindex`.
    pub zindex: u16,
    /// The buffer's filetype: the one identifying mark most floats carry
    /// (`cmp_menu`, `notify`, `TelescopeResults`), and empty for the rest.
    pub filetype: String,
    /// The buffer's name, empty for the unfiled scratch buffer nearly every
    /// float uses.
    pub name: String,
}

impl FloatSighting {
    /// The best name this float carries for whatever opened it, or `None`
    /// when it carries none.
    ///
    /// Best-effort and bounded by design: a floating window records no
    /// authorship, so this reads what the window itself sets -- the
    /// filetype first (the mark every observed plugin sets on its own
    /// menu), then the buffer name -- and never infers a plugin from
    /// geometry.
    #[must_use]
    pub fn identity(&self) -> Option<&str> {
        [self.filetype.as_str(), self.name.as_str()]
            .into_iter()
            .find(|mark| !mark.is_empty())
    }

    /// The inclusive `(top, left, bottom, right)` cell span this float
    /// covers, with its anchor resolved and every edge clamped into a
    /// `grid_w` by `grid_h` grid.
    ///
    /// `None` for a float covering no cells at all -- a zero width or
    /// height, or a rect entirely off the grid -- which claims nothing by
    /// definition.
    fn span(&self, grid_w: u16, grid_h: u16) -> Option<(i64, i64, i64, i64)> {
        if self.width == 0 || self.height == 0 || grid_w == 0 || grid_h == 0 {
            return None;
        }
        let width = i64::from(self.width);
        let height = i64::from(self.height);
        let (top, left) = match self.anchor {
            FloatAnchor::NorthWest => (self.row, self.col),
            FloatAnchor::NorthEast => (self.row, self.col - width),
            FloatAnchor::SouthWest => (self.row - height + 1, self.col),
            FloatAnchor::SouthEast => (self.row - height + 1, self.col - width),
        };
        let (bottom, right) = (top + height - 1, left + width - 1);
        let (last_row, last_col) = (i64::from(grid_h) - 1, i64::from(grid_w) - 1);
        if bottom < 0 || right < 0 || top > last_row || left > last_col {
            return None;
        }
        Some((
            top.max(0),
            left.max(0),
            bottom.min(last_row),
            right.min(last_col),
        ))
    }
}

/// How many rows at the bottom of the grid belong to the command line: the
/// row nvim itself would draw one on, plus the row above it that a plugin
/// drawing a cmdline completion holds back for it. nvim-cmp's own
/// `window.lua` clamps its height to keep exactly one, which is why its
/// menu's bottom edge lands on the second-to-last grid row rather than the
/// last one.
const CMDLINE_ROWS: i64 = 2;

/// The surface `float` is drawing over, or `None` when it is drawing
/// somewhere view does not.
///
/// Answers only about surfaces this session actually externalized
/// ([`Model::owns`]): with `[native] palette = false` the command line was
/// never taken from the user's plugins, so a float drawing one is doing its
/// job and there is nothing to report.
///
/// Each rule pairs a rect with a session state, because neither is
/// sufficient alone (see this module's own docs):
///
/// - **the command line**, when a command line is actually open and the
///   float's bottom edge lands in the rows the engine keeps for it. The
///   open command line is what parts nvim-cmp's cmdline menu from a picker
///   whose lowest chrome window sits one row above the same band.
/// - **the message area**, when the float is pinned to the grid's top
///   right corner -- where view stacks its toasts -- and is short enough to
///   be chrome rather than a screenful. A picker centered in the grid
///   overlaps that corner without being pinned to it, which is the
///   distinction the negative control turns on.
#[must_use]
pub fn claims(float: &FloatSighting, model: &Model) -> Option<Surface> {
    let (grid_w, grid_h) = model.engine.grid().size();
    let (top, _left, bottom, right) = float.span(grid_w, grid_h)?;
    let last_row = i64::from(grid_h) - 1;
    // half the grid: the bound between a piece of chrome pinned in a corner
    // and a window that has taken the screen over, which is a different
    // thing and not this detector's business
    let chrome_rows = i64::from(grid_h) / 2;
    if model.engine.cmdline.is_some() && bottom >= last_row - (CMDLINE_ROWS - 1) {
        return owned(Surface::Cmdline, model);
    }
    let rows = bottom - top + 1;
    if right == i64::from(grid_w) - 1 && top < chrome_rows && rows <= chrome_rows {
        return owned(Surface::Messages, model);
    }
    None
}

/// `surface` if this session externalized it, `None` otherwise -- the gate
/// that makes the detector follow the `[native]` switches instead of a
/// constant.
fn owned(surface: Surface, model: &Model) -> Option<Surface> {
    match row(surface)?.ext {
        Some(ext) if model.owns(ext) => Some(surface),
        _ => None,
    }
}

/// Which identities have been seen claiming which surfaces, so a second
/// sighting adds to one notice rather than raising a second one.
///
/// Keyed on the identity a float carries -- `None` being its own key,
/// shared by every float carrying none -- which is the key the notice's own
/// family is built from too: two notices sharing a family retract each
/// other by construction
/// ([`record_native_notice_once`](crate::model::EngineModel::record_native_notice_once)),
/// so the aggregation here and the withdrawal there have to agree on what
/// one claimant is.
#[derive(Debug, Default)]
pub struct SurfaceConflicts {
    claimants: Vec<Claimant>,
}

/// One identity's standing claim.
#[derive(Debug)]
struct Claimant {
    identity: Option<String>,
    surfaces: Vec<Surface>,
}

impl SurfaceConflicts {
    /// Records that `identity` was seen claiming `surface`, and answers
    /// every surface it now claims -- on the repeat sighting as much as on
    /// the first.
    ///
    /// The set is kept in [`SURFACES`] order rather than in the order the
    /// floats happened to arrive, so one identity claiming two surfaces
    /// reads the same way whichever it claimed first.
    ///
    /// Deliberately not "news only". A native notice is a transient toast
    /// that the user's *next keystroke* dismisses
    /// ([`Messages::dismiss_transient_on_keypress`](crate::model::Messages::dismiss_transient_on_keypress)),
    /// and the float this detects is most often one a keystroke summoned --
    /// nvim-cmp's cmdline menu re-lays itself out on every key. A set
    /// reported only the first time therefore raised one line that the very
    /// next key wiped, and nothing could ever say it again for the rest of
    /// the session (proven live: the notice stood for 214 ms and was gone
    /// before the scenario could read the screen). Answering every time
    /// leaves "say it once" where it belongs -- in
    /// [`record_native_notice_once`](crate::model::EngineModel::record_native_notice_once),
    /// which no-ops on a line already standing and speaks again only when
    /// nothing of that claimant's is on screen.
    pub fn record(&mut self, identity: Option<&str>, surface: Surface) -> Option<&[Surface]> {
        let index = match self
            .claimants
            .iter()
            .position(|claimant| claimant.identity.as_deref() == identity)
        {
            Some(index) => index,
            None => {
                self.claimants.push(Claimant {
                    identity: identity.map(str::to_owned),
                    surfaces: Vec::new(),
                });
                self.claimants.len() - 1
            }
        };
        let claimant = self.claimants.get_mut(index)?;
        if !claimant.surfaces.contains(&surface) {
            claimant.surfaces.push(surface);
            claimant
                .surfaces
                .sort_by_key(|surface| SURFACES.iter().position(|row| row.surface == *surface));
        }
        Some(&claimant.surfaces)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{claims, row, FloatAnchor, FloatSighting, Surface, SurfaceConflicts, SURFACES};
    use crate::events::UiEvent;
    use crate::model::Model;
    use crate::native::ext::Ext;
    use crate::update::update;

    /// The capture's own session: a 100x30 terminal whose nvim grid is 29
    /// rows, one terminal row being view's chrome. Every rect below is
    /// transcribed from `docs/surface-float-wire-capture.md` against
    /// exactly this geometry.
    const GRID_W: u16 = 100;
    const GRID_H: u16 = 29;

    /// A model attached to the capture's grid with every surface
    /// externalized, which is what the capture itself ran with
    /// (`native = {}`).
    fn captured_session() -> Model {
        let mut model = Model::with_term_size(GRID_W, GRID_H + 1);
        let _ = update(
            &mut model,
            crate::msg::Msg::Redraw(vec![UiEvent::GridResize {
                grid: 1,
                width: u64::from(GRID_W),
                height: u64::from(GRID_H),
            }]),
        );
        model
    }

    /// A command line standing open, the state nvim-cmp's cmdline menu
    /// only ever appears in.
    fn open_cmdline(model: &mut Model) {
        let _ = update(
            model,
            crate::msg::Msg::Redraw(vec![UiEvent::CmdlineShow {
                content: vec![(0, "e pre".to_string())],
                pos: 5,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            }]),
        );
    }

    /// A float with the capture's own defaults, named by its filetype.
    fn float(filetype: &str, row: i64, col: i64, width: u16, height: u16) -> FloatSighting {
        FloatSighting {
            win: 1003,
            buf: 2,
            row,
            col,
            width,
            height,
            anchor: FloatAnchor::NorthWest,
            zindex: 50,
            filetype: filetype.to_string(),
            name: String::new(),
        }
    }

    /// nvim-cmp's cmdline completion menu at `:pref`, verbatim from the
    /// capture: `row = 26, col = 0, width = 20, height = 2, zindex = 1001`,
    /// filetype `cmp_menu`.
    fn cmp_cmdline_menu() -> FloatSighting {
        FloatSighting {
            zindex: 1001,
            ..float("cmp_menu", 26, 0, 20, 2)
        }
    }

    /// nvim-notify's toast, verbatim from the capture: anchored `NE` at
    /// `row = 0, col = 100`, 50 by 3 -- which resolves to columns 50..99,
    /// not 51..100.
    fn notify_toast() -> FloatSighting {
        FloatSighting {
            anchor: FloatAnchor::NorthEast,
            ..float("notify", 0, 100, 50, 3)
        }
    }

    /// telescope's four picker windows, verbatim from the capture. The
    /// negative control: a detector that flags any of these flags every
    /// float.
    fn telescope_picker() -> Vec<FloatSighting> {
        vec![
            float("TelescopeResults", 2, 11, 78, 21),
            float("", 1, 10, 80, 23),
            float("TelescopePrompt", 25, 11, 78, 1),
            float("", 24, 10, 80, 3),
        ]
    }

    #[test]
    fn every_surface_has_exactly_one_row() {
        for surface in [
            Surface::Cmdline,
            Surface::Popupmenu,
            Surface::Messages,
            Surface::Tabline,
            Surface::Grid,
        ] {
            let rows = SURFACES.iter().filter(|r| r.surface == surface).count();
            assert_eq!(rows, 1, "{surface:?} needs exactly one row of the table");
            assert!(row(surface).is_some());
        }
        assert_eq!(
            SURFACES.len(),
            5,
            "a surface added to the enum needs a row here, with its own policy and remedy"
        );
    }

    #[test]
    fn a_float_on_the_cmdline_row_is_a_cmdline_claim() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        assert_eq!(
            claims(&cmp_cmdline_menu(), &model),
            Some(Surface::Cmdline),
            "cmp's menu bottom edge lands on the row the engine keeps for the command line"
        );
    }

    #[test]
    fn a_float_in_the_message_area_is_a_messages_claim() {
        let model = captured_session();
        assert_eq!(
            claims(&notify_toast(), &model),
            Some(Surface::Messages),
            "a toast pinned to the top right corner draws where view stacks its own"
        );
    }

    /// The test that keeps the detector from becoming noise. Every one of
    /// telescope's four windows is checked, with and without a command line
    /// open, because the picker's lowest chrome window sits one row above
    /// cmp's menu bottom and a threshold rule on the last few grid rows
    /// would flag it.
    #[test]
    fn a_centered_picker_float_claims_nothing() {
        let mut model = captured_session();
        for window in telescope_picker() {
            assert_eq!(claims(&window, &model), None, "{window:?}");
        }
        open_cmdline(&mut model);
        for window in telescope_picker() {
            assert_eq!(
                claims(&window, &model),
                None,
                "with a cmdline open: {window:?}"
            );
        }
    }

    /// A float claiming a surface this session handed back is not a
    /// conflict, whatever its geometry says. Both halves are asserted
    /// against a session that would otherwise answer `Some`: the cmdline is
    /// open and the toast is in the corner, so only the ownership gate can
    /// be what silences them.
    #[test]
    fn a_claim_on_a_surface_view_yielded_claims_nothing() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        model.attach_surfaces(vec![Ext::LineGrid, Ext::Tabline]);
        assert_eq!(claims(&cmp_cmdline_menu(), &model), None);
        assert_eq!(claims(&notify_toast(), &model), None);

        model.attach_surfaces(vec![Ext::LineGrid, Ext::Cmdline, Ext::Tabline]);
        assert_eq!(
            claims(&cmp_cmdline_menu(), &model),
            Some(Surface::Cmdline),
            "the cmdline comes back on its own switch, and the message area stays yielded"
        );
        assert_eq!(claims(&notify_toast(), &model), None);
    }

    #[test]
    fn a_float_with_no_cells_on_the_grid_claims_nothing() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        for empty in [
            float("cmp_menu", 26, 0, 0, 2),
            float("cmp_menu", 26, 0, 20, 0),
            float("cmp_menu", -9, 0, 20, 2),
            float("cmp_menu", 26, 200, 20, 2),
        ] {
            assert_eq!(claims(&empty, &model), None, "{empty:?}");
        }
    }

    #[test]
    fn an_identity_is_the_filetype_then_the_buffer_name_then_nothing() {
        assert_eq!(cmp_cmdline_menu().identity(), Some("cmp_menu"));
        let named = FloatSighting {
            name: "/tmp/scratch".to_string(),
            ..float("", 0, 0, 4, 4)
        };
        assert_eq!(named.identity(), Some("/tmp/scratch"));
        assert_eq!(float("", 0, 0, 4, 4).identity(), None);
    }

    #[test]
    fn one_claimant_accumulates_its_surfaces_in_table_order() {
        let mut conflicts = SurfaceConflicts::default();
        assert_eq!(
            conflicts.record(Some("noice"), Surface::Messages),
            Some([Surface::Messages].as_slice())
        );
        assert_eq!(
            conflicts.record(Some("noice"), Surface::Messages),
            Some([Surface::Messages].as_slice()),
            "a repeat answers the same set rather than falling silent: the line it \
             raises may have been dismissed since"
        );
        assert_eq!(
            conflicts.record(Some("noice"), Surface::Cmdline),
            Some([Surface::Cmdline, Surface::Messages].as_slice()),
            "the table's order, not the order the floats arrived in"
        );
        assert_eq!(
            conflicts.record(Some("cmp_menu"), Surface::Cmdline),
            Some([Surface::Cmdline].as_slice()),
            "another identity keeps its own set"
        );
        assert_eq!(
            conflicts.record(None, Surface::Cmdline),
            Some([Surface::Cmdline].as_slice()),
            "a float with no identity is its own claimant, the one the notice calls a plugin"
        );
    }
}
