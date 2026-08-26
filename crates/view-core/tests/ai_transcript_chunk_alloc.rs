//! Bench-style allocation budget for the transcript's per-chunk fold path:
//! hot paths stay allocation-free after warmup.
//!
//! The `#[global_allocator]` lives in this binary, not in
//! `view-test-support` (which only supplies [`CountingAllocator`]'s
//! counting logic): a process has exactly one global allocator, so scoping
//! the `static` to this one integration-test binary keeps it from
//! overriding allocation for any other crate's tests, or for
//! `view-core`'s own `#![deny(unsafe_code)]` library build.
//!
//! [`ALLOCATOR`] counts per thread and libtest gives each test one of its
//! own, so a second `#[test]` added here measures only itself -- what it
//! must not do is spawn a thread and read the count from the wrong side of
//! it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use view_core::native::ai_panel::{Transcript, TranscriptRole};
use view_test_support::CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

/// 500 chunks sharing one `message_id` fold into a single transcript entry
/// via a fixed 13 allocations: `String::push_str`'s amortized growth means
/// the total stays close to `log2(total bytes)` reallocations rather than
/// one allocation per chunk. The bound sits just above the measured count
/// rather than at a round number -- [`CountingAllocator`] counts only this
/// thread, so there is no harness noise for a slack to absorb, and slack
/// past what the code costs is only room for a regression to hide in. A
/// fresh `Vec`/`String` per chunk
/// -- the regression this budget exists to catch -- would cost exactly one
/// allocation per chunk instead, which a chatty agent streaming many chunks
/// per second would turn into a per-`update()`-call cost that scales with
/// stream chattiness rather than staying flat.
#[test]
fn five_hundred_same_id_chunks_fold_via_far_fewer_than_five_hundred_allocations() {
    let mut transcript = Transcript::new();

    let before = ALLOCATOR.count();
    for _ in 0..500 {
        transcript.append_or_extend(Some("m1"), "x", TranscriptRole::Agent);
    }
    let allocations = ALLOCATOR.count() - before;

    assert_eq!(transcript.len(), 1);
    let entry = transcript.iter().next().expect("one folded entry");
    assert_eq!(entry.text, "x".repeat(500));
    assert!(
        allocations <= 16,
        "500 chunks folded via {allocations} allocations -- the measured \
         constant is 13 (the entry, its message-id index slot, its \
         render-cache slot, and `String::push_str`'s doublings across 500 \
         bytes), and the 3 above it is room for a growth strategy that \
         doubles from a different floor. The regression this bounds costs \
         one allocation per chunk, so it lands at 500-odd and cannot fit \
         under any margin this side of the chunk count."
    );
}
