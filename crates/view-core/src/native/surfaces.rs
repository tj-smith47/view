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
    /// The `filetype`s this class's own floating windows present, which is
    /// what [`FloatSighting::identity`] reads a name out of. A sighting
    /// carrying one of these is this plugin drawing on a surface its own
    /// notice already names -- not a second plugin -- so the composition
    /// guard treats it exactly as it treats an anonymous float.
    ///
    /// Empty is the honest answer for a class whose windows carry no
    /// distinguishing filetype; those sightings reach the anonymous family
    /// and are absorbed there.
    pub identities: &'static [&'static str],
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
    // every window noice opens goes through its own nui view, which sets
    // `filetype = "noice"` on the buffer (lua/noice/view/nui.lua:41)
    identities: &["noice"],
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
    /// Whether the window's own `hide` flag is set, so it occupies its cells
    /// without drawing anything in them.
    ///
    /// Reported rather than filtered out at the scan, and the absorption is
    /// why: the one window view itself hides ([`Policy::Absorb`]) is the one
    /// whose rows view then has to keep reading as the candidate list
    /// narrows, and a scan that stopped reporting it the moment the hide
    /// landed would leave the palette holding the rows that stood at the
    /// keystroke the hide went out on. A hidden float draws nothing, so it
    /// is never a conflict to tell a user about -- that filter lives in
    /// `update::surface_conflict`, where the surface and the ownership are
    /// already known, rather than in a Lua chunk that knows neither.
    pub hidden: bool,
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

/// Whether a claim on `surface` is one view answers by taking the
/// claimant's rows into the palette rather than by telling the user about
/// it.
///
/// The filetypes a cmdline completion menu presents, and the whole of what
/// view will take a window over for.
///
/// An allow-list, which is the opposite discipline from
/// [`CONTENT_FILETYPES`]'s deny-list, because the two answers cost
/// different things. Getting a *name* wrong costs one wrong word in a
/// notice the user can read and act on. Getting a *taking* wrong costs the
/// user a window that stops drawing and, on the pinned engine, stays that
/// way through the plugin's own next reconfigure -- so absorption is gated
/// on what was measured rather than on what a rect suggests. The capture's
/// discriminator table has one column that reads "survives view's `hide`:
/// yes" and it is this one; every other float in it reads "not measured".
///
/// One row, on the same terms as [`SURFACE_CLAIMANTS`]: a menu no row names
/// takes the notice path instead, which is a line naming the plugin and the
/// `view.toml` switch that hands the command line back -- exactly what a
/// user got before any float was absorbed at all.
const COMPLETION_MENUS: [&str; 1] = ["cmp_menu"];

/// Whether a claim on `surface` by this float is one view answers by taking
/// the claimant's rows into the palette rather than by telling the user
/// about it.
///
/// Three conditions, and the float's own identity is the first of them.
/// The rect is not evidence: [`claims`] answers `Cmdline` for anything
/// whose bottom edge lands in the rows the command line keeps, and an LSP
/// progress spinner parked above the status line is in that band whenever a
/// user types `:w` (`compat/scenarios/fidget.toml` runs one at grid row
/// 28). Absorbing on the rect alone hides a window that is not a menu and
/// paints a diagnostic line into the palette as a completion candidate, so
/// the float has to present a completion menu's own identity
/// ([`COMPLETION_MENUS`]) before anything is taken. A float that claims the
/// command line without one is still a conflict and still gets its notice;
/// it is only never hidden.
///
/// Then the policy read, which is [`Surface::Popupmenu`]'s
/// ([`Policy::Absorb`], the same answer view already gives a
/// cmdline-sourced popupmenu that arrives on the wire): the rows a menu
/// draws belong to the *completion* surface rather than to the command line
/// the rect lands on -- [`claims`] names the cells, this names what was
/// drawn in them. And the ownership gate is that surface's own: `[native]
/// palette = false` detaches `ext_cmdline` and `ext_popupmenu` together, so
/// a session that handed the command line back absorbs nothing and hides
/// nobody's window.
///
/// Reading the table rather than matching on a variant is what keeps this
/// following the config: a surface whose row stops saying `Absorb` stops
/// being absorbed here, with no second place saying otherwise.
#[must_use]
pub fn absorbs(float: &FloatSighting, surface: Surface, model: &Model) -> bool {
    surface == Surface::Cmdline
        && float
            .identity()
            .is_some_and(|identity| COMPLETION_MENUS.contains(&identity))
        && row(Surface::Popupmenu).is_some_and(|row| row.policy == Policy::Absorb)
        && owned(Surface::Popupmenu, model).is_some()
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
    /// What the named claimant notices already account for. A float drawing
    /// on one of these surfaces, carrying no name or one of that claimant's
    /// own, adds nothing a user can act on -- same plugin, same surface,
    /// same `[native]` line -- and a second box saying so is the
    /// two-notices-for-one-plugin case the spec forbids.
    covers: Vec<Cover>,
}

