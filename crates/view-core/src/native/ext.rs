//! The `ext_*` UI capabilities a session can externalize, named once for
//! every crate that has to agree on them.
//!
//! The names are nvim's own `nvim_ui_attach` option keys, and they cross
//! three crates: `view-engine` sends them, `view-native` decides which of
//! them a `[native]` table asks for, and [`Model`](crate::model::Model)
//! carries the answer for the rest of the session. Spelling them here is
//! what keeps the three from drifting -- `view-native` may not depend on
//! `view-engine`, so a list owned by the sender would have to be copied to
//! be read.
//!
//! [`Ext`] rather than the bare strings for everything above the wire: the
//! vocabulary is closed, and a caller asking `Model::owns` about a
//! misspelled surface would otherwise be told `false` by a seam that had
//! simply never heard of it. Only the attach itself needs the strings, and
//! it takes them from [`ALL_NAMES`].

/// One `ext_*` UI capability, as asked for at attach and as answered by
/// [`Model::owns`](crate::model::Model::owns).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ext {
    /// The grid protocol itself, not a surface: without it nvim speaks the
    /// legacy per-cell redraw vocabulary this frontend does not decode.
    LineGrid,
    /// The command line, rendered by view as the palette.
    Cmdline,
    /// The completion popup, rendered inside the palette when the command
    /// line is what sourced it.
    Popupmenu,
    /// Messages, rendered by view as toasts and the message history.
    Messages,
    /// The tab line. Unconditional today: no native feature owns it, so
    /// there is no switch for it to follow.
    Tabline,
    /// Not a surface: the option that makes nvim address each window's grid
    /// separately (`docs/multigrid.md`). It externalizes nothing, so it is
    /// absent from [`ALL`] and a session that asked for it still owns
    /// exactly the surfaces [`ALL`] names -- it is a variant because the
    /// attach set travels as `Ext` from the resolver to the wire, and a
    /// second channel for one option is a second thing that can disagree
    /// with the first.
    Multigrid,
}

impl Ext {
    /// The `nvim_ui_attach` option key for this capability.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LineGrid => "ext_linegrid",
            Self::Cmdline => "ext_cmdline",
            Self::Popupmenu => "ext_popupmenu",
            Self::Messages => "ext_messages",
            Self::Tabline => "ext_tabline",
            Self::Multigrid => MULTIGRID_NAME,
        }
    }
}

/// Every surface this build can externalize, in attach order: the
/// vocabulary `view_native::config::ext_surfaces` filters, and the whole
/// answer to "which surfaces does view draw".
///
/// Not what a session with no config to read attaches -- that is
/// [`ALL_MULTIGRID`], these surfaces plus the one option that externalizes
/// none of them.
pub const ALL: &[Ext] = &[
    Ext::LineGrid,
    Ext::Cmdline,
    Ext::Popupmenu,
    Ext::Messages,
    Ext::Tabline,
];

/// [`ALL`] as the wire spells it, for `view-engine`'s attach and for the
/// oracle and corpus runners that ask nvim for every surface.
///
/// Each entry is its own variant's [`Ext::as_str`] rather than a literal,
/// so the two lists can differ only in membership -- which
/// `every_name_is_its_own_variants_spelling` then denies.
pub const ALL_NAMES: &[&str] = &[
    Ext::LineGrid.as_str(),
    Ext::Cmdline.as_str(),
    Ext::Popupmenu.as_str(),
    Ext::Messages.as_str(),
    Ext::Tabline.as_str(),
];

/// The `nvim_ui_attach` option that makes nvim address each window's grid
/// separately (`docs/multigrid.md`), as the wire spells
/// [`Ext::Multigrid`].
///
/// Not in [`ALL`]: it externalizes no surface, it changes how every grid on
/// the wire is addressed, so [`Model::owns`] answers about it only in the
/// sense of "this session negotiated it".
///
/// [`Model::owns`]: crate::model::Model::owns
pub const MULTIGRID_NAME: &str = "ext_multigrid";

/// The set a session with nothing to narrow it attaches today: [`ALL`] plus
/// [`Ext::Multigrid`], which is what `[engine] single_grid = true` takes
/// back off.
pub const ALL_MULTIGRID: &[Ext] = &[
    Ext::LineGrid,
    Ext::Cmdline,
    Ext::Popupmenu,
    Ext::Messages,
    Ext::Tabline,
    Ext::Multigrid,
];

/// [`ALL_NAMES`] plus [`MULTIGRID_NAME`], for the oracle sides that attach
/// under the multigrid vocabulary. Spelled out rather than concatenated
/// because a slice cannot be built from another one in a `const`;
/// `the_multigrid_set_is_the_default_set_plus_one_option` denies the two
/// lists drifting apart.
pub const ALL_NAMES_MULTIGRID: &[&str] = &[
    Ext::LineGrid.as_str(),
    Ext::Cmdline.as_str(),
    Ext::Popupmenu.as_str(),
    Ext::Messages.as_str(),
    Ext::Tabline.as_str(),
    MULTIGRID_NAME,
];

#[cfg(test)]
mod tests {
    use super::{Ext, ALL, ALL_MULTIGRID, ALL_NAMES, ALL_NAMES_MULTIGRID, MULTIGRID_NAME};

    #[test]
    fn every_name_is_its_own_variants_spelling() {
        assert_eq!(ALL.len(), ALL_NAMES.len(), "the two lists must agree");
        for (surface, name) in ALL.iter().zip(ALL_NAMES) {
            assert_eq!(&surface.as_str(), name);
        }
    }

    #[test]
    fn every_key_is_an_ext_option_nvim_would_recognize() {
        for surface in ALL {
            assert!(
                surface.as_str().starts_with("ext_"),
                "{surface:?} is not an nvim_ui_attach ext option"
            );
        }
        assert_eq!(ALL[0], Ext::LineGrid, "the grid protocol attaches first");
    }

    #[test]
    fn the_multigrid_set_is_the_default_set_plus_one_option() {
        let (head, tail) = ALL_NAMES_MULTIGRID.split_at(ALL_NAMES.len());
        assert_eq!(head, ALL_NAMES, "the multigrid set opens with the default");
        assert_eq!(tail, [MULTIGRID_NAME], "and adds exactly the one option");
        assert!(
            !ALL_NAMES.contains(&MULTIGRID_NAME),
            "the surface set must not negotiate multigrid"
        );
    }

    /// The typed set and the wire set are the same set, and the option that
    /// is not a surface is in neither surface list.
    #[test]
    fn the_shipped_attach_set_is_every_surface_plus_the_multigrid_option() {
        assert_eq!(ALL_MULTIGRID.len(), ALL_NAMES_MULTIGRID.len());
        for (ext, name) in ALL_MULTIGRID.iter().zip(ALL_NAMES_MULTIGRID) {
            assert_eq!(&ext.as_str(), name);
        }
        assert_eq!(Ext::Multigrid.as_str(), MULTIGRID_NAME);
        assert!(
            !ALL.contains(&Ext::Multigrid),
            "multigrid externalizes no surface, so it cannot join the surface list"
        );
    }
}
