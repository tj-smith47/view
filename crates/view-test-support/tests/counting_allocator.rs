//! The counter's own contract: an allocation budget measures the thread
//! that asked, and nothing else running in the process.
//!
//! A process has one global allocator, so every thread's allocations pass
//! through this one counter. libtest itself is one of those threads: it
//! runs each test on a thread of its own and keeps doing its bookkeeping --
//! a join-handle map insert, a timeout-queue push, an event channel send --
//! on the main thread while that test body runs. A counter shared across
//! threads therefore reports the harness's allocations as the test's own,
//! which on a two-core CI runner is a budget test that fails for reasons no
//! one can find in the code it measures.
//!
//! The `#[global_allocator]` lives in this binary rather than in the
//! library, for the reason the library's own doc gives: setting one applies
//! to the whole process, and no crate pulling this one in for `ScratchDir`
//! asked for its allocator to be replaced.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use view_test_support::CountingAllocator;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

/// A neighbouring thread allocating hard throughout the measurement window
/// contributes exactly nothing to it, while its own thread still sees every
/// one of those allocations -- the second half being what keeps the first
/// from passing on a counter that simply counts nothing.
#[test]
fn a_neighbouring_threads_allocations_stay_out_of_this_threads_window() {
    let stop = Arc::new(AtomicBool::new(false));
    let made = Arc::new(AtomicUsize::new(0));
    let helper_stop = Arc::clone(&stop);
    let helper_made = Arc::clone(&made);
    let helper = std::thread::spawn(move || {
        ALLOCATOR.reset();
        while !helper_stop.load(Ordering::Relaxed) {
            drop(std::hint::black_box(Vec::<u8>::with_capacity(64)));
            helper_made.fetch_add(1, Ordering::Relaxed);
        }
        ALLOCATOR.count()
    });

    // the window opens only once the helper is demonstrably running, and
    // closes only once it has allocated thousands of times inside it, so
    // what the assertion below reads is a real overlap rather than a race
    // that happened to resolve the quiet way
    while made.load(Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    ALLOCATOR.reset();
    let opened_at = made.load(Ordering::Relaxed);
    while made.load(Ordering::Relaxed) < opened_at + 4000 {
        std::thread::yield_now();
    }
    let intruders = ALLOCATOR.count();

    stop.store(true, Ordering::Relaxed);
    let helper_saw = helper.join().unwrap();

    assert_eq!(
        intruders, 0,
        "another thread allocated at least 4000 times across a window in \
         which this thread allocated nothing, and {intruders} of them were \
         counted here"
    );
    assert!(
        helper_saw > 0,
        "the allocating thread counted {helper_saw} of its own allocations, \
         so the zero above is a counter that counts nothing rather than one \
         that counts the right thread"
    );
}