/// One named claimant's accounted-for surfaces, with the identities its own
/// floats present.
///
/// Kept per claimant rather than as one flat surface set, because the
/// identity half is what makes the absorption *this* plugin's: a second
/// claimant covering the message area cannot make noice's own windows
/// anonymous, and a flat set could not tell the two apart.
#[derive(Debug)]
struct Cover {
    surfaces: Vec<Surface>,
    identities: &'static [&'static str],
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

    /// Whether a named claimant's notice already accounts for a float of
    /// `identity` drawing on `surface`, so the sighting is a conflict the
    /// user has already been told about, with the same remedy.
    ///
    /// `None` -- a float that names nobody -- is covered by any claimant
    /// holding the surface: the sighting cannot say who, and the standing
    /// notice can. A float that does name itself is covered only by the
    /// claimant whose own windows present that name
    /// ([`SurfaceClaimant::identities`]); any other name is a second plugin,
    /// whose line says something the first plugin's never does.
    #[must_use]
    pub fn covers(&self, surface: Surface, identity: Option<&str>) -> bool {
        self.covers.iter().any(|cover| {
            cover.surfaces.contains(&surface)
                && identity.is_none_or(|name| cover.identities.contains(&name))
        })
    }

    /// Records that a named claimant's notice now accounts for `surfaces`,
    /// drawn by floats presenting `identities`.
    ///
    /// Paired with [`Self::narrow`], which is what takes the notices already
    /// standing down to what this cover leaves of them.
    pub fn note_covered(&mut self, surfaces: &[Surface], identities: &'static [&'static str]) {
        if let Some(cover) = self
            .covers
            .iter_mut()
            .find(|cover| cover.identities == identities)
        {
            for surface in surfaces {
                if !cover.surfaces.contains(surface) {
                    cover.surfaces.push(*surface);
                }
            }
            return;
        }
        self.covers.push(Cover {
            surfaces: surfaces.to_vec(),
            identities,
        });
    }

    /// Answers what the recorded covers leave of `identity`'s standing
    /// claim: `None` when it has none or none of it was covered, `Some(&[])`
    /// when all of it was (its notice comes down), and `Some(rest)` when part
    /// of it survives (its notice is re-worded to the rest).
    pub fn narrow(&mut self, identity: Option<&str>) -> Option<&[Surface]> {
        let index = self
            .claimants
            .iter()
            .position(|claimant| claimant.identity.as_deref() == identity)?;
        let covered: Vec<Surface> = self
            .claimants
            .get(index)?
            .surfaces
            .iter()
            .copied()
            .filter(|surface| self.covers(*surface, identity))
            .collect();
        if covered.is_empty() {
            return None;
        }
        let claimant = self.claimants.get_mut(index)?;
        claimant
            .surfaces
            .retain(|surface| !covered.contains(surface));
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

/// How many times one window may be put back on screen after view has
/// hidden it before view stops absorbing it and says so instead.
///
/// The flash this bounds is real and cannot be designed away: between a
/// plugin's re-show and view's next observation of it there is a frame
/// carrying both chromes, and view cannot hold a window against its owner
/// -- nvim offers no lock. What it can do is stop after a bounded number of
/// them. Three, because two is a plugin that reconfigured its window twice
/// and a third is a plugin that is going to keep doing it; the wire capture
/// measured zero over 277 samples on the pinned versions, so this counter
/// is for the plugin and the engine nobody has run yet.
const MAX_RESHOWS: u8 = 3;

/// What one sighting of an absorbable float asks view to do.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AbsorbStep {
    /// Hide the window, then read its rows: the first sighting of a float
    /// view has not taken yet, and the re-hide after a tolerated re-show.
    HideThenRead,
    /// Read its rows and nothing else -- the window is already hidden, and
    /// what changes from here is the candidate list inside it.
    Read,
    /// Stop absorbing this one and tell the user about it instead: view
    /// hid it and it came back, or the hide never landed at all.
    Yield,
    /// Nothing at all: the first sighting of this window already carries
    /// somebody else's `hide`, so it is drawing nothing for view to take
    /// and nothing for view to report.
    Ignore,
}

