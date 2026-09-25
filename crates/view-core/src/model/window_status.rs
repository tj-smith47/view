//! What one window's own status segments read.
//!
//! The session-wide [`StatuslineState`](crate::native::statusline::StatuslineState)
//! answers for the window the cursor is in and for nothing else, and under
//! tiles every frame draws segments of its own. So the bridge reports one
//! of these per window that changed, and the model keeps the last one it
//! heard for each.

use std::collections::BTreeMap;

use crate::native::geometry::NativeSurface;
use crate::native::views::{Span, StyleRole};

/// `[ui] tile_titles`: the title a user gives a tile by its buffer's
/// filetype, ahead of the filetype itself.
pub type TileTitles = BTreeMap<String, String>;

/// One window's buffer identity, cursor position and diagnostic counts, as
/// the bridge's `window` trigger group reports them.
///
/// Keyed by [`WinHandle`](crate::events::WinHandle), with no grid in the
/// key, because the trigger runs in Lua where a window handle is the only
/// identity nvim offers. The painter maps a grid to its handle through
/// [`GridRegistry::window_handle`](crate::grid::registry::GridRegistry::window_handle).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowStatus {
    /// The buffer the window is showing.
    pub buf: u64,
    /// That buffer's tail name, empty for one that has never been named.
    /// For a terminal it is the tail of the job's program.
    pub name: String,
    /// Whether the buffer has unsaved changes.
    pub modified: bool,
    /// The cursor's line, 1-based as nvim reports it.
    pub row: u32,
    /// The cursor's column, 1-based as nvim reports it.
    pub col: u32,
    /// Error-severity diagnostics in the buffer.
    pub errors: u32,
    /// Warning-severity diagnostics in the buffer.
    pub warnings: u32,
    /// What the window holds, which decides its title and its segments.
    pub kind: TileKind,
    /// The buffer's line count.
    pub lines: u32,
}

/// What a tile holds, from the facts nvim reports about its buffer, or the
/// view surface that took its window.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum TileKind {
    /// A buffer backed by a file, `buftype = ""`.
    #[default]
    File,
    /// A help page.
    Help,
    /// The quickfix list.
    Quickfix,
    /// A window's location list.
    LocationList,
    /// A terminal job.
    Terminal,
    /// A `prompt` buffer, named by its filetype.
    Prompt {
        /// The buffer's filetype, empty where none is set.
        filetype: String,
    },
    /// A `nofile` buffer in a window held at a fixed width, which is how a
    /// file tree, an outline or a debugger panel keeps its column.
    Sidebar {
        /// The buffer's filetype, empty where none is set.
        filetype: String,
    },
    /// Any other buffer backed by no file.
    Scratch {
        /// The buffer's filetype, empty where none is set.
        filetype: String,
    },
    /// A window a view surface paints.
    Native(NativeSurface),
}

/// Which segments a tile's bottom edge carries.
///
/// `mode` and `showcmd` describe the session and show on the active tile
/// alone; the rest describe the tile's own window.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segments {
    /// The mode message.
    pub mode: bool,
    /// The git branch.
    pub branch: bool,
    /// The error and warning counts, each shown only above zero.
    pub diagnostics: bool,
    /// `row:col`.
    pub position: bool,
    /// `row/lines`.
    pub count: bool,
    /// The pending command.
    pub showcmd: bool,
}

impl Segments {
    const NONE: Self = Self {
        mode: false,
        branch: false,
        diagnostics: false,
        position: false,
        count: false,
        showcmd: false,
    };
}

impl TileKind {
    /// The kind nvim's own facts about a buffer name.
    ///
    /// Every buffer that no file backs and whose `buftype` has no meaning
    /// of its own is a scratch buffer, a `buftype` nvim adds later
    /// included.
    #[must_use]
    pub fn classify(buftype: &str, filetype: &str, loclist: bool) -> Self {
        Self::classify_window(buftype, filetype, loclist, false)
    }

