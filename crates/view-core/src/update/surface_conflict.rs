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
/// The watcher re-reports a float that moved -- every keystroke of a
/// cmdline session, for nvim-cmp -- and a repeat that adds no surface stops
/// at `SurfaceConflicts::record`, which answers news only. So a standing
/// claim costs a lookup per sighting and nothing else: no notice churn, and
/// no repaint asked of a screen that did not change.
///
/// The line is sticky for the same reason the repeat is answered: the
/// keystroke that summons the float is the keystroke that dismisses a
/// transient toast. Suppressing the repeat leaves a line raised once,
/// wiped, and never said again; answering the repeat over a transient line
/// leaves it blinking on and off at the scan rate for as long as the user
/// types. The conflict is true until the config changes, so the line stands
/// until it is replaced or deliberately dismissed.
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
    // reaching here at all means the claim is news -- `record` answers a
    // sighting that adds nothing with `None` above -- so the wording is about
    // to change and the frame does owe a repaint. The re-sighting that
    // changes no pixel never gets this far, which is what keeps a 6.7 Hz scan
    // off the paint loop for as long as a menu stands open.
    model.dirty = true;
    model.engine.record_native_notice_sticky_once(&family, text)
}

/// Answers the end of one float scan: every claimant the scan did not sight
/// has stopped drawing, so the line about it comes down.
///
/// The other half of the sticky notice. A line that stands until it is
/// dismissed would otherwise outlive the thing it describes -- and this one
/// is a box across the top rows, so an obsolete copy occludes the buffer for
/// the rest of the session. Withdrawal by family, on the same terms the
/// wording replacement uses, so a claimant's line comes down whichever of
/// its wordings is up.
pub(super) fn sweep_floats(model: &mut Model) -> Vec<Effect> {
    for identity in model.surface_conflicts.sweep() {
        let withdrew = model
            .engine
            .withdraw_native_notice(&family(identity.as_deref()));
        model.dirty |= withdrew;
    }
    Vec::new()
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

    /// The live failure this shape was built from, and the one the first fix
    /// for it produced. A cmdline session types a key every ~200 ms; each key
    /// dismisses whatever transient toast has had its frame, and arms the
    /// scan that sights the menu again ~150 ms later. So a transient line
    /// here is not "raised once" -- it is raised, wiped, raised, wiped, for
    /// as long as the user types, on the one path this feature exists to
    /// serve. This drives that whole cycle: every keystroke a user makes
    /// while the menu stands, with the sighting the keystroke arms, and the
    /// line has to be readable throughout.
    #[test]
    fn the_notice_stands_through_the_keystrokes_that_keep_summoning_the_float() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let expected = vec![
            "view: cmp_menu is drawing over the command line, which view owns. \
             Set [native] palette = false to give it back."
                .to_string(),
        ];
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        assert_eq!(notices(&model), expected);

        for key in 1..=8 {
            // the keystroke: a frame has been painted since the line landed,
            // which is the whole condition a transient dismissal needs
            model.engine.messages.note_flush();
            let _ = model.engine.messages.dismiss_transient_on_keypress(true);
            assert_eq!(
                notices(&model),
                expected,
                "keystroke {key} took the notice off the screen"
            );
            // and the scan that keystroke armed, 150 ms later
            model.dirty = false;
            let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
            assert_eq!(notices(&model), expected, "sighting {key} stacked a copy");
            assert!(
                !model.dirty,
                "sighting {key} asked for a repaint of a screen it did not change"
            );
        }

        // the way out is the deliberate one every sticky entry has
        assert!(model.engine.messages.dismiss_sticky());
        assert!(notices(&model).is_empty());
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

    /// What keeps a standing line from outliving what it says. The notice is
    /// sticky, and a sticky line about a menu that closed ten minutes ago is
    /// a box across the top of the buffer saying something untrue -- so a
    /// scan that no longer finds the float takes the line down with it, and
    /// the plugin drawing again raises it again.
    #[test]
    fn a_notice_comes_down_with_the_float_that_stopped_being_sighted() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = update(&mut model, Msg::FloatObserved(cmp_cmdline_menu("cmp_menu")));
        assert_eq!(notices(&model).len(), 1);

        // a scan that still finds it: the line stays
        let _ = update(&mut model, Msg::FloatObserved(cmp_cmdline_menu("cmp_menu")));
        let _ = update(&mut model, Msg::FloatSweep);
        assert_eq!(notices(&model).len(), 1, "the menu is still drawing");

        // the scan after the menu closed reports no float at all, and its
        // end marker is the only thing that says so
        model.dirty = false;
        let _ = update(&mut model, Msg::FloatSweep);
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
        assert!(model.dirty, "the box left the screen: that is a repaint");

        // and the same plugin drawing again is owed the line again
        let _ = update(&mut model, Msg::FloatObserved(cmp_cmdline_menu("cmp_menu")));
        assert_eq!(notices(&model).len(), 1);
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
