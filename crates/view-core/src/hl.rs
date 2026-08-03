//! The highlight table nvim's `hl_attr_define`/`default_colors_set` events
//! populate, kept free of any rendering-backend type so `view-core` stays
//! usable by any frontend.

/// One highlight group's rendering attributes, decoded from `hl_attr_define`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
///
/// Every cell on screen resolves its style through this table, so the table
/// is a paint input exactly as [`crate::grid::Grid`] is, and tracks its own
/// changes the same way: state is private, mutation goes through the
/// methods below, and each of them records that the resolved styles moved.
/// [`crate::model::Model::take_paint_damage`] folds that record into the
/// frame's damage, which is what keeps a colorscheme change from repainting
/// one damaged row over an otherwise stale screen. See [`HlTable::take_dirty`].
#[derive(Debug, Clone)]
pub struct HlTable {
    /// Default foreground, or `None` if nvim has not set one yet. Wire-
    /// ambiguous (see [`ProbedDefaults`]); `Theme::from_hl` prefers
    /// `confirmed` once its generation matches `probe_generation`.
    default_fg: Option<u32>,
    /// Default background, or `None` if nvim has not set one yet. Same
    /// wire ambiguity as `default_fg`.
    default_bg: Option<u32>,
    /// Highlight groups by id.
    attrs: std::collections::HashMap<u64, HlAttr>,
    /// Builtin UI element highlight group names (`"StatusLine"`,
    /// `"TabLineSel"`, `"Pmenu"`, ...) to the `hl_id` they currently
    /// resolve through `attrs`, populated by `hl_group_set` events. Chrome
    /// with no grid cell of its own (the tabline, the popup menu, ...)
    /// looks its style up here instead of holding a per-element `hl_id`
    /// directly, the same way a grid cell's `hl_id` looks itself up in
    /// `attrs`.
    groups: std::collections::HashMap<String, u64>,
    /// Bumped by every [`HlTable::set_default_colors`]; the `nvim_get_hl`
    /// probe effect emitted for that event carries this generation, so its
    /// eventual reply can be matched back to the event that requested it (or
    /// dropped as stale). See [`ProbedDefaults`] and `Theme::from_hl`.
    probe_generation: u64,
    /// The most recent accepted probe reply. `None` before the very first
    /// reply ever lands. May still be one generation stale relative to a
    /// `default_colors_set` that has fired since this reply arrived; reading
    /// this alone is never enough to know whether it is current -- compare
    /// `.generation` against [`HlTable::probe_generation`] first, exactly
    /// what `Theme::from_hl` does.
    confirmed: Option<ProbedDefaults>,
    /// Whether any of the above changed since the last [`HlTable::take_dirty`].
    dirty: bool,
}

