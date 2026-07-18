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

/// One `nvim_get_hl(0, {name = "Normal"})` probe reply, tagged with the
/// `DefaultColorsSet` generation it answers. The probe exists because
/// `default_colors_set`'s wire value is ambiguous: nvim sends `rgb_bg = 0`
/// both when `Normal` has no background at all (the terminal's own default
/// should show through) and when a colorscheme genuinely sets `guibg =
/// #000000`. The probe reply's `fg`/`bg` map keys are present only when the
/// color is genuinely set, which is what disambiguates the two cases; a
/// missing key becomes `None` here, decoded by `view-engine` from the raw
/// reply before it ever reaches this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbedDefaults {
    /// The `HlTable::probe_generation` this reply answers.
    pub generation: u64,
    /// The probe-confirmed default foreground, or `None` if genuinely unset.
    pub fg: Option<u32>,
    /// The probe-confirmed default background, or `None` if genuinely unset.
    pub bg: Option<u32>,
}

/// The highlight table: default colors plus every highlight group defined
/// so far, keyed by the `hl_id` `grid_line` cells reference.
pub struct HlTable {
    /// Default foreground, or `None` if nvim has not set one yet. Wire-
    /// ambiguous (see [`ProbedDefaults`]); `Theme::from_hl` prefers
    /// `confirmed` once its generation matches `probe_generation`.
    pub default_fg: Option<u32>,
    /// Default background, or `None` if nvim has not set one yet. Same
    /// wire ambiguity as `default_fg`.
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
    /// Bumped by `update()` on every `DefaultColorsSet`; the `nvim_get_hl`
    /// probe effect emitted for that event carries this generation, so its
    /// eventual reply can be matched back to the event that requested it (or
    /// dropped as stale). See [`ProbedDefaults`] and `Theme::from_hl`.
    pub probe_generation: u64,
    /// The most recent probe reply `update()` has accepted (its generation
    /// matched `probe_generation` at write time -- see `update()`'s
    /// `Msg::HlProbeReply` arm). `None` before the very first reply ever
    /// lands. May still be one generation stale relative to a
    /// `DefaultColorsSet` that has fired since this reply arrived; reading
    /// this field alone is never enough to know whether it is current --
    /// compare `.generation` against `probe_generation` first, exactly what
    /// `Theme::from_hl` does.
    pub confirmed: Option<ProbedDefaults>,
}
