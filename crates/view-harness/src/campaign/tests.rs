#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::collections::BTreeMap;

use super::*;
use crate::baselines::{declared_headroom, load_draws, load_headroom, CellMetrics, RATIO_HEADROOM};

/// The included minimal draws of the 2026-08-30 dev-macos scroll campaign,
/// as its sidecar publishes them: the one campaign whose sizing was worked
/// by hand end to end, so it is the reference this arithmetic reproduces.
const MINIMAL: [f64; 9] = [
    2.2472809621131984,
    2.253427803653809,
    2.2629560726876936,
    2.26901825526434,
    2.272541802721265,
    2.2803571695994007,
    2.286816213214881,
    2.3042971025670593,
    2.3093864336309866,
];

/// The same campaign's heavy fixture, whose band the scenario-scoped key
/// must also survive.
const HEAVY: [f64; 8] = [
    2.281312637480817,
    2.3084768627114762,
    2.3154541332038194,
    2.3166218135929815,
    2.321867380902642,
    2.321914359358111,
    2.3259334006054493,
    2.3371097683786504,
];

fn cell(scenario: &str, fixture: &str, metric: &str, value: f64) -> MeasuredCell {
    let mut metrics = CellMetrics::new();
    metrics.insert(metric.to_string(), value);
    MeasuredCell {
        id: CellId::new(scenario, fixture),
        metrics,
    }
}

fn draw(load: f64, cells: Vec<MeasuredCell>) -> ReplicateDraw {
    ReplicateDraw {
        load: Some(load),
        cells,
        refusal: None,
    }
}

fn provenance() -> Provenance {
    Provenance {
        class: "dev-macos".to_string(),
        engine_pin: "v0.11.4".to_string(),
        date: "2026-08-30".to_string(),
        commit: Some("87ef37b".to_string()),
        max_load: 2.0,
        samples: 1000,
        warmup: 100,
        trials: 3,
    }
}

/// A campaign of one included replicate per listed draw, so a band worked
/// out by hand can be put through the whole pipeline.
fn campaign_over(bands: &[(&str, &str, &str, &[f64])]) -> Campaign {
    let replicates = bands.iter().map(|(_, _, _, values)| values.len()).max();
    let count = replicates.unwrap_or(0);
    let draws: Vec<ReplicateDraw> = (0..count)
        .map(|index| {
            let cells = bands
                .iter()
                .filter_map(|(scenario, fixture, metric, values)| {
                    values
                        .get(index)
                        .map(|value| cell(scenario, fixture, metric, *value))
                })
                .collect();
            draw(1.0, cells)
        })
        .collect();
    let mut queue = draws.into_iter();
    Campaign::collect(
        count,
        count * 2,
        2.0,
        |_| queue.next().expect("one draw per replicate"),
        |_, _| {},
    )
    .expect("a quiet synthetic campaign includes every replicate")
}

/// The 2026-08-30 dev-macos scroll campaign was sized by hand and its
/// sidecar states every number that sizing produced. The tool's arithmetic
/// is the same arithmetic or it is a second implementation, so it is put
/// that campaign's own draws and must return its own published answers.
#[test]
fn the_hand_sized_scroll_campaign_resizes_to_exactly_what_it_published() {
    let minimal = SizedFactor::size(
        Headroom::Proportional(RATIO_HEADROOM),
        DrawStats::of(&MINIMAL).unwrap().median,
        &MINIMAL,
    )
    .unwrap();
    let heavy = SizedFactor::size(
        Headroom::Proportional(RATIO_HEADROOM),
        DrawStats::of(&HEAVY).unwrap().median,
        &HEAVY,
    )
    .unwrap();

    let stats = minimal.stats;
    assert!(
        (stats.median - 2.272541802721265).abs() < 1e-12,
        "{stats:?}"
    );
    assert!((stats.half_width - 0.0311).abs() < 5e-5, "{stats:?}");
    assert!(
        (stats.half_width_fraction() * 100.0 - 1.37).abs() < 5e-3,
        "{stats:?}"
    );

    // the sidecar's own words: "it asks 1.0276 for minimal ... and 1.0245
    // for heavy, worse fixture governing"
    assert!(
        (minimal.ratcheted_seat - 1.0276).abs() < 5e-5,
        "{minimal:?}"
    );
    assert!((heavy.ratcheted_seat - 1.0245).abs() < 5e-5, "{heavy:?}");
    assert_eq!(
        minimal.binding().0,
        "worst draw over a ratcheted seat",
        "{minimal:?}"
    );
    assert!((minimal.factor - 1.03).abs() < 1e-12, "{minimal:?}");
    assert!((heavy.factor - 1.03).abs() < 1e-12, "{heavy:?}");

    // "1.03 clears minimal's binding leg by 0.23% and heavy's by 0.54%,
    // and minimal's 2x half-width leg by 0.26%"
    let pct = |leg| SizedFactor::margin(1.03, leg) * 100.0;
    assert!((pct(minimal.ratcheted_seat) - 0.23).abs() < 5e-3);
    assert!((pct(heavy.ratcheted_seat) - 0.54).abs() < 5e-3);
    assert!((pct(minimal.two_half_widths) - 0.26).abs() < 5e-3);
}