/// What a reply carrying one float's rows leaves the palette holding.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RowsOutcome {
    /// The rows are view's to paint now. `changed` is whether they differ
    /// from what the palette already had, which is the only thing that owes
    /// a repaint: the scan re-reads a standing menu at its own cadence, and
    /// a frame per read of rows nobody moved is a paint loop keeping time
    /// with a plugin.
    Absorbed { changed: bool },
    /// The window was not hidden when its own rows were read, so the hide
    /// did not land -- an engine that does not take the flag, or a plugin
    /// that put the window straight back. Nothing is absorbed, nothing is
    /// painted twice, and the identity is handed back so the caller can
    /// raise the notice that says so.
    Yield(Option<String>),
    /// The reply names a window this session is not absorbing (a teardown
    /// overtook it, or it has already yielded), so there is nothing to do.
    Stale,
}

/// The floats view has taken over, and the rows it took off them.
///
/// Keyed on the window handle, which is the one identifier that is stable
/// for exactly as long as this state is: the capture measures one window id
/// reused across a whole cmdline session and never across two, which is
/// also the lifetime of an absorption ([`Self::forget`] runs at
/// `cmdline_hide`). Nothing here outlives the connection -- window handles
/// are per-session allocations, so a replacement engine's are somebody
/// else's numbers.
#[derive(Debug, Default)]
pub struct FloatAbsorption {
    windows: Vec<AbsorbedWindow>,
    /// The rows last read, and the window they came off, so a window that
    /// goes away takes its own rows with it and not another's.
    rows: Option<(u64, crate::native::palette::AbsorbedRows)>,
}

/// One float view has taken over.
#[derive(Debug)]
struct AbsorbedWindow {
    win: u64,
    /// What the float called itself when it was first sighted, kept so the
    /// notice a degrade raises names the same thing the notice would have
    /// named had view never absorbed it at all.
    identity: Option<String>,
    /// Whether a read has come back saying the hide actually landed. Until
    /// one has, a sighting that still shows the window is not yet evidence
    /// of anything: the reply is what nvim itself says about the flag, read
    /// after the hide ran, while a sighting is a scan that may have walked
    /// the window list before the hide was even written.
    confirmed: bool,
    reshows: u8,
    degraded: bool,
    /// Whether this window was sighted during the scan now running, on the
    /// same terms as [`Claimant::seen`].
    seen: bool,
}

