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

/// One plugin class whose whole purpose is to render a surface view also
/// renders, identified by the Lua module a session can be asked about.
///
/// A float sighting names a *widget*; this names a *plugin*, which is the
/// difference between "something is drawing over the command line" and
/// "noice.nvim is using the command line". The presence question is asked of
/// `package.loaded`, the public module registry, so nothing here reaches
/// into a plugin's private state or depends on a version of its config.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceClaimant {
    /// How a notice names the plugin, in the spelling its own README uses.
    pub class: &'static str,
    /// The module `package.loaded` is asked about.
    pub module: &'static str,
    /// Every surface this class exists to render. Filtered at notice time
    /// against [`Policy::Own`] and [`Model::owns`], so a surface this
    /// session handed back -- or one view absorbs rather than fights over --
    /// is not something the user is told about.
    pub surfaces: &'static [Surface],
}

/// The shipped claimant table.
///
/// One row, deliberately. A claimant row is a *named* claim, and the price
/// of naming a plugin that turns out not to be drawing anything is a notice
/// that says something false; the generic, evidence-first path for every
/// plugin nobody enumerated is the float detector ([`claims`]), which needs
/// no table at all. noice.nvim earns a row because it is the class of
/// record: it exists to take the command line, the popup menu and the
/// messages, it says so in its own health check, and on view's defaults that
/// check fires three errors at a user who has been told nothing about how to
/// resolve them.
pub const SURFACE_CLAIMANTS: &[SurfaceClaimant] = &[SurfaceClaimant {
    class: "noice.nvim",
    module: "noice",
    surfaces: &[Surface::Cmdline, Surface::Popupmenu, Surface::Messages],
}];

