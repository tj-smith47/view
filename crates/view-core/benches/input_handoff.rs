//! What one keystroke costs to cross from the input thread to the runtime
//! loop, measured on the production primitive.
//!
//! The tapped `echo_path` row puts `key-decoded->loop-wake` at 60.8us p50
//! on dev-linux, which is the largest single view-owned segment of the
//! typing round trip and roughly a third of the whole view-vs-nvim gap.
//! That segment is one `SyncSender::send` and the `recv()` that wakes for
//! it, so either the primitive really costs that much on this host or the
//! tapped number is measuring something else. This bench answers which
//! without the editor, the pty, or the tap channel in the way.
//!
//! Deliberately not a criterion bench of throughput: the quantity that
//! matters is the *latency of one handoff to an otherwise idle receiver*,
//! which is what the runtime loop is when a key arrives during steady
//! typing. Sending in a tight loop measures a hot queue instead, and a hot
//! queue never parks, so it cannot see the cost the editor pays.

use std::sync::mpsc::{sync_channel, SyncSender};
use std::time::{Duration, Instant};

/// The production channel's capacity (`startup::MSG_CHANNEL_CAPACITY`,
/// tied to `KEY_RING_CAPACITY`). Restated rather than imported because
/// `view-core` sits below the binary crate that defines it.
const MSG_CHANNEL_CAPACITY: usize = 66;

/// Ceiling on handoffs measured per row. Large enough for a stable p99 at
/// this cost scale.
const MAX_SAMPLES: usize = 20_000;

/// Wall-clock each row is allowed. The sweep spans a 200x range of idle
/// gaps, so a fixed sample count spends 200x longer on the deep end than
/// the shallow one and puts the whole bench out of reach of a gate. A
/// fixed time budget instead gives every row the same attention and caps
/// the total, at the cost of fewer tail samples where the gap is widest.
const ROW_BUDGET: Duration = Duration::from_secs(50);

/// Idle gaps swept between handoffs. How long the receiver has been parked
/// is not a nuisance parameter here: a core idle for milliseconds sits in a
/// deeper sleep state than one idle for microseconds, and exiting it is
/// charged to whoever wakes it. The echo row paces at 10ms and a human
/// types slower still, so the deep end is the case the editor actually
/// meets, not a pathological one.
const IDLE_GAPS: &[Duration] = &[
    Duration::from_micros(50),
    Duration::from_micros(200),
    Duration::from_millis(1),
    Duration::from_millis(10),
];

/// The message the input thread actually sends: an owned notation string,
/// so the handoff carries a heap allocation the way `Msg::Key` does.
struct Keystroke {
    notation: String,
    sent_at: Instant,
}

/// How many handoffs fit in [`ROW_BUDGET`] at this idle gap, capped at
/// [`MAX_SAMPLES`].
fn samples_for(gap: Duration) -> usize {
    let gap_nanos = gap.as_nanos().max(1);
    let fits = usize::try_from(ROW_BUDGET.as_nanos() / gap_nanos).unwrap_or(MAX_SAMPLES);
    fits.clamp(1, MAX_SAMPLES)
}

fn run(tx: &SyncSender<Keystroke>, gap: Duration) -> Vec<f64> {
    let samples = samples_for(gap);
    let mut out = Vec::with_capacity(samples);
    for _ in 0..samples {
        std::thread::sleep(gap);
        let sent_at = Instant::now();
        if tx
            .send(Keystroke {
                notation: "x".to_string(),
                sent_at,
            })
            .is_err()
        {
            break;
        }
        out.push(0.0);
    }
    out
}

fn percentile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx]
}

/// Relays one hop further, as the runtime loop does when it hands an
/// encoded RPC message to the writer thread. Returns the far end's
/// receiver so the caller times the whole chain rather than each link.
fn add_hop(rx: std::sync::mpsc::Receiver<Keystroke>) -> std::sync::mpsc::Receiver<Keystroke> {
    let (tx, out) = sync_channel::<Keystroke>(MSG_CHANNEL_CAPACITY);
    std::thread::spawn(move || {
        while let Ok(key) = rx.recv() {
            std::hint::black_box(key.notation.len());
            if tx.send(key).is_err() {
                break;
            }
        }
    });
    out
}

fn measure(gap: Duration, hops: usize) -> Vec<f64> {
    let (tx, rx) = sync_channel::<Keystroke>(MSG_CHANNEL_CAPACITY);
    let mut rx = rx;
    for _ in 1..hops {
        rx = add_hop(rx);
    }
    let receiver = std::thread::spawn(move || {
        let mut latencies = Vec::with_capacity(samples_for(gap));
        while let Ok(key) = rx.recv() {
            let woke = Instant::now();
            #[allow(clippy::cast_precision_loss)]
            latencies.push(woke.duration_since(key.sent_at).as_nanos() as f64 / 1000.0);
            // the runtime loop does real work per message; without it the
            // receiver re-enters recv() so fast it can spin rather than
            // park, which is not the state a keystroke actually finds it in
            std::hint::black_box(key.notation.len());
        }
        latencies
    });
    let _ = run(&tx, gap);
    drop(tx);
    let mut latencies = receiver.join().unwrap_or_default();
    latencies.sort_by(f64::total_cmp);
    latencies
}

fn main() {
    println!(
        "input handoff: SyncSender<Keystroke> capacity {MSG_CHANNEL_CAPACITY}, parked receivers, \
         up to {MAX_SAMPLES} samples per row within {ROW_BUDGET:?}"
    );
    println!("  idle gap | hops | samples |    p50 |    p90 |    p99");
    for gap in IDLE_GAPS {
        for hops in [1_usize, 2] {
            let latencies = measure(*gap, hops);
            println!(
                "  {:>7?} | {hops:>4} | {:>7} | {:6.2} | {:6.2} | {:6.2}",
                gap,
                latencies.len(),
                percentile(&latencies, 0.50),
                percentile(&latencies, 0.90),
                percentile(&latencies, 0.99),
            );
        }
    }
}
