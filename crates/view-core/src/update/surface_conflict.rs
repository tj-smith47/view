//! Telling a user, once, that something else is drawing over a surface view
//! took over -- which surface, what the window or the channel names itself,
//! and the `view.toml` line that hands it back.
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
use crate::native::channels::Channel;
use crate::native::surfaces::{self, FloatSighting, Surface, Taken};
use crate::native::toast::HoldOutcome;

/// How long the engine's float watcher waits after the first arming event
/// before it walks the window list, and therefore the rate a float storm
/// is bounded to: one walk per window rather than one per event.
///
/// Held here rather than in the Lua chunk that defers on it, because the
/// value is read in three of this module's own explanations and was
/// spelled two ways once a fourth dropped it -- a reader sent to a
/// throttle with no name had nowhere to go for the number.
/// `view_engine`'s `REGISTER_BRIDGE_CHUNK` takes it as an argument.
pub const FLOAT_SCAN_THROTTLE: std::time::Duration = std::time::Duration::from_millis(150);

// tied at compile time rather than by comment: a cmdline completion menu
// is redrawn on a debounce of its own -- 60 ms on the stack the wire
// capture measured -- and a scan that ran before the menu it exists to see
// reports the window the keystroke before it opened
const _: () = assert!(
    FLOAT_SCAN_THROTTLE.as_millis() > 60,
    "FLOAT_SCAN_THROTTLE must outlast a completion menu's own debounce"
);

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

/// The opening of the launch box's line about the config's writes to
/// surfaces view draws, and the box's family whenever that line leads.
const HELD_FAMILY: &str = "view: your config also draws ";

/// The opening of the launch box's line naming the features view draws,
/// and the box's family when no held channel leads it.
const DRAWING_FAMILY: &str = "view: now drawing ";

/// The opening of the launch box's line listing the user keys view maps,
/// and the box's family when that line is all the box says.
const MAPPING_FAMILY: &str = "view: now mapping ";

/// Every family the launch box can open with, in the order its lines
/// stand. A launch shows one box, so raising it withdraws the others.
const LAUNCH_FAMILIES: [&str; 3] = [HELD_FAMILY, DRAWING_FAMILY, MAPPING_FAMILY];

/// What kind of thing a held channel was holding, which decides how the
/// history entry words it. The box names the channel alone for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Holder {
    /// A replaced function, which nvim names by its source location.
    Function,
    /// An option's value.
    Value,
}

/// The kind of holder `channel` carries, per the channel table.
fn holder_of(channel: &str) -> Holder {
    let replaced = crate::native::channels::CHANNELS
        .iter()
        .flat_map(|entry| entry.channels.iter())
        .any(|entry| matches!(entry, Channel::Replaced(name) if *name == channel));
    if replaced {
        Holder::Function
    } else {
        Holder::Value
    }
}

/// The first-run record key a held channel is told under.
fn announced_key(channel: &str) -> String {
    format!("held:{channel}")
}

/// Answers one report that a channel of a surface view draws was holding
/// something the config wrote, and has been set back.
///
/// Every report goes to the history with what the channel held. The box is
/// raised only for a channel this config has not been told about: the
/// config rewrites the same channels on every launch, and a box that
/// repeats every launch is one a user learns to skip. A channel told for
/// the first time joins the launch's one box and is recorded
/// ([`Effect::RecordAnnounced`]), so the next launch records it to the
/// history alone.
///
/// Raised from the holds' own reports
/// ([`crate::msg::RpcCall::HoldWindowOption`] and its siblings), so the
/// option has already been set back by the time this runs.
///
/// Silent for a channel no surface claims and for a surface this session
/// handed back, since neither is view's to report, and silent for a holder
/// that spells nothing.
pub(super) fn on_channel_held(model: &mut Model, channel: &str, holder: &str) -> Vec<Effect> {
    if holder.is_empty() {
        return Vec::new();
    }
    let Some(surface) = crate::native::channels::claimants_of(channel)
        .find(|surface| surfaces::view_draws(*surface, model))
    else {
        return Vec::new();
    };
    // a surface the table gives no row is a surface the notice can name
    // nothing about, and an empty label reads as a sentence view broke
    if surfaces::row(surface).is_none() {
        return Vec::new();
    }
    let mut effects = note_held(model, surface);
    model
        .engine
        .record_history_only(vec![(0, history_line(channel, holder, holder_of(channel)))]);
    let key = announced_key(channel);
    if model.surface_conflicts.is_announced(&key) {
        return effects;
    }
    if !model.surface_conflicts.tell_held(surface, channel) {
        return effects;
    }
    model.surface_conflicts.note_announced(key.clone());
    effects.extend(raise_launch_box(model));
    effects.push(Effect::RecordAnnounced { key });
    effects
}

/// Adds what a launch handed to view (the features it draws, the user keys
/// it maps) to the launch's one box.
///
/// Each item comes with its first-run record key, and one this config was
/// told at an earlier launch is left out. The caller writes the record,
/// which keys these by feature and key.
pub(super) fn on_taken_over(model: &mut Model, taken: Vec<(String, Taken)>) -> Vec<Effect> {
    let mut news = false;
    for (key, taken) in taken {
        if model.surface_conflicts.note_announced(key) {
            news |= model.surface_conflicts.tell_taken(taken);
        }
    }
    if !news {
        return Vec::new();
    }
    raise_launch_box(model)
}

/// Raises or re-words the launch's one box from everything it names so far.
///
/// The box opens with whichever of its lines comes first, so its family
/// changes as more is told; every other launch family is withdrawn first,
/// which keeps one box standing however the reports interleave.
fn raise_launch_box(model: &mut Model) -> Vec<Effect> {
    let Some((family, text)) = launch_notice(
        model.surface_conflicts.told(),
        model.surface_conflicts.taken(),
        model.config_was_read(),
        model.surface_conflicts.startup_window_open(),
    ) else {
        return Vec::new();
    };
    let mut reworded = false;
    for other in LAUNCH_FAMILIES.iter().filter(|other| **other != family) {
        reworded |= model.engine.withdraw_native_notice(other);
    }
    model.dirty = true;
    // a held channel is the config fighting view, which stands until the
    // user acts; a feature or a key view took is told and times out
    let mut effects = if model.surface_conflicts.told().is_empty() {
        model.engine.record_native_notice_once(family, text)
    } else {
        model.engine.record_native_notice_sticky_once(family, text)
    };
    // a plugin notifier already popped this box under its earlier opening,
    // and the same family re-worded is not sent to it again either
    if reworded {
        effects.retain(|e| !matches!(e, Effect::Rpc(crate::msg::RpcCall::Notify { .. })));
    }
    effects
}

/// The history's account of one report, which is the one place the value a
/// channel held is spelled in full.
///
/// A function's location drops the `@` `debug.getinfo` puts in front of a
/// file path.
fn history_line(channel: &str, holder: &str, held: Holder) -> String {
    match held {
        Holder::Function => format!(
            "view: your config set {channel} to the function at {}, and view set it back.",
            holder.strip_prefix('@').unwrap_or(holder)
        ),
        Holder::Value => {
            format!("view: your config set {channel} to {holder}, and view set it back.")
        }
    }
}

/// The launch box, and the family it opens with: what the config also
/// draws, the features view draws now, the user keys view maps now, the
/// lines that give all of it back, and where the launch's messages went.
/// `None` when there is nothing to name.
///
/// A surface is named by its label with the channels that reported it, so
/// the user reads both the surface they see and the option they wrote. The
/// keys stand in one sentence, each with what it does now. The last line
/// names the key that shows this launch's own messages, which are in the
/// history on every launch; a box raised after the user has acted is not
/// about a launch and leaves it out.
fn launch_notice(
    told: &[(Surface, Vec<String>)],
    taken: &[Taken],
    config_was_read: bool,
    startup: bool,
) -> Option<(&'static str, String)> {
    let rows: Vec<_> = told
        .iter()
        .filter_map(|(surface, channels)| {
            surfaces::row(*surface).map(|row| (row, channels.join(", ")))
        })
        .collect();
    let mut lines: Vec<(&'static str, String)> = Vec::new();
    let mut switches: Vec<&str> = Vec::new();
    let mut add_switch = |switch: &'static str| {
        if !switches.contains(&switch) {
            switches.push(switch);
        }
    };
    if !rows.is_empty() {
        let items: Vec<String> = rows
            .iter()
            .map(|(row, channels)| format!("{} ({channels})", row.label))
            .collect();
        let items: Vec<&str> = items.iter().map(String::as_str).collect();
        let them = if rows.len() > 1 { "them" } else { "it" };
        lines.push((
            HELD_FAMILY,
            format!("{}. view draws {them} now.", join(&items)),
        ));
        for switch in rows.iter().filter_map(|(row, _)| row.off_switch()) {
            add_switch(switch);
        }
    }
    // a feature whose channel the held line already names is told there
    let drawing: Vec<String> = taken
        .iter()
        .filter_map(|taken| match taken {
            Taken::Drawing { feature, .. } => Some(*feature),
            Taken::Key { .. } => None,
        })
        .filter(|feature| !rows.iter().any(|(row, _)| row.feature == Some(*feature)))
        .map(feature_label)
        .collect();
    if !drawing.is_empty() {
        let drawing_read: Vec<&str> = drawing.iter().map(String::as_str).collect();
        lines.push((DRAWING_FAMILY, format!("{}.", join(&drawing_read))));
    }
    let keys: Vec<String> = taken
        .iter()
        .filter_map(|taken| match taken {
            Taken::Key { lhs, action, .. } => Some(format!("{lhs} to {action}")),
            Taken::Drawing { .. } => None,
        })
        .collect();
    if !keys.is_empty() {
        let keys_read: Vec<&str> = keys.iter().map(String::as_str).collect();
        let theirs = if keys.len() > 1 {
            "Your own mappings of them are off."
        } else {
            "Your own mapping of it is off."
        };
        lines.push((MAPPING_FAMILY, format!("{}. {theirs}", join(&keys_read))));
    }
    for taken in taken {
        add_switch(taken.off_switch());
    }
    let (family, _) = lines.first()?;
    let family = *family;
    let remedy = give_back(
        &switches,
        rows.len() + drawing.len() + keys.len(),
        config_was_read,
    );
    let account = if startup {
        "\nStartup messages: <leader>fm"
    } else {
        ""
    };
    let body: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(index, (opening, rest))| {
            if index == 0 {
                format!("{opening}{rest}")
            } else {
                format!("{}{rest}", continued(opening))
            }
        })
        .collect();
    Some((family, format!("{}{remedy}{account}", body.join("\n"))))
}

