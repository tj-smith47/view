//! The per-cell highlight identity the differential oracle compares, and its
//! canonical text rendering.
//!
//! # Why resolved content, not raw hl ids
//!
//! `hl_attr_define` assigns highlight-group ids per nvim session, in first-use
//! order. The oracle runs two independent embedded nvim processes (view's own
//! `EngineSession` and the reference `ReferenceSession`), so the same visual
//! highlight -- `Search`, `Visual`, a syntax `Comment` -- is almost never the
//! same numeric id on both sides. Comparing raw `hl_id`s would false-diverge
//! on every healthy run the moment any highlight appeared on screen.
//!
//! [`ResolvedAttr`] is the fix: each side resolves its own cell `hl_id`
//! through its own highlight table into the *content* the id stands for, and
//! the oracle compares that content. Two sessions painting the same on-screen
//! color and emphasis resolve to an equal [`ResolvedAttr`] regardless of the
//! per-session id each assigned it.
//!
//! # Which attributes
//!
//! Exactly the fields `view-engine`'s `hl_attr_define` decoder carries and
//! `view_core::hl::HlAttr` models: foreground, background, bold, italic,
//! underline, reverse. That set is deliberate, not incidental -- the oracle
//! guards what view actually renders, and view renders exactly these. An
//! attribute the wire carries but view drops (`special`/`undercurl`/
//! `strikethrough`/`blend`/...) is dropped identically on both sides (both
//! decode the redraw stream through the same `view-engine` path), so it can
//! never diverge, and including it here would only compare a field neither
//! applier reflects on screen.

/// One grid cell's highlight identity: the rendering attributes its `hl_id`
/// resolves to on the side that painted it. See this module's docs for why
/// resolved content, not the raw per-session id, is what the oracle compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedAttr {
    /// Foreground color, or `None` for the default.
    pub fg: Option<u32>,
    /// Background color, or `None` for the default.
    pub bg: Option<u32>,
    /// Whether the cell renders bold.
    pub bold: bool,
    /// Whether the cell renders italic.
    pub italic: bool,
    /// Whether the cell renders underlined.
    pub underline: bool,
    /// Whether the cell swaps foreground and background.
    pub reverse: bool,
}

impl ResolvedAttr {
    /// The rendering `hl_id` 0 resolves to, and the fallback for any id no
    /// `hl_attr_define` has defined yet: nvim's default highlight (default
    /// colors, no emphasis). Both sides resolve an unknown id to this same
    /// value, so a cell painted with the default highlight can never diverge
    /// merely because one session had defined the id and the other had not.
    pub(crate) const DEFAULT: Self = Self {
        fg: None,
        bg: None,
        bold: false,
        italic: false,
        underline: false,
        reverse: false,
    };

    fn is_default(&self) -> bool {
        *self == Self::DEFAULT
    }

    /// Appends this cell's canonical fingerprint to `out`: a lone `.` for the
    /// default highlight (the overwhelmingly common cell, kept to one byte so
    /// a plain unhighlighted row stays readable), otherwise a bracketed,
    /// key-tagged rendering of every set field (`[fg=ff0000;bg=005f00;bu]`).
    /// The tags make presence unambiguous -- a `Some(0)` foreground reads as
    /// `fg=000000`, never elided into an absent one -- and the brackets fence
    /// a highlighted cell off from its `.`-rendered neighbors with no
    /// separator needed between cells.
    fn write_fingerprint(&self, out: &mut String) {
        if self.is_default() {
            out.push('.');
            return;
        }
        out.push('[');
        if let Some(fg) = self.fg {
            out.push_str(&format!("fg={fg:06x};"));
        }
        if let Some(bg) = self.bg {
            out.push_str(&format!("bg={bg:06x};"));
        }
        if self.bold {
            out.push('b');
        }
        if self.italic {
            out.push('i');
        }
        if self.underline {
            out.push('u');
        }
        if self.reverse {
            out.push('r');
        }
        out.push(']');
    }
}

impl From<&view_core::hl::HlAttr> for ResolvedAttr {
    fn from(attr: &view_core::hl::HlAttr) -> Self {
        Self {
            fg: attr.fg,
            bg: attr.bg,
            bold: attr.bold,
            italic: attr.italic,
            underline: attr.underline,
            reverse: attr.reverse,
        }
    }
}

/// Renders one grid row's per-cell [`ResolvedAttr`] fingerprints into the
/// single string [`crate::compare`] diffs row-for-row against the other
/// side's same-row rendering. Concatenated with no separator: each cell's
/// fingerprint is self-delimiting (see [`ResolvedAttr::write_fingerprint`]).
pub(crate) fn row_fingerprint(cells: impl Iterator<Item = ResolvedAttr>) -> String {
    let mut out = String::new();
    for cell in cells {
        cell.write_fingerprint(&mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn default_cell_renders_as_a_single_dot() {
        let mut out = String::new();
        ResolvedAttr::DEFAULT.write_fingerprint(&mut out);
        assert_eq!(out, ".");
    }

    /// A set foreground of `0` must render as `fg=000000`, not vanish: the
    /// fingerprint distinguishes `Some(0)` (explicit black) from `None`
    /// (default), which a presence-by-truthiness scheme would conflate.
    #[test]
    fn explicit_black_foreground_is_not_elided() {
        let attr = ResolvedAttr {
            fg: Some(0),
            ..ResolvedAttr::DEFAULT
        };
        let mut out = String::new();
        attr.write_fingerprint(&mut out);
        assert_eq!(out, "[fg=000000;]");
    }

    #[test]
    fn all_fields_render_in_a_stable_order() {
        let attr = ResolvedAttr {
            fg: Some(0xff_0000),
            bg: Some(0x00_5f00),
            bold: true,
            italic: true,
            underline: true,
            reverse: true,
        };
        let mut out = String::new();
        attr.write_fingerprint(&mut out);
        assert_eq!(out, "[fg=ff0000;bg=005f00;biur]");
    }

    /// A highlighted cell between two default cells stays unambiguously
    /// fenced by its own brackets with no inter-cell separator.
    #[test]
    fn row_fingerprint_fences_highlighted_cells_from_default_neighbors() {
        let row = [
            ResolvedAttr::DEFAULT,
            ResolvedAttr {
                bold: true,
                ..ResolvedAttr::DEFAULT
            },
            ResolvedAttr::DEFAULT,
        ];
        assert_eq!(row_fingerprint(row.into_iter()), ".[b].");
    }
}
