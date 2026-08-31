//! One replicate campaign: the draws it kept, the seats they propose, and
//! the headroom factor that follows from them.
//!
//! Re-sizing a factor or re-seating a cell takes N gated replicates, a
//! pre-registered load exclusion, a median per cell and a factor sized so
//! it covers the band's worst draw, the band's own 2x half-width, and the
//! worst draw again once the seat ratchets as far down as the band admits.
//! Every ingredient is a number the measuring tool already holds, so the
//! arithmetic lives here rather than in a hand-typed sidecar comment, and
//! the walk that re-checks a published factor against its draws
//! ([`spread_violations`]) is the same code that sizes one.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::baselines::{gate_headroom, CellId, DrawSet, Headroom, MeasuredCell};

#[cfg(test)]
mod tests;

/// Errors a campaign refuses with.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CampaignError {
    #[error(
        "campaign wanted {target} included replicate(s) and reached {included} in {runs} run(s), \
         the cap this campaign may spend; loads seen: {loads}. Every run past an exclusion is a \
         replacement, so a host this busy cannot produce a band -- re-run when it is quiet{refusals}"
    )]
    ReplacementCap {
        target: usize,
        included: usize,
        runs: usize,
        loads: String,
        refusals: String,
    },
    #[error(
        "{key}: the factor sized from this campaign's own draws does not survive the walk that \
         re-checks it, which is a defect in the sizing rather than in the draws: {violations}"
    )]
    Unsizable { key: String, violations: String },
    #[error("campaign kept no replicate that measured {metric} on {scenario}.{fixture}")]
    NoDraws {
        scenario: String,
        fixture: String,
        metric: String,
    },
    #[error(
        "{metric} carries no gate policy, so a campaign cannot size a factor for it; a row \
         producing a metric name the policy rule does not classify is refused where it is \
         produced, so reaching here means that refusal was bypassed"
    )]
    Unclassified { metric: String },
}

/// The estimators one cell-metric's draws resolve to.
///
/// The median is the seat a campaign proposes; the half-width is half the
/// band's full span, so `median + 2 * half_width` is the rule the sidecars
/// state as "2x half-width".
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct DrawStats {
    pub median: f64,
    pub low: f64,
    pub high: f64,
    pub half_width: f64,
}

impl DrawStats {
    /// The estimators over `values`, or `None` for an empty band.
    #[must_use]
    pub fn of(values: &[f64]) -> Option<Self> {
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let (&low, &high) = (sorted.first()?, sorted.last()?);
        let mid = sorted.len() / 2;
        let median = if sorted.len().is_multiple_of(2) {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        };
        Some(Self {
            median,
            low,
            high,
            half_width: (high - low) / 2.0,
        })
    }

    /// The half-width as a fraction of the median, which is how a sidecar
    /// comment states a band's width.
    #[must_use]
    pub fn half_width_fraction(self) -> f64 {
        if self.median == 0.0 {
            return 0.0;
        }
        self.half_width / self.median
    }

    /// The value the 2x half-width rule asks a bar to cover.
    #[must_use]
    pub fn two_half_widths(self) -> f64 {
        self.median + 2.0 * self.half_width
    }
}

/// A factor sized against one band, with what each leg of the rule asked.
///
/// The three legs are the three claims a published factor makes, and the
/// binding one is whichever asks most. `ratcheted_seat` sizes against the
/// band's lowest draw rather than against wherever the ratchet actually
/// lands, because the ratchet can reach no lower than that: a factor that
/// clears the worst draw from the lowest seat clears it from every seat.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct SizedFactor {
    pub stats: DrawStats,
    pub seat: f64,
    /// Smallest factor whose bar covers the band's worst draw.
    pub worst_draw: f64,
    /// Smallest factor whose bar covers `median + 2 * half_width`.
    pub two_half_widths: f64,
    /// Smallest factor that still covers the worst draw from a seat
    /// ratcheted to the band's lowest draw.
    pub ratcheted_seat: f64,
    /// The factor to publish: the binding leg, rounded up to the two
    /// decimals a sidecar states a factor in.
    pub factor: f64,
}

