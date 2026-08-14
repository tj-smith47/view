//! The per-machine-class baseline file: recorded measurements the bench
//! gate compares against. TOML lives here (view-harness is the one
//! package sanctioned to parse it); `view-bench` itself stays serde-free
//! and only produces numbers.
//!
//! File shape (`crates/view-bench/baselines/<class>.toml`):
//!
//! ```toml
//! schema = 1
//! engine_pin = "v0.12.4"
//! machine_class = "dev-linux"
//!
//! [echo.minimal]
//! ratio_p99 = 1.21
//! paired_delta_p99_ms = 0.29
//! ```
//!
//! Measured gate headroom lives in a hand-curated sidecar next to it
//! (`<class>.headroom.toml`, see [`HeadroomTable`]), never in this file:
//! `--record` rewrites the baseline wholesale through a serializer that
//! keeps no comments, and a characterization's provenance comment is as
//! load-bearing as its factor. Keeping the two lifecycles in two files is
//! what lets every record pass leave the characterization untouched.
//!
//! Every metric is lower-is-better, so one gate rule covers all cells: a
//! breach is a measured value above the bar its recorded value implies,
//! with the bar policy chosen per metric kind and machine class by
//! [`gate_headroom`]. Most metrics are positive by construction (a
//! latency, a ratio, a size) and take proportional headroom; the paired
//! delta is a signed difference and takes [`Headroom::Signed`], which is
//! the same allowance measured off the magnitude so it does not invert
//! below zero.
//!
//! Machine classes split into two gate policies, encoded in the class
//! name itself (see [`is_controlled_class`]): a `controlled-` prefixed
//! class runs on a dedicated, load-controlled box and additionally gates
//! the paired tail statistics; every other class records them ungated.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The one schema this loader understands.
pub const SUPPORTED_SCHEMA: u32 = 1;

/// Default headroom for gated ratio metrics (view vs nvim from one
/// interleaved run), used where a class has not measured its own.
///
/// Medians are robust to ambient tail noise, and per-sample alternation
/// keeps both sides under the same ambient median shift: measured echo
/// ratio_p50 stayed inside a x1.07 band (1.51..1.62) across host-load
/// regimes whose absolute tails swung x300.
///
/// **1.25 is deliberately conservative and is not a target.** Where a
/// class has characterized the spread it should say so in its headroom
/// sidecar (see [`HeadroomTable`]) rather than inherit this:
/// dev-linux measured `ratio_p50` to a 1.70% half-width over eight
/// replicates spanning host loads 0.44 to 8.53, so 1.25 admitted a 25%
/// regression on a number that host resolves to under 2%, and it now
/// gates at 1.06 there. This value remains the floor for every metric and
/// class that has not been measured, because guessing tighter than the
/// evidence is how a gate starts failing on weather.
pub const RATIO_HEADROOM: f64 = 1.25;

/// Default headroom for absolute metrics (ms/us/MB budgets), used where a
/// class has not measured its own.
///
/// Absolute tails on a shared dev host move with ambient load, which is
/// why [`derives_from_tail`] metrics do not gate on a shared class at all;
/// what remains under this constant on such a class is the sizes, which do
/// not move with load. On a controlled class the tails gate here too.
pub const ABSOLUTE_HEADROOM: f64 = 1.5;

/// The smallest allowance [`Headroom::Signed`] grants, in the paired
/// delta's own unit (ms).
///
/// A proportional allowance shrinks to nothing as a value approaches zero,
/// which is exactly where a signed metric spends its most interesting
/// range: at a recorded delta of 0.01 ms a x1.25 factor is a 0.0025 ms
/// band, far under the round trip's own run-to-run jitter. This floor is
/// the same order as the resolution floor a paired echo round trip is
/// measured at, so it absorbs that jitter while a real slowdown of view
/// against nvim still breaches.
pub const SIGNED_DELTA_FLOOR_MS: f64 = 0.25;

/// How a metric's gated bar follows from its recorded value.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Headroom {
    /// `recorded * factor`. Valid only because these metrics are positive
    /// by construction.
    Proportional(f64),
    /// `recorded + max(|recorded| * (factor - 1), floor)`, for a metric
    /// that can legitimately be negative.
    ///
    /// A proportional factor inverts there: at a recorded -0.20 a x1.25
    /// "allowance" yields -0.25, a bar *tighter* than the value that
    /// produced it, so the very measurement just recorded breaches. Taking
    /// the allowance off the magnitude and adding it keeps the bar above
    /// the recorded value at every sign, and reduces to the proportional
    /// answer exactly whenever the recorded value is positive and large
    /// enough for the factor to clear the floor.
    Signed { factor: f64, floor: f64 },
}

impl Headroom {
    /// The bar `recorded` implies under this policy.
    #[must_use]
    pub fn bar(self, recorded: f64) -> f64 {
        match self {
            Self::Proportional(factor) => recorded * factor,
            Self::Signed { factor, floor } => {
                recorded + (recorded.abs() * (factor - 1.0)).max(floor)
            }
        }
    }

    /// Whether a metric under this policy may legitimately hold a value at
    /// or below zero, which decides how far its ratchet is allowed to fall.
    #[must_use]
    pub fn admits_non_positive(self) -> bool {
        matches!(self, Self::Signed { .. })
    }

    /// The lowest value a record may ratchet down to from `recorded` where
    /// this policy is a class's published spread for the statistic: the
    /// band that admits `bar(recorded)` of upward tolerance, mirrored
    /// downward off the same recorded value.
    ///
    /// The mirror is additive -- the floor is
    /// `recorded - (bar(recorded) - recorded)`, i.e.
    /// `2 * recorded - bar(recorded)` -- so the two policies reduce to:
    ///
    /// - [`Headroom::Proportional`]: `recorded * (2 - factor)`, which
    ///   reaches zero at a factor of 2; a spread that wide admits any draw
    ///   a positive-by-construction metric can produce, so the floor stops
    ///   binding exactly where the band stops excluding anything.
    /// - [`Headroom::Signed`]:
    ///   `recorded - max(|recorded| * (factor - 1), floor)`, the same
    ///   allowance the upward bar grants, subtracted instead of added.
    ///
    /// A value below this floor improved on the recorded one by more than
    /// the published spread says honest runs move on this class, which is
    /// the signature of a lucky draw rather than of faster code; see
    /// [`RatchetOutcome::RefusedBelowSpread`] for why such a draw must not
    /// become the bar.
    ///
    /// `recorded` is never NaN or infinite, and is non-positive only under
    /// [`Headroom::Signed`]: every value that ever reaches this cell first
    /// passed `ratchet_cell`'s own usability check (finite, and positive
    /// unless the policy admits a signed delta), so the precondition holds
    /// by construction rather than by an assertion here.
    #[must_use]
    pub fn record_floor(self, recorded: f64) -> f64 {
        2.0 * recorded - self.bar(recorded)
    }
}

impl std::fmt::Display for Headroom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proportional(factor) => write!(f, "x headroom {factor}"),
            Self::Signed { factor, floor } => {
                write!(
                    f,
                    "+ signed headroom {factor} over magnitude, floor {floor}"
                )
            }
        }
    }
}

/// The platforms a machine-class name may claim, spelled as
/// [`std::env::consts::OS`] spells them so the two can be compared
/// directly.
const PLATFORM_TOKENS: [&str; 3] = ["linux", "macos", "windows"];

/// The platform `class` claims, or `None` when it claims none or claims
/// two. The token is matched as a hyphen-delimited segment (`dev-linux`,
/// `gh-macos`, `controlled-linux-x86`), never as a substring, so a
/// machine name that merely contains a platform word cannot pass for a
/// declaration.
#[must_use]
pub fn class_platform(class: &str) -> Option<&'static str> {
    let mut claimed: Option<&'static str> = None;
    for segment in class.split('-') {
        let Some(token) = PLATFORM_TOKENS.iter().find(|token| **token == segment) else {
            continue;
        };
        if claimed.is_some_and(|first| first != *token) {
            return None;
        }
        claimed = Some(token);
    }
    claimed
}

/// Rejects running under a class whose platform is not this binary's
/// host platform.
///
/// Rows are measured per-platform under per-platform metric names, and
/// the machine class is what selects the baseline file. Both sides of
/// [`require_class_match`] are hand-supplied (the CLI argument and the
/// file's own field), so they can agree with each other while both being
/// wrong about the host; the host platform is the one value in the check
/// that cannot be typed in. Refusing here is what stops one platform's
/// recorded numbers from becoming another platform's bars.
///
/// # Errors
///
/// Returns [`BaselineError::ClassPlatformUnnamed`] when the class names
/// no single platform, and [`BaselineError::HostPlatformMismatch`] when
/// it names one this binary is not running on.
pub fn require_host_platform(class: &str) -> Result<(), BaselineError> {
    let host = std::env::consts::OS;
    let Some(named) = class_platform(class) else {
        return Err(BaselineError::ClassPlatformUnnamed {
            class: class.to_string(),
            tokens: PLATFORM_TOKENS.join(", "),
        });
    };
    if named != host {
        return Err(BaselineError::HostPlatformMismatch {
            class: class.to_string(),
            named: named.to_string(),
            host: host.to_string(),
        });
    }
    Ok(())
}

/// Whether `class` names a dedicated, load-controlled bench runner. The
/// policy lives in the class name itself (a `controlled-` prefix) rather
/// than a separate metadata field so a baseline file can never disagree
/// with its own class about which gate policy applies: there is no
/// second value to drift, hand-edit, or forget when recording a fresh
/// class.
#[must_use]
pub fn is_controlled_class(class: &str) -> bool {
    class.starts_with("controlled-")
}

/// The gate policy for one metric on one machine class: `None` means
/// recorded for reference but never gated on this mechanism.
///
/// Two families are exempt on a shared class: every statistic that
/// [`derives_from_tail`], and every [`is_cold_start_absolute`] metric,
/// whose value is set by cross-boot state a shared host cannot hold
/// fixed rather than by run-to-run scheduler noise. A size or a median is
/// neither, and gates everywhere. Ratios take [`RATIO_HEADROOM`] and
/// everything else [`ABSOLUTE_HEADROOM`], except a paired delta, which is
/// signed and so cannot take a proportional allowance at all.
///
/// The shared-class exemption is lifted per statistic, by measuring one
/// unchanged binary pair across host-load regimes. Two have been measured:
/// - `paired_delta_p99_ms` tracked ambient load x149 (0.62ms..92.5ms);
///   its regression protection is duplicated by the ratio from the same
///   paired run.
/// - `ratio_p99` has a +/-50% ambient noise floor (invocation medians
///   1.05..1.95): shared tails are scheduler-dominated, and load
///   compresses the ratio toward 1.
///
/// Neither result transfers to a quotient of two tails taken over
/// consecutive windows, which is what flood's `cadence_p99_ratio` is: the
/// interleaved pairing that lets `ratio_p99` cancel ambient load has no
/// counterpart when a spike lands inside one 15-second window and not the
/// other. Until that statistic has its own load-regime characterization it
/// is recorded and left ungated on a shared class, like every other tail.
///
/// On a dedicated runner those load regimes cannot occur, so the same
/// statistics gate there under the standard ratio headroom, and the spec
/// p99 budget gets a real bar instead of a reference number.
///
/// Lifting an exemption *for the ratchet* is a change to this function and
/// nothing else. A measured `[headroom]` entry resizes an allowance that
/// already exists and cannot restore one this returns `None` for (see
/// [`headroom_for`]), so giving a shared class an entry for
/// `cadence_p99_ratio` leaves the recorded-value ratchet ungated there
/// however well its spread has been characterized.
///
/// The exemption does not reach the budget gate, and a published factor is
/// read there whatever this function returns (see [`declared_headroom`]).
/// For a tail-derived metric that carries a spec budget, adding a sidecar
/// entry alone therefore changes a verdict with no edit here: it opens a
/// tolerated band between the bound and the recorded value under that
/// factor. The band can only loosen an existing hard failure, never create
/// one, which is why the two gates are allowed to disagree about the
/// exemption at all.
#[must_use]
pub fn gate_headroom(metric: &str, controlled: bool) -> Option<Headroom> {
    let factor = if metric.contains("ratio") {
        RATIO_HEADROOM
    } else {
        ABSOLUTE_HEADROOM
    };
    // Shape and exemption are independent questions -- what allowance the
    // value earns versus whether a shared class may consume it at all --
    // decided in that order so a metric that happened to answer both
    // exemption predicates at once still keeps the shape its own kind
    // demands rather than falling through to the wrong one.
    let shape = if metric.contains("delta") {
        // p99(view[i] - nvim[i]): negative whenever view beats nvim, which
        // is a state the paired scenario is built to reach and report
        Headroom::Signed {
            factor: RATIO_HEADROOM,
            floor: SIGNED_DELTA_FLOOR_MS,
        }
    } else {
        Headroom::Proportional(factor)
    };
    let exempt_on_shared = derives_from_tail(metric) || is_cold_start_absolute(metric);
    if !exempt_on_shared {
        return Some(shape);
    }
    controlled.then_some(shape)
}

/// The gate policy for `scenario`'s `metric`, preferring `table`'s measured
/// headroom for this class over the default for the metric's kind.
///
/// A `"scenario.metric"` entry wins over a bare `"metric"` entry, because
/// the same statistic name carries a different run-to-run spread in
/// different scenarios: on dev-macos the scroll replicates resolve the
/// bar-relevant spread of `ratio_p50` to a few percent, while the echo
/// replicates put the same name's spread an order of magnitude wider, and
/// one factor cannot be honest about both. A bare entry stays the host-wide characterization
/// for every scenario without a qualified one.
///
/// An override only resizes an allowance that already exists: a metric the
/// class does not gate at all stays ungated, because that exemption is about
/// whether the number means anything here, not about how far it moves.
#[must_use]
pub fn headroom_for(
    table: &HeadroomTable,
    scenario: &str,
    metric: &str,
    controlled: bool,
) -> Option<Headroom> {
    let policy = gate_headroom(metric, controlled)?;
    Some(declared_factor(table, scenario, metric).map_or(policy, |factor| resized(policy, factor)))
}

/// The factor `table` states for `scenario`'s `metric`, with a
/// `"scenario.metric"` entry winning over a bare `"metric"` one.
fn declared_factor(table: &HeadroomTable, scenario: &str, metric: &str) -> Option<f64> {
    table
        .get(&format!("{scenario}.{metric}"))
        .or_else(|| table.get(metric))
        .copied()
}

/// `policy` carrying `factor` instead of its own, keeping the shape the
/// metric's kind demands.
fn resized(policy: Headroom, factor: f64) -> Headroom {
    match policy {
        Headroom::Signed { floor, .. } => Headroom::Signed { factor, floor },
        Headroom::Proportional(_) => Headroom::Proportional(factor),
    }
}

