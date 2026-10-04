//! Live-nvim proof of the entry-point pair: an enabled feature's default
//! key really is registered over the user's own mapping in a real session
//! and reports the claim, a disabled feature leaves that mapping firing
//! untouched, and `:View` exists either way.
//!
//! The differential corpus cannot express this. Its entries compare view's
//! decode of a key script against a reference applier, and what is asserted
//! here is neither: it is which mapping a live nvim resolves `<leader>ff`
//! to after registration, and which side answers when the key is pressed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use view_core::events::UiEvent;
use view_core::model::{Look, Model};
use view_core::msg::{Effect, Key, Msg, RpcCall};
use view_core::native::key_log::Fired;
use view_core::native::mappings::{MappingClaim, MappingOwner};
use view_core::native::registry;
use view_core::native::speculate::{fold_redraw, is_cmdline_mode, SpecStamp, CMDLINE_LITERAL_KEYS};
use view_core::native::surfaces::Taken;
use view_core::update::update;
use view_engine::process::Engine;
use view_native::config::NativeConfig;
use view_native::mappings::register_plan;
use view_native::report::{report, Handover};
use view_native::supersede::plan;
use view_test_support::ScratchDir;

/// How long a claim reply or an invoke notification is waited for. Generous
/// because a cold nvim spawn on a loaded CI box is the slow part; a healthy
/// session answers in milliseconds.
const ARRIVAL: Duration = Duration::from_secs(10);

/// How long a run is watched for a notification that must never come. Short
/// on purpose: the barrier eval before it has already forced nvim through
/// the fed keys, so anything still in flight would have been written ahead
/// of the barrier's own reply.
const SILENCE: Duration = Duration::from_millis(300);

/// The fixture's leader. Not the default backslash: a test that passed
/// because view happened to register the same physical keys nvim's default
/// leader produces would prove nothing about reading the user's own
/// `mapleader`.
const LEADER: &str = ",";

/// A live nvim reading the fixture's `init.lua` and nothing else, with a UI
/// attached and its pump sink installed, so what crosses back from a
/// keypress is observable as the `Msg` the runtime loop would see.
///
/// The attach is not decoration: an `--embed` nvim holds startup until a UI
/// attaches, so a config-defined mapping read before that point does not
/// exist yet. It is also why production registers after `VimEnter` rather
/// than at spawn -- before the user's config has run, `mapleader` is not
/// theirs and there is no mapping to claim.
struct Session {
    engine: Engine,
    rx: Receiver<Msg>,
    /// Where the redraw traffic a `Msg::RedrawReady` announces is drained.
    damage: view_engine::DamagePump,
    /// Held so the fixture's config directory outlives the nvim reading it.
    _dir: ScratchDir,
}

impl Session {
    fn start(name: &str) -> Self {
        Self::start_with(name, "")
    }

    /// The same fixture with `extra` appended to its `init.lua`.
    fn start_with(name: &str, extra: &str) -> Self {
        Self::start_attached(name, extra, view_engine::UI_EXT_OPTIONS, 80)
    }

    /// The same fixture attached with `surfaces` at `width` columns.
    fn start_attached(name: &str, extra: &str, surfaces: &[&str], width: u16) -> Self {
        let dir = common::fixture(
            &format!("mappings-live-{name}"),
            &format!(
                "vim.g.mapleader = '{LEADER}'\n\
                 vim.g.view_user_ff = 0\n\
                 vim.keymap.set('n', '<leader>ff', function() vim.g.view_user_ff = 1 end)\n\
                 {extra}"
            ),
        );
        let cfg = common::isolated_reading(&dir.join("init.lua"));
        let (engine, damage, rx) = common::spawn_with_pump(cfg, 256);
        engine.handle.ui_attach(width, 24, surfaces).unwrap();
        Self {
            engine,
            rx,
            damage,
            _dir: dir,
        }
    }

    /// Registers the plan `cfg` produces, exactly as the runtime's executor
    /// would, and returns the specs it registered.
    ///
    /// A match rather than a call into the executor: that seam lives inside
    /// the bin target and is unreachable from an integration test. The
    /// mapping it mirrors is pinned separately by `runtime`'s own
    /// `register_mappings_effect_maps_to_engine_ops_register_mappings`.
    fn register(&self, cfg: &NativeConfig) -> Vec<String> {
        let call = register_plan(cfg, self.engine.api_info.channel_id);
        match call {
            RpcCall::RegisterMappings { specs, channel_id } => {
                self.engine
                    .handle
                    .register_mappings(&specs, channel_id)
                    .unwrap();
                specs.iter().map(|s| s.lhs.to_string()).collect()
            }
            other => panic!("the entry-point plan must ride one registration call, got {other:?}"),
        }
    }

    /// Types `notation` and waits for nvim to have consumed it.
    ///
    /// The eval is the barrier, not a value anyone reads: `feedkeys` queues
    /// into the typeahead rather than executing, and nvim answers a deferred
    /// request only once it is back waiting for input -- which is to say,
    /// once the fed keys have run and anything they notified is already on
    /// the wire ahead of this reply.
    fn press(&self, notation: &str) {
        self.engine.handle.feed_keys(notation).unwrap();
        self.engine.handle.eval_str("1").unwrap();
    }

    fn eval(&self, expr: &str) -> String {
        self.engine.handle.eval_str(expr).unwrap()
    }

    /// The first `Msg` the pump delivers that `want` answers for, within
    /// `budget`. Redraw traffic and every other message flow past.
    fn wait_for<T>(&self, budget: Duration, want: impl Fn(&Msg) -> Option<T>) -> Option<T> {
        common::drain_until(&self.rx, budget, want)
    }

    fn report(&self) -> (Vec<MappingClaim>, bool) {
        self.wait_for(ARRIVAL, |msg| match msg {
            Msg::MappingsClaimed {
                claimed,
                colon_mapped,
                ..
            } => Some((claimed.clone(), *colon_mapped)),
            _ => None,
        })
        .expect("the registration must answer with its claim list")
    }

    fn claims(&self) -> Vec<MappingClaim> {
        self.report().0
    }

    /// The next `:` reading the re-read reports.
    ///
    /// Only a change is reported, which is what makes this a wait rather
    /// than a poll: every step of the window-switch case moves the answer,
    /// so each one owes exactly one of these.
    fn next_colon_reading(&self) -> bool {
        self.wait_for(ARRIVAL, |msg| match msg {
            Msg::ColonMappingRead { mapped } => Some(*mapped),
            _ => None,
        })
        .expect("the re-read must report the answer moving")
    }

    fn invoke(&self, budget: Duration) -> Option<(String, String)> {
        self.wait_for(budget, |msg| match msg {
            Msg::FeatureInvoke { feature, verb, .. } => Some((feature.clone(), verb.clone())),
            _ => None,
        })
    }
}

/// What the launch box names for `claimed`, through the one report every
/// handover crosses.
fn taken(claimed: &[MappingClaim]) -> Vec<Taken> {
    let features = registry::features();
    report(
        &plan(&NativeConfig::all_enabled(), features, Look::default()),
        claimed,
        features,
    )
    .iter()
    .map(Handover::taken)
    .collect()
}

#[test]
fn an_enabled_features_key_is_claimed_over_the_users_and_reported_with_its_off_switch() {
    let session = Session::start("enabled");
    assert_eq!(
        session.eval("g:mapleader"),
        LEADER,
        "the fixture config never took effect, so nothing here could observe a claim"
    );

    let registered = session.register(&NativeConfig::all_enabled());
    let claimed = session.claims();

    assert_eq!(
        claimed.iter().map(|c| c.lhs.clone()).collect::<Vec<_>>(),
        registered,
        "every registered key must report, and only those"
    );
    let ff = claimed
        .iter()
        .find(|c| c.lhs == "<leader>ff")
        .expect("<leader>ff must be among the picker's default keys");
    assert!(
        ff.had_user_mapping,
        "the fixture mapped <leader>ff, so taking it is news: {claimed:?}"
    );
    assert!(
        claimed.iter().any(|c| !c.had_user_mapping),
        "the fixture mapped only <leader>ff, so the rest must report as free: {claimed:?}"
    );

    // the claim view reported and the mapping the live session resolves are
    // the same fact seen from two sides; a claim for a key nvim did not
    // actually take would be a report that lies
    let rhs = session.eval("maparg('<leader>ff', 'n')");
    assert!(
        rhs.contains("view_invoke") && rhs.contains("picker") && rhs.contains("files"),
        "view's own mapping must be what nvim resolves, and readable as such, got {rhs:?}"
    );

    let taken = taken(&claimed);
    assert!(
        taken.contains(&Taken::Key {
            lhs: "<leader>ff".to_string(),
            action: "picker files".to_string(),
            off_switch: "native.picker = false",
        }),
        "the claim must reach the launch box with the line that gives the key back, \
         got {taken:?}"
    );

    session.press(",ff");
    assert_eq!(
        session.invoke(ARRIVAL),
        Some(("picker".to_string(), "files".to_string())),
        "pressing the claimed key must reach view as its own feature invoke"
    );
    assert_eq!(
        session.eval("g:view_user_ff"),
        "0",
        "a claimed key must not also run the mapping it replaced"
    );
}

