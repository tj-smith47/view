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
use crate::native::surfaces::{self, FloatSighting, Surface};
use crate::native::toast::HoldOutcome;

/// The opening every one of a float claimant's notices shares, which is also
/// the family `record_native_notice_once` retracts on: everything after it
/// is wording that changes as the same claimant takes another surface, and
/// the name inside it is what keeps one plugin's notice from retracting
/// another's.
///
/// A float carrying no identity gets the anonymous family rather than a
/// guessed name (the plan's Deviation 3): a floating window records no
/// authorship, and the surface and the remedy are worth saying without one.
fn family(identity: Option<&str>) -> String {
    match identity {
        Some(identity) => format!("view: {identity} is drawing over "),
        None => ANONYMOUS_FAMILY.to_string(),
    }
}

/// The family every float that names nobody shares.
const ANONYMOUS_FAMILY: &str = "view: a plugin is drawing over ";

/// The opening of the notice a named plugin class gets. A different verb
/// from the float families on purpose, and not decoration: the two families
/// have to be pairwise non-prefix or the withdrawal in
/// `record_native_notice_once` is cross-family (see
/// `no_two_native_notice_families_prefix_each_other`), and "is using" says
/// the truer thing anyway -- this claim comes from the plugin being loaded
/// at all, not from a window sighted covering some cells.
fn claimant_family(class: &str) -> String {
    format!("view: {class} is using ")
}

/// Answers the claimant probe: one notice per loaded claimant that still
/// has a surface view draws, and the resolution of the startup hold.
///
/// One notice per claimant, aggregating every surface it takes, rather than
/// one per (claimant, surface) pair -- the same rule the float notices
/// follow, for the same reason: the notices of one claimant share a family
/// and would retract each other.
///
/// Raised once and never re-recorded. There is no running count in the
/// wording, because a count that updates is a notice that re-records, and a
/// notice that re-records re-enters the toast stack and re-animates.
///
/// Called for every reading the probe takes, not just the first, and the
/// notice is raised from whichever reading first names a claimant. The hold
/// is the part that is one-shot: a reading that names nobody resolves it
/// `Release`, and a claimant that loads after that -- noice's own documented
/// spec is `event = "VeryLazy"`, so this is the ordinary case rather than the
/// unlucky one -- finds the collapse window already closed. The bound,
/// stated: everything that plugin raised before view could detect it has
/// already been shown, and view has no way to un-show it. What the late
/// reading still buys is the notice itself -- which surfaces went, and the
/// `view.toml` line that hands them back -- which is the obligation here.
/// The hold was only ever the anti-flash mechanism for the eager case.
pub(super) fn on_claimants_probed(model: &mut Model, probed: &[String]) -> Vec<Effect> {
    let mut effects = Vec::new();
    let mut named = false;
    for claimant in surfaces::probed_claimants(probed) {
        let claimed: Vec<Surface> = claimant
            .surfaces
            .iter()
            .copied()
            .filter(|surface| surfaces::view_draws(*surface, model))
            .collect();
        if claimed.is_empty() {
            continue;
        }
        named = true;
        let family = claimant_family(claimant.class);
        let text = notice(&family, &claimed, model.config_was_read(), true);
        effects.extend(model.engine.record_native_notice_sticky_once(&family, text));
        effects.extend(absorb_float_notices(model, claimant, &claimed));
        model.dirty = true;
    }
    let outcome = if named {
        HoldOutcome::Collapse
    } else {
        HoldOutcome::Release
    };
    model.dirty |= model.engine.messages.resolve_startup_hold(outcome);
    effects
}

/// Takes the anonymous float notice down to whatever the claimant notice
/// just raised does not already cover, so a default first launch gets one
/// notice per plugin rather than one per way view noticed the same one.
///
/// The float detector cannot attribute an unnamed window to a plugin -- that
/// is what "anonymous" means -- but it does not have to: a claim on a
/// surface a named claimant has already been reported for is the same
/// conflict with the same remedy, told by the notice that could not say who.
/// The claimant's own windows are the same case with a name on them:
/// [`SurfaceClaimant::identities`](surfaces::SurfaceClaimant::identities)
/// says which filetypes this plugin's floats present, so a sighting of one
/// is this plugin drawing on the surface its notice already names, not a
/// second plugin. Any other name keeps its line -- one notice per plugin is
/// the rule.
///
/// Ordinarily a no-op: the probe answers at the session's first idle
/// transition, well before a float scan can have been armed and waited out
/// its 150 ms, so there is usually nothing standing yet to narrow. This is
/// the other order -- a claimant that loaded late, or a float sighted during
/// a slow startup -- and it exists because the guard in [`observe_float`]
/// only covers sightings that arrive after the notice.
fn absorb_float_notices(
    model: &mut Model,
    claimant: &surfaces::SurfaceClaimant,
    claimed: &[Surface],
) -> Vec<Effect> {
    model
        .surface_conflicts
        .note_covered(claimed, claimant.identities);
    let mut effects = Vec::new();
    for identity in std::iter::once(None).chain(claimant.identities.iter().copied().map(Some)) {
        let Some(rest) = model
            .surface_conflicts
            .narrow(identity)
            .map(<[Surface]>::to_vec)
        else {
            continue;
        };
        let family = family(identity);
        if rest.is_empty() {
            model.dirty |= model.engine.withdraw_native_notice(&family);
            continue;
        }
        let text = notice(&family, &rest, model.config_was_read(), false);
        effects.extend(model.engine.record_native_notice_sticky_once(&family, text));
    }
    effects
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
    let identity = float.identity().map(str::to_owned);
    let covered = model.surface_conflicts.covers(surface, identity.as_deref());
    if surfaces::absorbs(float, surface, model) && !covered {
        // ahead of the notice and ahead of the hidden-float filter below:
        // the window this hides is the one view then has to keep reading,
        // and a claimant already named (`covered`) is a plugin whose own
        // notice stands -- hiding one of its windows would leave a user
        // reading "noice.nvim is using the command line" beside a command
        // line whose menu view had quietly taken over
        return absorb(model, float, surface);
    }
    if float.hidden {
        // a window with its `hide` flag set draws nothing, so it covers
        // nothing; the scan reports it only so an absorption can keep
        // reading the rows behind it (`FloatSighting::hidden`)
        return Vec::new();
    }
    if !surfaces::view_draws(surface, model) {
        return Vec::new();
    }
    if covered {
        // a named claimant's notice already says this surface is taken, says
        // who by, and carries the same `[native]` line as the remedy; a
        // second box -- one that cannot even say who, or one spelling a
        // filetype that claimant's own windows present -- is the same
        // conflict counted twice
        return Vec::new();
    }
    raise_notice(model, identity.as_deref(), surface)
}

