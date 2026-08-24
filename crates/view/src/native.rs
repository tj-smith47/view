//! The native feature session: what `view.toml` left switched on, the
//! takeover this session performs for it, and the one notice that says so.
//!
//! `update()` is pure and `view-core` cannot read a config file or write a
//! record, so the two steps a native feature owes a real session -- taking
//! its surfaces over once nvim has finished sourcing the user's config, and
//! introducing itself once -- hang off the two messages that mark those
//! moments rather than off a startup call nothing would sequence. Both
//! arrive through the ordinary dispatch path, so both are covered wherever
//! that path runs: the cutover replay resolves a `VimEnter` staged before the
//! loop started, and the loop itself resolves one that fires after.

use std::path::PathBuf;

use view_core::model::Model;
use view_core::msg::{Effect, EngineRequest, Msg, OptionValue, RpcCall};
use view_core::native::registry;
use view_native::config::{NativeConfig, ViewConfig};
use view_native::report::report;
use view_native::supersede::{plan, Supersession};
use view_native::{mappings, paths, toast};

/// Which native step, if any, a message owes beyond `update()`'s own answer
/// to it.
///
/// Read from the message before `update()` consumes it, and applied after,
/// so the takeover follows nvim's `VimEnter` reply rather than racing it and
/// the notice sees the claims `update()` has already recorded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Nothing native is owed.
    None,
    /// The user's config has been sourced and `mapleader` is theirs: the
    /// moment every takeover this session performs is allowed to happen.
    VimEnter,
    /// The registration answered with what it claimed, which is the last
    /// fact the first-run notice was waiting on.
    Claims,
}

/// The step `msg` owes, or [`Stage::None`].
pub(crate) fn stage(msg: &Msg) -> Stage {
    match msg {
        Msg::EngineRequest(EngineRequest::VimEnter { .. }) => Stage::VimEnter,
        Msg::MappingsClaimed { .. } => Stage::Claims,
        _ => Stage::None,
    }
}

/// One session's native configuration and the plan it applies.
///
/// Built once, right after attach, so the config is read on the startup
/// thread rather than inside the loop, and the plan every consumer reads --
/// the takeover, the notice, and later doctor -- is the one this session
/// actually applied.
pub(crate) struct NativeSession {
    /// What the user left switched on.
    cfg: NativeConfig,
    /// The option surfaces this session takes over, in registry order.
    plan: Vec<Supersession>,
    /// The config file the answers came from, or `None` for a session
    /// running without one. Keys the first-run record, so a second config
    /// introduces itself on its own terms.
    config_path: Option<PathBuf>,
    /// Where the first-run record lives, or `None` for a machine with no
    /// resolvable state directory. Resolved once here rather than inside the
    /// notice, so no loop pass ever reads the environment.
    record: Option<PathBuf>,
    /// This connection's own channel, which the registered keys notify back
    /// over.
    channel_id: u64,
    /// Whether the takeover has already been emitted. `VimEnter` is a
    /// one-shot autocmd, so this guards a duplicate rather than a repeat:
    /// registering twice would answer the second pass with view's own
    /// mappings and report every key as one the user had.
    handed_over: bool,
    /// Snapshot of `model.ai_enabled` at construction, the same "read once
    /// before the loop, not per pass" rule every other field here follows.
    /// `mappings::register_plan` reads `NativeConfig` alone and has no `ai`
    /// switch to consult (`[ai]` is not a `[native]` key by design), so this
    /// crate -- which already knows `ai` by name through `view_ai::TrustStore`
    /// -- is where the registration plan's `ai` row is dropped when the
    /// feature is off, keeping `view-native` itself unaware of any feature
    /// beyond the generic registry/exemption predicate it already reads.
    ai_enabled: bool,
}

