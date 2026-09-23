//! The omarchy chords a headless view session answers to, and the ones it
//! deliberately does not.
//!
//! View's keys are omarchy's keys on a machine with no desktop of its own,
//! and nvim's leader keys on one that has one (`docs/keymaps.md`, Key
//! profiles). [`DESKTOP_CHORDS`] is the 46-row table that decision produces:
//! every chord a session under [`KeyProfile::Desktop`] registers, both the
//! `Super`-modifier spelling a kitty-protocol terminal delivers and the
//! `Alt`-modifier spelling every terminal delivers.
//! [`UNBOUND`] is every remaining omarchy chord, with the reason a terminal
//! session has nothing to bind it to.

use crate::native::mappings::Rhs;

/// Which key vocabulary a session registers.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyProfile {
    /// The omarchy chords, for a machine whose desktop is not holding them.
    Desktop,
    /// nvim's own window keys and view's leader keys, and no chords.
    Editor,
}

/// What `[keys] desktop_modifier` resolved to, before the terminal's answer
/// is known.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModifierChoice {
    #[default]
    Auto,
    Super,
    Alt,
}

/// The modifier the chords of this session are spelled with.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopModifier {
    /// `<D-...>`, delivered by a terminal speaking the kitty keyboard
    /// protocol.
    Super,
    /// `<M-...>`, delivered by every terminal.
    Alt,
}

/// One desktop chord: where it comes from, both spellings, what it does,
/// and the key that does the same thing under the editor profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopChord {
    /// The `[keys.desktop]` key that rebinds it.
    pub id: &'static str,
    /// The omarchy chord it answers to, spelled as
    /// `default/hypr/bindings/*.lua` spells it (a digit-row chord written as
    /// the digit where the source binds the xkb `code:N`, the one
    /// place this table's own spelling and the fixture's diverge -- the
    /// `#[cfg(test)]` `the_desktop_table_matches_omarchy_by_chord` carries
    /// the normalization).
    pub omarchy: &'static str,
    /// Its spelling under [`DesktopModifier::Super`], in nvim notation.
    pub with_super: &'static str,
    /// Its spelling under [`DesktopModifier::Alt`], which the legacy `ESC`
    /// path has to be able to deliver.
    pub with_alt: &'static str,
    pub feature: &'static str,
    /// The verb `:View` answers with, and the `id` of this row for a
    /// [`Rhs::Keys`] chord, which [`super::mappings::is_spellable`] vets as
    /// a token and the registration chunk writes into
    /// `desc = 'view: <feature> <verb>'`.
    pub verb: &'static str,
    pub rhs: Rhs,
    /// The key bound under both profiles that does the same thing.
    pub twin: &'static str,
}

impl DesktopChord {
    /// This chord's spelling under `modifier`.
    #[must_use]
    pub const fn lhs(&self, modifier: DesktopModifier) -> &'static str {
        match modifier {
            DesktopModifier::Super => self.with_super,
            DesktopModifier::Alt => self.with_alt,
        }
    }
}

/// How many chords the table holds, so the two arrays sized by it stay one
/// number.
pub const DESKTOP_CHORD_COUNT: usize = 46;

