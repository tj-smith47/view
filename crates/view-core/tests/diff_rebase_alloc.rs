//! Bench-style allocation budget for the diff review's rebase path: an
//! edit outside every open hunk costs zero allocations.
//!
//! `rebase` runs once per `Msg::BufTextChanged`, which is once per
//! keystroke in an attached buffer while a review is open, so it is on the
//! key-dispatch path. The common case there is an edit nowhere near a
//! hunk, and the whole of the work it may do is shift `u32` row offsets in
//! place -- a `Vec` per hunk per keystroke is the regression this pins.
//!
//! The `#[global_allocator]` lives in this binary rather than in
//! `view-test-support` for the reason `ai_transcript_chunk_alloc.rs` gives:
//! a process has exactly one, so scoping the `static` here keeps it from
//! overriding allocation for any other crate's tests or for `view-core`'s
//! own `#![deny(unsafe_code)]` library build.
//!
//! Single-test file by design, on the same terms as that file: the counter
//! is process-global and the default harness runs a binary's tests on
//! several threads at once, so a second `#[test]` here would race this one
//! for the same counter.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use view_core::msg::BufferHandle;
use view_core::native::diff::{hunk, rebase, BufTextChangedEvent};
use view_test_support::CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

/// Twenty open hunks and a thousand edits above all of them: the offsets
/// move every time and nothing is allocated. A rebase that rebuilt a
/// hunk's `new_lines`, cloned its anchor, or collected an iterator per
/// call would cost allocations proportional to keystrokes times hunks,
/// which is the shape that turns a review of a large change into a typing
/// stutter rather than a fixed cost.
#[test]
fn an_edit_outside_every_hunk_rebases_without_allocating() {
    let (old, new) = fixture(20, 10);
    let mut hunks = hunk::diff(Some(&old), &new);
    assert_eq!(hunks.len(), 20);
    let change = BufTextChangedEvent {
        buf: BufferHandle(1),
        generation: 1,
        firstline: 0,
        lastline: 0,
        linedata: vec!["inserted above every hunk".to_string()],
        changedtick: 1,
        desynced: false,
    };

    // One warmup call first: the budget is on the steady state, not on
    // whatever one-time work the first call through a cold path does.
    rebase(&mut hunks, &change);
    let before = ALLOCATOR.count();
    for _ in 0..1000 {
        rebase(&mut hunks, &change);
    }
    let allocations = ALLOCATOR.count() - before;

    assert_eq!(
        allocations, 0,
        "1000 rebases over 20 hunks allocated {allocations} times -- an \
         edit outside every anchor may only shift u32 offsets in place"
    );
    assert_eq!(
        hunks[0].old_range.0, 1001,
        "the shift itself must still have happened -- one row per call, \
         warmup included; a rebase that did nothing would also allocate \
         nothing"
    );
}

/// `hunks` changed rows, each separated by `spacing - 1` unchanged ones so
/// the diff resolves them as separate hunks with room for their own
/// context.
fn fixture(hunks: usize, spacing: usize) -> (String, String) {
    let mut old = String::new();
    let mut new = String::new();
    for i in 0..hunks {
        for j in 0..spacing {
            old.push_str(&format!("line {i}-{j}\n"));
            if j == 0 {
                new.push_str(&format!("changed {i}\n"));
            } else {
                new.push_str(&format!("line {i}-{j}\n"));
            }
        }
    }
    (old, new)
}