impl FloatAbsorption {
    /// Answers one sighting of a float view may absorb.
    ///
    /// The cadence bound lives here: a hide goes out on the first sighting
    /// of a window and on nothing else until the plugin puts that window
    /// back, so a menu observed at the scan rate for a whole cmdline session
    /// costs one hide, not one per keystroke.
    ///
    /// `hidden` is read on the first sighting as well as on the ones after
    /// it, and it parts two different windows: one view hid (`confirmed`,
    /// and its rows are what the palette is painting) from one that was
    /// already hidden when view first saw it. The second is somebody else's
    /// -- a user's own config, another plugin -- and it is drawing nothing,
    /// so there is nothing to take and nothing to give back.
    pub fn observe(&mut self, win: u64, hidden: bool, identity: Option<&str>) -> AbsorbStep {
        let index = match self.windows.iter().position(|w| w.win == win) {
            Some(index) => index,
            None if hidden => return AbsorbStep::Ignore,
            None => {
                self.windows.push(AbsorbedWindow {
                    win,
                    identity: identity.map(str::to_owned),
                    confirmed: false,
                    reshows: 0,
                    degraded: false,
                    seen: true,
                });
                return AbsorbStep::HideThenRead;
            }
        };
        let Some(window) = self.windows.get_mut(index) else {
            return AbsorbStep::Yield;
        };
        window.seen = true;
        if window.degraded {
            return AbsorbStep::Yield;
        }
        if hidden || !window.confirmed {
            return AbsorbStep::Read;
        }
        window.reshows = window.reshows.saturating_add(1);
        if window.reshows >= MAX_RESHOWS {
            window.degraded = true;
            self.drop_rows(win);
            return AbsorbStep::Yield;
        }
        AbsorbStep::HideThenRead
    }

    /// Folds one read of a float's rows.
    pub fn rows_read(
        &mut self,
        win: u64,
        hidden: bool,
        rows: crate::native::palette::AbsorbedRows,
    ) -> RowsOutcome {
        let Some(window) = self.windows.iter_mut().find(|w| w.win == win) else {
            return RowsOutcome::Stale;
        };
        if window.degraded {
            return RowsOutcome::Stale;
        }
        if !hidden {
            window.degraded = true;
            let identity = window.identity.clone();
            self.drop_rows(win);
            return RowsOutcome::Yield(identity);
        }
        window.confirmed = true;
        let changed = !matches!(&self.rows, Some((owner, held)) if *owner == win && held == &rows);
        self.rows = Some((win, rows));
        RowsOutcome::Absorbed { changed }
    }

    /// The rows the palette paints, or `None` while view is absorbing
    /// nothing.
    #[must_use]
    pub fn rows(&self) -> Option<&crate::native::palette::AbsorbedRows> {
        self.rows.as_ref().map(|(_, rows)| rows)
    }

    /// Closes one float scan: drops every absorbed window the scan did not
    /// sight, and answers whether the palette's rows went with one of them.
    ///
    /// The teardown for the case the command line does not cover: a prefix
    /// with no candidates produces no window at all (the capture's `:zqx`),
    /// so the menu is gone while the command line the user is still typing
    /// stays open. Without this the palette would keep offering the
    /// candidates of a prefix that no longer has any.
    pub fn sweep(&mut self) -> bool {
        let mut dropped = Vec::new();
        self.windows.retain_mut(|window| {
            if std::mem::take(&mut window.seen) {
                return true;
            }
            dropped.push(window.win);
            false
        });
        let mut cleared = false;
        for win in dropped {
            // every dropped window, not the first one that owned the rows:
            // `any` would stop walking at it and leave the rest holding
            cleared |= self.drop_rows(win);
        }
        cleared
    }

