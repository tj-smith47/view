//! The native statusline's segment state and its truncation policy.
//!
//! Every segment source is an event or a bridge callback -- see
//! `docs/statusline-wire-capture.md` -- so [`StatuslineState`] only ever
//! applies already-decoded text or counts; it does no RPC and reads no
//! buffer state itself. [`StatuslineState::view`] composes the current
//! segments into the three-zone [`StatuslineView`] `view-surface` already
//! knows how to lay out and paint.

use crate::native::views::{Span, StatuslineView, StyleRole};

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
/// the statusline feature row (spec's native-features table) names six
/// segments: mode, showcmd, file, diagnostics, git branch, ruler/position.
/// `Buffer` is added because "file, modified flag from Model's existing
/// buffer state" does not exist anywhere in the codebase (confirmed by
/// search) -- nothing tracks buffer identity today, so the statusline's
/// file segment needs its own bridge-sourced update like every other
/// segment. Additive over the row's six, `#[non_exhaustive]` so a future
/// segment can grow the same way without a breaking change.
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

    /// Clears the segments the engine both raises and retracts, for a
    /// connection being replaced.
    ///
    /// [`Self::mode`], [`Self::showcmd`], [`Self::ruler`] and
    /// [`Self::search_count`] arrive as `msg_showmode`/`msg_showcmd`/
    /// `msg_ruler`/`search_count` and are taken back by the same events
    /// carrying empty content -- which an engine that was killed mid-`d2` or
    /// mid-macro never sends, so the pending operator stays painted on the
    /// replacement's own bar. The replacement re-raises whatever is true of
    /// it, so clearing costs a frame of nothing.
    ///
    /// The other four ([`Self::file`], [`Self::modified`],
    /// [`Self::diagnostics`], [`Self::git_branch`]) are deliberately kept:
    /// they come from view's own bridge rather than from a redraw event,
    /// they still describe the buffers a restart recovers, and the
    /// replacement's bridge install re-fires them.
    pub fn forget_engine_segments(&mut self) {
        self.mode.clear();
        self.showcmd.clear();
        self.ruler.clear();
        self.search_count.clear();
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
    ///
    /// Each segment carries the [`StyleRole`] a painter resolves through
    /// the active colorscheme -- see `StyleRole::chrome_group` -- rather
    /// than one flat style for the whole bar, so an error count and a
    /// warning count read as visibly distinct at a glance instead of both
    /// landing in plain text.
    #[must_use]
    pub fn view(&self, width: u16) -> StatuslineView {
        let mode = self.mode.clone();

        // most to least important, matching the ranking documented above;
        // popping from the end below therefore drops least-important first
        let mut candidates: Vec<(Zone, Vec<Span>)> = Vec::new();
        if !self.ruler.is_empty() {
            candidates.push((
                Zone::Right,
                vec![Span::new(self.ruler.clone(), StyleRole::Ruler)],
            ));
        }
        if !self.search_count.is_empty() {
            candidates.push((
                Zone::Right,
                vec![Span::new(self.search_count.clone(), StyleRole::Ruler)],
            ));
        }
        if !self.file.is_empty() {
            let mut spans = vec![Span::new(self.file.clone(), StyleRole::File)];
            if self.modified {
                spans.push(Span::new(" [+]", StyleRole::Modified));
            }
            candidates.push((Zone::Center, spans));
        }
        if !self.showcmd.is_empty() {
            candidates.push((Zone::Left, vec![Span::plain(self.showcmd.clone())]));
        }
        if let Some((errors, warnings)) = self.diagnostics {
            if errors > 0 || warnings > 0 {
                let mut spans = Vec::new();
                if errors > 0 {
                    spans.push(Span::new(
                        format!("\u{25cf} {errors}"),
                        StyleRole::DiagnosticError,
                    ));
                }
                if warnings > 0 {
                    if !spans.is_empty() {
                        spans.push(Span::plain("  "));
                    }
                    spans.push(Span::new(
                        format!("\u{25b2} {warnings}"),
                        StyleRole::DiagnosticWarning,
                    ));
                }
                candidates.push((Zone::Center, spans));
            }
        }
        if !self.git_branch.is_empty() {
            candidates.push((
                Zone::Center,
                vec![Span::new(self.git_branch.clone(), StyleRole::GitBranch)],
            ));
        }

        while assembled_len(&mode, &candidates) > usize::from(width) && !candidates.is_empty() {
            candidates.pop();
        }

        let mut left = if mode.is_empty() {
            Vec::new()
        } else {
            vec![Span::new(mode, StyleRole::Mode)]
        };
        let mut center: Vec<Span> = Vec::new();
        let mut right: Vec<Span> = Vec::new();
        for (zone, spans) in candidates {
            match zone {
                Zone::Left => {
                    if !left.is_empty() {
                        left.push(Span::plain(" "));
                    }
                    left.extend(spans);
                }
                Zone::Center => {
                    if !center.is_empty() {
                        center.push(Span::plain("  "));
                    }
                    center.extend(spans);
                }
                Zone::Right => {
                    if !right.is_empty() {
                        right.push(Span::plain("  "));
                    }
                    right.extend(spans);
                }
            }
        }

        StatuslineView::from_spans(left, center, right)
    }
}

