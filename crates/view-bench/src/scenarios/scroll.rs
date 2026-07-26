//! The scroll scenario: sustained half-page scrolling through a
//! 100k-line file, measuring content staleness per keystroke: the time
//! from the scroll input byte until the CORRESPONDING scrolled content is
//! visible. Correspondence is checkable because every fixture line starts
//! with its own zero-padded line label (`L000042`): after each scroll the
//! top text row must show exactly the label the previous top plus the
//! editor's own constant scroll delta predicts, so a stale frame or a
//! partial repaint can never satisfy the wait.

use std::time::{Duration, Instant};

use crate::pairing::{paired_summary, PairedSummary};
use crate::sampling::{interleave_schedule, median_of_trials, Side};
use crate::scenarios::Protocol;
use crate::session::{BenchSession, SpawnSpec};
use crate::BenchError;

/// Width of the `L%06d` line label every fixture line starts with.
pub const LABEL_WIDTH: u16 = 7;

/// Lines the scroll fixture file must contain; the matrix's sampling
/// budget (trials x (warmup + samples) half-page scrolls per side) must
/// never run off the end of the file mid-trial.
pub const FIXTURE_LINES: usize = 100_000;

/// Renders the scroll fixture's content: `FIXTURE_LINES` lines, each
/// starting with its `L%06d` label (1-based, matching the editor's own
/// line numbering).
#[must_use]
pub fn fixture_content() -> String {
    let mut content = String::with_capacity(FIXTURE_LINES * 32);
    for line in 1..=FIXTURE_LINES {
        content.push_str(&format!("L{line:06} scroll benchmark line\n"));
    }
    content
}

/// Parses the line label at `(row, col)`, returning its number when the
/// cells hold a well-formed `L%06d`.
fn label_at(session: &mut BenchSession, row: u16, col: u16) -> Option<u32> {
    let text = session.with_screen(|screen| {
        let mut text = String::new();
        for offset in 0..LABEL_WIDTH {
            if let Some(cell) = screen.cell(row, col + offset) {
                text.push_str(cell.contents());
            }
        }
        text
    });
    let digits = text.strip_prefix('L')?;
    if digits.len() != 6 {
        return None;
    }
    digits.parse().ok()
}

/// One side's scroll state: where the label column sits and what the top
/// label currently reads.
struct SideState {
    session: BenchSession,
    label_row: u16,
    label_col: u16,
    top_line: u32,
    /// Lines one `<C-d>` advances the top row by; discovered from the
    /// first warmup scroll rather than assumed, then required constant.
    delta: Option<u32>,
    raw_ms: Vec<f64>,
}

impl SideState {
    fn prepare(spec: &SpawnSpec, settle_deadline: Duration) -> Result<Self, BenchError> {
        let mut session = BenchSession::spawn(spec)?;
        // Quiescence alone is not readiness: view's startup splash is a
        // static screen, so one settle pass can succeed before the engine
        // attaches and the fixture renders. Readiness here is the fixture
        // label actually being on screen, re-settling until the deadline.
        let deadline = Instant::now() + settle_deadline;
        let (label_row, label_col, top_line) = loop {
            if !session.settle(Duration::from_secs(2), settle_deadline) {
                return Err(BenchError::Desync {
                    context: format!(
                        "startup never went quiet within {settle_deadline:?}; screen:\n{}",
                        session.screen_text()
                    ),
                });
            }
            if let Some(origin) = find_label_origin(&mut session) {
                break origin;
            }
            if Instant::now() >= deadline {
                return Err(BenchError::Desync {
                    context: format!(
                        "no L-numbered fixture line visible within {settle_deadline:?}; screen:\n{}",
                        session.screen_text()
                    ),
                });
            }
        };
        Ok(Self {
            session,
            label_row,
            label_col,
            top_line,
            delta: None,
            raw_ms: Vec::new(),
        })
    }