impl SizedFactor {
    /// Sizes a factor for `seat` under `shape`'s policy from `values`.
    #[must_use]
    pub fn size(shape: Headroom, seat: f64, values: &[f64]) -> Option<Self> {
        let stats = DrawStats::of(values)?;
        let worst_draw = factor_clearing(shape, seat, stats.high);
        let two_half_widths = factor_clearing(shape, seat, stats.two_half_widths());
        let ratcheted_seat = factor_clearing(shape, stats.low, stats.high);
        let required = worst_draw.max(two_half_widths).max(ratcheted_seat);
        Some(Self {
            stats,
            seat,
            worst_draw,
            two_half_widths,
            ratcheted_seat,
            factor: publishable(required),
        })
    }

    /// The leg that governs the factor, named as a sidecar comment names it.
    #[must_use]
    pub fn binding(self) -> (&'static str, f64) {
        let legs = [
            ("worst draw", self.worst_draw),
            ("2x half-width", self.two_half_widths),
            ("worst draw over a ratcheted seat", self.ratcheted_seat),
        ];
        legs.into_iter()
            .fold(legs[0], |best, leg| if leg.1 > best.1 { leg } else { best })
    }

    /// The fraction by which `factor` clears `leg`.
    #[must_use]
    pub fn margin(factor: f64, leg: f64) -> f64 {
        if leg == 0.0 {
            return 0.0;
        }
        factor / leg - 1.0
    }
}

/// The smallest factor under `shape` whose bar over `from` reaches `target`.
///
/// A proportional bar is `from * factor`, so the factor is the quotient. A
/// signed bar adds `max(|from| * (factor - 1), floor)`, so a target already
/// inside the floor's own allowance needs no factor at all, and a `from` at
/// zero can never be moved by one.
fn factor_clearing(shape: Headroom, from: f64, target: f64) -> f64 {
    match shape {
        Headroom::Proportional(_) => {
            if from > 0.0 {
                target / from
            } else {
                1.0
            }
        }
        Headroom::Signed { floor, .. } => {
            let needed = target - from;
            if needed <= floor || from == 0.0 {
                1.0
            } else {
                1.0 + needed / from.abs()
            }
        }
    }
}

/// `required` rounded up to the two decimals a sidecar publishes, never at
/// or below 1.0: a factor there bars at or under the value that produced it
/// and the sidecar loader refuses it.
fn publishable(required: f64) -> f64 {
    ((required * 100.0).ceil() / 100.0).max(1.01)
}

/// The cell and metric a draws key names, or `None` where the key is not
/// the `scenario.fixture.metric` a draw is always a reading of.
#[must_use]
pub fn draws_cell(key: &str) -> Option<(CellId, &str)> {
    match key.split('.').collect::<Vec<_>>()[..] {
        [scenario, fixture, metric] => Some((CellId::new(scenario, fixture), metric)),
        _ => None,
    }
}

/// Whether a draws key at cell scope falls inside `entry`'s scope, by the
/// same three levels [`crate::baselines::headroom_for`] resolves.
#[must_use]
pub fn scope_covers(entry: &str, key: &str) -> bool {
    let (Some((cell, metric)), parts) = (draws_cell(key), entry.split('.').count()) else {
        return false;
    };
    match parts {
        1 => entry == metric,
        2 => entry == format!("{}.{metric}", cell.scenario),
        _ => entry == key,
    }
}

