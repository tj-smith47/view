//! Non-interference proof (the design spec's own charter exit gate): opening a
//! native feature (picker, tree, notifications/message-history) and closing
//! it again must leave nvim's own engine state -- buffer text, cursor, mode,
//! registers, marks -- exactly as it was before. Drives
//! `corpus/native/*.toml`, a subdirectory `task oracle`'s own default
//! corpus-wide walk never descends into (the same mechanism
//! `corpus/quarantine/` relies on, see `collect_entries` in
//! `bin/oracle.rs`): these entries have no meaningful reference-nvim
//! counterpart to diverge against, since a plugin-free reference session
//! has no registered mapping for the leader key at all and would sit
//! legitimately (and uninterestingly) blocked in a dangling `f`
//! target-character wait instead.
//!
//! Deliberately does not reuse `view_oracle::EngineSession`: that driver's
//! `pump_until_flush` only ever turns `Msg::Redraw` traffic into `update()`
//! calls (see its own module docs), with no path for the
//! `Msg::FeatureInvoke`/`Msg::MappingsClaimed` notifications a native
//! mapping round-trip produces. `Driver` below is the smallest extension
//! that closes that gap, wired the same way
//! `crates/view/tests/mappings_live.rs::Session` already is: a real
//! `Engine` plus a real `Model`, every `Msg` off the raw pump channel
//! applied through the same `update()` production drives. `RpcCall` is
//! `#[non_exhaustive]`, so `Driver::forward_effect` names an explicit
//! allowlist of the read-only calls these open/close paths are known to
//! produce (`Input`, the picker's `buffers` source's `ListBuffers`) and
//! panics on anything else, rather than assuming `Input` is the only kind
//! and silently dropping a future state-mutating call this charter-exit
//! gate exists to catch.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use view_core::model::Model;
use view_core::msg::{Effect, Key, Msg, RpcCall};
use view_core::update::update;
use view_engine::process::{Engine, EngineConfig};
use view_harness::corpus::{self, CorpusEntry};
use view_native::config::NativeConfig;
use view_native::mappings::register_plan;
use view_oracle::{snapshot, OracleError, Probe};

/// How long a `MappingsClaimed`/`FeatureInvoke` notification is waited for,
/// before the host's own contention is accounted for. A `--clean`,
/// plugin-free session with nothing installing answers in milliseconds.
const ARRIVAL_BOUND: Duration = Duration::from_secs(10);

/// [`ARRIVAL_BOUND`] widened for the load this run started under: the whole
/// wait is a real engine being scheduled and answering, which costs
/// whatever the machine has left over, so a flat wall clock here fails on a
/// busy runner without saying anything about the notification path.
fn arrival() -> Duration {
    view_test_support::host_deadline(ARRIVAL_BOUND)
}

/// A real embedded engine plus the client-side `Model` production's own
/// `update()` mutates, drained by hand instead of the production runtime
/// loop this test has no paint loop to run.
struct Driver {
    engine: Engine,
    rx: Receiver<Msg>,
    model: Model,
}

impl Driver {
    fn start() -> Self {
        let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
        let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(256);
        let (_pump, _cutover) = engine.start_pump(tx);
        engine
            .handle
            .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
            .unwrap();
        Self {
            engine,
            rx,
            model: Model::with_term_size(80, 24),
        }
    }

    /// Registers every default key, exactly as the runtime's executor does
    /// on `VimEnter`, and blocks until nvim's own claim reply confirms it
    /// landed.
    fn register_native_mappings(&mut self) {
        let call = register_plan(
            &NativeConfig::all_enabled(),
            self.engine.api_info.channel_id,
        );
        match call {
            RpcCall::RegisterMappings { specs, channel_id } => {
                self.engine
                    .handle
                    .register_mappings(&specs, channel_id)
                    .unwrap();
            }
            other => panic!("register_plan built {other:?}"),
        }
        self.wait_for(|msg| matches!(msg, Msg::MappingsClaimed { .. }).then_some(()))
            .expect("registration must answer with MappingsClaimed");
    }

    /// Applies one `Msg` through view-core's own `update()`. Non-RPC
    /// effects (`TreeScan`, `PickerQuery`, `PickerClose`, ...) are worker/
    /// matcher effects this driver has no worker for and deliberately
    /// drops: this test proves engine state parity, not picker/tree
    /// functional behavior, which the compat scenarios already cover.
    /// `Effect::Rpc` payloads go through [`Self::forward_effect`] instead,
    /// which is exhaustive over the calls this driver is willing to serve.
    fn apply(&mut self, msg: Msg) {
        for effect in update(&mut self.model, msg) {
            if let Effect::Rpc(call) = effect {
                self.forward_effect(call);
            }
        }
    }

    /// Forwards an `RpcCall` the way the runtime's own `Executor` would, for
    /// the narrow allowlist these three features' open/close paths are
    /// known to produce: every keypress (`Input`), and the picker's
    /// `buffers` source's read-only buffer list (`ListBuffers`,
    /// `update.rs::open_picker`). `RpcCall` is `#[non_exhaustive]`, so an
    /// unrecognized variant panics by name instead of being silently
    /// dropped -- a future state-mutating RPC added to one of these paths
    /// must fail this test loudly, not slip through unnoticed.
    fn forward_effect(&mut self, call: RpcCall) {
        match call {
            RpcCall::Input { notation } => self.engine.handle.input(&notation).unwrap(),
            RpcCall::ListBuffers { generation } => {
                self.engine.handle.list_buffers(generation).unwrap();
            }
            other => panic!(
                "native feature open/close path produced an RPC call outside \
                 Driver::forward_effect's allowlist: {other:?} -- extend the allowlist if this \
                 is a legitimate read-only call this driver should serve, or investigate why \
                 this path now mutates engine state"
            ),
        }
    }

