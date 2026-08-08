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
use view_native::config::NativeConfig;
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
    pub(crate) fn load(config_path: Option<PathBuf>, channel_id: u64, model: &mut Model) -> Self {
        let cfg = match NativeConfig::load(config_path.as_deref()) {
            Ok(cfg) => cfg,
            Err(err) => {
                crate::vlog::log_with("native", || format!("config unreadable: {err}"));
                model.engine.messages.push_native(
                    format!("view: {err}; every native feature stays on this session"),
                    false,
                );
                model.dirty = true;
                NativeConfig::all_enabled()
            }
        };
        let plan = plan(&cfg, registry::features());
        Self {
            cfg,
            plan,
            config_path,
            record: paths::state_dir().map(|dir| paths::first_run_record(&dir)),
            channel_id,
            handed_over: false,
        }
    }

    /// Carries out `stage` against `model`, returning whatever it owes the
    /// engine.
    #[must_use]
    pub(crate) fn follow_up(&mut self, model: &mut Model, stage: Stage) -> Vec<Effect> {
        match stage {
            Stage::None => Vec::new(),
            Stage::VimEnter => self.take_over(),
            Stage::Claims => {
                self.announce(model);
                Vec::new()
            }
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
        effects.push(Effect::Rpc(mappings::register_plan(
            &self.cfg,
            self.channel_id,
        )));
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
    fn announce(&self, model: &mut Model) {
        let handovers = report(&self.plan, model.claimed_keys(), registry::features());
        if handovers.is_empty() {
            return;
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
        for notice in notices {
            model.engine.messages.push_native(notice, false);
            model.dirty = true;
        }
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
    /// read each other's record.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("view-native-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory must be creatable");
        dir.join("first-run.toml")
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
    fn the_takeover_registers_the_clipboard_provider_even_with_every_plan_entry_empty() {
        let mut session = NativeSession {
            cfg: NativeConfig::all_enabled(),
            plan: Vec::new(),
            config_path: None,
            record: None,
            channel_id: 9,
            handed_over: false,
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
        let record = scratch("claimed");
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
            effects.is_empty(),
            "a notice talks to the user, not to nvim"
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
}
