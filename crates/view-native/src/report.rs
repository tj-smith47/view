//! What view tells a user it has taken over, in one vocabulary.
//!
//! Two things change hands when a native feature turns on: an nvim option a
//! plugin was using to draw a surface, and a default key the user's own
//! config had mapped. Both are the same news to a user -- something they set
//! up is no longer in force, and one config line puts it back -- so both are
//! reported as one [`Handover`] kind rather than as a takeover notice and a
//! separate mapping notice with their own wording and their own record.
//!
//! Only surfaces that actually changed hands appear here. A default key that
//! landed on nothing took nothing, so it is not news; what keys a session
//! registered at all is a different question, answered by the mapping table
//! and the config rather than by this report.

use view_core::native::chords;
use view_core::native::mappings;
use view_core::native::mappings::MappingClaim;
use view_core::native::registry::FeatureDesc;
use view_core::native::surfaces::Taken;

use crate::supersede::Supersession;

/// Which kind of surface a [`Handover`] changed hands on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Surface {
    /// A surface the supersession plan took for the session: an nvim option
    /// view keeps at its own value, `vim.notify` re-pointed at the engine
    /// default, or a surface the attach itself took by asking for its
    /// `ext_*` option. One variant for all three, because they are the same
    /// news to a user -- a surface their plugin was drawing is view's for
    /// this session, and one config line gives it back -- and the notice
    /// below reads the same for any of them.
    SessionHold,
    /// A default key view registered over a mapping the user's config had
    /// already made.
    Key {
        /// The key in nvim notation, exactly as it was registered.
        lhs: String,
    },
}

/// One surface that changed hands, and the exact line that gives it back.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handover {
    /// The registry id of the feature that took the surface.
    pub feature: &'static str,
    /// What it took.
    pub surface: Surface,
    /// The exact line a user writes to reverse this, verbatim from the
    /// registry's `off_switch`, so what a notice prints and what doctor
    /// prints can never disagree.
    pub reverses_with: &'static str,
}

impl Handover {
    /// This handover as the launch box names it, beside everything else the
    /// launch handed to view.
    #[must_use]
    pub fn taken(&self) -> Taken {
        match &self.surface {
            Surface::SessionHold => Taken::Drawing {
                feature: self.feature,
                off_switch: self.reverses_with,
            },
            Surface::Key { lhs } => Taken::Key {
                lhs: lhs.clone(),
                action: action(self.feature, lhs),
                off_switch: self.reverses_with,
            },
        }
    }

    /// How this handover is keyed in the first-run record.
    ///
    /// A feature that takes both an option and a key has two things to say
    /// and says each once, so the key is per surface rather than per
    /// feature. The held-surface form is the bare feature id, which is what
    /// earlier records already hold.
    #[must_use]
    pub fn record_key(&self) -> String {
        match &self.surface {
            Surface::SessionHold => self.feature.to_string(),
            Surface::Key { lhs } => format!("{}:key:{lhs}", self.feature),
        }
    }
}

/// What `lhs` does under `feature`, in words: the feature and the verb the
/// key registers, from the default key table or the desktop chord table.
///
/// A key neither table spells that way, a chord a `[keys.desktop]` row
/// rebound, is named by its feature alone.
fn action(feature: &str, lhs: &str) -> String {
    let verb = mappings::default_maps()
        .iter()
        .find(|spec| spec.feature == feature && spec.lhs == lhs)
        .map(|spec| spec.verb)
        .or_else(|| {
            chords::desktop_chords()
                .iter()
                .find(|chord| {
                    chord.feature == feature && (chord.with_super == lhs || chord.with_alt == lhs)
                })
                .map(|chord| chord.verb)
        });
    match verb {
        Some(verb) => format!("{feature} {}", verb.replace('_', " ")),
        None => format!("the {feature}"),
    }
}