// Ordered to match the design's own table (spec section 9): tile focus and
// move, tile lifecycle, palette/picker/tree, tabpages, resize, then the
// four chords with no tile-shaped twin.
static DESKTOP_CHORDS: [DesktopChord; DESKTOP_CHORD_COUNT] = [
    DesktopChord {
        id: "focus_left",
        omarchy: "SUPER + LEFT",
        with_super: "<D-Left>",
        with_alt: "<M-Left>",
        feature: "window",
        verb: "focus_left",
        rhs: Rhs::Keys("<C-w>h"),
        twin: "<C-w>h",
    },
    DesktopChord {
        id: "focus_right",
        omarchy: "SUPER + RIGHT",
        with_super: "<D-Right>",
        with_alt: "<M-Right>",
        feature: "window",
        verb: "focus_right",
        rhs: Rhs::Keys("<C-w>l"),
        twin: "<C-w>l",
    },
    DesktopChord {
        id: "focus_up",
        omarchy: "SUPER + UP",
        with_super: "<D-Up>",
        with_alt: "<M-Up>",
        feature: "window",
        verb: "focus_up",
        rhs: Rhs::Keys("<C-w>k"),
        twin: "<C-w>k",
    },
    DesktopChord {
        id: "focus_down",
        omarchy: "SUPER + DOWN",
        with_super: "<D-Down>",
        with_alt: "<M-Down>",
        feature: "window",
        verb: "focus_down",
        rhs: Rhs::Keys("<C-w>j"),
        twin: "<C-w>j",
    },
    DesktopChord {
        id: "move_left",
        omarchy: "SUPER + SHIFT + LEFT",
        with_super: "<S-D-Left>",
        with_alt: "<S-M-Left>",
        feature: "window",
        verb: "move_left",
        rhs: Rhs::Keys("<C-w>H"),
        twin: "<C-w>H",
    },
    DesktopChord {
        id: "move_right",
        omarchy: "SUPER + SHIFT + RIGHT",
        with_super: "<S-D-Right>",
        with_alt: "<S-M-Right>",
        feature: "window",
        verb: "move_right",
        rhs: Rhs::Keys("<C-w>L"),
        twin: "<C-w>L",
    },
    DesktopChord {
        id: "move_up",
        omarchy: "SUPER + SHIFT + UP",
        with_super: "<S-D-Up>",
        with_alt: "<S-M-Up>",
        feature: "window",
        verb: "move_up",
        rhs: Rhs::Keys("<C-w>K"),
        twin: "<C-w>K",
    },
    DesktopChord {
        id: "move_down",
        omarchy: "SUPER + SHIFT + DOWN",
        with_super: "<S-D-Down>",
        with_alt: "<S-M-Down>",
        feature: "window",
        verb: "move_down",
        rhs: Rhs::Keys("<C-w>J"),
        twin: "<C-w>J",
    },
    DesktopChord {
        id: "close",
        omarchy: "SUPER + W",
        with_super: "<D-w>",
        with_alt: "<M-w>",
        feature: "window",
        verb: "close",
        rhs: Rhs::Keys("<C-w>c"),
        twin: "<C-w>c",
    },
    DesktopChord {
        id: "close_alt",
        omarchy: "SUPER + Q",
        with_super: "<D-q>",
        with_alt: "<M-q>",
        feature: "window",
        verb: "close_alt",
        rhs: Rhs::Keys("<C-w>c"),
        twin: "<C-w>c",
    },
    DesktopChord {
        id: "new_tile",
        omarchy: "SUPER + RETURN",
        with_super: "<D-CR>",
        with_alt: "<M-CR>",
        feature: "window",
        verb: "new",
        rhs: Rhs::Invoke,
        twin: "<leader>wn",
    },
    DesktopChord {
        id: "zoom",
        omarchy: "SUPER + F",
        with_super: "<D-f>",
        with_alt: "<M-f>",
        feature: "window",
        verb: "zoom",
        rhs: Rhs::Invoke,
        twin: "<leader>wz",
    },
    DesktopChord {
        id: "full_width",
        omarchy: "SUPER + ALT + F",
        with_super: "<M-D-f>",
        with_alt: "<C-M-f>",
        feature: "window",
        verb: "full_width",
        rhs: Rhs::Keys("<C-w>|"),
        twin: "<C-w>|",
    },
    DesktopChord {
        id: "flip_split",
        omarchy: "SUPER + J",
        with_super: "<D-j>",
        with_alt: "<M-j>",
        feature: "window",
        verb: "flip",
        rhs: Rhs::Invoke,
        twin: "<leader>ws",
    },
    DesktopChord {
        id: "float",
        omarchy: "SUPER + T",
        with_super: "<D-t>",
        with_alt: "<M-t>",
        feature: "window",
        verb: "float",
        rhs: Rhs::Invoke,
        twin: "<leader>uf",
    },
    DesktopChord {
        id: "palette",
        omarchy: "SUPER + SPACE",
        with_super: "<D-Space>",
        with_alt: "<M-Space>",
        feature: "palette",
        verb: "open",
        rhs: Rhs::Invoke,
        twin: "<leader><leader>",
    },
    DesktopChord {
        id: "files",
        omarchy: "SUPER + ALT + SPACE",
        with_super: "<M-D-Space>",
        with_alt: "<C-M-Space>",
        feature: "picker",
        verb: "files",
        rhs: Rhs::Invoke,
        twin: "<leader>ff",
    },
    DesktopChord {
        id: "tree",
        omarchy: "SUPER + SHIFT + F",
        with_super: "<S-D-f>",
        with_alt: "<M-F>",
        feature: "tree",
        verb: "toggle",
        rhs: Rhs::Invoke,
        twin: "<leader>e",
    },
    DesktopChord {
        id: "tabpage_1",
        omarchy: "SUPER + 1",
        with_super: "<D-1>",
        with_alt: "<M-1>",
        feature: "window",
        verb: "tabpage_1",
        rhs: Rhs::Keys("1gt"),
        twin: "1gt",
    },
    DesktopChord {
        id: "tabpage_2",
        omarchy: "SUPER + 2",
        with_super: "<D-2>",
        with_alt: "<M-2>",
        feature: "window",
        verb: "tabpage_2",
        rhs: Rhs::Keys("2gt"),
        twin: "2gt",
    },
    DesktopChord {
        id: "tabpage_3",
        omarchy: "SUPER + 3",
        with_super: "<D-3>",
        with_alt: "<M-3>",
        feature: "window",
        verb: "tabpage_3",
        rhs: Rhs::Keys("3gt"),
        twin: "3gt",
    },
    DesktopChord {
        id: "tabpage_4",
        omarchy: "SUPER + 4",
        with_super: "<D-4>",
        with_alt: "<M-4>",
        feature: "window",
        verb: "tabpage_4",
        rhs: Rhs::Keys("4gt"),
        twin: "4gt",
    },
    DesktopChord {
        id: "tabpage_5",
        omarchy: "SUPER + 5",
        with_super: "<D-5>",
        with_alt: "<M-5>",
        feature: "window",
        verb: "tabpage_5",
        rhs: Rhs::Keys("5gt"),
        twin: "5gt",
    },
    DesktopChord {
        id: "tabpage_6",
        omarchy: "SUPER + 6",
        with_super: "<D-6>",
        with_alt: "<M-6>",
        feature: "window",
        verb: "tabpage_6",
        rhs: Rhs::Keys("6gt"),
        twin: "6gt",
    },
    DesktopChord {
        id: "tabpage_7",
        omarchy: "SUPER + 7",
        with_super: "<D-7>",
        with_alt: "<M-7>",
        feature: "window",
        verb: "tabpage_7",
        rhs: Rhs::Keys("7gt"),
        twin: "7gt",
    },
    DesktopChord {
        id: "tabpage_8",
        omarchy: "SUPER + 8",
        with_super: "<D-8>",
        with_alt: "<M-8>",
        feature: "window",
        verb: "tabpage_8",
        rhs: Rhs::Keys("8gt"),
        twin: "8gt",
    },
    DesktopChord {
        id: "tabpage_9",
        omarchy: "SUPER + 9",
        with_super: "<D-9>",
        with_alt: "<M-9>",
        feature: "window",
        verb: "tabpage_9",
        rhs: Rhs::Keys("9gt"),
        twin: "9gt",
    },
    DesktopChord {
        id: "to_tabpage_1",
        omarchy: "SUPER + SHIFT + 1",
        with_super: "<S-D-1>",
        with_alt: "<M-!>",
        feature: "window",
        verb: "to_tabpage_1",
        rhs: Rhs::Invoke,
        twin: "<leader>w1",
    },
    DesktopChord {
        id: "to_tabpage_2",
        omarchy: "SUPER + SHIFT + 2",
        with_super: "<S-D-2>",
        with_alt: "<M-@>",
        feature: "window",
        verb: "to_tabpage_2",
        rhs: Rhs::Invoke,
        twin: "<leader>w2",
    },
    DesktopChord {
        id: "to_tabpage_3",
        omarchy: "SUPER + SHIFT + 3",
        with_super: "<S-D-3>",
        with_alt: "<M-#>",
        feature: "window",
        verb: "to_tabpage_3",
        rhs: Rhs::Invoke,
        twin: "<leader>w3",
    },
    DesktopChord {
        id: "to_tabpage_4",
        omarchy: "SUPER + SHIFT + 4",
        with_super: "<S-D-4>",
        with_alt: "<M-$>",
        feature: "window",
        verb: "to_tabpage_4",
        rhs: Rhs::Invoke,
        twin: "<leader>w4",
    },
    DesktopChord {
        id: "to_tabpage_5",
        omarchy: "SUPER + SHIFT + 5",
        with_super: "<S-D-5>",
        with_alt: "<M-%>",
        feature: "window",
        verb: "to_tabpage_5",
        rhs: Rhs::Invoke,
        twin: "<leader>w5",
    },
    DesktopChord {
        id: "to_tabpage_6",
        omarchy: "SUPER + SHIFT + 6",
        with_super: "<S-D-6>",
        with_alt: "<M-^>",
        feature: "window",
        verb: "to_tabpage_6",
        rhs: Rhs::Invoke,
        twin: "<leader>w6",
    },
    DesktopChord {
        id: "to_tabpage_7",
        omarchy: "SUPER + SHIFT + 7",
        with_super: "<S-D-7>",
        with_alt: "<M-&>",
        feature: "window",
        verb: "to_tabpage_7",
        rhs: Rhs::Invoke,
        twin: "<leader>w7",
    },
    DesktopChord {
        id: "to_tabpage_8",
        omarchy: "SUPER + SHIFT + 8",
        with_super: "<S-D-8>",
        with_alt: "<M-*>",
        feature: "window",
        verb: "to_tabpage_8",
        rhs: Rhs::Invoke,
        twin: "<leader>w8",
    },
    DesktopChord {
        id: "to_tabpage_9",
        omarchy: "SUPER + SHIFT + 9",
        with_super: "<S-D-9>",
        with_alt: "<M-(>",
        feature: "window",
        verb: "to_tabpage_9",
        rhs: Rhs::Invoke,
        twin: "<leader>w9",
    },
    DesktopChord {
        id: "tabpage_next",
        omarchy: "SUPER + TAB",
        with_super: "<D-Tab>",
        with_alt: "<M-Tab>",
        feature: "window",
        verb: "tabpage_next",
        rhs: Rhs::Keys("gt"),
        twin: "gt",
    },
    DesktopChord {
        id: "tabpage_prev",
        omarchy: "SUPER + SHIFT + TAB",
        with_super: "<S-D-Tab>",
        with_alt: "<S-M-Tab>",
        feature: "window",
        verb: "tabpage_prev",
        rhs: Rhs::Keys("gT"),
        twin: "gT",
    },
    DesktopChord {
        id: "narrower",
        omarchy: "SUPER + code:20",
        with_super: "<D-->",
        with_alt: "<M-->",
        feature: "window",
        verb: "narrower",
        rhs: Rhs::Keys("<C-w><lt>"),
        twin: "<C-w><",
    },
    DesktopChord {
        id: "wider",
        omarchy: "SUPER + code:21",
        with_super: "<D-=>",
        with_alt: "<M-=>",
        feature: "window",
        verb: "wider",
        rhs: Rhs::Keys("<C-w>>"),
        twin: "<C-w>>",
    },
    DesktopChord {
        id: "shorter",
        omarchy: "SUPER + SHIFT + code:20",
        with_super: "<S-D-->",
        with_alt: "<M-_>",
        feature: "window",
        verb: "shorter",
        rhs: Rhs::Keys("<C-w>-"),
        twin: "<C-w>-",
    },
    DesktopChord {
        id: "taller",
        omarchy: "SUPER + SHIFT + code:21",
        with_super: "<S-D-=>",
        with_alt: "<M-+>",
        feature: "window",
        verb: "taller",
        rhs: Rhs::Keys("<C-w>+"),
        twin: "<C-w>+",
    },
    DesktopChord {
        id: "gaps",
        omarchy: "SUPER + SHIFT + BACKSPACE",
        with_super: "<S-D-BS>",
        with_alt: "<C-M-g>",
        feature: "ui",
        verb: "gaps",
        rhs: Rhs::Invoke,
        twin: "<leader>ug",
    },
    DesktopChord {
        id: "messages",
        omarchy: "SUPER + SHIFT + ALT + comma",
        with_super: "<S-M-D-,>",
        with_alt: "<C-M-n>",
        feature: "notifications",
        verb: "history",
        rhs: Rhs::Invoke,
        twin: "<leader>fm",
    },
    DesktopChord {
        id: "dismiss",
        omarchy: "SUPER + comma",
        with_super: "<D-,>",
        with_alt: "<M-,>",
        feature: "notifications",
        verb: "dismiss",
        rhs: Rhs::Invoke,
        twin: "<leader>fd",
    },
    DesktopChord {
        id: "agent",
        omarchy: "SUPER + SHIFT + CTRL + A",
        with_super: "<C-S-D-a>",
        with_alt: "<C-M-a>",
        feature: "ai",
        verb: "toggle",
        rhs: Rhs::Invoke,
        twin: "<leader>ai",
    },
];

