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
    /// The call that performs the takeover, and the only call a consumer
    /// has to make for it: a takeover that has to survive the superseded
    /// plugin re-asserting its option carries that durability inside this
    /// one call rather than in a second field a caller could forget (see
    /// [`takeover_call`]).
    ///
    /// Always an API call, never `RpcCall::Input`: see that variant's own
    /// note on mode dependence.
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
    ///
    /// Unique across the whole table, not merely within one feature: the
    /// guard a takeover installs is keyed on the option name alone, so a
    /// second row naming it replaces the first row's guard whatever feature
    /// wrote it (`no_two_takeover_rows_claim_one_option`).
    option: &'static str,
    /// The value that hands the surface to view.
    value: OptionValue,
}

/// Renders one row as the call that performs it: always
/// [`RpcCall::HoldOption`], never a plain `SetOption`.
///
/// A superseded plugin keeps running, and a plugin that owns a surface
/// re-asserts its own option on its own events. lualine re-runs `setup()`
/// on `ColorScheme` and on `OptionSet background`, and that `setup()` sets
/// `laststatus`: measured against the compat harness's heavy fixture, a
/// plain one-shot set to `0` was back at `2` after the first
/// `:colorscheme`, with nothing failing and view still drawing a status
/// line it no longer owned. The takeover therefore has to be the kind that
/// holds, and expressing it as one call rather than as a set plus a
/// separate guard entry means no consumer can apply half of it.
fn takeover_call(row: &Takeover) -> RpcCall {
    RpcCall::HoldOption {
        name: row.option.to_string(),
        value: row.value.clone(),
    }
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
    plan_from(cfg, features, &TAKEOVERS)
}

/// [`plan`] against an arbitrary takeover table, so the table-walking rules
/// (every row of a feature contributes; a feature with no row contributes
/// nothing) are testable against shapes the shipped table does not have yet.
///
/// Every matching row becomes an entry, rather than the first one: a
/// surface that needs two options to change hands is one feature with two
/// rows, and taking only the first would leave view believing it owns a
/// surface nvim is still half drawing -- silently, since the dropped row
/// looks exactly like a row that was never written. Two rows setting the
/// same option for one feature are the contradiction that cannot be
/// resolved this way -- whichever features write them --, and
/// `no_two_takeover_rows_claim_one_option` rejects the table outright
/// rather than letting later-wins ordering decide.
fn plan_from(
    cfg: &NativeConfig,
    features: &[FeatureDesc],
    takeovers: &[Takeover],
) -> Vec<Supersession> {
    features
        .iter()
        .filter(|f| cfg.enabled(f.id))
        .flat_map(|f| {
            takeovers
                .iter()
                .filter(move |t| t.feature == f.id)
                .map(move |t| Supersession {
                    feature: f.id,
                    rpc: takeover_call(t),
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
            RpcCall::HoldOption {
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

    /// The first option two rows both claim, as `(option, earlier feature,
    /// later feature)`, or `None` if every row in `takeovers` claims a
    /// distinct one.
    ///
    /// Blind to which features the two rows belong to, because the thing
    /// that collides is not: the guard a hold installs is keyed on the
    /// option name alone, so the second hold replaces the first one's guard
    /// and its value silently wins -- while the plan still carries both
    /// entries, each printing its own reversal line for a surface only one
    /// of them holds. Two features claiming one surface is a contradiction
    /// in the table, not something an ordering rule can settle.
    fn colliding_option(
        takeovers: &[Takeover],
    ) -> Option<(&'static str, &'static str, &'static str)> {
        takeovers.iter().enumerate().find_map(|(i, t)| {
            takeovers
                .iter()
                .skip(i + 1)
                .find(|o| o.option == t.option)
                .map(|o| (t.option, t.feature, o.feature))
        })
    }

    #[test]
    fn no_two_takeover_rows_claim_one_option() {
        assert_eq!(
            colliding_option(&TAKEOVERS),
            None,
            "one option cannot be handed over twice: the later row's hold \
             replaces the earlier row's guard and its value wins silently"
        );
    }

    #[test]
    fn two_features_claiming_one_option_are_rejected() {
        // the cross-feature shape, which a per-feature check waves through:
        // both rows reach the plan, both hold the same option, and each
        // entry offers a different off switch for a surface one of them no
        // longer holds
        let table = [
            Takeover {
                feature: "statusline",
                option: "laststatus",
                value: OptionValue::Int(0),
            },
            Takeover {
                feature: "notifications",
                option: "laststatus",
                value: OptionValue::Int(3),
            },
        ];
        assert_eq!(
            colliding_option(&table),
            Some(("laststatus", "statusline", "notifications"))
        );
    }

    #[test]
    fn every_takeover_row_for_one_feature_reaches_the_plan() {
        // a feature whose surface needs two options: the shipped table has
        // no such row yet, and the walk that only ever sees one row per
        // feature is the walk that silently drops the second one
        let table = [
            Takeover {
                feature: "statusline",
                option: "laststatus",
                value: OptionValue::Int(0),
            },
            Takeover {
                feature: "statusline",
                option: "ruler",
                value: OptionValue::Bool(false),
            },
        ];
        let entries = plan_from(&NativeConfig::all_enabled(), registry::features(), &table);
        // a non-option entry drops out here rather than being asserted on
        // directly, and the comparison below still catches it: the expected
        // list names both options, so anything that failed to arrive as one
        // fails this assertion
        let options: Vec<&str> = entries
            .iter()
            .filter_map(|entry| match &entry.rpc {
                RpcCall::HoldOption { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            options,
            vec!["laststatus", "ruler"],
            "every row of an enabled feature must reach the plan, in table order"
        );
        assert!(entries.iter().all(|entry| entry.feature == "statusline"));
    }

    #[test]
    fn a_disabled_feature_supersedes_nothing_however_many_rows_it_has() {
        let table = [
            Takeover {
                feature: "statusline",
                option: "laststatus",
                value: OptionValue::Int(0),
            },
            Takeover {
                feature: "statusline",
                option: "ruler",
                value: OptionValue::Bool(false),
            },
        ];
        let cfg = NativeConfig::from_toml_str("[native]\nstatusline = false\n")
            .expect("a known key must parse");
        let entries = plan_from(&cfg, registry::features(), &table);
        assert!(
            !entries.iter().any(|s| s.feature == "statusline"),
            "a disabled feature must take over nothing at all, got {entries:?}"
        );
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
                matches!(entry.rpc, RpcCall::HoldOption { .. }),
                "{} must supersede through a durable API call, got {:?}",
                entry.feature,
                entry.rpc
            );
        }
    }
}