    /// Forgets every absorption, and answers which windows view still owes
    /// an un-hide.
    ///
    /// The hide is view's to reverse, and the reversal is the last thing an
    /// absorption does. That nvim-cmp's own menu window is already gone by
    /// then -- it closes from a `CmdlineLeave` callback, which is also why
    /// no `WinClosed` announces it -- is a fact about one plugin, and view
    /// cannot see from here which one it is holding: a window this returns
    /// may be closed, and asking nvim to show a closed window is what the
    /// caller's chunk answers safely (it checks validity, and rides a
    /// `pcall`). What view must never do is stop tracking a window it hid
    /// while that window still exists, because the flag survives the
    /// plugin's own next reconfigure (the capture measured 277 samples with
    /// no re-show) and nothing else in the session will ever clear it.
    ///
    /// A window the scan stopped sighting is not in here to begin with:
    /// [`Self::sweep`] dropped it, which is the plugin having closed it and
    /// the one case where there is genuinely nothing to give back.
    ///
    /// Called when the command line closes and when a connection is
    /// replaced. The replacement discards the list rather than sending it:
    /// those handles were allocated in a session that no longer exists.
    #[must_use]
    pub fn forget(&mut self) -> Vec<u64> {
        self.rows = None;
        self.windows
            .drain(..)
            .map(|window| window.win)
            .collect::<Vec<_>>()
    }

    /// Drops the palette's rows if they came off `win`, and answers whether
    /// they did.
    fn drop_rows(&mut self, win: u64) -> bool {
        if self.rows.as_ref().is_some_and(|(owner, _)| *owner == win) {
            self.rows = None;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{
        claims, row, FloatAnchor, FloatSighting, Policy, Surface, SurfaceConflicts,
        CONTENT_FILETYPES, SURFACES,
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
            hidden: false,
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

    /// The two tables that decide what happens to one float must not name
    /// the same plugin: a claimant's own windows are covered by a notice
    /// that says which surfaces it took, and a window view also hides is a
    /// user reading that line beside a menu view quietly took over. The
    /// notice-side guard for it lives in `update::surface_conflict`, which
    /// answers per sighting; this is the guard that walks the population, so
    /// a row added to either table trips over it in a unit test rather than
    /// on somebody's screen.
    /// Every row of [`COMPLETION_MENUS`] read back out of the document that
    /// measured it, rather than trusted because somebody typed it here.
    ///
    /// Absorption is view taking a window away from the plugin that opened
    /// it, and the fact that makes that safe is one measurement:
    /// `docs/surface-float-wire-capture.md`'s discriminator table has a
    /// `survives view's hide` row, and it reads `yes` for exactly one
    /// column. A second completion plugin is welcome in the table -- after
    /// its own column is in the document with that cell answered, which is
    /// what this walk is here to insist on. A row added without the
    /// measurement fails here by name.
    #[test]
    fn every_absorbable_menu_is_one_the_capture_measured_a_hide_surviving() {
        const DOC: &str = include_str!("../../../../docs/surface-float-wire-capture.md");
        let row = |label: &str| -> Vec<String> {
            DOC.lines()
                .find(|line| line.starts_with(label))
                .map(|line| {
                    line.trim_matches('|')
                        .split('|')
                        .map(|cell| cell.trim().to_string())
                        .collect()
                })
                .expect("the capture doc must carry the discriminator table")
        };
        let filetypes = row("| buffer `filetype` |");
        let survives = row("| survives view's `hide` |");
        assert_eq!(
            filetypes.len(),
            survives.len(),
            "the two rows describe the same columns or neither says anything \
             about the other"
        );
        for menu in super::COMPLETION_MENUS {
            let column = filetypes
                .iter()
                .position(|cell| cell.contains(&format!("`{menu}`")))
                .unwrap_or_else(|| {
                    unreachable!(
                        "{menu} is absorbed but names no column of the capture's \
                         discriminator table"
                    )
                });
            assert!(
                survives
                    .get(column)
                    .is_some_and(|cell| cell.contains("yes")),
                "{menu} is absorbed on a column the capture never measured a \
                 hide surviving: {:?}",
                survives.get(column)
            );
        }
    }

    /// What a matrix cell says when the population it walks is empty.
    ///
    /// ASCII dashes: `scripts/check-style.sh` bans the em-dash outright in
    /// source and in `docs/`, and a marker that cannot be written on the
    /// page it is compared against is no marker at all.
    const NONE_CELL: &str = "-- none --";

    /// `cells` as one table cell, or [`NONE_CELL`] when there are none --
    /// which is the whole point of the page: a surface nothing claims and a
    /// surface no state proves read as gaps rather than as blanks.
    fn or_none(cells: &[String]) -> String {
        if cells.is_empty() {
            NONE_CELL.to_string()
        } else {
            cells.join(", ")
        }
    }

    /// Every `scenario`/`state` whose probes assert `ext`'s attach, in
    /// scenario-file then state order.
    ///
    /// A probe naming the option is what proves a row: the attach decides
    /// whether view draws the surface at all, so a state asserting it on
    /// proves the policy and one asserting it off proves the `[native]`
    /// line that hands it back. Read out of the scenario files themselves
    /// rather than written down here, so a state that is deleted or renamed
    /// takes its citation with it instead of leaving the page naming
    /// evidence that no longer runs.
    ///
    /// Comment lines are skipped even when they name the option: several
    /// scenarios explain in prose which `ext_*` their config detaches, and
    /// prose is not an assertion.
    fn proving_states(ext: Ext) -> Vec<String> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat/scenarios");
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("compat/scenarios must be readable")
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.extension().is_some_and(|suffix| suffix == "toml"))
            .collect();
        files.sort();
        let mut found: Vec<String> = Vec::new();
        for path in files {
            let scenario = path
                .file_stem()
                .expect("a scenario file has a stem")
                .to_string_lossy()
                .into_owned();
            let text = std::fs::read_to_string(&path).expect("a scenario must be readable");
            let mut state: Option<String> = None;
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("name = \"") {
                    state = rest.split('"').next().map(str::to_owned);
                    continue;
                }
                let trimmed = line.trim_start();
                if trimmed.starts_with('#')
                    || !trimmed.contains("probe = ")
                    || !trimmed.contains(ext.as_str())
                {
                    continue;
                }
                if let Some(state) = &state {
                    let cell = format!("`{scenario}`/`{state}`");
                    if !found.contains(&cell) {
                        found.push(cell);
                    }
                }
            }
        }
        found
    }

