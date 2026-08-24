//! Which default keys a session registers: the `[native]` table resolved
//! against [`view_core::native::mappings::default_maps`].
//!
//! The planner decides membership only. Registering a key, checking what was
//! there before it, and reporting the claim are the engine's and the
//! runtime's jobs; nothing here does I/O or speaks RPC.

use view_core::msg::RpcCall;
use view_core::native::mappings::default_maps;

use crate::config::NativeConfig;

/// The one call that registers this session's default keys and the `:View`
/// command, for the runtime to emit once `VimEnter` has fired.
///
/// A feature the user turned off contributes no spec, which is the whole of
/// how its default keys are given back: view never registers them, so
/// whatever the user's own config mapped to that key is what is still there.
/// The `:View` command carries no spec and registers unconditionally, so a
/// user who turned every default key off keeps a way in.
#[must_use]
pub fn register_plan(cfg: &NativeConfig, channel_id: u64) -> RpcCall {
    RpcCall::RegisterMappings {
        specs: default_maps()
            .iter()
            .filter(|spec| cfg.enabled(spec.feature))
            .copied()
            .collect(),
        channel_id,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn specs_of(call: &RpcCall) -> Vec<&'static str> {
        match call {
            RpcCall::RegisterMappings { specs, .. } => specs.iter().map(|s| s.lhs).collect(),
            other => panic!("register_plan built {other:?}"),
        }
    }

    #[test]
    fn a_disabled_feature_contributes_no_default_key() {
        let cfg = NativeConfig::from_toml_str("[native]\npicker = false\n").unwrap();
        let plan = register_plan(&cfg, 7);
        match &plan {
            RpcCall::RegisterMappings { specs, .. } => {
                let claimed: Vec<&str> = specs
                    .iter()
                    .filter(|s| s.feature == "picker")
                    .map(|s| s.lhs)
                    .collect();
                assert!(
                    claimed.is_empty(),
                    "picker is off, so nothing may register over the user's {claimed:?}"
                );
            }
            other => panic!("register_plan built {other:?}"),
        }
    }

    #[test]
    fn an_enabled_feature_contributes_every_one_of_its_default_keys() {
        let plan = register_plan(&NativeConfig::all_enabled(), 7);
        let registered = specs_of(&plan);
        for spec in default_maps() {
            assert!(
                registered.contains(&spec.lhs),
                "{} is enabled but {} never reached the plan",
                spec.feature,
                spec.lhs
            );
        }
    }

    #[test]
    fn the_plan_carries_the_channel_the_keys_notify_back_over() {
        match register_plan(&NativeConfig::all_enabled(), 42) {
            RpcCall::RegisterMappings { channel_id, .. } => assert_eq!(channel_id, 42),
            other => panic!("register_plan built {other:?}"),
        }
    }

    /// The keys page is documentation a user reads instead of the source,
    /// so a key added to the table and not to the page is a user pressing
    /// something nobody told them about. Asserted from the crate that owns
    /// the config example's own drift check, for the same reason: neither
    /// consumer-facing file may drift from what the build actually does.
    ///
    /// One of two tests pinning that page: view-core's
    /// `both_docs_pages_render_the_review_keys_this_build_installs` pins the
    /// review-key table on it, since that table is generated where the
    /// review keys live.
    #[test]
    fn the_keys_page_renders_the_table_this_build_registers() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/keymaps.md");
        let page = std::fs::read_to_string(path).expect("docs/keymaps.md must be readable");
        let table = view_core::native::mappings::render_table();
        assert!(
            page.contains(&table),
            "docs/keymaps.md is stale, it must carry:\n{table}"
        );
    }
}