/// Where `headroom` fails the arithmetic its own campaign publishes: one
/// message per violated leg of the published-spread rule.
///
/// The worst-excursion leg can never fire alone -- the median is never
/// below the lowest draw, so `median + 2 * half_width` is never below the
/// worst draw -- and it is reported separately anyway so a failure names
/// which claim the factor broke rather than only the wider of the two.
#[must_use]
pub fn spread_violations(key: &str, headroom: Headroom, draws: &DrawSet) -> Vec<String> {
    let Some(stats) = DrawStats::of(&draws.values) else {
        return vec![format!(
            "{key} publishes no draws to check its factor against"
        )];
    };
    let recorded = draws.recorded;
    let bar = headroom.bar(recorded);
    // a record ratchets the seat down to the lowest draw the published
    // band still admits; one below the floor is refused rather than
    // seated, so it never becomes the value a later bar is built from
    let floor = headroom.record_floor(recorded);
    let seat = draws
        .values
        .iter()
        .copied()
        .filter(|value| *value > floor)
        .fold(recorded, f64::min);
    let mut out = Vec::new();
    if bar < stats.high {
        out.push(format!(
            "{key}: {recorded} {headroom} bars at {bar}, under the campaign's own worst draw {}",
            stats.high
        ));
    }
    if bar < stats.two_half_widths() {
        out.push(format!(
            "{key}: {recorded} {headroom} bars at {bar}, under the 2x half-width rule's {}",
            stats.two_half_widths()
        ));
    }
    if headroom.bar(seat) < stats.high {
        out.push(format!(
            "{key}: a seat ratcheted to {seat} bars at {}, under the worst draw {}",
            headroom.bar(seat),
            stats.high
        ));
    }
    out
}

/// One cell-metric's readings as the walk over the replicates collects
/// them, before they are sized.
#[derive(Debug, Default)]
struct Band {
    values: Vec<f64>,
    /// The draws the load exclusion removed, as `(value, load)`.
    excluded: Vec<(f64, f64)>,
}

/// One replicate's reading: the load the campaign registered before it ran,
/// and either the cells it measured or the reason it withheld them.
#[derive(Debug, Clone)]
pub struct ReplicateDraw {
    pub load: Option<f64>,
    /// The null-pair deviations bracketing this replicate's cells, once
    /// both have been taken. A replicate the start bracket refused never
    /// reaches the second, so this stays `None` there.
    pub brackets: Option<(f64, f64)>,
    pub cells: Vec<MeasuredCell>,
    /// Set when the replicate refused its own measurement (a noisy
    /// calibration bracket, a row that withheld its number, a cell that
    /// failed outright); its cells are not a band's draws.
    pub refusal: Option<String>,
}

/// How one replicate ended, as the campaign's own log reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Included,
    /// The pre-registered load exclusion fired: the replicate measured, its
    /// draws are published as excluded, and another run replaces it.
    LoadExcluded,
    /// The replicate withheld its own measurement and another replaces it.
    Refused,
}

/// One replicate the campaign ran, with what became of it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Replicate {
    pub load: Option<f64>,
    /// See [`ReplicateDraw::brackets`].
    pub brackets: Option<(f64, f64)>,
    pub cells: Vec<MeasuredCell>,
    pub verdict: Verdict,
    pub refusal: Option<String>,
    /// How long this replicate took, which is what makes the campaign's
    /// own projection of its remaining cost a measurement rather than a
    /// guess.
    pub elapsed: Duration,
}

/// Where a campaign stands as one replicate lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// 1-based index of the run that just landed, which advances on every
    /// replicate including the ones that are replaced.
    pub run: usize,
    /// How much of the band is filled, which advances only on an included
    /// replicate.
    pub included: usize,
    pub target: usize,
    pub cap: usize,
}

/// Every replicate one campaign ran, included or not.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Campaign {
    pub target: usize,
    pub replicates: Vec<Replicate>,
}