    /// The ownership matrix as `docs/surface-ownership.md` carries it, on
    /// the pattern `docs/keymaps.md` already uses: the page is generated
    /// from the tables in this module plus the loaded scenario set, and the
    /// test below fails when the two disagree.
    ///
    /// Test-only, and private with it, for the reason `render_review_table`
    /// is: the page carries the rendered block, and nothing but the drift
    /// check needs to render it again.
    fn render_matrix() -> String {
        let mut out = String::from(
            "| surface | `ext_*` option | policy | `[native]` switch that hands it back \
             | claiming plugin classes | proving scenario / state |\n\
             | --- | --- | --- | --- | --- | --- |\n",
        );
        for table_row in SURFACES {
            let ext = table_row.ext.map_or_else(
                || NONE_CELL.to_string(),
                |ext| format!("`{}`", ext.as_str()),
            );
            let remedy = table_row
                .remedy
                .map_or_else(|| NONE_CELL.to_string(), |line| format!("`{line}`"));
            let claimants: Vec<String> = super::SURFACE_CLAIMANTS
                .iter()
                .filter(|claimant| claimant.surfaces.contains(&table_row.surface))
                .map(|claimant| {
                    let identities: Vec<String> = claimant
                        .identities
                        .iter()
                        .map(|identity| format!("`{identity}`"))
                        .collect();
                    format!("`{}` ({})", claimant.class, or_none(&identities))
                })
                .collect();
            let proving = table_row.ext.map(proving_states).unwrap_or_default();
            out.push_str(&format!(
                "| {} | {ext} | `{:?}` | {remedy} | {} | {} |\n",
                table_row.label,
                table_row.policy,
                or_none(&claimants),
                or_none(&proving),
            ));
        }
        out
    }

