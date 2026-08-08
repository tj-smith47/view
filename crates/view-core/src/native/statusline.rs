//! The native statusline's segment state and its truncation policy.
//!
//! Every segment source is an event or a bridge callback -- see
//! `docs/statusline-wire-capture.md` -- so [`StatuslineState`] only ever
//! applies already-decoded text or counts; it does no RPC and reads no
//! buffer state itself. [`StatuslineState::view`] composes the current
//! segments into the three-zone [`StatuslineView`] `view-surface` already
//! knows how to lay out and paint.

use crate::native::views::StatuslineView;

/// Which zone of the bar a segment's text lands in once
/// [`StatuslineState::view`] assembles the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zone {
    Left,
    Center,
    Right,
}

/// The statusline's current segment text, one field per source in
/// [`SegmentUpdate`]. All fields start empty/absent -- an empty segment is
/// simply not rendered, matching the wire's own "empty content hides the
/// segment" convention (see `docs/statusline-wire-capture.md`).
#[non_exhaustive]
#[derive(Debug, Clone, Default)]
pub struct StatuslineState {
    /// `msg_showmode`'s content verbatim: mode text and macro-recording
    /// text arrive from nvim already fused with no separator (the spec
    /// requires macro recording stay visible), so this is never split or
    /// re-derived.
    mode: String,
    /// `msg_showcmd`'s content verbatim: pending count/operator/keys.
    showcmd: String,
    /// `msg_ruler`'s content verbatim: cursor position, only emitted while
    /// `laststatus=0` (the statusline feature's own supersession setting).
    ruler: String,
    /// The `search_count` `msg_show` kind's content verbatim.
    search_count: String,
    /// The current buffer's tail name, from the bridge's `buffer` trigger.
    file: String,
    /// The current buffer's modified flag, from the same trigger.
    modified: bool,
    /// `vim.diagnostic.count(0)` totals from the bridge's `DiagnosticChanged`
    /// callback. `None` until the first callback fires (distinct from
    /// `Some((0, 0))`, a buffer confirmed clean); both render as hidden, so
    /// the distinction only matters to a future consumer that cares whether
    /// diagnostics have been checked yet at all.
    diagnostics: Option<(u32, u32)>,
    /// The current git branch, from the bridge's git trigger group's
    /// `vim.system()` lookup. Empty means no repo or a failed lookup.
    git_branch: String,
}

/// One segment source updating [`StatuslineState`]. Seven variants though
/// the brief names six: `Buffer` is added because "file, modified flag from
/// Model's existing buffer state" does not exist anywhere in the codebase
/// (confirmed by search) -- nothing tracks buffer identity today, so the
/// statusline's file segment needs its own bridge-sourced update like every
/// other segment. Additive over the brief's six, `#[non_exhaustive]` so a
/// future segment can grow the same way without a breaking change.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentUpdate {
    Mode(String),
    Showcmd(String),
    Ruler(String),
    SearchCount(String),
    Diagnostics { errors: u32, warnings: u32 },
    GitBranch(String),
    Buffer { name: String, modified: bool },
}

impl StatuslineState {
    /// Applies one decoded segment update in place.
    pub fn apply(&mut self, update: SegmentUpdate) {
        match update {
            SegmentUpdate::Mode(text) => self.mode = text,
            SegmentUpdate::Showcmd(text) => self.showcmd = text,
            SegmentUpdate::Ruler(text) => self.ruler = text,
            SegmentUpdate::SearchCount(text) => self.search_count = text,
            SegmentUpdate::Diagnostics { errors, warnings } => {
                self.diagnostics = Some((errors, warnings));
            }
            SegmentUpdate::GitBranch(branch) => self.git_branch = branch,
            SegmentUpdate::Buffer { name, modified } => {
                self.file = name;
                self.modified = modified;
            }
        }
    }

