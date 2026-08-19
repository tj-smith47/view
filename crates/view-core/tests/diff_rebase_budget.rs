//! Bench-style wall-time budget for the diff review's rebase path.
//!
//! `rebase` is an addend inside the same per-keystroke `update()` dispatch
//! the spec's input-path row bounds end to end (its performance-budget
//! table: key event read from the terminal -> RPC bytes written, with a
//! dev-linux p99 <= 100 µs bar on it). It is one item
//! among the several that interval pays for, so its own share is bounded
//! here at a tenth of it: a bar stated as a fraction of a budget that
//! already existed, never a number derived from what this code happens to
//! measure today -- the defect the spec withdrew two other bars for.
//!
//! The second assertion is the one that does not depend on the host at
//! all, and it is the real contract: rebasing the same twenty hunks in a
//! twenty-thousand-line buffer must cost the same as in a two-hundred-line
//! one. `rebase` is O(open hunks), never O(buffer), and a regression to a
//! whole-buffer scan would be invisible to a threshold on a small fixture.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::hint::black_box;
use std::time::{Duration, Instant};
use view_core::msg::BufferHandle;
use view_core::native::diff::{hunk, rebase, BufTextChangedEvent};

/// A tenth of the spec's 100 µs whole-input-path p99 bar. `rebase` shares
/// that interval with key decoding, the model dispatch, the msgpack
/// encode, the writability poll and the write, so a tenth is generous for
/// one addend and still fails long before the row it sits inside would.
const REBASE_BUDGET: Duration = Duration::from_micros(10);

/// Samples per measurement: enough that the reported median is not one
/// scheduler hiccup, few enough that the test stays instant.
const SAMPLES: usize = 200;

#[test]
fn rebasing_twenty_open_hunks_stays_inside_a_tenth_of_the_input_path_budget() {
    let median = measure(20, 10);
    assert!(
        median <= REBASE_BUDGET,
        "rebase over 20 open hunks measured {median:?} median, over the \
         {REBASE_BUDGET:?} share of the input path's own 100 µs p99 bar"
    );
}

#[test]
fn rebase_costs_the_same_in_a_twenty_thousand_line_buffer_as_in_a_small_one() {
    let small = measure(20, 10);
    let large = measure(20, 1000);
    let ceiling = small.max(REBASE_BUDGET) * 4;
    assert!(
        large <= ceiling,
        "the same 20 hunks cost {large:?} median in a 20,000-line buffer \
         against {small:?} in a 200-line one -- rebase is O(open hunks) and \
         must not have acquired a term in buffer size"
    );
}

/// The median wall time of one `rebase` call folding an edit above every
/// hunk of a `hunks`-hunk fixture whose changed rows are `spacing` apart.
fn measure(hunks: usize, spacing: usize) -> Duration {
    let (old, new) = fixture(hunks, spacing);
    let mut open = hunk::diff(Some(&old), &new);
    assert_eq!(open.len(), hunks);
    let change = BufTextChangedEvent {
        buf: BufferHandle(1),
        generation: 1,
        firstline: 0,
        lastline: 0,
        linedata: vec!["inserted above every hunk".to_string()],
        changedtick: 1,
        desynced: false,
    };
    for _ in 0..SAMPLES {
        rebase(&mut open, &change);
    }
    let mut timings = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        rebase(black_box(&mut open), black_box(&change));
        timings.push(started.elapsed());
    }
    timings.sort_unstable();
    timings[SAMPLES / 2]
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
