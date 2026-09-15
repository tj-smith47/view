//! What `[ui] theme` does at the one moment it can do anything: nvim's own
//! `VimEnter`, after the user's config has finished sourcing.
//!
//! There is one palette in a view session and nvim owns it. A named
//! colorscheme is therefore a command sent to nvim, not a set of colors held
//! here -- view's chrome is derived from the highlight table nvim reports
//! afterwards, by the same path that derives it for a scheme the user's own
//! config chose.

use crate::model::Model;
use crate::msg::{Effect, ReplyToken, ReplyValue, RpcCall};

/// The effects nvim's `VimEnter` is owed, in the order it is owed them.
///
/// The reply's place in this list is not when it is sent. The loop holds a
/// `VimEnter` answer behind everything else the pass produces -- these
/// calls, and the takeover and attach a native session adds after them --
/// because the engine's startup chunk drains what it parked on the channel
/// as soon as this answer frees it, and what it finds parked is what the
/// settled screen is drawn from (`view::runtime`'s `dispatch`). Nothing
/// here may wait on nvim in the meantime: it is inside the `rpcrequest`
/// this answers and cannot serve a call that blocks on it.
pub(super) fn on_vim_enter(model: &mut Model, token: ReplyToken) -> Vec<Effect> {
    let mut effects = vec![
        Effect::Reply {
            token,
            value: ReplyValue::Nil,
        },
        // this is also the first moment the reading is final -- nvim opens
        // the files it was given, and replays their swap files, before
        // `VimEnter` fires
        Effect::Rpc(RpcCall::ProbeSwapRecovery {
            generation: model.supervision.renew_swap_probe(),
        }),
    ];
    // last, and only when the user named one: a scheme applied any earlier
    // is one the config sourcing right up to this event would replace, and
    // `None` is not a scheme called "auto" -- the user named nothing, so
    // nothing is imposed and the chrome derives from wherever the config
    // ended
    if let Some(name) = &model.colorscheme {
        effects.push(Effect::Rpc(RpcCall::Colorscheme { name: name.clone() }));
    }
    effects
}

/// What a colorscheme nvim could not find owes the user: the name they wrote
/// and where they wrote it, so the fix is one edit away.
///
/// The derived chrome stands untouched. A name nvim cannot load changes
/// nothing about the highlight table it is already reporting, so the frame
/// after this notice is byte-for-byte the frame a session that named nothing
/// would have painted.
pub(super) fn on_colorscheme_missing(model: &mut Model, name: &str) -> Vec<Effect> {
    model.engine.record_native_notice(
        format!(
            "view: [ui] theme = {name} is a colorscheme nvim cannot find -- view's chrome is \
             derived from the colorscheme your own config ends on this run"
        ),
        false,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::msg::{EngineRequest, Msg};
    use crate::theme::Theme;
    use crate::update::update;

    /// Every colorscheme call one `VimEnter` produced, in order.
    fn schemes(effects: &[Effect]) -> Vec<&str> {
        effects
            .iter()
            .filter_map(|eff| match eff {
                Effect::Rpc(RpcCall::Colorscheme { name }) => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    fn vim_enter(model: &mut Model) -> Vec<Effect> {
        update(
            model,
            Msg::EngineRequest(EngineRequest::VimEnter {
                token: ReplyToken { msgid: 7 },
            }),
        )
    }

    /// One call, and only when a name was resolved. The count is the
    /// assertion, not merely the presence: `VimEnter` is dispatched from
    /// two places (the presink a fast config fires it into, and the
    /// steady-state loop a slow one reaches), and a scheme applied twice
    /// would run a user's colorscheme file twice on every launch.
    #[test]
    fn a_named_theme_issues_one_colorscheme_call() {
        let mut model = Model::new();
        model.colorscheme = Some("gruvbox".to_string());
        let effects = vim_enter(&mut model);
        assert_eq!(schemes(&effects), vec!["gruvbox"]);
        assert!(
            matches!(effects.first(), Some(Effect::Reply { .. })),
            "the reply still leads: nvim is blocked inside the request this answers, so a call \
             ahead of it waits on the engine that is waiting on view -- {effects:?}"
        );
    }

    /// The `auto` half of the same key. `None` is not a colorscheme called
    /// "auto": the user named nothing, so nothing is imposed and the chrome
    /// derives from wherever their own config ended.
    #[test]
    fn an_unnamed_theme_issues_no_colorscheme_call() {
        let mut model = Model::new();
        let effects = vim_enter(&mut model);
        assert!(
            schemes(&effects).is_empty(),
            "a session that named no scheme sent one anyway: {effects:?}"
        );
        assert!(
            effects
                .iter()
                .any(|eff| matches!(eff, Effect::Rpc(RpcCall::ProbeSwapRecovery { .. }))),
            "and everything `VimEnter` already owed is still owed: {effects:?}"
        );
    }

    #[test]
    fn a_missing_colorscheme_keeps_the_derived_theme_and_records_a_notice() {
        let mut model = Model::new();
        model.colorscheme = Some("nonexistent-scheme".to_string());
        let before = Theme::from_hl(model.engine.hl());

        let _ = update(
            &mut model,
            Msg::ColorSchemeMissing {
                name: "nonexistent-scheme".to_string(),
            },
        );

        let notices: Vec<String> = model
            .engine
            .messages
            .visible_lines(40)
            .into_iter()
            .map(|spans| spans.into_iter().map(|s| s.text).collect::<String>())
            .collect();
        assert_eq!(notices.len(), 1, "exactly one notice, got {notices:?}");
        for fact in ["nonexistent-scheme", "[ui] theme"] {
            assert!(
                notices[0].contains(fact),
                "{fact} is missing from {:?}",
                notices[0]
            );
        }
        assert_eq!(
            Theme::from_hl(model.engine.hl()),
            before,
            "a scheme nvim never loaded moved no highlight, so the chrome the frame derives is \
             the one an unnamed session would have derived"
        );
    }
}