impl HlTable {
    /// An empty table: no defaults, no groups, no probe reply yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            default_fg: None,
            default_bg: None,
            attrs: std::collections::HashMap::new(),
            groups: std::collections::HashMap::new(),
            probe_generation: 0,
            confirmed: None,
            dirty: false,
        }
    }

    /// The raw default foreground last set on the wire; see the field docs
    /// for why `Theme::from_hl` prefers a matching [`HlTable::confirmed`].
    #[must_use]
    pub fn default_fg(&self) -> Option<u32> {
        self.default_fg
    }

    /// The raw default background last set on the wire, wire-ambiguous for
    /// the value `Some(0)`; see [`ProbedDefaults`].
    #[must_use]
    pub fn default_bg(&self) -> Option<u32> {
        self.default_bg
    }

    /// `hl_id`'s attributes, or `None` if nvim has not defined that id.
    #[must_use]
    pub fn attr(&self, hl_id: u64) -> Option<HlAttr> {
        self.attrs.get(&hl_id).copied()
    }

    /// The `hl_id` a builtin UI element name currently resolves to, or
    /// `None` before nvim has associated one.
    #[must_use]
    pub fn group(&self, name: &str) -> Option<u64> {
        self.groups.get(name).copied()
    }

    /// The generation the next probe reply must carry to be accepted.
    #[must_use]
    pub fn probe_generation(&self) -> u64 {
        self.probe_generation
    }

    /// The most recent accepted probe reply, current only when its
    /// `generation` equals [`HlTable::probe_generation`].
    #[must_use]
    pub fn confirmed(&self) -> Option<ProbedDefaults> {
        self.confirmed
    }

    /// Records new default colors and opens a fresh probe generation for
    /// them, returning the generation the emitted `nvim_get_hl` probe must
    /// carry. The two are one operation because they are one fact: new
    /// defaults make any earlier probe reply answer a question that is no
    /// longer being asked, and leaving the generation behind would let a
    /// stale `confirmed` keep painting the previous colorscheme's
    /// disambiguated background.
    ///
    /// Dropping the returned generation leaves the new defaults with no
    /// probe to disambiguate them, so it is never correct to ignore it
    /// silently.
    #[must_use]
    pub fn set_default_colors(&mut self, fg: Option<u32>, bg: Option<u32>) -> u64 {
        self.default_fg = fg;
        self.default_bg = bg;
        self.probe_generation = self.probe_generation.wrapping_add(1);
        // unconditional rather than value-compared: the generation moves on
        // every call, and that alone re-derives the theme whenever it
        // invalidates a previously matching confirmed reply
        self.dirty = true;
        self.probe_generation
    }

    /// Defines (or redefines) one highlight id's attributes.
    pub fn define_attr(&mut self, hl_id: u64, attr: HlAttr) {
        if self.attrs.get(&hl_id) == Some(&attr) {
            // a redefinition to the identical value resolves every cell to
            // the same style, so it is not a repaint. Defensive rather than
            // load-bearing: instrumenting the pinned engine across four
            // plugin sessions saw 0 identical resends in 498 definitions
            // (it allocates a fresh id per distinct attribute set, even
            // across colorscheme reloads), but the table must not depend on
            // that -- an engine that did resend would turn every resend
            // into a whole-frame repaint, giving back the frames the damage
            // clip exists to save
            return;
        }
        self.attrs.insert(hl_id, attr);
        self.dirty = true;
    }

    /// Associates a builtin UI element name with the `hl_id` it resolves
    /// through.
    pub fn set_group(&mut self, name: String, hl_id: u64) {
        if self.groups.get(&name) == Some(&hl_id) {
            return;
        }
        self.groups.insert(name, hl_id);
        self.dirty = true;
    }

    /// Accepts one probe reply as the confirmed disambiguation of the
    /// current defaults. The caller checks the reply's generation against
    /// [`HlTable::probe_generation`] first: a reordered stale reply must be
    /// dropped rather than overwrite a newer, already-correct answer.
    pub fn confirm_defaults(&mut self, probe: ProbedDefaults) {
        if self.confirmed == Some(probe) {
            return;
        }
        self.confirmed = Some(probe);
        self.dirty = true;
    }

    /// Records that the resolved styles moved for a reason no single
    /// mutator above saw: a whole-table replacement, where the incoming
    /// table's own flag describes its construction rather than the swap.
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Drains whether anything here changed since the last call. Called once
    /// per frame by [`crate::model::Model::take_paint_damage`], which turns
    /// a `true` into whole-frame damage: a style change has no rows of its
    /// own, since it can restyle every cell on screen at once.
    ///
    /// Crate-private for the same reason [`crate::grid::Grid::take_dirty`]
    /// is: the highlight table is one of two paint inputs, and whoever
    /// drains it in isolation clips the next frame against a subset of what
    /// that frame paints from. Draining it here would leave the grid's own
    /// rows as the whole of the damage, which is exactly the one restyled
    /// stripe on an otherwise stale screen the fold exists to prevent.
    #[must_use]
    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }
}

impl Default for HlTable {
    fn default() -> Self {
        Self::new()
    }
}