#[test]
fn leader_leader_opens_the_palette() {
    let session = Session::start("palette-open");
    session.register(&NativeConfig::all_enabled());
    session.claims();
    session.press(",,");
    assert_eq!(
        session.invoke(ARRIVAL),
        Some(("palette".to_string(), "open".to_string())),
        "<leader><leader> must reach view as the palette's open verb"
    );
    // `update`'s own ("palette", "open") arm answers the invoke with the
    // same `:` a typed colon sends; feeding it here, the way the production
    // executor would once it applied that effect, proves the round trip
    // actually opens nvim's own command line, past the invoke arriving
    session.press(":");
    assert_eq!(
        session.eval("getcmdtype()"),
        ":",
        "the invoke's own : must have opened nvim's real command line"
    );
}

/// The palette's default key is claimed over a user's own mapping and
/// reported for it, the same as every other feature's default key -- proof
/// that widening the notifications anchors and adding this row did not carve
/// out a special case for the palette in the claim path.
#[test]
fn the_palette_key_is_rebindable_through_keys() {
    let session = Session::start_with(
        "palette-remap",
        "vim.g.view_user_palette = 0\n\
         vim.keymap.set('n', '<leader><leader>', function() vim.g.view_user_palette = 1 end)\n",
    );
    let registered = session.register(&NativeConfig::all_enabled());
    let claimed = session.claims();
    assert!(
        registered.iter().any(|lhs| lhs == "<leader><leader>"),
        "the palette's default key must be among what view registers: {registered:?}"
    );
    let palette = claimed
        .iter()
        .find(|c| c.lhs == "<leader><leader>")
        .expect("<leader><leader> must be among the palette's default keys");
    assert!(
        palette.had_user_mapping,
        "the fixture mapped <leader><leader> itself, so view taking it over is news: {claimed:?}"
    );

    session.press(",,");
    assert_eq!(
        session.eval("g:view_user_palette"),
        "0",
        "a claimed key must not also run the mapping it replaced"
    );
}

/// Each claim carries the verb its key runs, and a claim set over a mapping
/// of the user's carries that mapping's description and the script that set
/// it, which is what the key log names as the mapping the key was taken
/// from. A Lua callback is named by the file and line that defined it, for
/// a claimed key and for one of the user's own keys alike.
#[test]
fn a_claim_names_its_verb_and_the_users_mapping_it_displaced() {
    // a Vimscript file, so the mapping records a script id `getscriptinfo()`
    // can name; a Lua-set mapping records none unless nvim runs verbose
    let session = Session::start_with(
        "displaced",
        "vim.keymap.set('n', '<leader>zz', function() end, { desc = 'Mine' })\n\
         local dir = vim.fs.dirname(debug.getinfo(1, 'S').source:sub(2))\n\
         local keys = dir .. '/keys.vim'\n\
         vim.fn.writefile({\n\
         \x20 \"call nvim_set_keymap('n', '<leader>fg', ':echo 1<CR>', #{desc: 'Live grep'})\",\n\
         }, keys)\n\
         vim.cmd.source(keys)\n",
    );
    let _ = session.register(&NativeConfig::all_enabled());
    // the reply routes the user's keys and whose they are ahead of the claims
    let owners = session
        .wait_for(ARRIVAL, |msg| match msg {
            Msg::UserMappingOwners { owners } => Some(owners.clone()),
            _ => None,
        })
        .expect("the registration reads whose the user's own keys are");
    let claimed = session.claims();
    let claim = |lhs: &str| {
        claimed
            .iter()
            .find(|c| c.lhs == lhs)
            .unwrap_or_else(|| panic!("{lhs} must be claimed: {claimed:?}"))
    };
    assert_eq!(claim("<leader>ff").verb, "files");
    assert_eq!(claim("<leader>fb").verb, "buffers");
    let grep = claim("<leader>fg");
    assert_eq!(grep.verb, "grep");
    let displaced = grep
        .displaced
        .as_ref()
        .unwrap_or_else(|| panic!("the fixture mapped <leader>fg: {grep:?}"));
    assert_eq!(displaced.label, "Live grep", "{displaced:?}");
    assert!(
        displaced
            .script
            .as_deref()
            .is_some_and(|script| script.ends_with("keys.vim")),
        "the script that set the mapping must be named: {displaced:?}"
    );
    assert!(
        claim("<leader>fb").displaced.is_none(),
        "a key that landed on nothing displaced nothing: {claimed:?}"
    );
    // the fixture's own `<leader>ff` is a Lua callback on line 3 of init.lua,
    // read back through `maparg(..., v:true)`, which hands the callback to
    // Lua as a function `debug.getinfo` can read
    let ff = claim("<leader>ff").displaced.as_ref();
    assert!(
        ff.and_then(|owner| owner.script.as_deref())
            .is_some_and(|script| script.ends_with("init.lua:3")),
        "a Lua callback names the file and line that defined it: {ff:?}"
    );
    let mine = owners
        .iter()
        .find(|(lhs, _)| lhs == ",zz")
        .map(|(_, owner)| owner.clone());
    assert_eq!(
        mine.as_ref().map(|owner| owner.label.as_str()),
        Some("Mine"),
        "{owners:?}"
    );
    assert!(
        mine.as_ref()
            .and_then(|owner| owner.script.as_deref())
            .is_some_and(|script| script.ends_with("init.lua:4")),
        "a user's own Lua mapping names this fixture's init.lua: {mine:?}"
    );
}

#[test]
fn a_disabled_feature_leaves_the_users_own_mapping_firing() {
    let session = Session::start("disabled");
    let cfg = NativeConfig::from_toml_str(
        "[native]\npicker = false\ntree = false\nnotifications = false\npalette = false\n",
    )
    .unwrap();

    let registered = session.register(&cfg);
    assert_eq!(
        registered,
        vec![
            "<leader>ai".to_string(),
            "<leader>ug".to_string(),
            "<leader>uw".to_string(),
            "<leader>wn".to_string(),
            "<leader>wz".to_string(),
            "<leader>wf".to_string(),
            "<leader>ws".to_string(),
            "<leader>uf".to_string(),
            "<C-w>m".to_string(),
            "<leader>w1".to_string(),
            "<leader>w2".to_string(),
            "<leader>w3".to_string(),
            "<leader>w4".to_string(),
            "<leader>w5".to_string(),
            "<leader>w6".to_string(),
            "<leader>w7".to_string(),
            "<leader>w8".to_string(),
            "<leader>w9".to_string(),
            "<leader>fk".to_string(),
            "<leader>fv".to_string(),
        ],
        "a disabled feature must contribute no key of its own; the survivors \
             are ai's default key, the two ui actions, window's own tile \
             keys, the key log and the DVR scrub, none of which [native] has \
             a switch for, got {registered:?}"
    );
    let claimed = session.claims();
    assert_eq!(
        claimed.len(),
        20,
        "only the keys no [native] entry here names may be claimed: {claimed:?}"
    );
    assert_eq!(
        claimed.iter().map(|c| c.lhs.as_str()).collect::<Vec<_>>(),
        vec![
            "<leader>ai",
            "<leader>ug",
            "<leader>uw",
            "<leader>wn",
            "<leader>wz",
            "<leader>wf",
            "<leader>ws",
            "<leader>uf",
            "<C-w>m",
            "<leader>w1",
            "<leader>w2",
            "<leader>w3",
            "<leader>w4",
            "<leader>w5",
            "<leader>w6",
            "<leader>w7",
            "<leader>w8",
            "<leader>w9",
            "<leader>fk",
            "<leader>fv",
        ]
    );

    let rhs = session.eval("maparg('<leader>ff', 'n')");
    assert!(
        !rhs.contains("view_invoke"),
        "the user's own mapping must be untouched, got {rhs:?}"
    );

    session.press(",ff");
    assert_eq!(
        session.eval("g:view_user_ff"),
        "1",
        "the user's mapping must still be what the key runs"
    );
    assert_eq!(
        session.invoke(SILENCE),
        None,
        "view must not hear about a key it never took"
    );
}