/// The run-to-run spread this class has published for `scenario`'s
/// `metric`, or `None` where it has published none.
///
/// This answers a different question than [`headroom_for`], and the two are
/// not interchangeable:
///
/// - [`headroom_for`] answers "what bar does a recorded value earn here",
///   so it falls back to the conservative compiled-in default and honours
///   the shared-class tail exemption. Every gate that ratchets a recorded
///   measurement must use it.
/// - This answers "how far is this number known to move on this host",
///   which only a measurement can say. A default is a guess, so absence is
///   reported as absence rather than as a plausible number.
///
/// The shared-class tail exemption is deliberately not applied, and the
/// warrant for that is narrow. It is *not* that the value returned here is
/// somehow independent of a recorded measurement -- the one caller does
/// build its ceiling as `bar(recorded)`, on exactly the load-dependent
/// recorded value the exemption distrusts. It is that the caller uses that
/// ceiling only to *withdraw* a failure it would otherwise report: entry is
/// conditional on the measurement already being outside a bound that does
/// not come from any measurement, and anything past the ceiling returns the
/// same verdict it had before. A mis-sized ceiling can therefore only
/// mis-size a tolerance, never manufacture a breach, and its worst case is
/// bounded by the bound itself times the factor.
///
/// That is a property of the caller, not of this function, so any future
/// caller must re-establish it. Building a hard bar -- anything that can
/// turn a pass into a failure -- out of a shared-class tail on the strength
/// of this value is what the exemption exists to forbid, and nothing here
/// licenses it.
///
/// The ratchet's below-spread guard consumes this value under a different
/// warrant, established the same way: it acts only in the *improving*
/// direction, refusing to move a recorded bar further below itself than the
/// published band reaches ([`Headroom::record_floor`]). A mis-sized spread
/// there can only mis-place the refusal floor -- at worst demanding a
/// replicate campaign for an honest improvement, which is already the
/// documented practice on a wide cell -- and can never turn any measurement
/// into a regression verdict.
///
/// The returned policy carries the metric's own shape, so a signed paired
/// delta still gets its floor rather than a proportional allowance that
/// would invert below zero.
#[must_use]
pub fn declared_headroom(table: &HeadroomTable, scenario: &str, metric: &str) -> Option<Headroom> {
    let factor = declared_factor(table, scenario, metric)?;
    Some(resized(gate_headroom(metric, true)?, factor))
}

/// Whether `metric` names a statistic whose value is a function of a tail
/// percentile -- the percentile itself, a ratio of two, or a percentile of
/// a paired difference.
///
/// A percentile's value is set by the worst samples in the run, and on a
/// shared host the worst samples are the ones a foreign process preempted.
/// Measured on this class with one unchanged binary pair: `view_p99_ms`
/// spans 0.925ms to 6.676ms across host loads 0.44 to 8.53, a 7.4x range,
/// while the `ratio_p50` from those same eight runs stays inside 1.70%. No
/// fixed allowance can tell a 7x regression from a busy afternoon, so such
/// a statistic is recorded on a shared class and gated on a controlled one.
///
/// The regression protection is not lost: a real slowdown in view's own
/// tail moves the paired ratio from the same interleaved run, and that does
/// gate.
///
/// Matched on name components rather than on a suffix, because word order
/// is not a property of a statistic. `ratio_p99` and `cadence_p99_ratio`
/// are both quotients of tails and carry the same ambient noise, so a
/// suffix rule that separates them makes spelling into gate policy: a
/// consistency rename would flip a class's gate with no test failing.
///
/// A component rather than a substring, and any percentile at p99 or
/// deeper: `p999_ms` is a further-out tail than `p99_ms` and so is more
/// scheduler-dominated, not less, while `warmup99_ms` holds the letters of
/// a percentile without being one, and a substring rule would exempt it
/// from gating on the strength of a coincidence.
fn derives_from_tail(metric: &str) -> bool {
    metric
        .split('_')
        .any(|component| component.starts_with("p99"))
}

/// Whether `metric` names a cold-start absolute -- a p99 taken over cold
/// process starts rather than over warm samples within a run.
///
/// A cold-start absolute is set by state a process boot carries across
/// process starts: page cache occupancy, dyld/inode cache warmth, the
/// power and thermal state at the moment each spawn begins. A shared host
/// cannot hold any of that fixed between runs any more than it can hold a
/// scheduler's queue depth fixed for a tail percentile, and the evidence
/// is the same shape: `shell_visible` moved 6x cross-boot on gh-linux with
/// no change to the code it measured. It is recorded on a shared class
/// and gated on a controlled one, like a tail.
///
/// Matched on a name component rather than a substring, for the same
/// reason [`derives_from_tail`] is: `coldstart_ms` holds the letters of
/// "cold" without naming the cross-boot state this predicate exempts, and
/// a substring rule would exempt it on the strength of a coincidence.
fn is_cold_start_absolute(metric: &str) -> bool {
    metric.split('_').any(|component| component == "cold")
}

/// Metric values for one `[scenario.fixture]` cell.
pub type CellMetrics = BTreeMap<String, f64>;

/// Every metric name a row may record, and so the vocabulary
/// [`gate_headroom`] is proven exhaustive over.
///
/// A classification rule reads name components, which means a name nobody
/// classified still gets a policy -- silently, and usually the wrong one,
/// since the fall-through arm gates on every class. Declaring the vocabulary
/// makes that impossible to reach by accident: a row producing a name absent
/// from this list is refused at the moment it produces it, before anything
/// is recorded, and the per-metric policy table in this module's tests is
/// checked against this list rather than hand-kept beside it.
pub const RECORDED_METRICS: [&str; 26] = [
    "ratio_p50",
    "ratio_p99",
    "paired_delta_p99_ms",
    "view_p99_ms",
    "staleness_p99_ms",
    "shell_visible_cold_ms",
    "marker_cold_ms",
    "marker_ratio_p50",
    "marker_ratio_p99",
    "pss_mb",
    "phys_footprint_mb",
    "control_ratio_p50",
    "control_ratio_p99",
    "control_delta_p99_ms",
    "control_p99_ms",
    "pace_ratio",
    "cadence_p99_ms",
    "cadence_p99_ratio",
    "key_to_rpc_p99_us",
    "p99_ms",
    "match_paint_p50_ms",
    "match_paint_p99_ms",
    "first_page_p50_ms",
    "first_page_p99_ms",
    "wedge_detect_p99_ms",
    "restart_rehydrate_p99_ms",
];

/// Metric names in `measured` that [`RECORDED_METRICS`] does not declare,
/// in deterministic (sorted) order.
#[must_use]
pub fn undeclared_metrics(measured: &CellMetrics) -> Vec<String> {
    measured
        .keys()
        .filter(|metric| !RECORDED_METRICS.contains(&metric.as_str()))
        .cloned()
        .collect()
}

/// The two names that identify one matrix cell.
///
/// Both are `String` and both name the cell, so as a tuple they can be
/// written in one order and read back in the other with nothing in between
/// able to notice: no scenario name is checked against a vocabulary at
/// record time, so an inverted pair writes `[minimal.flood]` where
/// `[flood.minimal]` belongs and the run that wrote it reports success.
/// The next gate is loud about it, one full bench run later. Named fields
/// take the order out of it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellId {
    /// The row's scenario name, the baseline's outer table key.
    pub scenario: String,
    /// The fixture the row ran against, the inner table key.
    pub fixture: String,
}

impl CellId {
    /// A cell identified by borrowed names.
    #[must_use]
    pub fn new(scenario: &str, fixture: &str) -> Self {
        Self {
            scenario: scenario.to_string(),
            fixture: fixture.to_string(),
        }
    }
}

/// One measured cell as the record path carries it: which cell it is, and
/// the metrics produced for it.
#[derive(Debug, Clone)]
pub struct MeasuredCell {
    /// Which cell of the matrix produced these metrics.
    pub id: CellId,
    /// Every metric that cell produced this run.
    pub metrics: CellMetrics,
}

/// Errors loading, saving, or gating against a baseline file.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BaselineError {
    #[error(
        "{path}: [headroom] names {metric}, which no recorded cell measures; a headroom entry \
         binds nothing unless its metric exists"
    )]
    UnknownHeadroomMetric { path: String, metric: String },
    #[error(
        "{path}: carries a [headroom] table, but measured characterization lives in the \
         hand-curated sidecar {sidecar}; --record rewrites this file through a serializer that \
         keeps no comments, so a table here would lose its provenance on the next record"
    )]
    HeadroomInBaseline { path: String, sidecar: String },
    #[error(
        "{path}: [headroom] gives {metric} a factor of {factor}; a gate allowance must be finite \
         and above 1.0, since at or below it the recorded measurement breaches its own bar"
    )]
    UnusableHeadroom {
        path: String,
        metric: String,
        factor: f64,
    },
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: String,
        source: Box<toml::de::Error>,
    },
    #[error("serializing baseline: {source}")]
    Serialize { source: toml::ser::Error },
    #[error("{path} declares schema {found}; this harness supports schema {SUPPORTED_SCHEMA}")]
    UnsupportedSchema { path: String, found: u32 },
    #[error(
        "baseline {path} was recorded against engine pin {recorded:?} but the current pin is \
         {current:?}; re-record the baseline before gating or recording single cells"
    )]
    PinDrift {
        path: String,
        recorded: String,
        current: String,
    },
    #[error(
        "{path} declares machine_class {recorded:?} but this run named class {current:?}; the \
         gate policy is derived from the class, so the two must agree"
    )]
    ClassMismatch {
        path: String,
        recorded: String,
        current: String,
    },
    #[error(
        "machine class {class:?} names no platform; a class name must carry exactly one of \
         [{tokens}] as a hyphen-delimited segment, because baselines are per-platform and a \
         class that will not say which platform it belongs to cannot be checked against the host"
    )]
    ClassPlatformUnnamed { class: String, tokens: String },
    #[error(
        "machine class {class:?} names platform {named:?} but this binary runs on {host:?}; rows \
         are measured per-platform under per-platform metric names, so one platform's recorded \
         numbers are not another's bars"
    )]
    HostPlatformMismatch {
        class: String,
        named: String,
        host: String,
    },
    #[error("baseline {path} has no [{scenario}.{fixture}] cell to gate against")]
    MissingCell {
        path: String,
        scenario: String,
        fixture: String,
    },
}

/// Per-metric gate headroom measured on one class, overriding the policy
/// default from [`gate_headroom`].
///
/// A default is a guess about how much a number moves between runs on a
/// host nobody has characterized. An entry here is that number, measured.
/// Absence is therefore meaningful and is not a gap to be filled with a
/// plausible value: it says this metric's spread on this class has not been
/// established, so it gates on the conservative default until it has.
///
/// Loaded from the class's hand-curated sidecar
/// (`baselines/<class>.headroom.toml`, see [`headroom_path`]) via
/// [`load_headroom`], never from the recorded baseline itself: a
/// characterization has a different lifecycle than a recorded measurement,
/// and [`load`] refuses a baseline that carries one. The sidecar's shape:
///
/// ```toml
/// machine_class = "dev-linux"
///
/// [headroom]
/// ratio_p50 = 1.06
/// "scroll.ratio_p50" = 1.12
/// ```
///
/// A bare key characterizes the statistic host-wide; a quoted
/// `"scenario.metric"` key scopes it to one scenario and wins there (see
/// [`headroom_for`]). The quotes are TOML syntax, not decoration: unquoted,
/// the dot would open a nested table and the file would fail to load.
pub type HeadroomTable = BTreeMap<String, f64>;

/// The deserialization shape of the sidecar documented on
/// [`HeadroomTable`]. Unknown fields are refused so a recorded cell pasted
/// in here is a load error rather than a table that silently binds nothing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeadroomFile {
    machine_class: String,
    #[serde(default)]
    headroom: HeadroomTable,
}

/// Where one class's measured-headroom sidecar lives: next to its baseline,
/// as `<class>.headroom.toml`. Derived from the baseline path rather than
/// passed separately so the two files can never be selected from different
/// classes.
#[must_use]
pub fn headroom_path(baseline_path: &Path) -> std::path::PathBuf {
    baseline_path.with_extension("headroom.toml")
}

/// Loads the measured-headroom sidecar at `path` for `class`.
///
/// A missing file is the legitimate "nothing characterized yet" state and
/// loads as an empty table; every metric then gates on its policy default.
///
/// # Errors
///
/// Returns [`BaselineError::Read`]/[`BaselineError::Parse`] on I/O or TOML
/// failures, [`BaselineError::ClassMismatch`] when the file declares a
/// class other than `class` (a sidecar copied across classes would apply
/// one host's measured spread to another), and
/// [`BaselineError::UnusableHeadroom`] on a factor no gate can apply.
pub fn load_headroom(path: &Path, class: &str) -> Result<HeadroomTable, BaselineError> {
    if !path.exists() {
        return Ok(HeadroomTable::new());
    }
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|source| BaselineError::Read {
        path: display.clone(),
        source,
    })?;
    let file: HeadroomFile = toml::from_str(&text).map_err(|source| BaselineError::Parse {
        path: display.clone(),
        source: Box::new(source),
    })?;
    if file.machine_class != class {
        return Err(BaselineError::ClassMismatch {
            path: display,
            recorded: file.machine_class,
            current: class.to_string(),
        });
    }
    for (metric, &factor) in &file.headroom {
        if !factor.is_finite() || factor <= 1.0 {
            return Err(BaselineError::UnusableHeadroom {
                path: display,
                metric: metric.clone(),
                factor,
            });
        }
    }
    Ok(file.headroom)
}

/// Rejects a headroom table whose entries name metrics no cell of
/// `baseline` records.
///
/// Such an entry would silently do nothing: the lookup misses, the policy
/// default applies, and the sidecar reads as though a measured allowance is
/// in force when none is. That is the one way the table can lie, so it is
/// checked against every baseline the table is about to be used with --
/// including the one a record is about to write, so a record that drops the
/// last cell measuring a characterized metric refuses instead of orphaning
/// the entry.
///
/// # Errors
///
/// Returns [`BaselineError::UnknownHeadroomMetric`] naming the unbound
/// entry.
pub fn require_headroom_bound(
    table: &HeadroomTable,
    baseline: &BaselineFile,
    table_path: &Path,
) -> Result<(), BaselineError> {
    for key in table.keys() {
        // a dotted key scopes the entry to one scenario, so it binds only
        // if that scenario's own cells record the metric; a bare key binds
        // through any cell
        let bound = match key.split_once('.') {
            Some((scenario, metric)) => baseline
                .cells
                .get(scenario)
                .is_some_and(|fixtures| fixtures.values().any(|cell| cell.contains_key(metric))),
            None => baseline
                .cells
                .values()
                .flat_map(BTreeMap::values)
                .any(|cell| cell.contains_key(key)),
        };
        if !bound {
            return Err(BaselineError::UnknownHeadroomMetric {
                path: table_path.display().to_string(),
                metric: key.clone(),
            });
        }
    }
    Ok(())
}

/// One recorded machine class's baselines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineFile {
    pub schema: u32,
    pub engine_pin: String,
    pub machine_class: String,
    /// Deserialize-only trap for a `[headroom]` table that belongs in the
    /// sidecar (see [`HeadroomTable`]). Never serialized, so a recorded
    /// file cannot carry one by construction; [`load`] turns a non-empty
    /// value here into [`BaselineError::HeadroomInBaseline`] instead of
    /// letting the next record silently destroy it.
    #[serde(default, skip_serializing)]
    headroom: HeadroomTable,
    /// scenario -> fixture -> metric -> recorded value.
    #[serde(flatten)]
    pub cells: BTreeMap<String, BTreeMap<String, CellMetrics>>,
}