    /// The kind nvim's own facts about a window and its buffer name.
    ///
    /// Every buffer that no file backs and whose `buftype` has no meaning
    /// of its own is a scratch buffer, a `buftype` nvim adds later
    /// included. A `nofile` one in a window whose `winfixwidth` is set is a
    /// sidebar: every tree, outline and panel plugin sets that option so a
    /// split beside it leaves its column alone, and a named `buftype` keeps
    /// its own row whatever the window holds.
    #[must_use]
    pub fn classify_window(
        buftype: &str,
        filetype: &str,
        loclist: bool,
        fixed_width: bool,
    ) -> Self {
        match buftype {
            "help" => Self::Help,
            "quickfix" if loclist => Self::LocationList,
            "quickfix" => Self::Quickfix,
            "terminal" => Self::Terminal,
            "prompt" => Self::Prompt {
                filetype: filetype.to_owned(),
            },
            "" => Self::File,
            "nofile" if fixed_width => Self::Sidebar {
                filetype: filetype.to_owned(),
            },
            _ => Self::Scratch {
                filetype: filetype.to_owned(),
            },
        }
    }

    /// The title a tile of this kind carries on its frame.
    #[must_use]
    pub fn title(&self, status: &WindowStatus) -> Vec<Span> {
        self.title_with(status, &TileTitles::new())
    }

    /// The title a tile of this kind carries on its frame, where `titles`
    /// names a tile titled by its filetype.
    #[must_use]
    pub fn title_with(&self, status: &WindowStatus, titles: &TileTitles) -> Vec<Span> {
        let name = status.name.as_str();
        let mapped = |filetype: &str| titles.get(filetype).map_or("", String::as_str);
        let text = match self {
            Self::File => {
                if name.is_empty() {
                    return Vec::new();
                }
                let mut spans = vec![Span::new(name, StyleRole::File)];
                if status.modified {
                    spans.push(Span::new(" [+]", StyleRole::Modified));
                }
                return spans;
            }
            Self::Help => match name.strip_suffix(".txt").unwrap_or(name) {
                "" => "help".to_owned(),
                page => format!("help: {page}"),
            },
            Self::Quickfix => "quickfix".to_owned(),
            Self::LocationList => "location list".to_owned(),
            Self::Terminal if name.is_empty() => "terminal".to_owned(),
            Self::Terminal => format!("terminal: {name}"),
            Self::Prompt { filetype } => first_of(&[mapped(filetype), filetype], "prompt"),
            Self::Sidebar { filetype } | Self::Scratch { filetype } => {
                first_of(&[mapped(filetype), filetype, name], "scratch")
            }
            Self::Native(surface) => surface.id().to_owned(),
        };
        vec![Span::new(text, StyleRole::Title)]
    }

    /// The segments a tile of this kind carries on its bottom edge.
    #[must_use]
    pub const fn segments(&self) -> Segments {
        match self {
            Self::File => Segments {
                mode: true,
                branch: true,
                diagnostics: true,
                position: true,
                showcmd: true,
                ..Segments::NONE
            },
            Self::Help => Segments {
                position: true,
                ..Segments::NONE
            },
            Self::Quickfix | Self::LocationList => Segments {
                count: true,
                ..Segments::NONE
            },
            Self::Terminal | Self::Prompt { .. } => Segments {
                mode: true,
                ..Segments::NONE
            },
            Self::Native(NativeSurface::Tree) | Self::Sidebar { .. } => Segments {
                branch: true,
                ..Segments::NONE
            },
            Self::Scratch { .. } => Segments {
                diagnostics: true,
                ..Segments::NONE
            },
            Self::Native(_) => Segments::NONE,
        }
    }
}