/// A statistic two fixtures of one scenario measured is published once, at
/// scenario scope, with the fixture that asks most governing -- the shape
/// the hand campaign chose and the shape the gate's own precedence reads.
#[test]
fn two_fixtures_of_one_scenario_publish_one_key_the_worse_fixture_governs() {
    let campaign = campaign_over(&[
        ("scroll", "minimal", "ratio_p50", &MINIMAL),
        ("scroll", "heavy", "ratio_p50", &HEAVY),
    ]);
    let proposals = campaign.proposals().unwrap();
    assert_eq!(proposals.len(), 2, "{proposals:?}");
    for proposal in &proposals {
        assert_eq!(proposal.key, "scroll.ratio_p50", "{proposal:?}");
        assert!((proposal.published - 1.03).abs() < 1e-12, "{proposal:?}");
    }

    let alone = campaign_over(&[("scroll", "minimal", "ratio_p50", &MINIMAL)])
        .proposals()
        .unwrap();
    assert_eq!(alone[0].key, "scroll.minimal.ratio_p50", "{alone:?}");
}

/// The pipeline's whole point is that the factor it publishes is the one
/// the characterization walk recomputes from the draws published beside it.
/// The emitted file is therefore parsed back with the walk's own loaders
/// and put through the walk's own arithmetic.
#[test]
fn the_emitted_file_survives_the_walk_that_recomputes_it() {
    let campaign = campaign_over(&[
        ("scroll", "minimal", "ratio_p50", &MINIMAL),
        ("scroll", "heavy", "ratio_p50", &HEAVY),
    ]);
    let proposals = campaign.proposals().unwrap();
    let text = render(&campaign, &provenance(), &proposals).unwrap();

    let dir = crate::fixture::scratch_root("campaign-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dev-macos.campaign.toml");
    std::fs::write(&path, &text).unwrap();

    let table = load_headroom(&path, "dev-macos").expect("the emitted file must load as a sidecar");
    let draws = load_draws(&path, "dev-macos").expect("the emitted file must carry its draws");
    assert_eq!(table.len(), 1, "{table:?}");
    assert_eq!(draws.len(), 2, "{draws:?}");

    for (key, set) in &draws {
        let (id, metric) = draws_cell(key).expect("an emitted draws key names one cell");
        let headroom =
            declared_headroom(&table, &id, metric).expect("every emitted draw supports a factor");
        assert_eq!(
            spread_violations(key, headroom, set),
            Vec::<String>::new(),
            "the emitted factor must survive its own draws"
        );
        assert!(
            table.keys().any(|entry| scope_covers(entry, key)),
            "every emitted draws key must fall inside an emitted factor's scope: {key}"
        );
    }
    assert_eq!(
        table.get("scroll.ratio_p50").copied(),
        Some(1.03),
        "{table:?}"
    );
    for cell in ["scroll.minimal.ratio_p50", "scroll.heavy.ratio_p50"] {
        assert!(text.contains(&format!("[draws.\"{cell}\"]")), "{text}");
    }
    // the estimators, the binding leg and the margins the hand campaign
    // stated in prose are what a committed sidecar comment must carry
    assert!(text.contains("median 2.2725"), "{text}");
    assert!(text.contains("half-width 0.0311 (1.37%)"), "{text}");
    assert!(
        text.contains("worst draw over a ratcheted seat binds at 1.0276"),
        "{text}"
    );
    assert!(
        text.contains("minimal worst draw over a ratcheted seat by 0.23%"),
        "{text}"
    );
    std::fs::remove_file(&path).unwrap();
}