impl Campaign {
    /// Runs replicates until `target` of them are included, replacing every
    /// excluded or refused one, and refusing past `cap` total runs.
    ///
    /// `replicate` takes the 1-based run index and returns what that run
    /// measured; `report` is handed each replicate as it lands together
    /// with its own run index and the included count so far, so a
    /// campaign's log names an exclusion at the moment it happens rather
    /// than only in the summary. The two numbers are reported apart because
    /// they answer different questions -- how much of the cap is spent
    /// versus how much of the band is filled -- and only the first advances
    /// on an exclusion.
    ///
    /// # Errors
    ///
    /// [`CampaignError::ReplacementCap`] when `cap` runs did not produce
    /// `target` included replicates, naming every load seen.
    pub fn collect(
        target: usize,
        cap: usize,
        max_load: f64,
        mut replicate: impl FnMut(usize) -> ReplicateDraw,
        mut report: impl FnMut(&Replicate, Progress),
    ) -> Result<Self, CampaignError> {
        let mut replicates: Vec<Replicate> = Vec::new();
        let mut included = 0usize;
        while included < target && replicates.len() < cap {
            let run = replicates.len() + 1;
            let started = Instant::now();
            let draw = replicate(run);
            let verdict = if draw.refusal.is_some() {
                Verdict::Refused
            } else if draw.load.is_some_and(|load| load > max_load) {
                Verdict::LoadExcluded
            } else {
                included += 1;
                Verdict::Included
            };
            let landed = Replicate {
                load: draw.load,
                brackets: draw.brackets,
                cells: draw.cells,
                verdict,
                refusal: draw.refusal,
                elapsed: started.elapsed(),
            };
            report(
                &landed,
                Progress {
                    run,
                    included,
                    target,
                    cap,
                },
            );
            replicates.push(landed);
        }
        if included < target {
            return Err(CampaignError::ReplacementCap {
                target,
                included,
                runs: replicates.len(),
                loads: loads_seen(&replicates),
                refusals: refusals_seen(&replicates),
            });
        }
        Ok(Self { target, replicates })
    }

    /// The replicates whose draws are the band.
    pub fn included(&self) -> impl Iterator<Item = &Replicate> {
        self.replicates
            .iter()
            .filter(|r| r.verdict == Verdict::Included)
    }

    /// One proposal per cell-metric the included replicates measured, each
    /// carrying the factor its own draws size and the sidecar key it would
    /// be published under.
    ///
    /// A statistic several fixtures of one scenario measured is published
    /// at scenario scope with the worse fixture governing, which is how a
    /// hand campaign sizes one; a statistic only one fixture measured is
    /// published at that cell's own scope.
    ///
    /// # Errors
    ///
    /// [`CampaignError::NoDraws`] where a metric reached the walk with no
    /// included reading, and [`CampaignError::Unsizable`] where the sized
    /// factor does not survive [`spread_violations`] -- a defect in the
    /// sizing, refused rather than written.
    pub fn proposals(&self) -> Result<Vec<Proposal>, CampaignError> {
        let mut bands: BTreeMap<(CellId, String), Band> = BTreeMap::new();
        for replicate in &self.replicates {
            if replicate.verdict == Verdict::Refused {
                continue;
            }
            for cell in &replicate.cells {
                for (metric, &value) in &cell.metrics {
                    let band = bands.entry((cell.id.clone(), metric.clone())).or_default();
                    match replicate.verdict {
                        Verdict::Included => band.values.push(value),
                        // an excluded draw is published beside the band
                        // rather than dropped: what the exclusion removed
                        // is part of the campaign's own evidence
                        _ => band
                            .excluded
                            .push((value, replicate.load.unwrap_or(f64::NAN))),
                    }
                }
            }
        }

        // the widest scope this campaign's own cells justify: a statistic
        // several fixtures of one scenario measured is one entry governing
        // them all, which is how a hand campaign publishes one
        let mut fixtures: BTreeMap<(&str, &str), usize> = BTreeMap::new();
        for (cell, metric) in bands.keys() {
            *fixtures
                .entry((cell.scenario.as_str(), metric.as_str()))
                .or_default() += 1;
        }
        let keys: BTreeMap<(CellId, String), String> = bands
            .keys()
            .map(|(cell, metric)| {
                let key = if fixtures
                    .get(&(cell.scenario.as_str(), metric.as_str()))
                    .is_some_and(|count| *count > 1)
                {
                    format!("{}.{metric}", cell.scenario)
                } else {
                    format!("{}.{}.{metric}", cell.scenario, cell.fixture)
                };
                ((cell.clone(), metric.clone()), key)
            })
            .collect();

        let mut sized = Vec::new();
        for ((cell, metric), Band { values, excluded }) in bands {
            let shape =
                gate_headroom(&metric, true).ok_or_else(|| CampaignError::Unclassified {
                    metric: metric.clone(),
                })?;
            let stats = DrawStats::of(&values).ok_or_else(|| CampaignError::NoDraws {
                scenario: cell.scenario.clone(),
                fixture: cell.fixture.clone(),
                metric: metric.clone(),
            })?;
            let factor =
                SizedFactor::size(shape, stats.median, &values).ok_or(CampaignError::NoDraws {
                    scenario: cell.scenario.clone(),
                    fixture: cell.fixture.clone(),
                    metric: metric.clone(),
                })?;
            // ascending, as every committed sidecar lists a band: the
            // arithmetic sorts anyway, and a reviewer reads the band's
            // edges off the ends. Which replicate produced which draw is
            // in the per-replicate provenance lines above, not here
            let mut values = values;
            values.sort_by(f64::total_cmp);
            let key = keys
                .get(&(cell.clone(), metric.clone()))
                .cloned()
                .unwrap_or_else(|| format!("{}.{}.{metric}", cell.scenario, cell.fixture));
            sized.push(Proposal {
                key,
                published: factor.factor,
                shape,
                cell,
                metric,
                sized: factor,
                values,
                excluded,
            });
        }

        // one key, one factor: where several cells share a key the fixture
        // that asks most governs, so the entry covers every band under it
        let mut published: BTreeMap<String, f64> = BTreeMap::new();
        for proposal in &sized {
            let held = published.entry(proposal.key.clone()).or_insert(0.0);
            *held = held.max(proposal.sized.factor);
        }
        for proposal in &mut sized {
            proposal.published = published
                .get(&proposal.key)
                .copied()
                .unwrap_or(proposal.sized.factor);
        }

        // the sizing is only worth publishing if the walk that re-checks a
        // published factor agrees with it, so every proposal is put through
        // that walk here rather than one CI bench leg later
        for proposal in &sized {
            let violations = spread_violations(
                &proposal.key,
                crate::baselines::resized_headroom(proposal.shape, proposal.published),
                &DrawSet {
                    recorded: proposal.sized.seat,
                    values: proposal.values.clone(),
                },
            );
            if !violations.is_empty() {
                return Err(CampaignError::Unsizable {
                    key: proposal.key.clone(),
                    violations: violations.join("; "),
                });
            }
        }
        Ok(sized)
    }
}