/// Answers one sighting of a float view absorbs rather than reports: the
/// hide that stops two menus stacking, and the read that fills the palette
/// with what the hidden one was showing.
///
/// The read is an effect whose reply is a `Msg`, never a read taken here:
/// this runs on the same `update()` the keystrokes run on, and a paint that
/// waited on an RPC would make every cmdline keystroke cost a round trip.
///
/// No `dirty` on this path. A standing menu is re-sighted at the scan's own
/// rate for as long as a user reads it, and the frame owes a repaint only
/// when the rows actually move -- which is decided where they arrive
/// ([`on_float_rows`]), not here.
fn absorb(model: &mut Model, float: &FloatSighting, surface: Surface) -> Vec<Effect> {
    let step = model
        .engine
        .float_absorption
        .observe(float.win, float.hidden, float.identity());
    match step {
        surfaces::AbsorbStep::HideThenRead => vec![
            Effect::Rpc(crate::msg::RpcCall::SetFloatHidden {
                win: float.win,
                hide: true,
            }),
            Effect::Rpc(crate::msg::RpcCall::ReadFloatRows { win: float.win }),
        ],
        surfaces::AbsorbStep::Read => {
            vec![Effect::Rpc(crate::msg::RpcCall::ReadFloatRows {
                win: float.win,
            })]
        }
        // the disclosed failure mode: view could not hold the surface, so
        // the user is told which plugin took it and which line hands it
        // back, rather than left reading two menus
        surfaces::AbsorbStep::Yield => raise_notice(model, float.identity(), surface),
        // somebody else's hide, on a window view has never taken: it draws
        // nothing, so there is nothing here to paint and nothing to report
        surfaces::AbsorbStep::Ignore => Vec::new(),
    }
}

/// Answers one [`Msg::FloatRows`](crate::msg::Msg::FloatRows): the rows a
/// hidden float was drawing become the palette's, or the absorption gives
/// up and the notice takes its place.
pub(super) fn on_float_rows(
    model: &mut Model,
    win: u64,
    hidden: bool,
    lines: Vec<String>,
    selected: Option<usize>,
) -> Vec<Effect> {
    let rows = crate::native::palette::AbsorbedRows { lines, selected };
    match model.engine.float_absorption.rows_read(win, hidden, rows) {
        surfaces::RowsOutcome::Absorbed { changed } => {
            model.dirty |= changed;
            Vec::new()
        }
        surfaces::RowsOutcome::Yield(identity) => {
            raise_notice(model, identity.as_deref(), Surface::Cmdline)
        }
        surfaces::RowsOutcome::Stale => Vec::new(),
    }
}

/// The one notice a float claimant owes the user, raised once per identity
/// and re-worded rather than repeated as that identity takes more surfaces.
///
/// Empty for a sighting that adds nothing: `SurfaceConflicts::record`
/// answers news only, which is what keeps a 6.7 Hz scan off the paint loop
/// for as long as a menu stands open.
fn raise_notice(model: &mut Model, identity: Option<&str>, surface: Surface) -> Vec<Effect> {
    if !surfaces::view_draws(surface, model) {
        return Vec::new();
    }
    let Some(claimed) = model
        .surface_conflicts
        .record(identity, surface)
        .map(<[Surface]>::to_vec)
    else {
        return Vec::new();
    };
    let family = family(identity);
    let text = notice(&family, &claimed, model.config_was_read(), false);
    // reaching here at all means the claim is news, so the wording is about
    // to change and the frame does owe a repaint
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
    // the same rule for the floats view took over instead of reporting: a
    // prefix that narrows to no candidates closes the menu while the command
    // line stays open, and the palette must stop offering what it was
    // showing rather than hold the last set that had any
    model.dirty |= model.engine.float_absorption.sweep();
    Vec::new()
}

/// Ends every absorption, because the command line the absorbed menus were
/// completing has closed: the palette gives up the rows, and every window
/// view hid to get them is shown again.
///
/// The teardown the sweep cannot do on its own: nvim-cmp closes its menu
/// from inside a non-nested `CmdlineLeave` callback, so no `WinClosed`
/// announces it, and the scan that would eventually miss the window is
/// armed by events a closed command line no longer produces. Without this
/// the next command line would open onto the previous one's candidates.
///
/// The un-hide is sent for every window still being absorbed, including the
/// ones that are already closed -- which, for the plugin this was built
/// against, is all of them: cmp's menu goes with the `CmdlineLeave` that
/// produced this very event, so the show lands on a window that no longer
/// exists and the chunk answers it by doing nothing. That is the cheap
/// half. The expensive half is the other order, where the window outlives
/// the command line: nothing else in the session would ever clear the flag,
/// and the user is left with a window that has stopped drawing and no way
/// to know why.
pub(super) fn cmdline_closed(model: &mut Model) -> Vec<Effect> {
    let painted = model.engine.float_absorption.rows().is_some();
    let shown = model.engine.float_absorption.forget();
    model.dirty |= painted || !shown.is_empty();
    shown
        .into_iter()
        .map(|win| Effect::Rpc(crate::msg::RpcCall::SetFloatHidden { win, hide: false }))
        .collect()
}