#[test]
fn the_view_command_is_a_way_in_whatever_the_user_turned_off() {
    let session = Session::start("command");
    let cfg = NativeConfig::from_toml_str(
        "[native]\npicker = false\ntree = false\nnotifications = false\npalette = false\n",
    )
    .unwrap();
    session.register(&cfg);
    assert_eq!(
        session.claims().len(),
        20,
        "only ai's key, the two ui actions, window's own tile keys, the key \
             log and the DVR scrub, none of which [native] can turn off, \
             survive every other feature being disabled"
    );

    assert_eq!(
        session.eval("exists(':View')"),
        "2",
        "the command must exist even with every default key turned off"
    );
    // the completion the command offers is every entry point this build
    // has, not the subset this session mapped: a user who turned the keys
    // off is exactly the user who needs to be told what to type
    assert_eq!(
        session.eval("join(getcompletion('View pic', 'cmdline'), ',')"),
        "picker",
        "the command must complete the features it can invoke"
    );
    // the review's keys are buffer-local mappings on the file under
    // review, so no default key reaches it and the command line is the
    // whole of its discoverability -- a verb a user would have to already
    // know to type is not a way out of anything
    assert_eq!(
        session.eval("join(getcompletion('View rev', 'cmdline'), ',')"),
        "review",
        "the command must complete a feature no key reaches"
    );
    assert_eq!(
        session.eval("join(getcompletion('View review ', 'cmdline'), ',')"),
        "accept,accept_all,leave,next,prev,rediff,reject,reject_all",
        "and every verb it answers"
    );

    session.engine.handle.input(":View picker grep\r").unwrap();
    session.eval("1");
    assert_eq!(
        session.invoke(ARRIVAL),
        Some(("picker".to_string(), "grep".to_string())),
        "the command must invoke the feature it names"
    );
}

/// Applies every `Msg` the pump delivers to `model`, sending nvim the keys
/// the updates route to it, until `done` answers for the model an update
/// left and the message it applied, the first answer returned once the
/// whole wakeup is applied. `None` when nothing answers within `budget`.
fn pump<T>(
    session: &Session,
    model: &mut Model,
    budget: Duration,
    done: impl Fn(&Model, &Msg) -> Option<T>,
) -> Option<T> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        let received = session.rx.recv_timeout(left).ok()?;
        let found = applied(
            model,
            dispatched(session, received),
            |effects| send(session, effects),
            &done,
        );
        if found.is_some() {
            return found;
        }
    }
}

/// Applies every one of `msgs` to `model`, handing `send` the effects of
/// each, and returns the first answer `done` gives.
fn applied<T>(
    model: &mut Model,
    msgs: Vec<Msg>,
    mut send: impl FnMut(&[Effect]),
    done: &impl Fn(&Model, &Msg) -> Option<T>,
) -> Option<T> {
    let mut found = None;
    for msg in msgs {
        // the runtime reads every batch for the keys nvim answered before
        // the update applies it
        if let Msg::Redraw(events) = &msg {
            let stamp = SpecStamp::new(Duration::ZERO);
            send(&fold_redraw(model, events, stamp));
        }
        let effects = update(model, msg.clone());
        send(&effects);
        found = found.or_else(|| done(model, &msg));
    }
    found
}

/// A wakeup whose first message answers is still applied whole.
#[test]
fn a_wakeup_is_applied_whole_when_an_early_message_answers() {
    let mut model = Model::with_term_size(80, 24);
    let found = applied(
        &mut model,
        vec![
            Msg::Redraw(vec![UiEvent::Flush]),
            Msg::Resized {
                width: 90,
                height: 30,
            },
        ],
        |_| {},
        &|_, msg| matches!(msg, Msg::Redraw(_)).then_some(()),
    );
    assert!(found.is_some());
    assert_eq!((model.term_width, model.term_height), (90, 30));
}

/// The messages the runtime loop dispatches for `received`, in its order:
/// a redraw token drains up to an invocation not yet received,
/// [`view_engine::DamagePump::admit`] puts what nvim drew before an
/// invocation ahead of it, and every wakeup ends with the drain of what is
/// left.
fn dispatched(session: &Session, received: Msg) -> Vec<Msg> {
    let damage = &session.damage;
    let received = match received {
        Msg::RedrawReady => Msg::Redraw(damage.take_damage_folded().0),
        msg => msg,
    };
    let mut out = damage.admit(received).into_stack();
    out.reverse();
    let residue = damage.take_damage_folded().0;
    if !residue.is_empty() {
        out.push(Msg::Redraw(residue));
    }
    out
}

/// Sends nvim the keys `effects` route to it.
fn send(session: &Session, effects: &[Effect]) {
    for effect in effects {
        if let Effect::Rpc(RpcCall::Input { notation }) = effect {
            session.engine.handle.input(notation).unwrap();
        }
    }
}

/// Types `keys` into `model`, answering the keys it sent nvim.
fn type_into(session: &Session, model: &mut Model, keys: &[&str]) -> Vec<String> {
    let mut sent = Vec::new();
    for key in keys {
        model.set_now(std::time::SystemTime::now());
        let effects = update(
            model,
            Msg::Key(Key {
                notation: (*key).to_string(),
            }),
        );
        send(session, &effects);
        sent.extend(effects.iter().filter_map(|effect| match effect {
            Effect::Rpc(RpcCall::Input { notation }) => Some(notation.clone()),
            _ => None,
        }));
    }
    sent
}

/// The user's own mappings the key log holds, oldest first, each as its
/// lhs and whose it is.
fn user_rows(model: &Model) -> Vec<(String, Option<MappingOwner>)> {
    let mut rows: Vec<_> = model
        .key_log()
        .entries()
        .filter_map(|entry| match &entry.fired {
            Fired::User { lhs, owner } => Some((lhs.clone(), owner.clone())),
            _ => None,
        })
        .collect();
    rows.reverse();
    rows
}

/// A model that has applied the registration's claims, with nvim in normal
/// mode.
fn registered_model(session: &Session) -> Model {
    let mut model = Model::with_term_size(80, 24);
    model.ai_trusted = true;
    session.register(&NativeConfig::all_enabled());
    pump(session, &mut model, ARRIVAL, |_, msg| {
        matches!(msg, Msg::MappingsClaimed { .. }).then_some(())
    })
    .expect("the registration must answer with its claim list");
    assert_eq!(model.engine.mode.current, "normal");
    model
}

/// Applies the pump until whose the user's keys are has been read again
/// and `has_gd` answers for the read, then until nvim is in normal mode.
fn reread_owners(session: &Session, model: &mut Model, has_gd: bool) {
    pump(session, model, ARRIVAL, |_, msg| match msg {
        Msg::UserMappingOwners { owners } => {
            (owners.iter().any(|(lhs, _)| lhs == "gd") == has_gd).then_some(())
        }
        _ => None,
    })
    .expect("the reread must report whose the keys are");
    if model.engine.mode.current != "normal" {
        pump(session, model, ARRIVAL, |model, _| {
            (model.engine.mode.current == "normal").then_some(())
        })
        .expect("nvim must report normal mode");
    }
}

/// The labels and buffer marks of the rows the key log holds, oldest
/// first.
fn row_labels(model: &Model) -> Vec<(String, String, bool)> {
    user_rows(model)
        .into_iter()
        .map(|(lhs, owner)| {
            let owner = owner.unwrap_or_else(|| panic!("whose {lhs} is was read"));
            (lhs, owner.label, owner.buffer)
        })
        .collect()
}

