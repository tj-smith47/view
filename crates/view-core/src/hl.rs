//! The highlight table nvim's `hl_attr_define`/`default_colors_set` events
//! populate, kept free of any rendering-backend type so `view-core` stays
//! usable by any frontend.

/// One highlight group's rendering attributes, decoded from `hl_attr_define`.
pub struct HlAttr {
    /// Foreground color, or `None` to fall back to the default.
    pub fg: Option<u32>,
    /// Background color, or `None` to fall back to the default.
    pub bg: Option<u32>,
    /// Whether the group renders bold.
    pub bold: bool,
    /// Whether the group renders italic.
    pub italic: bool,
    /// Whether the group renders underlined.
    pub underline: bool,
    /// Whether foreground and background swap for this group.
    pub reverse: bool,
}

/// The highlight table: default colors plus every highlight group defined
/// so far, keyed by the `hl_id` `grid_line` cells reference.
pub struct HlTable {
    /// Default foreground, or `None` if nvim has not set one yet.
    pub default_fg: Option<u32>,
    /// Default background, or `None` if nvim has not set one yet.
    pub default_bg: Option<u32>,
    /// Highlight groups by id.
    pub attrs: std::collections::HashMap<u64, HlAttr>,
    /// Builtin UI element highlight group names (`"StatusLine"`,
    /// `"TabLineSel"`, `"Pmenu"`, ...) to the `hl_id` they currently
    /// resolve through `attrs`, populated by `hl_group_set` events. Chrome
    /// with no grid cell of its own (the tabline, the popup menu, ...)
    /// looks its style up here instead of holding a per-element `hl_id`
    /// directly, the same way a grid cell's `hl_id` looks itself up in
    /// `attrs`.
    pub groups: std::collections::HashMap<String, u64>,
}
