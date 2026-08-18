//! Context providers: RPC reads assembled into ACP content blocks.
//!
//! [`assemble`] is composed the moment a user submits a prompt, from
//! [`EngineReadSnapshot`], never from a painted `Surface` -- every field it
//! reads traces back to an RPC call, by construction rather than by
//! convention (spec L654-656). Its output is
//! [`view_core::native::ai_event::ContextBlock`], the same type
//! `AiCommand::Prompt` carries and `acp::driver` already serializes onto
//! the wire -- there is no separate context-block vocabulary for this
//! module to invent.

use view_core::native::ai_context::EngineReadSnapshot;
use view_core::native::ai_event::ContextBlock;

/// Assembles context purely from already-completed RPC reads. A field of
/// `reads` left absent or empty (a read that errored, one that had nothing
/// to report -- no selection, an empty diagnostics list, or a present read
/// whose text came back empty) produces no block for it: omitted, never an
/// empty one, mirroring the toast system's own "empty means hide the
/// segment" convention already established for the statusline.
#[must_use]
pub fn assemble(reads: &EngineReadSnapshot) -> Vec<ContextBlock> {
    let mut blocks = Vec::new();

    if let Some(buffer) = &reads.current_buffer {
        if !buffer.text.is_empty() {
            blocks.push(ContextBlock::CurrentBuffer {
                path: buffer.path.clone(),
                text: buffer.text.clone(),
            });
        }
    }
    if let Some(selection) = &reads.selection {
        if !selection.text.is_empty() {
            blocks.push(ContextBlock::Selection {
                text: selection.text.clone(),
                range: selection.range,
            });
        }
    }
    if let Some(cursor) = reads.cursor {
        blocks.push(ContextBlock::Cursor {
            line: cursor.line,
            col: cursor.col,
        });
    }
    if !reads.diagnostics.is_empty() {
        blocks.push(ContextBlock::Diagnostics {
            entries: reads.diagnostics.clone(),
        });
    }
    if !reads.quickfix.is_empty() {
        blocks.push(ContextBlock::QuickfixList {
            entries: reads.quickfix.clone(),
        });
    }

    blocks
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;

    use view_core::native::ai_context::{
        CurrentBufferRead, CursorRead, DiagnosticEntry, DiagnosticSeverity, QuickfixEntry,
        SelectionRead,
    };
    use view_core::native::ai_event::AiCommand;

    use super::*;

    #[test]
    fn selection_present_produces_a_selection_block() {
        let reads = EngineReadSnapshot::default()
            .with_selection(SelectionRead::new("let x = 1;".to_string(), (4, 4)));

        let blocks = assemble(&reads);

        assert_eq!(
            blocks,
            vec![ContextBlock::Selection {
                text: "let x = 1;".to_string(),
                range: (4, 4),
            }]
        );
    }

    #[test]
    fn selection_absent_produces_no_selection_block() {
        let reads = EngineReadSnapshot::default();

        let blocks = assemble(&reads);

        assert!(!blocks
            .iter()
            .any(|b| matches!(b, ContextBlock::Selection { .. })));
    }

    /// A present-but-empty selection read (nvim reporting `getpos("'<")`
    /// against a buffer that has never entered visual mode, e.g.) omits the
    /// block the same way an absent one does -- an empty-text block would
    /// attach nothing useful to every prompt, defeating the whole
    /// "omitted, not empty" contract for a case that never even needs an
    /// `Option::None` to represent it.
    #[test]
    fn selection_present_but_empty_produces_no_selection_block() {
        let reads =
            EngineReadSnapshot::default().with_selection(SelectionRead::new(String::new(), (0, 0)));

        let blocks = assemble(&reads);

        assert!(!blocks
            .iter()
            .any(|b| matches!(b, ContextBlock::Selection { .. })));
    }

    #[test]
    fn current_buffer_present_produces_a_current_buffer_block() {
        let reads = EngineReadSnapshot::default().with_current_buffer(CurrentBufferRead::new(
            PathBuf::from("/tmp/a.rs"),
            "fn main() {}".to_string(),
        ));

        let blocks = assemble(&reads);

        assert_eq!(
            blocks,
            vec![ContextBlock::CurrentBuffer {
                path: PathBuf::from("/tmp/a.rs"),
                text: "fn main() {}".to_string(),
            }]
        );
    }

    /// Mirrors `selection_present_but_empty_produces_no_selection_block`:
    /// an empty scratch buffer (nvim's `[No Name]`, or a genuinely
    /// zero-byte file) produces no `CurrentBuffer` block.
    #[test]
    fn current_buffer_present_but_empty_produces_no_current_buffer_block() {
        let reads = EngineReadSnapshot::default().with_current_buffer(CurrentBufferRead::new(
            PathBuf::from("/tmp/a.rs"),
            String::new(),
        ));

        let blocks = assemble(&reads);

        assert!(!blocks
            .iter()
            .any(|b| matches!(b, ContextBlock::CurrentBuffer { .. })));
    }

    #[test]
    fn cursor_present_produces_a_cursor_block() {
        let reads = EngineReadSnapshot::default().with_cursor(CursorRead::new(12, 4));

        let blocks = assemble(&reads);

        assert_eq!(blocks, vec![ContextBlock::Cursor { line: 12, col: 4 }]);
    }

    #[test]
    fn empty_diagnostics_produce_no_diagnostics_block() {
        let reads = EngineReadSnapshot::default();

        let blocks = assemble(&reads);

        assert!(!blocks
            .iter()
            .any(|b| matches!(b, ContextBlock::Diagnostics { .. })));
    }

    #[test]
    fn nonempty_diagnostics_produce_a_diagnostics_block() {
        let entry = DiagnosticEntry::new(
            3,
            1,
            DiagnosticSeverity::Error,
            "unresolved import".to_string(),
        );
        let reads = EngineReadSnapshot::default().with_diagnostics(vec![entry.clone()]);

        let blocks = assemble(&reads);

        assert_eq!(
            blocks,
            vec![ContextBlock::Diagnostics {
                entries: vec![entry]
            }]
        );
    }

    #[test]
    fn empty_quickfix_produces_no_quickfix_block() {
        let reads = EngineReadSnapshot::default();

        let blocks = assemble(&reads);

        assert!(!blocks
            .iter()
            .any(|b| matches!(b, ContextBlock::QuickfixList { .. })));
    }

    #[test]
    fn nonempty_quickfix_produces_a_quickfix_block() {
        let entry = QuickfixEntry::new(PathBuf::from("/tmp/a.rs"), 7, 2, "TODO".to_string());
        let reads = EngineReadSnapshot::default().with_quickfix(vec![entry.clone()]);

        let blocks = assemble(&reads);

        assert_eq!(
            blocks,
            vec![ContextBlock::QuickfixList {
                entries: vec![entry]
            }]
        );
    }

    #[test]
    fn every_read_absent_produces_no_blocks_and_never_panics() {
        let blocks = assemble(&EngineReadSnapshot::default());

        assert!(blocks.is_empty());
    }

    /// A populated snapshot produces its blocks in a stable order (buffer,
    /// selection, cursor, diagnostics, quickfix) so a caller assembling a
    /// prompt's context does not see it reshuffle between calls with the
    /// same reads.
    #[test]
    fn every_block_kind_together_preserves_field_order() {
        let reads = EngineReadSnapshot::default()
            .with_current_buffer(CurrentBufferRead::new(
                PathBuf::from("/tmp/a.rs"),
                "fn main() {}".to_string(),
            ))
            .with_selection(SelectionRead::new("main".to_string(), (0, 0)))
            .with_cursor(CursorRead::new(0, 3))
            .with_diagnostics(vec![DiagnosticEntry::new(
                0,
                0,
                DiagnosticSeverity::Warning,
                "unused".to_string(),
            )])
            .with_quickfix(vec![QuickfixEntry::new(
                PathBuf::from("/tmp/a.rs"),
                0,
                0,
                "note".to_string(),
            )]);

        let blocks = assemble(&reads);

        let kinds: Vec<&str> = blocks
            .iter()
            .map(|b| match b {
                ContextBlock::CurrentBuffer { .. } => "current_buffer",
                ContextBlock::Selection { .. } => "selection",
                ContextBlock::Cursor { .. } => "cursor",
                ContextBlock::Diagnostics { .. } => "diagnostics",
                ContextBlock::QuickfixList { .. } => "quickfix",
                _ => "other",
            })
            .collect();
        assert!(
            !kinds.contains(&"other"),
            "a block kind assemble emits must be named here, not lumped into the wildcard"
        );
        assert_eq!(
            kinds,
            vec![
                "current_buffer",
                "selection",
                "cursor",
                "diagnostics",
                "quickfix"
            ]
        );
    }

    /// `assemble`'s output has to be the exact type `AiCommand::Prompt.context`
    /// takes, with no conversion step in between -- a second, shadow
    /// `ContextBlock` type here would compile this call site against the
    /// wrong enum with no error until a caller tried to actually send it.
    #[test]
    fn assembled_blocks_construct_an_ai_command_prompt_directly() {
        let reads = EngineReadSnapshot::default()
            .with_selection(SelectionRead::new("x".to_string(), (1, 1)));

        let context = assemble(&reads);
        let command = AiCommand::Prompt {
            text: "explain this".to_string(),
            context,
        };

        let AiCommand::Prompt { context, .. } = command else {
            unreachable!("constructed a Prompt directly above");
        };
        assert_eq!(
            context,
            vec![ContextBlock::Selection {
                text: "x".to_string(),
                range: (1, 1),
            }]
        );
    }
}
