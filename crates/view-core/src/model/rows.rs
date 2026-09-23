//! What view keeps for itself out of the terminal, and the engine grid size
//! that leaves.
//!
//! Beside [`Model`] rather than inside it: the spawn needs the same
//! arithmetic before a `Model` exists, so the free function and the two
//! bounds it holds to belong with the methods that call it.

use super::Model;

impl Model {
    /// Terminal rows reserved for persistent chrome outside the engine
    /// grid: one row for the pill whenever it is showing, zero otherwise.
    /// Transient overlays (cmdline, messages, popupmenu) paint over the
    /// grid instead and never reserve rows.
    ///
    /// [`view_core::native::pill::shows`](crate::native::pill::shows) is
    /// the one answer, so the row this reserves and the row the painter
    /// draws into can never disagree.
    #[must_use]
    pub fn chrome_rows(&self) -> u16 {
        u16::from(crate::native::pill::shows(self))
    }

    /// Terminal rows reserved for the bottom-row statusline bar: one while
    /// the `statusline` native feature is enabled, zero otherwise. Distinct
    /// from [`Model::chrome_rows`] (a top-row offset for the tabline, not a
    /// total reservation) -- `view-surface::render` uses both together to
    /// find the engine grid's target size and the statusline layer's row.
    ///
    /// Under tiles the answer is zero whatever the switch says: each frame
    /// carries its own segments in its bottom edge, and nvim is held at
    /// `laststatus = 2` so every window has a row there to paint over.
    /// Under `panes = "nvim"` the feature holds nvim at `laststatus = 0`,
    /// so a session that drops the bar has no status line and no mode
    /// message anywhere on screen.
    #[must_use]
    pub fn statusline_rows(&self) -> u16 {
        self.look.bar_rows(self.statusline_enabled)
    }

    /// Rows nvim keeps for its command line at the foot of the outer grid.
    ///
    /// The takeover holds `cmdheight` at 0 for a session that owns both the
    /// command line and the message area, since one row carries both; a
    /// session that handed either back leaves nvim that row. The tiles
    /// painter spends the answer to tell the grid's last row from a
    /// window's status row, which it clears and draws a frame edge over.
    ///
    /// Read off the same channel table the hold is issued from, so the two
    /// cannot disagree about which surfaces decide it.
    #[must_use]
    pub fn cmdline_rows(&self) -> u16 {
        u16::from(
            !crate::native::channels::claimants_of("cmdheight")
                .all(|surface| crate::native::surfaces::view_draws(surface, self)),
        )
    }

    /// The `(width, height)` the engine grid should be resized to, given
    /// the current terminal size and reserved chrome rows. `update()` sends
    /// this as `Effect::Rpc(RpcCall::TryResize)` whenever the terminal size
    /// or the chrome reservation changes.
    #[must_use]
    pub fn grid_target(&self) -> (u16, u16) {
        grid_target_for(
            (self.term_width, self.term_height),
            self.chrome_rows(),
            self.statusline_rows() > 0,
            self.look.ring(),
        )
    }
}

/// The `(width, height)` an engine grid takes on a terminal of `size` with
/// `chrome_rows` reserved at the top, `ring` cells of view's own outer frame
/// across the width and one row of it at the top, and, when `statusline`
/// is on, view's own bottom bar.
///
/// `ring` is what tiles mode spends framing the screen itself: two cells
/// gapped, one gapless, none under `panes = "nvim"`. It comes off the width
/// whole, and the grid is placed one cell in from the terminal's left edge.
/// Only the ring's top row comes off the height. Under tiles view holds
/// `laststatus = 2` whatever `[native] statusline` says, so every bottom
/// tile has a status row between its frame and the terminal's bottom edge
/// where the ring's bottom row would be.
///
/// A free function because the spawn needs the answer before there is a
/// [`Model`] to ask. The child is started `--headless` with this size on a
/// `--cmd`, sources the user's config against it, and is then attached at
/// [`Model::grid_target`]: the two have to be the same arithmetic or every
/// launch relayouts every window at the attach, which is the resize the
/// geometry `--cmd` exists to avoid. No tabline exists at spawn, so that
/// caller passes `0` chrome rows.
///
/// A terminal reporting a zero on either axis is answered with
/// [`SIZE_FLOOR`], and the pair this returns is then held to
/// [`ENGINE_MIN_SIZE`] on each axis -- after the chrome, because what the
/// geometry `--cmd` and the attach spend is this pair, not the terminal's
/// own reading, and a 4-row terminal with a tabline and a statusline leaves
/// 2.
#[must_use]
pub fn grid_target_for(
    size: (u16, u16),
    chrome_rows: u16,
    statusline: bool,
    ring: u16,
) -> (u16, u16) {
    let size = if size.0 == 0 || size.1 == 0 {
        SIZE_FLOOR
    } else {
        size
    };
    let (width, height) = grid_room_for(size, chrome_rows, statusline, ring);
    (width.max(ENGINE_MIN_SIZE.0), height.max(ENGINE_MIN_SIZE.1))
}

/// What [`grid_target_for`] leaves the engine before any floor or clamp:
/// the terminal less the chrome, the bar and the ring. A spawn whose
/// target differs from this was clamped, and that is the one question the
/// caller asks of it.
#[must_use]
pub fn grid_room_for(
    size: (u16, u16),
    chrome_rows: u16,
    statusline: bool,
    ring: u16,
) -> (u16, u16) {
    // the width loses a cell on each side; the height loses only the top
    // row, since the bottom tiles' held status rows stand where a bottom
    // ring row would
    (
        size.0.saturating_sub(ring),
        size.1
            .saturating_sub(chrome_rows + u16::from(statusline))
            .saturating_sub(ring.min(1)),
    )
}

/// The smallest `columns` and `lines` the engine accepts, each on its own:
/// `vim.o.columns` under 12 raises `E594` and `vim.o.lines` under 3 raises
/// `E593`, and either aborts the geometry `--cmd`'s whole chunk (measured
/// against the pinned engine, which takes 12 and 3 and refuses 11 and 2).
///
/// Per axis rather than a fallback to [`SIZE_FLOOR`] because that is what
/// nvim's own TUI does with a positive-but-refused reading: on a pty sized
/// 40x5 it lays out at 12 columns and keeps 40 lines, and on one sized
/// 1x100 it keeps 100 columns and takes 3 lines. Only a non-positive axis
/// makes it drop both readings, which is the case [`SIZE_FLOOR`] answers.
pub const ENGINE_MIN_SIZE: (u16, u16) = (12, 3);

/// The terminal size [`grid_target_for`] answers a zero-axis reading with,
/// and nvim's own answer to the same reading: `tui_guess_size` takes both
/// defaults the moment either axis comes back non-positive, so a `nvim`
/// started on a pty whose size was never set lays out at 80x24 (measured
/// against the pinned engine, for 0x0, 0x40 and 100x0 alike).
///
/// A zero is not a hypothetical: a pty nothing has sized reports 0x0, and
/// so does a real terminal for the first instant of a session still
/// negotiating its size. Carrying one into the spawn is what makes it fatal
/// rather than merely small -- the geometry `--cmd` opens with
/// `vim.o.columns`, whose minimum is [`ENGINE_MIN_SIZE`], and the `E594`
/// that raises aborts the whole chunk, taking the `VimEnter` hook the
/// attach waits on with it.
/// The session then paints its shell frame and waits out the attach
/// deadline against a child that is alive and never says it started.
pub const SIZE_FLOOR: (u16, u16) = (80, 24);
