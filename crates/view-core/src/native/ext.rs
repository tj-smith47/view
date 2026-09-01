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
        }
    }
}

/// Every surface this build can externalize, in attach order -- the set a
/// session with no config to read attaches, and the vocabulary
/// `view_native::config::ext_surfaces` filters.
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

#[cfg(test)]
mod tests {
    use super::{Ext, ALL, ALL_NAMES};

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
}
