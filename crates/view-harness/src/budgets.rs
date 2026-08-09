//! Spec 3.1's budget table, in the form the bench gate can hold a
//! measurement to, plus the ledger of budgets the project has not met yet.
//!
//! This is a different question from the one [`crate::baselines`] answers.
//! A baseline asks "did this change make the number worse than last time";
//! a budget asks "is the number where the spec says it must be". A run can
//! pass one and fail the other, and conflating them is how a metric ends up
//! regression-green forever at a value the spec never accepted.
//!
//! File shape (`crates/view-bench/budgets.toml`):
//!
//! ```toml
//! schema = 1
//!
//! [[budget]]
//! spec_row = "view output path: redraw event parsed -> terminal write"
//! scenario = "output_path"
//! metric = "p99_ms"
//! max = 1.0
//! classes = ["dev-linux"]   # optional; absent means every class
//!
//! [[shortfall]]
//! scenario = "echo"
//! fixture = "minimal"
//! metric = "ratio_p50"
//! class = "dev-linux"
//! accepted = 1.3537866848241882
//! why = "unattributed; see task 19"
//! ```
//!
//! Every budget is an upper bound, because every metric the harness records
//! is lower-is-better (the invariant [`crate::baselines`] rests on too).
//!
//! The shortfall list is what makes an unmet budget representable without
//! being ignorable. Outside budget with no entry is a new shortfall and
//! fails; outside budget and past the ceiling its accepted value earns is a
//! widening shortfall and fails; outside budget but inside that ceiling
//! passes and is reported every run.
//!
//! The ceiling is the one [`crate::baselines`] already grants the recorded
//! bar for that metric on that class, not the accepted value itself. An
//! accepted value is one sample of a noisy statistic; comparing the next
//! sample to it exactly makes every listed shortfall a coin flip rather
//! than a gate.
//!
//! There is one further state, and it exists for the same reason: a metric
//! whose *recorded* value on this class is inside the bound, measuring
//! above the bound this run, on a class that has published a measured
//! spread for that statistic. A single reading inside the spread the class
//! has characterized around a compliant recorded value is not evidence the
//! bound stopped being met, so demanding a `[[shortfall]]` entry for it
//! would write ambient load into the ledger as an accepted gap. It passes,
//! bounded by the same ceiling the recorded value earns, and it reports
//! every run. Absent a published spread for the statistic the strict rule
//! stands unchanged, because a default allowance is a guess about the host
//! rather than a measurement of it.
//!
//! No state is quiet: everything except a value inside its bound prints.

use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

/// The one schema this loader understands.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// One spec budget: an upper bound on one metric of one scenario.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Budget {
    /// The spec 3.1 row this bound comes from, quoted closely enough that
    /// the drift check can find it in the spec text.
    pub spec_row: String,
    pub scenario: String,
    pub metric: String,
    pub max: f64,
    /// Machine classes this bound applies to; `None` means all of them.
    pub classes: Option<Vec<String>>,
}

impl Budget {
    /// Whether this bound applies to `class`.
    #[must_use]
    pub fn covers(&self, class: &str) -> bool {
        self.classes
            .as_ref()
            .is_none_or(|classes| classes.iter().any(|c| c == class))
    }
}

/// One budget the project has measured itself outside of, and the value it
/// was accepted at.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Shortfall {
    pub scenario: String,
    pub fixture: String,
    pub metric: String,
    pub class: String,
    /// The measured value when this shortfall was accepted. A later run may
    /// match or improve on it; anything worse fails.
    pub accepted: f64,
    pub why: String,
}

/// The parsed budget file.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct BudgetFile {
    pub schema: u32,
    #[serde(default)]
    pub budget: Vec<Budget>,
    #[serde(default)]
    pub shortfall: Vec<Shortfall>,
}

/// What the budget check concluded about one measured metric.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Verdict {
    /// Inside the spec bound.
    Inside,
    /// Outside the bound, listed as a shortfall, and inside the ceiling
    /// that shortfall's accepted value earns.
    Held { accepted: f64, why: String },
    /// Outside the bound with no shortfall entry: a budget the project
    /// stopped meeting without recording that it had.
    New,
    /// Outside the bound and past the ceiling the accepted value earns.
    Widened {
        accepted: f64,
        ceiling: f64,
        headroom: crate::baselines::Headroom,
    },
    /// Outside the bound, with no shortfall entry, on a class that has
    /// published a measured spread for this statistic and whose recorded
    /// value for it is itself inside the bound: a reading the class's own
    /// characterized noise accounts for, held to the ceiling that recorded
    /// value earns.
    Excursion {
        recorded: f64,
        ceiling: f64,
        headroom: crate::baselines::Headroom,
    },
}

impl Verdict {
    /// Whether this verdict must fail the gate.
    ///
    /// [`Self::Excursion`] does not, and that is the one place the answer is
    /// not obvious: it is a value past the spec bound. It passes because the
    /// class has measured that this statistic moves that far between runs on
    /// unchanged code, so failing on it would gate on ambient load. It is
    /// still printed every run, and anything past the ceiling is
    /// [`Self::New`] again.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::New | Self::Widened { .. })
    }
}

/// One metric's budget result, carrying everything a report line needs.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Finding {
    pub scenario: String,
    pub fixture: String,
    pub metric: String,
    pub measured: f64,
    pub budget: f64,
    pub spec_row: String,
    pub verdict: Verdict,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            scenario,
            fixture,
            metric,
            measured,
            budget,
            spec_row,
            verdict,
        } = self;
        match verdict {
            Verdict::Inside => write!(
                f,
                "budget OK [{scenario}.{fixture}] {metric}: {measured:.3} within {budget:.3}"
            ),
            Verdict::Held { accepted, why } => write!(
                f,
                "BUDGET SHORTFALL [{scenario}.{fixture}] {metric}: {measured:.3} against spec \
                 {budget:.3} (accepted at {accepted:.3}; {why}) [{spec_row}]"
            ),
            Verdict::New => write!(
                f,
                "BUDGET FAIL [{scenario}.{fixture}] {metric}: {measured:.3} against spec \
                 {budget:.3}, and no shortfall records it. Either fix it, or add a [[shortfall]] \
                 entry to budgets.toml saying why it stands [{spec_row}]"
            ),
            Verdict::Widened {
                accepted,
                ceiling,
                headroom,
            } => write!(
                f,
                "BUDGET FAIL [{scenario}.{fixture}] {metric}: {measured:.3} against spec \
                 {budget:.3} is past {ceiling:.3}, which is the accepted shortfall \
                 {accepted:.3} {headroom} on this class; a shortfall may hold or improve, never \
                 widen [{spec_row}]"
            ),
            Verdict::Excursion {
                recorded,
                ceiling,
                headroom,
            } => write!(
                f,
                "BUDGET EXCURSION [{scenario}.{fixture}] {metric}: {measured:.3} is past spec \
                 {budget:.3}, but inside {ceiling:.3}, which is its recorded {recorded:.3} \
                 {headroom} -- the spread this class has measured for the statistic, applied to \
                 a quiet value that is itself inside the bound. One reading this far out is that \
                 measured spread and not a budget miss; anything past the ceiling fails \
                 [{spec_row}]"
            ),
        }
    }
}