/// An excluded replicate is replaced, and both the exclusion and the draw
/// it removed stay in the record: a campaign that silently dropped either
/// would publish a band nobody could audit.
#[test]
fn a_load_excluded_replicate_is_replaced_and_its_draw_still_published() {
    let loads = [1.0, 3.15, 1.1];
    let values = [2.25, 2.3003, 2.26];
    let mut reported: Vec<(Verdict, usize)> = Vec::new();
    let campaign = Campaign::collect(
        2,
        4,
        2.0,
        |run| {
            draw(
                loads[run - 1],
                vec![cell("scroll", "minimal", "ratio_p50", values[run - 1])],
            )
        },
        |replicate, included| reported.push((replicate.verdict.clone(), included)),
    )
    .unwrap();

    assert_eq!(campaign.replicates.len(), 3, "the exclusion is replaced");
    assert_eq!(campaign.included().count(), 2);
    assert_eq!(
        reported,
        vec![
            (Verdict::Included, 1),
            (Verdict::LoadExcluded, 1),
            (Verdict::Included, 2),
        ],
        "every replicate is reported as it lands"
    );

    let proposals = campaign.proposals().unwrap();
    assert_eq!(proposals[0].values, vec![2.25, 2.26]);
    assert_eq!(proposals[0].excluded, vec![(2.3003, 3.15)]);
}

/// A replicate that withholds its own measurement is replaced like an
/// excluded one, and its reason is carried to the refusal so a campaign
/// that died of noise cannot read as one that died of load.
#[test]
fn a_refused_replicate_is_replaced_and_never_becomes_a_draw() {
    let campaign = Campaign::collect(
        1,
        4,
        2.0,
        |run| {
            if run == 1 {
                return ReplicateDraw {
                    load: Some(0.4),
                    cells: vec![cell("scroll", "minimal", "ratio_p50", 9.9)],
                    refusal: Some("null-pair bracket 1.31 over floor 1.15".to_string()),
                };
            }
            draw(0.4, vec![cell("scroll", "minimal", "ratio_p50", 2.25)])
        },
        |_, _| {},
    )
    .unwrap();
    assert_eq!(campaign.replicates.len(), 2);
    let proposals = campaign.proposals().unwrap();
    assert_eq!(
        proposals[0].values,
        vec![2.25],
        "a refused replicate's numbers are not draws"
    );
    assert!(proposals[0].excluded.is_empty(), "{proposals:?}");
}

/// Past its replacement cap a campaign refuses rather than publishing a
/// short band, and it names the loads it saw: "the host was busy" without
/// the numbers is a claim the operator cannot act on.
#[test]
fn a_campaign_past_its_replacement_cap_refuses_naming_the_loads() {
    let err = Campaign::collect(
        3,
        4,
        2.0,
        |run| {
            draw(
                2.0 + run as f64,
                vec![cell("scroll", "minimal", "ratio_p50", 2.3)],
            )
        },
        |_, _| {},
    )
    .expect_err("a campaign that cannot fill its band must refuse");
    let message = err.to_string();
    assert!(message.contains("3.00, 4.00, 5.00, 6.00"), "{message}");
    assert!(message.contains("4 run(s)"), "{message}");
    assert!(!message.contains("Refused replicate(s)"), "{message}");

    let refused = Campaign::collect(
        1,
        1,
        2.0,
        |_| ReplicateDraw {
            load: None,
            cells: Vec::new(),
            refusal: Some("cell failed: engine never attached".to_string()),
        },
        |_, _| {},
    )
    .expect_err("a campaign whose replicates all refuse must refuse");
    assert!(
        refused.to_string().contains("engine never attached"),
        "{refused}"
    );
    assert!(refused.to_string().contains("unavailable"), "{refused}");
}