    /// Drains the raw channel, applying every `Msg` through [`Self::apply`]
    /// as it arrives, until `extract` finds one worth returning or
    /// [`arrival`] elapses.
    fn wait_for<T>(&mut self, extract: impl Fn(&Msg) -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + arrival();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let msg = self.rx.recv_timeout(left).ok()?;
            let found = extract(&msg);
            self.apply(msg);
            if found.is_some() {
                return found;
            }
        }
    }

    /// Sends `notation` the way a real keystroke reaches `update()`
    /// (`Msg::Key`, `Focus::Engine`, forwarded to nvim as `RpcCall::Input`),
    /// then waits for and applies the `Msg::FeatureInvoke` nvim's own
    /// registered mapping fires back.
    fn invoke(&mut self, notation: &str) -> (String, String) {
        self.apply(Msg::Key(Key {
            notation: notation.to_string(),
        }));
        self.wait_for(|msg| match msg {
            Msg::FeatureInvoke { feature, verb } => Some((feature.clone(), verb.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{notation} never produced a FeatureInvoke"))
    }

    /// Closes whatever overlay is on top the way a real `<Esc>` would:
    /// through `update()`'s own overlay-dismissal handling, never a direct
    /// call into `Model`.
    fn close_overlay(&mut self) {
        self.apply(Msg::Key(Key {
            notation: "<Esc>".to_string(),
        }));
    }

    /// A fixed, deterministic buffer/cursor/mark/register state a
    /// before/after snapshot pair can actually catch drift in: an empty
    /// snapshot with nothing set would let a picker that clobbered a mark
    /// pass by coincidence, having had no mark to clobber.
    fn seed_baseline(&mut self) {
        self.press("ihello world<Esc>0mayy");
    }

    /// Types `notation` and waits for nvim to have consumed it: the same
    /// `feedkeys` + barrier-eval pattern
    /// `crates/view/tests/mappings_live.rs::Session::press` uses, since
    /// `feedkeys` queues into typeahead rather than executing synchronously,
    /// and nvim answers a deferred request only once back waiting for
    /// input -- by which point the typed keys have already run.
    fn press(&self, notation: &str) {
        self.engine.handle.feed_keys(notation).unwrap();
        self.engine.handle.eval_str("1").unwrap();
    }
}

impl Probe for Driver {
    fn eval_str(&mut self, expr: &str) -> Result<String, OracleError> {
        self.engine.handle.eval_str(expr).map_err(Into::into)
    }

    fn get_mode(&mut self) -> Result<(String, bool), OracleError> {
        self.engine.handle.get_mode().map_err(Into::into)
    }

    fn input(&mut self, notation: &str) -> Result<(), OracleError> {
        self.engine.handle.input(notation).map_err(Into::into)
    }
}

/// Loads `corpus/native/<name>.toml`.
fn load(name: &str) -> CorpusEntry {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../corpus/native")
        .join(format!("{name}.toml"));
    corpus::load_file(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Runs one feature's full non-interference proof: seed a baseline with
/// something in every field a mark/register/cursor drift could show up in,
/// snapshot it, open the feature by its corpus-declared key, close it with
/// `<Esc>`, snapshot again, and assert the two are the same fact.
/// `expect_feature` is passed explicitly rather than derived from
/// `entry_name`'s `native-` prefix: `native-picker-buffers` and
/// `native-picker-grep` both invoke the `picker` feature through a
/// different verb, so the corpus file's own name no longer determines it.
fn assert_no_interference(entry_name: &str, expect_feature: &str) {
    let entry = load(entry_name);

    let mut driver = Driver::start();
    driver.register_native_mappings();
    driver.seed_baseline();

    let before = snapshot(&mut driver).unwrap();

    let (invoked_feature, _verb) = driver.invoke(&entry.input);
    assert_eq!(
        invoked_feature, expect_feature,
        "corpus/native/{entry_name}.toml's key must invoke the {expect_feature} feature"
    );
    driver.close_overlay();

    let after = snapshot(&mut driver).unwrap();

    assert_eq!(
        before, after,
        "opening and closing {expect_feature} must leave the engine's own state untouched"
    );
}

#[test]
fn opening_and_closing_the_picker_leaves_the_engine_untouched() {
    assert_no_interference("native-picker", "picker");
}

#[test]
fn opening_and_closing_the_tree_leaves_the_engine_untouched() {
    assert_no_interference("native-tree", "tree");
}

#[test]
fn opening_and_closing_message_history_leaves_the_engine_untouched() {
    assert_no_interference("native-notifications", "notifications");
}

#[test]
fn opening_and_closing_the_picker_buffers_source_leaves_the_engine_untouched() {
    assert_no_interference("native-picker-buffers", "picker");
}

#[test]
fn opening_and_closing_the_picker_grep_source_leaves_the_engine_untouched() {
    assert_no_interference("native-picker-grep", "picker");
}
