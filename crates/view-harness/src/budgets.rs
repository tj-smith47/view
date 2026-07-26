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
//! fails; outside budget and worse than the accepted value is a widening
//! shortfall and fails; outside budget but holding or improving passes and
//! is reported every run. There is no fourth state, and in particular no
//! way to be quietly outside budget.

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
    /// Outside the bound, listed as a shortfall, and no worse than the
    /// value that shortfall was accepted at.
    Held { accepted: f64, why: String },
    /// Outside the bound with no shortfall entry: a budget the project
    /// stopped meeting without recording that it had.
    New,
    /// Outside the bound and worse than the accepted shortfall value.
    Widened { accepted: f64 },
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
            Verdict::Widened { accepted } => write!(
                f,
                "BUDGET FAIL [{scenario}.{fixture}] {metric}: {measured:.3} against spec \
                 {budget:.3} is worse than the accepted shortfall {accepted:.3}; a shortfall may \
                 hold or improve, never widen [{spec_row}]"
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

/// Checks one cell's measured metrics against the budgets that cover
/// `class`, in deterministic (metric-name) order.
///
/// Metrics with no budget produce no finding: a metric the spec states no
/// bound for is a real state, not a gap (see `budgets.toml`).
#[must_use]
pub fn check_cell(
    file: &BudgetFile,
    scenario: &str,
    fixture: &str,
    metrics: &crate::baselines::CellMetrics,
    class: &str,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for (metric, &measured) in metrics {
        let Some(budget) = find_budget(file, scenario, metric, class) else {
            continue;
        };
        let verdict = if measured <= budget.max {
            Verdict::Inside
        } else {
            match find_shortfall(file, scenario, fixture, metric, class) {
                None => Verdict::New,
                Some(shortfall) if measured > shortfall.accepted => Verdict::Widened {
                    accepted: shortfall.accepted,
                },
                Some(shortfall) => Verdict::Held {
                    accepted: shortfall.accepted,
                    why: shortfall.why.clone(),
                },
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::baselines::CellMetrics;

    fn file_from(text: &str) -> BudgetFile {
        parse(text, "test").unwrap()
    }

    fn metrics(pairs: &[(&str, f64)]) -> CellMetrics {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect::<CellMetrics>()
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
    /// widen would make the entry a licence to keep getting worse, which is
    /// exactly the state the baseline ratchet exists to prevent one level
    /// down.
    #[test]
    fn a_listed_shortfall_that_widens_fails() {
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
            "minimal",
            &metrics(&[("view_p99_ms", 9.01)]),
            "dev-linux",
        );
        assert_eq!(found[0].verdict, Verdict::Widened { accepted: 9.0 });
        assert!(found[0].verdict.is_failure());
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
    /// accepted gap while binding nothing, so the file refuses to load.
    #[test]
    fn a_shortfall_against_no_budget_is_a_load_error() {
        let text = format!(
            "{ONE_BUDGET}
[[shortfall]]
scenario = \"echo\"
fixture = \"minimal\"
metric = \"no_such_metric\"
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

    /// The shipped file is the one the gate actually reads; a typo in it
    /// would surface as a confusing gate failure rather than a load error.
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
}
