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

use view_core::native::mappings::MappingClaim;
use view_core::native::registry::FeatureDesc;

use crate::supersede::Supersession;

/// Which kind of surface a [`Handover`] changed hands on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Surface {
    /// An nvim option view holds for the session, from the supersession
    /// plan.
    SessionOption,
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
    /// The plugin surface being taken over, verbatim from the registry's
    /// `supersedes`, or `None` for a feature that names no plugin.
    pub supersedes: Option<&'static str>,
}

impl Handover {
    /// The user-facing sentence: what view took, what still loads, and the
    /// line that hands it back.
    ///
    /// Reads as prose because it is shown as prose, in a toast and in
    /// doctor's output alike. The off switch is never reworded or re-derived
    /// here, so what a user is told to paste is what the registry says
    /// works.
    #[must_use]
    pub fn notice(&self) -> String {
        let took = match &self.surface {
            Surface::SessionOption => format!("view is drawing the {}", self.feature),
            Surface::Key { lhs } => format!("view took {lhs} for the {}", self.feature),
        };
        match self.supersedes {
            Some(plugin) => format!(
                "{took} ({plugin} still loads). Turn it off with {}",
                self.reverses_with
            ),
            None => format!("{took}. Turn it off with {}", self.reverses_with),
        }
    }

    /// How this handover is keyed in the first-run record.
    ///
    /// A feature that takes both an option and a key has two things to say
    /// and says each once, so the key is per surface rather than per
    /// feature. The option form is the bare feature id, which is what
    /// earlier records already hold.
    #[must_use]
    pub fn record_key(&self) -> String {
        match &self.surface {
            Surface::SessionOption => self.feature.to_string(),
            Surface::Key { lhs } => format!("{}:key:{lhs}", self.feature),
        }
    }
}

/// Everything this session took over: the supersession plan's options first,
/// then the default keys that landed on a user's own mapping, in
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
    let mut out: Vec<Handover> = plan
        .iter()
        .map(|entry| Handover {
            feature: entry.feature,
            surface: Surface::SessionOption,
            reverses_with: entry.reverses_with,
            supersedes: entry.supersedes,
        })
        .collect();
    out.extend(
        claimed
            .iter()
            .filter(|c| c.had_user_mapping)
            .filter_map(|c| {
                let desc = features.iter().find(|f| f.id == c.feature)?;
                Some(Handover {
                    feature: desc.id,
                    surface: Surface::Key { lhs: c.lhs.clone() },
                    reverses_with: desc.off_switch,
                    supersedes: desc.supersedes,
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
            report[0].notice(),
            "view took <leader>ff for the picker (telescope still loads). \
             Turn it off with native.picker = false"
        );
    }

    #[test]
    fn a_held_option_is_reported_with_the_switch_that_returns_it() {
        let handover = report(
            &plan(&NativeConfig::all_enabled(), registry::features()),
            &[],
            registry::features(),
        )
        .into_iter()
        .find(|h| h.feature == "statusline")
        .expect("an all-enabled plan must supersede the statusline");
        assert_eq!(
            handover.notice(),
            "view is drawing the statusline (lualine still loads). \
             Turn it off with native.statusline = false"
        );
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
        let plan = plan(&NativeConfig::all_enabled(), registry::features());
        let report = report(
            &plan,
            &[claim("picker", "<leader>ff", true)],
            registry::features(),
        );
        assert_eq!(
            report.len(),
            plan.len() + 1,
            "every takeover and every claim must reach one report: {report:?}"
        );
        for handover in &report {
            let desc = registry::features()
                .iter()
                .find(|f| f.id == handover.feature)
                .expect("every handover must name a registry feature");
            assert!(
                handover.notice().contains(desc.off_switch),
                "{}'s notice must quote {} verbatim, got {:?}",
                handover.feature,
                desc.off_switch,
                handover.notice()
            );
        }
    }

    #[test]
    fn one_feature_taking_two_surfaces_records_each_of_them() {
        let report = report(
            &plan(&NativeConfig::all_enabled(), registry::features()),
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
}
