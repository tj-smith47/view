//! Telling a user, once, that a plugin is drawing over a surface view took
//! over -- which surface, which plugin as far as the window names one, and
//! the `view.toml` line that hands it back.
//!
//! One notice per claiming identity, aggregating every surface that
//! identity claims, rather than one per (identity, surface) pair. The
//! notices of one claimant share a family prefix, and
//! `record_native_notice_once` withdraws by family, so a second notice from
//! the same claimant would retract the first and leave the user reading
//! about one surface when two were taken. Aggregating is what makes the
//! second claim *add* to the line.

use crate::model::Model;
use crate::msg::Effect;
use crate::native::surfaces::{self, FloatSighting, Policy, Surface};

/// The opening every one of a claimant's notices shares, which is also the
/// family `record_native_notice_once` retracts on: everything after it is
/// wording that changes as the same claimant takes another surface, and the
/// name inside it is what keeps one plugin's notice from retracting
/// another's.
///
/// A float carrying no identity gets the anonymous family rather than a
/// guessed name (the plan's Deviation 3): a floating window records no
/// authorship, and the surface and the remedy are worth saying without one.
fn family(identity: Option<&str>) -> String {
    match identity {
        Some(identity) => format!("view: {identity} is drawing over "),
        None => "view: a plugin is drawing over ".to_string(),
    }
}

/// Answers one float sighting: nothing at all for a float drawing where
/// view does not, and otherwise the one notice its claimant owes the user.
///
/// Repeats are cheap rather than suppressed here: the watcher re-reports a
/// float that moved -- every keystroke of a cmdline session, for nvim-cmp --
/// and each repeat re-offers the same line to
/// `record_native_notice_once`, which leaves a standing line (and its own
/// expiry) exactly as it was. Suppressing the repeat one level earlier
/// instead is what made the notice unreadable: the keystroke that summons
/// the float is also the one that dismisses the toast it raises, so the
/// only line the session would ever have raised was already gone by the
/// time anything painted it.
pub(super) fn observe_float(model: &mut Model, float: &FloatSighting) -> Vec<Effect> {
    let Some(surface) = surfaces::claims(float, model) else {
        return Vec::new();
    };
    if surfaces::row(surface).map(|row| row.policy) != Some(Policy::Own) {
        return Vec::new();
    }
    let identity = float.identity().map(str::to_owned);
    let Some(claimed) = model
        .surface_conflicts
        .record(identity.as_deref(), surface)
        .map(<[Surface]>::to_vec)
    else {
        return Vec::new();
    };
    let family = family(identity.as_deref());
    let text = notice(&family, &claimed, model.config_was_read());
    model.dirty = true;
    model.engine.record_native_notice_once(&family, text)
}

/// The whole notice line for `claimed`, opening with its own `family` --
/// which `record_native_notice_once`'s `starts_with` withdrawal requires,
/// and why the family is prepended here rather than left to the caller.
fn notice(family: &str, claimed: &[Surface], config_was_read: bool) -> String {
    let rows: Vec<_> = claimed
        .iter()
        .filter_map(|surface| surfaces::row(*surface))
        .collect();
    let labels: Vec<&str> = rows.iter().map(|row| row.label).collect();
    let mut remedies: Vec<&str> = Vec::new();
    for remedy in rows.iter().filter_map(|row| row.remedy) {
        // two surfaces can share one switch (the palette returns both the
        // command line and the completion menu), and a line printed twice
        // reads as two things to do
        if !remedies.contains(&remedy) {
            remedies.push(remedy);
        }
    }
    let remedy = if !config_was_read {
        // never "set palette = false" on this leg: the file that would have
        // carried it is the one view could not read, so the user may have
        // written it already and been overruled by the fail-open that an
        // unreadable config takes (see `Model::config_was_read`)
        " view.toml could not be read this session, so every native feature \
         stayed on; fix that file and restart."
            .to_string()
    } else if remedies.is_empty() {
        String::new()
    } else {
        let them = if labels.len() > 1 { "them" } else { "it" };
        format!(" Set {} to give {them} back.", join(&remedies))
    };
    format!("{family}{}, which view owns.{remedy}", join(&labels))
}