/// A mapping a language server's attach sets on the buffer is logged with
/// its description and marked as the buffer's own, over a global mapping
/// with the same keys. A new description alone is read again. In a buffer
/// that does not map them, the same keys log nothing, or the global
/// mapping they run.
#[test]
fn a_buffer_local_mapping_set_on_lsp_attach_is_logged_as_the_buffers_own() {
    let session = Session::start_with(
        "buffer-local",
        "vim.g.view_gd = 0\n\
         function _G.view_gd_fn() vim.g.view_gd = vim.g.view_gd + 1 end\n\
         vim.keymap.set('n', 'gy', function() end, { desc = 'Global yank' })\n\
         vim.api.nvim_create_autocmd('LspAttach', { callback = function(args)\n\
         \x20 vim.keymap.set('n', 'gd', view_gd_fn, { buffer = args.buf, desc = 'Goto definition' })\n\
         \x20 vim.keymap.set('n', 'gy', function() end, { buffer = args.buf, desc = 'Buffer yank' })\n\
         end })\n\
         function _G.view_attach()\n\
         \x20 vim.api.nvim_exec_autocmds('LspAttach',\n\
         \x20   { buffer = 0, data = { client_id = 1 } })\n\
         end\n",
    );
    let mut model = registered_model(&session);
    session.eval("execute('lua view_attach()')");
    reread_owners(&session, &mut model, true);

    // every rhs is a function: nvim answers no request between a `<Nop>`
    // rhs and the next key
    let keys = ["g", "d", "g", "y"];
    assert_eq!(type_into(&session, &mut model, &keys[..2]), keys[..2]);
    // a key typed before nvim answers a mapping may have been text its rhs
    // waits on, and gd draws nothing, so gy waits out a round trip
    session.eval("execute('lua vim.wait(300, function() return false end)')");
    assert_eq!(type_into(&session, &mut model, &keys[2..]), keys[2..]);
    assert_eq!(session.eval("g:view_gd"), "1", "the buffer's gd ran");
    let at = |label: &str, buffer: bool, lhs: &str| (lhs.to_string(), label.to_string(), buffer);
    assert_eq!(
        row_labels(&model),
        [
            at("Goto definition", true, "gd"),
            at("Buffer yank", true, "gy")
        ]
    );
    let script = user_rows(&model)[0]
        .1
        .clone()
        .and_then(|owner| owner.script);
    assert!(
        script
            .as_deref()
            .is_some_and(|script| script.ends_with("init.lua:5")),
        "{script:?}"
    );

    session.eval(
        "execute('lua vim.keymap.set(\"n\", \"gd\", view_gd_fn, \
         { buffer = 0, desc = \"Peek definition\" })')",
    );
    session.eval("execute('doautocmd BufEnter')");
    pump(&session, &mut model, ARRIVAL, |_, msg| match msg {
        Msg::UserMappingOwners { owners } => owners
            .iter()
            .any(|(lhs, owner)| lhs == "gd" && owner.label == "Peek definition")
            .then_some(()),
        _ => None,
    })
    .expect("a new description alone must be read again");

    session.engine.handle.input(":enew<CR>").unwrap();
    reread_owners(&session, &mut model, false);
    let _ = type_into(&session, &mut model, &keys);
    session.eval("1");
    assert_eq!(
        row_labels(&model)[2..],
        [at("Global yank", false, "gy")],
        "gd is nobody's here, and gy is the global one"
    );
}

/// A callback names the file a person wrote: none for a C function, and
/// for a callback defined in nvim's runtime, the file of the function it
/// wraps. The fixture stands a directory of its own in for the runtime.
#[test]
fn a_callback_names_the_file_a_person_wrote_or_none() {
    let session = Session::start_with(
        "callback-scripts",
        "local dir = vim.fs.dirname(debug.getinfo(1, 'S').source:sub(2))\n\
         vim.fn.mkdir(dir .. '/rt', 'p')\n\
         vim.fn.writefile({ 'return function(fn) return function() return fn() end end' },\n\
         \x20 dir .. '/rt/wrap.lua')\n\
         local wrap = dofile(dir .. '/rt/wrap.lua')\n\
         vim.keymap.set('n', '<leader>o', print)\n\
         vim.keymap.set('n', '<leader>m', wrap(function() end), { desc = 'Wrapped' })\n\
         vim.env.VIMRUNTIME = dir .. '/rt'\n",
    );
    let _ = session.register(&NativeConfig::all_enabled());
    let owners = session
        .wait_for(ARRIVAL, |msg| match msg {
            Msg::UserMappingOwners { owners } => Some(owners.clone()),
            _ => None,
        })
        .expect("the registration reads whose the user's own keys are");
    let owner = |lhs: &str| {
        owners
            .iter()
            .find(|(key, _)| key == lhs)
            .map(|(_, owner)| owner.clone())
            .unwrap_or_else(|| panic!("{lhs} must be read: {owners:?}"))
    };
    let print = owner(",o");
    assert_eq!(
        (print.label.as_str(), print.script),
        ("<Lua callback>", None),
        "a C function has no file"
    );
    let wrapped = owner(",m");
    assert!(
        wrapped
            .script
            .as_deref()
            .is_some_and(|script| script.ends_with("init.lua:10")),
        "the wrapped function's own line: {wrapped:?}"
    );
}

/// Under the engine's own runtime, a mapping set straight to one of its
/// functions names no file, and one set to a function a person wrote in a
/// file of their own names that file.
#[test]
fn a_callback_set_straight_to_a_runtime_function_names_no_file() {
    let session = Session::start_with(
        "runtime-callback",
        "local dir = vim.fs.dirname(debug.getinfo(1, 'S').source:sub(2))\n\
         vim.fn.writefile({ 'return function() end' }, dir .. '/mine.lua')\n\
         vim.keymap.set('n', '<leader>d', vim.diagnostic.open_float, { desc = 'Float' })\n\
         vim.keymap.set('n', '<leader>k', dofile(dir .. '/mine.lua'), { desc = 'Mine' })\n",
    );
    let _ = session.register(&NativeConfig::all_enabled());
    let owners = session
        .wait_for(ARRIVAL, |msg| match msg {
            Msg::UserMappingOwners { owners } => Some(owners.clone()),
            _ => None,
        })
        .expect("the registration reads whose the user's own keys are");
    let script = |lhs: &str| {
        owners
            .iter()
            .find(|(key, _)| key == lhs)
            .map(|(_, owner)| owner.script.clone())
            .unwrap_or_else(|| panic!("{lhs} must be read: {owners:?}"))
    };
    assert_eq!(script(",d"), None, "nvim's own function has no file");
    let mine = script(",k");
    assert!(
        mine.as_deref()
            .is_some_and(|script| script.ends_with("mine.lua:1")),
        "{mine:?}"
    );
}

/// A user's mapping that draws nothing, then view's key and a query typed
/// at once: the query waits for view's invocation, and none of it reaches
/// nvim as a command that edits the buffer. The same holds where a pause
/// inside view's key falls close to `'timeoutlen'` on view's clock and
/// inside it on nvim's.
#[test]
fn keys_typed_ahead_after_a_users_mapping_never_edit_the_buffer() {
    let session = Session::start_with(
        "typed-ahead",
        "vim.keymap.set('n', '<leader>j', '<cmd>let g:view_quiet = 1<CR>')\n",
    );
    let mut model = registered_model(&session);
    let keys = [",", "j", ",", "f", "b", "m", "a", "i", "n"];
    let sent = type_into(&session, &mut model, &keys);
    pump(&session, &mut model, ARRIVAL, |_, msg| {
        matches!(msg, Msg::FeatureInvoke { .. }).then_some(())
    })
    .expect("nvim runs view's <leader>fb");
    assert_eq!(session.eval("g:view_quiet"), "1", "the user's mapping ran");
    assert_eq!(session.eval("join(getline(1, '$'), '|')"), "");
    assert_eq!(session.eval("mode()"), "n");
    assert_eq!(sent, keys[..5], "the query waits for view's invocation");

    let session = Session::start_with(
        "typed-ahead-pause",
        "vim.o.timeoutlen = 1000\n\
         vim.keymap.set('n', '<leader>j', '<cmd>let g:view_quiet = 1<CR>')\n",
    );
    let mut model = registered_model(&session);
    // round trips 296 ms apart put an 800 ms pause within reach of the
    // 1000 ms 'timeoutlen', with 200 ms to spare on nvim's clock
    model.engine.key_round_trips[0] = Some(Duration::from_millis(4));
    model.engine.key_round_trips[1] = Some(Duration::from_millis(300));
    let mut sent = type_into(&session, &mut model, &[",", "f"]);
    std::thread::sleep(Duration::from_millis(800));
    sent.extend(type_into(&session, &mut model, &["b", "m", "a", "i", "n"]));
    pump(&session, &mut model, ARRIVAL, |_, msg| {
        matches!(msg, Msg::FeatureInvoke { .. }).then_some(())
    })
    .expect("nvim runs view's <leader>fb after the pause");
    assert_eq!(session.eval("join(getline(1, '$'), '|')"), "");
    assert_eq!(session.eval("mode()"), "n");
    assert_eq!(sent, [",", "f", "b"], "the query waits after a pause");
}