/// Errors loading or validating the budget file.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BudgetError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: String,
        source: Box<toml::de::Error>,
    },
    #[error("{path} is schema {found}, this build understands {SUPPORTED_SCHEMA}")]
    Schema { path: String, found: u32 },
    #[error(
        "{path}: [[shortfall]] {scenario}.{fixture} {metric} on {class} names no budget; a \
         shortfall can only stand against a bound that exists"
    )]
    OrphanShortfall {
        path: String,
        scenario: String,
        fixture: String,
        metric: String,
        class: String,
    },
    #[error(
        "{path}: [[{table}]] for {scenario} names metric {metric}, which \
         baselines::RECORDED_METRICS does not declare; no row produces it, so the bound would \
         never be checked against anything"
    )]
    UnknownMetric {
        path: String,
        table: &'static str,
        scenario: String,
        metric: String,
    },
}

/// Loads and validates the budget file.
///
/// # Errors
///
/// Returns [`BudgetError`] if the file cannot be read or parsed, carries an
/// unsupported schema, or lists a shortfall against a budget that does not
/// exist for that class.
pub fn load(path: &Path) -> Result<BudgetFile, BudgetError> {
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|source| BudgetError::Read {
        path: display.clone(),
        source,
    })?;
    parse(&text, &display)
}

/// [`load`]'s validation over text already in hand, with `display` naming
/// the source in any error. Kept separate so the rules can be exercised
/// without a file on disk.
///
/// # Errors
///
/// Returns [`BudgetError`] if the text does not parse, carries an
/// unsupported schema, or lists a shortfall against a budget that does not
/// exist for that class.
pub fn parse(text: &str, display: &str) -> Result<BudgetFile, BudgetError> {
    let display = display.to_string();
    let file: BudgetFile = toml::from_str(text).map_err(|source| BudgetError::Parse {
        path: display.clone(),
        source: Box::new(source),
    })?;
    if file.schema != SUPPORTED_SCHEMA {
        return Err(BudgetError::Schema {
            path: display,
            found: file.schema,
        });
    }
    // a bound on a metric no row produces is checked against nothing and
    // reports nothing: the spec row it quotes silently stops being enforced,
    // which is exactly what a bound exists to prevent. A typo is the usual
    // way in, so the name is held to the same declared vocabulary the
    // measured side is
    for (table, scenario, metric) in file
        .budget
        .iter()
        .map(|b| ("budget", &b.scenario, &b.metric))
        .chain(
            file.shortfall
                .iter()
                .map(|s| ("shortfall", &s.scenario, &s.metric)),
        )
    {
        if !crate::baselines::RECORDED_METRICS.contains(&metric.as_str()) {
            return Err(BudgetError::UnknownMetric {
                path: display.clone(),
                table,
                scenario: scenario.clone(),
                metric: metric.clone(),
            });
        }
    }
    // a shortfall against no budget is dead weight that reads as an accepted
    // gap: it would sit in the file forever describing a bound nobody checks
    for shortfall in &file.shortfall {
        if find_budget(
            &file,
            &shortfall.scenario,
            &shortfall.metric,
            &shortfall.class,
        )
        .is_none()
        {
            return Err(BudgetError::OrphanShortfall {
                path: display,
                scenario: shortfall.scenario.clone(),
                fixture: shortfall.fixture.clone(),
                metric: shortfall.metric.clone(),
                class: shortfall.class.clone(),
            });
        }
    }
    Ok(file)
}

fn find_budget<'a>(
    file: &'a BudgetFile,
    scenario: &str,
    metric: &str,
    class: &str,
) -> Option<&'a Budget> {
    file.budget
        .iter()
        .find(|b| b.scenario == scenario && b.metric == metric && b.covers(class))
}

fn find_shortfall<'a>(
    file: &'a BudgetFile,
    scenario: &str,
    fixture: &str,
    metric: &str,
    class: &str,
) -> Option<&'a Shortfall> {
    file.shortfall.iter().find(|s| {
        s.scenario == scenario && s.fixture == fixture && s.metric == metric && s.class == class
    })
}

/// The worst value a shortfall accepted at `accepted` may still measure.
///
/// An accepted value is one sample of a noisy statistic, not a constant, so
/// comparing the next sample to it exactly makes any listed shortfall a
/// coin flip: the first re-run after this ledger was written measured
/// `echo.minimal` ratio_p50 at 1.176 against an accepted 1.172 and failed
/// the gate on a 0.35% difference. The ceiling is therefore the same one
/// [`crate::baselines`] grants the recorded bar for this metric on this
/// class, so the two gates agree about what counts as a regression instead
/// of one of them firing on measurement noise the other was built to
/// absorb. A metric the class does not gate at all (shared-class tail
/// statistics) gets no ceiling here either, for the same reason it gets
/// none there: on a shared host that number is not a property of the code,
/// and `None` here is that absence rather than a sentinel standing in for it.
fn shortfall_ceiling(
    scenario: &str,
    metric: &str,
    class: &str,
    accepted: f64,
    headroom_table: &crate::baselines::HeadroomTable,
) -> Option<(crate::baselines::Headroom, f64)> {
    let controlled = crate::baselines::is_controlled_class(class);
    let headroom = crate::baselines::headroom_for(headroom_table, scenario, metric, controlled)?;
    Some((headroom, headroom.bar(accepted)))
}

/// Whether a reading inside its budget is inside by more than the spread
/// this class grants the statistic -- the worst draw the spread allows for
/// this reading would still be inside the bound.
///
/// The mirror of [`shortfall_ceiling`], for the same reason: one sample of
/// a noisy statistic proves neither a regression nor a fix. A metric the
/// class does not gate gets no spread and one inside reading stays one
/// sample, so the entry stands.
fn provably_inside(
    scenario: &str,
    metric: &str,
    class: &str,
    measured: f64,
    budget: f64,
    headroom_table: &crate::baselines::HeadroomTable,
) -> bool {
    let controlled = crate::baselines::is_controlled_class(class);
    crate::baselines::headroom_for(headroom_table, scenario, metric, controlled)
        .is_some_and(|headroom| headroom.bar(measured) <= budget)
}

/// The recorded value and the worst reading it accounts for, when a metric
/// above its bound this run is inside what this class has measured the
/// statistic to move; `None` when nothing earns that.
///
/// Four conditions, each load-bearing:
///
/// - The class published a spread for this statistic. A compiled-in default
///   is a guess about a host nobody characterized, and widening every spec
///   bound on every class by a guess is how a budget stops meaning anything.
///   Absence keeps the strict rule.
/// - A recorded value exists for this metric in this cell. It is the
///   quiet-run reading the excursion is measured against; with no baseline
///   there is nothing to say this run is an excursion *from* anything.
/// - That recorded value is inside the bound. A cell whose quiet value is
///   already outside its budget has a real, standing gap, and the mechanism
///   for that is a `[[shortfall]]` entry with the reason written down --
///   never a noise allowance that would hide it.
/// - The spread is proportional. A signed metric's allowance carries
///   [`crate::baselines::SIGNED_DELTA_FLOOR_MS`], an absolute floor sized
///   for a paired delta's own jitter around a *recorded* value; against a
///   spec bound it would grant a fixed 0.25 ms band no published factor can
///   shrink, so a bound at 0.1 ms would tolerate 0.35. That floor is a
///   ratchet instrument and does not transfer here. No signed metric carries
///   a budget today, and refusing the shape keeps it that way by
///   construction rather than by a comment nobody reads when the first one
///   lands.
///
/// The ceiling is then the recorded value under the published spread, which
/// is the same bar [`crate::baselines`] holds that recorded value to. The
/// two gates therefore fail at the same number instead of one of them
/// firing on noise the other was built to absorb.
fn excursion_ceiling(
    scenario: &str,
    metric: &str,
    budget: f64,
    recorded: Option<&crate::baselines::CellMetrics>,
    headroom_table: &crate::baselines::HeadroomTable,
) -> Option<(crate::baselines::Headroom, f64, f64)> {
    let headroom = crate::baselines::declared_headroom(headroom_table, scenario, metric)?;
    if headroom.admits_non_positive() {
        return None;
    }
    let recorded = *recorded?.get(metric)?;
    (recorded <= budget).then(|| (headroom, recorded, headroom.bar(recorded)))
}