/// Every fixture chord (`compat/fixtures/omarchy/bindings.txt`) no row of
/// [`DESKTOP_CHORDS`] binds, with the grounds it is unbound on: the
/// scratchpad (a hidden workspace has no tabpage or window analogue),
/// window groups (nvim windows do not stack inside a tile), the Hyprland
/// layout modes, the saved window width (`[ui.surfaces]` holds a width that
/// outlives a session), compositor surface properties, monitors (one
/// terminal is one screen), `ALT + TAB` (terminals hand it to the desktop
/// first), menus, application launching, capture and media, workspace moves
/// that skip the switch, the tenth workspace having no digit key, the fine
/// and coarse resize steps, notification chords with no verb yet, and power
/// and session.
static UNBOUND: [(&str, &str); 149] = [
    ("CTRL + ALT + DELETE", "power and session"),
    ("SUPER + P", "the Hyprland layout modes"),
    ("SUPER + CTRL + F", "compositor surface properties"),
    ("SUPER + O", "compositor surface properties"),
    ("SUPER + ALT + Home", "the saved window width"),
    ("SUPER + Home", "the saved window width"),
    ("SUPER + L", "the Hyprland layout modes"),
    (
        "SUPER + SHIFT + ALT + code:10",
        "workspace moves that skip the switch",
    ),
    (
        "SUPER + SHIFT + ALT + code:11",
        "workspace moves that skip the switch",
    ),
    (
        "SUPER + SHIFT + ALT + code:12",
        "workspace moves that skip the switch",
    ),
    (
        "SUPER + SHIFT + ALT + code:13",
        "workspace moves that skip the switch",
    ),
    (
        "SUPER + SHIFT + ALT + code:14",
        "workspace moves that skip the switch",
    ),
    (
        "SUPER + SHIFT + ALT + code:15",
        "workspace moves that skip the switch",
    ),
    (
        "SUPER + SHIFT + ALT + code:16",
        "workspace moves that skip the switch",
    ),
    (
        "SUPER + SHIFT + ALT + code:17",
        "workspace moves that skip the switch",
    ),
    (
        "SUPER + SHIFT + ALT + code:18",
        "workspace moves that skip the switch",
    ),
    ("SUPER + code:19", "the tenth workspace has no digit key"),
    (
        "SUPER + SHIFT + code:19",
        "the tenth workspace has no digit key",
    ),
    (
        "SUPER + SHIFT + ALT + code:19",
        "the tenth workspace has no digit key",
    ),
    ("SUPER + S", "the scratchpad"),
    ("SUPER + ALT + S", "the scratchpad"),
    ("SUPER + grave", "the scratchpad"),
    ("SUPER + SHIFT + grave", "the scratchpad"),
    ("SUPER + CTRL + TAB", "the Hyprland layout modes"),
    ("SUPER + SHIFT + ALT + LEFT", "monitors"),
    ("SUPER + SHIFT + ALT + RIGHT", "monitors"),
    ("SUPER + SHIFT + ALT + UP", "monitors"),
    ("SUPER + SHIFT + ALT + DOWN", "monitors"),
    ("ALT + TAB", "ALT + TAB"),
    ("ALT + SHIFT + TAB", "ALT + TAB"),
    ("CTRL + ALT + TAB", "monitors"),
    ("CTRL + ALT + SHIFT + TAB", "monitors"),
    ("SUPER + ALT + code:20", "the fine and coarse resize steps"),
    ("SUPER + ALT + code:21", "the fine and coarse resize steps"),
    (
        "SUPER + SHIFT + ALT + code:20",
        "the fine and coarse resize steps",
    ),
    (
        "SUPER + SHIFT + ALT + code:21",
        "the fine and coarse resize steps",
    ),
    ("SUPER + CTRL + code:20", "the fine and coarse resize steps"),
    ("SUPER + CTRL + code:21", "the fine and coarse resize steps"),
    (
        "SUPER + CTRL + SHIFT + code:20",
        "the fine and coarse resize steps",
    ),
    (
        "SUPER + CTRL + SHIFT + code:21",
        "the fine and coarse resize steps",
    ),
    ("SUPER + mouse_down", "the Hyprland layout modes"),
    ("SUPER + mouse_up", "the Hyprland layout modes"),
    ("SUPER + mouse:272", "compositor surface properties"),
    ("SUPER + mouse:273", "compositor surface properties"),
    ("SUPER + G", "window groups"),
    ("SUPER + ALT + G", "window groups"),
    ("SUPER + ALT + LEFT", "window groups"),
    ("SUPER + ALT + RIGHT", "window groups"),
    ("SUPER + ALT + UP", "window groups"),
    ("SUPER + ALT + DOWN", "window groups"),
    ("SUPER + ALT + TAB", "window groups"),
    ("SUPER + ALT + SHIFT + TAB", "window groups"),
    ("SUPER + CTRL + LEFT", "window groups"),
    ("SUPER + CTRL + RIGHT", "window groups"),
    ("SUPER + ALT + mouse_down", "window groups"),
    ("SUPER + ALT + mouse_up", "window groups"),
    ("SUPER + ALT + code:10", "window groups"),
    ("SUPER + ALT + code:11", "window groups"),
    ("SUPER + ALT + code:12", "window groups"),
    ("SUPER + ALT + code:13", "window groups"),
    ("SUPER + ALT + code:14", "window groups"),
    ("SUPER + SLASH", "monitors"),
    ("SUPER + ALT + SLASH", "monitors"),
    ("SUPER + CTRL + E", "menus"),
    ("SUPER + CTRL + C", "capture and media"),
    ("SUPER + CTRL + O", "menus"),
    ("SUPER + CTRL + H", "menus"),
    ("SUPER + SHIFT + code:201", "menus"),
    ("SUPER + ESCAPE", "menus"),
    ("XF86PowerOff", "power and session"),
    ("SUPER + K", "menus"),
    ("SUPER + ALT + K", "menus"),
    ("SUPER + CTRL + K", "menus"),
    ("SUPER + CTRL + Q", "application launching"),
    ("XF86Calculator", "application launching"),
    ("SUPER + SHIFT + SPACE", "menus"),
    ("SUPER + CTRL + SPACE", "menus"),
    ("SUPER + SHIFT + CTRL + SPACE", "menus"),
    ("SUPER + BACKSPACE", "compositor surface properties"),
    ("SUPER + CTRL + BACKSPACE", "compositor surface properties"),
    ("SUPER + CTRL + ALT + F", "compositor surface properties"),
    (
        "SUPER + SHIFT + comma",
        "notification chords with no verb yet: dismiss all",
    ),
    (
        "SUPER + CTRL + comma",
        "notification chords with no verb yet: silence",
    ),
    (
        "SUPER + ALT + comma",
        "notification chords with no verb yet: invoke last",
    ),
    ("SUPER + CTRL + I", "power and session"),
    ("SUPER + CTRL + N", "compositor surface properties"),
    ("SUPER + CTRL + Delete", "monitors"),
    ("SUPER + CTRL + ALT + Delete", "monitors"),
    ("switch:on:Lid Switch", "power and session"),
    ("switch:off:Lid Switch", "power and session"),
    ("PRINT", "capture and media"),
    ("ALT + PRINT", "capture and media"),
    ("SUPER + ALT + code:34", "capture and media"),
    ("SUPER + ALT + code:35", "capture and media"),
    ("SUPER + PRINT", "capture and media"),
    ("SUPER + CTRL + PRINT", "capture and media"),
    ("SUPER + CTRL + S", "menus"),
    ("SUPER + CTRL + PERIOD", "capture and media"),
    ("SUPER + CTRL + R", "menus"),
    ("SUPER + CTRL + ALT + R", "menus"),
    ("SUPER + SHIFT + CTRL + R", "menus"),
    ("SUPER + CTRL + ALT + T", "menus"),
    ("SUPER + CTRL + ALT + B", "menus"),
    ("SUPER + CTRL + ALT + W", "menus"),
    ("SUPER + CTRL + A", "menus"),
    ("SUPER + CTRL + B", "menus"),
    ("SUPER + CTRL + D", "menus"),
    ("SUPER + CTRL + ALT + D", "menus"),
    ("SUPER + CTRL + W", "menus"),
    ("SUPER + CTRL + P", "menus"),
    ("SUPER + CTRL + T", "menus"),
    ("SUPER + CTRL + Z", "compositor surface properties"),
    ("SUPER + CTRL + ALT + Z", "compositor surface properties"),
    ("SUPER + CTRL + L", "power and session"),
    ("SUPER + SHIFT + RETURN", "application launching"),
    ("SUPER + ALT + SHIFT + F", "application launching"),
    ("SUPER + SHIFT + B", "application launching"),
    ("SUPER + SHIFT + ALT + B", "application launching"),
    ("SUPER + SHIFT + N", "application launching"),
    ("SUPER + ALT + RETURN", "application launching"),
    ("SUPER + CTRL + RETURN", "application launching"),
    ("SUPER + SHIFT + M", "application launching"),
    ("SUPER + SHIFT + ALT + M", "application launching"),
    ("SUPER + SHIFT + D", "application launching"),
    ("SUPER + SHIFT + G", "application launching"),
    ("SUPER + SHIFT + O", "application launching"),
    ("SUPER + SHIFT + W", "application launching"),
    ("SUPER + SHIFT + SLASH", "application launching"),
    ("SUPER + SHIFT + A", "application launching"),
    ("SUPER + SHIFT + ALT + A", "application launching"),
    ("SUPER + SHIFT + C", "application launching"),
    ("SUPER + SHIFT + E", "application launching"),
    ("SUPER + SHIFT + ALT + E", "application launching"),
    ("SUPER + SHIFT + Y", "application launching"),
    ("SUPER + SHIFT + ALT + G", "application launching"),
    ("SUPER + SHIFT + CTRL + G", "application launching"),
    ("SUPER + SHIFT + P", "application launching"),
    ("SUPER + SHIFT + S", "application launching"),
    ("SUPER + SHIFT + X", "application launching"),
    ("SUPER + SHIFT + ALT + X", "application launching"),
    ("SUPER + CTRL + code:10", "menus"),
    ("SUPER + CTRL + code:11", "menus"),
    ("SUPER + CTRL + code:12", "menus"),
    ("SUPER + CTRL + code:13", "menus"),
    ("SUPER + CTRL + code:14", "menus"),
    ("SUPER + CTRL + code:15", "menus"),
    ("SUPER + CTRL + code:16", "menus"),
    ("SUPER + CTRL + code:17", "menus"),
    ("SUPER + CTRL + code:18", "menus"),
];