/// Whether `events` carries something nvim draws because it read a key.
fn answers_a_key(events: &[UiEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            UiEvent::GridCursorGoto { .. }
                | UiEvent::ModeChange { .. }
                | UiEvent::CmdlineShow { .. }
                | UiEvent::GridLine { .. }
                | UiEvent::MsgShowcmd { .. }
        )
    })
}

/// Sends `key` and keeps what nvim sends back, in the order the runtime
/// applies it, until nvim echoes the keys it waits on, each message
/// waited for within `budget`.
fn echoed(session: &Session, key: &str, budget: Duration) -> Vec<Msg> {
    session.engine.handle.input(key).unwrap();
    let mut kept = Vec::new();
    loop {
        let received = session
            .rx
            .recv_timeout(budget)
            .expect("nvim echoes the key");
        let msgs = dispatched(session, received);
        let echo = msgs.iter().any(|msg| {
            matches!(msg, Msg::Redraw(events) if events.iter().any(|event| {
                matches!(event, UiEvent::MsgShowcmd { content } if !content.is_empty())
            }))
        });
        kept.extend(msgs);
        if echo {
            return kept;
        }
    }
}

/// Reads `kept`, then sends `keys`, each its own input, and reads what
/// nvim sends back in the order the runtime applies it, each message
/// waited for within `budget`, until view's invocation: whether a batch
/// answering a key came first.
fn answered_before_invoke(
    session: &Session,
    kept: Vec<Msg>,
    keys: &[&str],
    budget: Duration,
) -> bool {
    for key in keys {
        session.engine.handle.input(key).unwrap();
    }
    let mut answered = false;
    let mut msgs = kept;
    loop {
        for msg in msgs {
            match msg {
                Msg::FeatureInvoke { .. } => return answered,
                Msg::Redraw(events) => answered |= answers_a_key(&events),
                _ => {}
            }
        }
        let received = session.rx.recv_timeout(budget).expect("view's invocation");
        msgs = dispatched(session, received);
    }
}

/// Reads every message nvim sent before it answered a request, which it
/// does only once it has drawn what the keys before it did.
fn settle(session: &Session) {
    session.eval("1");
    while let Ok(received) = session.rx.try_recv() {
        let _ = dispatched(session, received);
    }
    let _ = session.damage.take_damage_folded();
}

/// The order view's invocation and nvim's redraw arrive in for one of
/// view's keys, over sixty presses each typed at once, typed with every
/// echo read before the next key, and typed with the echoes read only once
/// the last key is out, as a link with a longer round trip than the typing
/// delivers them. With every echo read before the next key, the
/// invocation arrives ahead of the completing key's own answer every time.
/// The other two counts are printed: the earlier keys' echoes can arrive
/// after the completing key, so an answer seen after a view key never says
/// that nvim ran some other mapping on it.
#[test]
fn an_answer_to_an_earlier_key_arrives_ahead_of_views_invocation() {
    let session = Session::start("invoke-order");
    let _ = session.register(&NativeConfig::all_enabled());
    let _ = session.claims();
    settle(&session);
    let mut at_once = Vec::new();
    let mut spaced = 0;
    let mut far = 0;
    for press in 0..60 {
        if answered_before_invoke(&session, Vec::new(), &[",", "f", "f"], ARRIVAL) {
            at_once.push(press);
        }
        settle(&session);
        let _ = echoed(&session, ",", ARRIVAL);
        let _ = echoed(&session, "f", ARRIVAL);
        spaced += usize::from(answered_before_invoke(
            &session,
            Vec::new(),
            &["f"],
            ARRIVAL,
        ));
        settle(&session);
        let mut kept = echoed(&session, ",", ARRIVAL);
        kept.extend(echoed(&session, "f", ARRIVAL));
        far += usize::from(answered_before_invoke(&session, kept, &["f"], ARRIVAL));
        settle(&session);
    }
    eprintln!(
        "answered before the invocation: at once {at_once:?} of 60, spaced {spaced}/60, \
         far {far}/60"
    );
    assert_eq!(
        spaced, 0,
        "view's invocation precedes the completing key's own answer"
    );
}

/// A user's mapping whose last key would enter insert mode on its own,
/// then view's key and a query typed at once: the query waits for view's
/// invocation, and none of it edits the buffer.
#[test]
fn keys_typed_ahead_after_a_users_mapping_on_a_mode_key_never_edit_the_buffer() {
    let session = Session::start_with(
        "typed-ahead-mode-key",
        "vim.keymap.set('n', '<leader>a', '<cmd>let g:view_a = 1<CR>')\n",
    );
    let mut model = Model::with_term_size(80, 24);
    model.ai_trusted = true;
    session.register(&NativeConfig::all_enabled());
    let seen = std::cell::Cell::new((false, false));
    pump(&session, &mut model, ARRIVAL, |_, msg| {
        let (claimed, read) = seen.get();
        seen.set(match msg {
            Msg::MappingsClaimed { .. } => (true, read),
            Msg::UserMappingsRead { keys, .. } => (claimed, keys.iter().any(|keys| keys == ",a")),
            _ => (claimed, read),
        });
        (seen.get() == (true, true)).then_some(())
    })
    .expect("the registration answers and the user's keys are read");
    assert_eq!(model.engine.mode.current, "normal");
    let keys = [",", "a", ",", "f", "f", "m", "a", "i", "n"];
    let sent = type_into(&session, &mut model, &keys);
    pump(&session, &mut model, ARRIVAL, |_, msg| {
        matches!(msg, Msg::FeatureInvoke { .. }).then_some(())
    })
    .expect("nvim runs view's <leader>ff");
    assert_eq!(session.eval("g:view_a"), "1", "the user's mapping ran");
    assert_eq!(session.eval("join(getline(1, '$'), '|')"), "");
    assert_eq!(session.eval("mode()"), "n");
    assert_eq!(sent, keys[..5], "the query waits for view's invocation");
}

/// A stub that maps the real handler over itself and types its keys again
/// logs one row for one press, naming the stub.
#[test]
fn a_lazy_stub_logs_one_row_naming_the_stub() {
    let session = Session::start_with(
        "lazy-stub",
        "vim.g.view_stub = 0\n\
         vim.keymap.set('n', '<leader>j', function()\n\
         \x20 vim.keymap.set('n', '<leader>j', function() vim.g.view_stub = vim.g.view_stub + 1 end,\n\
         \x20   { desc = 'Real' })\n\
         \x20 vim.api.nvim_feedkeys(',j', 'm', false)\n\
         end, { desc = 'Stub' })\n",
    );
    let mut model = registered_model(&session);
    assert_eq!(type_into(&session, &mut model, &[",", "j"]), [",", "j"]);
    assert_eq!(
        session.eval("g:view_stub"),
        "1",
        "the real handler ran once"
    );
    let rows = user_rows(&model);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, ",j");
    assert_eq!(
        rows[0].1.as_ref().map(|owner| owner.label.as_str()),
        Some("Stub")
    );

    idle_until_read(&session, &mut model, ",j", "Real");
    // the stub draws nothing, so only a key pressed a round trip later is
    // read in normal mode
    session.eval("execute('lua vim.wait(300, function() return false end)')");
    let _ = type_into(&session, &mut model, &[",", "j"]);
    assert_eq!(session.eval("g:view_stub"), "2");
    let rows = row_labels(&model);
    assert_eq!(
        rows.last()
            .map(|(lhs, label, _)| (lhs.as_str(), label.as_str())),
        Some((",j", "Real")),
        "the second press ran the real handler: {rows:?}"
    );
}