impl BaselineFile {
    /// A fresh, empty baseline for `class` under `pin`.
    #[must_use]
    pub fn new(class: &str, pin: &str) -> Self {
        Self {
            schema: SUPPORTED_SCHEMA,
            engine_pin: pin.to_string(),
            machine_class: class.to_string(),
            headroom: HeadroomTable::new(),
            cells: BTreeMap::new(),
        }
    }

    /// Inserts or replaces one cell's metrics.
    pub fn upsert_cell(&mut self, id: &CellId, metrics: CellMetrics) {
        self.cells
            .entry(id.scenario.clone())
            .or_default()
            .insert(id.fixture.clone(), metrics);
    }

    /// The recorded metrics for one cell, if present.
    ///
    /// Both keys arrive inside one [`CellId`] rather than as two strings
    /// the caller orders itself. Named the other way round a lookup simply
    /// misses, and a miss is a legitimate answer everywhere this is called:
    /// the gate walk skips the cell, so every recorded bar goes untested
    /// while the run still reports the cells as within them.
    #[must_use]
    pub fn cell(&self, id: &CellId) -> Option<&CellMetrics> {
        self.cells.get(&id.scenario)?.get(&id.fixture)
    }
}

/// One gate breach: a measured metric above its recorded bar.
#[derive(Debug, Clone, PartialEq)]
pub struct Breach {
    pub scenario: String,
    pub fixture: String,
    pub metric: String,
    pub measured: f64,
    pub recorded: f64,
    pub headroom: Headroom,
    pub bar: f64,
}

impl std::fmt::Display for Breach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GATE BREACH [{}.{}] {}: measured {:.4} > bar {:.4} (recorded {:.4} {})",
            self.scenario,
            self.fixture,
            self.metric,
            self.measured,
            self.bar,
            self.recorded,
            self.headroom
        )
    }
}

/// Compares one cell's measured metrics against the recorded baseline.
/// Only metrics present in the baseline gate (a newly added metric cannot
/// retroactively fail old baselines); every gated metric is
/// lower-is-better, with per-metric headroom from [`gate_headroom`].
/// Takes the machine class name, not a pre-derived flag, so no caller can
/// pair one class's cells with another class's gate policy.
///
/// The measured side arrives as the whole [`MeasuredCell`] and the recorded
/// side as bare [`CellMetrics`]: the two are different types, so the
/// comparison cannot be handed its arguments the other way round. Passing
/// the recorded numbers as the measurement and the measurement as the bar
/// inverts the gate into the direction CI reads as green, and nothing about
/// the resulting numbers looks wrong.
#[must_use]
pub fn gate_cell(
    measured: &MeasuredCell,
    recorded: &CellMetrics,
    class: &str,
    headroom_table: &HeadroomTable,
) -> Vec<Breach> {
    let controlled = is_controlled_class(class);
    let mut breaches = Vec::new();
    for (metric, recorded_value) in recorded {
        let Some(&measured_value) = measured.metrics.get(metric) else {
            continue;
        };
        let Some(headroom) =
            headroom_for(headroom_table, &measured.id.scenario, metric, controlled)
        else {
            continue;
        };
        let bar = headroom.bar(*recorded_value);
        if measured_value > bar {
            breaches.push(Breach {
                scenario: measured.id.scenario.clone(),
                fixture: measured.id.fixture.clone(),
                metric: metric.clone(),
                measured: measured_value,
                recorded: *recorded_value,
                headroom,
                bar,
            });
        }
    }
    breaches
}

/// Baseline cells that `measured` and `skipped` together leave
/// unchecked, in deterministic (sorted) order. The forward gate walks
/// measured cells only, so a cell that silently fell out of a
/// full-coverage run (dropped from the matrix, lost to a selection bug)
/// would otherwise stay green forever with its recorded bars never
/// re-tested; `skipped` carries the cells the run legitimately excluded
/// for platform reasons so they do not count as coverage gaps.
#[must_use]
pub fn uncovered_cells(
    baseline: &BaselineFile,
    measured: &[CellId],
    skipped: &[CellId],
) -> Vec<CellId> {
    let mut uncovered = Vec::new();
    for (scenario, fixtures) in &baseline.cells {
        for fixture in fixtures.keys() {
            let id = CellId::new(scenario, fixture);
            if !measured
                .iter()
                .chain(skipped.iter())
                .any(|seen| *seen == id)
            {
                uncovered.push(id);
            }
        }
    }
    uncovered
}

/// Cells this run measured that the baseline holds no bar for, in run
/// order.
///
/// The mirror of [`uncovered_cells`]: that one names baseline cells the run
/// never measured, this one names measured cells the baseline never
/// recorded. Both are coverage gaps and both are collected rather than
/// raised at the first occurrence -- a gate that stops at the first missing
/// cell tells the operator about one problem when it could have told them
/// about every one in the same run, and prints none of the verdicts the
/// cells behind it already earned.
#[must_use]
pub fn unrecorded_cells(baseline: &BaselineFile, measured: &[MeasuredCell]) -> Vec<CellId> {
    measured
        .iter()
        .filter(|cell| baseline.cell(&cell.id).is_none())
        .map(|cell| cell.id.clone())
        .collect()
}

/// Recorded metrics of one cell that the run produced no value for, in
/// deterministic (sorted) order.
///
/// [`gate_cell`] walks only the metrics both sides hold, so a cell that
/// still runs but stops producing one of its recorded numbers (a renamed
/// metric, a platform whose row measures a different quantity under a
/// different name) would otherwise report green with that recorded bar
/// silently untested. This is [`uncovered_cells`] one level down: cell
/// coverage proves the row ran, metric coverage proves it produced what
/// the baseline gates.
///
/// Same asymmetric argument types as [`gate_cell`], for the same reason:
/// a swap here would report full coverage for a cell that stopped
/// producing a gated number. The reversed direction is
/// [`unrecorded_metrics`].
#[must_use]
pub fn unmeasured_metrics(measured: &MeasuredCell, recorded: &CellMetrics) -> Vec<String> {
    recorded
        .keys()
        .filter(|metric| !measured.metrics.contains_key(*metric))
        .cloned()
        .collect()
}

/// Metrics `measured` produced that the recorded cell holds no bar for,
/// in sorted order.
///
/// The mirror of [`unmeasured_metrics`], and [`unrecorded_cells`] one
/// level down: a baseline cell that exists but has lost (or never gained)
/// one of its row's metrics would otherwise leave that metric silently
/// ungated -- deleting a single key from a recorded cell must be as loud
/// as deleting the whole cell. A metric new to an existing row therefore
/// fails a gate until a record run arms it, the same discipline a new
/// cell already goes through.
#[must_use]
pub fn unrecorded_metrics(measured: &MeasuredCell, recorded: &CellMetrics) -> Vec<String> {
    measured
        .metrics
        .keys()
        .filter(|metric| !recorded.contains_key(*metric))
        .cloned()
        .collect()
}

/// Loads and validates `path`.
///
/// # Errors
///
/// Returns [`BaselineError::Read`]/[`BaselineError::Parse`] on I/O or
/// TOML failures, and [`BaselineError::UnsupportedSchema`] on a schema
/// this harness does not understand.
pub fn load(path: &Path) -> Result<BaselineFile, BaselineError> {
    let display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|source| BaselineError::Read {
        path: display.clone(),
        source,
    })?;
    let file: BaselineFile = toml::from_str(&text).map_err(|source| BaselineError::Parse {
        path: display.clone(),
        source: Box::new(source),
    })?;
    if file.schema != SUPPORTED_SCHEMA {
        return Err(BaselineError::UnsupportedSchema {
            path: display,
            found: file.schema,
        });
    }
    // save() cannot write a [headroom] table, so one here is hand-added and
    // about to be destroyed by the next record; refusing is what routes the
    // characterization to the file records never rewrite
    if !file.headroom.is_empty() {
        return Err(BaselineError::HeadroomInBaseline {
            sidecar: headroom_path(path).display().to_string(),
            path: display,
        });
    }
    Ok(file)
}

/// Serializes `file` to `path`, creating parent directories.
///
/// # Errors
///
/// Returns [`BaselineError::Serialize`]/[`BaselineError::Write`] on
/// serialization or I/O failures.
pub fn save(path: &Path, file: &BaselineFile) -> Result<(), BaselineError> {
    let display = path.display().to_string();
    let text =
        toml::to_string_pretty(file).map_err(|source| BaselineError::Serialize { source })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| BaselineError::Write {
            path: display.clone(),
            source,
        })?;
    }
    std::fs::write(path, text).map_err(|source| BaselineError::Write {
        path: display,
        source,
    })
}

/// Rejects gating or single-cell recording against a baseline recorded
/// under a different engine pin: numbers measured against one engine
/// version are not a regression bar for another.
///
/// # Errors
///
/// Returns [`BaselineError::PinDrift`] when the pins differ.
pub fn require_pin_match(
    file: &BaselineFile,
    current_pin: &str,
    path: &Path,
) -> Result<(), BaselineError> {
    if file.engine_pin != current_pin {
        return Err(BaselineError::PinDrift {
            path: path.display().to_string(),
            recorded: file.engine_pin.clone(),
            current: current_pin.to_string(),
        });
    }
    Ok(())
}

/// What ratcheting one measured metric against the recorded baseline did.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RatchetOutcome {
    /// The metric was not in the baseline; the measured value is recorded as-is.
    New { metric: String, value: f64 },
    /// The measured value beat the recorded bar; the bar moves down to it.
    Improved { metric: String, old: f64, new: f64 },
    /// The measured value did not beat the recorded bar, so the bar is
    /// held. `regression_masked` is set when the measured value also
    /// exceeds the gated bar: the record is
    /// refusing to lower a bar the measurement would have breached, which
    /// is a regression signal the operator must see, not noise the ratchet
    /// should silently absorb.
    Held {
        metric: String,
        recorded: f64,
        measured: f64,
        regression_masked: bool,
    },
    /// The measured value sits further below the recorded one than the
    /// class's published spread for the statistic admits, so the record
    /// refused it: the recorded value is held and the measured one is not
    /// written.
    ///
    /// The mirror image of the masked-regression surprise. There a
    /// suspicious *worse* value still writes -- the ratchet keeps the
    /// better bar and the very next gate re-announces the regression --
    /// while a suspiciously *better* value must not, because
    /// ratchet-only-tightens makes a lucky draw permanent: a bar pinned
    /// below the class's own honest band fails a large fraction of later
    /// honest runs on unchanged code. The legitimate path to a bar this
    /// low is a replicate campaign's median hand-edited into the baseline
    /// file: the floor is measured off the held recorded value, so a
    /// `--record` of the median refuses identically, by design -- an
    /// automated acceptance would be the same single-draw trust the guard
    /// exists to withdraw. The record report names the command, the cell
    /// and the file beside the values.
    RefusedBelowSpread {
        metric: String,
        recorded: f64,
        measured: f64,
        /// The published spread that tripped the refusal, carried so the
        /// report can print the band the measurement fell out of.
        spread: Headroom,
    },
    /// The measurement was not a number a bar can be derived from (NaN,
    /// infinite, or non-positive for a metric whose values are positive by
    /// construction), so nothing was recorded for it.
    ///
    /// Recording it anyway is the worse option in both directions: as a
    /// fresh value it writes a bar every later honest measurement breaches,
    /// and as a held one it hides that the run produced no usable number.
    Rejected { metric: String, value: f64 },
}

/// Ratchets one cell's `measured` metrics against the `existing` recorded
/// cell (if any), so a recorded bar moves only in the improving (lower)
/// direction. Every metric is lower-is-better (see the module invariant),
/// so the ratchet keeps `min(recorded, measured)` per metric; a metric the
/// baseline never held is recorded as-is. `controlled` and `headroom` select
/// the same per-metric allowance the gate applies (via [`headroom_for`]), so
/// the ratchet and the gate agree about what a masked regression is: a
/// record run must not stay quiet about a value the very next gate would
/// breach, nor cry regression at one the class's measured spread accepts.
///
/// The returned cell carries exactly the measured metric keys: an existing
/// metric the run did not remeasure is not carried forward, matching the
/// full-matrix record's from-scratch hygiene.
///
/// Ratcheting to `min` pins a metric's bar to its best-ever quiet run, so
/// the gated ceiling becomes `min` times the class's allowance for the
/// metric -- a measured sidecar factor where one exists, the compiled
/// default otherwise. A fixed factor over a floor that only falls tightens
/// the bar on every lucky run while the honest band stays put, so a normal
/// run can breach on unchanged code; a measured factor moves the exposure
/// without removing it, and it reaches ratios as well as absolutes. The
/// scroll characterization is the live example: its ratio_p50 factor
/// clears the replicate band's own rule by under half a percent, so a
/// record lowering scroll.minimal's floor about 4% (1.7778 to 1.70) puts
/// the bar at 2.006 against a 2.013 already observed on unchanged
/// binaries. Where the class has published its resolution -- a sidecar
/// spread for the statistic, read via [`declared_headroom`] -- the
/// downward move is therefore bounded by the same band mirrored below the
/// recorded value ([`Headroom::record_floor`]): a single draw past it is
/// refused ([`RatchetOutcome::RefusedBelowSpread`]) and the recorded bar
/// held, so moving a bar further than the class's honest runs move takes a
/// replicate campaign's median, hand-recorded. A class without a published
/// spread keeps the pure min-ratchet -- a compiled default is a guess, not
/// a measurement to mirror -- and its shared-class breach stays the
/// documented loud-breach-then-rerun regime.
#[must_use]
pub fn ratchet_cell(
    existing: Option<&CellMetrics>,
    measured: &CellMetrics,
    scenario: &str,
    controlled: bool,
    headroom: &HeadroomTable,
) -> (CellMetrics, Vec<RatchetOutcome>) {
    let mut cell = CellMetrics::new();
    let mut outcomes = Vec::new();
    for (metric, &value) in measured {
        // resolved off the table before the gate allowance shadows it: the
        // guard reads the published spread, never the compiled default a
        // gate allowance falls back to
        let declared = declared_headroom(headroom, scenario, metric);
        let headroom = headroom_for(headroom, scenario, metric, controlled);
        // most metrics are a latency, a ratio or a size and cannot be zero
        // or below without the run having gone wrong; the signed paired
        // delta reaches negative exactly when view beats nvim, so it is
        // held to finiteness alone
        let signed = headroom.is_some_and(Headroom::admits_non_positive);
        let usable = value.is_finite() && (signed || value > 0.0);
        if !usable {
            outcomes.push(RatchetOutcome::Rejected {
                metric: metric.clone(),
                value,
            });
            if let Some(&recorded) = existing.and_then(|existing| existing.get(metric)) {
                cell.insert(metric.clone(), recorded);
            }
            continue;
        }
        match existing.and_then(|existing| existing.get(metric)) {
            None => {
                cell.insert(metric.clone(), value);
                outcomes.push(RatchetOutcome::New {
                    metric: metric.clone(),
                    value,
                });
            }
            Some(&recorded) if value < recorded => {
                // a default is a guess about how far the statistic moves,
                // and refusing a record on a guess would demand a replicate
                // campaign of every class nobody has characterized
                let tripped = declared.filter(|spread| value < spread.record_floor(recorded));
                if let Some(spread) = tripped {
                    cell.insert(metric.clone(), recorded);
                    outcomes.push(RatchetOutcome::RefusedBelowSpread {
                        metric: metric.clone(),
                        recorded,
                        measured: value,
                        spread,
                    });
                } else {
                    cell.insert(metric.clone(), value);
                    outcomes.push(RatchetOutcome::Improved {
                        metric: metric.clone(),
                        old: recorded,
                        new: value,
                    });
                }
            }
            Some(&recorded) => {
                cell.insert(metric.clone(), recorded);
                let regression_masked =
                    headroom.is_some_and(|headroom| value > headroom.bar(recorded));
                outcomes.push(RatchetOutcome::Held {
                    metric: metric.clone(),
                    recorded,
                    measured: value,
                    regression_masked,
                });
            }
        }
    }
    (cell, outcomes)
}