/// Everything this session took over: the supersession plan's held surfaces
/// first, then the default keys that landed on a user's own mapping, in
/// registration order.
///
/// `claimed` is the engine's own answer to the mapping registration (see
/// `Msg::MappingsClaimed`), not a re-derivation from the config: only nvim
/// knows what was mapped before view got there. A claim that took nothing,
/// or names a feature this build does not have, contributes nothing.
#[must_use]
pub fn report(
    plan: &[Supersession],
    claimed: &[MappingClaim],
    features: &[FeatureDesc],
) -> Vec<Handover> {
    let mut out: Vec<Handover> = Vec::new();
    for entry in plan.iter().filter(|entry| entry.announced) {
        // one line per feature, not per channel: a feature whose surface
        // changes hands on several channels says one thing to a user --
        // view draws it now, and one config line gives it back -- and its
        // record key is the feature id, so a second entry would be a
        // duplicate sentence that the record could not silence separately
        if out.iter().any(|held| held.feature == entry.feature) {
            continue;
        }
        out.push(Handover {
            feature: entry.feature,
            surface: Surface::SessionHold,
            reverses_with: entry.reverses_with,
        });
    }
    out.extend(
        claimed
            .iter()
            .filter(|c| c.had_user_mapping)
            .filter_map(|c| {
                // A feature the registry tracks answers first. A feature
                // reachable only through `mappings::exempt_feature` (a key
                // with no `FeatureDesc` row, `ai` today) answers the same
                // two facts from there: a user's own mapping being taken is
                // news whichever table names the feature.
                let (id, off_switch) =
                    if let Some(desc) = features.iter().find(|f| f.id == c.feature) {
                        (desc.id, desc.off_switch)
                    } else {
                        let exempt = mappings::exempt_feature(&c.feature)?;
                        (exempt.id, exempt.off_switch)
                    };
                Some(Handover {
                    feature: id,
                    surface: Surface::Key { lhs: c.lhs.clone() },
                    reverses_with: off_switch,
                })
            }),
    );
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::config::NativeConfig;
    use crate::supersede::plan;
    use view_core::model::Look;
    use view_core::native::registry;

    fn claim(feature: &str, lhs: &str, had_user_mapping: bool) -> MappingClaim {
        MappingClaim {
            feature: feature.to_string(),
            lhs: lhs.to_string(),
            had_user_mapping,
        }
    }

    #[test]
    fn a_key_taken_from_a_user_is_reported_with_the_switch_that_returns_it() {
        let report = report(
            &[],
            &[claim("picker", "<leader>ff", true)],
            registry::features(),
        );
        assert_eq!(report.len(), 1, "the claimed key must be reported");
        assert_eq!(
            report[0].taken(),
            Taken::Key {
                lhs: "<leader>ff".to_string(),
                action: "picker files".to_string(),
                off_switch: "native.picker = false",
            }
        );
    }

    #[test]
    fn a_held_option_is_reported_with_the_switch_that_returns_it() {
        let handover = report(
            &plan(
                &NativeConfig::all_enabled(),
                registry::features(),
                Look::default(),
            ),
            &[],
            registry::features(),
        )
        .into_iter()
        .find(|h| h.feature == "statusline")
        .expect("an all-enabled plan must supersede the statusline");
        assert_eq!(
            handover.taken(),
            Taken::Drawing {
                feature: "statusline",
                off_switch: "native.statusline = false",
            }
        );
    }

    /// The hold tiles keeps on a switched-off statusline names no switch:
    /// the user already wrote it.
    #[test]
    fn a_hold_the_look_keeps_on_a_disabled_feature_is_not_reported() {
        let cfg = NativeConfig::from_toml_str("[native]\nstatusline = false\n").unwrap();
        let plan = plan(
            &cfg,
            registry::features(),
            Look::new(view_core::model::Panes::Tiles, true),
        );
        assert!(plan.iter().any(|s| s.feature == "statusline"), "{plan:?}");
        let handovers = report(&plan, &[], registry::features());
        assert!(
            !handovers.iter().any(|h| h.feature == "statusline"),
            "{handovers:?}"
        );
    }

    /// The notify takeover is listed on exactly the same terms as the held
    /// option, through the same `Surface::SessionHold`: a user who lost
    /// their notification floats is told what took them and what gives them
    /// back, and does not have to notice that one of these two surfaces is
    /// an option and the other a Lua function.
    #[test]
    fn the_notify_takeover_is_reported_with_the_switch_that_returns_it() {
        let handover = report(
            &plan(
                &NativeConfig::all_enabled(),
                registry::features(),
                Look::default(),
            ),
            &[],
            registry::features(),
        )
        .into_iter()
        .find(|h| h.feature == "notifications")
        .expect("an all-enabled plan must supersede notifications");
        assert_eq!(handover.surface, Surface::SessionHold);
        assert_eq!(
            handover.taken(),
            Taken::Drawing {
                feature: "notifications",
                off_switch: "native.notifications = false",
            }
        );
    }

    /// A desktop chord's feature is `window`, a
    /// [`view_core::native::mappings::exempt_feature`] row with no registry
    /// row, so the claim must still reach the exempt-feature fallback
    /// `report` falls back to and print that row's own off switch. `window`
    /// structurally cannot carry a `[native]` line.
    #[test]
    fn a_chord_over_a_user_mapping_is_claimed_and_reported() {
        let report = report(
            &[],
            &[claim("window", "<D-Left>", true)],
            registry::features(),
        );
        assert_eq!(report.len(), 1, "the claimed chord must be reported");
        assert_eq!(
            report[0].taken(),
            Taken::Key {
                lhs: "<D-Left>".to_string(),
                action: "window focus left".to_string(),
                off_switch: "keys.profile = \"editor\"",
            }
        );
    }

    /// Every key a session can take from a user, by every spelling it
    /// registers under, reaches the launch box as the key, what it does now
    /// in words, and the line that gives it back. A key that landed on
    /// nothing says nothing.
    #[test]
    fn every_key_handover_names_its_action_in_words() {
        let mut keys: Vec<(String, String)> = mappings::default_maps()
            .iter()
            .map(|spec| (spec.feature.to_string(), spec.lhs.to_string()))
            .collect();
        for chord in chords::desktop_chords() {
            for lhs in [chord.with_super, chord.with_alt] {
                keys.push((chord.feature.to_string(), lhs.to_string()));
            }
        }
        assert!(keys.len() > 50, "the walk found nothing: {keys:?}");
        for (feature, lhs) in &keys {
            for had_user_mapping in [true, false] {
                let handovers = report(
                    &[],
                    &[claim(feature, lhs, had_user_mapping)],
                    registry::features(),
                );
                if !had_user_mapping {
                    assert!(handovers.is_empty(), "{feature} {lhs}: {handovers:?}");
                    continue;
                }
                assert_eq!(handovers.len(), 1, "{feature} {lhs}: {handovers:?}");
                let Taken::Key {
                    lhs: taken_lhs,
                    action,
                    off_switch,
                } = handovers[0].taken()
                else {
                    unreachable!("a key handover is a taken key");
                };
                assert_eq!(&taken_lhs, lhs);
                assert_eq!(off_switch, handovers[0].reverses_with, "{feature} {lhs}");
                let verb = action.strip_prefix(&format!("{feature} "));
                assert!(
                    verb.is_some_and(|verb| !verb.is_empty()),
                    "{feature} {lhs}: the action names the feature and a verb: {action}"
                );
                assert!(
                    !action.contains('_'),
                    "a verb is spelled in words: {action}"
                );
                assert!(!action.contains("chords"), "{action}");
            }
        }
    }

    #[test]
    fn a_key_that_landed_on_nothing_is_not_news() {
        let report = report(
            &[],
            &[claim("picker", "<leader>ff", false)],
            registry::features(),
        );
        assert!(
            report.is_empty(),
            "a key nobody had mapped took nothing from anyone: {report:?}"
        );
    }

    #[test]
    fn options_and_keys_report_through_the_same_mechanism() {
        let plan = plan(
            &NativeConfig::all_enabled(),
            registry::features(),
            Look::default(),
        );
        let report = report(
            &plan,
            &[claim("picker", "<leader>ff", true)],
            registry::features(),
        );
        let mut features: Vec<&str> = plan.iter().map(|entry| entry.feature).collect();
        features.dedup();
        assert_eq!(
            report.len(),
            features.len() + 1,
            "every feature the plan took and every claim must reach one report: {report:?}"
        );
        for handover in &report {
            let desc = registry::features()
                .iter()
                .find(|f| f.id == handover.feature)
                .expect("every handover must name a registry feature");
            assert_eq!(
                handover.taken().off_switch(),
                desc.off_switch,
                "{}'s launch box line must quote the registry switch verbatim",
                handover.feature
            );
        }
    }

    #[test]
    fn one_feature_taking_two_surfaces_records_each_of_them() {
        let report = report(
            &plan(
                &NativeConfig::all_enabled(),
                registry::features(),
                Look::default(),
            ),
            &[
                claim("picker", "<leader>ff", true),
                claim("picker", "<leader>fb", true),
            ],
            registry::features(),
        );
        let mut keys: Vec<String> = report.iter().map(Handover::record_key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(
            keys.len(),
            report.len(),
            "two surfaces sharing one record key would announce only one of them"
        );
    }

    #[test]
    fn a_claim_naming_no_feature_in_this_build_is_dropped() {
        let report = report(
            &[],
            &[claim("pickr", "<leader>ff", true)],
            registry::features(),
        );
        assert!(report.is_empty(), "{report:?}");
    }

    /// A claim naming a feature the registry does not track (`ai`, which
    /// has no `FeatureDesc` by design) must still be reported: a user's own
    /// mapping being taken is news whichever table names the feature that
    /// took it.
    #[test]
    fn a_claim_on_a_registry_exempt_feature_is_reported_not_dropped() {
        let report = report(
            &[],
            &[claim("ai", "<leader>ai", true)],
            registry::features(),
        );
        assert_eq!(
            report.len(),
            1,
            "the claimed ai key must be reported even with no FeatureDesc: {report:?}"
        );
        assert_eq!(
            report[0].taken(),
            Taken::Key {
                lhs: "<leader>ai".to_string(),
                action: "ai toggle".to_string(),
                off_switch: "ai.enabled = false",
            }
        );
    }
}