/// The claimants `probed` names, in table order -- the order a notice per
/// claimant is raised in, so two claimants read the same way whichever
/// module the probe listed first.
///
/// Unknown module names are ignored rather than trusted: the reply crosses
/// the wire, and a name no row carries names no claimant.
pub fn probed_claimants(probed: &[String]) -> impl Iterator<Item = &'static SurfaceClaimant> + '_ {
    SURFACE_CLAIMANTS
        .iter()
        .filter(|claimant| probed.iter().any(|name| name == claimant.module))
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

/// Filetypes that say what a buffer holds, never who opened the window.
///
/// The capture's discriminator table parts the two kinds cleanly. Every
/// float that names its plugin does it with a filetype the plugin invented
/// for its own widget -- `cmp_menu`, `notify`, `TelescopeResults`,
/// `TelescopePrompt` -- while noice's health float carries `markdown`,
/// which its `on_open` sets so the message *renders*, not to sign it
/// (`docs/surface-float-wire-capture.md`, the noice section). Taking that
/// as a name produces "view: markdown is drawing over the message area",
/// which names a document type as if it were a plugin, and mints a notice
/// family per content type on top.
///
/// A deny-list rather than an allow-list of widget filetypes because the
/// widget names are open-ended (every plugin invents its own) while the
/// document types a plugin sets to get rendering are a short, stable set.
/// A content type not listed here reads as a name until it is added; the
/// cost is one wrong word in one notice, against an allow-list's cost of
/// staying silent about every plugin nobody enumerated.
const CONTENT_FILETYPES: [&str; 5] = ["markdown", "help", "text", "man", "log"];

impl FloatSighting {
    /// The name this float carries for whatever opened it, or `None` when it
    /// carries none.
    ///
    /// Best-effort and bounded by design: a floating window records no
    /// authorship, so this reads the one mark a plugin sets on its own
    /// widget -- the buffer's filetype -- and never infers a plugin from
    /// geometry. A filetype naming what the buffer *holds* is not that mark
    /// ([`CONTENT_FILETYPES`]), and neither is the buffer's name: every float
    /// in `docs/surface-float-wire-capture.md` carries `name = ""`, and a
    /// plugin that floats a real file would put a path where a plugin's name
    /// belongs -- the same category error, plus a fresh claimant per file.
    /// A filetype that is not spelled the way filetypes are is refused as a
    /// name as well, and that is a trust boundary rather than tidiness: this
    /// string arrives off the wire and is interpolated into a notice family,
    /// which native notices are withdrawn by prefix match. A filetype
    /// carrying a space could spell another family exactly -- `a plugin`
    /// spells the anonymous one -- and a notice that retracts a different
    /// notice's line is a fact the user was told and then silently un-told.
    #[must_use]
    pub fn identity(&self) -> Option<&str> {
        let filetype = self.filetype.as_str();
        if filetype.is_empty() || CONTENT_FILETYPES.contains(&filetype) {
            return None;
        }
        if !filetype
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return None;
        }
        Some(filetype)
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

/// Whether this session actually draws `surface` and would fight a second
/// renderer for it: the table says [`Policy::Own`], and the `[native]`
/// switch behind its `ext_*` left it attached.
///
/// The one predicate a notice is gated on, wherever the notice comes from.
/// A surface view absorbs rather than owns is not a conflict, and one this
/// session handed back is not view's to complain about.
#[must_use]
pub fn view_draws(surface: Surface, model: &Model) -> bool {
    row(surface).is_some_and(|row| row.policy == Policy::Own && owned(surface, model).is_some())
}

/// Which identities have been seen claiming which surfaces, so a second
/// sighting adds to one notice rather than raising a second one.
///
/// One entry per distinct claiming identity, which
/// [`FloatSighting::identity`] bounds to widget filetypes: a session holds
/// as many as it has plugins drawing over an owned surface, and a plugin
/// floating one buffer after another adds none.
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
    /// Surfaces a named claimant's notice already accounts for. An unnamed
    /// float drawing on one of these adds nothing a user can act on -- same
    /// surface, same `[native]` line -- and a second box saying so is the
    /// two-notices-for-one-plugin case the spec forbids.
    named: Vec<Surface>,
}

/// One identity's standing claim.
#[derive(Debug)]
struct Claimant {
    identity: Option<String>,
    surfaces: Vec<Surface>,
    /// Whether a float of this identity was sighted during the scan now
    /// running; cleared by [`SurfaceConflicts::sweep`] at the end of each
    /// one, so a claimant that survives a sweep without being set was not
    /// on screen for that whole walk.
    seen: bool,
}

impl SurfaceConflicts {
    /// Records that `identity` was seen claiming `surface`, and answers the
    /// full set it claims when that set is *news* -- a claimant not standing
    /// before, or a surface it had not taken. `None` for the repeat sighting
    /// of a claim already recorded.
    ///
    /// The set is kept in [`SURFACES`] order rather than in the order the
    /// floats happened to arrive, so one identity claiming two surfaces
    /// reads the same way whichever it claimed first.
    ///
    /// News-only is safe here only because the line raised from it stands:
    /// it is sticky
    /// ([`record_native_notice_sticky_once`](crate::model::EngineModel::record_native_notice_sticky_once)),
    /// so no keystroke wipes it, and it is withdrawn by [`Self::sweep`] when
    /// the float stops being sighted -- which also drops the claimant, so a
    /// plugin that draws again is news again. The first shape of this
    /// answered every repeat instead, over a transient line, which raised
    /// the notice and re-raised it at the scan rate for as long as the user
    /// typed.
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
                    seen: false,
                });
                self.claimants.len() - 1
            }
        };
        let claimant = self.claimants.get_mut(index)?;
        claimant.seen = true;
        if claimant.surfaces.contains(&surface) {
            return None;
        }
        claimant.surfaces.push(surface);
        claimant
            .surfaces
            .sort_by_key(|surface| SURFACES.iter().position(|row| row.surface == *surface));
        Some(&claimant.surfaces)
    }

    /// Whether a named claimant's notice already accounts for `surface`, so
    /// an unnamed float sighted drawing there is a conflict the user has
    /// already been told about, with the same remedy.
    #[must_use]
    pub fn covered(&self, surface: Surface) -> bool {
        self.named.contains(&surface)
    }

    /// Records that a named claimant's notice now accounts for `surfaces`,
    /// and answers what that leaves of the anonymous claimant's standing
    /// claim: `None` when there is no anonymous claim or none of it was
    /// covered, `Some(&[])` when all of it was (its notice comes down), and
    /// `Some(rest)` when part of it survives (its notice is re-worded to
    /// the rest).
    ///
    /// The anonymous claimant is the one this can act on, and the only one.
    /// A float carrying a name is a different plugin making a different
    /// claim, and one notice per plugin is the rule rather than one notice
    /// per surface -- silencing the named one would lose a fact about a
    /// second plugin that the first plugin's notice never mentions. A float
    /// carrying no name, over a surface a named claimant has already been
    /// reported for, is the same fact told worse.
    pub fn cover(&mut self, surfaces: &[Surface]) -> Option<&[Surface]> {
        for surface in surfaces {
            if !self.named.contains(surface) {
                self.named.push(*surface);
            }
        }
        let index = self
            .claimants
            .iter()
            .position(|claimant| claimant.identity.is_none())?;
        let named = self.named.clone();
        let claimant = self.claimants.get_mut(index)?;
        let before = claimant.surfaces.len();
        claimant.surfaces.retain(|surface| !named.contains(surface));
        if claimant.surfaces.len() == before {
            return None;
        }
        if claimant.surfaces.is_empty() {
            self.claimants.remove(index);
            return Some(&[]);
        }
        Some(&self.claimants.get(index)?.surfaces)
    }

    /// Closes one scan: drops every claimant not sighted during it and
    /// answers their identities, so the caller can withdraw what it told the
    /// user about each. Called on [`crate::msg::Msg::FloatSweep`].
    ///
    /// A dropped claimant is forgotten entirely rather than remembered as
    /// "already told": the plugin drew, view said so, and the drawing
    /// stopped -- if it starts again the user is owed the line again, on a
    /// screen that no longer carries it.
    pub fn sweep(&mut self) -> Vec<Option<String>> {
        let mut gone = Vec::new();
        self.claimants.retain_mut(|claimant| {
            if std::mem::take(&mut claimant.seen) {
                return true;
            }
            gone.push(claimant.identity.clone());
            false
        });
        gone
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{
        claims, row, FloatAnchor, FloatSighting, Surface, SurfaceConflicts, CONTENT_FILETYPES,
        SURFACES,
    };
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

    /// The other half of the cmdline rule, and the half a rect alone cannot
    /// supply: view draws a command line only while one is open, so the same
    /// rows carry nothing of view's the rest of the time and a float sitting
    /// in them covers nothing. Dropping the conjunction turns every plugin
    /// that parks a window at the foot of the screen into a false report.
    #[test]
    fn a_float_on_the_cmdline_row_with_no_cmdline_open_claims_nothing() {
        let model = captured_session();
        assert!(
            model.owns(Ext::Cmdline),
            "the ownership gate must not be what answers here"
        );
        assert_eq!(claims(&cmp_cmdline_menu(), &model), None);
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

    /// Every identity the capture records, on both sides of the line: the
    /// four widget filetypes plugins invented for their own windows are
    /// names, and noice's `markdown` -- set so the message renders -- is not.
    /// The buffer name is never one either: the capture's own floats all
    /// carry `name = ""`, and the one shape that would produce a non-empty
    /// one is a plugin floating a real file, where the "identity" is a path.
    #[test]
    fn an_identity_is_a_widget_filetype_and_never_content_or_a_path() {
        for widget in ["cmp_menu", "notify", "TelescopeResults", "TelescopePrompt"] {
            assert_eq!(float(widget, 0, 0, 4, 4).identity(), Some(widget));
        }
        for content in CONTENT_FILETYPES {
            assert_eq!(
                float(content, 0, 0, 4, 4).identity(),
                None,
                "{content} says what the buffer holds, not who opened the window"
            );
        }
        let named = FloatSighting {
            name: "/tmp/scratch".to_string(),
            ..float("", 0, 0, 4, 4)
        };
        assert_eq!(named.identity(), None);
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
            None,
            "the repeat sighting is not news: the line it would raise is \
             sticky and still standing"
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

    /// What makes the standing line honest: a claimant sighted during a scan
    /// survives it, one that was not is gone and is answered so its notice
    /// can come down -- and it is forgotten, so the same plugin drawing
    /// again is news again rather than a claim nothing will ever say.
    #[test]
    fn a_sweep_drops_the_claimants_that_scan_did_not_sight() {
        let mut conflicts = SurfaceConflicts::default();
        let _ = conflicts.record(Some("cmp_menu"), Surface::Cmdline);
        let _ = conflicts.record(None, Surface::Messages);

        assert_eq!(
            conflicts.sweep(),
            Vec::<Option<String>>::new(),
            "both were sighted in the scan this closes"
        );

        // the next scan sees only one of them
        let _ = conflicts.record(Some("cmp_menu"), Surface::Cmdline);
        assert_eq!(
            conflicts.sweep(),
            vec![None],
            "the unnamed float has closed"
        );

        assert_eq!(
            conflicts.record(None, Surface::Messages),
            Some([Surface::Messages].as_slice()),
            "and it draws again: news, because the line about it came down"
        );
    }
}
