//! The runtime supersession plan: what view takes over from a plugin, as
//! RPC against the live session.
//!
//! Supersession is runtime only and reversible. Nothing here edits a user's
//! config, and nothing needs removing from an `init.lua`: a superseded
//! plugin keeps loading, and its cost is memory rather than conflict.
//! Turning a feature off in `[native]` and restarting is the whole reversal
//! procedure, which is why every entry carries the exact line that performs
//! it.
//!
//! One plan, built in one place, rather than each feature applying its own
//! takeover inside its own init: doctor and the first-run notice both ask
//! "what has view taken over?", and an answer reassembled by inspecting two
//! call sites drifts from the answer the session actually applied.

use view_core::msg::{OptionValue, RpcCall};
use view_core::native::registry::FeatureDesc;

use crate::config::NativeConfig;

/// One feature's takeover of a plugin surface: the RPC that performs it
/// against the live session, and the config line that gives the surface
/// back.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Supersession {
    /// The registry id of the feature doing the superseding.
    pub feature: &'static str,
    /// The call that performs the takeover. Always an API call, never
    /// `RpcCall::Input`: see that variant's own note on mode dependence.
    pub rpc: RpcCall,
    /// The exact line a user writes to reverse this, verbatim from the
    /// registry's `off_switch` so the reversal a notice prints and the
    /// reversal doctor prints can never disagree.
    pub reverses_with: &'static str,
    /// The plugin surface being taken over, verbatim from the registry's
    /// `supersedes`, or `None` for a feature that names no plugin.
    ///
    /// Carried on the entry rather than looked up again by consumers: the
    /// notice and doctor both render it, and a second lookup keyed on
    /// `feature` is a second place the plan and its description can
    /// disagree about what was taken over.
    pub supersedes: Option<&'static str>,
}

impl Supersession {
    /// The user-facing sentence for this takeover: what view now draws,
    /// that the superseded plugin still loads, and the line that hands the
    /// surface back.
    ///
    /// Reads as prose because it is shown as prose, in a toast and in
    /// doctor's output alike. The off switch is never reworded or
    /// re-derived here, so what a user is told to paste is what the
    /// registry says works.
    #[must_use]
    pub fn notice(&self) -> String {
        match self.supersedes {
            Some(plugin) => format!(
                "view is drawing the {} ({plugin} still loads). Turn it off with {}",
                self.feature, self.reverses_with
            ),
            None => format!(
                "view is drawing the {}. Turn it off with {}",
                self.feature, self.reverses_with
            ),
        }
    }
}

/// One row of the takeover table: the feature that owns it, and the option
/// its takeover sets on the live session.
struct Takeover {
    /// The registry id this row belongs to. Matched against the registry
    /// rather than trusted, so a renamed feature cannot leave a row here
    /// pointing at nothing.
    feature: &'static str,
    /// The nvim option name, exactly as `nvim_set_option_value` takes it.
    option: &'static str,
    /// The value that hands the surface to view.
    value: OptionValue,
}

/// Every takeover this build performs, as data rather than as a `match`, so
/// the set is enumerable: the drift check that every row still names a live
/// registry feature has something to walk.
///
/// Only surfaces nvim itself owns through an option appear here. A picker
/// or a tree claims its surface with a mapping and a command instead, and a
/// plugin's own loading is left alone in every case: `laststatus = 0` stops
/// nvim drawing a status line, and lualine keeps running and keeps setting
/// `statusline` for whenever the user turns the native one off.
static TAKEOVERS: [Takeover; 1] = [Takeover {
    feature: "statusline",
    option: "laststatus",
    value: OptionValue::Int(0),
}];