/// Every load the campaign registered, in run order, for a refusal that
/// must say what the host was doing rather than only that it was busy.
fn loads_seen(replicates: &[Replicate]) -> String {
    replicates
        .iter()
        .map(|r| match r.load {
            Some(load) => format!("{load:.2}"),
            None => "unavailable".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every refusal reason the campaign collected, or an empty string when
/// none refused (an exclusion-only campaign must not read as if a row
/// withheld a number).
fn refusals_seen(replicates: &[Replicate]) -> String {
    let refusals: Vec<String> = replicates
        .iter()
        .filter_map(|r| r.refusal.clone())
        .collect();
    if refusals.is_empty() {
        String::new()
    } else {
        format!(". Refused replicate(s): {}", refusals.join("; "))
    }
}

/// One cell-metric's campaign result: the draws, the seat they propose and
/// the factor sized from them.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Proposal {
    pub cell: CellId,
    pub metric: String,
    /// The sidecar key this proposal's factor is published under.
    pub key: String,
    /// The factor the key publishes, which is the worst sibling's where
    /// several cells share the key.
    pub published: f64,
    /// The policy shape the metric's kind demands.
    pub shape: Headroom,
    pub sized: SizedFactor,
    /// The included draws, ascending.
    pub values: Vec<f64>,
    /// The draws the load exclusion removed, as `(value, load)`.
    pub excluded: Vec<(f64, f64)>,
}

/// What a campaign knows about where its numbers came from.
#[derive(Debug, Clone)]
pub struct Provenance {
    pub class: String,
    pub engine_pin: String,
    pub date: String,
    /// The commit measured, where the tool could read one.
    pub commit: Option<String>,
    pub max_load: f64,
    pub samples: usize,
    pub warmup: usize,
    pub trials: usize,
}

/// The campaign file's text: a sidecar the characterization walk can parse
/// as it stands, with the estimators, the binding leg and the margins that
/// sized every factor in the comment above it.
///
/// Every refusal a campaign can produce is raised by
/// [`Campaign::proposals`], which is what builds the arithmetic this
/// renders; by the time proposals exist there is nothing left to fail on,
/// so this returns the text itself rather than a `Result` whose error arm
/// no caller can reach.
#[must_use]
pub fn render(campaign: &Campaign, provenance: &Provenance, proposals: &[Proposal]) -> String {
    let runs = campaign.replicates.len();
    let included = campaign.included().count();
    let excluded = campaign
        .replicates
        .iter()
        .filter(|r| r.verdict == Verdict::LoadExcluded)
        .count();
    let refused = campaign
        .replicates
        .iter()
        .filter(|r| r.verdict == Verdict::Refused)
        .count();
    // an unreadable load is not a quiet host: the exclusion could not judge
    // that replicate at all, and a provenance block that stated the rule
    // without stating where it could not run would read as though every
    // included draw had passed it
    let unjudged = campaign.included().filter(|r| r.load.is_none()).count();
    let exclusion = if unjudged == 0 {
        String::new()
    } else {
        format!(
            "\n#   {unjudged} included replicate(s) reported no load reading, so the exclusion \
             above could not be applied to them"
        )
    };
    let mut out = String::new();
    let class = &provenance.class;
    let _ = write!(
        out,
        "\
# A CAMPAIGN PROPOSAL, not a recorded bar, and nothing reads this file.
# `bench --campaign` measured the replicates below on this host and did the
# arithmetic; what remains is review and commit, where the diff decides.
# Committing it means: each [headroom] entry and the comment above it moves
# into {class}.headroom.toml, each [draws.\"...\"] table moves beside it, and
# each cell's metric in {class}.toml is set to the `recorded` value its
# draws table names -- the seat the factor is sized against. A factor
# committed without its draws is a claim nothing can re-check, and the
# characterization walk fails on it.
#
# Provenance, which the sidecar comment must keep once this is committed:
#   class {class}, engine pin {pin}, {date}{commit}
#   {os}/{arch}; the host's own hardware model is not something the tool
#   reads, so name it by hand beside this line
#   {runs} replicate(s) run: {included} included, {excluded} load-excluded, {refused} refused
#   pre-registered load exclusion: pre-replicate 1-min load > {max_load}{exclusion}
#   protocol per replicate: {samples} samples, {warmup} warmup, {trials} trials,
#   one unchanged binary pair throughout
{ledger}machine_class = \"{class}\"

[headroom]",
        ledger = replicate_ledger(campaign),
        pin = provenance.engine_pin,
        date = provenance.date,
        commit = provenance
            .commit
            .as_ref()
            .map_or_else(String::new, |c| format!(", at {c}")),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        max_load = provenance.max_load,
        samples = provenance.samples,
        warmup = provenance.warmup,
        trials = provenance.trials,
    );

    let mut keys: BTreeMap<&str, Vec<&Proposal>> = BTreeMap::new();
    for proposal in proposals {
        keys.entry(&proposal.key).or_default().push(proposal);
    }
    for (key, members) in &keys {
        for member in members {
            let stats = member.sized.stats;
            let (leg, asks) = member.sized.binding();
            let _ = write!(
                out,
                "\n# {}/{} {}: median {:.4}, half-width {:.4} ({:.2}%), worst {:.4}, over {} \
                 draw(s)\n#   legs: worst draw {:.4}, 2x half-width {:.4}, ratcheted seat {:.4}; \
                 {leg} binds at {asks:.4}\n",
                member.cell.scenario,
                member.cell.fixture,
                member.metric,
                stats.median,
                stats.half_width,
                stats.half_width_fraction() * 100.0,
                stats.high,
                member.values.len(),
                member.sized.worst_draw,
                member.sized.two_half_widths,
                member.sized.ratcheted_seat,
            );
            if !member.excluded.is_empty() {
                let listed = member
                    .excluded
                    .iter()
                    .map(|(value, load)| format!("{value:.4} (load {load:.2})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "#   excluded, not dropped: {listed}");
            }
        }
        let published = members.first().map_or(1.01, |m| m.published);
        let margins = members
            .iter()
            .map(|m| {
                let (leg, asks) = m.sized.binding();
                format!(
                    "{} {leg} by {:.2}%",
                    m.cell.fixture,
                    SizedFactor::margin(published, asks) * 100.0
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "# {published} clears {margins}. A draw that turns any margin negative is a re-sizing, \
             not a rounding."
        );
        if let Some(default) = members
            .first()
            .and_then(|m| default_factor(m.shape))
            .filter(|default| published > *default)
        {
            let _ = writeln!(
                out,
                "# WIDER than the compiled default {default} for this statistic's kind: committing \
                 it loosens the gate rather than tightening it, so it needs the same scrutiny a \
                 shortfall does."
            );
        }
        let _ = writeln!(out, "\"{key}\" = {published}");
    }

    for proposal in proposals {
        let _ = write!(
            out,
            "\n[draws.\"{}.{}.{}\"]\n# the seat this campaign proposes for the cell: set the \
             metric in {class}.toml to it\nrecorded = {}\nvalues = [\n",
            proposal.cell.scenario, proposal.cell.fixture, proposal.metric, proposal.sized.seat,
        );
        for chunk in proposal.values.chunks(3) {
            let row = chunk
                .iter()
                .map(|value| format!("{value},"))
                .collect::<Vec<_>>()
                .join(" ");
            let _ = writeln!(out, "  {row}");
        }
        let _ = writeln!(out, "]");
    }
    out
}

/// One line per replicate the campaign ran: what the host was doing before
/// it, what its calibration brackets read, and what became of it.
///
/// The brackets are the stronger quiet signal of the two a replicate takes
/// -- the load is ambient, the null pair is this host's own pairing noise
/// measured by the same machinery the draws come from -- and a bracket
/// computed, used for a pass/fail and dropped is a measurement nobody wrote
/// down. A reviewer reading a committed factor can then tell an included
/// replicate whose brackets sat at 1.02 from one that scraped the floor.
fn replicate_ledger(campaign: &Campaign) -> String {
    let mut out = String::from("#\n#   per replicate, in run order:\n");
    for (index, replicate) in campaign.replicates.iter().enumerate() {
        let load = replicate
            .load
            .map_or_else(|| "unreadable".to_string(), |load| format!("{load:.2}"));
        let brackets = replicate.brackets.map_or_else(
            || "not taken".to_string(),
            |(start, end)| format!("{start:.4}/{end:.4}"),
        );
        let verdict = match replicate.verdict {
            Verdict::Included => "included",
            Verdict::LoadExcluded => "load-excluded",
            Verdict::Refused => "refused",
        };
        let _ = writeln!(
            out,
            "#     {}: load {load}, null-pair brackets {brackets}, {verdict}",
            index + 1
        );
        // the draw beside the conditions it was taken under: the `values`
        // arrays below are sorted into bands, which is the shape the walk
        // reads and every committed sidecar states a spread in, so run order
        // is the one thing they cannot carry -- and pairing a draw back to
        // its load is exactly what auditing a near-threshold replicate needs
        for cell in &replicate.cells {
            let readings = cell
                .metrics
                .iter()
                .map(|(metric, value)| format!("{metric} {value:.4}"))
                .collect::<Vec<_>>()
                .join("  ");
            let _ = writeln!(
                out,
                "#        {}/{}: {readings}",
                cell.id.scenario, cell.id.fixture
            );
        }
    }
    out
}

/// The factor the compiled default publishes for `shape`, against which a
/// proposal that would loosen the gate is named.
fn default_factor(shape: Headroom) -> Option<f64> {
    match shape {
        Headroom::Proportional(factor) | Headroom::Signed { factor, .. } => Some(factor),
    }
}