    /// Composes the current segments into a [`StatuslineView`] that fits
    /// `width` columns.
    ///
    /// Truncation drops whole segments, least-important first, rather than
    /// clipping any one segment's characters -- clipping a segment to
    /// illegible fragments would be worse than omitting it outright. Ranked
    /// most to least important: mode/macro (never dropped -- the spec
    /// requires macro recording stay visible even at the narrowest width),
    /// ruler, search count, file/modified, pending keys, diagnostics, git
    /// branch. `view-surface::overlay`'s own character-level `clip()` is
    /// still the last-resort fallback for whatever this leaves too long
    /// (e.g. a single segment's text alone exceeding `width`).
    #[must_use]
    pub fn view(&self, width: u16) -> StatuslineView {
        let mode = self.mode.clone();

        // most to least important, matching the ranking documented above;
        // popping from the end below therefore drops least-important first
        let mut candidates: Vec<(Zone, String)> = Vec::new();
        if !self.ruler.is_empty() {
            candidates.push((Zone::Right, self.ruler.clone()));
        }
        if !self.search_count.is_empty() {
            candidates.push((Zone::Right, self.search_count.clone()));
        }
        if !self.file.is_empty() {
            let marker = if self.modified { " [+]" } else { "" };
            candidates.push((Zone::Center, format!("{}{marker}", self.file)));
        }
        if !self.showcmd.is_empty() {
            candidates.push((Zone::Left, self.showcmd.clone()));
        }
        if let Some((errors, warnings)) = self.diagnostics {
            if errors > 0 || warnings > 0 {
                candidates.push((Zone::Center, format!("E:{errors} W:{warnings}")));
            }
        }
        if !self.git_branch.is_empty() {
            candidates.push((Zone::Center, self.git_branch.clone()));
        }

        while assembled_len(&mode, &candidates) > usize::from(width) && !candidates.is_empty() {
            candidates.pop();
        }

        let mut left = mode;
        let mut center_parts = Vec::new();
        let mut right_parts = Vec::new();
        for (zone, text) in candidates {
            match zone {
                Zone::Left => {
                    if left.is_empty() {
                        left = text;
                    } else {
                        left = format!("{left} {text}");
                    }
                }
                Zone::Center => center_parts.push(text),
                Zone::Right => right_parts.push(text),
            }
        }

        StatuslineView::new(left, center_parts.join("  "), right_parts.join("  "))
    }
}