impl NativeSession {
    /// Resolves `config_path` into this session's answers, falling back to
    /// the full experience when the file cannot be read or understood.
    ///
    /// A broken config is reported to the user through `model`'s own message
    /// surface rather than to stderr: this runs behind the terminal's raw-mode
    /// alternate screen, where a stderr write is invisible at best.
    /// Falling back rather than refusing to start matches the loader's own
    /// contract that an absent file is the full experience -- an editor does
    /// not decline to open a file over a typo in an optional table -- but the
    /// user is told, because a silently ignored `picker = false` is a feature
    /// they turned off still taking their keys.
    ///
    /// Returns whatever effect the broken-config notice owes the engine
    /// alongside the built session, rather than pushing it and discarding
    /// the return the way a bare `push_native` call would: a broken config
    /// is discovered before `runtime::run`'s loop exists to run an effect
    /// through, so the caller (`main.rs`) is the one that knows whether
    /// that is "immediately, through the pre-cutover executor" or, for an
    /// even earlier failure, "once an executor exists at all" -- this
    /// method has no opinion on which and must not silently drop the
    /// effect deciding it does not apply yet.
    pub(crate) fn load(
        config_path: Option<PathBuf>,
        channel_id: u64,
        model: &mut Model,
    ) -> (Self, Vec<Effect>) {
        let mut effects = Vec::new();
        let resolved = match ViewConfig::load(config_path.as_deref()) {
            Ok(resolved) => resolved,
            Err(err) => {
                crate::vlog::log_with("native", || format!("config unreadable: {err}"));
                model.dirty = true;
                effects = model.engine.record_native_notice(
                    format!("view: {err}; every native feature stays on this session"),
                    false,
                );
                ViewConfig::defaults()
            }
        };
        let cfg = resolved.native;
        model.statusline_enabled = cfg.enabled("statusline");
        model.palette_enabled = cfg.enabled("palette");
        model.tree_width_pct = cfg.tree_width();
        // a width that could not be read is the one `[native]` mistake that
        // does not fail the table (see `resolve_tree_width`), so this is the
        // only place it can be said out loud
        if let Some(notice) = cfg.tree_width_notice() {
            model.dirty = true;
            effects.extend(model.engine.record_native_notice(notice.to_string(), false));
        }
        model.key_bindings = resolved.keys.bindings().clone();
        // an entry that named no key leaves its own action on the defaults
        // rather than failing the table (see `resolve_key_bindings`), so this
        // is the only place it can be said out loud
        for notice in resolved.keys.notices() {
            model.dirty = true;
            effects.extend(
                model
                    .engine
                    .record_native_notice((*notice).to_string(), false),
            );
        }
        model.supervision.auto_restart = resolved.supervision.auto_restart;
        // `ui_attach` already ran, at the raw terminal height, before this
        // config was even read (see `main.rs`'s call ordering), so nvim's
        // live grid still claims every row `statusline_rows()` now needs to
        // reserve. Without this, `view_surface::render` places the
        // statusline at `offset + grid_h` using nvim's still-full grid
        // height and paints it one row below the terminal entirely, same
        // shape as `UiEvent::TablineUpdate`'s resize-on-change below.
        if model.statusline_enabled {
            let (grid_width, grid_height) = model.grid_target();
            effects.push(Effect::Rpc(RpcCall::TryResize {
                width: grid_width,
                height: grid_height,
            }));
        }
        let plan = plan(&cfg, registry::features());
        let session = Self {
            cfg,
            plan,
            config_path,
            record: paths::state_dir().map(|dir| paths::first_run_record(&dir)),
            channel_id,
            handed_over: false,
            ai_enabled: model.ai_enabled,
        };
        (session, effects)
    }

    /// Points this session at a replacement engine.
    ///
    /// A restarted engine is a different connection: it answers on its own
    /// channel, and it carries none of the mappings or option takeovers the
    /// one it replaced was given. So both facts this session holds about
    /// the connection are reset -- the channel the registered keys notify
    /// back over, and whether the takeover has been performed -- and the
    /// fresh engine's own `VimEnter` performs it again. The config, the
    /// plan and the first-run record are untouched: they are facts about
    /// the session, which is the thing that survived.
    pub(crate) fn rebind(&mut self, channel_id: u64) {
        self.channel_id = channel_id;
        self.handed_over = false;
    }

    /// Carries out `stage` against `model`, returning whatever it owes the
    /// engine.
    #[must_use]
    pub(crate) fn follow_up(&mut self, model: &mut Model, stage: Stage) -> Vec<Effect> {
        match stage {
            Stage::None => Vec::new(),
            Stage::VimEnter => self.take_over(),
            Stage::Claims => self.announce(model),
        }
    }