/// The walk above passing on the shipped files does not prove it can fail,
/// so each half of the rule is put a factor its draws refuse.
#[test]
fn a_factor_its_own_draws_refuse_fails_the_half_it_breaks() {
    let refused = |factor: f64, recorded: f64, values: &[f64]| {
        spread_violations(
            "case",
            Headroom::Proportional(factor),
            &DrawSet {
                recorded,
                values: values.to_vec(),
            },
        )
    };

    let every_half = refused(1.2, 1.0, &[1.0, 1.4]);
    assert_eq!(every_half.len(), 3, "{every_half:?}");

    let two_half_widths = refused(1.2, 2.0, &[1.0, 1.9, 2.0]);
    assert_eq!(two_half_widths.len(), 1, "{two_half_widths:?}");
    assert!(
        two_half_widths[0].contains("2x half-width"),
        "a band wider than the bar must name that half: {two_half_widths:?}"
    );

    let ratchet = refused(1.6, 2.1, &[1.0, 2.0, 2.1]);
    assert_eq!(ratchet.len(), 1, "{ratchet:?}");
    assert!(
        ratchet[0].contains("ratcheted"),
        "a factor a reseat breaks must name the ratchet: {ratchet:?}"
    );

    assert!(
        refused(1.6, 2.0, &[1.9, 2.0, 2.1]).is_empty(),
        "a factor its draws support must pass every half"
    );
}

/// A signed paired delta takes its allowance off the magnitude with a
/// floor, so sizing one proportionally would invert below zero. The one
/// metric family whose shape differs is therefore sized in its own shape,
/// not special-cased out of the campaign.
#[test]
fn a_signed_metric_is_sized_in_the_shape_its_kind_demands() {
    let shape = Headroom::Signed {
        factor: RATIO_HEADROOM,
        floor: crate::baselines::SIGNED_DELTA_FLOOR_MS,
    };
    let values = [-0.9, -0.5, -0.1];
    let sized = SizedFactor::size(shape, DrawStats::of(&values).unwrap().median, &values).unwrap();
    assert_eq!(
        spread_violations(
            "signed",
            crate::baselines::resized_headroom(shape, sized.factor),
            &DrawSet {
                recorded: sized.seat,
                values: values.to_vec(),
            }
        ),
        Vec::<String>::new(),
        "{sized:?}"
    );

    let wide = [-0.9, 4.0];
    let sized = SizedFactor::size(shape, DrawStats::of(&wide).unwrap().median, &wide).unwrap();
    assert!(
        sized.factor > 1.01,
        "a band past the signed floor must earn a real factor: {sized:?}"
    );
    assert!(spread_violations(
        "signed",
        crate::baselines::resized_headroom(shape, sized.factor),
        &DrawSet {
            recorded: sized.seat,
            values: wide.to_vec(),
        }
    )
    .is_empty());
}

/// Every metric a campaign measures gets a factor, including the tails a
/// shared class does not gate: a campaign is a characterization, and what
/// the gate does with a published spread is the gate's own question.
#[test]
fn every_measured_metric_reaches_a_proposal() {
    let mut metrics = CellMetrics::new();
    metrics.insert("ratio_p50".to_string(), 2.0);
    metrics.insert("ratio_p99".to_string(), 3.0);
    metrics.insert("view_p50_ms".to_string(), 4.0);
    let mut second = BTreeMap::new();
    second.insert("ratio_p50".to_string(), 2.1);
    second.insert("ratio_p99".to_string(), 3.2);
    second.insert("view_p50_ms".to_string(), 4.4);

    let campaign = Campaign::collect(
        2,
        4,
        2.0,
        |run| {
            let metrics = if run == 1 {
                metrics.clone()
            } else {
                second.clone()
            };
            draw(
                0.5,
                vec![MeasuredCell {
                    id: CellId::new("scroll", "minimal"),
                    metrics,
                }],
            )
        },
        |_, _| {},
    )
    .unwrap();
    let proposals = campaign.proposals().unwrap();
    let named: Vec<&str> = proposals.iter().map(|p| p.metric.as_str()).collect();
    assert_eq!(named, vec!["ratio_p50", "ratio_p99", "view_p50_ms"]);
}