/// Checks every measured cell of one run against the budgets that cover
/// `class`, in the order the cells were measured.
///
/// The run-level entry point, because the budget verdict for a cell is not a
/// function of that cell alone: it also reads the cell the *baseline* holds
/// for the same id. Pairing the two is therefore done here, from one
/// `baseline`, rather than at each call site where a `None` or the wrong
/// cell's metrics would silently return the gate to its strict behavior with
/// every test still green.
#[must_use]
pub fn check_run(
    file: &BudgetFile,
    baseline: &crate::baselines::BaselineFile,
    measured: &[crate::baselines::MeasuredCell],
    class: &str,
    headroom_table: &crate::baselines::HeadroomTable,
) -> Vec<Finding> {
    measured
        .iter()
        .flat_map(|cell| check_cell(file, cell, baseline, class, headroom_table))
        .collect()
}

/// Checks one cell's measured metrics against the budgets that cover
/// `class`, in deterministic (metric-name) order.
///
/// Metrics with no budget produce no finding: a metric the spec states no
/// bound for is a real state, not a gap (see `budgets.toml`).
///
/// The cell arrives whole rather than as its scenario and fixture names
/// side by side, because those two are interchangeable strings: named the
/// other way round every budget lookup misses, every finding disappears,
/// and the gate reports the same zero failures a fully compliant matrix
/// does. `unreached_budgets` cannot see it either -- that walk reads the
/// measured ids, which are still correct.
///
/// `baseline` arrives whole for the same reason, one level up: the recorded
/// value is what tells a reading past the bound on a noisy class apart from
/// a bound the project stopped meeting (see [`excursion_ceiling`]), and it
/// is looked up here under the measured cell's own id. Handing this function
/// the recorded metrics directly would make "the wrong cell's numbers" and
/// "no numbers at all" both spellable, and both are silent: the first
/// re-sizes the ceiling from an unrelated cell, the second reverts the gate
/// to its strict behavior, and neither fails a test that does not already
/// know to look.
#[must_use]
pub fn check_cell(
    file: &BudgetFile,
    cell: &crate::baselines::MeasuredCell,
    baseline: &crate::baselines::BaselineFile,
    class: &str,
    headroom_table: &crate::baselines::HeadroomTable,
) -> Vec<Finding> {
    let (scenario, fixture) = (cell.id.scenario.as_str(), cell.id.fixture.as_str());
    let recorded = baseline.cell(&cell.id);
    let mut findings = Vec::new();
    for (metric, &measured) in &cell.metrics {
        let Some(budget) = find_budget(file, scenario, metric, class) else {
            continue;
        };
        let verdict = if measured <= budget.max {
            Verdict::Inside
        } else {
            match find_shortfall(file, scenario, fixture, metric, class) {
                None => {
                    match excursion_ceiling(scenario, metric, budget.max, recorded, headroom_table)
                    {
                        Some((headroom, recorded, ceiling)) if measured <= ceiling => {
                            Verdict::Excursion {
                                recorded,
                                ceiling,
                                headroom,
                            }
                        }
                        _ => Verdict::New,
                    }
                }
                Some(shortfall) => {
                    match shortfall_ceiling(
                        scenario,
                        metric,
                        class,
                        shortfall.accepted,
                        headroom_table,
                    ) {
                        Some((headroom, ceiling)) if measured > ceiling => Verdict::Widened {
                            accepted: shortfall.accepted,
                            ceiling,
                            headroom,
                        },
                        _ => Verdict::Held {
                            accepted: shortfall.accepted,
                            why: shortfall.why.clone(),
                        },
                    }
                }
            }
        };
        findings.push(Finding {
            scenario: scenario.to_string(),
            fixture: fixture.to_string(),
            metric: metric.clone(),
            measured,
            budget: budget.max,
            spec_row: budget.spec_row.clone(),
            verdict,
        });
    }
    findings
}

/// Shortfalls covering `class` whose metric this run measured inside its
/// budget by more than the spread the class grants the statistic, in
/// deterministic order: the gap is fixed and the entry is stale.
///
/// Inside by less than that spread proves nothing: a statistic whose
/// honest draws straddle the bound (dev-macos `echo.heavy` ratio_p50 spans
/// 0.974-1.251 across clean same-commit runs against a 1.1 bound) would
/// otherwise flip between a stale entry on a low draw and a missing one on
/// a high draw, with no ledger state that passes both. The spread is the
/// same one [`shortfall_ceiling`] grants above the bound, so the two
/// directions agree about what one sample can prove.
///
/// A shortfall whose cell produced no finding this run is unvisited, not
/// stale: a run that never measured the metric (a single-cell invocation,
/// or a platform that skips the scenario) has no reading to prove
/// anything with, and reporting it spent would claim a measurement that
/// never happened.
#[must_use]
pub fn unreached_shortfalls<'a>(
    file: &'a BudgetFile,
    class: &str,
    findings: &[Finding],
    headroom_table: &crate::baselines::HeadroomTable,
) -> Vec<&'a Shortfall> {
    let mut unreached: Vec<&Shortfall> = file
        .shortfall
        .iter()
        .filter(|s| s.class == class)
        .filter(|s| {
            let mut matched = findings.iter().filter(|f| {
                f.scenario == s.scenario && f.fixture == s.fixture && f.metric == s.metric
            });
            let mut visited = false;
            let live = matched.any(|f| {
                visited = true;
                f.verdict != Verdict::Inside
                    || !provably_inside(
                        &f.scenario,
                        &f.metric,
                        class,
                        f.measured,
                        f.budget,
                        headroom_table,
                    )
            });
            visited && !live
        })
        .collect();
    unreached.sort_by(|a, b| {
        (&a.scenario, &a.fixture, &a.metric).cmp(&(&b.scenario, &b.fixture, &b.metric))
    });
    unreached
}