    /// Every takeover this session performs, then the one registration that
    /// claims its keys and the `:View` command.
    ///
    /// Options first: they are what nvim stops drawing, and issuing them
    /// ahead of a registration that answers asynchronously means the session
    /// is never briefly holding a key for a surface it has not taken yet.
    ///
    /// The clipboard provider registers unconditionally, unlike every option
    /// and mapping above: `"+yy`/`"+p` are core editing infrastructure, not a
    /// feature a config can decline the way it declines the picker or
    /// statusline, so this push does not read `self.cfg` or `self.plan` at
    /// all.
    ///
    /// `cmdheight=0` registers unconditionally for the same reason, on a
    /// different fact: `ext_messages` sits in the fixed ext-option set this
    /// session attached with, not in `self.plan`, so `native.notifications
    /// = false` cannot un-attach it and messages route to view either way.
    /// Leaving the user's `cmdheight` alone while every message still
    /// arrives as a `msg_show` view must render would be the incoherent
    /// state -- the option follows the attach, not the feature toggle.
    fn take_over(&mut self) -> Vec<Effect> {
        if self.handed_over {
            return Vec::new();
        }
        self.handed_over = true;
        let mut effects: Vec<Effect> = self
            .plan
            .iter()
            .map(|entry| Effect::Rpc(entry.rpc.clone()))
            .collect();
        let mut mapping_call = mappings::register_plan(&self.cfg, self.channel_id);
        // `NativeConfig::enabled("ai")` is unconditionally `true` -- `[ai]`
        // has no `[native]` switch by design, so `register_plan` alone would
        // always register the key. `model.ai_enabled` is the bit `[native]`
        // structurally cannot carry for this one feature, so it is applied
        // here instead, once, rather than teaching `view-native` a feature
        // name it has no other reason to know.
        if !self.ai_enabled {
            if let RpcCall::RegisterMappings { specs, .. } = &mut mapping_call {
                specs.retain(|spec| spec.feature != "ai");
            }
        }
        effects.push(Effect::Rpc(mapping_call));
        effects.push(Effect::Rpc(RpcCall::RegisterClipboard {
            channel_id: self.channel_id,
        }));
        effects.push(Effect::Rpc(RpcCall::SetOption {
            name: "cmdheight".to_string(),
            value: OptionValue::Int(0),
        }));
        crate::vlog::log_with("native", || {
            let taken: Vec<&str> = self.plan.iter().map(|e| e.feature).collect();
            format!("takeover options={taken:?} channel={}", self.channel_id)
        });
        effects
    }

    /// Shows whatever this session took over for the first time, once.
    ///
    /// Options and keys come through one report, so the wording, the off
    /// switch and the record entry are the same mechanism for both. A record
    /// that cannot be written is logged and the notice shown anyway: the
    /// worst that costs is repeating it next launch, and staying silent
    /// instead would trade a repeated notice for a user who is never told
    /// what took their key.
    ///
    /// Latency consequence: `toast::first_run` underneath this reads and,
    /// when there is anything new to announce, writes the record file
    /// (`std::fs::read_to_string`/`create_dir_all`/`write` in
    /// `view-native`'s `toast.rs`) synchronously, on whatever thread calls
    /// this -- the same `dispatch` thread every `Msg` runs through, since
    /// this follow-up fires from `Stage::Claims`. That stage fires exactly
    /// once per session, right after nvim reports its key claims during
    /// startup, so the blocking disk I/O lands on time-to-first-paint at
    /// most once and never recurs on the per-frame steady-state path this
    /// crate's performance budgets actually gate.
    fn announce(&self, model: &mut Model) -> Vec<Effect> {
        let handovers = report(&self.plan, model.claimed_keys(), registry::features());
        if handovers.is_empty() {
            return Vec::new();
        }
        let notices = match &self.record {
            Some(record) => match toast::first_run(&handovers, self.config_path.as_deref(), record)
            {
                Ok(notices) => notices,
                Err(err) => {
                    crate::vlog::log_with("native", || format!("first-run record failed: {err}"));
                    handovers.iter().map(|h| h.notice()).collect()
                }
            },
            None => {
                crate::vlog::log(
                    "native",
                    "no state directory: the first-run notice cannot be recorded",
                );
                handovers.iter().map(|h| h.notice()).collect()
            }
        };
        let mut effects = Vec::new();
        for notice in notices {
            model.dirty = true;
            effects.extend(model.engine.record_native_notice(notice, false));
        }
        effects
    }
}

