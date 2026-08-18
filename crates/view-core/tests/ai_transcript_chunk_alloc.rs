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
//! Single-test file by design: [`ALLOCATOR`]'s counter is process-global,
//! and the default test harness runs a binary's tests on multiple threads
//! at once, so a second test here would race this one for the same
//! counter. Add a second test only alongside a way to keep them from
//! running concurrently (a shared `Mutex` guarding each test's
//! measurement window, or `--test-threads=1` pinned in this binary's own
//! config), not as a bare second `#[test]` function.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use view_core::native::ai_panel::Transcript;
use view_test_support::CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

/// 500 chunks sharing one `message_id` fold into a single transcript entry
/// via far fewer than 500 allocations: `String::push_str`'s amortized
/// growth means the total stays close to `log2(total bytes)` reallocations
/// rather than one allocation per chunk. A fresh `Vec`/`String` per chunk
/// -- the regression this budget exists to catch -- would cost exactly one
/// allocation per chunk instead, which a chatty agent streaming many chunks
/// per second would turn into a per-`update()`-call cost that scales with
/// stream chattiness rather than staying flat.
#[test]
fn five_hundred_same_id_chunks_fold_via_far_fewer_than_five_hundred_allocations() {
    let mut transcript = Transcript::new();

    let before = ALLOCATOR.count();
    for _ in 0..500 {
        transcript.append_or_extend(Some("m1"), "x", true);
    }
    let allocations = ALLOCATOR.count() - before;

    assert_eq!(transcript.len(), 1);
    let entry = transcript.iter().next().expect("one folded entry");
    assert_eq!(entry.text, "x".repeat(500));
    assert!(
        allocations <= 30,
        "500 chunks folded via {allocations} allocations -- expected a small \
         constant (one-time setup: the entry, the message-id index, the \
         render-cache slot) rather than anything that scales with chunk count"
    );
}