/// Every chord this build registers under [`KeyProfile::Desktop`], in the
/// table's own order.
#[must_use]
pub fn desktop_chords() -> &'static [DesktopChord] {
    &DESKTOP_CHORDS
}

/// The row whose `[keys.desktop]` id is `id`, or `None` for an id no row
/// carries.
#[must_use]
pub fn desktop_chord(id: &str) -> Option<&'static DesktopChord> {
    DESKTOP_CHORDS.iter().find(|chord| chord.id == id)
}

/// Every omarchy chord [`DESKTOP_CHORDS`] does not bind, with the grounds.
#[must_use]
pub fn unbound() -> &'static [(&'static str, &'static str)] {
    &UNBOUND
}

/// [`DESKTOP_CHORDS`] as a markdown table, so `docs/keymaps.md` and the
/// chords this build actually registers cannot disagree: both read
/// [`desktop_chords`]. The same reason [`super::mappings::render_table`]
/// exists for [`super::mappings::default_maps`].
#[must_use]
pub fn render_chord_table() -> String {
    let mut out = String::from(
        "| omarchy chord | `[keys.desktop]` row | super | alt | reaches | twin |\n\
         | --- | --- | --- | --- | --- | --- |\n",
    );
    for chord in desktop_chords() {
        let reaches = match chord.rhs {
            Rhs::Invoke => format!(":View {} {}", chord.feature, chord.verb),
            Rhs::Keys(keys) => keys.to_string(),
        };
        out.push_str(&format!(
            "| `{}` | `{}` | `{}` | `{}` | `{reaches}` | `{}` |\n",
            chord.omarchy, chord.id, chord.with_super, chord.with_alt, chord.twin
        ));
    }
    out
}