/// Raises an idle moment in nvim and applies the pump until `lhs` is read
/// as `label`'s, then until nvim is in normal mode.
fn idle_until_read(session: &Session, model: &mut Model, lhs: &str, label: &str) {
    session.eval("execute('doautocmd CursorHold')");
    pump(session, model, ARRIVAL, |_, msg| match msg {
        Msg::UserMappingOwners { owners } => owners
            .iter()
            .any(|(key, owner)| key == lhs && owner.label == label)
            .then_some(()),
        _ => None,
    })
    .unwrap_or_else(|| panic!("an idle moment must read {lhs} as {label}'s"));
    if model.engine.mode.current != "normal" {
        pump(session, model, ARRIVAL, |model, _| {
            (model.engine.mode.current == "normal").then_some(())
        })
        .expect("nvim must report normal mode");
    }
}

/// A buffer mapping a deferred callback sets after the buffer is entered
/// is read at the next idle moment, and its keys log the buffer's own
/// mapping over the shorter global one.
#[test]
fn a_buffer_mapping_set_after_the_buffer_is_entered_is_logged() {
    let session = Session::start_with(
        "deferred-buffer-map",
        "vim.g.view_hs = 0\n\
         vim.keymap.set('n', '<leader>h', function() end, { desc = 'Global h' })\n\
         vim.api.nvim_create_autocmd('BufEnter', { callback = function(args)\n\
         \x20 vim.defer_fn(function()\n\
         \x20   vim.keymap.set('n', '<leader>hs', function() vim.g.view_hs = 1 end,\n\
         \x20     { buffer = args.buf, desc = 'Deferred hs' })\n\
         \x20 end, 20)\n\
         end })\n",
    );
    let mut model = registered_model(&session);
    session.eval("execute('enew')");
    session.eval("execute('lua vim.wait(200, function() return false end)')");
    idle_until_read(&session, &mut model, ",hs", "Deferred hs");
    let keys = [",", "h", "s"];
    assert_eq!(type_into(&session, &mut model, &keys), keys);
    assert_eq!(session.eval("g:view_hs"), "1", "the buffer's ,hs ran");
    let rows = row_labels(&model);
    assert_eq!(
        rows.last(),
        Some(&(",hs".to_string(), "Deferred hs".to_string(), true)),
        "{rows:?}"
    );
}

/// A `:View` line nvim refuses runs nothing, so the error it reports for
/// the line releases the keys held behind it, whichever way nvim hands
/// its messages over, and where the message row already shows an earlier
/// error the new one is drawn against. The pump never delivers the hold's
/// own bound, so the release is the error's.
#[test]
fn keys_behind_a_view_line_nvim_refuses_reach_nvim_on_its_error() {
    use view_core::native::ext::Ext;
    let line = [
        ":", "f", "i", "l", "t", "e", "r", "<Space>", "/", "<Bslash>", "(", "/", "<Space>", "V",
        "i", "e", "w", "<Space>", "a", "i", "<Space>", "o", "p", "e", "n",
    ];
    let bogus = [":", "b", "o", "g", "u", "s", "<CR>"];
    let grid = || vec![Ext::LineGrid];
    let cmdline = || vec![Ext::LineGrid, Ext::Cmdline];
    let multigrid = || vec![Ext::LineGrid, Ext::Cmdline, Ext::Multigrid];
    for (name, surfaces, after_error, options, signs) in [
        (
            "messages",
            vec![Ext::LineGrid, Ext::Cmdline, Ext::Messages],
            false,
            "",
            false,
        ),
        ("grid", grid(), false, "", true),
        ("grid-laststatus-0", grid(), false, LASTSTATUS_0, true),
        ("grid-cmdheight-0", grid(), false, CMDHEIGHT_0, true),
        ("grid-cmdheight-2", grid(), false, CMDHEIGHT_2, true),
        ("cmdline-cmdheight-2", cmdline(), false, CMDHEIGHT_2, true),
        ("multigrid", multigrid(), false, "", false),
        ("multigrid-after-error", multigrid(), true, "", false),
        ("cmdline-after-error", cmdline(), true, "", false),
    ] {
        // every window row shows a sign in `ErrorMsg`'s id, on a screen
        // narrow enough to wrap a `:View` line where nvim draws it
        let (extra, width) = if signs {
            (format!("{options}{SIGNS}"), 30)
        } else {
            (options.to_string(), 80)
        };
        let names: Vec<_> = surfaces.iter().map(|surface| surface.as_str()).collect();
        let session = Session::start_attached(&format!("refused-{name}"), &extra, &names, width);
        let mut model = Model::with_term_size(width, 24);
        model.attach_surfaces(surfaces);
        model.ai_trusted = true;
        session.register(&NativeConfig::all_enabled());
        pump(&session, &mut model, ARRIVAL, |_, msg| {
            matches!(msg, Msg::MappingsClaimed { .. }).then_some(())
        })
        .expect("the registration must answer with its claim list");
        // view tracks a `:` as a line only in a mode nvim has reported
        assert_eq!(model.engine.mode.current, "normal", "{name}");
        if after_error {
            assert_eq!(type_into(&session, &mut model, &bogus), bogus);
            pump(&session, &mut model, ARRIVAL, |model, msg| {
                let Msg::Redraw(events) = msg else {
                    return None;
                };
                let drawn = events.iter().any(|event| {
                    matches!(event, UiEvent::GridLine { col_start: 0, cells, .. }
                        if cells.first().is_some_and(|cell| cell.text == "E"))
                });
                (drawn && model.engine.mode.current == "normal").then_some(())
            })
            .expect("nvim must draw the E492 its line reports");
        }
        assert_eq!(type_into(&session, &mut model, &line).len(), line.len());
        assert_eq!(type_into(&session, &mut model, &["<CR>"]), ["<CR>"]);
        assert!(
            model.submit_hold.is_holding(),
            "{name}: the line names View"
        );
        let typed = ["i", "h", "e", "l", "l", "o"];
        assert!(type_into(&session, &mut model, &typed).is_empty(), "{name}");

        let released = pump(&session, &mut model, ARRIVAL, |model, msg| {
            if matches!(msg, Msg::FeatureInvoke { .. }) {
                return Some(Err(format!("{msg:?}")));
            }
            (!model.submit_hold.is_holding()).then_some(Ok(matches!(msg, Msg::Redraw(_))))
        });
        assert_eq!(
            released,
            Some(Ok(true)),
            "{name}: the error nvim reports must release the held keys"
        );
        assert_eq!(session.eval("getline(1)"), "hello", "{name}");
        assert_eq!(session.eval("winnr('$')"), "1", "{name}: no window opened");
        assert_eq!(session.invoke(SILENCE), None, "{name}: nothing was invoked");
        if signs {
            keys_behind_a_wrapped_view_line_reach_what_it_opened(&session, &mut model);
        }
    }
}

/// No statusline under a single window, so a wrapped command line borrows
/// a window row.
const LASTSTATUS_0: &str = "vim.o.laststatus = 0\n";

/// No command-line row, so the command line overlays window rows.
const CMDHEIGHT_0: &str = "vim.o.cmdheight = 0\nvim.o.laststatus = 0\n";

/// A two-row command line, whose first row nvim draws an error on.
const CMDHEIGHT_2: &str = "vim.o.cmdheight = 2\n";

/// Forty empty lines, each with a sign nvim draws in `ErrorMsg`'s id.
const SIGNS: &str = "\
vim.api.nvim_buf_set_lines(0, 0, -1, false, vim.fn['repeat']({ '' }, 40))
vim.o.signcolumn = 'yes'
local ns = vim.api.nvim_create_namespace('signs')
for i = 0, 39 do
  vim.api.nvim_buf_set_extmark(0, ns, i, 0,
    { sign_text = 'E', sign_hl_group = 'DiagnosticSignError' })
end
";

