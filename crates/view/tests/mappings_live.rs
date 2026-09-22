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

use view_core::model::Look;
use view_core::msg::{Msg, RpcCall};
use view_core::native::mappings::MappingClaim;
use view_core::native::registry;
use view_core::native::speculate::CMDLINE_LITERAL_KEYS;
use view_engine::process::Engine;
use view_native::config::NativeConfig;
use view_native::mappings::register_plan;
use view_native::report::report;
use view_native::supersede::plan;
use view_native::toast::first_run;
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
    dir: ScratchDir,
}

impl Session {
    fn start(name: &str) -> Self {
        Self::start_with(name, "")
    }

    /// The same fixture with `extra` appended to its `init.lua`.
    fn start_with(name: &str, extra: &str) -> Self {
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
        let (engine, _pump, rx) = common::spawn_with_pump(cfg, 256);
        engine
            .handle
            .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
            .unwrap();
        Self { engine, rx, dir }
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
            Msg::FeatureInvoke { feature, verb } => Some((feature.clone(), verb.clone())),
            _ => None,
        })
    }
}

/// The notices a first run would show for `claimed`, through the one report
/// every handover crosses.
fn notices(claimed: &[MappingClaim], record: &Path) -> Vec<String> {
    let features = registry::features();
    let handovers = report(
        &plan(&NativeConfig::all_enabled(), features, Look::default()),
        claimed,
        features,
    );
    first_run(&handovers, Some(Path::new("/cfg/view.toml")), record).unwrap()
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

    let record = session.dir.join("first-run.toml");
    let notices = notices(&claimed, &record);
    let key = notices
        .iter()
        .find(|n| n.contains("<leader>ff"))
        .unwrap_or_else(|| panic!("the claim must reach the first-run notice, got {notices:?}"));
    assert!(
        key.contains("native.picker = false"),
        "the notice must name the exact line that gives the key back, got {key:?}"
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
    // actually opens nvim's own command line rather than stopping at the
    // invoke arriving
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
            "<leader>ws".to_string(),
            "<leader>uf".to_string(),
            "<leader>w1".to_string(),
            "<leader>w2".to_string(),
            "<leader>w3".to_string(),
            "<leader>w4".to_string(),
            "<leader>w5".to_string(),
            "<leader>w6".to_string(),
            "<leader>w7".to_string(),
            "<leader>w8".to_string(),
            "<leader>w9".to_string(),
        ],
        "a disabled feature must contribute no key of its own; the survivors \
             are ai's default key, the two ui actions and window's own tile \
             keys, none of which [native] has a switch for, got {registered:?}"
    );
    let claimed = session.claims();
    assert_eq!(
        claimed.len(),
        16,
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
            "<leader>ws",
            "<leader>uf",
            "<leader>w1",
            "<leader>w2",
            "<leader>w3",
            "<leader>w4",
            "<leader>w5",
            "<leader>w6",
            "<leader>w7",
            "<leader>w8",
            "<leader>w9",
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
        16,
        "only ai's key, the two ui actions and window's own tile keys, \
             none of which [native] can turn off, survive every other \
             feature being disabled"
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
