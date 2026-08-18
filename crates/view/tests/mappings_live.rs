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

use view_core::msg::{Msg, RpcCall};
use view_core::native::mappings::MappingClaim;
use view_core::native::registry;
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
        let dir = common::fixture(
            &format!("mappings-live-{name}"),
            &format!(
                "vim.g.mapleader = '{LEADER}'\n\
                 vim.g.view_user_ff = 0\n\
                 vim.keymap.set('n', '<leader>ff', function() vim.g.view_user_ff = 1 end)\n"
            ),
        );
        let cfg = common::isolated_reading(&dir.join("init.lua"));
        let (engine, _pump, rx) = common::spawn_with_pump(cfg, 256);
        engine.handle.ui_attach(80, 24).unwrap();
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

    fn claims(&self) -> Vec<MappingClaim> {
        self.wait_for(ARRIVAL, |msg| match msg {
            Msg::MappingsClaimed { claimed } => Some(claimed.clone()),
            _ => None,
        })
        .expect("the registration must answer with its claim list")
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
        &plan(&NativeConfig::all_enabled(), features),
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
fn a_disabled_feature_leaves_the_users_own_mapping_firing() {
    let session = Session::start("disabled");
    let cfg = NativeConfig::from_toml_str(
        "[native]\npicker = false\ntree = false\nnotifications = false\n",
    )
    .unwrap();

    let registered = session.register(&cfg);
    assert_eq!(
        registered,
        vec!["<leader>a".to_string()],
        "a disabled feature must contribute no key of its own; the only \
             survivor is ai's default key, which [native] has no switch to \
             turn off, got {registered:?}"
    );
    let claimed = session.claims();
    assert_eq!(
        claimed.len(),
        1,
        "only ai's key, which no [native] entry here names, may be claimed: {claimed:?}"
    );
    assert_eq!(claimed[0].lhs, "<leader>a");

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
        "[native]\npicker = false\ntree = false\nnotifications = false\n",
    )
    .unwrap();
    session.register(&cfg);
    assert_eq!(
        session.claims().len(),
        1,
        "only ai's key, which [native] cannot turn off, survives every other \
             feature being disabled: {:?}",
        session.claims()
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

    session.engine.handle.input(":View picker grep\r").unwrap();
    session.eval("1");
    assert_eq!(
        session.invoke(ARRIVAL),
        Some(("picker".to_string(), "grep".to_string())),
        "the command must invoke the feature it names"
    );
}