/// A valid `:View` line, wrapping onto two rows where nvim draws the
/// command line, on a screen whose window rows show error signs, typed
/// with the keys behind it inside one round trip and read in the runtime
/// loop's order. nvim redraws those rows as it leaves the command line
/// after the invocation, and the keys wait for the invocation and reach
/// the composer it opened.
fn keys_behind_a_wrapped_view_line_reach_what_it_opened(session: &Session, model: &mut Model) {
    // the silence watch before this read past redraw tokens without
    // taking their damage, and no token follows until it is taken
    send(
        session,
        &update(model, Msg::Redraw(session.damage.take_damage())),
    );
    assert_eq!(type_into(session, model, &["<Esc>"]), ["<Esc>"]);
    pump(session, model, ARRIVAL, |model, _| {
        (model.engine.mode.current == "normal").then_some(())
    })
    .expect("nvim must leave insert mode");
    // `:View` splits its arguments on whitespace, so the run of spaces
    // wraps the line and leaves the verb `open`
    let line: Vec<String> = ":View ai                          open"
        .chars()
        .map(|c| match c {
            ' ' => "<Space>".to_string(),
            c => c.to_string(),
        })
        .collect();
    let line: Vec<&str> = line.iter().map(String::as_str).collect();
    assert_eq!(type_into(session, model, &line).len(), line.len());
    assert_eq!(type_into(session, model, &["<CR>"]), ["<CR>"]);
    assert!(model.submit_hold.is_holding(), "the line names View");
    assert!(type_into(session, model, &["a", "b", "c"]).is_empty());

    // nothing is read until nvim has answered a request behind the line,
    // so the redraw that leaves the command line is already staged when
    // the token ahead of the invocation is drained
    let arrived = std::cell::RefCell::new(Vec::new());
    session
        .wait_for(ARRIVAL, |msg| {
            arrived.borrow_mut().push(msg.clone());
            matches!(msg, Msg::FeatureInvoke { .. }).then_some(())
        })
        .expect("nvim must invoke the feature");
    session.eval("1");

    let invoked = std::cell::Cell::new(false);
    let left = std::cell::Cell::new(false);
    let observe = |model: &Model, msg: &Msg| {
        match msg {
            Msg::FeatureInvoke { .. } => {
                assert!(
                    !model.submit_hold.is_holding(),
                    "the invocation ends the hold"
                );
                invoked.set(true);
            }
            Msg::Redraw(events) => {
                assert!(
                    invoked.get() || model.submit_hold.is_holding(),
                    "a redraw before the invocation released the keys"
                );
                for event in events {
                    if let UiEvent::ModeChange { mode, .. } = event {
                        left.set(!is_cmdline_mode(mode));
                    }
                }
            }
            _ => {}
        }
        (invoked.get() && left.get()).then_some(())
    };
    let mut done = None;
    for received in arrived.take() {
        for msg in dispatched(session, received) {
            send(session, &update(model, msg.clone()));
            done = done.or(observe(model, &msg));
        }
    }
    if done.is_none() {
        pump(session, model, ARRIVAL, observe)
            .expect("nvim must leave the command line after the invocation");
    }
    assert_eq!(model.ai_panel().input(), "abc");
    assert_eq!(session.eval("getline(1)"), "hello");
    assert_eq!(session.eval("mode()"), "n");
}

/// The `:` reading the palette's speculation is gated on, taken off the same
/// keymap snapshot the claims are.
///
/// Live rather than decoded from a canned reply: what is asserted is that a
/// real nvim, having read a real config, answers `true` for a `:` the config
/// mapped -- the one thing a fixture reply cannot say anything about.
#[test]
fn the_claim_report_says_whether_the_users_config_maps_colon() {
    let free = Session::start("colon-free");
    free.register(&NativeConfig::all_enabled());
    assert!(
        !free.report().1,
        "nothing in this fixture maps `:`, so the palette may speculate on it"
    );

    let mapped = Session::start_with(
        "colon-mapped",
        "vim.keymap.set('n', ':', ':', { silent = true })\n",
    );
    mapped.register(&NativeConfig::all_enabled());
    assert!(
        mapped.report().1,
        "the config maps `:` in normal mode, and a speculated palette would be drawn for a key \
         that reaches the mapping instead of the command line"
    );
}

/// The half the registration-time reading cannot answer: a `:` an ftplugin
/// maps in a buffer opened after the session started.
///
/// The reading is taken over the global maps and the current buffer's own,
/// so the buffer it has to be retaken on is whichever one the user is
/// typing into -- which is what the `FileType` trigger is for. Live because
/// the thing under test is an autocommand inside nvim: no decoded reply can
/// say whether one fired.
#[test]
fn an_ftplugin_mapping_colon_in_a_later_buffer_closes_the_gate() {
    let session = Session::start_with(
        "colon-ftplugin",
        "vim.api.nvim_create_autocmd('FileType', {\n\
         \x20 pattern = 'lua',\n\
         \x20 callback = function() vim.keymap.set('n', ':', ':', { buffer = 0 }) end,\n\
         })\n",
    );
    session.register(&NativeConfig::all_enabled());
    assert!(
        !session.report().1,
        "nothing maps `:` in the buffer the session started in"
    );

    session.eval("execute('setfiletype lua')");

    let mapped = session
        .wait_for(ARRIVAL, |msg| match msg {
            Msg::ColonMappingRead { mapped } => Some(*mapped),
            _ => None,
        })
        .expect("the re-read must report the answer moving");
    assert!(
        mapped,
        "the ftplugin's buffer-local `:` is the key the next keystroke reaches, and a palette \
         speculated for it would be drawn for a mapping"
    );
}

/// The buffer the reading is about is whichever one the user is typing
/// into, and a window switch changes that without opening anything: no
/// `FileType`, no `BufWinEnter`, and -- before `BufEnter` joined them --
/// no re-read either.
///
/// The direction that matters is entering the mapped buffer from the
/// unmapped one: the answer stays cached `false`, the gate opens, and every
/// `:` in that window speculates a palette for a key that reaches the
/// mapping instead. The test asserts both directions, because a re-read
/// that only ever reported `true` would leave the other window
/// unaccelerated for the rest of the session.
#[test]
fn moving_into_a_window_whose_buffer_maps_colon_closes_the_gate_and_leaving_it_opens_it() {
    let session = Session::start_with(
        "colon-window-switch",
        "vim.api.nvim_create_autocmd('FileType', {\n\
         \x20 pattern = 'lua',\n\
         \x20 callback = function() vim.keymap.set('n', ':', ':', { buffer = 0 }) end,\n\
         })\n",
    );
    session.register(&NativeConfig::all_enabled());
    assert!(
        !session.report().1,
        "nothing maps `:` in the buffer the session started in"
    );
    // a second window holding a second buffer, whose ftplugin maps `:`,
    // with the cursor left back in the first one
    session.eval("execute('split')");
    session.eval("execute('enew')");
    session.eval("execute('setfiletype lua')");
    assert!(
        session.next_colon_reading(),
        "the new buffer is the current one, and its ftplugin mapped `:`"
    );

    session.eval("execute('wincmd w')");
    assert!(
        !session.next_colon_reading(),
        "the window the cursor moved to holds a buffer nothing maps `:` in, so the gate reopens"
    );

    session.eval("execute('wincmd w')");
    assert!(
        session.next_colon_reading(),
        "moving back into the mapped buffer closes it again -- the reading follows the window, \
         and a stale `false` here speculates a palette for a key that reaches the mapping"
    );
}

/// The literal-taking set, re-derived from the engine it was read off.
///
/// `CMDLINE_LITERAL_KEYS` is what tells a `:` typed as a command from one
/// typed as `f`'s target, and it was transcribed by hand from the pinned
/// engine's `:help index.txt`. A version bump that adds an `x{char}`
/// command drifts the set silently, and nothing else in the tree reads that
/// file: every normal- and visual-mode entry whose command is one key
/// followed by a literal argument must name a key the set carries.
///
/// Skipped rather than failed where the runtime ships no documentation:
/// the set is still correct for the engine it was read off, and a test that
/// cannot read the file has nothing to say about it.
#[test]
fn every_literal_taking_key_the_pinned_engine_documents_is_in_the_set() {
    let session = Session::start("literal-keys");
    let runtime = session.eval("$VIMRUNTIME");
    let index = Path::new(&runtime).join("doc").join("index.txt");
    let Ok(text) = std::fs::read_to_string(&index) else {
        eprintln!(
            "skipped: {} is unreadable, so the pinned engine's own index is not there to \
             re-derive the set from",
            index.display()
        );
        return;
    };

    let derived = literal_taking_keys(&text);

    assert!(
        !derived.is_empty(),
        "{} no longer spells its commands where this reads them, so the derivation below \
         covers nothing",
        index.display()
    );
    for (key, command) in &derived {
        assert!(
            CMDLINE_LITERAL_KEYS.contains(&key.as_str()),
            "`{command}` reads its next keystroke as a literal, and `{key}` is not in \
             CMDLINE_LITERAL_KEYS: a `:` typed after it opens no command line, and view would \
             speculate a palette for it"
        );
    }
}

