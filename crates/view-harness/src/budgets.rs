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
//! passes and is reported every run. There is no fourth state, and in
//! particular no way to be quietly outside budget.
//!
//! The ceiling is the one [`crate::baselines`] already grants the recorded
//! bar for that metric on that class, not the accepted value itself. An
//! accepted value is one sample of a noisy statistic; comparing the next
//! sample to it exactly makes every listed shortfall a coin flip rather
//! than a gate.

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
    Widened { accepted: f64, ceiling: f64 },
}

impl Verdict {
    /// Whether this verdict must fail the gate.
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
            Verdict::Widened { accepted, ceiling } => write!(
                f,
                "BUDGET FAIL [{scenario}.{fixture}] {metric}: {measured:.3} against spec \
                 {budget:.3} is past {ceiling:.3}, the ceiling the accepted shortfall \
                 {accepted:.3} earns under this class's gate headroom; a shortfall may hold or \
                 improve, never widen [{spec_row}]"
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
/// none there: on a shared host that number is not a property of the code.
fn shortfall_ceiling(
    metric: &str,
    class: &str,
    accepted: f64,
    headroom_table: &crate::baselines::HeadroomTable,
) -> f64 {
    let controlled = crate::baselines::is_controlled_class(class);
    crate::baselines::headroom_for(headroom_table, metric, controlled)
        .map_or(f64::INFINITY, |headroom| headroom.bar(accepted))
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
#[must_use]
pub fn check_cell(
    file: &BudgetFile,
    cell: &crate::baselines::MeasuredCell,
    class: &str,
    headroom_table: &crate::baselines::HeadroomTable,
) -> Vec<Finding> {
    let (scenario, fixture) = (cell.id.scenario.as_str(), cell.id.fixture.as_str());
    let mut findings = Vec::new();
    for (metric, &measured) in &cell.metrics {
        let Some(budget) = find_budget(file, scenario, metric, class) else {
            continue;
        };
        let verdict = if measured <= budget.max {
            Verdict::Inside
        } else {
            match find_shortfall(file, scenario, fixture, metric, class) {
                None => Verdict::New,
                Some(shortfall) => {
                    let ceiling =
                        shortfall_ceiling(metric, class, shortfall.accepted, headroom_table);
                    if measured > ceiling {
                        Verdict::Widened {
                            accepted: shortfall.accepted,
                            ceiling,
                        }
                    } else {
                        Verdict::Held {
                            accepted: shortfall.accepted,
                            why: shortfall.why.clone(),
                        }
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

/// Shortfalls covering `class` that the run's findings did not reach, in
/// deterministic order.
///
/// A shortfall whose metric was measured and is now inside budget has been
/// fixed and its entry is stale; one whose cell never ran this invocation is
/// simply unvisited. The caller distinguishes them by whether the run
/// claimed full coverage, exactly as the baseline gate does for cells.
#[must_use]
pub fn unreached_shortfalls<'a>(
    file: &'a BudgetFile,
    class: &str,
    findings: &[Finding],
) -> Vec<&'a Shortfall> {
    let mut unreached: Vec<&Shortfall> = file
        .shortfall
        .iter()
        .filter(|s| s.class == class)
        .filter(|s| {
            !findings.iter().any(|f| {
                f.scenario == s.scenario
                    && f.fixture == s.fixture
                    && f.metric == s.metric
                    && f.verdict != Verdict::Inside
            })
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

    /// [`super::check_cell`] with no measured per-class headroom, so every
    /// case below reads against the policy defaults. The override path has
    /// its own test rather than being threaded through all of them.
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
        // default ABSOLUTE_HEADROOM 1.5 admits 13.5
        assert!(matches!(
            super::check_cell(
                &file,
                &measured,
                "controlled-linux",
                &crate::baselines::HeadroomTable::new()
            )[0]
            .verdict,
            Verdict::Held { .. }
        ));

        let tight: crate::baselines::HeadroomTable =
            [("view_p99_ms".to_string(), 1.05)].into_iter().collect();
        assert!(matches!(
            super::check_cell(&file, &measured, "controlled-linux", &tight)[0].verdict,
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
                ceiling: 13.5
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
    fn a_shortfall_the_run_no_longer_reaches_is_reported_as_unreached() {
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
        let fixed = check_cell(
            &file,
            "echo",
            "minimal",
            &metrics(&[("view_p99_ms", 1.0)]),
            "dev-linux",
        );
        let unreached = unreached_shortfalls(&file, "dev-linux", &fixed);
        assert_eq!(unreached.len(), 1, "a fixed shortfall's entry is now stale");

        let still_short = check_cell(
            &file,
            "echo",
            "minimal",
            &metrics(&[("view_p99_ms", 9.0)]),
            "dev-linux",
        );
        assert!(unreached_shortfalls(&file, "dev-linux", &still_short).is_empty());
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
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
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