/// The row's approximate assembled width: `mode` plus every candidate's
/// spans, each group with a one-column separator budgeted in. An
/// approximation (real zone joins use two-column separators in `view`),
/// deliberately biased toward dropping a hair early rather than late --
/// `clip()` downstream is a hard character truncation, and a segment
/// surviving into that is worse than one dropped a column before it was
/// strictly required.
fn assembled_len(mode: &str, candidates: &[(Zone, Vec<Span>)]) -> usize {
    mode.chars().count()
        + candidates
            .iter()
            .map(|(_, spans)| spans.iter().map(|s| s.text.chars().count()).sum::<usize>() + 1)
            .sum::<usize>()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A zone's rendered text, ignoring which spans it is built from: most
    /// of this module's assertions are about which segments survive and in
    /// what order, not about role assignment, which the dedicated `_roles`
    /// tests below cover instead.
    fn text(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn recording_a_macro_is_always_visible() {
        // macro recording must stay visible; `msg_showmode`'s content is
        // rendered verbatim, per docs/statusline-wire-capture.md.
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Mode("recording @q".to_string()));
        let view = state.view(80);
        assert_eq!(text(&view.left), "recording @q");
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
        assert_eq!(text(&view.left), "recording @q");
    }

    #[test]
    fn showcmd_renders_pending_keys() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Showcmd("12".to_string()));
        let view = state.view(80);
        assert_eq!(text(&view.left), "12");
    }

    #[test]
    fn mode_and_showcmd_share_the_left_zone() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Mode("-- INSERT --".to_string()));
        state.apply(SegmentUpdate::Showcmd("12".to_string()));
        let view = state.view(80);
        assert_eq!(text(&view.left), "-- INSERT -- 12");
    }

    #[test]
    fn ruler_renders_in_the_right_zone() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Ruler("1,26          All".to_string()));
        let view = state.view(80);
        assert_eq!(text(&view.right), "1,26          All");
    }

    #[test]
    fn search_count_renders_alongside_ruler() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Ruler("1,5           All".to_string()));
        state.apply(SegmentUpdate::SearchCount(
            "/cat            [2/2]".to_string(),
        ));
        let view = state.view(80);
        assert_eq!(
            text(&view.right),
            "1,5           All  /cat            [2/2]"
        );
    }

    #[test]
    fn diagnostics_hide_when_clean() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Diagnostics {
            errors: 0,
            warnings: 0,
        });
        let view = state.view(80);
        assert_eq!(text(&view.center), "");
    }

    #[test]
    fn diagnostics_render_counts_when_nonzero() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Diagnostics {
            errors: 2,
            warnings: 1,
        });
        let view = state.view(80);
        assert_eq!(text(&view.center), "\u{25cf} 2  \u{25b2} 1");
    }

    /// The mock-up in the task brief shows the error glyph and the warning
    /// glyph carrying visibly distinct styling; this is the assertion that
    /// they actually do, not just that the text reads right.
    #[test]
    fn diagnostic_glyphs_carry_distinct_error_and_warning_roles() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Diagnostics {
            errors: 2,
            warnings: 1,
        });
        let view = state.view(80);
        let error = view
            .center
            .iter()
            .find(|s| s.text.contains('2'))
            .expect("the error count span is present");
        let warning = view
            .center
            .iter()
            .find(|s| s.text.contains('1'))
            .expect("the warning count span is present");
        assert_eq!(error.role, StyleRole::DiagnosticError);
        assert_eq!(warning.role, StyleRole::DiagnosticWarning);
        assert_ne!(error.role, warning.role);
    }

    /// Mode, file, the modified marker, and git branch each resolve to
    /// their own role rather than one flat style for the whole bar.
    #[test]
    fn mode_file_modified_and_branch_carry_their_own_roles() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Mode("-- INSERT --".to_string()));
        state.apply(SegmentUpdate::Buffer {
            name: "statusline.rs".to_string(),
            modified: true,
        });
        state.apply(SegmentUpdate::GitBranch("main".to_string()));
        let view = state.view(80);

        assert_eq!(view.left, vec![Span::new("-- INSERT --", StyleRole::Mode)]);
        assert_eq!(
            view.center,
            vec![
                Span::new("statusline.rs", StyleRole::File),
                Span::new(" [+]", StyleRole::Modified),
                Span::plain("  "),
                Span::new("main", StyleRole::GitBranch),
            ]
        );
    }

    #[test]
    fn git_branch_renders_in_the_center_zone() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::GitBranch("main".to_string()));
        let view = state.view(80);
        assert_eq!(text(&view.center), "main");
    }

    #[test]
    fn buffer_renders_file_and_modified_marker() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Buffer {
            name: "statusline.rs".to_string(),
            modified: true,
        });
        let view = state.view(80);
        assert_eq!(text(&view.center), "statusline.rs [+]");
    }

    #[test]
    fn unmodified_buffer_renders_without_a_marker() {
        let mut state = StatuslineState::default();
        state.apply(SegmentUpdate::Buffer {
            name: "statusline.rs".to_string(),
            modified: false,
        });
        let view = state.view(80);
        assert_eq!(text(&view.center), "statusline.rs");
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
        assert_eq!(text(&view.left), "-- INSERT -- 12");
        assert_eq!(text(&view.right), "1,26          All  /cat [2/2]");
        assert_eq!(
            text(&view.center),
            "statusline.rs [+]  \u{25cf} 1  \u{25b2} 2  dev/p4-native-features"
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
        assert_eq!(text(&view.left), "-- INSERT -- 12");
        assert_eq!(text(&view.right), "1,26          All  /cat [2/2]");
        assert_eq!(text(&view.center), "statusline.rs [+]");
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
        assert_eq!(text(&view.left), "-- INSERT --");
        assert_eq!(text(&view.center), "");
        assert_eq!(text(&view.right), "");
    }

    #[test]
    fn empty_state_renders_an_empty_bar() {
        let state = StatuslineState::default();
        let view = state.view(80);
        assert_eq!(view, StatuslineView::new("", "", ""));
    }
}