    /// Sends one `<C-d>` and records how long the corresponding top label
    /// took to appear. The first scroll of a session discovers the
    /// editor's constant per-scroll delta (waiting only for the label to
    /// change); every later scroll requires exactly `top + delta`.
    fn sample_one(&mut self, timeout: Duration, inter_sample: Duration) -> Result<(), BenchError> {
        let previous = self.top_line;
        let expected = self.delta.map(|d| previous + d);
        let start = Instant::now();
        self.session.send(b"\x04")?;
        let deadline = start + timeout;
        let new_top = loop {
            if let Some(label) = label_at(&mut self.session, self.label_row, self.label_col) {
                match expected {
                    Some(target) if label == target => break label,
                    None if label > previous => break label,
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(BenchError::Desync {
                    context: format!(
                        "top label never advanced from L{previous:06} to {}; screen:\n{}",
                        expected
                            .map_or_else(|| "any higher label".to_string(), |t| format!("L{t:06}")),
                        self.session.screen_text()
                    ),
                });
            }
            std::thread::yield_now();
        };
        self.raw_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        if self.delta.is_none() {
            self.delta = Some(new_top - previous);
        }
        self.top_line = new_top;
        std::thread::sleep(inter_sample);
        Ok(())
    }
}

/// Scans the screen for the first row whose leading cells parse as a
/// `L%06d` label, trying a handful of left offsets so a number/sign
/// gutter cannot hide the label column.
fn find_label_origin(session: &mut BenchSession) -> Option<(u16, u16, u32)> {
    let (rows, cols) = session.with_screen(vt100::Screen::size);
    for row in 0..rows {
        for col in 0..cols.saturating_sub(LABEL_WIDTH).min(16) {
            let text = row_text_at(session, row, col);
            if looks_like_label(&text) {
                if let Some(line) = label_at(session, row, col) {
                    return Some((row, col, line));
                }
            }
        }
    }
    None
}

fn row_text_at(session: &mut BenchSession, row: u16, col: u16) -> String {
    session.with_screen(|screen| crate::boundaries::row_text_from(screen, row, col, LABEL_WIDTH))
}

fn looks_like_label(text: &str) -> bool {
    text.strip_prefix('L')
        .is_some_and(|digits| digits.len() == 6 && digits.chars().all(|c| c.is_ascii_digit()))
}

/// The scroll run's outcome.
#[derive(Debug)]
pub struct ScrollOutcome {
    pub trials: Vec<PairedSummary>,
    /// Median across trials of the view side's staleness p99.
    pub gated_staleness_p99_ms: f64,
    /// Median across trials of the per-trial p50 ratio.
    pub gated_ratio_p50: f64,
    /// Median across trials of the per-trial p99 ratio.
    pub gated_ratio_p99: f64,
}

/// Drives the full scroll scenario: both editors opened on the same
/// generated fixture file, `protocol.trials` interleaved trials of
/// `warmup + samples` half-page scrolls per side, continuing down the
/// file across trials (the fixture is sized so the matrix never reaches
/// the bottom).
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if either editor's top label stops
/// advancing as predicted, or any underlying session error.
pub fn run(
    view: &SpawnSpec,
    nvim: &SpawnSpec,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<ScrollOutcome, BenchError> {
    let per_side_total = protocol.trials * (protocol.warmup + protocol.samples);
    // a half-page scroll advances by at most half the grid height;
    // refusing here beats a mid-run desync at the file's bottom edge
    let needed = per_side_total * usize::from(crate::session::GRID_ROWS / 2)
        + usize::from(crate::session::GRID_ROWS);
    if needed > FIXTURE_LINES {
        return Err(BenchError::Desync {
            context: format!(
                "scroll budget needs up to {needed} fixture lines but the fixture has \
                 {FIXTURE_LINES}; lower --samples/--trials"
            ),
        });
    }

    let mut view_state = SideState::prepare(view, settle_deadline).map_err(|e| label("view", e))?;
    let mut nvim_state = SideState::prepare(nvim, settle_deadline).map_err(|e| label("nvim", e))?;

    let mut trials = Vec::with_capacity(protocol.trials);
    for trial in 0..protocol.trials {
        view_state.raw_ms.clear();
        nvim_state.raw_ms.clear();
        let start = if trial % 2 == 0 {
            Side::View
        } else {
            Side::Nvim
        };
        let per_side = protocol.warmup + protocol.samples;
        for block in interleave_schedule(per_side, protocol.block, start) {
            let (state, side_name) = match block.side {
                Side::View => (&mut view_state, "view"),
                Side::Nvim => (&mut nvim_state, "nvim"),
            };
            for _ in 0..block.count {
                state
                    .sample_one(protocol.sample_timeout, protocol.inter_sample)
                    .map_err(|e| label(side_name, e))?;
            }
        }
        trials.push(paired_summary(
            &view_state.raw_ms,
            &nvim_state.raw_ms,
            protocol.warmup,
        )?);
    }

    view_state.session.shutdown();
    nvim_state.session.shutdown();

    let stalenesses: Vec<f64> = trials.iter().map(|t| t.view.p99()).collect();
    let median_ratios: Vec<f64> = trials.iter().map(|t| t.ratio_p50).collect();
    let ratios: Vec<f64> = trials.iter().map(|t| t.ratio_p99).collect();
    Ok(ScrollOutcome {
        gated_staleness_p99_ms: median_of_trials(&stalenesses)?,
        gated_ratio_p50: median_of_trials(&median_ratios)?,
        gated_ratio_p99: median_of_trials(&ratios)?,
        trials,
    })
}

fn label(side: &str, err: BenchError) -> BenchError {
    match err {
        BenchError::Desync { context } => BenchError::Desync {
            context: format!("[{side} side] {context}"),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn fixture_content_labels_every_line_one_based() {
        let content = fixture_content();
        let mut lines = content.lines();
        assert_eq!(lines.next().unwrap(), "L000001 scroll benchmark line");
        assert_eq!(content.lines().count(), FIXTURE_LINES);
        assert!(content.ends_with("L100000 scroll benchmark line\n"));
    }

    #[test]
    fn label_recognition_requires_the_full_zero_padded_shape() {
        assert!(looks_like_label("L000042"));
        assert!(!looks_like_label("L42"));
        assert!(!looks_like_label("X000042"));
        assert!(!looks_like_label("L00004x"));
    }
}