/// Every command in `index.txt`'s normal-mode and visual-mode sections
/// whose form is one key followed by a literal argument, as
/// `(the key in view's own notation, the command as the file spells it)`.
///
/// The file's own layout: `|tag|`, a tab, then the command, which is what
/// is read here. A brace group naming a motion, a filter, a pattern, a
/// count or a height is the other kind of argument -- another command, or
/// text -- and nvim announces a mode for a pending operator, which the
/// gate's own mode list already excludes.
fn literal_taking_keys(index: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in index.lines() {
        if line.contains("*normal-index*") {
            inside = true;
        } else if line.contains("*ex-edit-index*") {
            break;
        }
        if !inside || !line.starts_with('|') {
            continue;
        }
        let Some(command) = line.split('\t').filter(|field| !field.is_empty()).nth(1) else {
            continue;
        };
        let command = command.trim();
        let Some(key) = literal_taking_key(command) else {
            continue;
        };
        out.push((key, command.to_string()));
    }
    out
}

/// The key `command` takes its literal argument after, in view's own
/// notation, or `None` when `command` takes no literal argument.
fn literal_taking_key(command: &str) -> Option<String> {
    let inner = command.strip_suffix('}')?;
    let (key, argument) = inner.split_once('{')?;
    if argument.contains('{') || argument.is_empty() {
        return None;
    }
    // the placeholders that stand for another command or for text, never
    // for one keystroke
    if ["motion", "filter", "pattern", "height", "count"].contains(&argument) {
        return None;
    }
    let key = key.trim();
    if let Some(name) = key.strip_prefix("CTRL-") {
        // the set carries both cases of a control key, since nvim's own
        // notation for one is not view's
        return Some(format!("<C-{}>", name.to_lowercase()));
    }
    (key.chars().count() == 1).then(|| key.to_string())
}

/// The command tables a `:` line is read with, re-derived from the engine
/// they were read off.
///
/// `TAKES_BAR` is `:help :bar`'s list and `MODIFIERS` is
/// `:help :command-modifiers`' list less `:sandbox`, both transcribed by
/// hand, so a version bump that adds or drops a command drifts them
/// silently. The commands' fewest-characters counts are asked of the
/// engine's own `fullcommand()`. The modifiers' counts are asked of the
/// modifier parser, which keeps its own minimums, by running a user `View`
/// that records its `<q-mods>` behind each one at its count and at one
/// character fewer. `:filter` is left out of `<q-mods>`, so its rows ask
/// whether `View` ran at all.
///
/// Skipped where the runtime ships no documentation, for the reason the
/// literal-key test above gives.
#[test]
fn the_command_tables_match_the_pinned_engines_help() {
    use view_core::native::submit_hold::commands::{FILTER, MODIFIERS, SCRIPT_COMMANDS, TAKES_BAR};

    let session = Session::start_with(
        "command-tables",
        "vim.cmd([[\n\
         command! -nargs=* View let g:ran = <q-mods>\n\
         function! Ran(line)\n\
           let g:ran = 'not run'\n\
           try\n\
             exe a:line\n\
           catch\n\
           endtry\n\
           return g:ran\n\
         endfunction\n\
         ]])\n",
    );
    let doc = Path::new(&session.eval("$VIMRUNTIME")).join("doc");
    let (Ok(cmdline), Ok(map)) = (
        std::fs::read_to_string(doc.join("cmdline.txt")),
        std::fs::read_to_string(doc.join("map.txt")),
    ) else {
        eprintln!(
            "skipped: {} holds no cmdline.txt and map.txt to re-derive the tables from",
            doc.display()
        );
        return;
    };

    let (named, forms) = bar_commands(&cmdline);
    let names = |table: &[(&'static str, usize)]| -> Vec<&'static str> {
        table.iter().map(|&(name, _)| name).collect()
    };
    assert_eq!(names(&TAKES_BAR), named, ":help :bar lists these commands");
    assert_eq!(
        forms,
        [":read !", ":write !", ":[range]!"],
        ":help :bar lists these filter forms, which `takes_bar` reads apart from the table"
    );
    let mut listed = command_modifiers(&map);
    assert!(
        listed.contains(&"sandbox"),
        ":help :command-modifiers no longer lists :sandbox, which MODIFIERS leaves out"
    );
    listed.retain(|&name| name != "sandbox");
    assert_eq!(
        names(&MODIFIERS),
        listed,
        ":help :command-modifiers lists these modifiers besides :sandbox"
    );

    for &(full, shortest) in TAKES_BAR.iter().chain(&SCRIPT_COMMANDS) {
        let runs = |typed: &str| session.eval(&format!("fullcommand('{typed}')")) == full;
        assert!(
            runs(&full[..shortest]),
            "`:{}` runs `:{full}`",
            &full[..shortest]
        );
        assert!(
            shortest == 1 || !runs(&full[..shortest - 1]),
            "`:{}` already runs `:{full}`, so {shortest} is not the fewest characters",
            &full[..shortest - 1]
        );
    }

    const NOT_RUN: &str = "not run";
    let ran = |line: &str| session.eval(&format!("Ran('{line}')"));
    assert_eq!(
        ran("sandbox View"),
        NOT_RUN,
        "`:sandbox View` runs a user command, so MODIFIERS should list :sandbox"
    );
    for &(full, shortest) in &MODIFIERS {
        let reported = match full {
            "leftabove" => "aboveleft",
            "rightbelow" => "belowright",
            _ => full,
        };
        let line = |length: usize| format!("{} View", &full[..length]);
        assert_eq!(
            ran(&line(shortest)),
            reported,
            "`:{}` runs `:View` behind `:{full}`",
            line(shortest)
        );
        assert_ne!(
            ran(&line(shortest - 1)),
            reported,
            "`:{}` already runs `:View` behind `:{full}`, so {shortest} is not the fewest characters",
            line(shortest - 1)
        );
    }
    let (full, shortest) = FILTER;
    for pattern in [" /x/", " /[/]/", " /x/g"] {
        let line = |length: usize| format!("{}{pattern} View", &full[..length]);
        assert_ne!(
            ran(&line(shortest)),
            NOT_RUN,
            "`:{}` does not run `:View`",
            line(shortest)
        );
        assert_eq!(
            ran(&line(shortest - 1)),
            NOT_RUN,
            "`:{}` already runs `:View`, so {shortest} is not the fewest characters of `:{full}`",
            line(shortest - 1)
        );
    }
}

/// The commands `:help :bar` lists by name, and the forms it lists with
/// an argument (`:read !`), in the order the file gives them.
fn bar_commands(help: &str) -> (Vec<&str>, Vec<&str>) {
    let (named, forms) = help
        .lines()
        .skip_while(|line| !line.contains("*:bar*"))
        .skip_while(|line| !line.starts_with("    :"))
        .take_while(|line| line.starts_with("    :"))
        .map(str::trim)
        .partition::<Vec<_>, _>(|entry| entry[1..].chars().all(|c| c.is_ascii_alphanumeric()));
    (named.into_iter().map(|entry| &entry[1..]).collect(), forms)
}

/// The modifiers `:help :command-modifiers` lists, in its order.
fn command_modifiers(help: &str) -> Vec<&str> {
    let Some(start) = help.find("*:command-modifiers*") else {
        return Vec::new();
    };
    let paragraph = &help[start..];
    let end = paragraph.find("Note that").unwrap_or(paragraph.len());
    paragraph[..end]
        .split('|')
        .filter_map(|token| token.strip_prefix(':'))
        .filter(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase()))
        .collect()
}
