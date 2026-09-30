//! The file tree's glyphs: which glyph set a session draws, and the Nerd
//! Font icon and colour a file or folder row opens with.
//!
//! The table is view's own data, taken from nvim-web-devicons' default set:
//! each entry carries the glyph and the hex colour that plugin gives the
//! same name or extension, and a name it does not list falls to the same
//! generic file glyph it falls to there. A name is looked up the way that
//! plugin looks it up: lower-cased, as an exact file name first, and then
//! by every dotted suffix from the first dot onward, so `a.tar.gz` tries
//! `tar.gz` and then `gz`.

/// Which glyphs a tree row opens with, chosen by `[ui] tree_icons`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TreeIcons {
    /// A Nerd Font folder glyph on a folder row and a file-type icon in its
    /// own colour on a file row.
    Nerd,
    /// A `+` on a closed folder, a `-` on an open one and a blank on a
    /// file, for a font without Nerd Font glyphs.
    #[default]
    None,
}

impl TreeIcons {
    /// The word a user writes for this answer, and the word a report
    /// prints.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Nerd => "nerd",
            Self::None => "none",
        }
    }

    /// The answer `value` spells, or `None` for a word this build does not
    /// know. `auto` returns `None`, which the config layer reads as no
    /// choice made.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "nerd" => Some(Self::Nerd),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// What `"auto"` resolves to: Nerd Font glyphs where the terminal was
    /// probed to draw box glyphs one cell wide, the one fact view has
    /// about the glyphs a font carries, and the markers elsewhere.
    #[must_use]
    pub const fn derived(unicode_boxes: bool) -> Self {
        if unicode_boxes {
            Self::Nerd
        } else {
            Self::None
        }
    }
}

/// One glyph and the colour it is painted in, as `0xRRGGBB`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Devicon {
    pub glyph: &'static str,
    pub color: u32,
}

/// A closed folder's glyph.
pub const FOLDER_CLOSED: &str = "\u{e5ff}";

/// An open folder's glyph.
pub const FOLDER_OPEN: &str = "\u{e5fe}";

/// The glyph a file with no entry of its own opens with.
pub const DEFAULT_FILE: Devicon = Devicon {
    glyph: "\u{f0f6}",
    color: 0x006d_8086,
};

/// Exact file names, lower-cased, sorted for the binary search.
const BY_NAME: &[(&str, &str, u32)] = &[
    (".git-blame-ignore-revs", "\u{e702}", 0x00f5_4d27),
    (".gitattributes", "\u{e702}", 0x00f5_4d27),
    (".gitconfig", "\u{e615}", 0x00f5_4d27),
    (".gitignore", "\u{e702}", 0x00f5_4d27),
    (".gitmodules", "\u{e702}", 0x00f5_4d27),
    ("commit_editmsg", "\u{e702}", 0x00f5_4d27),
    ("copying", "\u{e60a}", 0x00cb_cb41),
    ("dockerfile", "\u{f0868}", 0x0045_8ee6),
    ("license", "\u{e60a}", 0x00d0_bf41),
    ("license.md", "\u{e60a}", 0x00d0_bf41),
    ("makefile", "\u{e779}", 0x006d_8086),
    ("readme.md", "\u{f00ba}", 0x00ed_eded),
    ("taskfile.yaml", "\u{f01a6}", 0x0069_d3c9),
    ("taskfile.yml", "\u{f01a6}", 0x0069_d3c9),
];