/// [`UNBOUND`] as a markdown table, for the same reason
/// [`render_chord_table`] exists.
#[must_use]
pub fn render_unbound_table() -> String {
    let mut out = String::from("| omarchy chord | unbound on |\n| --- | --- |\n");
    for (chord, why) in unbound() {
        out.push_str(&format!("| `{chord}` | {why} |\n"));
    }
    out
}

/// The omarchy chord `bindings.txt` spells with the xkb digit-row code this
/// table's own [`DesktopChord::omarchy`] and [`UNBOUND`] cells spell as the
/// plain digit (`SUPER + 1` for the fixture's `SUPER + code:10`), or `None`
/// for a chord this normalization does not touch.
///
/// The design table (spec section 9) writes the nine workspace-switch
/// chords as digits for readability; every other `code:N` chord
/// (`code:20`/`code:21` for the resize keys, `code:34`/`code:35` for the
/// webcam-overlay keys, `code:201` for the root menu) is left as the
/// fixture spells it, since xkb's numbering is the only name those keys
/// have.
#[cfg(test)]
fn digit_row_spelling(fixture_chord: &str) -> Option<String> {
    let (prefix, code) = fixture_chord.rsplit_once("code:")?;
    let code: u32 = code.parse().ok()?;
    if !(10..=19).contains(&code) {
        return None;
    }
    let digit = if code == 19 { 0 } else { code - 9 };
    Some(format!("{prefix}{digit}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::native::keys::well_formed;
    use crate::native::mappings::{default_maps, is_reachable_feature};
    use std::collections::HashSet;

    #[test]
    fn every_chord_spelling_is_well_formed() {
        for chord in desktop_chords() {
            assert!(
                well_formed(chord.with_super),
                "{}'s with_super {} is not a spelling this build could ever decode",
                chord.id,
                chord.with_super
            );
            assert!(
                well_formed(chord.with_alt),
                "{}'s with_alt {} is not a spelling this build could ever decode",
                chord.id,
                chord.with_alt
            );
        }
    }

    #[test]
    fn no_two_chords_share_a_spelling_under_either_modifier() {
        let mut supers = HashSet::new();
        let mut alts = HashSet::new();
        for chord in desktop_chords() {
            assert!(
                supers.insert(chord.with_super),
                "{} is not the first row to claim {}",
                chord.id,
                chord.with_super
            );
            assert!(
                alts.insert(chord.with_alt),
                "{} is not the first row to claim {}",
                chord.id,
                chord.with_alt
            );
        }
    }

    /// The twin is either an nvim key view registers nothing for (starts
    /// with `<C-w>` or is a bare `gt`/`gT`/digit-prefixed `gt`) or an
    /// existing [`default_maps`] row, so a chord naming a twin that could
    /// never become a key fails here, ahead of registration time.
    #[test]
    fn every_desktop_chord_has_an_editor_twin() {
        for chord in desktop_chords() {
            let native_key = chord.twin.starts_with("<C-w>")
                || chord.twin == "gt"
                || chord.twin == "gT"
                || (chord.twin.ends_with("gt")
                    && chord.twin[..chord.twin.len() - 2].parse::<u32>().is_ok());
            let default_map_row = default_maps().iter().any(|spec| spec.lhs == chord.twin);
            assert!(
                native_key || default_map_row,
                "{}'s twin {} is neither an nvim key nor a default_maps() row",
                chord.id,
                chord.twin
            );
        }
    }

    /// Every chord's feature is reachable through the registry or
    /// [`crate::native::mappings::REGISTRY_EXEMPT_FEATURES`] -- the off
    /// switch a claim notice needs exists the moment the chord does.
    /// `every_registered_feature_invoke_has_a_dispatch_handler` in
    /// `view-core::update::tests` is the pin that refuses a form with
    /// nothing behind it.
    #[test]
    fn every_chord_names_a_reachable_feature_or_a_command_only_form() {
        for chord in desktop_chords() {
            assert!(
                is_reachable_feature(chord.feature),
                "{} names {}, which is neither a registry feature nor a REGISTRY_EXEMPT_FEATURES row",
                chord.id,
                chord.feature
            );
        }
    }

    #[test]
    fn the_desktop_table_matches_omarchy_by_chord() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../compat/fixtures/omarchy/bindings.txt");
        let fixture = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        let mut fixture_chords = Vec::new();
        for line in fixture.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let after = line
                .split_once('(')
                .expect("every non-comment line is a bind call")
                .1;
            let chord = after
                .split_once('"')
                .and_then(|(_, rest)| rest.split_once('"'))
                .map(|(chord, _)| chord)
                .expect("every bind call's first argument is a quoted chord");
            fixture_chords.push(chord.to_string());
        }
        assert!(
            !fixture_chords.is_empty(),
            "the fixture named no chord at all"
        );

        let table_omarchy: HashSet<&str> = desktop_chords().iter().map(|c| c.omarchy).collect();
        let unbound_omarchy: HashSet<&str> = unbound().iter().map(|(chord, _)| *chord).collect();

        for chord in &fixture_chords {
            let normalized = digit_row_spelling(chord);
            let spelling = normalized.as_deref().unwrap_or(chord.as_str());
            assert!(
                table_omarchy.contains(spelling) || unbound_omarchy.contains(chord.as_str()),
                "fixture chord {chord} (normalized: {spelling}) is neither a DESKTOP_CHORDS row nor an UNBOUND row"
            );
        }

        let fixture_set: HashSet<&str> = fixture_chords.iter().map(String::as_str).collect();
        for chord in desktop_chords() {
            let matches = fixture_set.contains(chord.omarchy)
                || fixture_chords
                    .iter()
                    .any(|f| digit_row_spelling(f).as_deref() == Some(chord.omarchy));
            assert!(
                matches,
                "{}'s omarchy cell {} names no line of the fixture",
                chord.id, chord.omarchy
            );
        }
        for (chord, _) in unbound() {
            assert!(
                fixture_set.contains(chord),
                "UNBOUND names {chord}, which is no line of the fixture"
            );
        }
    }

    #[test]
    fn lhs_answers_the_modifier_it_is_asked_for() {
        let chord = desktop_chord("focus_left").expect("focus_left is a row");
        assert_eq!(chord.lhs(DesktopModifier::Super), "<D-Left>");
        assert_eq!(chord.lhs(DesktopModifier::Alt), "<M-Left>");
    }

    #[test]
    fn desktop_chord_finds_a_row_by_id_and_nothing_by_a_stranger() {
        assert!(desktop_chord("focus_left").is_some());
        assert!(desktop_chord("does-not-exist").is_none());
    }

    /// The keys page carries the same two generated tables this build
    /// walks, for the reason `view-native`'s
    /// `the_keys_page_renders_the_table_this_build_registers` pins
    /// `render_table`: a chord added here and not to the page is a key a
    /// reader presses with no documentation of it.
    #[test]
    fn the_generated_chord_table_matches_the_chord_walk() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/keymaps.md");
        let page = std::fs::read_to_string(&path).expect("docs/keymaps.md must be readable");
        let chords = render_chord_table();
        assert!(
            page.contains(&chords),
            "docs/keymaps.md is stale, it must carry:\n{chords}"
        );
        let unbound_table = render_unbound_table();
        assert!(
            page.contains(&unbound_table),
            "docs/keymaps.md is stale, it must carry:\n{unbound_table}"
        );
    }
}
