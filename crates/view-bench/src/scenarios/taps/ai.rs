//! The AI rows: view's agent panel measured through the same tap channel
//! the session-absent rows use.
//!
//! A child module of [`super`] rather than a sibling of it, because every
//! one of these rows is one of that module's samplers with a different
//! preamble in front of it -- the preparation, the chain resolution and
//! the outcome shape are all its private machinery, reached here through
//! `use super::*` instead of being widened for one caller.

use std::path::Path;
use std::time::Duration;

use super::*;

/// The taps a composer keystroke walks, and the labels for the intervals
/// between them.
///
/// It never reaches the engine: the composer is native state, so there is
/// no `S`/`W` handoff in the chain and the whole boundary is view's own
/// key-read to terminal-write. That is exactly why the row exists -- the
/// path with no RPC in it must be faster than the one that has it, and a
/// panel that repainted every row it covered for one typed character made
/// it slower.
const COMPOSER_CHAIN: &[u8] = b"KUBF";
const COMPOSER_LABELS: [&str; 5] = [
    "pty->key-read",
    "key-read->loop-wake",
    "loop-wake->draw-start",
    "draw-start->flush-start",
    "flush-start->term-written",
];

/// The composer text the `heavy` fixture seeds before it samples: one
/// line, opening with a character wider than a byte, far longer than the
/// rows the panel paints.
///
/// `minimal` oscillates between zero and one ASCII character, so every
/// paint it samples finds its row boundary by arithmetic on a grid that
/// counts a cell as a byte. That grid is exact only over ASCII. A line
/// opening with an em dash is folded character by character instead, and
/// this is the shape whose per-keystroke fold the composer's recorded row
/// boundaries exist to bound -- the one no other cell of the matrix
/// reaches. Sixty-four kilobytes so that folding the line and folding a
/// row differ by orders of magnitude rather than by noise.
#[must_use]
pub fn heavy_composer_seed() -> String {
    let mut seed = String::with_capacity(HEAVY_COMPOSER_BYTES + 4);
    seed.push('\u{2014}');
    seed.extend((0..HEAVY_COMPOSER_BYTES).map(|i| char::from(b'a' + (i % 26) as u8)));
    seed
}

const HEAVY_COMPOSER_BYTES: usize = 1 << 16;

/// The composer-echo row: one character typed into the open agent panel's
/// prompt, from the key arriving to the terminal write that shows it.
///
/// The prompt the keystroke lands in is the fixture's: `minimal` types
/// into an empty composer, `heavy` into [`heavy_composer_seed`].
///
/// Held to `echo`'s class of budget rather than paired against nvim,
/// because nvim has no counterpart to price it against: the composer is
/// view's own surface, so the bar is the recorded absolute rather than a
/// ratio.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] when the panel never opens, when `seed`
/// never reaches the screen, when a keystroke produces no terminal write
/// within the sample timeout, or when the tap stream dropped records.
pub fn run_ai_composer(
    spec: &SpawnSpec,
    pipe: &TapPipe,
    protocol: &Protocol,
    settle_deadline: Duration,
    seed: &str,
) -> Result<TapsOutcome, BenchError> {
    let mut session = prepare(spec, pipe, settle_deadline)?;
    ai_session::open_panel(&mut session)?;
    if !seed.is_empty() {
        ai_session::seed_composer(&mut session, seed)?;
    }
    // the panel's own opening frames are not this row's subject
    let _ = pipe.drain();
    let outcome = sample_ai_composer(&mut session, pipe, protocol);
    session.shutdown();
    outcome
}