/// A launch family's opening as a line under the box's first: the family
/// prefix dropped and the first letter raised.
fn continued(opening: &str) -> String {
    let bare = opening.strip_prefix("view: ").unwrap_or(opening);
    let mut chars = bare.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The name a feature is drawn under: the label of the first surface it
/// draws, or its registry id where no surface row names it.
fn feature_label(feature: &str) -> String {
    surfaces::SURFACES
        .iter()
        .find(|row| row.feature == Some(feature))
        .map_or_else(|| format!("the {feature}"), |row| row.label.to_string())
}

/// The line that names the switches handing `named` things back, on a line
/// of its own so it reads as the thing to do. Every notice about what view
/// took ends with it, so the switch is worded once.
fn give_back(switches: &[&str], named: usize, config_was_read: bool) -> String {
    if !config_was_read {
        return UNREAD_CONFIG.to_string();
    }
    if switches.is_empty() {
        return String::new();
    }
    let verb = if switches.len() > 1 { "give" } else { "gives" };
    let them = if named > 1 { "them" } else { "it" };
    format!("\n{} {verb} {them} back.", join(switches))
}

/// [`give_back`]'s line for a switch that protects what comes next and
/// brings back nothing already lost.
pub(super) fn keeps(switches: &[&str], what: &str) -> String {
    if switches.is_empty() {
        return String::new();
    }
    let verb = if switches.len() > 1 { "keep" } else { "keeps" };
    format!("\n{} {verb} {what}.", join(switches))
}

/// The remedy line on a session whose `view.toml` could not be read. Never
/// "set palette = false" on this leg: the file that would have carried it
/// is the one view could not read, so the user may have written it already
/// and been overruled by the fail-open an unreadable config takes (see
/// `Model::config_was_read`).
const UNREAD_CONFIG: &str = "\nview.toml could not be read this session, so every native feature \
     stayed at its default; fix that file and restart.";

/// Records that a channel of `surface` was found populated by a holder that
/// is not view: the complaint grace opens on the first such finding, and
/// every float line the channel notice now accounts for is narrowed or
/// withdrawn.
///
/// One conflict, one box. A float over the message area and a replaced
/// `vim.notify` are the same renderer drawing through two channels, and a
/// second line saying so with the same `[native]` remedy is that conflict
/// counted twice.
fn note_held(model: &mut Model, surface: Surface) -> Vec<Effect> {
    if !model.surface_conflicts.note_channel_held(surface) {
        return Vec::new();
    }
    let mut effects = Vec::new();
    // the bound on how long a holder's own complaints are still view's to
    // take down: they are not all raised by the time anyone can type, so
    // the keystroke alone would leave the late ones standing beside the
    // notice they duplicate
    if model.surface_conflicts.arm_complaint_grace() {
        effects.push(Effect::ScheduleComplaintGrace {
            after: super::COMPLAINT_GRACE,
            generation: model.surface_conflicts.engine_generation(),
        });
    }
    for (identity, rest) in model.surface_conflicts.narrow_to_held() {
        let family = family(identity.as_deref());
        if rest.is_empty() {
            model.dirty |= model.engine.withdraw_native_notice(&family);
            continue;
        }
        let text = notice(&family, &rest, model.config_was_read());
        effects.extend(model.engine.record_native_notice_sticky_once(&family, text));
        model.dirty = true;
    }
    effects
}

/// Answers one reading of the message area's replaced global
/// ([`crate::msg::Msg::NotifySinkRead`]): the channel is recorded as held
/// when somebody other than view stands at it, and the startup hold is
/// resolved.
///
/// Taken at the session's first idle transition and re-taken whenever the
/// answer moves, which is what makes it the reading a float placed during
/// startup waits for ([`classify_sink_holds`]).
///
/// The hold's own resolution is one-shot: a reading that finds nvim's
/// default at `vim.notify` releases it, and a notifier installed after that
/// finds the collapse window already closed. The bound, stated: everything
/// that renderer drew before view could detect it has already been shown,
/// and view has no way to un-show it. The hold was only ever the anti-flash
/// mechanism for the eager case.
pub(super) fn on_notify_sink_read(model: &mut Model, foreign: bool) -> Vec<Effect> {
    let mut effects = Vec::new();
    let held = foreign && surfaces::view_draws(Surface::Messages, model);
    if held {
        effects.extend(note_held(model, Surface::Messages));
        // whatever that renderer drew about view's defaults is on screen
        // already, and the next thing that would arm a scan is CursorHold
        // seconds away or the keystroke that ends the startup window
        effects.push(Effect::Rpc(crate::msg::RpcCall::ScanFloats));
    }
    let outcome = if held {
        HoldOutcome::Collapse
    } else {
        HoldOutcome::Release
    };
    model.dirty |= model.engine.messages.resolve_startup_hold(outcome);
    effects.extend(classify_sink_holds(model));
    effects
}

/// Answers one float sighting: nothing at all for a float drawing where
/// view does not, and otherwise the one notice its claimant owes the user.
///
/// The notice runs on nvim's own command line rather than on the one the
/// frame is painting: a line telling the user what took the command line is
/// wrong the moment a speculated `:` was.
///
/// The watcher re-reports a float that moved -- every keystroke of a
/// cmdline session, for a menu that follows the cursor -- and a repeat that
/// adds no surface stops at `SurfaceConflicts::record`, which answers news
/// only. So a standing claim costs a lookup per sighting and nothing else:
/// no notice churn, and no repaint asked of a screen that did not change.
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
    if surface == Surface::Cmdline && model.engine.cmdline.is_none() {
        // the command line this float is over is view's own guess at one,
        // and a sticky line never rests on a guess: the notice waits for
        // the `cmdline_show` that makes it true
        return Vec::new();
    }
    if float.hidden {
        // a window with its `hide` flag set draws nothing, so it covers
        // nothing
        return Vec::new();
    }
    if !surfaces::view_draws(surface, model) {
        return Vec::new();
    }
    if model.surface_conflicts.channel_held(surface) {
        // the channel notice already says this surface is taken, says what
        // was in the channel, and carries the same `[native]` line as the
        // remedy; a second box is the same conflict counted twice
        return take_complaint(model, float, surface);
    }
    raise_notice(model, float.identity(), surface)
}

/// Answers one `win_float_pos`: a float opened over a native surface whose
/// channel is already held by somebody else is kept off the screen before
/// the frame that would paint it, and its rows are asked for.
///
/// The sighting the float scan takes is the same judgment one round trip
/// later, which is a round trip after that window's first frame is already
/// on the terminal: the scan is armed by autocmd transitions and runs on
/// [`FLOAT_SCAN_THROTTLE`], a toast opened `noautocmd` fires no `WinNew`,
/// and a slide animation moves the window with `nvim_win_set_config`,
/// which arms nothing either. So a complaint drawn during a startup nobody
/// has typed into waits for the next unrelated arming event -- seconds away
/// on both of the configurations this was measured on. The placement event
/// is the zero-latency sighting, and this is the whole reason it is read
/// here.
///
/// The bar is [`take_complaint`]'s, with the identity half left out
/// because a placement carries none: the rect claims a surface view draws
/// ([`surfaces::claims_at`]), a channel of that surface is already reported
/// held, and the startup window or the complaint grace is still open. Every
/// other float -- a picker, a hover, a completion menu, anything outside
/// that window -- is classified in the same arithmetic and paints on the
/// frame it arrived for.
///
/// A holder is suspected as well as known: while the message area's
/// replaced global is unread the middle term cannot be evaluated at all, so
/// the float is held on the same terms and [`classify_sink_holds`] runs the
/// judgment over it when the reading lands.
///
/// The cost, stated: one round trip of delay for a benign float that lands
/// in the message area's corner while a channel of that surface is known
/// held, and nothing at all for every other float. The paint loop waits on
/// none of it -- the flag is model state, and the rows lift it.
pub(super) fn on_float_placed(
    model: &mut Model,
    grid: crate::grid::registry::GridId,
    win: u64,
    row: i64,
    col: i64,
) -> Vec<Effect> {
    if model.surface_conflicts.is_complaint(win) {
        // a window being animated sends a placement per step, and it is the
        // same window at every one of them
        let withheld = model.surface_conflicts.withholds_float(win, grid);
        model.dirty |= model.engine.withhold_float(grid, withheld);
        return Vec::new();
    }
    let Some((width, height)) = model.engine.grids().grid(grid).map(crate::grid::Grid::size) else {
        return Vec::new();
    };
    let Some(surface) = surfaces::claims_at(
        row,
        col,
        width,
        height,
        surfaces::FloatAnchor::NorthWest,
        model,
    ) else {
        return Vec::new();
    };
    if surface != Surface::Messages || !surfaces::view_draws(surface, model) {
        return Vec::new();
    }
    if !model.surface_conflicts.startup_window_open()
        && !model.surface_conflicts.within_complaint_grace()
    {
        return Vec::new();
    }
    if !model.surface_conflicts.channel_held(surface) {
        // the suspected half: until the reading arrives, "no holder is
        // known" and "no holder is there" are the same answer, and a timer
        // firing at a fixed offset from `VimEnter` can beat that round trip
        // over a slow link. Held on the same terms and classified by the
        // reading
        if model.surface_conflicts.hold_for_sink_read(win, grid) {
            model.dirty |= model.engine.withhold_float(grid, true);
        }
        return Vec::new();
    }
    if !model.surface_conflicts.claim_complaint(win, Some(grid)) {
        return Vec::new();
    }
    model.dirty |= model.engine.withhold_float(grid, true);
    vec![Effect::Rpc(crate::msg::RpcCall::ReadFloatRows { win })]
}

/// Puts every float the unread channel held off the screen through the
/// classification a float over a held channel takes at its placement, now
/// that the reading has named who stands there: over a surface whose
/// channel is held, the rows are asked for and the window is taken;
/// otherwise it goes back to the screen on the next frame.
///
/// The cost, stated: a benign float opened inside that reading's own round
/// trip waits for it before it paints. Nothing else changes -- a float
/// placed after the reading is judged by the held set alone, as before.
fn classify_sink_holds(model: &mut Model) -> Vec<Effect> {
    let mut effects = Vec::new();
    for (win, grid) in model.surface_conflicts.read_sink() {
        let taken = surfaces::view_draws(Surface::Messages, model)
            && model.surface_conflicts.channel_held(Surface::Messages)
            && (model.surface_conflicts.startup_window_open()
                || model.surface_conflicts.within_complaint_grace())
            && model.surface_conflicts.claim_complaint(win, Some(grid));
        if taken {
            effects.push(Effect::Rpc(crate::msg::RpcCall::ReadFloatRows { win }));
        } else {
            model.dirty |= model.engine.withhold_float(grid, false);
        }
    }
    effects
}

/// Starts the take-down of one float the channel notice already accounts
/// for: its text goes to the notification history, and the window goes,
/// once [`complaint_recorded`] has the lines.
///
/// The read first and the close second, never the close alone: spec 5.5
/// discards nothing, and a window closed before its buffer was read takes
/// the plugin's own account of the conflict with it.
///
/// Two bounds, and the take-down needs both.
///
/// The message area, because that is what a complaint is: a float over the
/// *command line* is a menu the user is typing at, and view's answer to one
/// of those is a notice, never a close.
///
/// And the time bound, which is the startup window -- ending at the first
/// key, click or paste
/// ([`SurfaceConflicts::startup_window_open`](surfaces::SurfaceConflicts::startup_window_open))
/// -- or the claimant-complaint grace that outlives it
/// ([`SurfaceConflicts::within_complaint_grace`](surfaces::SurfaceConflicts::within_complaint_grace)).
/// What that bound buys is that view never closes a window a user opened:
/// a float standing outside it is something the session asked for -- a log
/// view opened by hand among them -- and closing that would be view taking
/// a window out from under the person reading it. The grace exists because
/// a holder's own complaints are not all raised by the time anyone can
/// type, and inside it the rows have to read as a complaint
/// before anything is closed ([`on_float_rows`]) -- which bar applies is
/// fixed here, at the sighting, not when the reply lands.
fn take_complaint(model: &mut Model, float: &FloatSighting, surface: Surface) -> Vec<Effect> {
    if surface != Surface::Messages {
        return Vec::new();
    }
    if !model.surface_conflicts.startup_window_open()
        && !model.surface_conflicts.within_complaint_grace()
    {
        return Vec::new();
    }
    if !model.surface_conflicts.claim_complaint(float.win, None) {
        return Vec::new();
    }
    vec![Effect::Rpc(crate::msg::RpcCall::ReadFloatRows {
        win: float.win,
    })]
}