/// Extensions without their dot, lower-cased, sorted for the binary
/// search.
const BY_EXTENSION: &[(&str, &str, u32)] = &[
    ("bash", "\u{e760}", 0x0089_e051),
    ("c", "\u{e61e}", 0x0059_9eff),
    ("cpp", "\u{e61d}", 0x0051_9aba),
    ("css", "\u{e6b8}", 0x0066_3399),
    ("diff", "\u{e728}", 0x0041_535b),
    ("gif", "\u{e60d}", 0x00a0_74c4),
    ("git", "\u{e702}", 0x00f1_4c28),
    ("go", "\u{e627}", 0x0000_add8),
    ("h", "\u{f0fd}", 0x00a0_74c4),
    ("hpp", "\u{f0fd}", 0x00a0_74c4),
    ("html", "\u{e736}", 0x00e4_4d26),
    ("jpeg", "\u{e60d}", 0x00a0_74c4),
    ("jpg", "\u{e60d}", 0x00a0_74c4),
    ("js", "\u{e60c}", 0x00cb_cb41),
    ("json", "\u{e60b}", 0x00cb_cb41),
    ("jsx", "\u{e625}", 0x0020_c2e3),
    ("license", "\u{e60a}", 0x00cb_cb41),
    ("lock", "\u{e672}", 0x00bb_bbbb),
    ("lua", "\u{e620}", 0x0051_a0cf),
    ("md", "\u{f48a}", 0x00dd_dddd),
    ("patch", "\u{e728}", 0x0041_535b),
    ("png", "\u{e60d}", 0x00a0_74c4),
    ("py", "\u{e606}", 0x00ff_bc03),
    ("rs", "\u{e68b}", 0x00de_a584),
    ("sh", "\u{e795}", 0x004d_5a5e),
    ("svg", "\u{f0721}", 0x00ff_b13b),
    ("toml", "\u{e6b2}", 0x009c_4221),
    ("ts", "\u{e628}", 0x0051_9aba),
    ("tsx", "\u{e7ba}", 0x0013_54bf),
    ("txt", "\u{f0219}", 0x0089_e051),
    ("vim", "\u{e62b}", 0x0001_9833),
    ("yaml", "\u{e8eb}", 0x00d7_0000),
    ("yml", "\u{e8eb}", 0x00d7_0000),
    ("zsh", "\u{e795}", 0x0089_e051),
];

fn find(table: &[(&str, &'static str, u32)], key: &str) -> Option<Devicon> {
    table
        .binary_search_by(|(k, _, _)| (*k).cmp(key))
        .ok()
        .map(|i| Devicon {
            glyph: table[i].1,
            color: table[i].2,
        })
}

/// The icon a file called `name` opens with.
#[must_use]
pub fn file_icon(name: &str) -> Devicon {
    let name = name.to_lowercase();
    if let Some(icon) = find(BY_NAME, &name) {
        return icon;
    }
    let mut rest = name.as_str();
    while let Some((_, suffix)) = rest.split_once('.') {
        if let Some(icon) = find(BY_EXTENSION, suffix) {
            return icon;
        }
        rest = suffix;
    }
    DEFAULT_FILE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_icon_tables_are_sorted_for_their_binary_search() {
        for table in [BY_NAME, BY_EXTENSION] {
            assert!(
                table.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "a table out of order hides every entry the search skips"
            );
        }
    }

    #[test]
    fn tree_icon_for_a_rust_file_is_the_rust_glyph_in_its_colour() {
        let icon = file_icon("main.rs");
        assert_eq!(icon.glyph, "\u{e68b}");
        assert_eq!(icon.color, 0x00de_a584);
    }

    #[test]
    fn tree_icon_for_an_unknown_extension_is_the_generic_file_glyph() {
        assert_eq!(file_icon("notes.zzz"), DEFAULT_FILE);
        assert_eq!(file_icon("LICENSE-MIT"), DEFAULT_FILE);
    }

    #[test]
    fn tree_icon_matches_an_exact_name_before_its_extension() {
        assert_eq!(file_icon("Taskfile.yml").color, 0x0069_d3c9);
        assert_eq!(file_icon("other.yml").color, 0x00d7_0000);
        assert_eq!(file_icon(".gitignore").glyph, "\u{e702}");
        assert_eq!(file_icon("Cargo.lock").glyph, "\u{e672}");
        assert_eq!(file_icon("archive.tar.json").glyph, "\u{e60b}");
    }

    #[test]
    fn tree_icons_follow_the_terminal_under_auto_and_the_user_when_forced() {
        let mut model = crate::model::Model::new();
        model.caps.unicode_boxes = true;
        assert_eq!(model.tree_icons_shown(), TreeIcons::Nerd);
        model.caps.unicode_boxes = false;
        assert_eq!(model.tree_icons_shown(), TreeIcons::None);
        let model = model.with_tree_icons(Some(TreeIcons::Nerd));
        assert_eq!(model.tree_icons_shown(), TreeIcons::Nerd);
    }

    #[test]
    fn tree_icons_parse_their_two_words_and_refuse_others() {
        assert_eq!(TreeIcons::parse("nerd"), Some(TreeIcons::Nerd));
        assert_eq!(TreeIcons::parse("none"), Some(TreeIcons::None));
        assert_eq!(TreeIcons::parse("emoji"), None);
    }
}