/// Which cells a record touches, which decides how the recorded file is
/// assembled around the ratchet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecordMode {
    /// The whole matrix was measured. The file is rebuilt from the measured
    /// cells so a cell that left the matrix does not survive with a stale
    /// bar, and an existing file that cannot be a ratchet reference (a
    /// different pin or class) is discarded rather than blocking the record.
    FullMatrix,
    /// A single cell was measured. The existing file's other cells are
    /// preserved untouched; only the measured cell is ratcheted in.
    SingleCell,
}

/// What one cell's ratchet did during a record.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CellRatchet {
    pub scenario: String,
    pub fixture: String,
    pub outcomes: Vec<RatchetOutcome>,
}

/// The file to save and a per-cell account of what the ratchet did.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RecordPlan {
    pub file: BaselineFile,
    pub cells: Vec<CellRatchet>,
    /// The machine class recorded, carried so a below-spread refusal can
    /// name the exact replicate-campaign command in its own message.
    pub class: String,
    /// Set when a full-matrix record found an existing baseline it could
    /// not ratchet against (different pin or class) and recorded fresh
    /// instead; the operator is told why the bars did not ratchet.
    pub reset_reason: Option<String>,
}

/// Assembles the baseline to record from `measured`, ratcheting each cell
/// against `existing` when that file is a valid reference for it (same
/// engine `pin` and `class`; see [`ratchet_cell`] for the per-metric rule).
///
/// The two record modes differ only in how the surrounding file is built:
/// [`RecordMode::FullMatrix`] starts fresh so stale cells fall away and an
/// incomparable existing file is set aside (recorded in `reset_reason`);
/// [`RecordMode::SingleCell`] preserves the existing file's other cells.
///
/// Precondition for [`RecordMode::SingleCell`]: when `existing` is
/// `Some`, it must already be pin- and class-matched by the caller (via
/// [`require_pin_match`]/[`require_class_match`]), because a single-cell
/// edit of a file that fails those checks would silently invalidate it.
#[must_use]
pub fn plan_record(
    existing: Option<BaselineFile>,
    mode: RecordMode,
    class: &str,
    pin: &str,
    measured: &[MeasuredCell],
    headroom: &HeadroomTable,
) -> RecordPlan {
    let controlled = is_controlled_class(class);
    let comparable = existing
        .as_ref()
        .is_some_and(|file| file.engine_pin == pin && file.machine_class == class);

    let reset_reason = match &existing {
        Some(file) if !comparable => Some(format!(
            "existing baseline is pin {:?} class {:?}, not pin {pin:?} class {class:?}; recorded \
             fresh, no ratchet",
            file.engine_pin, file.machine_class
        )),
        _ => None,
    };

    // A single-cell record edits one cell of the existing file, so it starts
    // from that file to keep the others -- but only when the file is
    // comparable; cloning an incomparable one would save its cells (measured
    // under a different engine or class) relabeled under this run's pin. A
    // full-matrix record always rebuilds the file so a cell that left the
    // matrix cannot survive with a stale bar.
    let mut file = match (mode, &existing) {
        (RecordMode::SingleCell, Some(existing)) if comparable => existing.clone(),
        _ => BaselineFile::new(class, pin),
    };

    // The ratchet reference is the existing file only when it is comparable;
    // an incomparable file's numbers were taken against a different engine or
    // gate policy and are not a bar this run's numbers may be held to.
    let reference = comparable.then_some(existing.as_ref()).flatten();

    let mut cells = Vec::new();
    for cell in measured {
        let existing_cell = reference.and_then(|file| file.cell(&cell.id));
        let (ratcheted, outcomes) = ratchet_cell(
            existing_cell,
            &cell.metrics,
            &cell.id.scenario,
            controlled,
            headroom,
        );
        file.upsert_cell(&cell.id, ratcheted);
        cells.push(CellRatchet {
            scenario: cell.id.scenario.clone(),
            fixture: cell.id.fixture.clone(),
            outcomes,
        });
    }

    RecordPlan {
        file,
        cells,
        class: class.to_string(),
        reset_reason,
    }
}

impl RecordPlan {
    /// The number of held metrics whose measurement would have breached the
    /// gate: a ratcheted record kept the better bar, but each of these is a
    /// regression the operator must see rather than a bar that merely failed
    /// to improve.
    #[must_use]
    pub fn masked_regressions(&self) -> usize {
        self.cells
            .iter()
            .flat_map(|cell| &cell.outcomes)
            .filter(|outcome| {
                matches!(
                    outcome,
                    RatchetOutcome::Held {
                        regression_masked: true,
                        ..
                    }
                )
            })
            .count()
    }

    /// The number of metrics the record refused to move further below their
    /// recorded values than the class's published spread admits. Each held
    /// its recorded bar, the refused values are not in the file, and the
    /// operator's path to the lower bar is the replicate-campaign command
    /// in the metric's own alert line.
    #[must_use]
    pub fn spread_refusals(&self) -> usize {
        self.cells
            .iter()
            .flat_map(|cell| &cell.outcomes)
            .filter(|outcome| matches!(outcome, RatchetOutcome::RefusedBelowSpread { .. }))
            .count()
    }

    /// Human-readable lines summarizing what the record did, for `target`
    /// (the baseline path). Carries the reset note (if any), one line per
    /// metric ratcheted, the count recorded, and a trailing warning when any
    /// held bar hid a regression, so the operator never reads "recorded N
    /// cells" as "the bars all moved."
    #[must_use]
    pub fn report_lines(&self, target: &str) -> Vec<String> {
        self.report(target).info
    }

    /// The record's report, split by where each line must go: `info` is the
    /// ordinary per-metric ledger, `alerts` is what an operator must not
    /// skim past.
    ///
    /// A masked regression is the one outcome here that reports a *worse*
    /// measurement while still succeeding, so it goes to stderr with the
    /// breaches rather than into the stdout ledger it would otherwise sit
    /// in the middle of.
    #[must_use]
    pub fn report(&self, target: &str) -> RecordReport {
        let mut lines = Vec::new();
        let mut alerts = Vec::new();
        if let Some(reason) = &self.reset_reason {
            lines.push(format!("      {reason}"));
        }
        for cell in &self.cells {
            let (scenario, fixture) = (&cell.scenario, &cell.fixture);
            for outcome in &cell.outcomes {
                lines.push(match outcome {
                    RatchetOutcome::Improved { metric, old, new } => {
                        format!(
                            "      improved {scenario}.{fixture} {metric}: {old:.4} -> {new:.4}"
                        )
                    }
                    RatchetOutcome::New { metric, value } => {
                        format!("      new {scenario}.{fixture} {metric}: {value:.4}")
                    }
                    RatchetOutcome::Held {
                        metric,
                        recorded,
                        measured,
                        regression_masked: false,
                    } => format!(
                        "      held {scenario}.{fixture} {metric}: recorded {recorded:.4} kept \
                         (measured {measured:.4} did not improve it)"
                    ),
                    RatchetOutcome::RefusedBelowSpread {
                        metric,
                        recorded,
                        measured,
                        spread,
                    } => {
                        alerts.push(format!(
                            "RECORD REFUSED [{scenario}.{fixture}] {metric}: measured \
                             {measured:.4} is further below the recorded {recorded:.4} than the \
                             published spread ({spread}) admits (record floor {floor:.4}); one \
                             draw this deep pins a bar honest runs fail, so the recorded value \
                             was held and the measured one was not written. To move the bar, \
                             re-run `task bench -- --scenario {scenario} --fixture {fixture} \
                             --class {class}` repeatedly on a quiet host, take the replicate \
                             median, and hand-edit {metric} under [{scenario}.{fixture}] in \
                             {target} to it. The floor is measured off the recorded value this \
                             refusal holds, so `--record` will refuse the median the same way, \
                             by design: only a hand edit carries a campaign's median into the \
                             file",
                            floor = spread.record_floor(*recorded),
                            class = self.class,
                        ));
                        continue;
                    }
                    RatchetOutcome::Held {
                        metric,
                        recorded,
                        measured,
                        regression_masked: true,
                    } => {
                        alerts.push(format!(
                            "RECORD REGRESSION MASKED [{scenario}.{fixture}] {metric}: measured \
                             {measured:.4} would breach the recorded bar {recorded:.4}, held to \
                             keep the ratchet; a later --gate will breach on it, so investigate \
                             first"
                        ));
                        continue;
                    }
                    RatchetOutcome::Rejected { metric, value } => {
                        alerts.push(format!(
                            "RECORD REJECTED [{scenario}.{fixture}] {metric}: measured {value:.4} \
                             is not a value a bar can be derived from, so nothing was recorded \
                             for it this run"
                        ));
                        continue;
                    }
                });
            }
        }
        lines.push(format!(
            "recorded {} cell(s) into {target}",
            self.cells.len()
        ));
        let masked = self.masked_regressions();
        if masked > 0 {
            alerts.push(format!(
                "RECORD WARNING: {masked} held metric(s) would have breached the gate; the record \
                 kept the better bar but the measurement shows a regression"
            ));
        }
        let refused = self.spread_refusals();
        if refused > 0 {
            alerts.push(format!(
                "RECORD REFUSAL: {refused} metric(s) measured further below their recorded bars \
                 than this class's published spread admits; the recorded bars were held and the \
                 file does not carry the refused value(s)"
            ));
        }
        RecordReport {
            info: lines,
            alerts,
        }
    }
}

/// A [`RecordPlan`]'s report, split by stream.
///
/// Two vectors rather than one tagged list: the caller's only decision is
/// which stream a line goes to, and a caller that has to match on a tag to
/// find that out can get it wrong silently.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct RecordReport {
    /// The per-metric ledger and the recorded-cell count, for stdout.
    pub info: Vec<String>,
    /// Outcomes an operator must act on, for stderr.
    pub alerts: Vec<String>,
}