/// Finishes one take-down: the lines the float was drawing are recorded in
/// their own voice, and the window is closed.
///
/// Two destinations, parted by what the text is rather than by when it
/// arrived. A complaint about the surfaces view took
/// ([`SurfaceConflicts::reads_as_complaint`](surfaces::SurfaceConflicts::reads_as_complaint))
/// goes to the notification history and never to the toast stack: view's
/// own notice already says which surface went and which `view.toml` line
/// hands it back, and a second box would be that conflict counted twice.
/// Anything else is a notification that window was showing the user -- an
/// update summary is the case of record -- and it reaches the toast stack
/// in view's chrome
/// ([`record_seen_notification`](crate::model::EngineModel::record_seen_notification)),
/// because view withheld the window it was drawn in.
///
/// The blank rows a notify-style float pads its text with are dropped. They
/// are geometry -- the window's own top and bottom margin -- and a history
/// entry that opens with two empty lines reads as a message with nothing
/// in it.
fn complaint_recorded(model: &mut Model, win: u64, lines: &[String]) -> Vec<Effect> {
    let text = lines
        .iter()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim_matches('\n').to_string();
    let mut effects = if text.is_empty() {
        Vec::new()
    } else if surfaces::SurfaceConflicts::reads_as_complaint(lines) {
        model.engine.record_history_only(vec![(0, text)]);
        Vec::new()
    } else {
        model.engine.record_seen_notification(vec![(0, text)])
    };
    effects.push(Effect::Rpc(crate::msg::RpcCall::CloseFloat { win }));
    // the float was drawing over view's own cells until this call lands
    model.dirty = true;
    effects
}

/// Answers one [`Msg::FloatRows`](crate::msg::Msg::FloatRows): the lines a
/// withheld float was drawing, read before the window is closed.
///
/// `hidden` is the window's own flag as nvim reported it, and a reply about
/// a window nothing claimed is a read this session is no longer waiting on.
pub(super) fn on_float_rows(model: &mut Model, win: u64, lines: Vec<String>) -> Vec<Effect> {
    if !model.surface_conflicts.is_complaint(win) {
        return Vec::new();
    }
    // the grace's own bar, applied here because the rows are what it is
    // about, but decided at the sighting: a float sighted before anyone
    // had acted is a complaint by construction, and a key landing inside
    // this round trip does not turn it into a window the user opened
    if !model.surface_conflicts.claimed_unconditionally(win)
        && !surfaces::SurfaceConflicts::reads_as_complaint(&lines)
    {
        return release_float(model, win);
    }
    complaint_recorded(model, win, &lines)
}

/// Gives a withheld float back to the screen: the rows say a user opened
/// it, so it paints from the next frame exactly as an unclassified one
/// would have.
///
/// The claim on the window stays, which is what stops the scan's next
/// sighting from asking for the same rows again at its own cadence -- the
/// answer has been read, and it was "not view's to take".
fn release_float(model: &mut Model, win: u64) -> Vec<Effect> {
    if let Some(grid) = model.surface_conflicts.release_complaint(win) {
        model.dirty |= model.engine.withhold_float(grid, false);
    }
    Vec::new()
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
    let text = notice(&family, &claimed, model.config_was_read());
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
    Vec::new()
}