    /// The sentence the matrix owes about absorption, which no policy
    /// column can carry on its own: the command line's own policy is
    /// [`Policy::Own`], and the taking is decided one row down --
    /// [`Surface::Popupmenu`]'s [`Policy::Absorb`] read against the
    /// float's own filetype ([`super::absorbs`]). Generated from both
    /// rows and from the menu list, so a policy that changes rewrites the
    /// sentence rather than leaving it asserting the old arrangement.
    fn render_absorb_note() -> String {
        let menus: Vec<String> = super::COMPLETION_MENUS
            .iter()
            .map(|menu| format!("`{menu}`"))
            .collect();
        format!(
            "A float whose rows land in the command line's band is taken into the palette \
             instead of being reported, but only when it presents a completion menu's own \
             filetype ({}). That is the completion menu's `{:?}` read at the moment of the \
             claim; the command line's own policy stays `{:?}`.",
            or_none(&menus),
            row(Surface::Popupmenu)
                .expect("the completion menu has a row")
                .policy,
            row(Surface::Cmdline)
                .expect("the command line has a row")
                .policy,
        )
    }

    /// The page a user reads instead of this module, pinned to what the
    /// module actually does. A policy, a switch, a claimant or a proving
    /// state that changes here and not there fails naming the row that
    /// drifted.
    #[test]
    fn the_surface_matrix_page_matches_the_policy_table() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/surface-ownership.md");
        let page =
            std::fs::read_to_string(path).expect("docs/surface-ownership.md must be readable");
        let matrix = render_matrix();
        for line in matrix.lines().skip(2) {
            assert!(
                page.contains(line),
                "docs/surface-ownership.md is stale, this row drifted:\n{line}"
            );
        }
        assert!(
            page.contains(&matrix),
            "docs/surface-ownership.md is stale, it must carry:\n{matrix}"
        );
        let note = render_absorb_note();
        assert!(
            page.contains(&note),
            "docs/surface-ownership.md is stale, it must carry:\n{note}"
        );
    }

    /// A surface view draws either names the `view.toml` line that hands it
    /// back or says it has none, and the page says which. The tab line is
    /// the honest none: view owns it unconditionally and no `[native]`
    /// switch reaches it, so a notice about it says what happened and stops
    /// rather than naming a setting that does not exist.
    #[test]
    fn every_owned_surface_names_its_off_switch_or_says_it_has_none() {
        let matrix = render_matrix();
        for table_row in SURFACES.iter().filter(|row| row.policy != Policy::Yield) {
            assert!(
                table_row
                    .remedy
                    .is_none_or(|line| line.starts_with("[native] ") && line.ends_with(" = false")),
                "{}'s switch is not a [native] line a user can paste: {:?}",
                table_row.label,
                table_row.remedy
            );
            let cell = table_row
                .remedy
                .map_or_else(|| NONE_CELL.to_string(), |line| format!("`{line}`"));
            assert!(
                matrix.contains(&format!("| {cell} |")),
                "{} renders no off-switch cell",
                table_row.label
            );
        }
        assert_eq!(
            row(Surface::Tabline)
                .expect("the tab line has a row")
                .remedy,
            None,
            "the tab line is the matrix's honest none row"
        );
        assert!(
            matrix.contains(NONE_CELL),
            "the none marker never renders, so a coverage gap could not be seen"
        );
    }

    #[test]
    fn no_claimant_names_an_absorbable_identity() {
        for claimant in super::SURFACE_CLAIMANTS {
            for identity in claimant.identities {
                assert!(
                    !super::COMPLETION_MENUS.contains(identity),
                    "{} presents {identity}, which is also in COMPLETION_MENUS: \
                     a claimant's float is reported, never taken",
                    claimant.class
                );
            }
        }
    }
}