/// The supersession plan for `cfg`: one entry per enabled feature in
/// `features` that takes a surface over through RPC, in registry order.
///
/// A disabled feature contributes nothing, and neither does an enabled
/// feature whose takeover needs no runtime call (a picker claims its
/// surface through mappings, not through an option), so an empty plan is an
/// ordinary answer rather than a failure.
///
/// Walks `features` rather than the takeover table so the plan's order is
/// the registry's listing order, which is the order every consumer-facing
/// listing of these features already uses.
#[must_use]
pub fn plan(cfg: &NativeConfig, features: &[FeatureDesc]) -> Vec<Supersession> {
    features
        .iter()
        .filter(|f| cfg.enabled(f.id))
        .filter_map(|f| {
            let takeover = TAKEOVERS.iter().find(|t| t.feature == f.id)?;
            Some(Supersession {
                feature: f.id,
                rpc: RpcCall::SetOption {
                    name: takeover.option.to_string(),
                    value: takeover.value.clone(),
                },
                reverses_with: f.off_switch,
                supersedes: f.supersedes,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::path::{Path, PathBuf};

    use super::*;
    use view_core::msg::Effect;
    use view_core::native::registry;

    #[test]
    fn an_enabled_statusline_yields_one_entry_reversed_by_its_own_off_switch() {
        let cfg = NativeConfig::all_enabled();
        let entries: Vec<Supersession> = plan(&cfg, registry::features())
            .into_iter()
            .filter(|s| s.feature == "statusline")
            .collect();
        let desc = registry::features()
            .iter()
            .find(|f| f.id == "statusline")
            .expect("the registry must carry a statusline feature");
        assert_eq!(
            entries.len(),
            1,
            "an enabled statusline must supersede exactly once, got {entries:?}"
        );
        assert_eq!(entries[0].reverses_with, desc.off_switch);
        assert_eq!(
            entries[0].rpc,
            RpcCall::SetOption {
                name: "laststatus".to_string(),
                value: OptionValue::Int(0),
            }
        );
    }

    #[test]
    fn a_disabled_feature_supersedes_nothing() {
        let cfg = NativeConfig::from_toml_str("[native]\nstatusline = false\n")
            .expect("a known key must parse");
        let plan = plan(&cfg, registry::features());
        assert!(
            !plan.iter().any(|s| s.feature == "statusline"),
            "a disabled statusline must take over nothing, got {plan:?}"
        );
    }

    #[test]
    fn every_takeover_row_names_a_live_registry_feature() {
        for t in &TAKEOVERS {
            assert!(
                registry::is_feature(t.feature),
                "takeover row {} names no registry feature",
                t.feature
            );
        }
    }

    #[test]
    fn every_entry_reverses_with_its_own_registry_off_switch() {
        let plan = plan(&NativeConfig::all_enabled(), registry::features());
        assert!(!plan.is_empty(), "the all-enabled plan must not be empty");
        for entry in &plan {
            let desc = registry::features()
                .iter()
                .find(|f| f.id == entry.feature)
                .expect("every plan entry must name a registry feature");
            assert_eq!(
                entry.reverses_with, desc.off_switch,
                "{}'s reversal must be its registry off switch verbatim",
                entry.feature
            );
        }
    }

    /// Every file under `dir`, as sorted `(relative name, bytes)` pairs:
    /// the fixture snapshot the config-untouched invariant compares. Byte
    /// equality rather than a digest of the bytes, which is the same
    /// comparison with a collision risk added; the listing rides along so a
    /// plan that left the existing files alone and wrote a *new* one beside
    /// them is caught by the same assertion.
    fn snapshot_dir(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
            .expect("the fixture directory must be readable")
            .map(|entry| {
                let entry = entry.expect("every fixture directory entry must be readable");
                let bytes =
                    std::fs::read(entry.path()).expect("every fixture file must be readable");
                (entry.file_name().to_string_lossy().into_owned(), bytes)
            })
            .collect();
        out.sort();
        out
    }

    /// A [`snapshot_dir`] result rendered readably for a failure message: a
    /// byte-vector diff of two Lua files says nothing at a glance about
    /// which line changed.
    fn readable(snapshot: &[(String, Vec<u8>)]) -> Vec<(String, String)> {
        snapshot
            .iter()
            .map(|(name, bytes)| (name.clone(), String::from_utf8_lossy(bytes).into_owned()))
            .collect()
    }

    /// A fixture config directory holding an `init.lua`, at the path this
    /// module's own disconfirmation writes to (see the invariant test).
    fn fixture_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("view-supersede-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the fixture directory must be creatable");
        std::fs::write(
            dir.join("init.lua"),
            "vim.opt.laststatus = 3\nrequire('lualine').setup({})\n",
        )
        .expect("the fixture init.lua must be writable");
        dir
    }

    #[test]
    fn applying_a_plan_leaves_the_user_config_byte_for_byte_untouched() {
        let dir = fixture_dir("untouched");
        let before = snapshot_dir(&dir);

        let plan = plan(&NativeConfig::all_enabled(), registry::features());
        let effects: Vec<Effect> = plan.iter().map(|s| Effect::Rpc(s.rpc.clone())).collect();
        assert_eq!(
            effects.len(),
            plan.len(),
            "every plan entry must become exactly one effect"
        );

        let after = snapshot_dir(&dir);
        std::fs::remove_dir_all(&dir).expect("the fixture directory must be removable");
        assert_eq!(
            before,
            after,
            "supersession is runtime only: no config file may be created, edited or removed\nbefore: {:?}\nafter:  {:?}",
            readable(&before),
            readable(&after)
        );
    }

    #[test]
    fn every_entry_carries_its_registry_supersedes_verbatim() {
        let plan = plan(&NativeConfig::all_enabled(), registry::features());
        assert!(!plan.is_empty(), "the all-enabled plan must not be empty");
        for entry in &plan {
            let desc = registry::features()
                .iter()
                .find(|f| f.id == entry.feature)
                .expect("every plan entry must name a registry feature");
            assert_eq!(entry.supersedes, desc.supersedes);
        }
    }

    #[test]
    fn a_notice_names_the_feature_the_plugin_and_the_off_switch_verbatim() {
        let entry = plan(&NativeConfig::all_enabled(), registry::features())
            .into_iter()
            .find(|s| s.feature == "statusline")
            .expect("an all-enabled plan must supersede the statusline");
        assert_eq!(
            entry.notice(),
            "view is drawing the statusline (lualine still loads). \
             Turn it off with native.statusline = false"
        );
    }

    #[test]
    fn a_notice_states_the_off_switch_the_registry_states() {
        for entry in plan(&NativeConfig::all_enabled(), registry::features()) {
            let desc = registry::features()
                .iter()
                .find(|f| f.id == entry.feature)
                .expect("every plan entry must name a registry feature");
            assert!(
                entry.notice().contains(desc.off_switch),
                "{}'s notice must quote {} verbatim, got {:?}",
                entry.feature,
                desc.off_switch,
                entry.notice()
            );
        }
    }

    #[test]
    fn every_entry_rides_an_api_call_never_the_keyboard() {
        let plan = plan(&NativeConfig::all_enabled(), registry::features());
        for entry in &plan {
            assert!(
                matches!(entry.rpc, RpcCall::SetOption { .. }),
                "{} must supersede through an API call, got {:?}",
                entry.feature,
                entry.rpc
            );
        }
    }
}