fn sample_ai_composer(
    session: &mut BenchSession,
    pipe: &TapPipe,
    protocol: &Protocol,
) -> Result<TapsOutcome, BenchError> {
    let (overhead, overhead_pace) =
        characterize_overhead_adaptive(pipe, OVERHEAD_ITERATIONS, OVERHEAD_PACE)?;
    let mut trial_distributions = Vec::with_capacity(protocol.trials);
    let mut all_records = Vec::new();
    let mut pools: Vec<Vec<f64>> = vec![Vec::new(); COMPOSER_LABELS.len()];
    for _ in 0..protocol.trials {
        let mut deltas_ms = Vec::with_capacity(protocol.warmup + protocol.samples);
        for index in 0..(protocol.warmup + protocol.samples) {
            // every second sample deletes the character the one before it
            // typed: the input length oscillates between zero and one
            // instead of growing past the panel's width into a wrap, so
            // both keys are the same one-row edit
            let key: &[u8] = if index % 2 == 0 { b"x" } else { b"\x7f" };
            all_records.extend(pipe.drain());
            let t0 = monotonic_nanos();
            session.send(key)?;
            let Some((chain, written)) =
                wait_for_chained(pipe, protocol.sample_timeout, t0, COMPOSER_CHAIN, b'T')
            else {
                return Err(BenchError::Desync {
                    context: format!(
                        "no terminal write with a resolvable K/U/B/F chain behind it within {:?} \
                         of a composer keypress; screen:\n{}",
                        protocol.sample_timeout,
                        session.screen_text()
                    ),
                });
            };
            let Some(read) = chain.first() else {
                return Err(BenchError::Desync {
                    context: "empty tap chain for a composer keypress".to_string(),
                });
            };
            deltas_ms.push(delta_us(read.nanos, written.nanos) / 1000.0);
            if index >= protocol.warmup {
                let mut prev = t0;
                for (pool, hit) in pools.iter_mut().zip(&chain) {
                    pool.push(delta_us(prev, hit.nanos));
                    prev = hit.nanos;
                }
                if let Some(pool) = pools.get_mut(chain.len()) {
                    pool.push(delta_us(prev, written.nanos));
                }
            }
            std::thread::sleep(protocol.inter_sample);
        }
        trial_distributions.push(Distribution::from_samples(&deltas_ms, protocol.warmup)?);
    }
    all_records.extend(pipe.drain());
    verify_no_drops(&all_records)?;

    let p99s: Vec<f64> = trial_distributions.iter().map(Distribution::p99).collect();
    Ok(TapsOutcome {
        gated_p99: crate::sampling::median_of_trials(&p99s)?,
        trial_distributions,
        segments: summarize_segments(&COMPOSER_LABELS, &pools),
        overhead,
        overhead_pace,
        paints: PaintSplit::default(),
    })
}

/// The sampling one AI row drives, so both rows reach it through one
/// function pointer rather than one copy each.
type Sampler = fn(&mut BenchSession, &TapPipe, &Protocol) -> Result<TapsOutcome, BenchError>;

/// The input row's boundary, measured with an agent turn streaming into an
/// open panel: the "AI presence never degrades editor responsiveness"
/// mandate held to the same number the session-absent row records, rather
/// than asserted.
///
/// Everything but the session state is shared with [`run_input_path`] --
/// the same preparation, the same [`sample_input_path`] -- so a difference
/// between the two rows is a difference in what the session was doing,
/// which is the only thing this row exists to price.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] for everything [`run_input_path`] does,
/// plus a turn that never started or stopped streaming partway.
pub fn run_ai_session_active(
    spec: &SpawnSpec,
    pipe: &TapPipe,
    protocol: &Protocol,
    settle_deadline: Duration,
    cwd: &Path,
) -> Result<(TapsOutcome, u64), BenchError> {
    ai_row(
        spec,
        pipe,
        protocol,
        settle_deadline,
        cwd,
        sample_input_path,
    )
}

/// The output row's boundary under the same live turn, for the same reason
/// and with the same evidence -- see [`run_ai_session_active`].
///
/// # Errors
///
/// Returns [`BenchError::Desync`] for everything [`run_output_path`] does,
/// plus a turn that never started or stopped streaming partway.
pub fn run_ai_streaming(
    spec: &SpawnSpec,
    pipe: &TapPipe,
    protocol: &Protocol,
    settle_deadline: Duration,
    cwd: &Path,
) -> Result<(TapsOutcome, u64), BenchError> {
    ai_row(
        spec,
        pipe,
        protocol,
        settle_deadline,
        cwd,
        sample_output_path,
    )
}

/// One AI row: prepare the session, put a turn in flight, sample the
/// boundary `sample` measures, and confirm the turn is still streaming
/// while the session is still up.
///
/// The liveness check sits here, between the last sample and the teardown,
/// because after teardown every signal a dead agent leaves reads exactly
/// like a live one's. Both rows run this one body, so neither can drift
/// into checking less than the other.
fn ai_row(
    spec: &SpawnSpec,
    pipe: &TapPipe,
    protocol: &Protocol,
    settle_deadline: Duration,
    cwd: &Path,
    sample: Sampler,
) -> Result<(TapsOutcome, u64), BenchError> {
    let mut session = prepare(spec, pipe, settle_deadline)?;
    let turn = ai_session::start(&mut session, cwd)?;
    // the panel's own opening frames are not this row's subject, and
    // `prepare` drained on the same reasoning
    let _ = pipe.drain();
    let outcome = sample(&mut session, pipe, protocol);
    let streamed = turn.still_streaming();
    session.shutdown();
    Ok((outcome?, streamed?))
}