/// Budgets covering `class` whose scenario ran but never produced the
/// metric they bound, in deterministic order.
///
/// A budget only binds while some row produces its metric under its
/// scenario. One that pairs a real metric with a scenario that does not
/// measure it matches nothing in [`check_cell`], produces no finding, and
/// so reports exactly as a scenario inside its budget would: the spec row
/// it quotes stops being enforced without anything failing. The load-time
/// vocabulary check cannot see this, because a name only becomes wrong once
/// it is paired with a scenario, and that pairing is only observable after
/// the matrix has run.
///
/// A scenario that did not run at all this invocation is not reported: a
/// platform that skips a row, or a single-cell invocation, is a coverage
/// question the caller already answers elsewhere, not a dead bound.
#[must_use]
pub fn unreached_budgets<'a>(
    file: &'a BudgetFile,
    class: &str,
    measured: &[crate::baselines::MeasuredCell],
) -> Vec<&'a Budget> {
    let mut unreached: Vec<&Budget> = file
        .budget
        .iter()
        .filter(|budget| budget.covers(class))
        .filter(|budget| {
            let ran = measured
                .iter()
                .any(|cell| cell.id.scenario == budget.scenario);
            let produced = measured.iter().any(|cell| {
                cell.id.scenario == budget.scenario && cell.metrics.contains_key(&budget.metric)
            });
            ran && !produced
        })
        .collect();
    unreached.sort_by(|a, b| (&a.scenario, &a.metric).cmp(&(&b.scenario, &b.metric)));
    unreached
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::baselines::CellMetrics;

    fn file_from(text: &str) -> BudgetFile {
        parse(text, "test").unwrap()
    }

    /// A baseline holding one cell, or an empty one when `recorded` is
    /// `None`.
    fn baseline_with(
        scenario: &str,
        fixture: &str,
        recorded: Option<&CellMetrics>,
    ) -> crate::baselines::BaselineFile {
        let mut file = crate::baselines::BaselineFile::new("test-class", "v0");
        if let Some(recorded) = recorded {
            file.cells.insert(
                scenario.to_string(),
                [(fixture.to_string(), recorded.clone())]
                    .into_iter()
                    .collect(),
            );
        }
        file
    }

    /// [`super::check_cell`] with no measured per-class headroom and an
    /// empty baseline, so every case below reads against the policy defaults
    /// and the strict outside-budget rule. The override paths have their own
    /// tests rather than being threaded through all of them.
    fn check_cell(
        file: &BudgetFile,
        scenario: &str,
        fixture: &str,
        metrics: &CellMetrics,
        class: &str,
    ) -> Vec<Finding> {
        super::check_cell(
            file,
            &crate::baselines::MeasuredCell {
                id: crate::baselines::CellId::new(scenario, fixture),
                metrics: metrics.clone(),
            },
            &baseline_with(scenario, fixture, None),
            class,
            &crate::baselines::HeadroomTable::new(),
        )
    }

    /// A shortfall ceiling must move with the class's measured headroom, not
    /// with the compiled-in default: the two gates agreeing is the whole
    /// reason the ceiling is derived from the ratchet's policy at all.
    #[test]
    fn a_measured_headroom_resizes_the_shortfall_ceiling() {
        let file = file_from(&format!(
            "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"view_p99_ms\"
class = \"controlled-linux\"
accepted = 9.0
why = \"because\"
"
        ));
        let measured = measured_cell("echo", "minimal", &[("view_p99_ms", 10.0)]);
        let baseline = baseline_with("echo", "minimal", None);
        // default ABSOLUTE_HEADROOM 1.5 admits 13.5
        assert!(matches!(
            super::check_cell(
                &file,
                &measured,
                &baseline,
                "controlled-linux",
                &crate::baselines::HeadroomTable::new()
            )[0]
            .verdict,
            Verdict::Held { .. }
        ));

        let tight: crate::baselines::HeadroomTable =
            [("view_p99_ms".to_string(), 1.05)].into_iter().collect();
        assert!(matches!(
            super::check_cell(&file, &measured, &baseline, "controlled-linux", &tight)[0].verdict,
            Verdict::Widened { .. }
        ));
    }

    fn metrics(pairs: &[(&str, f64)]) -> CellMetrics {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect::<CellMetrics>()
    }

    fn measured_cell(
        scenario: &str,
        fixture: &str,
        pairs: &[(&str, f64)],
    ) -> crate::baselines::MeasuredCell {
        crate::baselines::MeasuredCell {
            id: crate::baselines::CellId::new(scenario, fixture),
            metrics: metrics(pairs),
        }
    }

    const ONE_BUDGET: &str = r#"
schema = 1
[[budget]]
spec_row = "row"
scenario = "echo"
metric = "view_p99_ms"
max = 8.0
"#;

    #[test]
    fn a_metric_inside_its_bound_is_inside() {
        let file = file_from(ONE_BUDGET);
        let found = check_cell(
            &file,
            "echo",
            "minimal",
            &metrics(&[("view_p99_ms", 1.0)]),
            "dev-linux",
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].verdict, Verdict::Inside);
        assert!(!found[0].verdict.is_failure());
    }

    /// The whole point of the mechanism: a budget can be missed, but not
    /// quietly. Without an entry saying so, the gate fails.
    ///
    /// Disconfirm: treating an unlisted overrun as a warning instead makes
    /// `is_failure` false and lets a metric drift outside spec forever with
    /// nothing recording that it had.
    #[test]
    fn an_unlisted_overrun_fails_the_gate() {
        let file = file_from(ONE_BUDGET);
        let found = check_cell(
            &file,
            "echo",
            "minimal",
            &metrics(&[("view_p99_ms", 9.0)]),
            "dev-linux",
        );
        assert_eq!(found[0].verdict, Verdict::New);
        assert!(found[0].verdict.is_failure());
    }

    /// The picker rows' arming guarantee: a row whose very first recording
    /// lands outside its spec budget must fail the gate, not ratchet at the
    /// recorded value and stay green forever. The ratchet cannot catch this
    /// case by construction -- recorded equals measured on a first record --
    /// so the budget gate is the only thing standing between "40 ms recorded"
    /// and "2.5x over the 16 ms bar, never reported". Checked against the
    /// shipped budgets.toml so the test binds the real picker bounds, with
    /// the recorded baseline holding exactly the measured values.
    #[test]
    fn a_first_picker_recording_over_budget_fails_despite_a_green_ratchet() {
        let path = crate::fixture::workspace_root()
            .join("crates")
            .join("view-bench")
            .join("budgets.toml");
        let file = load(&path).expect("the shipped budgets.toml must load");
        let over = &[
            ("match_paint_p99_ms", 40.0),
            ("match_paint_p50_ms", 30.0),
            ("first_page_p99_ms", 250.0),
            ("first_page_p50_ms", 200.0),
        ];
        let measured = measured_cell("picker", "minimal", over);
        // recorded == measured: the state a first --record leaves behind,
        // which every recorded-bar comparison passes by construction
        let baseline = baseline_with("picker", "minimal", Some(&metrics(over)));
        let findings = super::check_cell(
            &file,
            &measured,
            &baseline,
            "controlled-linux",
            &crate::baselines::HeadroomTable::new(),
        );
        let failed: Vec<&str> = findings
            .iter()
            .filter(|finding| finding.verdict.is_failure())
            .map(|finding| finding.metric.as_str())
            .collect();
        assert_eq!(
            failed,
            vec!["first_page_p99_ms", "match_paint_p99_ms"],
            "both picker spec bounds must fail a first recording outside them"
        );
        for finding in &findings {
            assert_eq!(finding.verdict, Verdict::New, "{finding:?}");
        }
    }

    /// Inside the bounds, the same first-recording shape passes: the picker
    /// budgets bind the spec numbers (16 ms match, 100 ms first page), not
    /// whatever the first record happened to say.
    #[test]
    fn a_first_picker_recording_inside_budget_passes() {
        let path = crate::fixture::workspace_root()
            .join("crates")
            .join("view-bench")
            .join("budgets.toml");
        let file = load(&path).expect("the shipped budgets.toml must load");
        let inside = &[("match_paint_p99_ms", 15.9), ("first_page_p99_ms", 99.9)];
        let measured = measured_cell("picker", "minimal", inside);
        let baseline = baseline_with("picker", "minimal", Some(&metrics(inside)));
        let findings = super::check_cell(
            &file,
            &measured,
            &baseline,
            "controlled-linux",
            &crate::baselines::HeadroomTable::new(),
        );
        assert_eq!(findings.len(), 2);
        for finding in &findings {
            assert_eq!(finding.verdict, Verdict::Inside, "{finding:?}");
        }
    }

    /// The row this mechanism exists for, in its real proportions:
    /// `first_paint.minimal` `marker_cold_ms` on dev-linux records 25.151 ms
    /// against a 30 ms bound while the class's sidecar puts that statistic's
    /// spread at x2.0. A quiet run sits 19% under a bound the host's own
    /// noise clears several times over, and the row is one-shot -- no
    /// median-of-trials stands between an ambient spike and the verdict.
    const COLD_START_BUDGET: &str = r#"
