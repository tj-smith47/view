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

/// The grid protocol itself, not a surface: without it nvim speaks the
/// legacy per-cell redraw vocabulary this frontend does not decode.
pub const LINEGRID: &str = "ext_linegrid";

/// The command line, rendered by view as the palette.
pub const CMDLINE: &str = "ext_cmdline";

/// The completion popup, rendered inside the palette when the command line
/// is what sourced it.
pub const POPUPMENU: &str = "ext_popupmenu";

/// Messages, rendered by view as toasts and the message history.
pub const MESSAGES: &str = "ext_messages";

/// The tab line. Unconditional today: no native feature owns it, so there
/// is no switch for it to follow.
pub const TABLINE: &str = "ext_tabline";

/// Every surface this build can externalize, in attach order -- the set a
/// session with no config to read attaches, and the vocabulary
/// `view_native::config::ext_surfaces` filters.
pub const ALL: &[&str] = &[LINEGRID, CMDLINE, POPUPMENU, MESSAGES, TABLINE];