/// The row's approximate assembled width: `mode` plus every candidate's
/// text, each with a one-column separator budgeted in. An approximation
/// (real zone joins use two-column separators in `view`), deliberately
/// biased toward dropping a hair early rather than late -- `clip()`
/// downstream is a hard character truncation, and a segment surviving into
/// that is worse than one dropped a column before it was strictly required.
fn assembled_len(mode: &str, candidates: &[(Zone, String)]) -> usize {
    mode.chars().count()
        + candidates
            .iter()
            .map(|(_, text)| text.chars().count() + 1)
            .sum::<usize>()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn recording_a_macro_is_always_visible() {
        // macro recording must stay visible; `msg_showmode`'s content is
        // rendered verbatim, per docs/statusline-wire-capture.md.
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Mode("recording @q".to_string()));
        let view = state.view(80);
        assert_eq!(view.left, "recording @q");
    }

    #[test]
    fn mode_survives_even_at_the_narrowest_width() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Mode("recording @q".to_string()));
        state.apply(SegmentUpdate::Ruler("0,0-1         All".to_string()));
        state.apply(SegmentUpdate::GitBranch(
            "dev/p4-native-features".to_string(),
        ));
        let view = state.view(1);
        assert_eq!(view.left, "recording @q");
    }

    #[test]
    fn showcmd_renders_pending_keys() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Showcmd("12".to_string()));
        let view = state.view(80);
        assert_eq!(view.left, "12");
    }

    #[test]
    fn mode_and_showcmd_share_the_left_zone() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Mode("-- INSERT --".to_string()));
        state.apply(SegmentUpdate::Showcmd("12".to_string()));
        let view = state.view(80);
        assert_eq!(view.left, "-- INSERT -- 12");
    }

    #[test]
    fn ruler_renders_in_the_right_zone() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Ruler("1,26          All".to_string()));
        let view = state.view(80);
        assert_eq!(view.right, "1,26          All");
    }

    #[test]
    fn search_count_renders_alongside_ruler() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Ruler("1,5           All".to_string()));
        state.apply(SegmentUpdate::SearchCount(
            "/cat            [2/2]".to_string(),
        ));
        let view = state.view(80);
        assert_eq!(view.right, "1,5           All  /cat            [2/2]");
    }

    #[test]
    fn diagnostics_hide_when_clean() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Diagnostics {
            errors: 0,
            warnings: 0,
        });
        let view = state.view(80);
        assert_eq!(view.center, "");
    }

    #[test]
    fn diagnostics_render_counts_when_nonzero() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Diagnostics {
            errors: 2,
            warnings: 1,
        });
        let view = state.view(80);
        assert_eq!(view.center, "E:2 W:1");
    }

    #[test]
    fn git_branch_renders_in_the_center_zone() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::GitBranch("main".to_string()));
        let view = state.view(80);
        assert_eq!(view.center, "main");
    }

    #[test]
    fn buffer_renders_file_and_modified_marker() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Buffer {
            name: "statusline.rs".to_string(),
            modified: true,
        });
        let view = state.view(80);
        assert_eq!(view.center, "statusline.rs [+]");
    }

    #[test]
    fn unmodified_buffer_renders_without_a_marker() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Buffer {
            name: "statusline.rs".to_string(),
            modified: false,
        });
        let view = state.view(80);
        assert_eq!(view.center, "statusline.rs");
    }

    /// A fully populated bar at 200 columns: every segment present, nothing
    /// dropped.
    #[test]
    fn wide_width_keeps_every_segment() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Mode("-- INSERT --".to_string()));
        state.apply(SegmentUpdate::Showcmd("12".to_string()));
        state.apply(SegmentUpdate::Ruler("1,26          All".to_string()));
        state.apply(SegmentUpdate::SearchCount("/cat [2/2]".to_string()));
        state.apply(SegmentUpdate::Buffer {
            name: "statusline.rs".to_string(),
            modified: true,
        });
        state.apply(SegmentUpdate::Diagnostics {
            errors: 1,
            warnings: 2,
        });
        state.apply(SegmentUpdate::GitBranch(
            "dev/p4-native-features".to_string(),
        ));

        let view = state.view(200);
        assert_eq!(view.left, "-- INSERT -- 12");
        assert_eq!(view.right, "1,26          All  /cat [2/2]");
        assert_eq!(
            view.center,
            "statusline.rs [+]  E:1 W:2  dev/p4-native-features"
        );
    }

    /// The same fully populated bar at a width that fits everything except
    /// the two least-important segments: git branch and diagnostics drop
    /// whole, rather than any segment's text being chopped mid-word, and
    /// every more-important segment survives intact.
    #[test]
    fn narrow_width_drops_least_important_segments_first() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Mode("-- INSERT --".to_string()));
        state.apply(SegmentUpdate::Showcmd("12".to_string()));
        state.apply(SegmentUpdate::Ruler("1,26          All".to_string()));
        state.apply(SegmentUpdate::SearchCount("/cat [2/2]".to_string()));
        state.apply(SegmentUpdate::Buffer {
            name: "statusline.rs".to_string(),
            modified: true,
        });
        state.apply(SegmentUpdate::Diagnostics {
            errors: 1,
            warnings: 2,
        });
        state.apply(SegmentUpdate::GitBranch(
            "dev/p4-native-features".to_string(),
        ));

        let view = state.view(64);
        assert_eq!(view.left, "-- INSERT -- 12");
        assert_eq!(view.right, "1,26          All  /cat [2/2]");
        assert_eq!(view.center, "statusline.rs [+]");
    }

    /// At the narrowest realistic width, every segment but mode drops --
    /// dropped whole, never chopped mid-word.
    #[test]
    fn extremely_narrow_width_drops_every_segment_but_mode() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Mode("-- INSERT --".to_string()));
        state.apply(SegmentUpdate::Showcmd("12".to_string()));
        state.apply(SegmentUpdate::Ruler("1,26          All".to_string()));
        state.apply(SegmentUpdate::SearchCount("/cat [2/2]".to_string()));
        state.apply(SegmentUpdate::Buffer {
            name: "statusline.rs".to_string(),
            modified: true,
        });
        state.apply(SegmentUpdate::Diagnostics {
            errors: 1,
            warnings: 2,
        });
        state.apply(SegmentUpdate::GitBranch(
            "dev/p4-native-features".to_string(),
        ));

        let view = state.view(12);
        assert_eq!(view.left, "-- INSERT --");
        assert_eq!(view.center, "");
        assert_eq!(view.right, "");
    }

    #[test]
    fn empty_state_renders_an_empty_bar() {
        let state = StatuslineState::default();
        let view = state.view(80);
        assert_eq!(view, StatuslineView::new("", "", ""));
    }
}