/// `["a", "b", "c"]` as `"a, b and c"`: the reading order a sentence needs,
/// with no separator at all for the single-element case every notice today
/// actually takes.
fn join(parts: &[&str]) -> String {
    match parts {
        [] => String::new(),
        [only] => (*only).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::observe_float;
    use crate::events::UiEvent;
    use crate::model::Model;
    use crate::msg::Msg;
    use crate::native::ext::Ext;
    use crate::native::surfaces::{FloatAnchor, FloatSighting};
    use crate::update::update;

    /// The wire capture's own session: a 100x30 terminal whose nvim grid is
    /// 29 rows, with every surface externalized.
    fn captured_session() -> Model {
        let mut model = Model::with_term_size(100, 30);
        let _ = update(
            &mut model,
            Msg::Redraw(vec![UiEvent::GridResize {
                grid: 1,
                width: 100,
                height: 29,
            }]),
        );
        model
    }

    fn open_cmdline(model: &mut Model) {
        let _ = update(
            model,
            Msg::Redraw(vec![UiEvent::CmdlineShow {
                content: vec![(0, "e pre".to_string())],
                pos: 5,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            }]),
        );
    }

    /// nvim-cmp's cmdline menu, verbatim from the capture.
    fn cmp_cmdline_menu(filetype: &str) -> FloatSighting {
        FloatSighting {
            win: 1003,
            buf: 2,
            row: 26,
            col: 0,
            width: 20,
            height: 2,
            anchor: FloatAnchor::NorthWest,
            zindex: 1001,
            filetype: filetype.to_string(),
            name: String::new(),
        }
    }

    /// nvim-notify's toast, verbatim from the capture: `NE` at row 0,
    /// col 100, 50 by 3.
    fn toast(filetype: &str) -> FloatSighting {
        FloatSighting {
            win: 1008,
            buf: 4,
            row: 0,
            col: 100,
            width: 50,
            height: 3,
            anchor: FloatAnchor::NorthEast,
            zindex: 50,
            filetype: filetype.to_string(),
            name: String::new(),
        }
    }

    /// Every native line standing on `model`, in the order they were
    /// recorded.
    fn notices(model: &Model) -> Vec<String> {
        model
            .engine
            .messages
            .entries
            .iter()
            .filter(|entry| entry.is_native())
            .filter_map(|entry| entry.content.first().map(|(_, line)| line.clone()))
            .collect()
    }

    #[test]
    fn a_float_over_the_cmdline_is_named_once_with_the_line_that_resolves_it() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        assert_eq!(
            notices(&model),
            vec![
                "view: cmp_menu is drawing over the command line, which view owns. \
                 Set [native] palette = false to give it back."
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_float_with_no_identity_is_named_a_plugin() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmp_cmdline_menu(""));
        assert_eq!(
            notices(&model),
            vec![
                "view: a plugin is drawing over the command line, which view owns. \
                 Set [native] palette = false to give it back."
                    .to_string()
            ]
        );
    }

    #[test]
    fn one_identity_claiming_two_surfaces_raises_one_notice_naming_both() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmp_cmdline_menu("noice"));
        let _ = observe_float(&mut model, &toast("noice"));
        assert_eq!(
            notices(&model),
            vec![
                "view: noice is drawing over the command line and the message area, \
                 which view owns. Set [native] palette = false and \
                 [native] notifications = false to give them back."
                    .to_string()
            ],
            "one line, both surfaces, both remedies -- never two notices retracting each other"
        );
    }

    #[test]
    fn a_repeated_detection_replaces_its_wording_instead_of_stacking() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        for _ in 0..5 {
            let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        }
        assert_eq!(notices(&model).len(), 1, "a repeat is not a second notice");
        let _ = observe_float(&mut model, &toast("cmp_menu"));
        let standing = notices(&model);
        assert_eq!(standing.len(), 1, "the wider wording replaced the narrower");
        assert!(
            standing[0].contains("the command line and the message area"),
            "and it says what the earlier one did plus what is new: {standing:?}"
        );
    }

    /// The live failure this shape was built from: a native notice is a
    /// transient toast, and the keystroke that summons a cmdline completion
    /// menu is also the one that dismisses the toast the previous keystroke
    /// raised. A detector that spoke only the first time a pair was seen
    /// therefore said it once, into a frame the next key wiped, and stayed
    /// silent for the rest of the session while the plugin kept drawing.
    #[test]
    fn a_notice_a_keypress_dismissed_is_raised_again_by_the_next_sighting() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        assert_eq!(notices(&model).len(), 1);
        // what the user's next keystroke does to a toast that has already
        // had its frame
        model.engine.messages.note_flush();
        assert!(model.engine.messages.dismiss_transient_on_keypress(true));
        assert!(notices(&model).is_empty(), "the toast is gone");
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        assert_eq!(
            notices(&model),
            vec![
                "view: cmp_menu is drawing over the command line, which view owns. \
                 Set [native] palette = false to give it back."
                    .to_string()
            ],
            "the conflict is still true, so the line is on screen again"
        );
    }

    #[test]
    fn each_float_notice_starts_with_its_own_family() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        let _ = observe_float(&mut model, &toast("notify"));
        let standing = notices(&model);
        assert_eq!(
            standing.len(),
            2,
            "two claimants, two notices: {standing:?}"
        );
        assert!(standing
            .iter()
            .any(|line| line.starts_with("view: cmp_menu is drawing over ")));
        assert!(standing
            .iter()
            .any(|line| line.starts_with("view: notify is drawing over ")));
    }

    /// A claim on a surface this session handed back is not a conflict, so
    /// nothing is said at all -- the detector follows the `[native]`
    /// switches, never a constant.
    #[test]
    fn a_claim_on_a_surface_view_yielded_notices_nothing() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        model.attach_surfaces(vec![Ext::LineGrid, Ext::Tabline]);
        let effects = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        assert!(effects.is_empty());
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
        let effects = observe_float(&mut model, &toast("notify"));
        assert!(effects.is_empty());
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
    }

    /// The fail-open leg: view kept the surfaces because it could not read
    /// the config, so the user may already have written the very line a
    /// remedy would tell them to write.
    #[test]
    fn an_unread_config_is_never_told_to_set_a_line_it_may_already_carry() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        model.note_config_unread();
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        let standing = notices(&model);
        assert_eq!(standing.len(), 1);
        assert!(
            standing[0].starts_with("view: cmp_menu is drawing over the command line, "),
            "{standing:?}"
        );
        assert!(
            !standing[0].contains("Set [native]"),
            "a session that never read the file cannot tell the user to set a line in it: \
             {standing:?}"
        );
        assert!(
            standing[0].contains("view.toml could not be read"),
            "{standing:?}"
        );
    }

    #[test]
    fn a_float_drawing_where_view_does_not_says_nothing() {
        let mut model = captured_session();
        // telescope's results window, verbatim from the capture
        let picker = FloatSighting {
            win: 1010,
            buf: 6,
            row: 2,
            col: 11,
            width: 78,
            height: 21,
            anchor: FloatAnchor::NorthWest,
            zindex: 50,
            filetype: "TelescopeResults".to_string(),
            name: String::new(),
        };
        let effects = observe_float(&mut model, &picker);
        assert!(effects.is_empty());
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
    }

    /// The dispatch seam itself: the message a decoded bridge notification
    /// arrives as reaches the same answer `observe_float` gives.
    #[test]
    fn the_float_message_routes_through_update() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = update(&mut model, Msg::FloatObserved(cmp_cmdline_menu("cmp_menu")));
        assert_eq!(notices(&model).len(), 1, "{:?}", notices(&model));
    }
}