schema = 1
[[budget]]
spec_row = "row"
scenario = "first_paint"
metric = "marker_cold_ms"
max = 30.0
"#;

    const COLD_START_RECORDED: f64 = 25.151;

    /// The dev-linux sidecar's own entry for the row above.
    fn characterized() -> crate::baselines::HeadroomTable {
        [("first_paint.marker_cold_ms".to_string(), 2.0)]
            .into_iter()
            .collect()
    }

    fn cold_start(
        budgets: &str,
        measured: f64,
        recorded: Option<f64>,
        table: &crate::baselines::HeadroomTable,
    ) -> Verdict {
        let file = file_from(budgets);
        let recorded = recorded.map(|value| metrics(&[("marker_cold_ms", value)]));
        super::check_cell(
            &file,
            &measured_cell("first_paint", "minimal", &[("marker_cold_ms", measured)]),
            &baseline_with("first_paint", "minimal", recorded.as_ref()),
            "dev-linux",
            table,
        )[0]
        .verdict
        .clone()
    }

    /// The tolerance must survive the run-level walk the gate actually
    /// calls, reading the recorded value out of the baseline under each
    /// cell's own id. This is the wiring no per-cell test can see: hand the
    /// walk a baseline and it must still reach `Excursion`, and it must
    /// size the ceiling from the cell it is checking rather than from a
    /// sibling that records a different value for the same metric.
    ///
    /// Disconfirm: a walk that stops consulting the baseline returns `New`
    /// for the first cell; one that reads any fixed or wrong cell's metrics
    /// gives `minimal` the heavy fixture's 40.0 recorded value, and the
    /// asserted `recorded`/`ceiling` pair fails.
    #[test]
    fn the_run_walk_reaches_an_excursion_with_each_cell_own_recorded_value() {
        let file = file_from(COLD_START_BUDGET);
        let mut baseline = crate::baselines::BaselineFile::new("dev-linux", "v0");
        baseline.cells.insert(
            "first_paint".to_string(),
            [
                (
                    "minimal".to_string(),
                    metrics(&[("marker_cold_ms", COLD_START_RECORDED)]),
                ),
                ("heavy".to_string(), metrics(&[("marker_cold_ms", 20.0)])),
            ]
            .into_iter()
            .collect(),
        );
        let measured = [
            measured_cell("first_paint", "minimal", &[("marker_cold_ms", 34.0)]),
            measured_cell("first_paint", "heavy", &[("marker_cold_ms", 34.0)]),
        ];

        let findings = check_run(&file, &baseline, &measured, "dev-linux", &characterized());
        assert_eq!(findings.len(), 2);
        assert_eq!(
            findings[0].verdict,
            Verdict::Excursion {
                recorded: COLD_START_RECORDED,
                ceiling: COLD_START_RECORDED * 2.0,
                headroom: crate::baselines::Headroom::Proportional(2.0),
            },
            "the minimal cell must be sized by its own recorded 25.151"
        );
        assert_eq!(
            findings[1].verdict,
            Verdict::Excursion {
                recorded: 20.0,
                ceiling: 40.0,
                headroom: crate::baselines::Headroom::Proportional(2.0),
            },
            "the heavy cell must be sized by its own recorded 20.0"
        );
    }

    /// A signed metric's allowance carries an absolute floor sized for a
    /// paired delta's jitter around a recorded value. Against a spec bound
    /// that floor is a fixed band no published factor can shrink, so the
    /// shape is refused outright and the strict rule stands.
    ///
    /// Disconfirm: without the refusal this returns `Excursion` with a
    /// ceiling of 0.35 against a 0.10 bound -- a 3.5x tolerance bought by a
    /// x1.01 measured spread.
    #[test]
    fn a_signed_metric_earns_no_excursion_however_tight_its_published_spread() {
        let file = file_from(
            r#"
schema = 1
[[budget]]
spec_row = "row"
scenario = "echo"
metric = "paired_delta_p99_ms"
max = 0.10
"#,
        );
        let table: crate::baselines::HeadroomTable = [("paired_delta_p99_ms".to_string(), 1.01)]
            .into_iter()
            .collect();
        let baseline = baseline_with(
            "echo",
            "minimal",
            Some(&metrics(&[("paired_delta_p99_ms", 0.10)])),
        );
        let findings = super::check_cell(
            &file,
            &measured_cell("echo", "minimal", &[("paired_delta_p99_ms", 0.12)]),
            &baseline,
            "controlled-linux",
            &table,
        );
        assert_eq!(findings[0].verdict, Verdict::New);
    }

    /// A characterized class does not loosen the bound itself: a reading
    /// inside it is inside it, with nothing tolerated and nothing reported.
    #[test]
    fn a_reading_inside_its_bound_is_inside_on_a_characterized_class() {
        assert_eq!(
            cold_start(
                COLD_START_BUDGET,
                29.9,
                Some(COLD_START_RECORDED),
                &characterized()
            ),
            Verdict::Inside
        );
    }

    /// Host noise cannot turn a quiet-run-compliant metric into a hard
    /// failure on a class that has measured how far the statistic moves --
    /// and cannot buy silence either: the reading is reported every run.
    ///
    /// Disconfirm: without the ceiling this is an unbounded amnesty, so the
    /// far side is asserted too. The ceiling is the recorded value under the
    /// class's published spread, which is the same bar the regression
    /// ratchet holds that recorded value to, so the two gates fail at one
    /// number instead of one firing on noise the other absorbs.
    #[test]
    fn host_noise_past_a_bound_a_quiet_run_meets_is_tolerated_and_reported() {
        let ceiling = COLD_START_RECORDED * 2.0;
        let verdict = |measured| {
            cold_start(
                COLD_START_BUDGET,
                measured,
                Some(COLD_START_RECORDED),
                &characterized(),
            )
        };

        let tolerated = verdict(34.0);
        assert_eq!(
            tolerated,
            Verdict::Excursion {
                recorded: COLD_START_RECORDED,
                ceiling,
                headroom: crate::baselines::Headroom::Proportional(2.0),
            }
        );
        assert!(!tolerated.is_failure());
        assert!(matches!(verdict(ceiling), Verdict::Excursion { .. }));

        let past = verdict(ceiling * 1.001);
        assert_eq!(past, Verdict::New);
        assert!(past.is_failure());
    }

    /// A default allowance is a guess about a host nobody characterized.
    /// Letting one earn the tolerance would widen every spec bound on every
    /// class by 50% at a stroke, so a class with no published spread for the
    /// statistic keeps the strict rule exactly.
    #[test]
    fn an_uncharacterized_class_keeps_the_strict_rule() {
        assert_eq!(
            cold_start(
                COLD_START_BUDGET,
                34.0,
                Some(COLD_START_RECORDED),
                &crate::baselines::HeadroomTable::new()
            ),
            Verdict::New
        );
    }

    /// The tolerance is measured against the quiet run, so it needs one. A
    /// cell the baseline holds no value for has nothing saying the bound was
    /// ever met, and a recorded value already outside the bound is a
    /// standing gap whose mechanism is a written `[[shortfall]]` -- never a
    /// noise allowance that would absorb it unstated.
    #[test]
    fn an_excursion_needs_a_recorded_value_that_is_itself_inside_the_bound() {
        assert_eq!(
            cold_start(COLD_START_BUDGET, 34.0, None, &characterized()),
            Verdict::New
        );
        assert_eq!(
            cold_start(COLD_START_BUDGET, 34.0, Some(30.1), &characterized()),
            Verdict::New
        );
    }

    /// A tolerated reading must never read as a clean pass: the report line
    /// says the value is past spec, names the ceiling that admitted it, and
    /// names the recorded value that ceiling comes from.
    #[test]
    fn an_excursion_reports_as_past_spec_not_as_a_pass() {
        let line = Finding {
            scenario: "first_paint".to_string(),
            fixture: "minimal".to_string(),
            metric: "marker_cold_ms".to_string(),
            measured: 34.0,
            budget: 30.0,
            spec_row: "row".to_string(),
            verdict: Verdict::Excursion {
                recorded: COLD_START_RECORDED,
                ceiling: COLD_START_RECORDED * 2.0,
                headroom: crate::baselines::Headroom::Proportional(2.0),
            },
        }
        .to_string();
        assert!(line.starts_with("BUDGET EXCURSION"), "{line}");
        assert!(line.contains("past spec 30.000"), "{line}");
        assert!(
            line.contains("x headroom 2"),
            "the line must name the factor that produced the ceiling: {line}"
        );
        assert!(line.contains("50.302"), "{line}");
        assert!(line.contains("25.151"), "{line}");
        assert!(!line.contains("budget OK"), "{line}");
    }

    /// Where a shortfall exists the shortfall decides, even when the cell
    /// would otherwise qualify for the noise tolerance. Otherwise a written
    /// gap could be reported as weather, and the stale-shortfall sweep --
    /// which reads the verdicts -- would stop seeing the entry it is meant
    /// to retire.
    #[test]
    fn a_listed_shortfall_still_decides_where_an_excursion_would_apply() {
        let with_shortfall = format!(
            "{COLD_START_BUDGET}
[[shortfall]]
scenario = \"first_paint\"
fixture = \"minimal\"
metric = \"marker_cold_ms\"
class = \"dev-linux\"
accepted = 40.0
why = \"because\"
"
        );
        assert!(matches!(
            cold_start(
                &with_shortfall,
                34.0,
                Some(COLD_START_RECORDED),
                &characterized()
            ),
            Verdict::Held { .. }
        ));
    }

    #[test]
    fn a_listed_shortfall_that_holds_or_improves_passes_and_still_reports() {
        let file = file_from(&format!(
            "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"view_p99_ms\"
class = \"dev-linux\"
accepted = 9.0
why = \"because\"
"
        ));
        for measured in [9.0, 8.5] {
            let found = check_cell(
                &file,
                "echo",
                "minimal",
                &metrics(&[("view_p99_ms", measured)]),
                "dev-linux",
            );
            assert!(!found[0].verdict.is_failure(), "{measured} must not fail");
            assert!(matches!(found[0].verdict, Verdict::Held { .. }));
        }
    }

    /// A shortfall is a ceiling of its own, not an amnesty. Letting it
    /// widen without limit would make the entry a licence to keep getting
    /// worse, which is exactly the state the baseline ratchet exists to
    /// prevent one level down.
    ///
    /// The ceiling is that same ratchet's, not the accepted value itself.
    /// `view_p99_ms` is an absolute metric, so it earns
    /// [`crate::baselines::ABSOLUTE_HEADROOM`]: 9.0 accepted admits 13.5.
    ///
    /// Disconfirm: comparing the next sample to `accepted` exactly is what
    /// this replaced, and it failed on the first re-run of a freshly
    /// written ledger over a 0.35% difference. Both directions are asserted
    /// here, because a ceiling that only ever passes is not a gate.
    #[test]
    fn a_listed_shortfall_widens_only_past_the_ratchet_it_shares() {
        let file = file_from(&format!(
            "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"view_p99_ms\"
class = \"controlled-linux\"
accepted = 9.0
why = \"because\"
"
        ));
        let verdict = |measured| {
            check_cell(
                &file,
                "echo",
                "minimal",
                &metrics(&[("view_p99_ms", measured)]),
                "controlled-linux",
            )[0]
            .verdict
            .clone()
        };

        // noise past the accepted value is absorbed, not failed
        assert!(matches!(verdict(9.01), Verdict::Held { .. }));
        assert!(matches!(verdict(13.5), Verdict::Held { .. }));

        let widened = verdict(13.51);
        assert_eq!(
            widened,
            Verdict::Widened {
                accepted: 9.0,
                ceiling: 13.5,
                headroom: crate::baselines::Headroom::Proportional(
                    crate::baselines::ABSOLUTE_HEADROOM
                ),
            }
        );
        assert!(widened.is_failure());
    }

    /// A ratio takes the tighter of the two headrooms, so the same accepted
    /// value buys a different ceiling depending on what kind of number it
    /// is. Without this the ratio metrics that carry most of the shortfall
    /// ledger would inherit the absolute metric's wider band.
    #[test]
    fn a_ratio_shortfall_takes_the_ratio_headroom() {
        let file = file_from(
            r#"
schema = 1
[[budget]]
spec_row = "row"
scenario = "echo"
metric = "ratio_p50"
max = 1.1
[[shortfall]]
scenario = "echo"
fixture = "minimal"
metric = "ratio_p50"
class = "dev-linux"
accepted = 1.2
why = "because"
"#,
        );
        let verdict = |measured| {
            check_cell(
                &file,
                "echo",
                "minimal",
                &metrics(&[("ratio_p50", measured)]),
                "dev-linux",
            )[0]
            .verdict
            .clone()
        };
        assert!(matches!(verdict(1.5), Verdict::Held { .. }));
        assert!(matches!(verdict(1.51), Verdict::Widened { .. }));
    }

    /// A tail statistic the class does not gate is not gated here either.
    /// `ratio_p99` on a shared class tracks ambient load by +/-50%, which is
    /// why the baseline ratchet exempts it; enforcing a shortfall ceiling on
    /// it would reintroduce that noise as a build failure through the other
    /// door.
    #[test]
    fn a_shortfall_on_a_metric_the_class_does_not_gate_has_no_ceiling() {
        let file = file_from(
            r#"
schema = 1
[[budget]]
spec_row = "row"
scenario = "echo"
metric = "ratio_p99"
max = 1.1
[[shortfall]]
scenario = "echo"
fixture = "minimal"
metric = "ratio_p99"
class = "dev-linux"
accepted = 1.2
why = "because"
[[shortfall]]
scenario = "echo"
fixture = "minimal"
metric = "ratio_p99"
class = "controlled-linux"
accepted = 1.2
why = "the same entry on a class that gates this statistic"
"#,
        );
        let found = check_cell(
            &file,
            "echo",
            "minimal",
            &metrics(&[("ratio_p99", 99.0)]),
            "dev-linux",
        );
        assert!(matches!(found[0].verdict, Verdict::Held { .. }));

        // and the same metric on a controlled class does have one
        let gated = check_cell(
            &file,
            "echo",
            "minimal",
            &metrics(&[("ratio_p99", 99.0)]),
            "controlled-linux",
        );
        assert!(matches!(gated[0].verdict, Verdict::Widened { .. }));
    }

    /// A shortfall is keyed on the fixture too: `echo.heavy` missing its
    /// bound says nothing about `echo.minimal`, and one entry must never
    /// cover for the other.
    #[test]
    fn a_shortfall_does_not_cover_a_different_fixture() {
        let file = file_from(&format!(
            "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"view_p99_ms\"
class = \"dev-linux\"
accepted = 9.0
why = \"because\"
"
        ));
        let found = check_cell(
            &file,
            "echo",
            "heavy",
            &metrics(&[("view_p99_ms", 9.0)]),
            "dev-linux",
        );
        assert_eq!(found[0].verdict, Verdict::New);
    }

    #[test]
    fn a_budget_scoped_to_one_class_does_not_bind_another() {
        let file = file_from(
            r#"
schema = 1
[[budget]]
spec_row = "row"
scenario = "input_path"
metric = "key_to_rpc_p99_us"
max = 232.0
classes = ["dev-linux"]
"#,
        );
        let over = metrics(&[("key_to_rpc_p99_us", 400.0)]);
        assert_eq!(
            check_cell(&file, "input_path", "minimal", &over, "dev-linux")[0].verdict,
            Verdict::New
        );
        assert!(
            check_cell(&file, "input_path", "minimal", &over, "dev-macos").is_empty(),
            "a class the budget does not name has no bound to breach"
        );
    }

    #[test]
    fn a_metric_with_no_budget_produces_no_finding() {
        let file = file_from(ONE_BUDGET);
        assert!(check_cell(
            &file,
            "first_paint",
            "minimal",
            &metrics(&[("marker_cold_ms", 999.0)]),
            "dev-linux"
        )
        .is_empty());
    }

    /// A shortfall naming a budget that does not exist would read as an
    /// accepted gap while binding nothing, so the file refuses to load. The
    /// metric it names is real; what is missing is the bound.
    #[test]
    fn a_shortfall_against_no_budget_is_a_load_error() {
        let text = format!(
            "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"ratio_p50\"
class = \"dev-linux\"
accepted = 9.0
why = \"because\"
"
        );
        assert!(matches!(
            parse(&text, "test"),
            Err(BudgetError::OrphanShortfall { .. })
        ));
    }

    /// A budget on a metric no row produces is compared with nothing and
    /// reports nothing: the spec row it quotes stops being enforced without
    /// anything failing. A typo is the usual way in, and the declared
    /// vocabulary is the only thing that can tell one from a real name.
    #[test]
    fn a_budget_on_a_metric_no_row_produces_is_a_load_error() {
        let text = "
schema = 1
[[budget]]
spec_row = \"row\"
scenario = \"echo\"
metric = \"veiw_p99_ms\"
max = 8.0
";
        assert!(
            matches!(
                parse(text, "test"),
                Err(BudgetError::UnknownMetric {
                    table: "budget",
                    ref metric,
                    ..
                }) if metric == "veiw_p99_ms"
            ),
            "a transposed metric name must refuse the file, got {:?}",
            parse(text, "test")
        );

        let shortfall = format!(
            "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"veiw_p99_ms\"
class = \"dev-linux\"
accepted = 9.0
why = \"because\"
"
        );
        assert!(matches!(
            parse(&shortfall, "test"),
            Err(BudgetError::UnknownMetric {
                table: "shortfall",
                ..
            })
        ));
    }

    #[test]
    fn a_shortfall_measured_provably_inside_is_reported_stale() {
        let file = file_from(&format!(
            "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"view_p99_ms\"
class = \"controlled-linux\"
accepted = 9.0
why = \"because\"
"
        ));
        let none = crate::baselines::HeadroomTable::new();
        // budget max 8.0; default ABSOLUTE_HEADROOM 1.5 puts a 1.0 reading
        // inside even at its worst draw (1.5 <= 8.0), so the entry is spent
        let fixed = check_cell(
            &file,
            "echo",
            "minimal",
            &metrics(&[("view_p99_ms", 1.0)]),
            "controlled-linux",
        );
        let unreached = unreached_shortfalls(&file, "controlled-linux", &fixed, &none);
        assert_eq!(unreached.len(), 1, "a fixed shortfall's entry is now stale");

        let still_short = check_cell(
            &file,
            "echo",
            "minimal",
            &metrics(&[("view_p99_ms", 9.0)]),
            "controlled-linux",
        );
        assert!(unreached_shortfalls(&file, "controlled-linux", &still_short, &none).is_empty());
    }

    /// A statistic whose honest draws straddle its bound must not spend its
    /// shortfall entry on one low draw: with the entry gone, the next high
    /// draw fails as a new shortfall, and no ledger state passes both.
    #[test]
    fn an_inside_reading_within_the_spread_does_not_spend_the_shortfall() {
        let file = file_from(&format!(
            "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"view_p99_ms\"
class = \"controlled-linux\"
accepted = 9.0
why = \"because\"
"
        ));
        // budget max 8.0; default ABSOLUTE_HEADROOM 1.5 says a 7.0 reading
        // could as honestly have drawn 10.5, so the entry is still earning
        let inside_within_spread = check_cell(
            &file,
            "echo",
            "minimal",
            &metrics(&[("view_p99_ms", 7.0)]),
            "controlled-linux",
        );
        let none = crate::baselines::HeadroomTable::new();
        assert!(
            unreached_shortfalls(&file, "controlled-linux", &inside_within_spread, &none)
                .is_empty(),
            "an inside draw within the spread keeps the entry"
        );

        // a class that resolves the statistic to 5% proves the same 7.0
        // reading inside: 7.35 <= 8.0, and the entry is spent
        let tight: crate::baselines::HeadroomTable =
            [("view_p99_ms".to_string(), 1.05)].into_iter().collect();
        assert_eq!(
            unreached_shortfalls(&file, "controlled-linux", &inside_within_spread, &tight).len(),
            1,
            "a spread the reading clears spends the entry"
        );

        // a tail statistic on a shared class publishes no spread at all, and
        // one inside reading there proves nothing either
        assert!(
            unreached_shortfalls(
                &file_from(&format!(
                    "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"view_p99_ms\"
class = \"dev-linux\"
accepted = 9.0
why = \"because\"
"
                )),
                "dev-linux",
                &check_cell(
                    &file,
                    "echo",
                    "minimal",
                    &metrics(&[("view_p99_ms", 1.0)]),
                    "dev-linux",
                ),
                &tight,
            )
            .is_empty(),
            "no published spread leaves the entry standing"
        );
    }

    /// A cell the run never measured (a platform-skipped scenario, or an
    /// invocation scoped to another cell) produces no findings, and an
    /// absent reading proves nothing about the entry either way.
    #[test]
    fn a_shortfall_whose_cell_never_ran_is_not_reported_stale() {
        let file = file_from(&format!(
            "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"view_p99_ms\"
class = \"controlled-linux\"
accepted = 9.0
why = \"because\"
"
        ));
        let none = crate::baselines::HeadroomTable::new();
        assert!(
            unreached_shortfalls(&file, "controlled-linux", &[], &none).is_empty(),
            "an unvisited cell leaves the entry standing"
        );

        let other_cell = check_cell(
            &file,
            "echo",
            "heavy",
            &metrics(&[("view_p99_ms", 1.0)]),
            "controlled-linux",
        );
        assert!(
            unreached_shortfalls(&file, "controlled-linux", &other_cell, &none).is_empty(),
            "a reading from a different fixture is not this cell's"
        );
    }

    /// A bound whose metric is declared but whose scenario never produces it
    /// is invisible to every other check: it parses, it names a real metric,
    /// and it matches nothing, so the run reports exactly what a scenario
    /// inside its budget reports. Only the measured cells can tell them
    /// apart.
    #[test]
    fn a_budget_no_row_ever_matches_is_reported_once_the_matrix_has_run() {
        let file = file_from(
            r#"
schema = 1
[[budget]]
spec_row = "row"
scenario = "output_path"
metric = "p99_ms"
max = 1.0
[[budget]]
spec_row = "row"
scenario = "echo"
metric = "pss_mb"
max = 150.0
"#,
        );
        let measured = vec![
            measured_cell("output_path", "minimal", &[("p99_ms", 0.5)]),
            measured_cell("echo", "minimal", &[("ratio_p50", 1.0)]),
        ];
        let unreached = unreached_budgets(&file, "dev-linux", &measured);
        assert_eq!(
            unreached
                .iter()
                .map(|budget| (budget.scenario.as_str(), budget.metric.as_str()))
                .collect::<Vec<_>>(),
            vec![("echo", "pss_mb")],
            "the bound the run never matched is the one to report"
        );
    }

    /// The metric a dead bound names can be entirely real and produced on
    /// every run -- by a different row. Matching the name alone would read
    /// that as reached, and a bound retired by moving it onto a metric its
    /// own scenario never measures takes exactly that shape.
    #[test]
    fn a_budget_whose_metric_only_another_scenario_produces_is_reported() {
        let file = file_from(
            r#"
schema = 1
[[budget]]
spec_row = "row"
scenario = "output_path"
metric = "pss_mb"
max = 150.0
"#,
        );
        let measured = vec![
            measured_cell("output_path", "minimal", &[("p99_ms", 0.5)]),
            measured_cell("memory", "minimal", &[("pss_mb", 3.4)]),
        ];
        let unreached = unreached_budgets(&file, "dev-linux", &measured);
        assert_eq!(
            unreached
                .iter()
                .map(|budget| (budget.scenario.as_str(), budget.metric.as_str()))
                .collect::<Vec<_>>(),
            vec![("output_path", "pss_mb")],
            "the metric is produced this run, by another scenario, so the bound still checks \
             nothing"
        );
    }

    /// A row a platform does not run leaves its budgets unmatched for a
    /// reason that is not a dead bound, and the coverage question it raises
    /// is answered by the cell-level check rather than here.
    #[test]
    fn a_budget_whose_scenario_never_ran_is_not_reported() {
        let file = file_from(ONE_BUDGET);
        assert!(unreached_budgets(&file, "dev-linux", &[]).is_empty());
    }

    /// Class scoping is what keeps the two platform memory metrics from
    /// reporting each other as dead on every run.
    #[test]
    fn a_budget_scoped_to_another_class_is_not_reported() {
        let file = file_from(
            r#"
schema = 1
[[budget]]
spec_row = "row"
scenario = "memory"
metric = "phys_footprint_mb"
max = 150.0
classes = ["dev-macos"]
"#,
        );
        let measured = vec![measured_cell("memory", "minimal", &[("pss_mb", 3.4)])];
        assert!(unreached_budgets(&file, "dev-linux", &measured).is_empty());
        assert_eq!(unreached_budgets(&file, "dev-macos", &measured).len(), 1);
    }

    /// The shipped file is the one the gate actually reads; a typo in it
    /// would surface as a confusing gate failure rather than a load error.
    /// Loading it here holds every one of its metric names to the declared
    /// vocabulary as well, since that check is part of the load.
    #[test]
    fn the_shipped_budget_file_loads_and_binds_every_shortfall() {
        let path = crate::fixture::workspace_root()
            .join("crates")
            .join("view-bench")
            .join("budgets.toml");
        let file = load(&path).expect("the shipped budgets.toml must load");
        assert!(!file.budget.is_empty());
        for shortfall in &file.shortfall {
            assert!(
                find_budget(
                    &file,
                    &shortfall.scenario,
                    &shortfall.metric,
                    &shortfall.class
                )
                .is_some(),
                "{shortfall:?} names no budget"
            );
        }
    }

    /// The shipped budget table against every shipped baseline, which is
    /// what makes [`unreached_budgets`] more than a unit-tested function: a
    /// bound moved onto a metric its own scenario never produces is dead,
    /// and nothing else in the tree can see it. The gate that would catch it
    /// needs a full matrix run; a recorded baseline is the same
    /// scenario-to-metric map that run would produce, already committed.
    ///
    /// This reads what each class last recorded, so a row that renames a
    /// metric without the class being re-recorded shows up here as a dead
    /// bound on that class. That is the same staleness the gate's own
    /// coverage walk reports, and the file is the thing to correct.
    #[test]
    fn every_shipped_budget_is_reached_by_the_baselines_that_recorded_its_scenario() {
        let root = crate::fixture::workspace_root();
        let bench = root.join("crates").join("view-bench");
        let file = load(&bench.join("budgets.toml")).expect("the shipped budgets.toml must load");
        let dir = bench.join("baselines");
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("the baselines directory must exist")
            .map(|entry| entry.expect("readable directory entry").path())
            // a recorded baseline is exactly <class>.toml, so its stem
            // carries no dot; selecting positively keeps headroom sidecars,
            // partial-record diagnostics and any future dotted sibling out
            // without a denylist that must learn each one
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "toml")
                    && path
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(|stem| !stem.contains('.'))
            })
            .collect();
        paths.sort();
        assert!(
            !paths.is_empty(),
            "no baseline under {} would make this assert nothing",
            dir.display()
        );
        let mut dead = Vec::new();
        for path in &paths {
            let recorded = crate::baselines::load(path).expect("every shipped baseline must load");
            let measured: Vec<crate::baselines::MeasuredCell> = recorded
                .cells
                .iter()
                .flat_map(|(scenario, fixtures)| {
                    fixtures
                        .iter()
                        .map(move |(fixture, metrics)| crate::baselines::MeasuredCell {
                            id: crate::baselines::CellId::new(scenario, fixture),
                            metrics: metrics.clone(),
                        })
                })
                .collect();
            for budget in unreached_budgets(&file, &recorded.machine_class, &measured) {
                dead.push(format!(
                    "{}: [{}] {} bounds a metric that scenario did not record",
                    path.display(),
                    budget.scenario,
                    budget.metric
                ));
            }
        }
        assert!(dead.is_empty(), "dead spec bounds:\n{}", dead.join("\n"));
    }
}
