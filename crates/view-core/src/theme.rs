//! The color vocabulary painting resolves style through, derived purely
//! from the live highlight state nvim's `default_colors_set`/
//! `hl_attr_define`/`hl_group_set` event stream already populates into
//! [`HlTable`]. No separate theme config or RPC query: `Theme::from_hl` is
//! a read of state the engine already streams, so re-deriving it on every
//! `ColorScheme` change costs nothing beyond a handful of `HashMap` lookups.

use crate::hl::HlTable;

/// One fully-resolved cell or chrome style: colors plus text attributes,
/// backend-free so any frontend (not just `ratatui`) can consume it.
/// `reverse` is a flag rather than pre-swapped `fg`/`bg` values so it stays
/// meaningful even when one or both colors are unset: a rendering backend's
/// own reverse-video mode still inverts against its ambient default in that
/// case, where a pre-swap of two `None` values would carry no information
/// at all and silently lose that guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResolvedStyle {
    /// Foreground color, or `None` to inherit the terminal's own default.
    pub fg: Option<u32>,
    /// Background color, or `None` to inherit the terminal's own default.
    pub bg: Option<u32>,
    /// Whether the style renders bold.
    pub bold: bool,
    /// Whether the style renders italic.
    pub italic: bool,
    /// Whether the style renders underlined.
    pub underline: bool,
    /// Whether foreground and background render swapped.
    pub reverse: bool,
}

/// What a chrome group resolves to while nvim has not associated its name
/// with a highlight id yet -- before the first `hl_group_set` batch, or
/// under a minimal colorscheme that never redefines the group at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChromeFallback {
    /// [`Theme::normal`]: unmapped chrome should look like plain text.
    Normal,
    /// [`Theme::emphasis`]: a selection-style group stays visibly distinct
    /// from the row beside it even with zero color information.
    Emphasis,
}

/// How many identifiers were handed in, evaluated at compile time so
/// `ALL`'s length is a fact about the declaration list rather than a number
/// anyone maintains.
macro_rules! chrome_group_count {
    () => { 0usize };
    ($head:ident $($tail:ident)*) => { 1usize + chrome_group_count!($($tail)*) };
}