#[cfg(test)]
impl NativeSession {
    /// A session that hands nothing over, for the tests whose subject is the
    /// dispatch path itself rather than what a native feature does on it.
    pub(crate) fn inert() -> Self {
        Self {
            cfg: NativeConfig::all_enabled(),
            plan: Vec::new(),
            config_path: None,
            record: None,
            channel_id: 0,
            handed_over: true,
            ai_enabled: true,
        }
    }

    /// A session with every feature on, reading no config file, recording
    /// its notices at `record`, notifying back over `channel_id`.
    pub(crate) fn all_enabled(channel_id: u64, record: Option<PathBuf>) -> Self {
        Self {
            cfg: NativeConfig::all_enabled(),
            plan: plan(&NativeConfig::all_enabled(), registry::features()),
            config_path: None,
            record,
            channel_id,
            handed_over: false,
            ai_enabled: true,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use view_core::msg::ReplyToken;
    use view_core::native::mappings::MappingClaim;

    fn model() -> Model {
        Model::with_term_size(80, 24)
    }

    /// A scratch record path for one test, named for it so two tests never
    /// read each other's record. The returned guard must outlive every use
    /// of the path: dropping it removes the directory the path points
    /// into.
    fn scratch(name: &str) -> (view_test_support::ScratchDir, PathBuf) {
        let dir = view_test_support::ScratchDir::new(&format!("native-{name}")).unwrap();
        let record = dir.join("first-run.toml");
        (dir, record)
    }

    /// Everything the message surface is currently showing, as one string.
    fn shown(model: &Model) -> String {
        model
            .engine
            .messages
            .entries
            .iter()
            .flat_map(|e| e.content.iter().map(|(_, t)| t.as_str()))
            .collect()
    }

    #[test]
    fn vim_enter_is_the_stage_that_hands_the_surfaces_over() {
        assert!(
            stage(&Msg::EngineRequest(EngineRequest::VimEnter {
                token: ReplyToken { msgid: 1 }
            })) == Stage::VimEnter
        );
        assert!(
            stage(&Msg::MappingsClaimed {
                claimed: Vec::new()
            }) == Stage::Claims
        );
        assert!(stage(&Msg::RedrawReady) == Stage::None);
    }

    #[test]
    fn the_takeover_holds_every_planned_option_and_registers_the_keys_once() {
        let mut session = NativeSession::all_enabled(7, None);
        let mut m = model();
        let effects = session.follow_up(&mut m, Stage::VimEnter);
        let holds = effects
            .iter()
            .filter(|e| matches!(e, Effect::Rpc(RpcCall::HoldOption { .. })))
            .count();
        assert_eq!(
            holds,
            plan(&NativeConfig::all_enabled(), registry::features()).len(),
            "every planned option must be held: {effects:?}"
        );
        assert!(
            holds > 0,
            "the shipped plan holds at least one option; a takeover of none means the plan never reached the seam"
        );
        let registrations: Vec<&Effect> = effects
            .iter()
            .filter(|e| matches!(e, Effect::Rpc(RpcCall::RegisterMappings { .. })))
            .collect();
        assert_eq!(
            registrations.len(),
            1,
            "the keys register in exactly one chunk: {effects:?}"
        );
        match registrations[0] {
            Effect::Rpc(RpcCall::RegisterMappings { channel_id, .. }) => {
                assert_eq!(*channel_id, 7);
            }
            other => unreachable!("{other:?}"),
        }
        let clipboard_registrations = effects
            .iter()
            .filter(|e| matches!(e, Effect::Rpc(RpcCall::RegisterClipboard { .. })))
            .count();
        assert_eq!(
            clipboard_registrations, 1,
            "the clipboard provider registers exactly once, unconditionally: {effects:?}"
        );
        assert!(
            session.follow_up(&mut m, Stage::VimEnter).is_empty(),
            "a second VimEnter must register nothing: the second pass would read view's own keys back as the user's"
        );
    }

    #[test]
    fn a_disabled_ai_feature_registers_no_ai_key() {
        let mut session = NativeSession {
            cfg: NativeConfig::all_enabled(),
            plan: plan(&NativeConfig::all_enabled(), registry::features()),
            config_path: None,
            record: None,
            channel_id: 13,
            handed_over: false,
            ai_enabled: false,
        };
        let mut m = model();
        let effects = session.follow_up(&mut m, Stage::VimEnter);
        let specs = effects
            .iter()
            .find_map(|e| match e {
                Effect::Rpc(RpcCall::RegisterMappings { specs, .. }) => Some(specs),
                _ => None,
            })
            .expect("a RegisterMappings effect must still register the rest");
        assert!(
            specs.iter().all(|s| s.feature != "ai"),
            "ai must contribute no key while disabled: {specs:?}"
        );
        assert!(
            specs.iter().any(|s| s.feature == "picker"),
            "every other feature's keys must still register: {specs:?}"
        );
    }

    #[test]
    fn an_enabled_ai_feature_still_registers_its_key() {
        let mut session = NativeSession::all_enabled(14, None);
        let mut m = model();
        let effects = session.follow_up(&mut m, Stage::VimEnter);
        let specs = effects
            .iter()
            .find_map(|e| match e {
                Effect::Rpc(RpcCall::RegisterMappings { specs, .. }) => Some(specs),
                _ => None,
            })
            .expect("a RegisterMappings effect must register the plan");
        assert!(
            specs.iter().any(|s| s.feature == "ai"),
            "the default (enabled) session must still register the ai key: {specs:?}"
        );
    }

    #[test]
    fn load_snapshots_ai_enabled_from_the_model_rather_than_a_hardcoded_default() {
        let mut m = model();
        m.ai_enabled = false;
        let (mut session, _effects) = NativeSession::load(None, 21, &mut m);
        let effects = session.follow_up(&mut m, Stage::VimEnter);
        let specs = effects
            .iter()
            .find_map(|e| match e {
                Effect::Rpc(RpcCall::RegisterMappings { specs, .. }) => Some(specs),
                _ => None,
            })
            .expect("a RegisterMappings effect must still register the rest");
        assert!(
            specs.iter().all(|s| s.feature != "ai"),
            "load() must carry model.ai_enabled into the session, not a hardcoded true: {specs:?}"
        );
    }

    #[test]
    fn the_takeover_registers_the_clipboard_provider_even_with_every_plan_entry_empty() {
        let mut session = NativeSession {
            cfg: NativeConfig::all_enabled(),
            plan: Vec::new(),
            config_path: None,
            record: None,
            channel_id: 9,
            handed_over: false,
            ai_enabled: true,
        };
        let mut m = model();
        let effects = session.follow_up(&mut m, Stage::VimEnter);
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::Rpc(RpcCall::RegisterClipboard { channel_id: 9 })
            )),
            "clipboard registration is not registry-gated: it must survive an empty plan, got {effects:?}"
        );
    }

    /// A replacement engine has none of the registrations the one it
    /// replaced was given, and answers on a channel of its own. A session
    /// that kept either fact would leave a recovered editor with view's keys
    /// unbound and its notifications addressed to a channel that is gone.
    #[test]
    fn a_rebound_session_hands_over_again_and_to_the_new_channel() {
        let mut session = NativeSession::all_enabled(7, None);
        let mut m = model();
        let first = session.follow_up(&mut m, Stage::VimEnter);
        assert!(
            !first.is_empty(),
            "the first takeover registered nothing at all"
        );
        assert!(
            session.follow_up(&mut m, Stage::VimEnter).is_empty(),
            "one engine is handed over to exactly once"
        );

        session.rebind(21);
        let again = session.follow_up(&mut m, Stage::VimEnter);
        assert!(
            again.iter().any(|e| matches!(
                e,
                Effect::Rpc(RpcCall::RegisterClipboard { channel_id: 21 })
            )),
            "the replacement engine was never handed the registrations the \
             dead one had, or was handed them on the dead one's channel: {again:?}"
        );
        assert!(
            again.iter().any(|e| matches!(
                e,
                Effect::Rpc(RpcCall::RegisterMappings { channel_id: 21, .. })
            )),
            "view's own keys are unbound in the recovered session: {again:?}"
        );
    }

    #[test]
    fn cmdheight_is_forced_to_zero_even_with_the_notifications_feature_off() {
        let cfg =
            NativeConfig::from_toml_str("[native]\nnotifications = false\n").expect("valid toml");
        let mut session = NativeSession {
            plan: plan(&cfg, registry::features()),
            cfg,
            config_path: None,
            record: None,
            channel_id: 11,
            handed_over: false,
            ai_enabled: true,
        };
        let mut m = model();
        let effects = session.follow_up(&mut m, Stage::VimEnter);
        let cmdheight = effects.iter().find_map(|e| match e {
            Effect::Rpc(RpcCall::SetOption { name, value }) if name == "cmdheight" => Some(value),
            _ => None,
        });
        assert_eq!(
            cmdheight,
            Some(&OptionValue::Int(0)),
            "cmdheight must be forced to 0 unconditionally -- ext_messages is attach-level, \
             not gated on native.notifications: {effects:?}"
        );
    }

    #[test]
    fn a_claimed_key_is_announced_with_the_switch_that_returns_it() {
        let (_dir, record) = scratch("claimed");
        let claimed = vec![MappingClaim {
            feature: "picker".to_string(),
            lhs: "<leader>ff".to_string(),
            had_user_mapping: true,
        }];

        let mut session = NativeSession::all_enabled(7, Some(record.clone()));
        let mut m = model();
        m.record_claimed_keys(claimed.clone());
        let effects = session.follow_up(&mut m, Stage::Claims);
        assert!(
            !effects.is_empty()
                && effects
                    .iter()
                    .all(|e| matches!(e, Effect::ScheduleToastExpiry { .. })),
            "every notice this stage pushes must talk to the user through the same \
             choke point every other locally-synthesized notice uses (never straight \
             to nvim), got {effects:?}"
        );
        let first = shown(&m);
        assert!(
            first.contains("<leader>ff") && first.contains("native.picker = false"),
            "the notice must name the key and the switch that returns it, got {first:?}"
        );
        assert!(
            first.contains("native.statusline = false"),
            "the option this session held must announce itself through the same notice, got {first:?}"
        );
        assert!(m.dirty);

        let mut next = NativeSession::all_enabled(7, Some(record));
        let mut later = model();
        later.record_claimed_keys(claimed);
        let _ = next.follow_up(&mut later, Stage::Claims);
        assert_eq!(
            shown(&later),
            "",
            "a surface introduces itself once per config, not every launch"
        );
    }

    /// `ui_attach` already ran, at the raw terminal height, before `load`
    /// ever reads a config (see `main.rs`'s call ordering): nvim's live
    /// grid still claims the row the default-on statusline now needs.
    /// Without a resize here, `view_surface::render` would place the
    /// statusline at `offset + grid_h` using nvim's still-full grid height
    /// and paint it one row below the terminal entirely.
    #[test]
    fn load_reserves_the_statusline_row_with_a_resize_when_nothing_disables_it() {
        let mut m = model();
        let (_session, effects) = NativeSession::load(None, 7, &mut m);
        assert!(m.statusline_enabled, "an absent config is every feature on");
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::Rpc(RpcCall::TryResize {
                    width: 80,
                    height: 23
                })
            )),
            "load must reserve the statusline's row the moment it turns the \
             feature on, got {effects:?}"
        );
    }

    /// The opposite of the row-reservation test above: a config that turns
    /// the statusline off must never touch nvim's grid, or a shrunk grid
    /// with no statusline painted over it would leave a permanently blank
    /// row a user never asked to give up.
    #[test]
    fn load_skips_the_resize_when_the_config_turns_the_statusline_off() {
        let dir = view_test_support::ScratchDir::new("native-statusline-resize").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(&path, "[native]\nstatusline = false\n")
            .expect("a temp config must be writable");

        let mut m = model();
        let (_session, effects) = NativeSession::load(Some(path), 7, &mut m);

        assert!(
            !m.statusline_enabled,
            "the config explicitly disabled the feature"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Rpc(RpcCall::TryResize { .. }))),
            "a disabled statusline reserves no row and must not resize nvim's \
             already-correct grid, got {effects:?}"
        );
    }

    /// The palette's off switch, mirroring the statusline pair above minus
    /// the resize concern: the palette floats over the grid rather than
    /// reserving a row from it, so turning it off has nothing to undo on
    /// nvim's side -- only `Model::palette_enabled` itself changes.
    #[test]
    fn load_turns_the_palette_on_by_default_and_off_when_configured() {
        let mut on = model();
        let _ = NativeSession::load(None, 7, &mut on);
        assert!(on.palette_enabled, "an absent config is every feature on");

        let dir = view_test_support::ScratchDir::new("native-palette-toggle").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(
            &path,
            "[native]
palette = false
",
        )
        .expect("a temp config must be writable");

        let mut off = model();
        let _ = NativeSession::load(Some(path), 7, &mut off);

        assert!(
            !off.palette_enabled,
            "the config explicitly disabled the feature"
        );
    }
    /// The `[supervision]` table's one switch reaching the model, over the
    /// same single load every other table's answers cross.
    #[test]
    fn load_recovers_automatically_by_default_and_stops_when_configured() {
        let mut on = model();
        let _ = NativeSession::load(None, 7, &mut on);
        assert!(
            on.supervision.auto_restart,
            "an absent config must keep automatic recovery on"
        );

        let dir = view_test_support::ScratchDir::new("native-supervision-toggle").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(
            &path,
            "[supervision]
auto_restart = false
",
        )
        .expect("a temp config must be writable");

        let mut off = model();
        let _ = NativeSession::load(Some(path), 7, &mut off);

        assert!(
            !off.supervision.auto_restart,
            "the config explicitly turned automatic recovery off"
        );
        assert!(
            off.palette_enabled,
            "a supervision-only config must leave every native feature on"
        );
    }

    /// The `[keys]` table crossing the same load: the resolved bindings
    /// reach the model, and an entry naming no key leaves its own action
    /// alone while telling the user.
    #[test]
    fn load_carries_the_key_bindings_and_reports_one_it_could_not_read() {
        use view_core::native::keys::{Action, Direction, Resolved};

        let mut default = model();
        let _ = NativeSession::load(None, 7, &mut default);
        assert_eq!(
            default.key_bindings.resolve(Some("<C-w>"), ">"),
            Some(Resolved::Act(Action::Resize(Direction::Wider))),
            "an absent config is the shipped chord"
        );
        assert_eq!(
            default.key_bindings.resolve(None, "<M-CR>"),
            Some(Resolved::Act(Action::ComposerNewline)),
            "and the composer's shipped line break"
        );

        let dir = view_test_support::ScratchDir::new("native-keys").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(
            &path,
            "[keys]
sidebar_wider = \"<M-.>\"
sidebar_narrower = 30
composer_newline = \"<A-x>\"
",
        )
        .expect("a temp config must be writable");

        let mut configured = model();
        let (_session, effects) = NativeSession::load(Some(path), 7, &mut configured);

        assert_eq!(
            configured.key_bindings.resolve(None, "<M-.>"),
            Some(Resolved::Act(Action::Resize(Direction::Wider))),
            "the readable action was rebound"
        );
        assert_eq!(
            configured.key_bindings.resolve(None, "<S-Left>"),
            Some(Resolved::Act(Action::Resize(Direction::Narrower))),
            "the unreadable one kept its defaults"
        );
        assert!(
            !effects.is_empty(),
            "the notice is raised through an effect"
        );
        let raised = format!("{:?}", configured.engine.messages.entries);
        assert!(
            raised.contains("sidebar_narrower"),
            "and the user is told which entry was dropped: {raised}"
        );
        assert!(
            !raised.contains("sidebar_wider"),
            "while the entry that read fine is not complained about: {raised}"
        );
        assert_eq!(
            configured.key_bindings.resolve(None, "<M-x>"),
            Some(Resolved::Act(Action::ComposerNewline)),
            "and Alt reaches the same binding however the config spells it"
        );
    }
}