/// Rejects using a baseline file whose recorded `machine_class` differs
/// from the class this invocation named: the gate policy (controlled vs
/// shared) is derived from the class, so a mismatch would silently apply
/// one class's policy to another class's numbers.
///
/// # Errors
///
/// Returns [`BaselineError::ClassMismatch`] when the classes differ.
pub fn require_class_match(
    file: &BaselineFile,
    current_class: &str,
    path: &Path,
) -> Result<(), BaselineError> {
    if file.machine_class != current_class {
        return Err(BaselineError::ClassMismatch {
            path: path.display().to_string(),
            recorded: file.machine_class.clone(),
            current: current_class.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::float_cmp)]
    use std::collections::BTreeSet;

    use super::*;

    /// [`super::gate_cell`] with no measured per-class headroom, so every
    /// case below reads against the policy defaults. The override path has
    /// its own test rather than being threaded through all of them.
    fn gate_cell(
        scenario: &str,
        fixture: &str,
        measured: &CellMetrics,
        recorded: &CellMetrics,
        class: &str,
    ) -> Vec<Breach> {
        super::gate_cell(
            &MeasuredCell {
                id: CellId::new(scenario, fixture),
                metrics: measured.clone(),
            },
            recorded,
            class,
            &HeadroomTable::new(),
        )
    }

    fn metrics(pairs: &[(&str, f64)]) -> CellMetrics {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn round_trips_through_toml_with_scenario_fixture_tables() {
        let mut file = BaselineFile::new("dev-linux", "v0.12.4");
        file.upsert_cell(
            &CellId::new("echo", "minimal"),
            metrics(&[("ratio_p99", 1.21), ("paired_delta_p99_ms", 0.29)]),
        );
        let text = toml::to_string_pretty(&file).unwrap();
        assert!(text.contains("[echo.minimal]"), "actual TOML:\n{text}");
        let parsed: BaselineFile = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed.cell(&CellId::new("echo", "minimal")).unwrap()["ratio_p99"],
            1.21
        );
        assert_eq!(parsed.machine_class, "dev-linux");
    }

    /// A record pass over a class with a measured headroom characterization
    /// must leave both the factor and the comment documenting its
    /// measurement protocol in force afterwards: the factor is what the
    /// gate applies, and the comment is the only record of the replicate
    /// protocol that justifies it. The sidecar survives byte-for-byte
    /// because the record flow writes only the baseline path.
    #[test]
    fn a_record_pass_preserves_the_headroom_characterization() {
        let dir = std::env::temp_dir().join(format!("view-headroom-record-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dev-linux.toml");
        std::fs::write(
            &path,
            "schema = 1\nengine_pin = \"v0.12.4\"\nmachine_class = \"dev-linux\"\n\n\
             [echo.minimal]\nratio_p50 = 1.17\n",
        )
        .unwrap();
        let sidecar = headroom_path(&path);
        let curated = "machine_class = \"dev-linux\"\n\n[headroom]\n\
                       # 8 report-only replicates, one unchanged binary pair.\n\
                       ratio_p50 = 1.06\n";
        std::fs::write(&sidecar, curated).unwrap();

        let headroom = load_headroom(&sidecar, "dev-linux").unwrap();
        let existing = load(&path).unwrap();
        let measured = vec![MeasuredCell {
            id: CellId::new("echo", "minimal"),
            metrics: metrics(&[("ratio_p50", 1.18)]),
        }];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
            &headroom,
        );
        require_headroom_bound(&headroom, &plan.file, &sidecar).unwrap();
        save(&path, &plan.file).unwrap();

        // the by-construction claim rests on a serde attribute a future
        // edit could delete; an emitted empty table would still pass the
        // load-time refusal, so the written text is checked directly
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("[headroom]"),
            "the record writer emitted a headroom table into the baseline"
        );

        let text = std::fs::read_to_string(&sidecar).unwrap();
        assert_eq!(
            text, curated,
            "the record flow rewrote the hand-curated sidecar"
        );
        let survived = load_headroom(&sidecar, "dev-linux").unwrap();
        assert_eq!(survived.get("ratio_p50"), Some(&1.06));
        assert_eq!(
            headroom_for(&survived, "echo", "ratio_p50", false),
            Some(Headroom::Proportional(1.06))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `[headroom]` table inside the baseline is exactly what the next
    /// `--record` destroys, so it is refused at load with the sidecar named.
    #[test]
    fn a_baseline_carrying_a_headroom_table_is_refused() {
        let dir = std::env::temp_dir().join(format!("view-headroom-inline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dev-linux.toml");
        std::fs::write(
            &path,
            "schema = 1\nengine_pin = \"v0.12.4\"\nmachine_class = \"dev-linux\"\n\n\
             [headroom]\nratio_p50 = 1.06\n\n[echo.minimal]\nratio_p50 = 1.17\n",
        )
        .unwrap();
        let err = load(&path).unwrap_err();
        assert!(matches!(err, BaselineError::HeadroomInBaseline { .. }));
        assert!(
            err.to_string().contains("dev-linux.headroom.toml"),
            "the refusal must name the sidecar the table belongs in: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every shipped headroom sidecar against the baseline it characterises:
    /// the committed pair is what the gate will actually load, and a record
    /// that had orphaned or dropped a shipped characterization would
    /// otherwise only surface one full bench run later. This is the shipped
    /// counterpart of [`a_record_pass_preserves_the_headroom_characterization`].
    #[test]
    fn every_shipped_headroom_sidecar_binds_to_its_baseline() {
        let dir = crate::fixture::workspace_root()
            .join("crates")
            .join("view-bench")
            .join("baselines");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&dir).expect("the baselines directory must exist") {
            let path = entry.expect("readable directory entry").path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(class) = name.strip_suffix(".headroom.toml") else {
                continue;
            };
            let table = load_headroom(&path, class).expect("every shipped sidecar must load");
            assert!(
                !table.is_empty(),
                "{} characterises nothing; delete it rather than shipping an empty statement",
                path.display()
            );
            let baseline =
                load(&dir.join(format!("{class}.toml"))).expect("a sidecar's baseline must exist");
            require_headroom_bound(&table, &baseline, &path)
                .expect("every shipped headroom entry must bind to a recorded metric");
            checked += 1;
        }
        assert!(
            checked > 0,
            "dev-linux ships a characterization, so this walk must find at least one sidecar"
        );
    }

    /// The one way this table can lie is an entry that binds nothing: a key
    /// nothing measures looks like an allowance in force while the default
    /// silently applies. Malformed factors and wrong-class sidecars are
    /// load errors; an unbound entry is refused against the baseline it
    /// would be used with.
    #[test]
    fn a_headroom_entry_that_binds_nothing_is_a_load_error() {
        let dir = std::env::temp_dir().join(format!("view-headroom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dev-linux.toml");
        let sidecar = headroom_path(&path);
        let mut file = BaselineFile::new("dev-linux", "v0.12.4");
        file.upsert_cell(
            &CellId::new("echo", "minimal"),
            metrics(&[("ratio_p50", 1.17)]),
        );
        save(&path, &file).unwrap();

        let write = |body: &str| {
            std::fs::write(&sidecar, format!("machine_class = \"dev-linux\"\n{body}")).unwrap();
        };

        write("[headroom]\nratoi_p50 = 1.06\n");
        let table = load_headroom(&sidecar, "dev-linux").unwrap();
        assert!(matches!(
            require_headroom_bound(&table, &file, &sidecar),
            Err(BaselineError::UnknownHeadroomMetric { .. })
        ));

        write("[headroom]\nratio_p50 = 0.9\n");
        assert!(matches!(
            load_headroom(&sidecar, "dev-linux"),
            Err(BaselineError::UnusableHeadroom { .. })
        ));

        write("[headroom]\nratio_p50 = 1.06\n");
        assert!(matches!(
            load_headroom(&sidecar, "gh-linux"),
            Err(BaselineError::ClassMismatch { .. })
        ));

        let table = load_headroom(&sidecar, "dev-linux").unwrap();
        assert!(require_headroom_bound(&table, &file, &sidecar).is_ok());

        // a qualified entry binds through its own scenario's cells only: the
        // metric existing elsewhere in the file must not satisfy it, or a
        // scoped characterization typo'd against the wrong scenario would
        // read as an allowance in force
        write("[headroom]\n\"echo.ratio_p50\" = 1.06\n");
        let table = load_headroom(&sidecar, "dev-linux").unwrap();
        assert!(require_headroom_bound(&table, &file, &sidecar).is_ok());

        write("[headroom]\n\"scroll.ratio_p50\" = 1.12\n");
        let table = load_headroom(&sidecar, "dev-linux").unwrap();
        assert!(matches!(
            require_headroom_bound(&table, &file, &sidecar),
            Err(BaselineError::UnknownHeadroomMetric { .. })
        ));

        std::fs::remove_file(&sidecar).unwrap();
        assert!(
            load_headroom(&sidecar, "dev-linux").unwrap().is_empty(),
            "no sidecar means nothing characterized, which loads as an empty table"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_replaces_only_the_named_cell() {
        let mut file = BaselineFile::new("dev-linux", "v0.12.4");
        file.upsert_cell(
            &CellId::new("echo", "minimal"),
            metrics(&[("ratio_p99", 1.2)]),
        );
        file.upsert_cell(
            &CellId::new("echo", "heavy"),
            metrics(&[("ratio_p99", 1.4)]),
        );
        file.upsert_cell(
            &CellId::new("echo", "minimal"),
            metrics(&[("ratio_p99", 1.1)]),
        );
        assert_eq!(
            file.cell(&CellId::new("echo", "minimal")).unwrap()["ratio_p99"],
            1.1
        );
        assert_eq!(
            file.cell(&CellId::new("echo", "heavy")).unwrap()["ratio_p99"],
            1.4
        );
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<CellId> {
        list.iter().map(|(s, f)| CellId::new(s, f)).collect()
    }

    #[test]
    fn unrecorded_cells_names_every_measured_cell_the_baseline_has_no_bar_for() {
        // every one, not the first: a gate that stops at the first missing
        // cell reports one problem where it could have reported all of them
        let mut file = BaselineFile::new("dev-macos", "v0.12.4");
        file.upsert_cell(
            &CellId::new("echo", "minimal"),
            metrics(&[("ratio_p50", 1.5)]),
        );
        let measured = vec![
            measured_cell("first_paint", "minimal", &[("shell_visible_cold_ms", 4.3)]),
            measured_cell("echo", "minimal", &[("ratio_p50", 1.4)]),
            measured_cell("first_paint", "heavy", &[("shell_visible_cold_ms", 9.1)]),
        ];
        assert_eq!(
            unrecorded_cells(&file, &measured),
            vec![
                CellId::new("first_paint", "minimal"),
                CellId::new("first_paint", "heavy"),
            ]
        );
        assert!(unrecorded_cells(&file, &measured[1..2]).is_empty());
    }

    #[test]
    fn uncovered_cells_names_a_baseline_cell_the_run_never_touched() {
        let mut file = BaselineFile::new("dev-linux", "v0.12.4");
        file.upsert_cell(
            &CellId::new("echo", "minimal"),
            metrics(&[("ratio_p50", 1.5)]),
        );
        file.upsert_cell(
            &CellId::new("memory", "minimal"),
            metrics(&[("pss_mb", 3.4)]),
        );
        let uncovered = uncovered_cells(&file, &pairs(&[("echo", "minimal")]), &[]);
        assert_eq!(uncovered, pairs(&[("memory", "minimal")]));
    }

    #[test]
    fn uncovered_cells_accepts_a_platform_skipped_cell() {
        // a baseline holding a cell whose scenario this platform has no
        // measurement for, which is the shape the skip list exists for
        let mut file = BaselineFile::new("gh-windows", "v0.12.4");
        file.upsert_cell(
            &CellId::new("echo", "minimal"),
            metrics(&[("ratio_p50", 1.5)]),
        );
        file.upsert_cell(
            &CellId::new("memory", "minimal"),
            metrics(&[("pss_mb", 3.4)]),
        );
        let uncovered = uncovered_cells(
            &file,
            &pairs(&[("echo", "minimal")]),
            &pairs(&[("memory", "minimal")]),
        );
        assert!(
            uncovered.is_empty(),
            "a platform-skipped cell must not read as a coverage gap: {uncovered:?}"
        );
    }

    #[test]
    fn class_platform_reads_one_hyphen_delimited_token() {
        assert_eq!(class_platform("dev-linux"), Some("linux"));
        assert_eq!(class_platform("gh-macos"), Some("macos"));
        assert_eq!(class_platform("controlled-linux-x86"), Some("linux"));
        assert_eq!(class_platform("gh-windows"), Some("windows"));
        assert_eq!(class_platform("dev-linuxish"), None);
        assert_eq!(class_platform("bench-box-1"), None);
        assert_eq!(class_platform("dev-linux-macos"), None);
    }

    #[test]
    fn a_class_naming_another_platform_is_refused_on_this_host() {
        let host = std::env::consts::OS;
        assert!(
            require_host_platform(&format!("dev-{host}")).is_ok(),
            "the host's own platform must be accepted"
        );
        let other = if host == "linux" { "macos" } else { "linux" };
        let err = require_host_platform(&format!("gh-{other}")).unwrap_err();
        assert!(
            matches!(err, BaselineError::HostPlatformMismatch { .. }),
            "a foreign platform's class must be refused, got {err}"
        );
        let err = require_host_platform("bench-box-1").unwrap_err();
        assert!(matches!(err, BaselineError::ClassPlatformUnnamed { .. }));
    }

    #[test]
    fn a_recorded_metric_the_run_never_measured_is_named() {
        const RECORDED: &[(&str, f64)] = &[("pss_mb", 3.4)];
        const MEASURED: &[(&str, f64)] = &[("phys_footprint_mb", 41.0)];
        let recorded = metrics(RECORDED);
        assert!(
            gate_cell(
                "memory",
                "minimal",
                &metrics(MEASURED),
                &recorded,
                "dev-linux"
            )
            .is_empty(),
            "a differently named metric cannot breach, which is why coverage must catch it"
        );
        assert_eq!(
            unmeasured_metrics(&measured_cell("memory", "minimal", MEASURED), &recorded),
            vec!["pss_mb"]
        );
        assert!(
            unmeasured_metrics(&measured_cell("memory", "minimal", RECORDED), &recorded).is_empty()
        );
    }

    #[test]
    fn gate_passes_within_headroom_and_breaches_above_it() {
        let recorded = metrics(&[("ratio_p50", 1.0)]);
        let ok = gate_cell(
            "echo",
            "minimal",
            &metrics(&[("ratio_p50", 1.20)]),
            &recorded,
            "dev-linux",
        );
        assert!(ok.is_empty(), "1.20 sits inside 1.0 x {RATIO_HEADROOM}");
        let bad = gate_cell(
            "echo",
            "minimal",
            &metrics(&[("ratio_p50", 1.30)]),
            &recorded,
            "dev-linux",
        );
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].metric, "ratio_p50");
        assert_eq!(bad[0].headroom, Headroom::Proportional(RATIO_HEADROOM));
        let rendered = bad[0].to_string();
        assert!(
            rendered.contains("[echo.minimal]"),
            "breach must name the cell; actual: {rendered}"
        );
    }

    #[test]
    fn controlled_class_is_a_naming_convention() {
        assert!(is_controlled_class("controlled-linux-x86"));
        assert!(!is_controlled_class("dev-linux"));
        assert!(!is_controlled_class("shared-ci"));
        assert!(!is_controlled_class("linux-controlled"));
    }

    /// Every metric any row records, with the policy it gets on a shared and
    /// on a controlled class. Exhaustive on purpose: the classification rule
    /// reads name components, so a change to it can move a policy nobody
    /// intended, and only a metric-by-metric table shows which ones moved.
    ///
    /// Disconfirm: give `derives_from_tail` the word-order-sensitive rule it
    /// replaced -- `ends_with("ratio_p99") || ends_with("delta_p99_ms") ||
    /// (!contains("ratio") && (contains("_p99") || == "p99_ms"))` -- and the
    /// failure names exactly one moved policy, `cadence_p99_ratio` on a
    /// shared class, going from `None` to `Some(Proportional(1.25))`.
    #[test]
    fn headroom_policy_maps_every_recorded_metric() {
        let ratio = Some(Headroom::Proportional(RATIO_HEADROOM));
        let absolute = Some(Headroom::Proportional(ABSOLUTE_HEADROOM));
        let signed = Some(Headroom::Signed {
            factor: RATIO_HEADROOM,
            floor: SIGNED_DELTA_FLOOR_MS,
        });
        // a median and a size do not move with ambient load, so they gate on
        // every class; everything built on a p99 does not, until that
        // statistic has its own load-regime characterization; a cold-start
        // absolute is the same shape as a tail for a different reason --
        // cross-boot state rather than run-to-run scheduler noise -- and
        // gates the same way
        let roster = [
            ("ratio_p50", ratio, ratio),
            ("pace_ratio", ratio, ratio),
            ("marker_ratio_p50", ratio, ratio),
            ("control_ratio_p50", ratio, ratio),
            ("pss_mb", absolute, absolute),
            ("phys_footprint_mb", absolute, absolute),
            ("ratio_p99", None, ratio),
            ("marker_ratio_p99", None, ratio),
            ("control_ratio_p99", None, ratio),
            ("cadence_p99_ratio", None, ratio),
            ("view_p99_ms", None, absolute),
            ("staleness_p99_ms", None, absolute),
            ("cadence_p99_ms", None, absolute),
            ("control_p99_ms", None, absolute),
            ("key_to_rpc_p99_us", None, absolute),
            ("p99_ms", None, absolute),
            ("match_paint_p50_ms", absolute, absolute),
            ("first_page_p50_ms", absolute, absolute),
            ("match_paint_p99_ms", None, absolute),
            ("first_page_p99_ms", None, absolute),
            ("shell_visible_cold_ms", None, absolute),
            ("marker_cold_ms", None, absolute),
            ("wedge_detect_p99_ms", None, absolute),
            ("restart_rehydrate_p99_ms", None, absolute),
            ("paired_delta_p99_ms", None, signed),
            ("control_delta_p99_ms", None, signed),
        ];
        // the table above classifies the declared vocabulary, so the two
        // cannot drift: a metric declared and left unclassified, or
        // classified here and never declared, fails before any policy is
        // compared
        let listed: BTreeSet<&str> = roster.iter().map(|(metric, _, _)| *metric).collect();
        let declared: BTreeSet<&str> = RECORDED_METRICS.iter().copied().collect();
        assert_eq!(
            listed, declared,
            "every declared metric needs a policy row and vice versa"
        );

        // every mismatch is collected rather than asserted one at a time, so
        // a rule change reports the full set of policies it moved instead of
        // stopping at the first: "only this metric moved" is the claim being
        // checked, and one panic cannot support it
        let mut moved = Vec::new();
        for (metric, shared, controlled) in roster {
            for (class, expected) in [("shared", shared), ("controlled", controlled)] {
                let actual = gate_headroom(metric, class == "controlled");
                if actual != expected {
                    moved.push(format!(
                        "{metric} on a {class} class: expected {expected:?}, got {actual:?}"
                    ));
                }
            }
        }
        assert!(moved.is_empty(), "gate policy moved:\n{}", moved.join("\n"));
    }

    /// The tail rule reads a name component, not the letters anywhere in the
    /// name, and it covers every percentile from p99 outward. A deeper tail
    /// carries more of the ambient noise the shared-class exemption exists
    /// for, while a name that merely contains the letters of one carries
    /// none of it and must keep gating.
    ///
    /// Disconfirm: match `metric.contains("p99")` instead, and `warmup99_ms`
    /// loses its gate on a shared class; match the component exactly against
    /// `"p99"`, and `p999_ms` acquires one.
    #[test]
    fn the_tail_rule_reads_percentile_components_not_the_letters_p99() {
        let absolute = Some(Headroom::Proportional(ABSOLUTE_HEADROOM));
        assert_eq!(gate_headroom("p999_ms", false), None);
        assert_eq!(gate_headroom("p999_ms", true), absolute);
        assert_eq!(gate_headroom("warmup99_ms", false), absolute);
        assert_eq!(gate_headroom("warmup99_ms", true), absolute);
    }

    /// The cold-start rule reads a name component, not the letters anywhere
    /// in the name: `cold` as a whole component names the cross-boot state
    /// the exemption exists for, while a name that merely contains those
    /// letters carries none of it and must keep gating.
    ///
    /// Disconfirm: match `metric.contains("cold")` instead, and
    /// `coldstart_ms` loses its gate on a shared class.
    #[test]
    fn the_cold_start_rule_reads_a_component_not_the_letters_cold() {
        let absolute = Some(Headroom::Proportional(ABSOLUTE_HEADROOM));
        assert_eq!(gate_headroom("marker_cold_ms", false), None);
        assert_eq!(gate_headroom("marker_cold_ms", true), absolute);
        assert_eq!(gate_headroom("coldstart_ms", false), absolute);
        assert_eq!(gate_headroom("coldstart_ms", true), absolute);
    }

    /// A metric no policy row classifies would still get one from the
    /// fall-through arm -- gating on every class, on a rule nobody applied
    /// to it. The declared vocabulary is what keeps that unreachable, so a
    /// name outside it has to be visible to the row that produced it.
    #[test]
    fn a_metric_outside_the_declared_vocabulary_is_named() {
        let recorded = metrics(&[("ratio_p50", 1.0), ("cadence_p99_ratio", 1.0)]);
        assert!(undeclared_metrics(&recorded).is_empty());

        let with_a_stray = metrics(&[("ratio_p50", 1.0), ("cadence_p50_ms", 12.2)]);
        assert_eq!(undeclared_metrics(&with_a_stray), vec!["cadence_p50_ms"]);
    }

    /// `control_delta_p99_ms` is `paired_delta_p99_ms` for the control row:
    /// the same signed statistic, scoped by a prefix. A proportional bar
    /// inverts once the value goes negative, which is precisely the state a
    /// paired delta is built to reach when view wins, so a scoped delta that
    /// misses the signed policy is gated by a bar that reads backwards.
    ///
    /// Disconfirm: narrow the delta arm in `gate_headroom` to the exact name
    /// `paired_delta_p99_ms`, and this row falls through to the proportional
    /// tail policy instead of the signed one.
    #[test]
    fn a_scoped_paired_delta_inherits_the_signed_policy() {
        assert_eq!(gate_headroom("control_delta_p99_ms", false), None);
        assert_eq!(
            gate_headroom("control_delta_p99_ms", true),
            Some(Headroom::Signed {
                factor: RATIO_HEADROOM,
                floor: SIGNED_DELTA_FLOOR_MS
            })
        );
        assert!(
            gate_headroom("control_delta_p99_ms", true).is_some_and(Headroom::admits_non_positive)
        );
    }

    /// A measured entry resizes the allowance; absence leaves the default.
    /// An entry cannot resurrect a gate the class does not have, because the
    /// exemption answers "does this number mean anything here", which no
    /// amount of measured spread changes.
    #[test]
    fn a_measured_headroom_resizes_but_never_resurrects_a_gate() {
        let table: HeadroomTable = [
            ("ratio_p50".to_string(), 1.06),
            ("view_p99_ms".to_string(), 1.10),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            headroom_for(&table, "echo", "ratio_p50", false),
            Some(Headroom::Proportional(1.06))
        );
        assert_eq!(
            headroom_for(&table, "echo", "marker_ratio_p50", false),
            Some(Headroom::Proportional(RATIO_HEADROOM)),
            "a metric with no entry keeps the default"
        );
        assert_eq!(
            headroom_for(&table, "echo", "view_p99_ms", false),
            None,
            "a shared-class tail stays ungated however well its spread is known"
        );
        assert_eq!(
            headroom_for(&table, "echo", "view_p99_ms", true),
            Some(Headroom::Proportional(1.10))
        );
    }

    /// A `"scenario.metric"` entry wins over the bare entry in its own
    /// scenario and binds nowhere else, because the same statistic name
    /// carries a different measured spread per scenario.
    ///
    /// Disconfirm: a bare-key-only lookup gives every scenario 1.06 and the
    /// first assertion fails; a qualified key that leaked host-wide would
    /// fail the third.
    #[test]
    fn a_scenario_qualified_headroom_wins_only_in_its_scenario() {
        let table: HeadroomTable = [
            ("ratio_p50".to_string(), 1.06),
            ("scroll.ratio_p50".to_string(), 1.12),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            headroom_for(&table, "scroll", "ratio_p50", false),
            Some(Headroom::Proportional(1.12))
        );
        assert_eq!(
            headroom_for(&table, "echo", "ratio_p50", false),
            Some(Headroom::Proportional(1.06)),
            "the bare entry stays the host-wide characterization"
        );
        assert_eq!(
            headroom_for(&table, "flood", "ratio_p50", false),
            Some(Headroom::Proportional(1.06)),
            "a qualified entry must not leak outside its scenario"
        );
    }

    /// The published spread is reported only where it was published: no
    /// default stands in for it, and the `"scenario.metric"` key wins in its
    /// own scenario exactly as it does for the ratchet's allowance.
    ///
    /// Disconfirm: falling back to the policy default here would report every
    /// uncharacterized metric on every class as measured at 1.25 or 1.5, and
    /// the second assertion fails.
    #[test]
    fn a_declared_headroom_is_reported_only_where_the_class_published_one() {
        let table: HeadroomTable = [
            ("ratio_p50".to_string(), 1.06),
            ("scroll.ratio_p50".to_string(), 1.18),
        ]
        .into_iter()
        .collect();

        assert_eq!(
            declared_headroom(&table, "scroll", "ratio_p50"),
            Some(Headroom::Proportional(1.18))
        );
        assert_eq!(
            declared_headroom(&table, "echo", "view_p99_ms"),
            None,
            "an uncharacterized statistic reports absence, never a default"
        );
        assert_eq!(
            declared_headroom(&table, "echo", "ratio_p50"),
            Some(Headroom::Proportional(1.06))
        );
    }

    /// A published spread is the class's load-dependence, measured, so it is
    /// reported for a statistic the ratchet exempts from gating too: the
    /// exemption says a recorded value is too load-dependent to make a bar
    /// out of, which is a statement about the recorded value and not about
    /// how far the number moves. A caller holding a bound that came from
    /// somewhere other than a recorded measurement can act on the spread.
    ///
    /// The shape still follows the metric: a signed paired delta keeps its
    /// floor rather than taking a proportional allowance that would invert
    /// below zero.
    #[test]
    fn a_declared_headroom_survives_the_shared_class_exemption_and_keeps_its_shape() {
        let table: HeadroomTable = [
            ("cadence_p99_ms".to_string(), 1.15),
            ("paired_delta_p99_ms".to_string(), 1.30),
        ]
        .into_iter()
        .collect();

        assert_eq!(headroom_for(&table, "flood", "cadence_p99_ms", false), None);
        assert_eq!(
            declared_headroom(&table, "flood", "cadence_p99_ms"),
            Some(Headroom::Proportional(1.15))
        );
        assert_eq!(
            declared_headroom(&table, "echo", "paired_delta_p99_ms"),
            Some(Headroom::Signed {
                factor: 1.30,
                floor: SIGNED_DELTA_FLOOR_MS
            })
        );
    }

    /// The shared-class tail exemption is a property of the statistic, not
    /// of one spelling of it. A row that scopes the name to its own boundary
    /// carries the same scheduler-dominated tail and the same +/-50% ambient
    /// noise floor, so it must inherit the exemption rather than acquire a
    /// gate by renaming.
    ///
    /// Disconfirm: matching the metric name exactly instead of by suffix
    /// gives `marker_ratio_p99` the ordinary ratio headroom on `dev-linux`,
    /// so the first assertion returns `Some` and fails.
    #[test]
    fn a_scoped_tail_ratio_inherits_the_shared_class_exemption() {
        assert_eq!(gate_headroom("marker_ratio_p99", false), None);
        assert_eq!(
            gate_headroom("marker_ratio_p99", true),
            Some(Headroom::Proportional(RATIO_HEADROOM))
        );
        // the median ratio is the one every class gates, scoped or not
        assert_eq!(
            gate_headroom("marker_ratio_p50", false),
            Some(Headroom::Proportional(RATIO_HEADROOM))
        );
    }

    #[test]
    fn tail_metrics_never_breach_on_a_shared_class() {
        let recorded = metrics(&[("paired_delta_p99_ms", 0.6), ("ratio_p99", 1.05)]);
        let measured = metrics(&[("paired_delta_p99_ms", 92.5), ("ratio_p99", 1.95)]);
        assert!(
            gate_cell("echo", "minimal", &measured, &recorded, "dev-linux").is_empty(),
            "reference-only metrics must not gate even far above their recorded values"
        );
    }

    #[test]
    fn tail_metrics_gate_on_a_controlled_class() {
        let recorded = metrics(&[("paired_delta_p99_ms", 0.6), ("ratio_p99", 1.05)]);
        let measured = metrics(&[("paired_delta_p99_ms", 92.5), ("ratio_p99", 1.95)]);
        let breaches = gate_cell("echo", "minimal", &measured, &recorded, "controlled-linux");
        assert_eq!(
            breaches.len(),
            2,
            "both tail statistics must gate on a controlled class; got {breaches:?}"
        );
        assert_eq!(
            breaches
                .iter()
                .find(|b| b.metric == "ratio_p99")
                .map(|b| b.headroom),
            Some(Headroom::Proportional(RATIO_HEADROOM))
        );
        assert_eq!(
            breaches
                .iter()
                .find(|b| b.metric == "paired_delta_p99_ms")
                .map(|b| b.headroom),
            Some(Headroom::Signed {
                factor: RATIO_HEADROOM,
                floor: SIGNED_DELTA_FLOOR_MS
            })
        );
        let within = metrics(&[("paired_delta_p99_ms", 0.7), ("ratio_p99", 1.2)]);
        assert!(gate_cell("echo", "minimal", &within, &recorded, "controlled-linux").is_empty());
    }

    #[test]
    fn gate_cell_leaves_unrecorded_metrics_to_the_coverage_check() {
        // no recorded bar means nothing for the breach scan to compare, so
        // the finding belongs to unrecorded_metrics, which must name it
        let recorded = metrics(&[("ratio_p50", 1.0)]);
        let measured = metrics(&[("ratio_p50", 0.9), ("new_metric", 99.0)]);
        assert!(gate_cell("echo", "minimal", &measured, &recorded, "dev-linux").is_empty());
        assert_eq!(
            unrecorded_metrics(
                &measured_cell(
                    "echo",
                    "minimal",
                    &[("ratio_p50", 0.9), ("new_metric", 99.0)]
                ),
                &recorded
            ),
            vec!["new_metric"]
        );
    }

    #[test]
    fn a_present_cell_missing_one_recorded_metric_is_named() {
        // deleting a single key from a recorded cell must be as loud as
        // deleting the cell: the breach scan cannot see a bar that is not
        // there, so coverage has to
        const MEASURED: &[(&str, f64)] = &[
            ("first_page_p50_ms", 2.4),
            ("first_page_p99_ms", 5.3),
            ("match_paint_p50_ms", 3.3),
            ("match_paint_p99_ms", 5.1),
        ];
        let full = metrics(MEASURED);
        let cell = measured_cell("picker", "minimal", MEASURED);
        assert!(unrecorded_metrics(&cell, &full).is_empty());
        let mut one_deleted = full;
        one_deleted.remove("match_paint_p99_ms");
        assert!(
            gate_cell(
                "picker",
                "minimal",
                &metrics(MEASURED),
                &one_deleted,
                "dev-linux"
            )
            .is_empty(),
            "the deleted bar cannot breach, which is why coverage must catch it"
        );
        assert_eq!(
            unrecorded_metrics(&cell, &one_deleted),
            vec!["match_paint_p99_ms"]
        );
    }

    #[test]
    fn class_mismatch_is_rejected() {
        let file = BaselineFile::new("dev-linux", "v0.12.4");
        let err = require_class_match(&file, "controlled-linux", Path::new("x.toml")).unwrap_err();
        assert!(matches!(err, BaselineError::ClassMismatch { .. }));
        assert!(require_class_match(&file, "dev-linux", Path::new("x.toml")).is_ok());
    }

    #[test]
    fn unsupported_schema_is_rejected_on_load() {
        let dir = std::env::temp_dir().join(format!("view-baselines-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad-schema.toml");
        std::fs::write(
            &path,
            "schema = 2\nengine_pin = \"v0.12.4\"\nmachine_class = \"dev-linux\"\n",
        )
        .unwrap();
        assert!(matches!(
            load(&path).unwrap_err(),
            BaselineError::UnsupportedSchema { found: 2, .. }
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pin_drift_is_rejected() {
        let file = BaselineFile::new("dev-linux", "v0.12.3");
        let err = require_pin_match(&file, "v0.12.4", Path::new("x.toml")).unwrap_err();
        assert!(matches!(err, BaselineError::PinDrift { .. }));
        let file = BaselineFile::new("dev-linux", "v0.12.4");
        assert!(require_pin_match(&file, "v0.12.4", Path::new("x.toml")).is_ok());
    }

    #[test]
    fn save_and_load_round_trip_on_disk() {
        let dir = std::env::temp_dir().join(format!("view-baselines-rt-{}", std::process::id()));
        let path = dir.join("dev-linux.toml");
        let mut file = BaselineFile::new("dev-linux", "v0.12.4");
        file.upsert_cell(
            &CellId::new("first_paint", "heavy"),
            metrics(&[("cold_ms", 38.0)]),
        );
        save(&path, &file).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(
            loaded.cell(&CellId::new("first_paint", "heavy")).unwrap()["cold_ms"],
            38.0
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_cell_survives_the_round_trip_as_present() {
        // what a refused row records: a cell with no metrics. It must load
        // back as present-and-empty, not absent -- absent would fail the
        // gate's unrecorded_cells walk on the very next run, turning an
        // attributed refusal into an unattributed coverage failure
        let dir = std::env::temp_dir().join(format!("view-baselines-empty-{}", std::process::id()));
        let path = dir.join("gh-macos.toml");
        let mut file = BaselineFile::new("gh-macos", "v0.12.4");
        let id = CellId::new("output_path", "minimal");
        file.upsert_cell(&id, CellMetrics::new());
        save(&path, &file).unwrap();
        let loaded = load(&path).unwrap();
        let cell = loaded
            .cell(&id)
            .expect("an empty cell must still be a recorded cell");
        assert!(cell.is_empty());
        assert!(unrecorded_cells(
            &loaded,
            &[MeasuredCell {
                id,
                metrics: CellMetrics::new(),
            }]
        )
        .is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn outcome_for<'a>(outcomes: &'a [RatchetOutcome], name: &str) -> &'a RatchetOutcome {
        outcomes
            .iter()
            .find(|o| match o {
                RatchetOutcome::New { metric, .. }
                | RatchetOutcome::Improved { metric, .. }
                | RatchetOutcome::Held { metric, .. }
                | RatchetOutcome::RefusedBelowSpread { metric, .. }
                | RatchetOutcome::Rejected { metric, .. } => metric == name,
            })
            .expect("outcome present for the named metric")
    }

    #[test]
    fn ratchet_holds_the_bar_when_the_measurement_regresses() {
        let existing = metrics(&[("ratio_p50", 1.20)]);
        let measured = metrics(&[("ratio_p50", 1.35)]);
        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &measured,
            "echo",
            false,
            &HeadroomTable::new(),
        );
        assert_eq!(
            cell["ratio_p50"], 1.20,
            "a worse measurement must not raise the recorded bar"
        );
        assert_eq!(
            outcome_for(&outcomes, "ratio_p50"),
            &RatchetOutcome::Held {
                metric: "ratio_p50".to_string(),
                recorded: 1.20,
                measured: 1.35,
                regression_masked: false,
            }
        );
    }

    #[test]
    fn ratchet_lowers_the_bar_when_the_measurement_improves() {
        let existing = metrics(&[("ratio_p50", 1.35)]);
        let measured = metrics(&[("ratio_p50", 1.20)]);
        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &measured,
            "echo",
            false,
            &HeadroomTable::new(),
        );
        assert_eq!(
            cell["ratio_p50"], 1.20,
            "a better measurement must lower the recorded bar to it"
        );
        assert_eq!(
            outcome_for(&outcomes, "ratio_p50"),
            &RatchetOutcome::Improved {
                metric: "ratio_p50".to_string(),
                old: 1.35,
                new: 1.20,
            }
        );
    }

    #[test]
    fn ratchet_records_a_metric_the_baseline_never_held() {
        let existing = metrics(&[("ratio_p50", 1.20)]);
        let measured = metrics(&[("ratio_p50", 1.15), ("view_p99_ms", 2.0)]);
        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &measured,
            "echo",
            false,
            &HeadroomTable::new(),
        );
        assert_eq!(cell["view_p99_ms"], 2.0);
        assert_eq!(
            outcome_for(&outcomes, "view_p99_ms"),
            &RatchetOutcome::New {
                metric: "view_p99_ms".to_string(),
                value: 2.0,
            }
        );
    }

    #[test]
    fn ratchet_with_no_existing_cell_records_every_metric_as_new() {
        let measured = metrics(&[("ratio_p50", 1.20), ("view_p99_ms", 2.0)]);
        let (cell, outcomes) = ratchet_cell(None, &measured, "echo", false, &HeadroomTable::new());
        assert_eq!(cell, measured);
        assert!(outcomes
            .iter()
            .all(|o| matches!(o, RatchetOutcome::New { .. })));
        assert_eq!(outcomes.len(), 2);
    }

    #[test]
    fn ratchet_flags_a_held_value_that_exceeds_the_gated_bar_as_a_masked_regression() {
        // recorded 1.0, headroom 1.25 -> gated bar 1.25; a measurement of
        // 1.40 both fails to improve AND would breach the gate, so holding
        // the bar hides a real regression the operator must be told about.
        let existing = metrics(&[("ratio_p50", 1.0)]);
        let measured = metrics(&[("ratio_p50", 1.40)]);
        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &measured,
            "echo",
            false,
            &HeadroomTable::new(),
        );
        assert_eq!(cell["ratio_p50"], 1.0);
        assert_eq!(
            outcome_for(&outcomes, "ratio_p50"),
            &RatchetOutcome::Held {
                metric: "ratio_p50".to_string(),
                recorded: 1.0,
                measured: 1.40,
                regression_masked: true,
            }
        );
    }

    #[test]
    fn ratchet_does_not_carry_forward_a_metric_the_run_did_not_remeasure() {
        let existing = metrics(&[("ratio_p50", 1.20), ("stale_metric", 9.0)]);
        let measured = metrics(&[("ratio_p50", 1.15)]);
        let (cell, _outcomes) = ratchet_cell(
            Some(&existing),
            &measured,
            "echo",
            false,
            &HeadroomTable::new(),
        );
        assert!(
            !cell.contains_key("stale_metric"),
            "an existing metric the run did not remeasure must not survive: {cell:?}"
        );
    }

    #[test]
    fn ratchet_flags_masked_regression_only_for_gated_metrics() {
        // ratio_p99 is ungated on a shared class (gate_headroom None), so a
        // held-worse value there is not a masked gate breach: there is no
        // bar for it to have breached.
        let existing = metrics(&[("ratio_p99", 1.0)]);
        let measured = metrics(&[("ratio_p99", 5.0)]);
        let (_cell, outcomes) = ratchet_cell(
            Some(&existing),
            &measured,
            "echo",
            false,
            &HeadroomTable::new(),
        );
        assert_eq!(
            outcome_for(&outcomes, "ratio_p99"),
            &RatchetOutcome::Held {
                metric: "ratio_p99".to_string(),
                recorded: 1.0,
                measured: 5.0,
                regression_masked: false,
            }
        );
    }

    fn spread_table(entries: &[(&str, f64)]) -> HeadroomTable {
        entries
            .iter()
            .map(|(key, factor)| (key.to_string(), *factor))
            .collect()
    }

    #[test]
    fn a_below_spread_improvement_is_refused_and_the_recorded_bar_held() {
        // recorded 1.20 under a published x1.10 spread tolerates 0.12 of
        // movement either way; 1.05 is 0.15 below, a draw the published band
        // says honest runs do not produce, and ratchet-only-tightens would
        // pin the bar to it forever
        let existing = metrics(&[("ratio_p50", 1.20)]);
        let measured = metrics(&[("ratio_p50", 1.05)]);
        let table = spread_table(&[("echo.ratio_p50", 1.10)]);
        let (cell, outcomes) = ratchet_cell(Some(&existing), &measured, "echo", false, &table);
        assert_eq!(
            cell["ratio_p50"], 1.20,
            "the refused value must not be written; the recorded bar is held"
        );
        assert_eq!(
            outcome_for(&outcomes, "ratio_p50"),
            &RatchetOutcome::RefusedBelowSpread {
                metric: "ratio_p50".to_string(),
                recorded: 1.20,
                measured: 1.05,
                spread: Headroom::Proportional(1.10),
            }
        );
    }

    #[test]
    fn the_record_floor_boundary_is_inclusive_for_a_proportional_spread() {
        // "further below than the spread" is strict: a draw AT the mirrored
        // floor moved exactly as far as the published band reaches, which
        // the band vouches for; one just past it is the draw the band
        // excludes
        let table = spread_table(&[("echo.ratio_p50", 1.10)]);
        let existing = metrics(&[("ratio_p50", 1.20)]);
        let floor = Headroom::Proportional(1.10).record_floor(1.20);

        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("ratio_p50", floor)]),
            "echo",
            false,
            &table,
        );
        assert_eq!(cell["ratio_p50"], floor, "an at-floor improvement records");
        assert!(matches!(
            outcome_for(&outcomes, "ratio_p50"),
            RatchetOutcome::Improved { .. }
        ));

        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("ratio_p50", floor - 1e-9)]),
            "echo",
            false,
            &table,
        );
        assert_eq!(cell["ratio_p50"], 1.20);
        assert!(matches!(
            outcome_for(&outcomes, "ratio_p50"),
            RatchetOutcome::RefusedBelowSpread { .. }
        ));
    }

    #[test]
    fn the_record_floor_boundary_is_inclusive_for_a_signed_spread() {
        // at a recorded 0.5 the 0.25 ms floor dominates 0.5 * 0.30, so the
        // band admits a fall to exactly 0.25 -- half the recorded value,
        // where a proportional mirror would have placed the floor elsewhere
        let table = spread_table(&[("echo.paired_delta_p99_ms", 1.30)]);
        let existing = metrics(&[("paired_delta_p99_ms", 0.5)]);
        let spread = Headroom::Signed {
            factor: 1.30,
            floor: SIGNED_DELTA_FLOOR_MS,
        };
        let floor = spread.record_floor(0.5);

        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("paired_delta_p99_ms", floor)]),
            "echo",
            true,
            &table,
        );
        assert_eq!(cell["paired_delta_p99_ms"], floor);
        assert!(matches!(
            outcome_for(&outcomes, "paired_delta_p99_ms"),
            RatchetOutcome::Improved { .. }
        ));

        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("paired_delta_p99_ms", floor - 1e-9)]),
            "echo",
            true,
            &table,
        );
        assert_eq!(cell["paired_delta_p99_ms"], 0.5);
        assert!(matches!(
            outcome_for(&outcomes, "paired_delta_p99_ms"),
            RatchetOutcome::RefusedBelowSpread { .. }
        ));
    }

    #[test]
    fn the_signed_mirror_stays_below_a_negative_recorded_value() {
        // the downward counterpart of the upward no-inversion guarantee: at
        // a recorded -0.20 under Signed { 1.30, 0.25 } the floor-dominant
        // allowance mirrors to -0.45, strictly below the recorded value. A
        // proportional mirror would invert here (-0.20 * (2 - 1.30) =
        // -0.14, a floor ABOVE the value it mirrors from) and refuse every
        // improvement, however small
        let spread = Headroom::Signed {
            factor: 1.30,
            floor: SIGNED_DELTA_FLOOR_MS,
        };
        assert!((spread.record_floor(-0.20) - (-0.45)).abs() < 1e-12);

        let table = spread_table(&[("echo.paired_delta_p99_ms", 1.30)]);
        let existing = metrics(&[("paired_delta_p99_ms", -0.20)]);
        let floor = spread.record_floor(-0.20);

        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("paired_delta_p99_ms", floor)]),
            "echo",
            true,
            &table,
        );
        assert_eq!(cell["paired_delta_p99_ms"], floor);
        assert!(matches!(
            outcome_for(&outcomes, "paired_delta_p99_ms"),
            RatchetOutcome::Improved { .. }
        ));

        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("paired_delta_p99_ms", floor - 1e-9)]),
            "echo",
            true,
            &table,
        );
        assert_eq!(cell["paired_delta_p99_ms"], -0.20);
        assert!(matches!(
            outcome_for(&outcomes, "paired_delta_p99_ms"),
            RatchetOutcome::RefusedBelowSpread { .. }
        ));
    }

    #[test]
    fn the_mirrored_band_matches_the_published_upward_tolerance() {
        // Proportional mirrors to recorded * (2 - factor)
        assert!((Headroom::Proportional(1.25).record_floor(1.0) - 0.75).abs() < 1e-12);
        // a factor of 2 mirrors to zero, so a positive-by-construction
        // metric can never trip a band that wide
        assert!(Headroom::Proportional(2.0).record_floor(1.0).abs() < 1e-12);
        let signed = Headroom::Signed {
            factor: 1.30,
            floor: 0.25,
        };
        // factor-dominant: recorded - |recorded| * (factor - 1)
        assert!((signed.record_floor(2.0) - 1.4).abs() < 1e-9);
        // floor-dominant: recorded - floor
        assert!((signed.record_floor(0.5) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn a_deep_improvement_without_a_published_spread_records_normally() {
        // the compiled default is a guess about how far the statistic
        // moves, not a measurement; refusing a record on a guess would
        // demand a replicate campaign of every uncharacterized class
        let existing = metrics(&[("ratio_p50", 1.20)]);
        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("ratio_p50", 0.30)]),
            "echo",
            false,
            &HeadroomTable::new(),
        );
        assert_eq!(cell["ratio_p50"], 0.30);
        assert!(matches!(
            outcome_for(&outcomes, "ratio_p50"),
            RatchetOutcome::Improved { .. }
        ));
    }

    #[test]
    fn a_worsening_stays_on_the_masked_regression_path_under_a_published_spread() {
        // the guard is one-directional: the worse-direction surprise
        // already has its own channel, and it writes (the ratchet keeps
        // the better bar) where the better-direction surprise must not
        let table = spread_table(&[("echo.ratio_p50", 1.05)]);
        let existing = metrics(&[("ratio_p50", 1.0)]);
        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("ratio_p50", 1.10)]),
            "echo",
            false,
            &table,
        );
        assert_eq!(cell["ratio_p50"], 1.0);
        assert_eq!(
            outcome_for(&outcomes, "ratio_p50"),
            &RatchetOutcome::Held {
                metric: "ratio_p50".to_string(),
                recorded: 1.0,
                measured: 1.10,
                regression_masked: true,
            }
        );
    }

    #[test]
    fn the_scenario_qualified_spread_governs_the_guard_in_its_own_scenario() {
        // the bare x2.0 host-wide entry mirrors to a floor of 0 and admits
        // any positive draw; the echo-qualified x1.05 puts the floor at
        // 0.95, and in echo it must be the one the guard reads
        let table = spread_table(&[("ratio_p50", 2.0), ("echo.ratio_p50", 1.05)]);
        let existing = metrics(&[("ratio_p50", 1.0)]);
        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("ratio_p50", 0.90)]),
            "echo",
            false,
            &table,
        );
        assert_eq!(cell["ratio_p50"], 1.0);
        assert!(matches!(
            outcome_for(&outcomes, "ratio_p50"),
            RatchetOutcome::RefusedBelowSpread {
                spread: Headroom::Proportional(factor),
                ..
            } if (*factor - 1.05).abs() < 1e-12
        ));
    }

    #[test]
    fn the_guard_reads_a_published_spread_the_shared_class_gate_exempts() {
        // ratio_p99 is ungated on a shared class, but the ratchet still
        // writes tail bars there and a lucky tail draw pins them just the
        // same; the spread is a measured fact about how far the number
        // moves, which is the only thing the guard consumes
        let table = spread_table(&[("echo.ratio_p99", 1.10)]);
        let existing = metrics(&[("ratio_p99", 1.0)]);
        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("ratio_p99", 0.85)]),
            "echo",
            false,
            &table,
        );
        assert_eq!(cell["ratio_p99"], 1.0);
        assert!(matches!(
            outcome_for(&outcomes, "ratio_p99"),
            RatchetOutcome::RefusedBelowSpread { .. }
        ));
    }

    #[test]
    fn a_fresh_metric_is_not_guarded_because_nothing_recorded_exists_to_fall_below() {
        let table = spread_table(&[("echo.ratio_p50", 1.05)]);
        let (cell, outcomes) = ratchet_cell(
            None,
            &metrics(&[("ratio_p50", 0.50)]),
            "echo",
            false,
            &table,
        );
        assert_eq!(cell["ratio_p50"], 0.50);
        assert!(matches!(
            outcome_for(&outcomes, "ratio_p50"),
            RatchetOutcome::New { .. }
        ));
    }

    fn measured_cell(scenario: &str, fixture: &str, pairs: &[(&str, f64)]) -> MeasuredCell {
        MeasuredCell {
            id: CellId::new(scenario, fixture),
            metrics: metrics(pairs),
        }
    }

    type CellSpec<'a> = (&'a str, &'a str, &'a [(&'a str, f64)]);

    fn baseline_with(pin: &str, class: &str, cells: &[CellSpec]) -> BaselineFile {
        let mut file = BaselineFile::new(class, pin);
        for (scenario, fixture, pairs) in cells {
            file.upsert_cell(&CellId::new(scenario, fixture), metrics(pairs));
        }
        file
    }

    #[test]
    fn full_matrix_record_ratchets_each_cell_against_a_comparable_baseline() {
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[("echo", "minimal", &[("ratio_p50", 1.30)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert_eq!(
            plan.file.cell(&CellId::new("echo", "minimal")).unwrap()["ratio_p50"],
            1.20
        );
        assert!(plan.reset_reason.is_none());
    }

    #[test]
    fn full_matrix_record_drops_a_cell_no_longer_measured() {
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[
                ("echo", "minimal", &[("ratio_p50", 1.30)]),
                ("scroll", "minimal", &[("ratio_p50", 1.70)]),
            ],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.25)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert!(
            plan.file.cell(&CellId::new("scroll", "minimal")).is_none(),
            "a full-matrix record must not carry a cell the run no longer measures"
        );
    }

    #[test]
    fn full_matrix_record_ignores_a_pin_drifted_baseline_and_records_fresh() {
        let existing = baseline_with(
            "v0.12.3",
            "dev-linux",
            &[("echo", "minimal", &[("ratio_p50", 0.5)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert_eq!(
            plan.file.cell(&CellId::new("echo", "minimal")).unwrap()["ratio_p50"],
            1.20,
            "a baseline under another pin must not ratchet the fresh measurement down to its own number"
        );
        assert!(plan.reset_reason.is_some());
        assert!(matches!(
            plan.cells[0].outcomes[0],
            RatchetOutcome::New { .. }
        ));
    }

    #[test]
    fn full_matrix_record_with_no_existing_file_records_fresh_without_a_reset_reason() {
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            None,
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert_eq!(
            plan.file.cell(&CellId::new("echo", "minimal")).unwrap()["ratio_p50"],
            1.20
        );
        assert!(
            plan.reset_reason.is_none(),
            "recording the first baseline is not a reset"
        );
    }

    #[test]
    fn single_cell_record_preserves_the_other_cells_and_ratchets_the_measured_one() {
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[
                ("echo", "minimal", &[("ratio_p50", 1.30)]),
                ("scroll", "minimal", &[("ratio_p50", 1.70)]),
            ],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::SingleCell,
            "dev-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert_eq!(
            plan.file.cell(&CellId::new("scroll", "minimal")).unwrap()["ratio_p50"],
            1.70,
            "a single-cell record must leave the other cells untouched"
        );
        assert_eq!(
            plan.file.cell(&CellId::new("echo", "minimal")).unwrap()["ratio_p50"],
            1.20
        );
    }

    #[test]
    fn report_names_an_improved_metric_and_counts_no_masked_regression() {
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[("echo", "minimal", &[("ratio_p50", 1.30)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert_eq!(plan.masked_regressions(), 0);
        let lines = plan.report_lines("dev-linux.toml");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("improved") && l.contains("ratio_p50")),
            "an improved metric must be named in the report: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("recorded 1 cell(s) into dev-linux.toml")),
            "the report must state where and how many cells were recorded: {lines:?}"
        );
    }

    #[test]
    fn report_flags_a_masked_regression_and_a_trailing_warning() {
        // recorded 1.0, headroom 1.25; measuring 1.40 does not improve and
        // would breach the gate, so the held bar hides a regression.
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[("echo", "minimal", &[("ratio_p50", 1.0)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.40)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert_eq!(plan.masked_regressions(), 1);
        let report = plan.report("dev-linux.toml");
        // both belong to the alert stream, not the ledger: a run that
        // otherwise succeeds is exactly where a worse measurement gets
        // skimmed past
        assert!(
            report
                .alerts
                .iter()
                .any(|l| l.contains("REGRESSION MASKED")),
            "a masked regression must be called out: {report:?}"
        );
        assert!(
            report.alerts.iter().any(|l| l.contains("RECORD WARNING:")),
            "a warning must summarize the masked regression: {report:?}"
        );
        assert!(
            !report.info.iter().any(|l| l.contains("MASKED")),
            "the ledger must not be where a masked regression is reported: {report:?}"
        );
    }

    #[test]
    fn a_refused_record_names_the_replicate_campaign_in_its_own_message() {
        // the motivating cell: a bar recorded at the 1.114 replicate median
        // meets a lucky 0.974 draw; under a published x1.10 spread the
        // record floor is about 1.003, and the operator's whole path from
        // refusal to a legitimate record -- the scoped command, the spread,
        // both values -- must live in the alert itself
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[("echo", "heavy", &[("ratio_p50", 1.114)])],
        );
        let measured = vec![measured_cell("echo", "heavy", &[("ratio_p50", 0.974)])];
        let table = spread_table(&[("echo.ratio_p50", 1.10)]);
        let plan = plan_record(
            Some(existing),
            RecordMode::SingleCell,
            "dev-linux",
            "v0.12.4",
            &measured,
            &table,
        );
        assert_eq!(plan.spread_refusals(), 1);
        assert_eq!(
            plan.file.cell(&CellId::new("echo", "heavy")).unwrap()["ratio_p50"],
            1.114,
            "the file to be written must not carry the refused value"
        );
        let report = plan.report("dev-linux.toml");
        let alert = report
            .alerts
            .iter()
            .find(|line| line.contains("RECORD REFUSED"))
            .expect("a refusal must produce its own alert line");
        for needle in [
            "RECORD REFUSED [echo.heavy] ratio_p50:",
            "task bench -- --scenario echo --fixture heavy --class dev-linux",
            "(x headroom 1.1)",
            "record floor 1.0026",
            "1.1140",
            "0.9740",
            "quiet host",
            "replicate median",
            "hand-edit ratio_p50 under [echo.heavy] in dev-linux.toml",
            "will refuse the median the same way, by design",
        ] {
            assert!(
                alert.contains(needle),
                "the refusal alert must carry {needle:?}: {alert}"
            );
        }
        assert!(
            report.alerts.iter().any(|l| l.contains("RECORD REFUSAL:")),
            "a trailing summary must count the refusals: {:?}",
            report.alerts
        );
        assert!(
            !report.info.iter().any(|l| l.contains("REFUSED")),
            "the ledger is not where a refusal is reported: {:?}",
            report.info
        );
    }

    #[test]
    fn a_signed_delta_bar_stays_above_the_value_it_was_recorded_from() {
        // the bug a proportional factor has on a signed metric: recorded
        // -0.20 x 1.25 = -0.25, a bar tighter than the recorded value, so
        // the very measurement just recorded breaches its own bar
        let recorded = metrics(&[("paired_delta_p99_ms", -0.20)]);
        let same = metrics(&[("paired_delta_p99_ms", -0.20)]);
        assert!(
            gate_cell("echo", "minimal", &same, &recorded, "controlled-linux").is_empty(),
            "a re-measurement equal to the recorded value must never breach"
        );

        let worse = metrics(&[("paired_delta_p99_ms", 0.40)]);
        assert_eq!(
            gate_cell("echo", "minimal", &worse, &recorded, "controlled-linux").len(),
            1,
            "view turning 0.6ms slower than nvim must still breach"
        );
    }

    #[test]
    fn a_signed_delta_bar_matches_the_proportional_one_where_both_are_valid() {
        // the signed policy is the same allowance measured off the
        // magnitude, so it must not silently retune the positive range the
        // recorded baselines actually live in
        let headroom = Headroom::Signed {
            factor: RATIO_HEADROOM,
            floor: SIGNED_DELTA_FLOOR_MS,
        };
        let recorded = 8.676_833_f64;
        assert!(
            (headroom.bar(recorded) - recorded * RATIO_HEADROOM).abs() < 1e-9,
            "a value large enough for the factor to clear the floor must gate identically"
        );
    }

    #[test]
    fn a_signed_delta_ratchets_down_across_zero() {
        // view overtaking nvim is the improvement this metric exists to
        // report; refusing to record it tells the operator the opposite
        let existing = metrics(&[("paired_delta_p99_ms", 0.45)]);
        let measured = metrics(&[("paired_delta_p99_ms", -0.30)]);
        let (cell, outcomes) = ratchet_cell(
            Some(&existing),
            &measured,
            "echo",
            true,
            &HeadroomTable::new(),
        );
        assert_eq!(cell.get("paired_delta_p99_ms"), Some(&-0.30));
        assert!(
            matches!(
                outcome_for(&outcomes, "paired_delta_p99_ms"),
                RatchetOutcome::Improved { new, .. } if (*new + 0.30).abs() < 1e-9
            ),
            "expected an Improved outcome, got {outcomes:?}"
        );
    }

    #[test]
    fn a_non_finite_measurement_is_rejected_rather_than_recorded() {
        let measured = metrics(&[("view_p99_ms", f64::NAN)]);
        let (cell, outcomes) = ratchet_cell(None, &measured, "echo", false, &HeadroomTable::new());
        assert!(
            cell.is_empty(),
            "a NaN must not be written as a fresh bar: {cell:?}"
        );
        assert!(
            matches!(
                outcome_for(&outcomes, "view_p99_ms"),
                RatchetOutcome::Rejected { .. }
            ),
            "expected a Rejected outcome, got {outcomes:?}"
        );
    }

    #[test]
    fn a_rejected_measurement_keeps_the_bar_it_could_not_replace() {
        let existing = metrics(&[("view_p99_ms", 2.0)]);
        let measured = metrics(&[("view_p99_ms", f64::INFINITY)]);
        let (cell, _) = ratchet_cell(
            Some(&existing),
            &measured,
            "echo",
            false,
            &HeadroomTable::new(),
        );
        assert_eq!(
            cell.get("view_p99_ms"),
            Some(&2.0),
            "rejecting a measurement must not also drop the recorded bar"
        );
    }

    #[test]
    fn ratchet_does_not_lower_a_bar_to_a_nonpositive_measurement() {
        // a 0.0 (or negative) measurement would install a 0.0 bar, and the
        // gate bar 0.0 * headroom = 0.0 breaches on every later positive
        // measurement forever; a metric that cannot physically be <= 0 must
        // never ratchet the bar there.
        let existing = metrics(&[("view_p99_ms", 2.0)]);
        let (cell, _) = ratchet_cell(
            Some(&existing),
            &metrics(&[("view_p99_ms", 0.0)]),
            "echo",
            false,
            &HeadroomTable::new(),
        );
        assert_eq!(
            cell["view_p99_ms"], 2.0,
            "a 0.0 measurement must not install a 0.0 bar"
        );
    }

    #[test]
    fn ratchet_does_not_lower_a_bar_to_a_nonfinite_measurement() {
        let existing = metrics(&[("view_p99_ms", 2.0)]);
        let (cell, _) = ratchet_cell(
            Some(&existing),
            &metrics(&[("view_p99_ms", f64::NAN)]),
            "echo",
            false,
            &HeadroomTable::new(),
        );
        assert_eq!(
            cell["view_p99_ms"], 2.0,
            "a non-finite measurement must not disturb the recorded bar"
        );
    }

    #[test]
    fn ratchet_flags_masked_regression_for_a_tail_metric_on_a_controlled_class() {
        // ratio_p99 is gated ONLY on controlled classes; there a held-worse
        // value that would breach must be flagged, exactly opposite to the
        // shared-class case tested above.
        let existing = metrics(&[("ratio_p99", 1.0)]);
        let (_cell, outcomes) = ratchet_cell(
            Some(&existing),
            &metrics(&[("ratio_p99", 1.40)]),
            "echo",
            true,
            &HeadroomTable::new(),
        );
        assert_eq!(
            outcome_for(&outcomes, "ratio_p99"),
            &RatchetOutcome::Held {
                metric: "ratio_p99".to_string(),
                recorded: 1.0,
                measured: 1.40,
                regression_masked: true,
            }
        );
    }

    #[test]
    fn plan_record_derives_controlled_from_the_class_for_the_masked_flag() {
        let existing = baseline_with(
            "v0.12.4",
            "controlled-linux",
            &[("echo", "minimal", &[("ratio_p99", 1.0)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p99", 1.40)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "controlled-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert_eq!(
            plan.masked_regressions(),
            1,
            "a controlled class must flag a tail-metric regression a shared class would not"
        );
    }

    #[test]
    fn full_matrix_record_holds_a_bar_the_measurement_did_not_beat() {
        let existing = baseline_with(
            "v0.12.4",
            "dev-linux",
            &[("echo", "minimal", &[("ratio_p50", 1.20)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.35)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert_eq!(
            plan.file.cell(&CellId::new("echo", "minimal")).unwrap()["ratio_p50"],
            1.20,
            "a worse full-matrix remeasure must hold the better recorded bar"
        );
    }

    #[test]
    fn full_matrix_record_ignores_a_class_mismatched_baseline() {
        let existing = baseline_with(
            "v0.12.4",
            "controlled-linux",
            &[("echo", "minimal", &[("ratio_p50", 0.5)])],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::FullMatrix,
            "dev-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert_eq!(
            plan.file.cell(&CellId::new("echo", "minimal")).unwrap()["ratio_p50"],
            1.20
        );
        assert_eq!(plan.file.machine_class, "dev-linux");
        assert!(
            plan.reset_reason.is_some(),
            "a class-mismatched baseline is not a ratchet reference"
        );
    }

    #[test]
    fn single_cell_record_against_an_incomparable_existing_records_fresh_not_a_stale_clone() {
        // the caller normally errors before here, but plan_record must not
        // silently save the old pin's cells relabeled under the new run: the
        // saved file must carry this run's pin and drop cells measured under
        // the old engine.
        let existing = baseline_with(
            "v0.12.3",
            "dev-linux",
            &[
                ("echo", "minimal", &[("ratio_p50", 0.5)]),
                ("scroll", "minimal", &[("ratio_p50", 1.7)]),
            ],
        );
        let measured = vec![measured_cell("echo", "minimal", &[("ratio_p50", 1.20)])];
        let plan = plan_record(
            Some(existing),
            RecordMode::SingleCell,
            "dev-linux",
            "v0.12.4",
            &measured,
            &HeadroomTable::new(),
        );
        assert_eq!(
            plan.file.engine_pin, "v0.12.4",
            "the saved file must carry the run's pin, not the drifted one"
        );
        assert!(
            plan.file.cell(&CellId::new("scroll", "minimal")).is_none(),
            "cells measured under the old pin must not survive relabeled under the new one"
        );
        assert_eq!(
            plan.file.cell(&CellId::new("echo", "minimal")).unwrap()["ratio_p50"],
            1.20,
            "the fresh measurement, not the drifted 0.5, must be recorded"
        );
        assert!(plan.reset_reason.is_some());
    }
}
