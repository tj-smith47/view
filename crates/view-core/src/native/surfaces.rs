//! The surfaces view draws itself, what it does when something else draws
//! over one of them, and the arithmetic that decides whether a floating
//! window is doing exactly that.
//!
//! The set view claims is fixed and small, so the conflict class is
//! decidable from the grid alone. What a float actually carries is recorded
//! live in `docs/surface-float-wire-capture.md`, and two of its findings
//! shape everything here:
//!
//! - **Rect overlap with what view paints answers backwards.** Read
//!   against view's own palette box, a cmdline completion menu on the grid's
//!   last two rows misses it entirely while a picker filling rows 1..26
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
//! Nothing here does I/O or allocates per observation: [`claims`] is
//! integer arithmetic over one rect and the grid's size.

use crate::model::Model;
use crate::native::channels::{Channel, Region};
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
    /// The status line, which view draws itself once nvim has stopped.
    Statusline,
    /// The buffer grid, which view never draws over: nvim owns it, and so
    /// does anything that wants to float above it.
    Grid,
}

impl Surface {
    /// The `[native]` feature whose switch decides whether view draws this
    /// surface, where one decides it.
    ///
    /// Read off the table rather than matched here, so a row states its
    /// gate once and the takeover, the notice and the generated page all
    /// read the same answer.
    #[must_use]
    pub fn feature(self) -> Option<&'static str> {
        row(self).and_then(|row| row.feature)
    }
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
    /// The `[native]` feature whose switch decides whether view draws it,
    /// or `None` for a surface no switch reaches.
    ///
    /// Equal to [`Ext::feature`] for every row an attach carries
    /// (`every_owned_surface_names_the_switch_its_attach_is_gated_on`); a
    /// row with no `ext` carries it alone, which is how a surface nvim
    /// gives up through an option rather than through a capability states
    /// its gate at all.
    pub feature: Option<&'static str>,
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
        feature: Some("palette"),
        policy: Policy::Own,
        label: "the command line",
        remedy: Some("[native] palette = false"),
    },
    OwnedSurface {
        surface: Surface::Popupmenu,
        ext: Some(Ext::Popupmenu),
        feature: Some("palette"),
        policy: Policy::Own,
        label: "the completion menu",
        remedy: Some("[native] palette = false"),
    },
    OwnedSurface {
        surface: Surface::Messages,
        ext: Some(Ext::Messages),
        feature: Some("notifications"),
        policy: Policy::Own,
        label: "the message area",
        remedy: Some("[native] notifications = false"),
    },
    OwnedSurface {
        surface: Surface::Tabline,
        ext: Some(Ext::Tabline),
        feature: Some("tabline"),
        policy: Policy::Own,
        label: "the tab line",
        remedy: Some("[native] tabline = false"),
    },
    OwnedSurface {
        surface: Surface::Statusline,
        ext: None,
        feature: Some("statusline"),
        policy: Policy::Own,
        label: "the status line",
        remedy: Some("[native] statusline = false"),
    },
    OwnedSurface {
        surface: Surface::Grid,
        ext: None,
        feature: None,
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
/// anchor places a corner-pinned toast off the grid entirely (the wire
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
    /// The buffer's filetype: the one identifying mark most floats carry,
    /// and empty for the rest.
    pub filetype: String,
    /// The buffer's name, empty for the unfiled scratch buffer nearly every
    /// float uses.
    pub name: String,
    /// Whether the window's own `hide` flag is set, so it occupies its cells
    /// without drawing anything in them.
    ///
    /// Reported rather than filtered out at the scan: a hidden float draws
    /// nothing, so it is never a conflict to tell a user about, and that
    /// filter lives in `update::surface_conflict`, where the surface and
    /// the ownership are already known, rather than in a Lua chunk that
    /// knows neither.
    pub hidden: bool,
}

/// Filetypes that say what a buffer holds, never who opened the window.
///
/// The capture's discriminator table parts the two kinds cleanly. A widget
/// carries a filetype its author invented for that widget alone, while a
/// float rendering a document carries the document's own type --
/// `markdown` on a health report, set so the text *renders*
/// (`docs/surface-float-wire-capture.md`). Taking that as a name produces
/// "view: markdown is drawing over the message area", which names a
/// document type as if it were the thing that opened the window, and mints
/// a notice family per content type on top.
///
/// A deny-list rather than an allow-list of widget filetypes because the
/// widget names are open-ended while the document types set to get
/// rendering are a short, stable set. A content type not listed here reads
/// as a name until it is added; the cost is one wrong word in one notice,
/// against an allow-list's cost of staying silent about every window
/// nobody enumerated.
const CONTENT_FILETYPES: [&str; 5] = ["markdown", "help", "text", "man", "log"];

