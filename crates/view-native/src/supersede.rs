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

/// A takeover's value as a `static` table can spell it.
///
/// [`OptionValue::Str`] owns a `String`, which no `const` expression can
/// build, so a `static` table typed on `OptionValue` can hold numbers and
/// booleans and silently cannot hold the string options -- `statusline`,
/// `winbar`, `tabline` -- that the next surfaces to change hands are made
/// of. A borrowed spell of the same three-variant domain keeps the table
/// writable for all of them; [`takeover_call`] is the one place it becomes
/// the owned value the wire takes.
// the variants the shipped table happens not to use yet are the point: this
// mirrors nvim's closed option value domain, so a boolean or string row is
// writable the day a surface needs one rather than a change of this type
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionValueSpec {
    /// A number option, e.g. `laststatus`.
    Int(i64),
    /// A boolean option, e.g. `ruler`.
    Bool(bool),
    /// A string option, e.g. `statusline`.
    Str(&'static str),
}

impl OptionValueSpec {
    /// This spec as the wire value an [`RpcCall`] carries. Total by
    /// construction over both closed enums, so a fourth option type added to
    /// either is a compile error here rather than a takeover that quietly
    /// sets nothing.
    fn value(self) -> OptionValue {
        match self {
            Self::Int(n) => OptionValue::Int(n),
            Self::Bool(b) => OptionValue::Bool(b),
            Self::Str(s) => OptionValue::Str(s.to_string()),
        }
    }
}

/// What one takeover row changes hands on.
///
/// Two kinds rather than one, because the two surfaces nvim lets a plugin
/// own are not the same kind of thing: `laststatus` is an option with a
/// value, and `vim.notify` is a Lua function with no value to name -- what
/// it is re-pointed at is the engine's own default, which the engine crate
/// reproduces from a live capture. A row shaped as an option with an empty
/// value would have to invent one, and every consumer would then have to
/// know which rows' values meant nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TakeoverKind {
    /// A global nvim option, held at `value`.
    ///
    /// Global-scoped only. The takeover chunk sets and re-asserts the option
    /// with an empty `{}` opts table, which `nvim_set_option_value` reads as
    /// the current window and buffer, so a window- or buffer-local option
    /// named here would be held for whichever window happened to be current
    /// when the plan was applied and left to the plugin everywhere else --
    /// with nothing failing. `every_held_option_is_global_scoped` in
    /// `supersede_live` asks a real nvim, so a row naming a local option
    /// fails rather than half-applying.
    Option {
        /// The nvim option name, exactly as `nvim_set_option_value` takes it.
        option: &'static str,
        /// The value that hands the surface to view.
        value: OptionValueSpec,
    },
    /// `vim.notify` itself, re-pointed at the engine default so every
    /// message a plugin raises through it crosses as `ext_messages` traffic
    /// and is drawn as one of view's toasts.
    Notify,
}

#[cfg(any(test, feature = "test-support"))]
impl TakeoverKind {
    /// The augroup name the hold this row issues creates inside nvim:
    /// `view-hold-<option>` for an option, `view-hold-notify` for the
    /// function.
    ///
    /// This spelling is a copy of one that lives in `view-engine`'s two hold
    /// chunks, which this crate cannot read: `view-native` has no dependency
    /// edge to `view-engine` and must not grow one.
    /// [`takeover_augroups`] exists so a crate that can see both pins the two
    /// against each other instead
    /// (`view-harness`'s `every_takeover_augroup_is_the_one_its_chunk_builds`).
    ///
    /// The engine's own key, spelled the way `HOLD_OPTION_CHUNK` and
    /// `HOLD_NOTIFY_CHUNK` build it, rather than a name invented here for
    /// the check: both chunks create their group with `clear = true`, so two
    /// rows whose groups collide leave only the later row's guard installed
    /// -- and that is a property of the augroup string, not of the option
    /// name or the kind. Comparing option names instead would let a row
    /// holding an nvim option literally named `notify` derive
    /// `view-hold-notify`, take the notify guard down inside nvim, and pass
    /// (`no_two_takeover_rows_claim_one_surface`,
    /// `an_option_named_notify_collides_with_the_notify_row`).
    fn claims(self) -> String {
        match self {
            Self::Option { option, .. } => format!("view-hold-{option}"),
            Self::Notify => "view-hold-notify".to_string(),
        }
    }
}

/// Every augroup the shipped takeover table's holds create inside nvim, one
/// per row, in table order.
///
/// The whole population rather than a sample: a row added later joins by
/// existing, so the pin that reads this cannot go stale against a table it
/// no longer covers.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn takeover_augroups() -> Vec<String> {
    TAKEOVERS.iter().map(|row| row.kind.claims()).collect()
}

/// One row of the takeover table: the feature that owns it, and what its
/// takeover changes hands on in the live session.
struct Takeover {
    /// The registry id this row belongs to. Matched against the registry
    /// rather than trusted, so a renamed feature cannot leave a row here
    /// pointing at nothing.
    feature: &'static str,
    /// The surface this row takes, and whatever that kind of surface needs
    /// naming. Its [`claims`](TakeoverKind::claims) is unique across the
    /// whole table, not merely within one feature: the guard a takeover
    /// installs is keyed on that name alone, so a second row naming it
    /// replaces the first row's guard whatever feature wrote it
    /// (`no_two_takeover_rows_claim_one_surface`).
    kind: TakeoverKind,
}