/// The first non-empty candidate, else `fallback`.
fn first_of(candidates: &[&str], fallback: &str) -> String {
    candidates
        .iter()
        .find(|text| !text.is_empty())
        .copied()
        .unwrap_or(fallback)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row of the kind table: what nvim reports, the kind it names, the
    /// title a tile named `name` carries and the segments it keeps.
    struct Row {
        buftype: &'static str,
        filetype: &'static str,
        loclist: bool,
        fixed_width: bool,
        kind: TileKind,
        title: &'static str,
        segments: Segments,
    }

    const FILE: Segments = Segments {
        mode: true,
        branch: true,
        diagnostics: true,
        position: true,
        count: false,
        showcmd: true,
    };
    const POSITION: Segments = Segments {
        position: true,
        ..Segments::NONE
    };
    const COUNT: Segments = Segments {
        count: true,
        ..Segments::NONE
    };
    const MODE: Segments = Segments {
        mode: true,
        ..Segments::NONE
    };
    const BRANCH: Segments = Segments {
        branch: true,
        ..Segments::NONE
    };
    const DIAGNOSTICS: Segments = Segments {
        diagnostics: true,
        ..Segments::NONE
    };

    /// A row read with the window's `winfixwidth` off; [`table`] sets it.
    fn row(
        buftype: &'static str,
        filetype: &'static str,
        loclist: bool,
        kind: TileKind,
        title: &'static str,
        segments: Segments,
    ) -> Row {
        Row {
            buftype,
            filetype,
            loclist,
            fixed_width: false,
            kind,
            title,
            segments,
        }
    }

    /// The table in docs/tiled-ui.md as data. Every buftype nvim documents
    /// and one it does not, crossed with no filetype, a tree plugin's and
    /// an ordinary one, with `loclist` and `winfixwidth` each on and off, so
    /// a plugin's filetype on any buftype, a fixed-width window holding any
    /// buftype and a buftype nvim adds later all land on a row.
    fn table(name: &str) -> Vec<Row> {
        let mut rows = Vec::new();
        for fixed_width in [false, true] {
            for mut row in rows_at(name, fixed_width) {
                row.fixed_width = fixed_width;
                rows.push(row);
            }
        }
        rows
    }

    fn rows_at(name: &str, fixed_width: bool) -> Vec<Row> {
        let scratch = |filetype: &'static str| TileKind::Scratch {
            filetype: filetype.to_owned(),
        };
        let sidebar = |filetype: &'static str| TileKind::Sidebar {
            filetype: filetype.to_owned(),
        };
        let prompt = |filetype: &'static str| TileKind::Prompt {
            filetype: filetype.to_owned(),
        };
        assert_eq!(name, "page.txt", "the titles below are written for it");
        let mut rows = Vec::new();
        for loclist in [false, true] {
            rows.extend([
                row("", "", loclist, TileKind::File, "page.txt [+]", FILE),
                row("", "lua", loclist, TileKind::File, "page.txt [+]", FILE),
                row(
                    "",
                    "NvimTree",
                    loclist,
                    TileKind::File,
                    "page.txt [+]",
                    FILE,
                ),
            ]);
            for filetype in ["", "NvimTree", "lua"] {
                rows.push(row(
                    "help",
                    filetype,
                    loclist,
                    TileKind::Help,
                    "help: page",
                    POSITION,
                ));
                rows.push(if loclist {
                    row(
                        "quickfix",
                        filetype,
                        true,
                        TileKind::LocationList,
                        "location list",
                        COUNT,
                    )
                } else {
                    row(
                        "quickfix",
                        filetype,
                        false,
                        TileKind::Quickfix,
                        "quickfix",
                        COUNT,
                    )
                });
                rows.push(row(
                    "terminal",
                    filetype,
                    loclist,
                    TileKind::Terminal,
                    "terminal: page.txt",
                    MODE,
                ));
            }
            rows.extend([
                row("prompt", "", loclist, prompt(""), "prompt", MODE),
                row("prompt", "lua", loclist, prompt("lua"), "lua", MODE),
                row(
                    "prompt",
                    "NvimTree",
                    loclist,
                    prompt("NvimTree"),
                    "NvimTree",
                    MODE,
                ),
            ]);
            if fixed_width {
                rows.extend([
                    row("nofile", "", loclist, sidebar(""), "page.txt", BRANCH),
                    row("nofile", "lua", loclist, sidebar("lua"), "lua", BRANCH),
                    row(
                        "nofile",
                        "NvimTree",
                        loclist,
                        sidebar("NvimTree"),
                        "NvimTree",
                        BRANCH,
                    ),
                ]);
            }
            let scratches: &[&'static str] = if fixed_width {
                &["nowrite", "acwrite", "someday"]
            } else {
                &["nofile", "nowrite", "acwrite", "someday"]
            };
            for &buftype in scratches {
                rows.extend([
                    row(buftype, "", loclist, scratch(""), "page.txt", DIAGNOSTICS),
                    row(buftype, "lua", loclist, scratch("lua"), "lua", DIAGNOSTICS),
                    row(
                        buftype,
                        "NvimTree",
                        loclist,
                        scratch("NvimTree"),
                        "NvimTree",
                        DIAGNOSTICS,
                    ),
                ]);
            }
        }
        rows
    }

    fn text(spans: &[Span]) -> String {
        spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn every_tile_kind_takes_its_title_and_segments_from_the_table() {
        let mut status = WindowStatus {
            name: "page.txt".to_owned(),
            modified: true,
            ..WindowStatus::default()
        };
        for row in table(&status.name.clone()) {
            let kind =
                TileKind::classify_window(row.buftype, row.filetype, row.loclist, row.fixed_width);
            let at = format!(
                "buftype {:?}, filetype {:?}, loclist {}, winfixwidth {}",
                row.buftype, row.filetype, row.loclist, row.fixed_width
            );
            assert_eq!(kind, row.kind, "{at}");
            let title = kind.title(&status);
            assert_eq!(text(&title), row.title, "{at}");
            let file = kind == TileKind::File;
            assert!(
                title
                    .iter()
                    .all(|span| file || span.role == StyleRole::Title),
                "{at}: a tile no file backs is titled in the title colour"
            );
            assert_eq!(kind.segments(), row.segments, "{at}");
        }
        let native = [
            (NativeSurface::Tree, "tree", BRANCH),
            (NativeSurface::Agent, "agent", Segments::NONE),
            (NativeSurface::Palette, "palette", Segments::NONE),
            (
                NativeSurface::Notifications,
                "notifications",
                Segments::NONE,
            ),
        ];
        assert_eq!(native.len(), NativeSurface::ALL.len());
        for (surface, title, segments) in native {
            let kind = TileKind::Native(surface);
            assert_eq!(text(&kind.title(&status)), title, "{surface:?}");
            assert_eq!(kind.segments(), segments, "{surface:?}");
        }
        // the fallbacks a name or a filetype stands in front of
        status.name.clear();
        for (kind, title) in [
            (TileKind::File, ""),
            (TileKind::Help, "help"),
            (TileKind::Terminal, "terminal"),
            (
                TileKind::Scratch {
                    filetype: String::new(),
                },
                "scratch",
            ),
            (
                TileKind::Sidebar {
                    filetype: String::new(),
                },
                "scratch",
            ),
        ] {
            assert_eq!(text(&kind.title(&status)), title, "{kind:?} unnamed");
        }
    }

    /// `[ui] tile_titles` names every tile titled by its filetype, ahead of
    /// the filetype, and a kind titled by anything else keeps its own.
    #[test]
    fn a_tile_titled_by_its_filetype_takes_the_title_the_user_mapped_it_to() {
        let status = WindowStatus {
            name: "page.txt".to_owned(),
            ..WindowStatus::default()
        };
        let titles: TileTitles = [("NvimTree", "files"), ("blank", "")]
            .into_iter()
            .map(|(filetype, title)| (filetype.to_owned(), title.to_owned()))
            .collect();
        let of = |filetype: &str| filetype.to_owned();
        for (kind, title) in [
            (
                TileKind::Sidebar {
                    filetype: of("NvimTree"),
                },
                "files",
            ),
            (
                TileKind::Scratch {
                    filetype: of("NvimTree"),
                },
                "files",
            ),
            (
                TileKind::Prompt {
                    filetype: of("NvimTree"),
                },
                "files",
            ),
            (
                TileKind::Sidebar {
                    filetype: of("lua"),
                },
                "lua",
            ),
            (TileKind::Scratch { filetype: of("") }, "page.txt"),
            (TileKind::Prompt { filetype: of("") }, "prompt"),
            // an entry mapped to nothing names nothing, and the filetype
            // stands in
            (
                TileKind::Sidebar {
                    filetype: of("blank"),
                },
                "blank",
            ),
            (TileKind::File, "page.txt"),
            (TileKind::Help, "help: page"),
            (TileKind::Native(NativeSurface::Tree), "tree"),
        ] {
            assert_eq!(text(&kind.title_with(&status, &titles)), title, "{kind:?}");
        }
    }
}