/// The whole notice for `claimed`, opening with its own `family` -- which
/// `record_native_notice_once`'s `starts_with` withdrawal requires, and why
/// the family is prepended here rather than left to the caller.
///
/// Three lines at most, broken on `\n` because that is the only break the
/// message box takes: `MessageEntry::lines` splits on it, and the layer that
/// sizes the box clips at the grid width rather than wrapping, so a remedy
/// pushed onto the end of the first sentence is a remedy the user cannot
/// read.
///
/// `startup_account` adds the third line, and only the claimant notice
/// passes it true: that notice is the account of a launch, and the history
/// is where everything else from that launch is. It is not conditional on
/// anything having actually been parked -- view's own startup lines are in
/// the ring on every launch, so the sentence is true whether or not the
/// hold caught a foreign one, and a user reading a box about their first
/// launch is owed the key that shows the rest of it. A float notice raised
/// mid-session is about a window that just opened, not about a launch, and
/// says nothing about the history.
fn notice(
    family: &str,
    claimed: &[Surface],
    config_was_read: bool,
    startup_account: bool,
) -> String {
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
        "\nview.toml could not be read this session, so every native feature \
         stayed on; fix that file and restart."
            .to_string()
    } else if remedies.is_empty() {
        String::new()
    } else {
        let them = if labels.len() > 1 { "them" } else { "it" };
        format!(
            "\nSet {} in view.toml to give {them} back.",
            join(&remedies)
        )
    };
    // `--` rather than the decided box's em dash: notice text is written to
    // the grid verbatim, and the charset a terminal can draw is a capability
    // reading the message layer does not take
    let history = if startup_account {
        "\nStartup messages from this launch are in the history -- <leader>fm."
    } else {
        ""
    };
    format!(
        "{family}{}, which view owns.{remedy}{history}",
        join(&labels)
    )
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

    use super::{absorb_float_notices, observe_float, surfaces};
    use crate::events::UiEvent;
    use crate::model::Model;
    use crate::msg::{Effect, Msg, RpcCall};
    use crate::native::ext::Ext;
    use crate::native::surfaces::{FloatAnchor, FloatSighting, Surface};
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
            hidden: false,
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
            hidden: false,
        }
    }

    /// The same menu as the scan sees it once view's hide has landed.
    fn hidden_menu(filetype: &str) -> FloatSighting {
        FloatSighting {
            hidden: true,
            ..cmp_cmdline_menu(filetype)
        }
    }

    /// One sighting of a cmdline float view cannot take over: the sighting
    /// itself, then the engine answering that the window is still not
    /// hidden.
    ///
    /// The path back onto the notice for every test below that is about the
    /// wording rather than about the absorption. A float over the command
    /// line is absorbed first now, and the notice is what a *failed*
    /// absorption leaves -- so a test that wants the line has to drive the
    /// failure rather than assume it.
    fn unhidable(model: &mut Model, float: &FloatSighting) -> Vec<Effect> {
        let mut effects = observe_float(model, float);
        effects.extend(update(
            model,
            Msg::FloatRows {
                win: float.win,
                hidden: false,
                lines: Vec::new(),
                selected: None,
            },
        ));
        effects
    }

    /// The rows one read of an absorbed float answers with.
    fn rows_arrive(model: &mut Model, win: u64, lines: &[&str], selected: Option<usize>) {
        let _ = update(
            model,
            Msg::FloatRows {
                win,
                hidden: true,
                lines: lines.iter().map(|line| (*line).to_string()).collect(),
                selected,
            },
        );
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
        let _ = unhidable(&mut model, &cmp_cmdline_menu("cmp_menu"));
        assert_eq!(
            notices(&model),
            vec![
                "view: cmp_menu is drawing over the command line, which view owns.\n\
                 Set [native] palette = false in view.toml to give it back."
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_float_with_no_identity_is_named_a_plugin() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = unhidable(&mut model, &cmp_cmdline_menu(""));
        assert_eq!(
            notices(&model),
            vec![
                "view: a plugin is drawing over the command line, which view owns.\n\
                 Set [native] palette = false in view.toml to give it back."
                    .to_string()
            ]
        );
    }

    #[test]
    fn one_identity_claiming_two_surfaces_raises_one_notice_naming_both() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = unhidable(&mut model, &cmp_cmdline_menu("noice"));
        let _ = observe_float(&mut model, &toast("noice"));
        assert_eq!(
            notices(&model),
            vec![
                "view: noice is drawing over the command line and the message area, \
                 which view owns.\nSet [native] palette = false and \
                 [native] notifications = false in view.toml to give them back."
                    .to_string()
            ],
            "one line, both surfaces, both remedies -- never two notices retracting each other"
        );
    }

    #[test]
    fn a_repeated_detection_replaces_its_wording_instead_of_stacking() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = unhidable(&mut model, &cmp_cmdline_menu("cmp_menu"));
        for _ in 0..4 {
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
            "view: cmp_menu is drawing over the command line, which view owns.\n\
             Set [native] palette = false in view.toml to give it back."
                .to_string(),
        ];
        let _ = unhidable(&mut model, &cmp_cmdline_menu("cmp_menu"));
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
        let _ = unhidable(&mut model, &cmp_cmdline_menu("cmp_menu"));
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
        let _ = unhidable(&mut model, &cmp_cmdline_menu("cmp_menu"));
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
            hidden: false,
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
        let _ = unhidable(&mut model, &cmp_cmdline_menu("cmp_menu"));
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
        let _ = unhidable(&mut model, &cmp_cmdline_menu("cmp_menu"));
        assert_eq!(notices(&model).len(), 1);
    }

    /// The dispatch seam itself: the message a decoded bridge notification
    /// arrives as reaches the same answer `observe_float` gives, and the
    /// message its reply arrives as reaches `on_float_rows`.
    #[test]
    fn the_float_message_routes_through_update() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let effects = update(&mut model, Msg::FloatObserved(cmp_cmdline_menu("cmp_menu")));
        assert!(
            matches!(
                effects.as_slice(),
                [
                    Effect::Rpc(RpcCall::SetFloatHidden {
                        win: 1003,
                        hide: true
                    }),
                    Effect::Rpc(RpcCall::ReadFloatRows { win: 1003 })
                ]
            ),
            "{effects:?}"
        );
        rows_arrive(&mut model, 1003, &[" preflight  Text "], Some(0));
        assert_eq!(
            model
                .engine
                .absorbed_rows()
                .map(|rows| rows.lines.clone())
                .unwrap_or_default(),
            vec![" preflight  Text ".to_string()]
        );
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
    }

    /// The ordering that is the whole point: a frame carrying view's palette
    /// rows *and* the plugin's own menu is the bug this feature exists to
    /// prevent, so the hide is issued before the read and the rows do not
    /// exist to paint until the read comes back -- which it cannot do before
    /// the hide it followed down the same connection has run.
    #[test]
    fn the_float_is_hidden_before_its_rows_are_painted() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let effects = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        let calls: Vec<&RpcCall> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Rpc(call) => Some(call),
                _ => None,
            })
            .collect();
        assert!(
            matches!(
                calls.as_slice(),
                [
                    RpcCall::SetFloatHidden {
                        win: 1003,
                        hide: true
                    },
                    RpcCall::ReadFloatRows { win: 1003 }
                ]
            ),
            "the hide goes first, and both name the sighted window: {calls:?}"
        );
        assert!(
            model.engine.absorbed_rows().is_none(),
            "nothing is painted from a float until its own read answers"
        );
        rows_arrive(&mut model, 1003, &["preflight"], None);
        assert!(model.engine.absorbed_rows().is_some());
    }

    /// The cadence bound, which is what keeps this off the key path: the
    /// window is hidden once and remembered, so a menu re-sighted at the
    /// scan's own rate for a whole cmdline session costs one hide and not
    /// one per keystroke.
    #[test]
    fn the_same_float_is_hidden_once_however_often_it_is_observed() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let mut hides = 0;
        let mut reads = 0;
        let mut others = 0;
        for key in 0..8 {
            // the first sighting is of a window nobody has hidden yet; every
            // one after it is the same window with view's own hide standing
            let float = if key == 0 {
                cmp_cmdline_menu("cmp_menu")
            } else {
                hidden_menu("cmp_menu")
            };
            for effect in update(&mut model, Msg::FloatObserved(float)) {
                match effect {
                    Effect::Rpc(RpcCall::SetFloatHidden { hide: true, .. }) => hides += 1,
                    Effect::Rpc(RpcCall::ReadFloatRows { .. }) => reads += 1,
                    _ => others += 1,
                }
            }
            rows_arrive(&mut model, 1003, &["preflight"], None);
        }
        assert_eq!(others, 0, "an absorbed float owes nothing else");
        assert_eq!(hides, 1, "one hide per window, never one per keystroke");
        assert_eq!(reads, 8, "the rows are re-read, because they change");
    }

    /// The flash bound, as a counter rather than as a test of the frame: view
    /// cannot hold a window against its owner, so a plugin that puts its menu
    /// back gets re-hidden -- twice, and then view stops absorbing it and
    /// says so instead.
    #[test]
    fn a_float_that_reshows_three_times_falls_back_to_the_notice() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        rows_arrive(&mut model, 1003, &["preflight"], None);

        for reshow in 1..=2 {
            let effects = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
            assert!(
                effects.iter().any(|effect| matches!(
                    effect,
                    Effect::Rpc(RpcCall::SetFloatHidden { hide: true, .. })
                )),
                "re-show {reshow} must be re-hidden"
            );
            assert!(notices(&model).is_empty(), "{:?}", notices(&model));
            rows_arrive(&mut model, 1003, &["preflight"], None);
        }

        let effects = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                Effect::Rpc(RpcCall::SetFloatHidden { hide: true, .. })
            )),
            "the third re-show stops the absorption rather than fighting on: {effects:?}"
        );
        assert_eq!(
            notices(&model),
            vec![
                "view: cmp_menu is drawing over the command line, which view owns.\n\
                 Set [native] palette = false in view.toml to give it back."
                    .to_string()
            ]
        );
        assert!(
            model.engine.absorbed_rows().is_none(),
            "and the palette stops showing rows off a menu that is on screen"
        );
    }

    /// The other failure mode the same disclosure covers: an engine that does
    /// not take the flag at all. The read that follows the hide reports the
    /// window still visible, and view stops rather than painting its rows
    /// under a menu that never went away.
    #[test]
    fn a_hide_failure_degrades_to_the_notice_and_never_paints_both() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        let _ = update(
            &mut model,
            Msg::FloatRows {
                win: 1003,
                hidden: false,
                lines: vec![" preflight  Text ".to_string()],
                selected: Some(0),
            },
        );
        assert!(
            model.engine.absorbed_rows().is_none(),
            "rows read off a window still on screen are the double chrome"
        );
        assert_eq!(
            notices(&model),
            vec![
                "view: cmp_menu is drawing over the command line, which view owns.\n\
                 Set [native] palette = false in view.toml to give it back."
                    .to_string()
            ],
            "the failure mode is telling the user, never drawing over them"
        );
        // and it stays yielded: a window view could not hide is not one to
        // keep trying to hide on every scan
        let effects = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                Effect::Rpc(RpcCall::SetFloatHidden { hide: true, .. })
            )),
            "{effects:?}"
        );
    }

    /// The teardown. nvim-cmp closes its menu from inside a non-nested
    /// `CmdlineLeave` callback, so no `WinClosed` announces it and no scan is
    /// armed to miss it -- without this the next command line would open onto
    /// the previous one's candidates.
    #[test]
    fn absorption_stops_when_the_cmdline_closes() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        rows_arrive(&mut model, 1003, &["preflight", "prefabricated"], Some(0));
        assert!(model.engine.absorbed_rows().is_some());

        model.dirty = false;
        let _ = update(&mut model, Msg::Redraw(vec![UiEvent::CmdlineHide]));
        assert!(
            model.engine.absorbed_rows().is_none(),
            "the palette must not reopen holding the last command line's candidates"
        );
        assert!(model.dirty, "rows left the screen: that is a repaint");
    }

    /// The teardown the command line cannot do, because it is still open: a
    /// prefix that narrows to no candidates closes the menu on its own (the
    /// capture's `:zqx` opens no window at all), and the scan's end marker is
    /// the only thing that says so.
    #[test]
    fn absorption_stops_when_the_menu_closes_under_an_open_cmdline() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = update(&mut model, Msg::FloatObserved(cmp_cmdline_menu("cmp_menu")));
        rows_arrive(&mut model, 1003, &["preflight"], None);
        let _ = update(&mut model, Msg::FloatSweep);
        assert!(
            model.engine.absorbed_rows().is_some(),
            "a scan that still finds the menu changes nothing"
        );

        model.dirty = false;
        let _ = update(&mut model, Msg::FloatSweep);
        assert!(model.engine.absorbed_rows().is_none(), "the menu closed");
        assert!(model.dirty, "rows left the screen: that is a repaint");
    }

    /// The same gate on the surface whose rows are the ones being taken.
    /// `[native] palette = false` detaches `ext_cmdline` and
    /// `ext_popupmenu` together, so no config reaches this state -- but
    /// `absorbs()` answers about a surface handed to it rather than about a
    /// rect it resolved itself, and an answer of "yes, take those rows" for
    /// a completion surface this session does not draw would be one its
    /// caller's invariants, not its own, were holding up.
    #[test]
    fn a_session_that_does_not_draw_completions_absorbs_nothing() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        model.attach_surfaces(vec![
            Ext::LineGrid,
            Ext::Cmdline,
            Ext::Messages,
            Ext::Tabline,
        ]);
        assert!(!surfaces::absorbs(
            &cmp_cmdline_menu("cmp_menu"),
            Surface::Cmdline,
            &model
        ));
    }

    /// The window the effects of one sighting would take over, if any.
    fn taken(effects: &[Effect]) -> Vec<u64> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Rpc(RpcCall::SetFloatHidden { win, hide: true })
                | Effect::Rpc(RpcCall::ReadFloatRows { win }) => Some(*win),
                _ => None,
            })
            .collect()
    }

    /// The rect is not evidence, and this is the float that proves it: a
    /// diagnostic float carrying `markdown` -- the filetype
    /// `CONTENT_FILETYPES` exists to refuse as a plugin's name -- standing
    /// in the two rows the command line keeps.
    ///
    /// Absorbed on the rect alone, its window goes dark and its text is
    /// offered to the user as completion candidates under `> :`. It is a
    /// conflict and it gets the conflict's own answer: the notice naming
    /// the surface and the `view.toml` line that hands it back.
    #[test]
    fn a_float_that_is_not_a_completion_menu_is_reported_never_taken() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let diagnostic = FloatSighting {
            win: 2001,
            filetype: "markdown".to_string(),
            ..cmp_cmdline_menu("markdown")
        };
        let effects = observe_float(&mut model, &diagnostic);
        assert!(
            taken(&effects).is_empty(),
            "a float view cannot read as a menu is never hidden and never read: {effects:?}"
        );
        assert!(
            model.engine.absorbed_rows().is_none(),
            "and its lines are never candidates"
        );
        assert_eq!(
            notices(&model),
            vec![
                "view: a plugin is drawing over the command line, which view owns.\n\
                 Set [native] palette = false in view.toml to give it back."
                    .to_string()
            ],
            "the claim is still a claim: it takes the notice path instead"
        );
    }

    /// The same rule against the float that reaches it from this repo's own
    /// plugin set. `compat/scenarios/fidget.toml` waits on grid row 28 for
    /// an LSP progress float to clear, so fidget's window is in the band the
    /// command line keeps for every `:w` a user types -- and it presents a
    /// widget filetype, so "the float names itself" is not the bound
    /// either. A progress spinner that goes dark for the rest of the session
    /// is the failure this shape is here to keep failing.
    #[test]
    fn an_lsp_progress_float_under_a_command_line_is_reported_never_taken() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let progress = FloatSighting {
            win: 2002,
            row: 27,
            col: 60,
            width: 40,
            height: 2,
            zindex: 45,
            filetype: "fidget".to_string(),
            ..cmp_cmdline_menu("fidget")
        };
        let effects = observe_float(&mut model, &progress);
        assert!(taken(&effects).is_empty(), "{effects:?}");
        assert!(model.engine.absorbed_rows().is_none());
        assert_eq!(
            notices(&model),
            vec![
                "view: fidget is drawing over the command line, which view owns.\n\
                 Set [native] palette = false in view.toml to give it back."
                    .to_string()
            ]
        );
    }

    /// A window that was already hidden when view first saw it is somebody
    /// else's -- a user's own config, another plugin -- and it is drawing
    /// nothing. Absorbing it would put its buffer on the wire at the scan's
    /// own rate and paint rows nobody can see beside them.
    #[test]
    fn a_float_hidden_before_view_ever_saw_it_is_left_alone() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let effects = observe_float(&mut model, &hidden_menu("cmp_menu"));
        assert!(effects.is_empty(), "{effects:?}");
        assert!(model.engine.absorbed_rows().is_none());
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
    }

    /// The other half of the hide: view gives the window back.
    ///
    /// The flag survives the plugin's own next reconfigure -- the capture
    /// measured 277 samples with no re-show -- so a window view hid and then
    /// stopped absorbing has nothing left in the session that would ever
    /// show it again. The absorption ending is the only moment view still
    /// knows the handle.
    #[test]
    fn a_window_that_outlives_the_command_line_is_shown_again() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        rows_arrive(&mut model, 1003, &["preflight"], None);

        let effects = update(&mut model, Msg::Redraw(vec![UiEvent::CmdlineHide]));
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Rpc(RpcCall::SetFloatHidden {
                    win: 1003,
                    hide: false
                })]
            ),
            "the window view hid is the window view shows: {effects:?}"
        );
    }

    /// And the case that is not owed one: the plugin closed its own menu,
    /// the scan that missed it dropped the window, and asking nvim to show a
    /// window nobody has is a call with an error at the end of it.
    #[test]
    fn a_menu_its_plugin_closed_is_never_asked_to_come_back() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = update(&mut model, Msg::FloatObserved(cmp_cmdline_menu("cmp_menu")));
        rows_arrive(&mut model, 1003, &["preflight"], None);
        // the scan that no longer finds it, which is what says it closed
        let _ = update(&mut model, Msg::FloatSweep);
        let _ = update(&mut model, Msg::FloatSweep);
        assert!(model.engine.absorbed_rows().is_none());

        let effects = update(&mut model, Msg::Redraw(vec![UiEvent::CmdlineHide]));
        assert!(effects.is_empty(), "{effects:?}");
    }

    /// The one line that keeps a 6.7 Hz scan off the paint loop. A standing
    /// menu is re-read once per scan for as long as a user reads it, and a
    /// read that brings back the rows the palette already has changed
    /// nothing on the screen -- a frame per read of those is view's paint
    /// loop keeping time with a plugin's debounce.
    #[test]
    fn a_re_read_that_brings_back_the_same_rows_asks_for_no_frame() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        rows_arrive(&mut model, 1003, &["preflight", "prefabricated"], Some(0));

        model.dirty = false;
        for scan in 1..=4 {
            let _ = update(&mut model, Msg::FloatObserved(hidden_menu("cmp_menu")));
            rows_arrive(&mut model, 1003, &["preflight", "prefabricated"], Some(0));
            assert!(
                !model.dirty,
                "scan {scan} repainted a screen whose candidates never moved"
            );
        }

        // and the read that does move them
        rows_arrive(&mut model, 1003, &["preflight"], Some(0));
        assert!(model.dirty, "rows that changed are rows worth a frame");
    }

    /// Absorption follows ownership, exactly as the notice does: with
    /// `[native] palette = false` the command line was never taken from the
    /// user's plugins, so view neither hides their window nor reads their
    /// buffer -- the detector and the absorber both follow the config rather
    /// than a constant.
    #[test]
    fn a_yielded_cmdline_absorbs_nothing_and_hides_nobody() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        model.attach_surfaces(vec![Ext::LineGrid, Ext::Messages, Ext::Tabline]);
        let effects = observe_float(&mut model, &cmp_cmdline_menu("cmp_menu"));
        assert!(effects.is_empty(), "{effects:?}");
        assert!(model.engine.absorbed_rows().is_none());
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
    }

    /// A float already accounted for by a named claimant's notice is that
    /// plugin's, and its windows are not view's to take over: a user reading
    /// "noice.nvim is using the command line" must not also find that command
    /// line's menu quietly absorbed.
    #[test]
    fn a_claimants_own_float_is_never_absorbed() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        probe(&mut model, &["noice"]);
        let effects = observe_float(&mut model, &cmp_cmdline_menu("noice"));
        assert!(effects.is_empty(), "{effects:?}");
        assert!(model.engine.absorbed_rows().is_none());
    }

    /// A hidden float draws nothing, so it is nobody's conflict -- the scan
    /// reports it only so an absorption can keep reading behind it.
    #[test]
    fn a_hidden_float_is_never_reported_as_drawing_over_anything() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        probe(&mut model, &["noice"]);
        let effects = observe_float(
            &mut model,
            &FloatSighting {
                hidden: true,
                ..toast("notify")
            },
        );
        assert!(effects.is_empty(), "{effects:?}");
        assert!(
            !notices(&model)
                .iter()
                .any(|line| line.starts_with("view: notify is drawing over ")),
            "{:?}",
            notices(&model)
        );
    }

    fn probe(model: &mut Model, loaded: &[&str]) {
        let _ = update(
            model,
            Msg::ClaimantsProbed(loaded.iter().map(|m| (*m).to_string()).collect()),
        );
    }

    /// The wording `compat/scenarios/noice.toml` reads back off a real
    /// screen, asserted here so a reworded notice fails in a unit test
    /// rather than in a 15-second pty wait.
    #[test]
    fn a_loaded_claimant_is_named_once_with_every_surface_it_takes() {
        let mut model = captured_session();
        probe(&mut model, &["noice"]);
        assert_eq!(
            notices(&model),
            vec![
                "view: noice.nvim is using the command line and the message area, \
                 which view owns.\n\
                 Set [native] palette = false and [native] notifications = false \
                 in view.toml to give them back.\n\
                 Startup messages from this launch are in the history -- <leader>fm."
                    .to_string()
            ]
        );
    }

    /// The lifetime the claimant notice is recorded with, pinned at the one
    /// call site that chooses it.
    ///
    /// Every compat `wait_for` finds this line well inside the four-second
    /// transient window, so recording it as an ordinary `"native"` notice
    /// keeps the whole battery green while turning the flagship line into
    /// something that leaves on a timer -- or on the next key the user
    /// presses, which is a key they press while reading it. Both exits are
    /// driven here.
    #[test]
    fn the_conflict_notice_outlives_the_transient_timeout() {
        let mut model = captured_session();
        let effects = update(&mut model, Msg::ClaimantsProbed(vec!["noice".to_string()]));
        let standing = notices(&model);
        assert_eq!(standing.len(), 1, "{standing:?}");
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, Effect::ScheduleToastExpiry { .. })),
            "a notice on an idle-expiry timer is one that leaves without being \
             answered: {effects:?}"
        );

        // the same fact read off the entry rather than off the effect list:
        // `Msg::ToastExpired` retains by id alone, so what keeps the timer
        // from ever being armed for this line is the kind it carries
        assert!(
            model
                .engine
                .messages
                .entries
                .iter()
                .all(crate::model::MessageEntry::is_persistent),
            "{:?}",
            notices(&model)
        );

        // and the keystroke that wipes a transient line
        model.engine.messages.note_flush();
        let _ = model.engine.messages.dismiss_transient_on_keypress(true);
        assert_eq!(notices(&model), standing, "a keystroke took it down");
    }

    /// The decided wording, line for line: the three-line shape the message
    /// box can actually draw -- one break per sentence, because the layer
    /// that sizes the box clips at the grid width instead of wrapping -- with
    /// the file the remedy goes in and the key that shows the rest of the
    /// launch.
    ///
    /// Ranged over whether the hold actually parked a foreign message,
    /// because neither clause is conditional on that: the remedy is a line in
    /// a file either way, and view's own startup lines are in the history on
    /// every launch, so a user reading this box is one key from the rest of
    /// it whatever the hold caught. The flagship launch is in fact the
    /// `false` leg -- noice raises its errors through nvim-notify directly,
    /// so nothing foreign is ever parked there.
    #[test]
    fn the_notice_breaks_at_every_sentence_so_the_remedy_is_on_screen() {
        for parked in [false, true] {
            let mut model = captured_session();
            if parked {
                let _ = update(
                    &mut model,
                    Msg::Redraw(vec![UiEvent::MsgShow {
                        kind: "echomsg".to_string(),
                        content: vec![(0, "noice.nvim: setup".to_string())],
                        replace_last: false,
                    }]),
                );
            }
            probe(&mut model, &["noice"]);
            let standing = notices(&model);
            assert_eq!(standing.len(), 1, "parked={parked}: {standing:?}");
            let lines: Vec<&str> = standing[0].split('\n').collect();
            assert_eq!(
                lines,
                vec![
                    "view: noice.nvim is using the command line and the message area, \
                     which view owns.",
                    "Set [native] palette = false and [native] notifications = false \
                     in view.toml to give them back.",
                    "Startup messages from this launch are in the history -- <leader>fm.",
                ],
                "parked={parked}"
            );
            for line in &lines {
                assert!(
                    line.chars().count() <= 98,
                    "a line the toast layer clips is a line the user cannot read: {line:?}"
                );
            }
        }
    }

    /// The composition the review mandated, ranged over both of the float
    /// detector's families on the default-launch path.
    ///
    /// One notice per plugin is the rule. noice's own health float carries
    /// `markdown` -- a document type, not a name -- so it reaches the
    /// anonymous family, and a second box about a surface the claimant
    /// notice already names would be noice reported twice. A float that
    /// does name itself is a different plugin, whose line says something
    /// the claimant's does not.
    #[test]
    fn a_default_launch_names_each_plugin_once_however_many_ways_view_noticed() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        probe(&mut model, &["noice"]);
        let claimant = notices(&model);
        assert_eq!(claimant.len(), 1, "{claimant:?}");

        // noice's own floats, unnamed, over the two surfaces its notice
        // already covers
        let _ = update(&mut model, Msg::FloatObserved(cmp_cmdline_menu("markdown")));
        let _ = update(&mut model, Msg::FloatObserved(toast("")));
        let _ = update(&mut model, Msg::FloatSweep);
        assert_eq!(
            notices(&model),
            claimant,
            "a float that cannot say who it belongs to, over a surface the claimant \
             notice already names, is that claimant reported twice"
        );

        // and a second plugin, which says who it is
        let _ = unhidable(&mut model, &cmp_cmdline_menu("cmp_menu"));
        let standing = notices(&model);
        assert_eq!(standing.len(), 2, "{standing:?}");
        assert!(
            standing
                .iter()
                .any(|line| line.starts_with("view: cmp_menu is drawing over ")),
            "one notice per plugin, not one per surface: {standing:?}"
        );
    }

    /// The ordering a lazy claimant actually launches in: noice's own
    /// documented spec is `event = "VeryLazy"`, so the session's first
    /// `SafeState` finds `package.loaded.noice` empty, and the hold resolves
    /// `Release` -- everything parked goes onto the stack, and anything the
    /// plugin raises after that toasts normally.
    ///
    /// The notice is the obligation; the hold is only the anti-flash
    /// mechanism for the eager case. So the bound this pins is the honest
    /// one: the messages raised before the late detection have already been
    /// seen, and the notice that says which surfaces went and how to get
    /// them back still arrives.
    #[test]
    fn a_claimant_that_loads_after_the_hold_resolved_still_gets_its_notice() {
        let mut model = captured_session();
        let _ = update(
            &mut model,
            Msg::Redraw(vec![UiEvent::MsgShow {
                kind: "echomsg".to_string(),
                content: vec![(0, "some other plugin: loaded".to_string())],
                replace_last: false,
            }]),
        );
        assert!(!model.engine.messages.held().is_empty());

        // the first idle transition, with nothing loaded yet
        probe(&mut model, &[]);
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
        assert!(
            model.engine.messages.held().is_empty(),
            "an empty reading releases the hold rather than stranding what it caught"
        );

        // and the reading the re-firing probe takes once the plugin loads
        probe(&mut model, &["noice"]);
        let standing = notices(&model);
        assert_eq!(standing.len(), 1, "{standing:?}");
        assert!(
            standing[0].starts_with("view: noice.nvim is using "),
            "{standing:?}"
        );
    }

    /// The same rule for a float that does name itself, when the name it
    /// carries is the claimant's own.
    ///
    /// noice sets `filetype = "noice"` on every window it opens
    /// (`lua/noice/view/nui.lua:41`), so its floats reach the *named* family
    /// rather than the anonymous one the guard above covers, and a default
    /// launch that both loads noice and sights one of its windows would say
    /// "noice.nvim is using the command line" and "noice is drawing over the
    /// command line" -- one plugin, two boxes, differing only in how view
    /// happened to notice it. Driven in both orders, because the claimant
    /// probe and the float scan race each other on a real launch.
    #[test]
    fn a_claimants_own_windows_are_that_claimant_rather_than_a_second_plugin() {
        for float_first in [false, true] {
            let mut model = captured_session();
            open_cmdline(&mut model);
            if float_first {
                let _ = update(&mut model, Msg::FloatObserved(cmp_cmdline_menu("noice")));
            }
            probe(&mut model, &["noice"]);
            if !float_first {
                let _ = update(&mut model, Msg::FloatObserved(cmp_cmdline_menu("noice")));
            }
            let standing = notices(&model);
            assert_eq!(standing.len(), 1, "float_first={float_first}: {standing:?}");
            assert!(
                standing[0].starts_with("view: noice.nvim is using "),
                "float_first={float_first}: {standing:?}"
            );
        }
    }

    /// The other order, which the sighting-time guard cannot cover: the
    /// unnamed float was already reported when the claimant answered.
    #[test]
    fn a_claimant_notice_absorbs_the_float_notice_already_standing() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = unhidable(&mut model, &cmp_cmdline_menu(""));
        assert_eq!(notices(&model).len(), 1);
        probe(&mut model, &["noice"]);
        let standing = notices(&model);
        assert_eq!(standing.len(), 1, "{standing:?}");
        assert!(
            standing[0].starts_with("view: noice.nvim is using "),
            "{standing:?}"
        );
    }

    /// The narrowing half of the same seam: an unnamed float claiming a
    /// surface no claimant covers keeps its line, re-worded to what is left.
    #[test]
    fn a_float_claim_the_notice_does_not_cover_survives_it() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = unhidable(&mut model, &cmp_cmdline_menu(""));
        let _ = update(&mut model, Msg::FloatObserved(toast("")));
        assert_eq!(notices(&model).len(), 1);
        // a claimant that takes only the command line back
        let claimant = crate::native::surfaces::SurfaceClaimant {
            surfaces: &[Surface::Cmdline],
            ..*crate::native::surfaces::SURFACE_CLAIMANTS
                .first()
                .expect("the shipped table has a row")
        };
        let _ = absorb_float_notices(&mut model, &claimant, &[Surface::Cmdline]);
        let standing = notices(&model);
        assert_eq!(standing.len(), 1, "{standing:?}");
        assert!(
            standing[0].starts_with("view: a plugin is drawing over the message area,"),
            "{standing:?}"
        );
    }

    #[test]
    fn a_claimant_this_session_did_not_load_says_nothing() {
        let mut model = captured_session();
        probe(&mut model, &[]);
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
        probe(&mut model, &["telescope"]);
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
    }

    /// A claimant is only a conflict for the surfaces this session still
    /// draws, so a config that already handed them back is told nothing.
    #[test]
    fn a_claimant_whose_surfaces_view_yielded_says_nothing() {
        let mut model = captured_session();
        model.attach_surfaces(vec![Ext::LineGrid, Ext::Tabline]);
        probe(&mut model, &["noice"]);
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
    }

    #[test]
    fn a_claimant_notice_on_an_unread_config_names_the_file_not_the_line() {
        let mut model = captured_session();
        model.note_config_unread();
        probe(&mut model, &["noice"]);
        let standing = notices(&model);
        assert_eq!(standing.len(), 1);
        assert!(!standing[0].contains("Set [native]"), "{standing:?}");
        assert!(
            standing[0].contains("view.toml could not be read this session"),
            "{standing:?}"
        );
    }

    /// Every family a native notice is recorded under, instantiated
    /// adversarially: the identities and paths inside three of them come
    /// from the user's own session, so a collision is something a filetype
    /// or a filename can cause rather than only a future edit here.
    ///
    /// Withdrawal is `starts_with`, so a family that prefixes another
    /// retracts the other's line -- a plugin's notice silently cancelling
    /// the one about a file that vanished under an unsaved buffer.
    fn every_family() -> Vec<String> {
        let mut families = vec![super::ANONYMOUS_FAMILY.to_string()];
        // what a plugin can call a float, through the wire boundary rather
        // than around it: what a family may be built from is exactly what
        // `identity` accepts, and these are the spellings that would spell
        // another family if it accepted them
        for filetype in [
            "",
            "a plugin",
            "a plugin is drawing over ",
            "file /tmp/x",
            "noice.nvim",
            "view",
            "x",
        ] {
            let sighting = FloatSighting {
                filetype: filetype.to_string(),
                ..cmp_cmdline_menu(filetype)
            };
            families.push(super::family(sighting.identity()));
        }
        // the class is a compile-time constant, never a session's own
        // string, so the shipped table plus the names a future row would
        // plausibly carry is the whole population
        for class in crate::native::surfaces::SURFACE_CLAIMANTS
            .iter()
            .map(|claimant| claimant.class)
            .chain(["noice", "notify", "telescope.nvim", "view"])
        {
            families.push(super::claimant_family(class));
        }
        // and what a user can call a file. The path is the one part of a
        // family that stays under the user's control after the boundary
        // above -- a path has no charset to hold it to -- so the terminator
        // is what separates these, and the case it is there for is the pair
        // where one path opens the other. Its bound, stated: a path that
        // embeds another standing notice's whole opening, terminator
        // included, still prefixes it.
        for path in [
            "/proj/src/lib.rs",
            "/proj/src/lib.rs.bak",
            "/proj/a",
            "/proj/a b",
            "/proj/a/b",
            "/proj/view",
            "/proj/file",
            "a plugin",
            "a plugin b",
        ] {
            families.push(crate::update::watch::file_notice_family(
                std::path::Path::new(path),
            ));
        }
        families.sort();
        families.dedup();
        families
    }

    /// The population `every_family` has to walk, bound to the crate rather
    /// than to whoever last remembered to extend the list.
    ///
    /// A notice family is produced by exactly one shape of function -- named
    /// `family` or ending `_family` -- and a family the walk never
    /// instantiates is a family no collision test ever sees. This fails the
    /// moment a new producer is written, which is the point at which joining
    /// the walk is one line rather than an archaeology exercise.
    ///
    /// Mechanism honesty: the naming convention is what makes a producer
    /// reachable here. A family built inline, under no function of its own,
    /// is invisible to this -- and is also a family with no single place to
    /// change its wording, which review catches on other grounds.
    #[test]
    fn every_family_walks_every_family_producer_in_the_crate() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut producers: Vec<String> = Vec::new();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("the crate's own source tree") {
                let path = entry.expect("a readable directory entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("a readable source file");
                // production only: a test helper named `..._family` is a
                // fixture, not a family a notice is ever recorded under
                let text = text.split("\n#[cfg(test)]").next().unwrap_or_default();
                for line in text.lines() {
                    // a family producer is a free function returning the
                    // opening string itself; `pub(super) fn` and friends are
                    // why this looks for `fn ` rather than a line prefix
                    if !line.ends_with("-> String {") {
                        continue;
                    }
                    let Some(rest) = line.split_once("fn ").map(|(_, rest)| rest) else {
                        continue;
                    };
                    let Some(name) = rest.split('(').next() else {
                        continue;
                    };
                    if name == "family" || name.ends_with("_family") {
                        producers.push(name.to_string());
                    }
                }
            }
        }
        producers.sort();
        producers.dedup();
        assert!(
            producers.len() >= 3,
            "the walk found nothing, so it proves nothing: {producers:?}"
        );

        let this = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/update/surface_conflict.rs"),
        )
        .expect("this file");
        let body = this
            .split_once("fn every_family() -> Vec<String> {")
            .expect("the walk this test is about")
            .1;
        let body = body.split_once("\n    #[test]").expect("the walk's end").0;
        for producer in &producers {
            assert!(
                body.contains(producer.as_str()),
                "{producer} builds a notice family that `every_family` never instantiates, \
                 so no collision test ever sees it"
            );
        }
    }

    /// The one collision the terminator does not close, pinned rather than
    /// claimed in a comment.
    ///
    /// A path is the one part of a family that stays under the user's
    /// control past the charset guard, and nothing in a filesystem forbids a
    /// file whose name embeds another notice's whole opening, terminator
    /// included. Withdrawing the shorter one then retracts the longer one's
    /// line too.
    ///
    /// Left standing: closing it means either rejecting filenames view can
    /// otherwise watch perfectly well, or wording the notice around a case
    /// no user has. What is owed is that the bound is a fact this suite
    /// states, so a change that widens or closes it moves this test rather
    /// than passing unnoticed.
    #[test]
    fn a_path_that_spells_another_notices_opening_still_prefixes_it() {
        let inner = crate::update::watch::file_notice_family(std::path::Path::new("/proj/a"));
        let outer =
            crate::update::watch::file_notice_family(std::path::Path::new("/proj/a is /proj/b"));
        assert!(
            outer.starts_with(&inner),
            "the residual this states is gone -- {outer:?} no longer opens with {inner:?}, \
             so the collision is closed and this bound is stale"
        );
    }

    #[test]
    fn no_two_native_notice_families_prefix_each_other() {
        let families = every_family();
        for (i, one) in families.iter().enumerate() {
            for (j, other) in families.iter().enumerate() {
                if i == j {
                    continue;
                }
                assert!(
                    !one.starts_with(other.as_str()),
                    "withdrawing {other:?} would also retract {one:?}"
                );
            }
        }
    }

    #[test]
    fn every_native_notice_family_opens_the_same_way() {
        for family in every_family() {
            assert!(
                family.starts_with("view: "),
                "a family a user's own filetype or filename can spell must still be \
                 recognisable as view's: {family:?}"
            );
            assert!(
                family.ends_with(' '),
                "a family that does not end on a word boundary can prefix a longer \
                 one: {family:?}"
            );
        }
    }

    #[test]
    fn the_notice_text_starts_with_its_own_family() {
        let claimed = [Surface::Cmdline, Surface::Messages];
        for family in [
            super::ANONYMOUS_FAMILY.to_string(),
            super::family(Some("cmp_menu")),
            super::claimant_family("noice.nvim"),
        ] {
            for read in [true, false] {
                for parked in [true, false] {
                    let text = super::notice(&family, &claimed, read, parked);
                    assert!(text.starts_with(&family), "{text:?} is not in {family:?}");
                }
            }
        }
    }
}