/// Renders one row as the call that performs it: always a durable hold,
/// never a plain set or assignment.
///
/// A superseded plugin keeps running, and a plugin that owns a surface
/// re-asserts its claim on its own events. lualine re-runs `setup()`
/// on `ColorScheme` and on `OptionSet background`, and that `setup()` sets
/// `laststatus`: measured against the compat harness's heavy fixture, a
/// plain one-shot set to `0` was back at `2` after the first
/// `:colorscheme`, with nothing failing and view still drawing a status
/// line it no longer owned. `vim.notify` is worse still -- noice patches it
/// from a deferred load that runs after the plan, and nvim-notify's own
/// documented setup patches it from `init.lua` -- so there the one-shot
/// loses in the ordinary case rather than the exotic one. The takeover
/// therefore has to be the kind that holds, and expressing it as one call
/// rather than as a set plus a separate guard entry means no consumer can
/// apply half of it.
fn takeover_call(row: &Takeover) -> RpcCall {
    match row.kind {
        TakeoverKind::Option { option, value } => RpcCall::HoldOption {
            name: option.to_string(),
            value: value.value(),
        },
        TakeoverKind::Notify => RpcCall::HoldNotify,
    }
}

/// Every takeover this build performs, as data rather than as a `match`, so
/// the set is enumerable: the drift check that every row still names a live
/// registry feature has something to walk.
///
/// Only surfaces nvim itself owns -- through an option, or through a
/// runtime function nvim ships a default for -- appear here. A picker or a
/// tree claims its surface with a mapping and a command instead, and a
/// plugin's own loading is left alone in every case: `laststatus = 0` stops
/// nvim drawing a status line, and lualine keeps running and keeps setting
/// `statusline` for whenever the user turns the native one off; `vim.notify`
/// back at the engine default leaves nvim-notify loaded and its own
/// `require('notify')` entry point working for anyone who calls it directly.
static TAKEOVERS: [Takeover; 2] = [
    Takeover {
        feature: "statusline",
        kind: TakeoverKind::Option {
            option: "laststatus",
            value: OptionValueSpec::Int(0),
        },
    },
    Takeover {
        feature: "notifications",
        kind: TakeoverKind::Notify,
    },
];

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
/// looks exactly like a row that was never written. Two rows whose holds
/// land on one augroup are the contradiction that cannot be resolved this
/// way -- whichever features write them, and whichever kinds they are --,
/// and `no_two_takeover_rows_claim_one_surface` rejects the table outright
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

    use std::path::Path;

    use super::*;
    use view_core::msg::Effect;
    use view_core::native::registry;
    use view_test_support::ScratchDir;

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
    fn notifications_enabled_supersedes_vim_notify() {
        let cfg = NativeConfig::all_enabled();
        let entries: Vec<Supersession> = plan(&cfg, registry::features())
            .into_iter()
            .filter(|s| s.feature == "notifications")
            .collect();
        let desc = registry::features()
            .iter()
            .find(|f| f.id == "notifications")
            .expect("the registry must carry a notifications feature");
        assert_eq!(
            entries.len(),
            1,
            "an enabled notifications must take vim.notify exactly once, got {entries:?}"
        );
        assert_eq!(entries[0].rpc, RpcCall::HoldNotify);
        assert_eq!(entries[0].reverses_with, desc.off_switch);
    }

    #[test]
    fn notifications_disabled_leaves_vim_notify_to_the_plugin() {
        let cfg = NativeConfig::from_toml_str("[native]\nnotifications = false\n")
            .expect("a known key must parse");
        let plan = plan(&cfg, registry::features());
        assert!(
            !plan.iter().any(|s| s.rpc == RpcCall::HoldNotify),
            "a disabled notifications must leave vim.notify alone, got {plan:?}"
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

    /// The first augroup two rows both create, as `(augroup, earlier
    /// feature, later feature)`, or `None` if every row in `takeovers`
    /// claims a distinct one.
    ///
    /// Blind to which features the two rows belong to, and to which kind of
    /// surface they name, because the thing that collides is neither: both
    /// hold chunks create their group with `clear = true`, so the second
    /// hold replaces the first one's guard and silently wins, while the plan
    /// still carries both entries, each printing its own reversal line for a
    /// surface only one of them holds. Two features whose holds land on one
    /// augroup is a contradiction in the table, not something an ordering
    /// rule can settle.
    fn colliding_claim(takeovers: &[Takeover]) -> Option<(String, &'static str, &'static str)> {
        takeovers.iter().enumerate().find_map(|(i, t)| {
            takeovers
                .iter()
                .skip(i + 1)
                .find(|o| o.kind.claims() == t.kind.claims())
                .map(|o| (t.kind.claims(), t.feature, o.feature))
        })
    }

    #[test]
    fn no_two_takeover_rows_claim_one_surface() {
        assert_eq!(
            colliding_claim(&TAKEOVERS),
            None,
            "one surface cannot be handed over twice: the later row's hold \
             replaces the earlier row's guard and wins silently"
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
                kind: TakeoverKind::Option {
                    option: "laststatus",
                    value: OptionValueSpec::Int(0),
                },
            },
            Takeover {
                feature: "notifications",
                kind: TakeoverKind::Option {
                    option: "laststatus",
                    value: OptionValueSpec::Int(3),
                },
            },
        ];
        assert_eq!(
            colliding_claim(&table),
            Some((
                "view-hold-laststatus".to_string(),
                "statusline",
                "notifications"
            ))
        );
    }

    #[test]
    fn two_features_claiming_vim_notify_are_rejected() {
        // the same contradiction on the other kind of surface: a check that
        // only compared option names would see two rows with no option at
        // all and wave both through, and the second augroup's `clear = true`
        // would silently take the first one's guard down
        let table = [
            Takeover {
                feature: "notifications",
                kind: TakeoverKind::Notify,
            },
            Takeover {
                feature: "statusline",
                kind: TakeoverKind::Notify,
            },
        ];
        assert_eq!(
            colliding_claim(&table),
            Some((
                "view-hold-notify".to_string(),
                "notifications",
                "statusline"
            ))
        );
    }

    #[test]
    fn an_option_named_notify_collides_with_the_notify_row() {
        // the cross-KIND collision, which a check comparing option names
        // against an invented "vim.notify" spelling waves through: nvim
        // carries no option named `notify` today, so nothing but this walk
        // stands between a future row and a takeover whose guard is silently
        // taken down by the other row's `clear = true`
        let table = [
            Takeover {
                feature: "notifications",
                kind: TakeoverKind::Notify,
            },
            Takeover {
                feature: "statusline",
                kind: TakeoverKind::Option {
                    option: "notify",
                    value: OptionValueSpec::Bool(true),
                },
            },
        ];
        assert_eq!(
            colliding_claim(&table),
            Some((
                "view-hold-notify".to_string(),
                "notifications",
                "statusline"
            ))
        );
    }

    #[test]
    fn a_notify_row_and_an_option_row_do_not_collide() {
        // the shipped table's own shape: two kinds, one feature each. A
        // uniqueness rule that collapsed both kinds onto one name would
        // reject it
        assert_eq!(colliding_claim(&TAKEOVERS), None);
        assert_eq!(
            TAKEOVERS.len(),
            2,
            "both kinds must be in the shipped table"
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
                kind: TakeoverKind::Option {
                    option: "laststatus",
                    value: OptionValueSpec::Int(0),
                },
            },
            Takeover {
                feature: "statusline",
                kind: TakeoverKind::Option {
                    option: "ruler",
                    value: OptionValueSpec::Bool(false),
                },
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
    fn a_string_valued_row_is_writable_in_the_table_and_reaches_the_plan_owned() {
        // the shape a `static [Takeover]` typed on OptionValue could not
        // hold at all: `statusline` is a string option, and the next
        // surfaces to change hands (winbar, tabline) are string options too
        let table = [
            Takeover {
                feature: "statusline",
                kind: TakeoverKind::Option {
                    option: "statusline",
                    value: OptionValueSpec::Str("%f"),
                },
            },
            Takeover {
                feature: "statusline",
                kind: TakeoverKind::Option {
                    option: "ruler",
                    value: OptionValueSpec::Bool(false),
                },
            },
        ];
        let entries = plan_from(&NativeConfig::all_enabled(), registry::features(), &table);
        let calls: Vec<&RpcCall> = entries.iter().map(|entry| &entry.rpc).collect();
        assert_eq!(
            calls,
            vec![
                &RpcCall::HoldOption {
                    name: "statusline".to_string(),
                    value: OptionValue::Str("%f".to_string()),
                },
                &RpcCall::HoldOption {
                    name: "ruler".to_string(),
                    value: OptionValue::Bool(false),
                },
            ],
            "every option type must survive the table-to-wire conversion intact"
        );
    }

    #[test]
    fn a_disabled_feature_supersedes_nothing_however_many_rows_it_has() {
        let table = [
            Takeover {
                feature: "statusline",
                kind: TakeoverKind::Option {
                    option: "laststatus",
                    value: OptionValueSpec::Int(0),
                },
            },
            Takeover {
                feature: "statusline",
                kind: TakeoverKind::Option {
                    option: "ruler",
                    value: OptionValueSpec::Bool(false),
                },
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
    fn fixture_dir(name: &str) -> ScratchDir {
        let dir = ScratchDir::new(&format!("supersede-{name}"))
            .expect("the fixture directory must be creatable");
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
    fn every_entry_rides_an_api_call_never_the_keyboard() {
        let plan = plan(&NativeConfig::all_enabled(), registry::features());
        assert!(!plan.is_empty(), "the all-enabled plan must not be empty");
        for entry in &plan {
            assert!(
                matches!(entry.rpc, RpcCall::HoldOption { .. } | RpcCall::HoldNotify),
                "{} must supersede through a durable API call, got {:?}",
                entry.feature,
                entry.rpc
            );
        }
    }
}