impl FloatSighting {
    /// The name this float carries for whatever opened it, or `None` when it
    /// carries none.
    ///
    /// A name for the notice to quote and nothing view decides on: what a
    /// float is doing is read off the region it covers, so this only
    /// answers what to call it. A floating window records no authorship,
    /// so the mark read here is the one its author set on the widget's own
    /// buffer -- the filetype. A filetype naming what the buffer *holds*
    /// is not that mark ([`CONTENT_FILETYPES`]), and neither is the
    /// buffer's name: every float in `docs/surface-float-wire-capture.md`
    /// carries `name = ""`, and a window floating a real file would put a
    /// path where a name belongs, plus a fresh family per file. A filetype
    /// that is not spelled the way filetypes are is refused as a name as
    /// well, and that is a trust boundary rather than tidiness: this string
    /// arrives off the wire and is interpolated into a notice family, which
    /// native notices are withdrawn by prefix match. A filetype carrying a
    /// space could spell another family exactly -- `a plugin` spells the
    /// anonymous one -- and a notice that retracts a different notice's
    /// line is a fact the user was told and then silently un-told.
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
}

/// The inclusive `(top, left, bottom, right)` cell span a float covers,
/// with its anchor resolved and every edge clamped into a `grid_w` by
/// `grid_h` grid.
///
/// `None` for a float covering no cells at all -- a zero width or height,
/// or a rect entirely off the grid -- which claims nothing by definition.
fn span(
    row: i64,
    col: i64,
    width: u16,
    height: u16,
    anchor: FloatAnchor,
    grid_w: u16,
    grid_h: u16,
) -> Option<(i64, i64, i64, i64)> {
    if width == 0 || height == 0 || grid_w == 0 || grid_h == 0 {
        return None;
    }
    let width = i64::from(width);
    let height = i64::from(height);
    let (top, left) = match anchor {
        FloatAnchor::NorthWest => (row, col),
        FloatAnchor::NorthEast => (row, col - width),
        FloatAnchor::SouthWest => (row - height + 1, col),
        FloatAnchor::SouthEast => (row - height + 1, col - width),
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

/// How many rows at the bottom of the grid belong to the command line: the
/// row nvim itself would draw one on, plus the row above it that a plugin
/// drawing a cmdline completion holds back for it. A menu that leaves the
/// command line visible clamps its own height to keep exactly one, which
/// is why such a menu's bottom edge lands on the second-to-last grid row
/// rather than the last one.
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
/// - **the command line**, when a command line is on screen and the float's
///   bottom edge lands in the rows the engine keeps for it. The command
///   line is what parts a cmdline completion menu from a picker whose
///   lowest chrome window sits one row above the same band. A command line
///   view is speculating counts here, and the notice does not rest on it:
///   a line naming what took the command line waits for the
///   `cmdline_show` that makes it true (`update::surface_conflict`'s
///   `observe_float`), so a guess and a window drawing under it can share
///   the screen until the answer lands.
/// - **the message area**, when the float is pinned to the grid's top
///   right corner -- where view stacks its toasts -- and is short enough to
///   be chrome rather than a screenful. A picker centered in the grid
///   overlaps that corner without being pinned to it, which is the
///   distinction the negative control turns on.
#[must_use]
pub fn claims(float: &FloatSighting, model: &Model) -> Option<Surface> {
    claims_at(
        float.row,
        float.col,
        float.width,
        float.height,
        float.anchor,
        model,
    )
}

/// [`claims`] for a float known only by the box it occupies: the rect a
/// `win_float_pos` resolved and the size of the grid behind it, which is
/// everything the placement event carries and everything the rules below
/// read.
///
/// The identity half of a sighting has no bearing here -- [`claims`] never
/// consulted it -- so the placement answers the same surface the scan's own
/// sighting of the same window will, one round trip earlier.
#[must_use]
pub fn claims_at(
    row: i64,
    col: i64,
    width: u16,
    height: u16,
    anchor: FloatAnchor,
    model: &Model,
) -> Option<Surface> {
    let (grid_w, grid_h) = model.engine.grid().size();
    let (top, _left, bottom, right) = span(row, col, width, height, anchor, grid_w, grid_h)?;
    let last_row = i64::from(grid_h) - 1;
    // half the grid: the bound between a piece of chrome pinned in a corner
    // and a window that has taken the screen over, which is a different
    // thing and not this detector's business
    let chrome_rows = i64::from(grid_h) / 2;
    let rows = bottom - top + 1;
    let hit = crate::native::channels::CHANNELS.iter().find(|entry| {
        entry.channels.iter().any(|channel| match channel {
            Channel::Float(Region::CmdlineBand) => {
                model.engine.paints_cmdline() && bottom >= last_row - (CMDLINE_ROWS - 1)
            }
            Channel::Float(Region::TopRightChrome) => {
                right == i64::from(grid_w) - 1 && top < chrome_rows && rows <= chrome_rows
            }
            _ => false,
        })
    })?;
    owned(hit.surface, model)
}

/// `surface` if this session draws it, `None` otherwise -- the gate that
/// makes the detector follow the `[native]` switches instead of a
/// constant.
///
/// Two ways a session answers, and the row says which it takes. A surface
/// nvim gives up at `nvim_ui_attach` is drawn by whoever holds the
/// capability ([`Model::owns`]). A surface nvim gives up through an option
/// carries no capability at all, so its `[native]` switch is the whole
/// answer -- read straight off the session
/// ([`Model::feature_switch`]), because a row like that read through
/// `ext` answers `false` for every session and a hold issued for it would
/// take the user's chrome with nothing said.
fn owned(surface: Surface, model: &Model) -> Option<Surface> {
    let row = row(surface)?;
    let drawn = match row.ext {
        Some(ext) => model.owns(ext),
        None => model.feature_switch(row.feature?)?,
    };
    drawn.then_some(surface)
}

/// Whether this session actually draws `surface` and would fight a second
/// renderer for it: the table says [`Policy::Own`], and the row's own
/// `[native]` answer ([`owned`]) left it with view.
///
/// The one predicate a notice is gated on, wherever the notice comes from.
/// A surface this session handed back is not view's to complain about.
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
    /// The surfaces a channel report has already found populated by a
    /// holder that is not view ([`Self::note_channel_held`]). A float
    /// drawing on one of them is the same conflict the channel notice
    /// already names, with the same `[native]` line, and a second box
    /// saying so is one conflict counted twice.
    held: Vec<Surface>,
    /// The floating windows the take-down has already asked rows of,
    /// whether or not the answer was filed, so a scan that sights one again
    /// before its close lands does not record it twice. Each carries the
    /// bar its reply is held to, decided at the sighting. Emptied only by
    /// [`SurfaceConflicts::forget_engine`]: within one engine the handles
    /// stay valid names for windows that are gone, and the set is bounded
    /// by the floats one startup opens, but a replacement process issues
    /// handles from 1000 again.
    complaints: Vec<Complaint>,
    /// Whether the user has acted -- a key, a click or a paste -- which is
    /// where spec 5.5 ends the startup conflict window.
    typed: bool,
    /// Whether a channel of a surface view draws was found held this
    /// session and is still inside the grace that finding armed, during
    /// which a complaint the holder raises is taken down even though the
    /// user has acted.
    ///
    /// The window a keystroke closes is the wrong bound for a renderer
    /// that raises its complaint on a timer of its own: a health check
    /// re-running every second raises the one about view holding
    /// `vim.notify` several seconds in, which is past any realistic first
    /// keystroke. What the grace does not relax is what may be taken: a
    /// float inside it is closed only when its rows read as a complaint
    /// ([`SurfaceConflicts::reads_as_complaint`]), so a window the user
    /// opened over the message area is left standing.
    complaint_grace: bool,
    /// Whether the reading of the message area's replaced global has
    /// arrived yet ([`crate::msg::Msg::NotifySinkRead`]).
    ///
    /// Before it does, view knows nothing about who stands at
    /// `vim.notify`, so a float landing on the message area cannot be told
    /// from that holder's own -- and the reading's round trip is short
    /// enough that the answer is worth waiting for and long enough that a
    /// timer firing at a fixed offset from `VimEnter` can beat it (a remote
    /// link left tens of milliseconds between the two). A float placed
    /// while this is false is held in `sink_holds` and classified by the
    /// reading.
    sink_read: bool,
    /// The floats held off the screen only because that reading had not
    /// arrived when they were placed, and the grid each draws into.
    /// Drained by [`SurfaceConflicts::read_sink`], which is what puts them
    /// through the classification a float over a held channel takes at its
    /// placement.
    sink_holds: Vec<(u64, crate::grid::registry::GridId)>,
    /// Which engine the deadlines armed this session belong to. A timer
    /// thread sleeping on a dead engine's behalf still wakes, and the
    /// expiry it sends names this value as it was when the deadline was
    /// armed: bumped by [`SurfaceConflicts::forget_engine`], so the
    /// replacement answers to no deadline but its own. An expiry effect's
    /// generation is checked by the method that consumes it
    /// ([`SurfaceConflicts::end_complaint_grace`],
    /// [`Model::expire_startup_hold`](crate::model::Model::expire_startup_hold)),
    /// never at the dispatch site.
    generation: u64,
}

/// One float the take-down has claimed, and the bar its rows are held to.
#[derive(Debug)]
struct Complaint {
    win: u64,
    /// Whether the sighting alone qualified it: inside the startup window a
    /// float over a covered surface is a complaint by construction, and the
    /// rows are filed unread. Decided when the read is asked for, never
    /// when it is answered -- the user can act inside that round trip, and
    /// a bar re-read at the reply would drop a complaint the sighting had
    /// already qualified.
    unconditional: bool,
    /// The grid this float draws into, once a placement event has named
    /// one. `None` for a float the scan sighted without view having seen
    /// its `win_float_pos` -- one already on screen when the channel
    /// report landed -- which is a window nothing is holding back.
    grid: Option<crate::grid::registry::GridId>,
    /// Whether the rows came back reading as something the user opened, so
    /// the window is theirs again. The claim itself stays: it is what keeps
    /// the scan's next sighting of the same window from asking for the rows
    /// a second time.
    released: bool,
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

    /// Whether a channel notice already accounts for a float drawing on
    /// `surface`, so the sighting is a conflict the user has been told
    /// about with the same remedy.
    ///
    /// Read off what the channel audit found rather than off any name the
    /// float carries: a window over a surface whose channel is populated by
    /// a holder that is not view is that holder drawing, whatever its
    /// buffer happens to call itself.
    #[must_use]
    pub fn channel_held(&self, surface: Surface) -> bool {
        self.held.contains(&surface)
    }

    /// Records that a channel of `surface` was found populated by a holder
    /// that is not view, and answers whether that is news for this
    /// surface.
    ///
    /// News-only because the grace is armed from it: a second channel of
    /// the same surface reporting later must not re-arm a deadline that is
    /// already running, or the first deadline's expiry ends a grace the
    /// second report had just extended.
    pub fn note_channel_held(&mut self, surface: Surface) -> bool {
        if self.held.contains(&surface) {
            return false;
        }
        self.held.push(surface);
        true
    }

    /// Drops every surface a channel report has accounted for from the
    /// standing float claims, and answers each claim it changed with what
    /// is left of it.
    ///
    /// An empty rest is a claim the channel notice now covers whole, so the
    /// float line comes down; a shorter one is re-worded to the rest. A
    /// claimant left with nothing is forgotten, so a float drawing later on
    /// a surface no report covers is news again.
    pub fn narrow_to_held(&mut self) -> Vec<(Option<String>, Vec<Surface>)> {
        let held = self.held.clone();
        let mut narrowed = Vec::new();
        self.claimants.retain_mut(|claimant| {
            let before = claimant.surfaces.len();
            claimant.surfaces.retain(|surface| !held.contains(surface));
            if claimant.surfaces.len() == before {
                return true;
            }
            narrowed.push((claimant.identity.clone(), claimant.surfaces.clone()));
            !claimant.surfaces.is_empty()
        });
        narrowed
    }

    /// Claims `win`'s text for the notification history, and answers
    /// whether this is the first claim on it.
    ///
    /// The float scan re-sights a standing window at its own cadence, and
    /// the close that follows the read is a notification with no reply, so
    /// a plugin whose complaint outlives one scan would otherwise have its
    /// lines recorded once per scan until the window went.
    ///
    /// The bar the reply is held to is fixed here, from whether the startup
    /// window is still open at the sighting
    /// ([`Self::claimed_unconditionally`]).
    pub fn claim_complaint(
        &mut self,
        win: u64,
        grid: Option<crate::grid::registry::GridId>,
    ) -> bool {
        if self.is_complaint(win) {
            return false;
        }
        self.complaints.push(Complaint {
            win,
            unconditional: self.startup_window_open(),
            grid,
            released: false,
        });
        true
    }

    /// Notes the grid `win`'s float draws into and answers whether view is
    /// still holding that float off the screen.
    ///
    /// The grid is re-noted on every call because a plugin animating its
    /// window sends a placement per step, and the flag has to follow the
    /// window rather than the position it was withheld at.
    pub fn withholds_float(&mut self, win: u64, grid: crate::grid::registry::GridId) -> bool {
        let Some(complaint) = self.complaints.iter_mut().find(|c| c.win == win) else {
            return false;
        };
        complaint.grid = Some(grid);
        !complaint.released
    }

    /// Gives `win`'s float back to the screen, and answers which grid to
    /// paint again.
    ///
    /// `None` when nothing was being held: a float the scan sighted before
    /// any placement named its grid, or one already released.
    pub fn release_complaint(&mut self, win: u64) -> Option<crate::grid::registry::GridId> {
        let complaint = self.complaints.iter_mut().find(|c| c.win == win)?;
        if std::mem::replace(&mut complaint.released, true) {
            return None;
        }
        complaint.grid
    }

    /// Holds `win`'s float off the screen because the message area's
    /// replaced global has not been read yet, and answers whether it is now
    /// held.
    ///
    /// `false` once that reading has arrived: from then on what the audit
    /// found held is the whole test, and a float this says `false` about
    /// paints on the frame it arrived for.
    pub fn hold_for_sink_read(&mut self, win: u64, grid: crate::grid::registry::GridId) -> bool {
        if self.sink_read {
            return false;
        }
        match self.sink_holds.iter_mut().find(|held| held.0 == win) {
            // a window being animated sends a placement per step
            Some(held) => held.1 = grid,
            None => self.sink_holds.push((win, grid)),
        }
        true
    }

    /// Marks the reading arrived and hands back every float held for it,
    /// for the caller to classify now that the holder is known.
    pub fn read_sink(&mut self) -> Vec<(u64, crate::grid::registry::GridId)> {
        self.sink_read = true;
        std::mem::take(&mut self.sink_holds)
    }

    /// Whether `win`'s rows were read for the notification history rather
    /// than for the palette, which is what parts one `Msg::FloatRows` reply
    /// from the other.
    #[must_use]
    pub fn is_complaint(&self, win: u64) -> bool {
        self.complaints.iter().any(|complaint| complaint.win == win)
    }

    /// Whether `win` was sighted inside the startup window, where its rows
    /// are filed without being read for a signature. `false` for a window
    /// never claimed, which is a reply nothing is waiting on.
    #[must_use]
    pub fn claimed_unconditionally(&self, win: u64) -> bool {
        self.complaints
            .iter()
            .any(|complaint| complaint.win == win && complaint.unconditional)
    }

    /// The engine every deadline armed from now on belongs to, carried in
    /// the arming effect and echoed back in its expiry.
    #[must_use]
    pub fn engine_generation(&self) -> u64 {
        self.generation
    }

    /// Notes that the user has acted -- a key, a click or a paste --
    /// closing the startup conflict window for good.
    pub fn note_user_acted(&mut self) {
        self.typed = true;
    }

    /// Opens the complaint grace, and answers whether this call is what
    /// opened it.
    ///
    /// Answered rather than assumed: a second channel found held later
    /// must not re-arm a deadline that is already running, or the first
    /// deadline's expiry ends a grace the second report had just extended.
    /// The grace runs from the first report of a held channel.
    pub fn arm_complaint_grace(&mut self) -> bool {
        if self.complaint_grace {
            return false;
        }
        self.complaint_grace = true;
        true
    }

    /// Whether a complaint raised now is still inside the grace.
    #[must_use]
    pub fn within_complaint_grace(&self) -> bool {
        self.complaint_grace
    }

    /// Closes the grace, on the deadline the arming scheduled -- and only
    /// that one: an expiry carrying another engine's `generation` was armed
    /// against a report this engine never made, and closing on it would end
    /// the replacement's grace early.
    pub fn end_complaint_grace(&mut self, generation: u64) {
        if generation == self.generation {
            self.complaint_grace = false;
        }
    }

    /// Whether the lines a float was drawing read as a complaint about the
    /// UI view took over, rather than as something the user opened.
    ///
    /// The discriminator is view's own vocabulary appearing in somebody
    /// else's text: the `ext_*` capability names ([`Ext::as_str`]) are what
    /// a GUI takes and what a renderer names when it says it cannot work,
    /// and `vim.notify` is the one function a takeover re-points, which is
    /// the other thing such a window reports as broken. Derived from the
    /// enum rather than written down, so a surface added later is matched
    /// without a second list to remember.
    ///
    /// Only consulted for a float sighted after the user has acted: inside
    /// the startup window every complaint over a surface whose channel was
    /// found held is taken, text unread, and which bar applies is fixed at
    /// the sighting ([`Self::claim_complaint`]). This is the narrower bar
    /// the grace runs under, and the cost of getting it wrong is a window
    /// closed under someone's hand. The cost the other way is accepted and
    /// pinned: a window the user opened that quotes one of these names --
    /// a log view listing the health error -- reads as a complaint inside
    /// the grace and is filed and closed.
    #[must_use]
    pub fn reads_as_complaint(lines: &[String]) -> bool {
        lines.iter().any(|line| {
            line.contains("vim.notify")
                || crate::native::ext::ALL_MULTIGRID
                    .iter()
                    .any(|ext| line.contains(ext.as_str()))
        })
    }

    /// Drops what a replacement engine invalidates, called beside
    /// [`EngineModel::forget_overlays`](crate::model::EngineModel::forget_overlays)
    /// from the restart.
    ///
    /// | field | why |
    /// | --- | --- |
    /// | `complaints` | window handles, and a fresh process issues them from 1000 again: a handle held past the death names one of the replacement's own windows, and the reply to a read of it would file a live window's rows into the history and close it |
    /// | `typed` | the replacement sources the config again, so whatever drew over a surface draws again, and a session that had been typed at would leave those windows stacked beside the re-raised notice |
    /// | `complaint_grace` | a deadline armed against the dead engine's report, and the replacement's own report arms its own |
    /// | `sink_read`, `sink_holds` | the attach re-reads the message area's replaced global per engine, so the replacement is back inside the window where a float over a native surface is held until that reading lands; the held handles belong to the dead process |
    /// | `generation` | bumped: the dead engine's deadlines are still sleeping in their timer threads, and their expiries must find nobody to answer to |
    /// | `claimants` | kept: a claimant is named by identity, not by handle, and the same config draws the same windows -- forgetting it would raise a second notice per window for one conflict |
    /// | `held` | cleared: the replacement re-reports its own channels, and a surface left in here would swallow that report as a conflict already accounted for |
    pub fn forget_engine(&mut self) {
        self.complaints.clear();
        self.typed = false;
        self.complaint_grace = false;
        self.sink_read = false;
        self.sink_holds.clear();
        self.held.clear();
        self.generation += 1;
    }

    /// Whether the startup conflict window is still open, which is spec
    /// 5.5's own bound: everything before the first key, click or paste.
    ///
    /// Deliberately not the startup *hold*
    /// ([`StartupHold`](crate::native::toast::StartupHold)), which shares
    /// the first-act end but also ends on a three-second deadline of
    /// its own -- a bound on how long a message may be parked, not on how
    /// long a launch lasts. A heavy configuration is still loading plugins
    /// at that point, and the complaints this window exists for have not
    /// been raised yet.
    #[must_use]
    pub fn startup_window_open(&self) -> bool {
        !self.typed
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

    /// A typed `:` the gate admits, leaving the palette standing on a
    /// guess with no `cmdline_show` behind it.
    fn speculate_colon(model: &mut Model) {
        model.palette_enabled = true;
        model.engine.mode.current = "normal".to_string();
        crate::native::speculate::fold_engine_call(
            model,
            &crate::msg::RpcCall::Input {
                notation: ":".to_string(),
            },
            crate::native::speculate::SpecStamp::default(),
        );
        assert!(
            model.engine.cmdline_speculated.is_some(),
            "the gate refused a `:` this case is about"
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
            Surface::Statusline,
            Surface::Grid,
        ] {
            let rows = SURFACES.iter().filter(|r| r.surface == surface).count();
            assert_eq!(rows, 1, "{surface:?} needs exactly one row of the table");
            assert!(row(surface).is_some());
        }
        assert_eq!(
            SURFACES.len(),
            6,
            "a surface added to the enum needs a row here, with its own policy and remedy"
        );
    }

    /// The float rule reads the channel table, so every region the table
    /// names has to be answerable through it -- and a surface the table
    /// gives no float row must be answered for neither region. A
    /// `Channel::Float` row added to a surface, or taken off one, changes
    /// what `claims_at` says and is what this reads.
    #[test]
    fn every_float_region_the_table_names_is_claimed_through_it() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        model.attach_surfaces(vec![Ext::Cmdline, Ext::Messages, Ext::Tabline]);
        for entry in crate::native::channels::CHANNELS {
            for channel in entry.channels {
                let crate::native::channels::Channel::Float(region) = channel else {
                    continue;
                };
                let sighting = match region {
                    crate::native::channels::Region::CmdlineBand => cmp_cmdline_menu(),
                    crate::native::channels::Region::TopRightChrome => notify_toast(),
                };
                assert_eq!(
                    claims(&sighting, &model),
                    Some(entry.surface),
                    "{:?} names {region:?} and a float parked there answers otherwise",
                    entry.surface
                );
            }
        }

        let floatless: Vec<Surface> = crate::native::channels::CHANNELS
            .iter()
            .filter(|entry| {
                !entry
                    .channels
                    .iter()
                    .any(|channel| matches!(channel, crate::native::channels::Channel::Float(_)))
            })
            .map(|entry| entry.surface)
            .collect();
        for sighting in [cmp_cmdline_menu(), notify_toast()] {
            let answer = claims(&sighting, &model);
            assert!(
                !answer.is_some_and(|surface| floatless.contains(&surface)),
                "a surface the table gives no float row was claimed by one: {answer:?}"
            );
        }
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

    /// The command line the rule reads is the one the frame is painting,
    /// which during the silence after a typed `:` is view's own guess: the
    /// menu a plugin opens there is sighted before any `cmdline_show`, and
    /// a rule that waited for one would leave it and view's palette on
    /// screen together for the whole silence.
    #[test]
    fn a_float_on_the_cmdline_row_claims_the_speculated_command_line() {
        let mut model = captured_session();
        speculate_colon(&mut model);
        assert_eq!(
            claims(&cmp_cmdline_menu(), &model),
            Some(Surface::Cmdline),
            "a guess is a command line on screen as far as the rect rule is concerned"
        );
    }

    /// And the guess coming back off takes the claim with it: the rows are
    /// view's again only while something is drawing in them.
    #[test]
    fn a_float_on_the_cmdline_row_claims_nothing_once_the_guess_is_withdrawn() {
        let mut model = captured_session();
        speculate_colon(&mut model);
        crate::native::speculate::withdraw_cmdline_speculation(&mut model);
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

    /// Whether one scenario line asserts `ext`'s attach.
    ///
    /// The option name alone is not the test, and a bare substring match is
    /// spoofable by anything that happens to spell it: a row is proof when
    /// what it *queries* is the attach -- the `nvim_list_uis()[1].ext_*`
    /// form every citation in the corpus takes -- and only the half of the
    /// row before `expect` is read, so a value that echoes the query back is
    /// a string rather than an assertion. Comment lines are skipped for the
    /// same reason: several scenarios explain in prose which `ext_*` their
    /// config detaches, and prose asserts nothing.
    fn probes_attach(line: &str, ext: Ext) -> bool {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            return false;
        }
        let Some((_, probe)) = trimmed.split_once("probe = ") else {
            return false;
        };
        let query = probe
            .split_once("expect = ")
            .map_or(probe, |(query, _)| query);
        query.contains(&format!("nvim_list_uis()[1].{}", ext.as_str()))
    }

    /// Two shapes a bare substring rule cannot part from evidence, either of
    /// which would put a citation on the page for a state that never
    /// asserted the attach: a probe querying something else entirely that
    /// merely mentions an `ext_*` name in the value it expects back, and one
    /// that spells the whole query inside that value.
    #[test]
    fn a_probe_that_only_mentions_an_ext_name_is_not_proof() {
        let real = r#"{ probe = "luaeval('tostring(vim.api.nvim_list_uis()[1].ext_messages == true)')", expect = "false" },"#;
        assert!(probes_attach(real, Ext::Messages));
        assert!(
            !probes_attach(real, Ext::Cmdline),
            "one option's probe is not another's"
        );
        assert!(!probes_attach(&format!("  # {real}"), Ext::Messages));

        let mention =
            r#"{ probe = "luaeval('vim.g.view_last_detached')", expect = "ext_messages" },"#;
        assert!(!probes_attach(mention, Ext::Messages));

        let echoed = r#"{ probe = "luaeval('vim.g.view_last_probe')", expect = "vim.api.nvim_list_uis()[1].ext_messages" },"#;
        assert!(
            !probes_attach(echoed, Ext::Messages),
            "a query spelled in an expected value is a string, not an assertion"
        );
    }

    /// Every `scenario`/`state` whose probes assert `ext`'s attach, in
    /// scenario-file then state order.
    ///
    /// The attach decides whether view draws the surface at all, so a state
    /// asserting it on proves the policy and one asserting it off proves the
    /// `[native]` line that hands it back. Read out of the scenario files
    /// themselves rather than written down here, so a state that is deleted
    /// or renamed takes its citation with it instead of leaving the page
    /// naming evidence that no longer runs.
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
                if !probes_attach(line, ext) {
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
             | channels that draw it | proving scenario / state |\n\
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
            let claimants: Vec<String> = crate::native::channels::channels(table_row.surface)
                .iter()
                .map(|channel| format!("`{}`", channel.name()))
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

    /// The page a user reads instead of this module, pinned to what the
    /// module actually does. A policy, a switch, a channel or a proving
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
    }

    /// A surface with no attach answers to its `[native]` switch alone, so
    /// the model has to carry that switch by name: one this session cannot
    /// answer reads as a surface nobody draws, and a hold issued for it
    /// takes the user's chrome with nothing said.
    #[test]
    fn every_owned_surface_without_an_attach_names_a_switch_the_model_answers() {
        for table_row in SURFACES
            .iter()
            .filter(|row| row.policy != Policy::Yield && row.ext.is_none())
        {
            let mut model = Model::with_term_size(80, 24);
            model.attach_surfaces(Vec::new());
            let answered = table_row
                .feature
                .and_then(|feature| model.feature_switch(feature));
            assert!(
                answered.is_some(),
                "{} answers to {:?}, which Model::feature_switch does not carry",
                table_row.label,
                table_row.feature
            );
        }
    }

    /// The gate itself, over the session that turns one such surface off
    /// and the session that leaves it on: read through `ext`, both answer
    /// the same, and the notice about a held channel never reaches the
    /// screen.
    #[test]
    fn a_surface_with_no_attach_follows_its_own_switch() {
        for (enabled, drawn) in [(false, false), (true, true)] {
            let mut model = Model::with_term_size(80, 24);
            model.attach_surfaces(Vec::new());
            model.statusline_enabled = enabled;
            assert_eq!(
                super::view_draws(Surface::Statusline, &model),
                drawn,
                "the status line follows `[native] statusline`, not an attach"
            );
        }
    }

    /// A surface view draws names the `view.toml` line that hands it back,
    /// and that line is the switch its own `ext_*` capability is gated on
    /// ([`Ext::feature`]) rather than a string typed twice: a surface no
    /// switch reaches says so, and a notice about it then says what
    /// happened and stops rather than naming a setting that does not exist.
    #[test]
    fn every_owned_surface_names_the_switch_its_attach_is_gated_on() {
        let matrix = render_matrix();
        for table_row in SURFACES.iter().filter(|row| row.policy != Policy::Yield) {
            let gate = table_row.feature.map(|id| format!("[native] {id} = false"));
            assert_eq!(
                table_row.remedy.map(str::to_string),
                gate,
                "{}'s off switch is not the one its feature answers to",
                table_row.label
            );
            if let Some(ext) = table_row.ext {
                assert_eq!(
                    table_row.feature,
                    ext.feature(),
                    "{}'s feature and its attach's gate disagree",
                    table_row.label
                );
            }
            let cell = table_row
                .remedy
                .map_or_else(|| NONE_CELL.to_string(), |line| format!("`{line}`"));
            assert!(
                matrix.contains(&format!("| {cell} |")),
                "{} renders no off-switch cell",
                table_row.label
            );
        }
        assert!(
            matrix.contains(NONE_CELL),
            "the none marker never renders, so a coverage gap could not be seen"
        );
    }

    /// The half of the signature the enum derivation is for: every surface
    /// name a GUI can take reads as a complaint when a plugin quotes it,
    /// and a row naming none of them, nor `vim.notify`, reads as the user's.
    /// Walked over `ALL_MULTIGRID` rather than a copied list, so a surface
    /// added later joins the pin by existing -- and narrowing the
    /// derivation to the shipped set fails here by the name it dropped.
    #[test]
    fn every_surface_name_a_plugin_can_quote_reads_as_a_complaint() {
        for ext in crate::native::ext::ALL_MULTIGRID {
            let line = format!("You're using a GUI that uses `{}`", ext.as_str());
            assert!(
                SurfaceConflicts::reads_as_complaint(std::slice::from_ref(&line)),
                "{line:?} is a plugin saying it cannot work under view, and reads as nothing"
            );
        }
        assert!(SurfaceConflicts::reads_as_complaint(&[
            "`vim.notify` has been overwritten by another plugin?".to_string()
        ]));
        assert!(
            !SurfaceConflicts::reads_as_complaint(&[
                String::new(),
                "2 messages  Ctrl-D to dismiss".to_string(),
                "written 12 lines".to_string(),
            ]),
            "a window quoting no surface and no function is the user's to close"
        );
    }
}