/// The whole notice for `claimed`, opening with its own `family`, which
/// `record_native_notice_once`'s `starts_with` withdrawal requires and why
/// the family is prepended here.
///
/// The switch sentence is [`give_back`]'s, the same one the launch box
/// ends with.
fn notice(family: &str, claimed: &[Surface], config_was_read: bool) -> String {
    let rows: Vec<_> = claimed
        .iter()
        .filter_map(|surface| surfaces::row(*surface))
        .collect();
    let labels: Vec<&str> = rows.iter().map(|row| row.label).collect();
    let mut switches: Vec<&str> = Vec::new();
    for switch in rows.iter().filter_map(|row| row.off_switch()) {
        // two surfaces can share one switch (the palette returns both the
        // command line and the completion menu), and a line printed twice
        // reads as two things to do
        if !switches.contains(&switch) {
            switches.push(switch);
        }
    }
    let remedy = give_back(&switches, labels.len(), config_was_read);
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
    use crate::msg::{Effect, Msg, RpcCall};
    use crate::native::ext::Ext;
    use crate::native::surfaces::{FloatAnchor, FloatSighting, Surface, Taken};
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
    fn cmdline_float(filetype: &str) -> FloatSighting {
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
            ..cmdline_float(filetype)
        }
    }

    /// One sighting of a cmdline float view cannot take over: the sighting
    /// itself, then the engine answering that the window is still not
    /// hidden.
    ///
    /// Every native line standing on `model`, in the order they were
    /// recorded.
    fn notices(model: &Model) -> Vec<String> {
        model
            .engine
            .messages
            .entries
            .iter()
            .filter(|entry| entry.is_native())
            .filter_map(|entry| entry.content().first().map(|(_, line)| line.clone()))
            .collect()
    }

    /// The window-local hold's report, seen from the user's side: the line
    /// names the surface, the option, and the switch that hands the surface
    /// back, and leaves the value to the history.
    #[test]
    fn a_held_channel_is_named_with_its_option_and_the_switch_that_returns_it() {
        let mut model = captured_session();
        model.attach_surfaces(vec![crate::native::ext::Ext::Tabline]);

        let _ = update(
            &mut model,
            Msg::ChannelHeld {
                channel: "winbar".to_string(),
                holder: "%{%v:lua.crumbs()%}".to_string(),
            },
        );

        let lines = notices(&model);
        assert_eq!(lines.len(), 1, "one box: {lines:?}");
        let line = &lines[0];
        for part in ["winbar", "the tab line", "native.tabline = false"] {
            assert!(
                line.contains(part),
                "the line must name `{part}`, and reads: {line}"
            );
        }
        assert!(!line.contains("crumbs"), "{line}");
    }

    /// The same report twice, which is what two windows holding one option
    /// produce.
    #[test]
    fn a_second_window_holding_the_same_channel_adds_no_second_line() {
        let mut model = captured_session();
        model.attach_surfaces(vec![crate::native::ext::Ext::Tabline]);
        for _ in 0..2 {
            let _ = update(
                &mut model,
                Msg::ChannelHeld {
                    channel: "winbar".to_string(),
                    holder: "%f".to_string(),
                },
            );
        }

        assert_eq!(notices(&model).len(), 1, "{:?}", notices(&model));
    }

    /// A config drawing one surface through several channels: barbecue
    /// writes `winbar`, bufferline writes `tabline`, and the switch that
    /// gives the row back is the same line in both boxes.
    #[test]
    fn a_second_channel_of_the_same_surface_adds_no_second_line() {
        let mut model = captured_session();
        model.attach_surfaces(vec![crate::native::ext::Ext::Tabline]);
        for (channel, holder) in [
            ("winbar", "%#barbecue_normal#"),
            ("tabline", "%!v:lua.nvim_bufferline()"),
            ("showtabline", "2"),
        ] {
            let _ = update(
                &mut model,
                Msg::ChannelHeld {
                    channel: channel.to_string(),
                    holder: holder.to_string(),
                },
            );
        }

        assert_eq!(notices(&model).len(), 1, "{:?}", notices(&model));
    }

    /// The sink reading lands first on a live launch and records the
    /// message area as held without raising anything, so the report that
    /// follows is still the first notice about that surface.
    #[test]
    fn a_sink_reading_leaves_the_report_that_follows_it_its_own_line() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        let _ = update(
            &mut model,
            Msg::ChannelHeld {
                channel: "vim.notify".to_string(),
                holder: "function <a.renderer>".to_string(),
            },
        );

        assert_eq!(notices(&model).len(), 1, "{:?}", notices(&model));
    }

    /// A channel of a surface this session handed back is nvim's to draw,
    /// so there is nothing to report.
    #[test]
    fn a_channel_of_a_surface_view_does_not_draw_is_left_unreported() {
        let mut model = captured_session();
        model.attach_surfaces(vec![crate::native::ext::Ext::LineGrid]);

        let _ = update(
            &mut model,
            Msg::ChannelHeld {
                channel: "winbar".to_string(),
                holder: "%f".to_string(),
            },
        );

        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
    }

    #[test]
    fn a_float_over_the_cmdline_is_named_once_with_the_line_that_resolves_it() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmdline_float("cmp_menu"));
        assert_eq!(
            notices(&model),
            vec![
                "view: cmp_menu is drawing over the command line, which view owns.\n\
                 native.palette = false gives it back."
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_float_with_no_identity_is_named_a_plugin() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmdline_float(""));
        assert_eq!(
            notices(&model),
            vec![
                "view: a plugin is drawing over the command line, which view owns.\n\
                 native.palette = false gives it back."
                    .to_string()
            ]
        );
    }

    #[test]
    fn one_identity_claiming_two_surfaces_raises_one_notice_naming_both() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmdline_float("noice"));
        let _ = observe_float(&mut model, &toast("noice"));
        assert_eq!(
            notices(&model),
            vec![
                "view: noice is drawing over the command line and the message area, \
                 which view owns.\nnative.palette = false and \
                 native.notifications = false give them back."
                    .to_string()
            ],
            "one line, both surfaces, both remedies -- never two notices retracting each other"
        );
    }

    #[test]
    fn a_repeated_detection_replaces_its_wording_instead_of_stacking() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmdline_float("cmp_menu"));
        for _ in 0..4 {
            let _ = observe_float(&mut model, &cmdline_float("cmp_menu"));
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

    /// The live failure this shape was built from. A cmdline session types a
    /// key faster than a notice expires, and each key arms the scan that
    /// sights the menu again one [`FLOAT_SCAN_THROTTLE`] later -- a cadence
    /// far inside a transient notice's own four seconds, so an expiring line
    /// here is raised, retired, raised, retired, for as long as the user
    /// types, on the one path this feature exists to serve. This drives that
    /// cycle and the line has to be readable throughout.
    #[test]
    fn the_notice_stands_through_the_sightings_that_keep_finding_the_float() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let expected = vec![
            "view: cmp_menu is drawing over the command line, which view owns.\n\
             native.palette = false gives it back."
                .to_string(),
        ];
        let _ = observe_float(&mut model, &cmdline_float("cmp_menu"));
        assert_eq!(notices(&model), expected);

        for key in 1..=8 {
            // the scan a keystroke arms, one FLOAT_SCAN_THROTTLE later
            model.dirty = false;
            let _ = observe_float(&mut model, &cmdline_float("cmp_menu"));
            assert_eq!(notices(&model), expected, "sighting {key} stacked a copy");
            assert!(
                !model.dirty,
                "sighting {key} asked for a repaint of a screen it did not change"
            );
        }

        // the way out that leaves the rest of the stack alone is `d` in
        // the message history, which retracts one family
        assert!(model
            .engine
            .withdraw_native_notice("view: cmp_menu is drawing over "));
        assert!(notices(&model).is_empty());
    }

    #[test]
    fn each_float_notice_starts_with_its_own_family() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmdline_float("cmp_menu"));
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
        let effects = observe_float(&mut model, &cmdline_float("cmp_menu"));
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
        let _ = observe_float(&mut model, &cmdline_float("cmp_menu"));
        let standing = notices(&model);
        assert_eq!(standing.len(), 1);
        assert!(
            standing[0].starts_with("view: cmp_menu is drawing over the command line, "),
            "{standing:?}"
        );
        assert!(
            !standing[0].contains("= false"),
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
        let _ = observe_float(&mut model, &cmdline_float("cmp_menu"));
        assert_eq!(notices(&model).len(), 1);

        // a scan that still finds it: the line stays
        let _ = update(&mut model, Msg::FloatObserved(cmdline_float("cmp_menu")));
        let _ = update(&mut model, Msg::FloatSweep);
        assert_eq!(notices(&model).len(), 1, "the menu is still drawing");

        // the scan after the menu closed reports no float at all, and its
        // end marker is the only thing that says so
        model.dirty = false;
        let _ = update(&mut model, Msg::FloatSweep);
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
        assert!(model.dirty, "the box left the screen: that is a repaint");

        // and the same plugin drawing again is owed the line again
        let _ = observe_float(&mut model, &cmdline_float("cmp_menu"));
        assert_eq!(notices(&model).len(), 1);
    }

    /// The dispatch seam itself: the message a decoded bridge notification
    /// arrives as reaches the same answer `observe_float` gives.
    #[test]
    fn the_float_message_routes_through_update() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let routed = update(&mut model, Msg::FloatObserved(cmdline_float("a.menu")));
        let lines = notices(&model);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("view: a.menu is drawing over "),
            "{lines:?}"
        );

        let mut direct = captured_session();
        open_cmdline(&mut direct);
        let called = observe_float(&mut direct, &cmdline_float("a.menu"));
        assert_eq!(format!("{routed:?}"), format!("{called:?}"));
        assert_eq!(notices(&direct), lines);
    }

    /// A window that was already hidden when view first saw it is somebody
    /// else's -- a user's own config, another plugin -- and it is drawing
    /// nothing, so there is nothing for view to report or to take.
    #[test]
    fn a_float_hidden_before_view_ever_saw_it_is_left_alone() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let effects = observe_float(&mut model, &hidden_menu("a.menu"));
        assert!(effects.is_empty(), "{effects:?}");
        assert!(notices(&model).is_empty(), "{:?}", notices(&model));
    }

    /// A hidden float draws nothing, so it is nobody's conflict -- the scan
    /// reports it only so an absorption can keep reading behind it.
    #[test]
    fn a_hidden_float_is_never_reported_as_drawing_over_anything() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        sink_read(&mut model, true);
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

    /// One reading of the message area's replaced global, as the bridge
    /// notification carrying it arrives: `true` when somebody other than
    /// view stands at `vim.notify`.
    fn sink_read(model: &mut Model, foreign: bool) {
        let _ = update(model, Msg::NotifySinkRead { foreign });
    }

    /// A notifier of the user's standing at `vim.notify`, as a live
    /// session delivers it: the hold names the function it set back, and
    /// the sink reading arrives on the same idle transition.
    fn notifier_takes_the_messages(model: &mut Model) -> Vec<Effect> {
        let mut effects = update(
            model,
            Msg::ChannelHeld {
                channel: "vim.notify".to_string(),
                holder: "function <a.renderer>".to_string(),
            },
        );
        effects.extend(update(model, Msg::NotifySinkRead { foreign: true }));
        effects
    }

    /// The startup hold's deadline, as the timer thread the running
    /// engine's attach armed would deliver it.
    fn expire_hold(model: &mut Model) {
        let expired = Msg::StartupHoldExpired {
            generation: model.surface_conflicts.engine_generation(),
        };
        let _ = update(model, expired);
    }

    /// The complaint grace's deadline, as the timer thread the running
    /// engine's probe reply armed would deliver it.
    fn expire_grace(model: &mut Model) {
        let expired = Msg::ComplaintGraceExpired {
            generation: model.surface_conflicts.engine_generation(),
        };
        let _ = update(model, expired);
    }

    fn key(model: &mut Model) {
        press(model, "j");
    }

    /// One keypress, as the terminal's own decode delivers it.
    fn press(model: &mut Model, notation: &str) {
        let _ = update(
            model,
            Msg::Key(crate::msg::Key {
                notation: notation.to_string(),
            }),
        );
    }

    /// The generation the one grace `effects` arms carries.
    fn armed_grace(effects: &[Effect]) -> u64 {
        let generations: Vec<u64> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::ScheduleComplaintGrace { generation, .. } => Some(*generation),
                _ => None,
            })
            .collect();
        assert_eq!(
            generations.len(),
            1,
            "one grace per naming reply: {effects:?}"
        );
        generations[0]
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
        let effects = notifier_takes_the_messages(&mut model);
        let standing = notices(&model);
        assert_eq!(standing.len(), 1, "{standing:?}");

        // the kind it carries is what keeps it off the slot queue, and
        // `Msg::ToastExpired` retires by that queue alone
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

        // the idle expiry a transient line would have owned: this kind is
        // handed none by the slot it never holds
        assert!(model.engine.messages.arm_top_slot().is_none());

        // the one timer it does own says only that it has now been up long
        // enough to have been read (`Messages::dismiss_read_sticky`), and
        // nothing about it leaves the screen when that lands
        let _ = update(
            &mut model,
            Msg::ToastExpired {
                id: read_window(&effects),
            },
        );
        assert_eq!(notices(&model), standing, "an idle timer took it down");
    }

    /// The reading window's own timer, off the effects the notice's record
    /// returned.
    fn read_window(effects: &[Effect]) -> crate::model::MessageId {
        let armed: Vec<crate::model::MessageId> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::ScheduleToastExpiry { id, after } => {
                    assert_eq!(*after, crate::native::toast::TRANSIENT_TOAST_TIMEOUT);
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        assert_eq!(armed.len(), 1, "{effects:?}");
        armed[0]
    }

    /// The decided wording, line for line: the surface with the channel and
    /// the function's location, the switch that gives it back, and the key
    /// that shows the rest of the launch.
    ///
    /// Ranged over whether anyone has typed yet, because only the last line
    /// is conditional on that: a notice raised mid-session is not about a
    /// launch and says nothing about the history.
    #[test]
    fn the_launch_box_reads_line_for_line() {
        for acted in [false, true] {
            let mut model = captured_session();
            if acted {
                key(&mut model);
            }
            let _ = update(
                &mut model,
                Msg::ChannelHeld {
                    channel: "vim.notify".to_string(),
                    holder: "function <a.renderer>".to_string(),
                },
            );
            let standing = notices(&model);
            assert_eq!(standing.len(), 1, "acted={acted}: {standing:?}");
            let lines: Vec<&str> = standing[0].split('\n').collect();
            let mut want = vec![
                "view: your config also draws the message area (vim.notify). view draws it now.",
                "native.notifications = false gives it back.",
            ];
            if !acted {
                want.push("Startup messages: <leader>fm");
            }
            assert_eq!(lines, want, "acted={acted}");
        }
    }

    /// A replaced function's holder is the path nvim gives for its source:
    /// the box names the channel alone, and the history spells the path
    /// without the `@` in front of it.
    #[test]
    fn a_function_holder_reaches_the_history_and_never_the_box() {
        let mut model = captured_session();
        let path = "/home/a/.local/share/nvim/lazy/a.renderer/lua/a/renderer/init.lua";
        let _ = update(
            &mut model,
            Msg::ChannelHeld {
                channel: "vim.notify".to_string(),
                holder: format!("@{path}"),
            },
        );
        let standing = notices(&model);
        assert_eq!(standing.len(), 1, "{standing:?}");
        for piece in ["/", ".lua", "renderer", "@"] {
            assert!(!standing[0].contains(piece), "{piece}: {}", standing[0]);
        }
        assert!(
            history(&model)
                .iter()
                .any(|line| line.contains(&format!("the function at {path},"))),
            "{:?}",
            history(&model)
        );
    }

    /// Every line the history holds on `model`, newest first.
    fn history(model: &Model) -> Vec<String> {
        model
            .engine
            .toast_history
            .entries()
            .flat_map(|entry| entry.lines())
            .collect()
    }

    /// Every channel a hold reports on, the options a hold covers included,
    /// which is the population a first-run key is written for.
    fn reported_channels() -> Vec<&'static str> {
        crate::native::channels::CHANNELS
            .iter()
            .flat_map(|entry| entry.channels.iter())
            .filter(|channel| {
                matches!(
                    channel,
                    crate::native::channels::Channel::Hold { .. }
                        | crate::native::channels::Channel::Covered { .. }
                        | crate::native::channels::Channel::Replaced(_)
                )
            })
            .map(|channel| channel.name())
            .collect()
    }

    /// A session drawing every surface a channel can report on.
    fn drawing_everything() -> Model {
        let mut model = captured_session();
        model.attach_surfaces(crate::native::ext::ALL.to_vec());
        model.statusline_enabled = true;
        model
    }

    fn held(model: &mut Model, channel: &str, holder: &str) -> Vec<Effect> {
        update(
            model,
            Msg::ChannelHeld {
                channel: channel.to_string(),
                holder: holder.to_string(),
            },
        )
    }

    /// A surface names its channels in the channel table's order, so a
    /// needle that finds `(vim.notify` finds it whichever channel of the
    /// message area reported first.
    #[test]
    fn a_surface_names_its_channels_in_table_order() {
        for order in [["cmdheight", "vim.notify"], ["vim.notify", "cmdheight"]] {
            let mut model = drawing_everything();
            for channel in order {
                let holder = if channel == "cmdheight" {
                    "1"
                } else {
                    "function <a.renderer>"
                };
                let _ = held(&mut model, channel, holder);
            }
            let boxes = notices(&model);
            assert_eq!(boxes.len(), 1, "{order:?}: {boxes:?}");
            assert!(
                boxes[0].contains("the message area (vim.notify, cmdheight)"),
                "{order:?}: {}",
                boxes[0]
            );
        }
    }

    /// A config rewrites the same channels on every launch, so a box about
    /// them is told once per config and the history carries every launch
    /// after it.
    #[test]
    fn a_channel_already_announced_under_this_config_goes_to_history_only() {
        let channels = reported_channels();
        assert!(channels.len() > 5, "the walk found nothing: {channels:?}");
        for channel in channels {
            let key = super::announced_key(channel);
            for announced in [false, true] {
                let mut model = drawing_everything();
                if announced {
                    model.seed_announced([key.clone()]);
                }
                let effects = held(&mut model, channel, "x");
                let recorded = effects.iter().any(
                    |effect| matches!(effect, Effect::RecordAnnounced { key: k } if *k == key),
                );
                let boxes = notices(&model);
                assert!(
                    history(&model)
                        .iter()
                        .any(|line| line.contains(channel) && line.contains("set it back")),
                    "{channel} announced={announced}: every report goes to the history"
                );
                // the history line stays off the stack whatever the startup
                // hold decides after it
                let _ = model
                    .engine
                    .messages
                    .resolve_startup_hold(crate::native::toast::HoldOutcome::Release);
                assert!(
                    !model.engine.messages.entries.iter().any(|entry| entry
                        .content()
                        .iter()
                        .any(|(_, line)| line.contains("set it back"))),
                    "{channel} announced={announced}: the history line reached the stack"
                );
                if announced {
                    assert!(boxes.is_empty(), "{channel}: {boxes:?}");
                    assert!(!recorded, "{channel}: an announced key is written again");
                } else {
                    assert_eq!(boxes.len(), 1, "{channel}: {boxes:?}");
                    assert!(boxes[0].contains(channel), "{channel}: {boxes:?}");
                    assert!(recorded, "{channel}: the first telling is not recorded");
                }
            }
        }
    }

    /// A plugin's complaint recorded while the startup hold is pending stays
    /// in the history when the hold releases what it parked onto the stack.
    #[test]
    fn a_complaint_recorded_under_the_startup_hold_never_reaches_the_stack() {
        let mut model = captured_session();
        let complaint = "`vim.notify` has been overwritten by another plugin?".to_string();
        let _ = super::complaint_recorded(&mut model, 1001, &[complaint]);
        let _ = model
            .engine
            .messages
            .resolve_startup_hold(crate::native::toast::HoldOutcome::Release);
        assert!(
            !model
                .engine
                .messages
                .entries
                .iter()
                .flat_map(|entry| entry.lines())
                .any(|line| line.contains("overwritten by another plugin")),
            "{:?}",
            model.engine.messages.entries
        );
        assert!(history(&model)
            .iter()
            .any(|line| line.contains("overwritten by another plugin")));
    }

    /// Every order of `items`.
    fn orders<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        if items.len() <= 1 {
            return vec![items.to_vec()];
        }
        let mut out = Vec::new();
        for (i, first) in items.iter().enumerate() {
            let mut rest = items.to_vec();
            rest.remove(i);
            for mut tail in orders(&rest) {
                tail.insert(0, first.clone());
                out.push(tail);
            }
        }
        out
    }

    /// However many channels report and in whatever order, a launch shows
    /// one box, and that box names every one of them.
    #[test]
    fn one_launch_raises_one_box_however_many_channels_report() {
        let pool = [
            ("vim.notify", "function <a.renderer>"),
            ("tabline", "%!v:lua.nvim_bufferline()"),
            ("statusline", "%{%v:lua.lualine()%}"),
            ("winbar", "%#barbecue_normal#"),
            ("cmdheight", "0"),
        ];
        for n in 1..=pool.len() {
            for order in orders(&pool[..n]) {
                let mut model = drawing_everything();
                for (channel, holder) in &order {
                    let _ = held(&mut model, channel, holder);
                }
                let boxes = notices(&model);
                assert_eq!(boxes.len(), 1, "{order:?}: {boxes:?}");
                for (channel, _) in &order {
                    assert!(boxes[0].contains(channel), "{order:?}: {}", boxes[0]);
                }
            }
        }
    }

    /// A status line value is a screenful of escapes that names no plugin,
    /// so the box names the option and the history keeps the value whole.
    #[test]
    fn a_held_value_never_reaches_the_toast() {
        let long = "%#Lualine_a#%{v:lua.x()}".repeat(40);
        for holder in [
            "%!v:lua.nvim_bufferline()",
            "%{%v:lua.require'lualine'.statusline()%}",
            "%{&ff}",
            long.as_str(),
        ] {
            for channel in ["statusline", "tabline", "winbar", "rulerformat"] {
                let mut model = drawing_everything();
                let _ = held(&mut model, channel, holder);
                let boxes = notices(&model);
                assert_eq!(boxes.len(), 1, "{channel} {holder}: {boxes:?}");
                for piece in ["%!", "%{", "%#", "v:lua"] {
                    assert!(!boxes[0].contains(piece), "{channel}: {}", boxes[0]);
                }
                assert!(
                    history(&model).iter().any(|line| line.contains(holder)),
                    "{channel}: the history must keep {holder:?} whole"
                );
            }
        }
        let replaced: Vec<&str> = crate::native::channels::CHANNELS
            .iter()
            .flat_map(|entry| entry.channels.iter())
            .filter_map(|entry| match entry {
                super::Channel::Replaced(name) => Some(*name),
                _ => None,
            })
            .collect();
        assert!(!replaced.is_empty(), "the walk found no replaced channel");
        for channel in replaced {
            let location = "/home/a/.local/share/nvim/lazy/a.renderer/lua/a/renderer/notify.lua";
            let mut model = drawing_everything();
            let _ = held(&mut model, channel, &format!("@{location}"));
            let boxes = notices(&model);
            assert_eq!(boxes.len(), 1, "{channel}: {boxes:?}");
            for piece in ["/", ".lua", "@"] {
                assert!(!boxes[0].contains(piece), "{channel}: {}", boxes[0]);
            }
            assert!(
                history(&model).iter().any(|line| line.contains(location)),
                "{channel}: the history must keep {location:?} whole"
            );
        }
    }

    /// The other order, which the sighting-time guard cannot cover: the
    /// unnamed float was already reported when the channel answered.
    #[test]
    fn a_channel_notice_withdraws_the_float_notice_already_standing() {
        let mut model = captured_session();
        let _ = observe_float(&mut model, &toast(""));
        assert_eq!(notices(&model).len(), 1);
        let _ = update(
            &mut model,
            Msg::ChannelHeld {
                channel: "vim.notify".to_string(),
                holder: "function <a.renderer>".to_string(),
            },
        );
        let standing = notices(&model);
        assert_eq!(standing.len(), 1, "{standing:?}");
        assert!(standing[0].starts_with(super::HELD_FAMILY), "{standing:?}");
    }

    /// The narrowing half of the same seam: an unnamed float claiming a
    /// surface the held channel does not draw keeps its line, re-worded to
    /// what is left.
    #[test]
    fn a_float_claim_the_notice_does_not_cover_survives_it() {
        let mut model = captured_session();
        open_cmdline(&mut model);
        let _ = observe_float(&mut model, &cmdline_float(""));
        let _ = update(&mut model, Msg::FloatObserved(toast("")));
        assert_eq!(notices(&model).len(), 1);
        let _ = update(
            &mut model,
            Msg::ChannelHeld {
                channel: "vim.notify".to_string(),
                holder: "function <a.renderer>".to_string(),
            },
        );
        let standing = notices(&model);
        assert_eq!(standing.len(), 2, "{standing:?}");
        assert!(
            standing
                .iter()
                .any(|line| line.starts_with("view: a plugin is drawing over the command line,")),
            "{standing:?}"
        );
    }

    /// noice's own startup complaint, over the message area its claimant
    /// notice already names: view reads it, files it in the notification
    /// history, closes the window, and leaves exactly one box on screen.
    ///
    /// The startup hold is resolved before the float is ever sighted, which
    /// is the live order and not a stricter setup than the product gets:
    /// the hold ends three seconds after attach, and the heavy fixture's
    /// noice raises this complaint several seconds later still
    /// (`VIEW_COMPAT_LOG`, the unaccommodated state). A take-down keyed on
    /// the hold fires for
    /// neither the real launch nor this test.
    #[test]
    fn a_claimants_own_startup_complaint_goes_to_the_history_and_the_window_goes() {
        let mut model = captured_session();
        let _ = notifier_takes_the_messages(&mut model);
        expire_hold(&mut model);
        let claimant = notices(&model);
        assert_eq!(claimant.len(), 1, "{claimant:?}");

        // the capture's own health float: `markdown`, so it names nobody,
        // padded with the blank rows nvim-notify's window draws
        let float = toast("markdown");
        let read = update(&mut model, Msg::FloatObserved(float.clone()));
        assert!(
            matches!(
                read.as_slice(),
                [Effect::Rpc(RpcCall::ReadFloatRows { win })] if *win == float.win
            ),
            "a covered claimant float is read before it is closed, or its \
             account of the conflict is what the close discards; {read:?}"
        );
        let closed = update(
            &mut model,
            Msg::FloatRows {
                win: float.win,
                lines: vec![
                    String::new(),
                    String::new(),
                    "`vim.notify` has been overwritten by another plugin?".to_string(),
                ],
            },
        );
        assert!(
            closed.iter().any(|effect| matches!(
                effect,
                Effect::Rpc(RpcCall::CloseFloat { win }) if *win == float.win
            )),
            "{closed:?}"
        );

        assert_eq!(
            notices(&model),
            claimant,
            "the plugin's complaint never joins view's notice on screen"
        );
        assert!(
            !model
                .engine
                .messages
                .entries
                .iter()
                .flat_map(|entry| entry.lines())
                .any(|line| line.contains("overwritten by another plugin")),
            "recorded, not stacked: the complaint is off the toast stack whatever \
             the startup hold resolved to; {:?}",
            model.engine.messages.entries
        );
        let filed: Vec<String> = model
            .engine
            .toast_history
            .entries()
            .flat_map(|entry| entry.lines())
            .collect();
        assert!(
            filed
                .iter()
                .any(|line| line == "`vim.notify` has been overwritten by another plugin?"),
            "nothing is discarded: the plugin's own words are in the history; {filed:?}"
        );
        assert!(
            !filed.iter().any(String::is_empty),
            "the blank rows are the window's margin, not the message; {filed:?}"
        );

        // the scan re-sights a standing float until the close lands, and a
        // second read would file the same complaint twice
        assert!(
            update(&mut model, Msg::FloatObserved(float)).is_empty(),
            "one read per window"
        );
    }

    /// The reply that names a claimant is not an editor transition, so
    /// nothing in the bridge would arm a scan for seconds -- and the floats
    /// the take-down exists for are on screen at that moment.
    #[test]
    fn naming_a_claimant_arms_one_float_scan() {
        let mut model = captured_session();
        let effects = update(&mut model, Msg::NotifySinkRead { foreign: true });
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Rpc(RpcCall::ScanFloats)))
                .count(),
            1,
            "{effects:?}"
        );

        let mut quiet = captured_session();
        let none = update(&mut quiet, Msg::NotifySinkRead { foreign: false });
        assert!(
            !none
                .iter()
                .any(|effect| matches!(effect, Effect::Rpc(RpcCall::ScanFloats))),
            "a reading that names nobody has no complaint to look for: {none:?}"
        );
    }

    /// A replacement engine issues window handles from 1000 again, so
    /// everything the take-down learned about the dead process's windows is
    /// a wrong answer about the new one's: a handle still marked a
    /// complaint would file a live window's rows into the history and close
    /// it, and a session still marked typed-at would let the replacement's
    /// own complaints stack beside the re-raised notice.
    #[test]
    fn a_replacement_engine_inherits_no_window_handle_and_no_keypress() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        expire_hold(&mut model);
        let float = toast("markdown");
        let _ = update(&mut model, Msg::FloatObserved(float.clone()));
        key(&mut model);
        assert!(model.surface_conflicts.is_complaint(float.win));
        assert!(!model.surface_conflicts.startup_window_open());

        assert!(model.surface_conflicts.within_complaint_grace());

        model.forget_engine_conflicts();

        assert!(
            !model.surface_conflicts.is_complaint(float.win),
            "the replacement's window {} would be read as the dead engine's complaint",
            float.win
        );
        assert!(
            model.surface_conflicts.startup_window_open(),
            "the replacement gets its own startup, and its own claimants complain again"
        );
        assert!(
            !model.surface_conflicts.within_complaint_grace(),
            "the grace was the dead engine's probe reply's; the replacement's reply arms its own"
        );
    }

    /// The dead engine's grace timer is still sleeping in its thread when
    /// the replacement's probe reply arms a grace of its own, and it wakes
    /// first. Its expiry carries the generation it was armed under, and the
    /// replacement's grace stays open until the expiry that carries its
    /// own -- or the late complaint the grace exists for is left standing
    /// beside the re-raised notice, seconds early.
    #[test]
    fn a_dead_engines_grace_expiry_does_not_close_the_replacements() {
        let mut model = captured_session();
        let dead = armed_grace(&notifier_takes_the_messages(&mut model));

        model.forget_engine_conflicts();
        let live = armed_grace(&notifier_takes_the_messages(&mut model));
        assert_ne!(
            dead, live,
            "two engines, one generation: nothing tells the expiries apart"
        );

        let _ = update(&mut model, Msg::ComplaintGraceExpired { generation: dead });
        assert!(
            model.surface_conflicts.within_complaint_grace(),
            "the dead engine's deadline closed the replacement's grace"
        );
        let _ = update(&mut model, Msg::ComplaintGraceExpired { generation: live });
        assert!(!model.surface_conflicts.within_complaint_grace());
    }

    /// The bar is fixed where the take-down is decided. A float sighted
    /// inside the startup window is a complaint by construction, and a key
    /// landing between the read and its reply does not re-open the question:
    /// rows quoting neither `vim.notify` nor a surface name -- a plugin that
    /// names the conflict in prose -- are still filed and the window still
    /// goes, rather than left standing with its handle marked as read so
    /// no later scan looks again.
    #[test]
    fn a_key_inside_the_read_round_trip_does_not_drop_a_complaint_the_sighting_qualified() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        assert!(model.surface_conflicts.startup_window_open());
        let float = toast("markdown");
        let read = update(&mut model, Msg::FloatObserved(float.clone()));
        assert!(
            matches!(
                read.as_slice(),
                [Effect::Rpc(RpcCall::ReadFloatRows { win })] if *win == float.win
            ),
            "{read:?}"
        );

        key(&mut model);
        assert!(!model.surface_conflicts.startup_window_open());

        let prose = "noice.nvim: this GUI is unsupported";
        assert!(
            !crate::native::surfaces::SurfaceConflicts::reads_as_complaint(&[prose.to_string()]),
            "the row has to carry no signature, or the reply-time bar would pass it anyway"
        );
        let closed = update(
            &mut model,
            Msg::FloatRows {
                win: float.win,
                lines: vec![prose.to_string()],
            },
        );
        assert!(
            closed.iter().any(|effect| matches!(
                effect,
                Effect::Rpc(RpcCall::CloseFloat { win }) if *win == float.win
            )),
            "the sighting qualified it; the keystroke inside the round trip dropped it: {closed:?}"
        );
        assert!(
            model
                .engine
                .toast_history
                .entries()
                .flat_map(|entry| entry.lines())
                .any(|line| line == prose),
            "filed in the plugin's own words"
        );
    }

    /// The bound the startup window alone cannot carry: a claimant that
    /// re-checks its own health on a timer raises complaints past any
    /// realistic first keystroke (noice's `vim.notify` line arrives seconds
    /// into a heavy launch, on a one-second interval), so the grace its
    /// probe reply arms is what takes those down. One grace per session,
    /// armed by the first reply that names anyone -- a second reply must
    /// not restart a deadline whose expiry would then close it early.
    #[test]
    fn the_first_reply_that_names_a_claimant_arms_one_grace() {
        let mut model = captured_session();
        let first = update(&mut model, Msg::NotifySinkRead { foreign: true });
        assert_eq!(
            first
                .iter()
                .filter(|effect| matches!(effect, Effect::ScheduleComplaintGrace { .. }))
                .count(),
            1,
            "{first:?}"
        );
        assert!(model.surface_conflicts.within_complaint_grace());

        let again = update(&mut model, Msg::NotifySinkRead { foreign: true });
        assert!(
            !again
                .iter()
                .any(|effect| matches!(effect, Effect::ScheduleComplaintGrace { .. })),
            "the running deadline is the grace; a second arming ends it early: {again:?}"
        );

        expire_grace(&mut model);
        assert!(!model.surface_conflicts.within_complaint_grace());
    }

    /// The complaint the fix round 1 timeline left standing: raised after
    /// the user has typed, inside the grace, and still view's to take down
    /// -- one box for one conflict is the spec's rule whether the second
    /// box arrives before the keystroke or four seconds after it.
    #[test]
    fn a_complaint_raised_after_the_first_key_is_taken_inside_the_grace() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        expire_hold(&mut model);
        key(&mut model);
        assert!(!model.surface_conflicts.startup_window_open());

        let float = toast("markdown");
        let read = update(&mut model, Msg::FloatObserved(float.clone()));
        assert!(
            matches!(
                read.as_slice(),
                [Effect::Rpc(RpcCall::ReadFloatRows { win })] if *win == float.win
            ),
            "{read:?}"
        );
        let closed = update(
            &mut model,
            Msg::FloatRows {
                win: float.win,
                lines: vec!["`vim.notify` has been overwritten by another plugin?".to_string()],
            },
        );
        assert!(
            closed.iter().any(|effect| matches!(
                effect,
                Effect::Rpc(RpcCall::CloseFloat { win }) if *win == float.win
            )),
            "{closed:?}"
        );
        assert!(
            model
                .engine
                .toast_history
                .entries()
                .flat_map(|entry| entry.lines())
                .any(|line| line.contains("overwritten by another plugin")),
            "the plugin's own account of the conflict is what the history keeps"
        );
    }

    /// And the price of that grace, bounded: once the user has acted, only
    /// the text says whether a float over the message area is a plugin
    /// complaining or a window someone opened. `:Noice` output at five
    /// seconds is the realistic case, and view neither files it nor closes
    /// it.
    #[test]
    fn a_window_the_user_opened_inside_the_grace_stays_open() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        expire_hold(&mut model);
        key(&mut model);

        let float = toast("markdown");
        let _ = update(&mut model, Msg::FloatObserved(float.clone()));
        let answered = update(
            &mut model,
            Msg::FloatRows {
                win: float.win,
                lines: vec!["2 messages  Ctrl-D to dismiss".to_string()],
            },
        );
        assert!(
            answered.is_empty(),
            "a window the user opened is theirs to close: {answered:?}"
        );
        assert!(
            model
                .engine
                .toast_history
                .entries()
                .flat_map(|entry| entry.lines())
                .all(|line| !line.contains("Ctrl-D to dismiss")),
            "nothing the user is reading is filed as a complaint"
        );
    }

    /// The residual the grace accepts, recorded so it is falsifiable: a
    /// window the user opened over the message area inside the grace whose
    /// rows quote one of view's surface names -- a `:Noice` log listing
    /// the health error -- reads as a complaint, and is filed and closed.
    /// The cost is a reopenable window whose text is in the history; the
    /// alternative, a second required token in the plugin's own English,
    /// fails toward two boxes when the plugin rewords. A later attempt to
    /// narrow the signature fails here by name.
    #[test]
    fn a_log_the_user_opened_inside_the_grace_that_quotes_a_surface_name_is_taken() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        expire_hold(&mut model);
        key(&mut model);
        assert!(!model.surface_conflicts.startup_window_open());
        assert!(model.surface_conflicts.within_complaint_grace());

        let float = toast("noice");
        let _ = update(&mut model, Msg::FloatObserved(float.clone()));
        let log = "12:00:01 ERROR  Noice can't work when the GUI has `ext_cmdline` enabled";
        let answered = update(
            &mut model,
            Msg::FloatRows {
                win: float.win,
                lines: vec![log.to_string()],
            },
        );
        assert!(
            answered.iter().any(|effect| matches!(
                effect,
                Effect::Rpc(RpcCall::CloseFloat { win }) if *win == float.win
            )),
            "the accepted cost of the signature, no longer accepted: {answered:?}"
        );
        assert!(
            model
                .engine
                .toast_history
                .entries()
                .flat_map(|entry| entry.lines())
                .any(|line| line == log),
            "what was closed under the user's hand is at least in the history"
        );
    }

    /// The three messages that mean the user has acted all end the startup
    /// conflict window, not the keyed one alone: a click or a paste is a
    /// session someone is driving, and view holding a licence to close
    /// windows there would take one down under a hand already on the mouse.
    #[test]
    fn a_click_and_a_paste_close_the_startup_window_the_way_a_key_does() {
        for msg in [
            Msg::Mouse(crate::msg::MouseInput {
                button: "left".to_string(),
                action: "press".to_string(),
                modifier: String::new(),
                row: 0,
                col: 0,
            }),
            Msg::Paste("hello".to_string()),
        ] {
            let mut model = captured_session();
            assert!(model.surface_conflicts.startup_window_open());
            let _ = update(&mut model, msg.clone());
            assert!(
                !model.surface_conflicts.startup_window_open(),
                "{msg:?} left the take-down armed"
            );
        }
    }

    /// The outer bound on the take-down: once the user has acted *and* the
    /// claimant's grace has run out, a float over a covered surface is
    /// something the session asked for, and view does not even read it.
    #[test]
    fn a_float_that_opens_after_the_grace_is_left_unread() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        key(&mut model);
        expire_grace(&mut model);
        assert!(
            update(&mut model, Msg::FloatObserved(toast("markdown"))).is_empty(),
            "a window the user opened is not view's to close"
        );
    }

    #[test]
    fn a_claimant_notice_on_an_unread_config_names_the_file_not_the_line() {
        let mut model = captured_session();
        model.note_config_unread();
        let _ = notifier_takes_the_messages(&mut model);
        let standing = notices(&model);
        assert_eq!(standing.len(), 1);
        assert!(!standing[0].contains("= false"), "{standing:?}");
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
        let mut families = vec![
            super::ANONYMOUS_FAMILY.to_string(),
            // a fixed opening like the anonymous one, and here for the same
            // reason: nothing else instantiates it, so a wording that came
            // to prefix another family would go unnoticed
            crate::update::surfaces::CLIPBOARD_NOTICE_FAMILY.to_string(),
        ];
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
                ..cmdline_float(filetype)
            };
            families.push(super::family(sighting.identity()));
        }
        // the openings the one launch box can take, fixed the way the
        // anonymous one is
        for launch in [
            super::HELD_FAMILY,
            super::DRAWING_FAMILY,
            super::MAPPING_FAMILY,
        ] {
            families.push(launch.to_string());
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
                // a `tests.rs` is a test module whole, with no boundary line
                // of its own to cut at
                if path.extension().is_none_or(|ext| ext != "rs")
                    || path.file_name().is_some_and(|name| name == "tests.rs")
                {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("a readable source file");
                // production only: a test helper named `..._family` is a
                // fixture, not a family a notice is ever recorded under
                let text = text.split("\n#[cfg(test)]").next().unwrap_or_default();
                for line in text.lines() {
                    // a fixed opening is a producer too, and the one a
                    // rewording can make prefix another as easily as a
                    // built one
                    let fixed = line
                        .split_once("const ")
                        .and_then(|(_, rest)| rest.split_once(": &str"))
                        .map(|(name, _)| name)
                        .filter(|name| name.ends_with("_FAMILY"));
                    if let Some(name) = fixed {
                        producers.push(name.to_string());
                        continue;
                    }
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

    /// One `win_float_pos` for a float anchored in the message area's
    /// corner: the float's own grid sized, then the resolved corner nvim
    /// carries in the placement event.
    fn place_float(model: &mut Model, grid: u64, win: u64, screen_col: u64) -> Vec<Effect> {
        let mut effects = update(
            model,
            Msg::Redraw(vec![UiEvent::GridResize {
                grid,
                width: 100 - screen_col,
                height: 3,
            }]),
        );
        effects.extend(step_float(model, grid, win, 0, screen_col));
        effects
    }

    /// One step of the slide a notify-style float animates with: the same
    /// window at a new position, and nothing else.
    fn step_float(
        model: &mut Model,
        grid: u64,
        win: u64,
        screen_row: u64,
        screen_col: u64,
    ) -> Vec<Effect> {
        update(
            model,
            Msg::Redraw(vec![UiEvent::WinFloatPos {
                grid,
                win: crate::events::WinHandle(win),
                anchor_grid: 1,
                zindex: 50,
                compindex: 1,
                screen_row,
                screen_col,
            }]),
        )
    }

    /// Whether the compositor would paint `grid`'s pane on the next frame.
    fn painted(model: &Model, grid: u64) -> bool {
        model
            .engine
            .grids()
            .panes_in_z_order()
            .iter()
            .any(|pane| pane.id == crate::grid::registry::GridId(grid))
    }

    /// The whole point of the placement path: the plugin's window is off
    /// the screen on the frame the placement itself arrived for, not one
    /// float scan later.
    #[test]
    fn a_claimants_float_is_withheld_before_the_first_frame_that_would_paint_it() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        let read = place_float(&mut model, 7, 1008, 50);
        assert!(
            read.iter().any(|effect| matches!(
                effect,
                Effect::Rpc(RpcCall::ReadFloatRows { win }) if *win == 1008
            )),
            "the rows are asked for at the placement: {read:?}"
        );
        assert!(
            !painted(&model, 7),
            "a claimant's float paints no frame at all, not even the first"
        );

        // the negative control, which is also the cost statement: a float
        // that lands anywhere else is classified by the same arithmetic and
        // paints on the frame it arrived for
        let elsewhere = update(
            &mut model,
            Msg::Redraw(vec![UiEvent::GridResize {
                grid: 8,
                width: 40,
                height: 10,
            }]),
        );
        assert!(elsewhere.is_empty(), "{elsewhere:?}");
        let elsewhere = step_float(&mut model, 8, 1009, 5, 10);
        assert!(
            elsewhere.is_empty(),
            "nothing is asked of it: {elsewhere:?}"
        );
        assert!(painted(&model, 8), "and nothing is held back from it");
    }

    /// And the classification is what lifts it: rows that read as a window
    /// someone opened hand the window straight back, one round trip after
    /// it was withheld.
    #[test]
    fn the_classification_gives_a_window_the_user_opened_back_to_the_screen() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        expire_hold(&mut model);
        key(&mut model);
        let _ = place_float(&mut model, 7, 1008, 50);
        assert!(!painted(&model, 7));

        let answered = update(
            &mut model,
            Msg::FloatRows {
                win: 1008,
                lines: vec!["2 messages  Ctrl-D to dismiss".to_string()],
            },
        );
        assert!(
            answered.is_empty(),
            "nothing is taken from a window the user opened: {answered:?}"
        );
        assert!(painted(&model, 7), "and it draws again from the next frame");
        let step = step_float(&mut model, 7, 1008, 1, 50);
        assert!(
            step.is_empty() && painted(&model, 7),
            "a released float is a float like any other: {step:?}"
        );
    }

    /// nvim-notify moves its window every 16 ms while a box slides in, so
    /// the flag follows the window rather than the position it was set at.
    #[test]
    fn the_withheld_flag_survives_a_slide_animations_position_steps() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        let _ = place_float(&mut model, 7, 1008, 50);
        for row in 1..4 {
            let step = step_float(&mut model, 7, 1008, row, 50);
            assert!(
                step.is_empty(),
                "one read per window, whatever the animation does: {step:?}"
            );
            assert!(!painted(&model, 7), "and not one step of it is painted");
        }
    }

    /// The suspected half of the ruling: before the probe answers, "no
    /// claimant is known" and "no claimant is loaded" are the same answer,
    /// and a plugin whose timer fires at a fixed offset from `VimEnter` can
    /// beat the probe's round trip. So the float waits, off the screen.
    #[test]
    fn a_float_placed_before_the_probe_answers_is_withheld() {
        let mut model = captured_session();
        let placed = place_float(&mut model, 7, 1008, 50);
        assert!(
            placed.is_empty(),
            "nothing is asked of a window nobody has been named for yet: {placed:?}"
        );
        assert!(
            !painted(&model, 7),
            "and it is off the screen for the frame the placement arrived for"
        );
    }

    /// And the reply is what judges it, on the terms a placement after the
    /// reply is judged on: taken when a claimant of the surface loaded,
    /// handed back when none did.
    #[test]
    fn the_probe_reply_takes_a_held_float_or_gives_it_back() {
        let mut claimed = captured_session();
        let _ = place_float(&mut claimed, 7, 1008, 50);
        let answered = update(&mut claimed, Msg::NotifySinkRead { foreign: true });
        assert!(
            answered.iter().any(|effect| matches!(
                effect,
                Effect::Rpc(RpcCall::ReadFloatRows { win }) if *win == 1008
            )),
            "the reply asks the held float for its rows: {answered:?}"
        );
        assert!(!painted(&claimed, 7), "and goes on holding it");

        let mut benign = captured_session();
        let _ = place_float(&mut benign, 7, 1008, 50);
        let answered = update(&mut benign, Msg::NotifySinkRead { foreign: false });
        assert!(
            !answered
                .iter()
                .any(|effect| matches!(effect, Effect::Rpc(RpcCall::ReadFloatRows { .. }))),
            "a reply naming nobody takes nothing: {answered:?}"
        );
        assert!(
            painted(&benign, 7),
            "and the window paints from the next frame"
        );
    }

    /// The restart arms a probe of its own, so the replacement is back
    /// inside the window where a float over a native surface waits: the
    /// same placement that painted under the answered probe is held again.
    #[test]
    fn a_restart_restores_the_hold_until_the_new_probe_answers() {
        let mut model = captured_session();
        sink_read(&mut model, false);
        let _ = place_float(&mut model, 7, 1008, 50);
        assert!(
            painted(&model, 7),
            "an answered probe naming nobody holds nothing back"
        );

        model.forget_engine_conflicts();
        let _ = place_float(&mut model, 9, 1010, 50);
        assert!(
            !painted(&model, 9),
            "and the replacement's own probe is unanswered again"
        );
        sink_read(&mut model, false);
        assert!(painted(&model, 9), "until it answers");
    }

    /// The close and the plugin's own next animation step race by
    /// construction -- view cannot hold a window against its owner -- so a
    /// step landing after the take is answered the way a vanished window
    /// is: nothing asked of it, nothing painted.
    #[test]
    fn a_taken_floats_later_position_step_asks_for_nothing_and_paints_nothing() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        let _ = place_float(&mut model, 7, 1008, 50);
        let closed = update(
            &mut model,
            Msg::FloatRows {
                win: 1008,
                lines: vec!["`vim.notify` has been overwritten by another plugin?".to_string()],
            },
        );
        assert!(
            closed.iter().any(|effect| matches!(
                effect,
                Effect::Rpc(RpcCall::CloseFloat { win }) if *win == 1008
            )),
            "{closed:?}"
        );
        let step = step_float(&mut model, 7, 1008, 1, 50);
        assert!(step.is_empty(), "{step:?}");
        assert!(
            !painted(&model, 7),
            "the window is gone as far as the screen is concerned, whatever \
             its plugin's timer still does to it"
        );
    }

    /// What the two destinations are for: a plugin's complaint about the
    /// surfaces view took is the conflict view's own notice already
    /// explains, and goes to the history; a plugin's notification is
    /// something the user was being told, and reaches view's chrome
    /// because view withheld the window it was drawn in.
    #[test]
    fn a_withheld_notification_is_toasted_while_a_complaint_only_files() {
        let mut model = captured_session();
        sink_read(&mut model, true);
        let _ = place_float(&mut model, 7, 1008, 50);
        let _ = update(
            &mut model,
            Msg::FloatRows {
                win: 1008,
                lines: vec![
                    String::new(),
                    "# Plugin Updates".to_string(),
                    "- **nvim-lspconfig**".to_string(),
                ],
            },
        );
        let stacked: Vec<String> = model
            .engine
            .messages
            .entries
            .iter()
            .flat_map(|entry| entry.lines())
            .collect();
        assert!(
            stacked.iter().any(|line| line.contains("Plugin Updates")),
            "the plugin's own notification is on view's stack: {stacked:?}"
        );
        assert!(
            !stacked.first().is_some_and(String::is_empty),
            "and without the blank rows the window padded it with: {stacked:?}"
        );
        assert!(
            on_screen(&model)
                .iter()
                .any(|line| line.contains("Plugin Updates")),
            "and on the toast stack, which is where the window it replaces \
             was: {:?}",
            on_screen(&model)
        );

        let _ = place_float(&mut model, 8, 1009, 50);
        let _ = update(
            &mut model,
            Msg::FloatRows {
                win: 1009,
                lines: vec!["Noice can't work when `ext_messages` is enabled".to_string()],
            },
        );
        assert!(
            !model
                .engine
                .messages
                .entries
                .iter()
                .flat_map(|entry| entry.lines())
                .any(|line| line.contains("Noice can't work")),
            "a complaint never joins the notice that already explains it"
        );
        assert!(
            model
                .engine
                .toast_history
                .entries()
                .flat_map(|entry| entry.lines())
                .any(|line| line.contains("Noice can't work")),
            "nothing is discarded: it is in the history"
        );
        assert!(
            !on_screen(&model)
                .iter()
                .any(|line| line.contains("Noice can't work")),
            "and takes no toast slot: the notice standing above it already \
             says what it says: {:?}",
            on_screen(&model)
        );
    }

    /// The toast stack as a user reads it, which is what parts a
    /// notification from a complaint: one takes a slot on the screen, the
    /// other only a line in the history.
    fn on_screen(model: &Model) -> Vec<String> {
        model
            .engine
            .messages
            .visible_lines(40)
            .into_iter()
            .map(|spans| spans.into_iter().map(|span| span.text).collect())
            .collect()
    }

    /// The claimant notice's one way down, which is what the session that
    /// left one standing top-right for its whole length was missing.
    ///
    /// One rule for every input, `<Esc>` included: the notice comes down on
    /// whatever the user does next, but only once the line has stood the
    /// window a transient one gets. A notice wiped by whatever key the user
    /// pressed next is a notice they never read, and this one lands while
    /// they are still starting up -- which is as true of the `<Esc>` that
    /// leaves insert mode as of any letter.
    #[test]
    fn the_conflict_notice_comes_down_on_any_input_once_it_has_stood_its_window() {
        for notation in ["j", "<Esc>"] {
            let mut model = captured_session();
            let effects = notifier_takes_the_messages(&mut model);
            let standing = notices(&model);
            assert_eq!(standing.len(), 1, "{notation}: {standing:?}");

            press(&mut model, notation);
            assert_eq!(
                notices(&model),
                standing,
                "{notation} arrived while the notice was still being read \
                 and took it down"
            );

            let _ = update(
                &mut model,
                Msg::ToastExpired {
                    id: read_window(&effects),
                },
            );
            press(&mut model, notation);
            assert!(
                notices(&model).is_empty(),
                "{notation}: {:?}",
                notices(&model)
            );
        }
    }

    /// Every needle `compat/scenarios/noice.toml` waits for on the claimant
    /// notice is a row this notice still writes.
    ///
    /// The file held a hand copy of the wording, and nothing read it back:
    /// a rewording left the battery waiting fifteen seconds for a line view
    /// no longer writes, naming only the needle. Read row by row rather
    /// than needle by needle -- the file splits the remedy row across two
    /// needles, and its other waits are about the screen rather than about
    /// this notice -- so every row has to be covered by a needle and a
    /// needle left behind by a rewording covers no row.
    #[test]
    fn every_compat_needle_is_a_row_view_still_writes() {
        let needles = wait_needles(include_str!("../../../../compat/scenarios/noice.toml"));
        assert!(
            needles.len() > 5,
            "the file's needles went unread: {needles:?}"
        );

        let standing = notices(&noice_launch());
        assert_eq!(standing.len(), 1, "{standing:?}");

        let uncovered: Vec<&str> = standing[0]
            .split('\n')
            .filter(|row| !needles.iter().any(|needle| row.contains(needle.as_str())))
            .collect();
        assert!(
            uncovered.is_empty(),
            "compat/scenarios/noice.toml waits for none of these rows, so a \
             rewording of them fails no wait and the needles beside them are \
             a copy of a line view no longer writes:\n  {}\nIts needles \
             are:\n  {}",
            uncovered.join("\n  "),
            needles.join("\n  ")
        );
    }

    /// Every `wait_for` needle a scenario file names.
    fn wait_needles(scenario: &str) -> Vec<String> {
        step_needles(scenario, "wait_for")
    }

    /// Every needle a scenario file names under `step`.
    fn step_needles(scenario: &str, step: &str) -> Vec<String> {
        let opening = format!("{step} = \"");
        scenario
            .lines()
            .filter_map(|line| line.split_once(opening.as_str()))
            .filter_map(|(_, rest)| rest.split_once('"'))
            .map(|(needle, _)| needle.to_string())
            .collect()
    }

    /// The launch box noice's first launch raises: its message area held.
    ///
    /// Whether a line naming a feature view draws stands beside it depends
    /// on which of the fixture's plugins has written its channel by the
    /// time the box is read, so the scenario waits for none of that line.
    fn noice_launch() -> Model {
        let mut model = captured_session();
        let _ = update(
            &mut model,
            Msg::ChannelHeld {
                channel: "vim.notify".to_string(),
                holder: "function <a.renderer>".to_string(),
            },
        );
        model
    }

    /// Every launch box a compat fixture can raise, as its lines: each
    /// channel the fixtures' plugins write, alone and all together, and
    /// `cmdheight` reported ahead of `vim.notify`, each with no taken key,
    /// with one, and with everything a launch can take.
    fn fixture_launches() -> Vec<Vec<String>> {
        let channels = ["vim.notify", "statusline", "tabline"];
        let mut sets: Vec<Vec<&str>> = channels.iter().map(|channel| vec![*channel]).collect();
        sets.push(channels.to_vec());
        sets.push(vec!["cmdheight", "vim.notify"]);
        let every = every_taken();
        let one_key: Vec<(String, Taken)> = every
            .iter()
            .filter(|(_, taken)| matches!(taken, Taken::Key { .. }))
            .take(1)
            .cloned()
            .collect();
        let mut boxes = Vec::new();
        for set in &sets {
            for taken in [Vec::new(), one_key.clone(), every.clone()] {
                let mut model = drawing_everything();
                let _ = crate::update::tell_taken_over(&mut model, taken);
                for channel in set {
                    let holder = match *channel {
                        "vim.notify" => "function <a.renderer>",
                        "cmdheight" => "1",
                        _ => "%!v:lua.a()",
                    };
                    let _ = held(&mut model, channel, holder);
                }
                let standing = notices(&model);
                assert_eq!(standing.len(), 1, "{set:?}: {standing:?}");
                boxes.push(standing[0].split('\n').map(str::to_string).collect());
            }
        }
        boxes
    }

    /// One side of the terminal every compat scenario runs in, read from
    /// the harness that sets it: `COMPAT_COLS` or `COMPAT_ROWS`.
    fn compat_size(name: &str) -> u16 {
        let prefix = format!("const {name}: u16 = ");
        include_str!("../../../view-harness/src/bin/oracle/compat.rs")
            .lines()
            .find_map(|line| line.strip_prefix(prefix.as_str()))
            .and_then(|rest| rest.strip_suffix(';'))
            .and_then(|size| size.parse().ok())
            .expect("the compat harness declares its terminal size")
    }

    /// Every width the notice column takes on the compat terminal, under
    /// either layout a scenario state runs in and with or without gaps: a
    /// tile's own frame and inset leave the tiled column narrower.
    fn compat_notice_widths() -> Vec<u16> {
        let size = (compat_size("COMPAT_COLS"), compat_size("COMPAT_ROWS"));
        let mut widths: Vec<u16> = Vec::new();
        for panes in [crate::model::Panes::Nvim, crate::model::Panes::Tiles] {
            for gaps in [true, false] {
                let look = crate::model::Look::new(panes, gaps);
                let scene = crate::model::notice::tests::scene(size, look, &[], 1)
                    .expect("the compat terminal holds one tile");
                let width = scene.model.notice_column().rect.2;
                if !widths.contains(&width) {
                    widths.push(width);
                }
            }
        }
        widths
    }

    /// Every needle a compat scenario waits for or asserts absent, with the
    /// file that names it.
    fn compat_needles() -> Vec<(std::path::PathBuf, String)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat/scenarios");
        let mut needles: Vec<(std::path::PathBuf, String)> = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("the compat scenarios") {
            let path = entry.expect("a readable directory entry").path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let scenario = std::fs::read_to_string(&path).expect("a readable scenario");
            for step in ["wait_for", "assert_absent"] {
                for needle in step_needles(&scenario, step) {
                    needles.push((path.clone(), needle));
                }
            }
        }
        needles
    }

    /// Asserts every needle a box's lines carry sits on one row of that box
    /// at every compat notice width, and returns how many it graded.
    fn grade_needles_against(boxes: &[Vec<String>]) -> usize {
        let needles = compat_needles();
        let widths = compat_notice_widths();
        assert!(widths.len() > 1, "the layouts gave one width: {widths:?}");
        let mut graded = 0;
        for (lines, width) in boxes
            .iter()
            .flat_map(|lines| widths.iter().map(move |width| (lines, *width)))
        {
            let rows = crate::model::wrap_toast(lines, width);
            for (path, needle) in &needles {
                if !lines.iter().any(|line| line.contains(needle.as_str())) {
                    continue;
                }
                graded += 1;
                assert!(
                    rows.iter().any(|row| row.contains(needle.as_str())),
                    "{}: {needle:?} straddles a row of the {width}-column box \
                     as the harness wraps it:\n  {}",
                    path.display(),
                    rows.join("\n  ")
                );
            }
        }
        graded
    }

    /// A needle a scenario waits for or asserts absent from the launch box
    /// sits on one row of it as the compat harness's terminal wraps it,
    /// because a wait reads one screen row at a time. Graded against every
    /// box a fixture can raise, since the channels a box names move where a
    /// needle stands in it.
    #[test]
    fn every_compat_needle_from_the_launch_box_fits_one_row() {
        let launches = fixture_launches();
        assert_eq!(launches.len(), 15, "the walk lost a box shape");
        let graded = grade_needles_against(&launches);
        assert!(graded > 50, "the walk graded nothing: {graded}");
    }

    /// Every conflict notice a compat needle can come from: the anonymous
    /// family and each identity a needle names, over every set of surfaces
    /// a claimant can take, with and without a config read.
    fn fixture_conflict_notices() -> Vec<Vec<String>> {
        let mut families = vec![super::ANONYMOUS_FAMILY.to_string()];
        for (_, needle) in compat_needles() {
            let Some((identity, _)) = needle.split_once(" is drawing over") else {
                continue;
            };
            let identity = identity.strip_prefix("view: ").unwrap_or(identity);
            let family = if identity == "a plugin" {
                super::ANONYMOUS_FAMILY.to_string()
            } else {
                super::family(Some(identity))
            };
            if !families.contains(&family) {
                families.push(family);
            }
        }
        let surfaces = crate::native::surfaces::SURFACES;
        let mut notices = Vec::new();
        for family in &families {
            for mask in 1..(1u32 << surfaces.len()) {
                let claimed: Vec<Surface> = surfaces
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(_, row)| row.surface)
                    .collect();
                for read in [true, false] {
                    let text = super::notice(family, &claimed, read);
                    notices.push(text.split('\n').map(str::to_string).collect());
                }
            }
        }
        notices
    }

    /// The same one-row rule for the conflict notices a float raises at
    /// runtime, which is where the nvim-cmp and telescope needles come from.
    #[test]
    fn every_compat_needle_from_a_conflict_notice_fits_one_row() {
        let notices = fixture_conflict_notices();
        assert!(notices.len() > 1000, "the walk lost a notice shape");
        let graded = grade_needles_against(&notices);
        assert!(graded > 1000, "the walk graded nothing: {graded}");
    }

    #[test]
    fn the_notice_text_starts_with_its_own_family() {
        let claimed = [Surface::Cmdline, Surface::Messages];
        for family in [
            super::ANONYMOUS_FAMILY.to_string(),
            super::family(Some("a.menu")),
        ] {
            for read in [true, false] {
                let text = super::notice(&family, &claimed, read);
                assert!(
                    text.starts_with(family.as_str()),
                    "{text:?} is not in {family:?}"
                );
            }
        }
        let told = [
            (Surface::Messages, vec!["vim.notify".to_string()]),
            (Surface::Tabline, vec!["winbar".to_string()]),
        ];
        let taken = every_taking();
        for told in [&told[..], &[]] {
            for taken in [&taken[..], &taken[..1], &taken[taken.len() - 1..], &[]] {
                for read in [true, false] {
                    for startup in [true, false] {
                        let launch = super::launch_notice(told, taken, read, startup);
                        let Some((family, text)) = launch else {
                            assert!(told.is_empty() && taken.is_empty());
                            continue;
                        };
                        assert!(super::LAUNCH_FAMILIES.contains(&family), "{family:?}");
                        assert!(text.starts_with(family), "{text:?} is not in {family:?}");
                    }
                }
            }
        }
    }

    /// Everything a launch can hand to view beside the channels: every
    /// feature the registry names, and every default key and desktop chord
    /// as a key the user had mapped, keyed the way the first-run record
    /// keys them.
    fn every_taken() -> Vec<(String, Taken)> {
        let mut taken: Vec<(String, Taken)> = crate::native::registry::features()
            .iter()
            .map(|desc| {
                (
                    desc.id.to_string(),
                    Taken::Drawing {
                        feature: desc.id,
                        off_switch: desc.off_switch,
                    },
                )
            })
            .collect();
        let keys = crate::native::mappings::default_maps()
            .iter()
            .map(|spec| (spec.feature, spec.lhs.to_string(), spec.verb))
            .chain(
                crate::native::chords::desktop_chords()
                    .iter()
                    .map(|chord| (chord.feature, chord.with_super.to_string(), chord.verb)),
            );
        for (feature, lhs, verb) in keys {
            taken.push((
                format!("{feature}:key:{lhs}"),
                Taken::Key {
                    lhs: lhs.to_string(),
                    action: format!("{feature} {}", verb.replace('_', " ")),
                    off_switch: "keys.profile = \"editor\"",
                },
            ));
        }
        taken
    }

    fn every_taking() -> Vec<Taken> {
        every_taken().into_iter().map(|(_, taken)| taken).collect()
    }

    /// The launch box stands until the user acts once a held channel is in
    /// it, and times out while it tells only what view took.
    #[test]
    fn the_launch_box_stands_only_for_a_held_channel() {
        let kinds = |model: &Model| -> Vec<String> {
            model
                .engine
                .messages
                .entries
                .iter()
                .filter(|entry| entry.is_native())
                .map(|entry| entry.kind.clone())
                .collect()
        };
        let mut model = drawing_everything();
        let _ = crate::update::tell_taken_over(&mut model, every_taken());
        assert_eq!(kinds(&model), ["native"]);
        let _ = held(&mut model, "vim.notify", "function <a.renderer>");
        assert_eq!(kinds(&model), ["native_sticky"]);
    }

    /// The launch shows one box whatever a config trips and in whatever
    /// order the reports land, and the keys stand in one line of it. The
    /// next launch under the same config raises none.
    #[test]
    fn a_first_launch_that_trips_everything_raises_one_box() {
        let channels: Vec<&str> = reported_channels();
        let taken = every_taken();
        let (first, rest) = taken.split_at(taken.len() / 2);
        let orders: [&[&str]; 3] = [
            &["taken", "held"],
            &["held", "taken"],
            &["first", "held", "rest"],
        ];
        for order in orders {
            let mut model = drawing_everything();
            for step in order {
                let _ = match *step {
                    "held" => channels
                        .iter()
                        .flat_map(|channel| held(&mut model, channel, "function <a.renderer>"))
                        .collect(),
                    "taken" => crate::update::tell_taken_over(&mut model, taken.clone()),
                    "first" => crate::update::tell_taken_over(&mut model, first.to_vec()),
                    _ => crate::update::tell_taken_over(&mut model, rest.to_vec()),
                };
            }
            let boxes = notices(&model);
            assert_eq!(boxes.len(), 1, "{order:?}: {boxes:?}");
            let lines: Vec<&str> = boxes[0].split('\n').collect();
            let mapping: Vec<&&str> = lines
                .iter()
                .filter(|line| line.contains("ow mapping "))
                .collect();
            assert_eq!(mapping.len(), 1, "{order:?}: {lines:?}");
            for (_, taken) in &taken {
                if let Taken::Key { lhs, .. } = taken {
                    assert!(
                        mapping[0].contains(lhs.as_str()),
                        "{lhs} {order:?}: {lines:?}"
                    );
                }
            }
            for channel in &channels {
                assert!(
                    boxes[0].contains(channel),
                    "{channel} {order:?}: {}",
                    boxes[0]
                );
            }

            let mut next = drawing_everything();
            next.seed_announced(
                taken
                    .iter()
                    .map(|(key, _)| key.clone())
                    .chain(channels.iter().map(|channel| super::announced_key(channel))),
            );
            let _ = crate::update::tell_taken_over(&mut next, taken.clone());
            for channel in &channels {
                let _ = held(&mut next, channel, "function <a.renderer>");
            }
            assert!(notices(&next).is_empty(), "{:?}", notices(&next));
        }
    }
}