/// Declares the chrome-group vocabulary: one line per group giving its
/// variant, the name nvim's `hl_group_set` event uses for it, and what it
/// resolves to before nvim has named it. The enum, `ALL`, `COUNT`,
/// `hl_name` and `fallback` are all generated from that single list.
///
/// A group cannot be half-declared, because there is nowhere to half-declare
/// it: the enum has no definition outside this list, so an arm that exists
/// at all exists in `ALL`, carries a name and carries a fallback. That is
/// the whole reason the vocabulary is generated rather than written out --
/// the hand-written shape it replaced allowed an arm the exhaustive matches
/// forced you to handle while `ALL` silently stayed one short, and every
/// consumer that iterates `ALL` (derivation, the cache, every regression
/// test) then skipped the new group without a single compiler complaint.
macro_rules! chrome_groups {
    ($($(#[$attr:meta])* $variant:ident => $hl_name:literal, $fallback:ident;)+) => {
        /// One builtin chrome element nvim's `hl_group_set` event names, and
        /// the key both the live derivation and the on-disk cache address
        /// that element by.
        ///
        /// An enum rather than a struct field per group so a group is
        /// declared once and every consumer follows from
        /// [`ChromeGroup::ALL`]: derivation, the paint-side lookup, and the
        /// persisted cache all iterate the same list, so a group that
        /// reaches one of them reaches all three. The previous
        /// one-field-per-group shape failed silently in exactly the
        /// direction that costs a user colors -- a group added to the live
        /// type but missed in the cache's mirror compiled clean and simply
        /// never persisted, leaving cold start painting a stale default for
        /// that one element.
        ///
        /// `#[repr(usize)]` is load-bearing: [`index`](Self::index) is the
        /// discriminant cast, so slot assignment is declaration order by
        /// construction and no separate table can disagree with it.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(usize)]
        #[non_exhaustive]
        pub enum ChromeGroup {
            $($(#[$attr])* $variant,)+
        }

        impl ChromeGroup {
            /// Every chrome group, in the order [`index`](Self::index)
            /// assigns.
            pub const ALL: [Self; chrome_group_count!($($variant)+)] =
                [$(Self::$variant),+];

            /// How many chrome groups this build resolves.
            pub const COUNT: usize = Self::ALL.len();

            /// The name nvim's `hl_group_set` event uses for this group,
            /// which is also the key it is cached under on disk. One
            /// spelling for both, so a group cannot be looked up live under
            /// one name and persisted under another.
            #[must_use]
            pub const fn hl_name(self) -> &'static str {
                match self {
                    $(Self::$variant => $hl_name,)+
                }
            }

            /// What this group resolves to while nvim has not named it.
            #[must_use]
            pub const fn fallback(self) -> ChromeFallback {
                match self {
                    $(Self::$variant => ChromeFallback::$fallback,)+
                }
            }

            /// This group's slot in [`Theme`]'s resolved-style array: an
            /// O(1) integer, so a per-frame chrome lookup costs an array
            /// read with no string compare and no allocation. `pub(crate)`:
            /// the slot layout is `Theme`'s own storage detail, and every
            /// caller outside it resolves a group through `Theme`'s public
            /// API instead of indexing the array directly.
            #[must_use]
            pub(crate) const fn index(self) -> usize {
                self as usize
            }
        }
    };
}

chrome_groups! {
    /// The statusline bar's own text. `view-tui`'s startup placeholder
    /// paints it before the engine attaches; a full native statusline
    /// widget over live buffer state is still unbuilt, and `Theme`'s
    /// contract is to resolve every builtin group nvim names regardless of
    /// how many consumers exist yet.
    StatusLine => "StatusLine", Normal;
    /// An unselected tab's label.
    TabLine => "TabLine", Normal;
    /// The current tab's label.
    TabLineSel => "TabLineSel", Emphasis;
    /// The tabline row's background beyond the tab labels themselves.
    TabLineFill => "TabLineFill", Normal;
    /// An unselected popup-menu row.
    Pmenu => "Pmenu", Normal;
    /// The selected popup-menu row.
    PmenuSel => "PmenuSel", Emphasis;
    /// The message-log overlay's text.
    MsgArea => "MsgArea", Normal;
    /// The mode indicator nvim's own message area shows for `-- INSERT --`
    /// and friends (`:h hl-ModeMsg`). The native statusline's mode segment
    /// reuses it rather than inventing a statusline-only group, so a
    /// colorscheme that already themes nvim's mode message gets a matching
    /// statusline mode segment for free.
    ModeMsg => "ModeMsg", Emphasis;
    /// nvim's own warning-message group (`:h hl-WarningMsg`). Two native
    /// statusline segments reuse it: the modified-buffer marker (an unsaved
    /// buffer is exactly the kind of thing that group already exists to
    /// draw the eye to) and the warning-diagnostic count, via
    /// [`crate::native::views::StyleRole::DiagnosticWarning`]. They share a
    /// group because nvim broadcasts no finer warning group than this one
    /// -- see [`ChromeGroup::ErrorMsg`] for why that constraint decides
    /// these mappings.
    WarningMsg => "WarningMsg", Emphasis;
    /// nvim's own directory-label group (`:h hl-Directory`). The native
    /// statusline's git-branch segment reuses it, the closest existing
    /// builtin to "a short, path-adjacent label", rather than a
    /// statusline-only group no colorscheme has ever themed.
    Directory => "Directory", Normal;
    /// nvim's own error-message group (`:h hl-ErrorMsg`). The native
    /// statusline's error-count glyph resolves through this.
    ///
    /// `DiagnosticError`/`DiagnosticWarn` would read as the better fit and
    /// were what this resolved through until 2026-08-09, but they are not
    /// groups nvim can deliver: `hl_group_set` broadcasts the builtin
    /// highlight table only, and the diagnostic groups are defined in Lua
    /// (`vim.diagnostic`), outside it. Measured against the pinned engine
    /// -- a `--clean` `nvim_ui_attach` broadcasts 75 names, and neither
    /// diagnostic group is among them (`ErrorMsg` and `WarningMsg` both
    /// are), so those two slots sat on their declared fallback for the life
    /// of every session and no colorscheme could ever move them.
    /// [`every_group_is_one_nvim_broadcasts`] is the pin that keeps a group
    /// nvim never names from being declared here again.
    ErrorMsg => "ErrorMsg", Emphasis;
    /// nvim's builtin incremental-search group (`:h hl-IncSearch`). The
    /// picker's [`crate::native::views::StyleRole::Match`] resolves through
    /// this: a matched substring inside a candidate row is exactly what
    /// `IncSearch` already exists to draw the eye to, so a colorscheme that
    /// themes incremental search gets a matching picker highlight for free
    /// instead of an unthemed picker-only group.
    IncSearch => "IncSearch", Emphasis;
    /// nvim's builtin added-diff-line group (`:h hl-DiffAdd`). The file
    /// tree's [`crate::native::views::StyleRole::GitAdded`] resolves through
    /// this, the same group a colorscheme already uses for an added diff
    /// hunk, so an added or copied entry's glyph matches it.
    DiffAdd => "DiffAdd", Emphasis;
    /// nvim's builtin changed-diff-line group (`:h hl-DiffChange`), the
    /// modified counterpart of [`ChromeGroup::DiffAdd`].
    DiffChange => "DiffChange", Emphasis;
    /// nvim's builtin deleted-diff-line group (`:h hl-DiffDelete`), the
    /// deleted counterpart of [`ChromeGroup::DiffAdd`].
    DiffDelete => "DiffDelete", Emphasis;
    /// nvim's builtin floating-window body group (`:h hl-NormalFloat`).
    /// Every native overlay is a float in nvim's own vocabulary -- a box
    /// drawn over the buffer with a frame around it -- so its interior fill
    /// resolves through the group a colorscheme already themes floats with,
    /// rather than through the popup-menu group a completion list owns. The
    /// `Normal` fallback is the correct degrade rather than a placeholder: a
    /// colorscheme that never defines `NormalFloat` wants its floats on the
    /// buffer's own background.
    NormalFloat => "NormalFloat", Normal;
    /// nvim's builtin floating-window title group (`:h hl-FloatTitle`). A
    /// native overlay's title is set into its top border, and this is the
    /// group nvim already uses for exactly that -- the title of a float,
    /// distinct from the border it sits in -- so a colorscheme that themes
    /// floating windows themes view's overlay titles with it. Without a
    /// group of its own the title inherited the frame's dimmed border
    /// color, which is the one part of the frame a user reads as text.
    FloatTitle => "FloatTitle", Emphasis;
    /// nvim's builtin prompt group (`:h hl-Question`), what it colors the
    /// questions it puts to the user. The agent panel's own
    /// [`crate::native::views::StyleRole::AiUser`] resolves through it: a
    /// prompt the user composed for the agent is the same kind of text, and
    /// a colorscheme that themes nvim's own prompts themes it for free.
    Question => "Question", Emphasis;
    /// nvim's builtin informational-message group (`:h hl-MoreMsg`), the
    /// closest builtin to "the editor talking back". The agent's replies
    /// ([`crate::native::views::StyleRole::AiAgent`]) resolve through it, so
    /// a reply reads in a different color from the prompt above it without
    /// either being spelled out in words.
    MoreMsg => "MoreMsg", Emphasis;
    /// nvim's builtin group for cells that are not buffer text (`:h
    /// hl-NonText`) -- the one builtin every colorscheme themes as
    /// deliberately recessive. The agent's reasoning and an unresolved tool
    /// call's status glyph both resolve through it, which is what makes
    /// them read as beside the conversation rather than part of it.
    NonText => "NonText", Normal;
    /// nvim's builtin success-message group (`:h hl-OkMsg`), the positive
    /// counterpart of [`ChromeGroup::ErrorMsg`]. A completed tool call's
    /// check glyph resolves through it, so the two outcomes take the
    /// colorscheme's own answers to "this went well" and "this did not"
    /// rather than a pair chosen here.
    OkMsg => "OkMsg", Emphasis;
}

/// The active colorscheme's resolved design system: the default/"Normal"
/// colors plus every [`ChromeGroup`] nvim's `hl_group_set` event associates
/// a name with, so native chrome renders in the colorscheme's own colors
/// instead of a hardcoded style nvim has no way to reach.
///
/// Deliberately holds resolved *values*, not the live `HlTable` itself:
/// per-cell grid attributes are always resolved fresh against the live
/// table ([`Theme::style_for`], parametrized by `hl_id` per cell -- a fixed
/// set of chrome slots could never cover every cell), but the fixed chrome
/// vocabulary is cheap to resolve once per frame and correspondingly cheap
/// to persist to disk for a themed first paint before that live state
/// exists, seeded from persisted state before attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Theme {
    /// The default/"Normal" foreground color.
    pub fg: Option<u32>,
    /// The default/"Normal" background color.
    pub bg: Option<u32>,
    /// Every chrome group's resolved style, indexed by
    /// [`ChromeGroup::index`]. Private so the array and the enum can never
    /// disagree about which slot belongs to which group: reads go through
    /// [`Theme::chrome`], writes through [`Theme::set_chrome`].
    chrome: [ResolvedStyle; ChromeGroup::COUNT],
}

impl Theme {
    /// Derives a `Theme` from the engine's live highlight table. Pure and
    /// deterministic: the same `HlTable` state always derives the same
    /// `Theme`, so callers may re-derive on every frame with no history to
    /// track. A group with no mapping yet falls back to its declared
    /// [`ChromeGroup::fallback`], so a selection-style row stays visibly
    /// distinct even with zero color information rather than degrading to
    /// indistinguishable-from-everything-else.
    #[must_use]
    pub fn from_hl(hl: &HlTable) -> Self {
        // `confirmed` is trusted only when its generation matches the
        // current `probe_generation`: a `DefaultColorsSet` bumps the
        // generation immediately, so a still-in-flight probe for the new
        // generation leaves `confirmed` one generation stale until its
        // reply lands. `fg`'s wire value carries no equivalent ambiguity
        // (nvim always sends a genuine color or -1/None for it, decoded
        // upstream), so it falls straight back to the raw wire value for
        // that window, self-correcting the moment the fresh reply arrives.
        let fg = match hl.confirmed() {
            Some(p) if p.generation == hl.probe_generation() => p.fg,
            _ => hl.default_fg(),
        };
        // `bg`, unlike `fg`, has a genuinely ambiguous wire encoding: nvim
        // sends 0 both for "Normal has no background" and for a real
        // `guibg=#000000`. Painting that 0 before it is disambiguated is
        // exactly the black-flash this branch exists to prevent, so an
        // in-flight (generation-mismatched) ambiguous zero is held back
        // rather than painted: it prefers the last confirmed value this
        // session has ever seen (a stale-but-real prior probe reply, or a
        // value seeded from persisted state before attach, carrying a
        // matching generation) over the raw wire zero, and only degrades to
        // "unset" when no confirmed value has ever existed at all (a true
        // cold start with no seeded state). A non-zero wire value carries no
        // such ambiguity and keeps applying immediately, exactly like `fg`.
        let bg = match hl.confirmed() {
            Some(p) if p.generation == hl.probe_generation() => p.bg,
            _ if hl.default_bg() == Some(0) => hl.confirmed().and_then(|p| p.bg),
            _ => hl.default_bg(),
        };
        let mut theme = Self {
            fg,
            bg,
            ..Self::default()
        };
        for group in ChromeGroup::ALL {
            let fallback = theme.fallback_style(group);
            let resolved = theme.named(hl, group.hl_name(), fallback);
            theme.set_chrome(group, resolved);
        }
        theme
    }

    /// A theme carrying only the default/"Normal" colors, with every chrome
    /// group left unset.
    ///
    /// The constructor a caller outside this crate builds a `Theme` from:
    /// the chrome slots are private (see [`Theme::chrome`]), so the struct
    /// itself cannot be literal-constructed elsewhere, and a group is
    /// written afterwards through [`Theme::set_chrome`].
    #[must_use]
    pub fn with_colors(fg: Option<u32>, bg: Option<u32>) -> Self {
        Self {
            fg,
            bg,
            ..Self::default()
        }
    }

    /// One chrome group's resolved style: an array read keyed by
    /// [`ChromeGroup::index`], so a per-frame paint pays no string compare
    /// and allocates nothing.
    ///
    /// Total by construction rather than by indexing: a group whose slot
    /// this build does not carry reads as its declared fallback, which is
    /// the same honest answer as "nvim has not named this group yet", and
    /// never a neighbouring group's colors.
    #[must_use]
    pub fn chrome(&self, group: ChromeGroup) -> ResolvedStyle {
        self.chrome
            .get(group.index())
            .copied()
            .unwrap_or_else(|| self.fallback_style(group))
    }

    /// Overwrites `group`'s resolved style. A no-op for a group whose slot
    /// this build does not carry, for the same reason [`Theme::chrome`]
    /// reads a fallback rather than panicking.
    pub fn set_chrome(&mut self, group: ChromeGroup, style: ResolvedStyle) {
        if let Some(slot) = self.chrome.get_mut(group.index()) {
            *slot = style;
        }
    }

    /// The style `group` resolves to while nvim has not named it, per its
    /// declared [`ChromeGroup::fallback`].
    #[must_use]
    fn fallback_style(&self, group: ChromeGroup) -> ResolvedStyle {
        match group.fallback() {
            ChromeFallback::Normal => self.normal(),
            ChromeFallback::Emphasis => self.emphasis(),
        }
    }

    /// The base/"Normal" resolved style: this theme's colors, no attributes.
    #[must_use]
    pub fn normal(&self) -> ResolvedStyle {
        ResolvedStyle {
            fg: self.fg,
            bg: self.bg,
            ..ResolvedStyle::default()
        }
    }

    /// Chrome's "this row stands out" style: `normal`'s colors plus the
    /// `reverse` flag, which -- unlike pre-swapping color values -- stays
    /// visibly distinct even before any theme color is known (an unset
    /// `fg`/`bg` still inverts against a rendering backend's own ambient
    /// default). The documented fallback [`Theme::from_hl`] uses for a
    /// selection-style named group it cannot resolve yet.
    #[must_use]
    pub fn emphasis(&self) -> ResolvedStyle {
        ResolvedStyle {
            reverse: true,
            ..self.normal()
        }
    }

    /// One grid cell's resolved style: `hl_id`'s attributes from `hl`
    /// layered over this theme's base colors, matching nvim's own
    /// fallback rule (an attribute the highlight group does not set falls
    /// back to the default/"Normal" value).
    #[must_use]
    pub fn style_for(&self, hl_id: u64, hl: &HlTable) -> ResolvedStyle {
        let mut style = self.normal();
        if let Some(a) = hl.attr(hl_id) {
            if a.fg.is_some() {
                style.fg = a.fg;
            }
            if a.bg.is_some() {
                style.bg = a.bg;
            }
            style.reverse = a.reverse;
            style.bold = a.bold;
            style.italic = a.italic;
            style.underline = a.underline;
        }
        style
    }

    /// One builtin chrome element's resolved style, looked up in
    /// the table by nvim's `hl_group_set` name and resolved the same way
    /// a grid cell's `hl_id` resolves ([`Theme::style_for`]). `fallback` is
    /// used only when nvim has not associated `name` with an `hl_id` at
    /// all yet; once it has, that resolution is trusted as-is, even where
    /// it happens to equal `fallback`.
    #[must_use]
    pub fn named(&self, hl: &HlTable, name: &str, fallback: ResolvedStyle) -> ResolvedStyle {
        match hl.group(name) {
            Some(hl_id) => self.style_for(hl_id, hl),
            None => fallback,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::hl::HlAttr;
    use crate::model::Model;
    use crate::msg::Msg;
    use crate::update::update;

    /// A table carrying one defined highlight id and one `default_colors_set`
    /// worth of defaults, built through the same transitions nvim's events
    /// drive, so no test can pin a state production cannot reach. The
    /// defaults write leaves `probe_generation` at 1, the generation a probe
    /// reply for these colors must carry.
    fn table_with(
        default_fg: Option<u32>,
        default_bg: Option<u32>,
        id: u64,
        attr: HlAttr,
    ) -> HlTable {
        let mut hl = HlTable::new();
        hl.define_attr(id, attr);
        let _ = hl.set_default_colors(default_fg, default_bg);
        hl
    }

    fn no_attrs() -> HlAttr {
        HlAttr {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        }
    }

    /// `ALL`'s ordering and `index`'s answers are two statements of the
    /// same fact, and `Theme` reads its array through the second while
    /// every consumer iterates the first. A disagreement between them would
    /// hand one group another's colors with nothing failing loudly. The
    /// generated vocabulary makes that disagreement unrepresentable -- both
    /// follow from declaration order, one as the array literal and one as
    /// the discriminant -- and this pins it anyway, because the thing that
    /// would break it is a future `#[repr]` or explicit-discriminant edit
    /// no other test would notice.
    #[test]
    fn every_group_indexes_its_own_slot_in_all() {
        assert_eq!(ChromeGroup::ALL.len(), ChromeGroup::COUNT);
        for (slot, group) in ChromeGroup::ALL.into_iter().enumerate() {
            assert_eq!(
                group.index(),
                slot,
                "{} indexes a slot other than its own position in ALL",
                group.hl_name()
            );
        }
    }

    /// Every name nvim's `hl_group_set` event carries on a `--clean`
    /// `nvim_ui_attach`, captured live against the pinned engine (v0.12.4)
    /// by attaching a raw msgpack-RPC UI and reading the batch that precedes
    /// the first `flush`.
    ///
    /// This is nvim's builtin highlight table and nothing else. A group
    /// defined in Lua -- every `Diagnostic*` group, every LSP semantic-token
    /// group, anything a plugin defines -- is absent from it no matter how
    /// thoroughly a colorscheme themes that group, because the UI protocol
    /// only ever announces the builtins.
    ///
    /// Recorded rather than queried at test time so the pin fails on a
    /// deliberate reading of what changed, not on whichever nvim happens to
    /// be on a contributor's `PATH`. An engine-pin bump that adds or drops
    /// a builtin makes [`every_group_is_one_nvim_broadcasts`] fail, which is
    /// the intended prompt to re-capture this list.
    const BROADCAST: [&str; 75] = [
        "ColorColumn",
        "Conceal",
        "CurSearch",
        "Cursor",
        "CursorColumn",
        "CursorLine",
        "CursorLineFold",
        "CursorLineNr",
        "CursorLineSign",
        "DiffAdd",
        "DiffChange",
        "DiffDelete",
        "DiffText",
        "DiffTextAdd",
        "Directory",
        "EndOfBuffer",
        "ErrorMsg",
        "FloatBorder",
        "FloatFooter",
        "FloatTitle",
        "FoldColumn",
        "Folded",
        "IncSearch",
        "LineNr",
        "LineNrAbove",
        "LineNrBelow",
        "ModeMsg",
        "MoreMsg",
        "MsgArea",
        "MsgSeparator",
        "NonText",
        "NormalFloat",
        "NormalNC",
        "OkMsg",
        "Pmenu",
        "PmenuBorder",
        "PmenuExtra",
        "PmenuExtraSel",
        "PmenuKind",
        "PmenuKindSel",
        "PmenuMatch",
        "PmenuMatchSel",
        "PmenuSbar",
        "PmenuSel",
        "PmenuThumb",
        "PreInsert",
        "Question",
        "QuickFixLine",
        "Search",
        "SignColumn",
        "SpecialKey",
        "SpellBad",
        "SpellCap",
        "SpellLocal",
        "SpellRare",
        "StatusLine",
        "StatusLineNC",
        "StatusLineTerm",
        "StatusLineTermNC",
        "StderrMsg",
        "StdoutMsg",
        "TabLine",
        "TabLineFill",
        "TabLineSel",
        "TermCursor",
        "Title",
        "VertSplit",
        "Visual",
        "VisualNC",
        "WarningMsg",
        "Whitespace",
        "WildMenu",
        "WinBar",
        "WinBarNC",
        "WinSeparator",
    ];

    /// A `ChromeGroup` naming a group nvim never broadcasts can never be
    /// resolved from the live table: `Theme::from_hl` finds no id for it,
    /// every session paints it with its declared fallback, and no
    /// colorscheme the user installs can move it. Nothing fails -- the
    /// group simply looks hardcoded forever, which is the exact opposite of
    /// this type's contract.
    ///
    /// That is not hypothetical. `DiagnosticError` and `DiagnosticWarn`
    /// were declared here and shipped in precisely that state until
    /// 2026-08-09; the statusline's diagnostic counts resolved through them
    /// and never took a colorscheme's colors. This test is what makes the
    /// mistake loud, and it is the reason those two roles now resolve
    /// through [`ChromeGroup::ErrorMsg`] and [`ChromeGroup::WarningMsg`].
    #[test]
    fn every_group_is_one_nvim_broadcasts() {
        let broadcast: std::collections::HashSet<&str> = BROADCAST.into_iter().collect();
        for group in ChromeGroup::ALL {
            assert!(
                broadcast.contains(group.hl_name()),
                "{} is not in nvim's builtin highlight table, so hl_group_set never \
                 names it and this group can only ever hold its {:?} fallback -- pick \
                 the closest builtin nvim does broadcast, or re-capture BROADCAST if \
                 the engine pin added it",
                group.hl_name(),
                group.fallback()
            );
        }
    }

    /// Two groups sharing an `hl_name` would collide in the on-disk cache's
    /// name-keyed map, where one would silently overwrite the other.
    #[test]
    fn every_group_has_its_own_hl_name() {
        let names: std::collections::HashSet<&str> = ChromeGroup::ALL
            .into_iter()
            .map(ChromeGroup::hl_name)
            .collect();
        assert_eq!(names.len(), ChromeGroup::COUNT);
    }

    /// A style written for one group must be readable back from that group
    /// alone, for every arm: the storage layer's whole contract.
    #[test]
    fn set_chrome_writes_only_the_group_it_names() {
        let mut theme = Theme::default();
        for (offset, group) in ChromeGroup::ALL.into_iter().enumerate() {
            theme.set_chrome(
                group,
                ResolvedStyle {
                    fg: Some(0x10_0000 + offset as u32),
                    ..ResolvedStyle::default()
                },
            );
        }
        for (offset, group) in ChromeGroup::ALL.into_iter().enumerate() {
            assert_eq!(
                theme.chrome(group).fg,
                Some(0x10_0000 + offset as u32),
                "{} read back another group's style",
                group.hl_name()
            );
        }
    }

    /// Derivation covers every arm, not just the ones a consumer happens to
    /// paint today: a group added to the enum and left out of `from_hl`'s
    /// loop would resolve to `ResolvedStyle::default()` -- an all-unset
    /// style that is neither the colorscheme's answer nor the declared
    /// fallback.
    #[test]
    fn from_hl_resolves_every_group_in_all() {
        let mut hl = table_with(Some(0x101010), Some(0x202020), 1, no_attrs());
        for (offset, group) in ChromeGroup::ALL.into_iter().enumerate() {
            let id = 100 + offset as u64;
            hl.define_attr(
                id,
                HlAttr {
                    fg: Some(0x30_0000 + offset as u32),
                    bg: None,
                    bold: false,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
            );
            hl.set_group(group.hl_name().to_string(), id);
        }
        let theme = Theme::from_hl(&hl);
        for (offset, group) in ChromeGroup::ALL.into_iter().enumerate() {
            assert_eq!(
                theme.chrome(group).fg,
                Some(0x30_0000 + offset as u32),
                "{} was not resolved from the live table",
                group.hl_name()
            );
        }
    }

    #[test]
    fn from_hl_derives_default_colors_as_normal() {
        // bg is deliberately non-zero here: 0 is the wire-ambiguous case
        // covered by its own dedicated tests below, not this plain
        // straight-line-derivation one.
        let hl = table_with(Some(0xFFFFFF), Some(0x123456), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.fg, Some(0xFFFFFF));
        assert_eq!(theme.bg, Some(0x123456));
    }

    /// Derivation stability: the same `HlTable` state always derives an
    /// identical `Theme`, whether re-derived once or many times -- the
    /// property both a persisted-state round trip into a themed first
    /// paint and `paint`'s per-frame re-derivation depend on. Asserts
    /// concrete field values (not
    /// just that the three derivations match each other): a `from_hl` that
    /// always returned the same wrong `Theme` would still pass a
    /// three-way equality check alone.
    #[test]
    fn from_hl_is_stable_across_repeated_derivation() {
        let hl = table_with(Some(0x112233), Some(0x445566), 7, no_attrs());
        let first = Theme::from_hl(&hl);
        let second = Theme::from_hl(&hl);
        let third = Theme::from_hl(&hl);
        assert_eq!(first, second);
        assert_eq!(second, third);
        assert_eq!(first.fg, Some(0x112233));
        assert_eq!(first.bg, Some(0x445566));
    }

    /// Derivation reads live `Model.engine.hl` state built the same way
    /// production code builds it: through `update()` decoding real
    /// `DefaultColorsSet`/`HlAttrDefine`/`HlGroupSet` events, not
    /// hand-built structs.
    #[test]
    fn from_hl_derives_stably_from_events_decoded_through_update() {
        let mut model = Model::new();
        let _ = update(
            &mut model,
            Msg::Redraw(vec![
                crate::events::UiEvent::DefaultColorsSet {
                    fg: Some(0xABCDEF),
                    bg: Some(0x010203),
                    sp: None,
                },
                crate::events::UiEvent::HlAttrDefine {
                    id: 3,
                    fg: Some(0xFF0000),
                    bg: None,
                    bold: true,
                    italic: false,
                    underline: false,
                    reverse: false,
                },
                crate::events::UiEvent::HlGroupSet {
                    name: "StatusLine".to_string(),
                    hl_id: 3,
                },
            ]),
        );
        let first = Theme::from_hl(model.engine.hl());
        let second = Theme::from_hl(model.engine.hl());
        assert_eq!(first, second);
        assert_eq!(first.fg, Some(0xABCDEF));
        assert_eq!(first.bg, Some(0x010203));
        assert_eq!(
            first.style_for(3, model.engine.hl()),
            ResolvedStyle {
                fg: Some(0xFF0000),
                bg: Some(0x010203),
                bold: true,
                italic: false,
                underline: false,
                reverse: false,
            }
        );
        assert_eq!(
            first.chrome(ChromeGroup::StatusLine),
            first.style_for(3, model.engine.hl())
        );
    }

    #[test]
    fn style_for_unknown_hl_id_falls_back_to_normal() {
        let hl = table_with(Some(0x111111), Some(0x222222), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.style_for(999, &hl), theme.normal());
    }

    #[test]
    fn style_for_partial_override_keeps_unset_channel_at_default() {
        let hl = table_with(
            Some(0x111111),
            Some(0x222222),
            1,
            HlAttr {
                fg: Some(0x333333),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
        );
        let theme = Theme::from_hl(&hl);
        let style = theme.style_for(1, &hl);
        assert_eq!(style.fg, Some(0x333333));
        assert_eq!(style.bg, Some(0x222222));
    }

    #[test]
    fn style_for_reverse_sets_the_reverse_flag_not_swapped_values() {
        let hl = table_with(
            Some(0x111111),
            Some(0x222222),
            1,
            HlAttr {
                fg: Some(0xAAAAAA),
                bg: Some(0xBBBBBB),
                bold: false,
                italic: false,
                underline: false,
                reverse: true,
            },
        );
        let theme = Theme::from_hl(&hl);
        let style = theme.style_for(1, &hl);
        assert_eq!(style.fg, Some(0xAAAAAA));
        assert_eq!(style.bg, Some(0xBBBBBB));
        assert!(style.reverse);
    }

    #[test]
    fn normal_carries_no_text_attributes() {
        let hl = table_with(Some(0x1), Some(0x2), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        let style = theme.normal();
        assert!(!style.bold);
        assert!(!style.italic);
        assert!(!style.underline);
        assert!(!style.reverse);
    }

    #[test]
    fn emphasis_sets_reverse_over_normal_colors() {
        let hl = table_with(Some(0x1), Some(0x2), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        let style = theme.emphasis();
        assert_eq!(style.fg, Some(0x1));
        assert_eq!(style.bg, Some(0x2));
        assert!(style.reverse);
    }

    /// The load-bearing property this whole extension exists to prove: once
    /// nvim has named a group, `Theme` prefers its real resolved colors
    /// over the derived fallback, even where the two would otherwise
    /// collide by coincidence for an ordinary (non-selection) group.
    #[test]
    fn named_prefers_the_mapped_groups_colors_over_the_derived_fallback() {
        let mut hl = table_with(
            Some(0x000000),
            Some(0xFFFFFF),
            9,
            HlAttr {
                fg: Some(0x123456),
                bg: Some(0x654321),
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
        );
        hl.set_group("StatusLine".to_string(), 9);
        let theme = Theme::from_hl(&hl);
        let status_line = theme.chrome(ChromeGroup::StatusLine);
        assert_eq!(status_line.fg, Some(0x123456));
        assert_eq!(status_line.bg, Some(0x654321));
        assert_ne!(status_line, theme.normal());
    }

    /// The other half of the same property: a selection-style group with
    /// no mapping yet falls back to `emphasis()` (reverse-video), not
    /// `normal()`, so the selected/current row stays visually distinct.
    #[test]
    fn named_selection_group_falls_back_to_emphasis_when_unmapped() {
        let hl = table_with(Some(0x1), Some(0x2), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.chrome(ChromeGroup::TabLineSel), theme.emphasis());
        assert_eq!(theme.chrome(ChromeGroup::PmenuSel), theme.emphasis());
        assert_ne!(theme.chrome(ChromeGroup::TabLineSel), theme.normal());
    }

    /// And the plain-group counterpart: an ordinary (non-selection) group
    /// with no mapping yet falls back to `normal()`, not `emphasis()` --
    /// unmapped chrome should look like plain text, not spuriously
    /// reverse-video.
    #[test]
    fn named_plain_group_falls_back_to_normal_when_unmapped() {
        let hl = table_with(Some(0x1), Some(0x2), 1, no_attrs());
        let theme = Theme::from_hl(&hl);
        for group in ChromeGroup::ALL {
            if group.fallback() == ChromeFallback::Emphasis {
                continue;
            }
            assert_eq!(
                theme.chrome(group),
                theme.normal(),
                "{} must read as plain text while unmapped",
                group.hl_name()
            );
        }
    }

    /// `default_colors_set` alone cannot tell "no background" from
    /// "background is genuinely black" (nvim sends `rgb_bg = 0` for both),
    /// so `default_bg = Some(0)` on the wire must not survive into
    /// `Theme::bg` once a probe reply says the key was absent.
    ///
    /// A confirmed-unset `bg` is produced by the generation-matched branch
    /// and by the ambiguous-zero fallback alike, so this pins the property
    /// rather than either branch on its own; the branches are told apart by
    /// `probe_confirmed_bg_zero_for_the_current_generation_still_paints_black`
    /// and by
    /// `warm_start_ambiguous_wire_zero_prefers_the_stale_confirmed_value_while_a_probe_is_in_flight`.
    /// Disconfirm: collapsing `bg`'s derivation to the raw wire default --
    /// dropping the generation-matched branch and the ambiguous-zero
    /// fallback together -- makes this assert `Some(0)` instead of `None`,
    /// an all-black paint where the terminal's own background should show
    /// through instead.
    #[test]
    fn probe_confirmed_no_bg_overrides_the_wire_ambiguous_zero() {
        let mut hl = table_with(Some(0xF8F8F2), Some(0), 1, no_attrs());
        hl.confirm_defaults(crate::hl::ProbedDefaults {
            generation: hl.probe_generation(),
            fg: Some(0xF8F8F2),
            bg: None,
        });
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.bg, None, "confirmed-unset bg must never paint");
        assert_eq!(theme.fg, Some(0xF8F8F2));
    }

    /// The other half of the same property: a probe reply that confirms
    /// `bg = 0` (a genuinely-black theme, e.g. `guibg=#000000`) must keep
    /// painting black rather than being conflated with the unset case. The
    /// reply answers the generation currently open, so both channels read
    /// it in preference to the wire values.
    ///
    /// The wire defaults deliberately disagree with the reply, and the wire
    /// `bg` is deliberately non-zero: that is the only shape of state in
    /// which the generation-matched branch is observable at all. Wherever
    /// the two agree, or wherever the wire `bg` is the ambiguous zero, the
    /// stale-confirmed fallback derives the identical value, and the branch
    /// could be deleted outright with every assertion still passing.
    /// Reaching such a state live needs `nvim_get_hl`'s answer to differ
    /// from the `default_colors_set` that opened the same generation, so it
    /// is built here directly rather than driven from an event sequence.
    /// Disconfirm: deleting the generation-matched arm from either
    /// derivation makes this read the wire values instead -- `Some(0x1F1F1F)`
    /// for `fg`, `Some(0x2A2A2A)` for `bg`.
    #[test]
    fn probe_confirmed_bg_zero_for_the_current_generation_still_paints_black() {
        let mut hl = table_with(Some(0x1F1F1F), Some(0x2A2A2A), 1, no_attrs());
        hl.confirm_defaults(crate::hl::ProbedDefaults {
            generation: hl.probe_generation(),
            fg: Some(0xF8F8F2),
            bg: Some(0),
        });
        assert_eq!(
            hl.confirmed().map(|p| p.generation),
            Some(hl.probe_generation()),
            "the reply must answer the generation currently open"
        );
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.bg, Some(0), "a confirmed black must paint black");
        assert_eq!(
            theme.fg,
            Some(0xF8F8F2),
            "a matching reply is authoritative for fg as well as bg"
        );
    }

    /// Cold start: no probe reply has ever landed for this session
    /// (`confirmed: None`) and no theme cache seeded one either, so an
    /// ambiguous wire zero has no confirmed value to fall back to at all.
    /// Derivation treats it as unset rather than painting the raw wire
    /// zero, so a transparent config never flashes black on its very first
    /// `DefaultColorsSet` while the disambiguating probe is still in
    /// flight. `fg` carries no such ambiguity and still reads the raw wire
    /// value immediately.
    #[test]
    fn cold_start_ambiguous_wire_zero_with_no_confirmed_history_stays_unset() {
        let hl = table_with(Some(0x123456), Some(0x0), 1, no_attrs());
        assert_eq!(hl.confirmed(), None);
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.bg, None);
        assert_eq!(theme.fg, Some(0x123456));
    }

    /// Warm start: a confirmed value already exists from earlier this
    /// session (or, in production, from persisted state seeded before
    /// attach, carrying a matching generation), but it is now stale
    /// relative to a fresh `DefaultColorsSet`'s bumped generation, whose own
    /// probe reply has not landed yet. An ambiguous wire zero in that window
    /// prefers the stale-but-real confirmed value over the raw wire zero,
    /// so a colorscheme that was already known to be transparent never
    /// flashes black while its new probe is in flight.
    /// Disconfirm: reverting to the raw-wire fallback for this branch makes
    /// this assert `Some(0)` instead of `None` -- exactly the black flash
    /// this branch exists to remove.
    #[test]
    fn warm_start_ambiguous_wire_zero_prefers_the_stale_confirmed_value_while_a_probe_is_in_flight()
    {
        let mut hl = table_with(Some(0xF8F8F2), Some(0), 1, no_attrs());
        hl.confirm_defaults(crate::hl::ProbedDefaults {
            generation: hl.probe_generation(),
            fg: Some(0xF8F8F2),
            bg: None,
        });
        // a second default_colors_set opens a new generation, leaving the
        // reply above one behind and its own probe in flight
        let _ = hl.set_default_colors(Some(0xF8F8F2), Some(0));
        let theme = Theme::from_hl(&hl);
        assert_eq!(
            theme.bg, None,
            "a stale-but-known transparent bg must not be superseded by the raw wire zero"
        );
    }

    /// The colorscheme-switch case: a session was already confirmed
    /// genuinely black (not transparent), and a newer `DefaultColorsSet`'s
    /// probe is now in flight. The stale confirmed value it falls back to
    /// is itself `Some(0)`, so the frame keeps painting black through the
    /// switch rather than flashing transparent -- "prefer the last known
    /// value" cuts both directions, not just toward `None`.
    ///
    /// The opposed generation relationship to
    /// `probe_confirmed_bg_zero_for_the_current_generation_still_paints_black`
    /// is what this pins: there the reply answers the open generation and is
    /// read, here it answers a superseded one and is not. The reply's `fg`
    /// deliberately differs from the wire's so that difference is
    /// observable, since an unread reply leaves `fg` coming off the wire.
    /// Disconfirm: dropping the `p.generation == hl.probe_generation()`
    /// guard makes `fg` read `Some(0xC0FFEE)` instead.
    #[test]
    fn warm_start_ambiguous_wire_zero_keeps_a_stale_confirmed_black_while_a_probe_is_in_flight() {
        let mut hl = table_with(Some(0xFFFFFF), Some(0), 1, no_attrs());
        hl.confirm_defaults(crate::hl::ProbedDefaults {
            generation: hl.probe_generation(),
            fg: Some(0xC0FFEE),
            bg: Some(0),
        });
        // a second default_colors_set opens a new generation, leaving the
        // reply above one behind and its own probe in flight
        let _ = hl.set_default_colors(Some(0xFFFFFF), Some(0));
        assert_ne!(
            hl.confirmed().map(|p| p.generation),
            Some(hl.probe_generation()),
            "the reply must be one generation behind the open probe"
        );
        let theme = Theme::from_hl(&hl);
        assert_eq!(theme.bg, Some(0));
        assert_eq!(
            theme.fg,
            Some(0xFFFFFF),
            "a reply for a superseded generation must not be read"
        );
    }

    /// A `confirmed` value left over from a superseded `DefaultColorsSet`
    /// (its generation no longer matches `probe_generation`, because a
    /// newer `DefaultColorsSet` has since fired and bumped it) must not be
    /// read as authoritative for the current frame: `Theme::from_hl` falls
    /// back to the newer raw value, rather than painting a stale theme's
    /// disambiguated colors onto a new theme's frame.
    #[test]
    fn stale_generation_confirmed_value_is_not_read() {
        // bg is deliberately non-zero here: an unambiguous raw value beats
        // a stale confirmed value outright, with no fallback-to-confirmed
        // involved. The wire-ambiguous (zero) case is covered by the
        // warm_start_* tests above.
        let mut hl = table_with(Some(0x1), Some(0x282a36), 1, no_attrs());
        hl.confirm_defaults(crate::hl::ProbedDefaults {
            generation: hl.probe_generation(),
            fg: Some(0x1),
            bg: None,
        });
        // the newer colorscheme's default_colors_set, which supersedes the
        // reply above
        let _ = hl.set_default_colors(Some(0x1), Some(0x282a36));
        let theme = Theme::from_hl(&hl);
        assert_eq!(
            theme.bg,
            Some(0x282a36),
            "must read the fresh raw value, not the superseded confirmation"
        );
    }
}
