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
//! applied through the same `update()` production drives, with
//! `Effect::Rpc(RpcCall::Input)` -- the one RPC effect these three
//! features' open/close paths ever produce -- forwarded back to nvim the
//! way the runtime's own `Executor` would.
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

/// How long a `MappingsClaimed`/`FeatureInvoke` notification is waited for.
/// Generous for a loaded CI box; a `--clean`, plugin-free session with
/// nothing installing answers in milliseconds.
const ARRIVAL: Duration = Duration::from_secs(10);

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
        engine.handle.ui_attach(80, 24).unwrap();
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

    /// Applies one `Msg` through view-core's own `update()`, forwarding the
    /// one RPC effect kind (`RpcCall::Input`) these three features' open and
    /// close paths ever produce back to nvim the way the runtime's
    /// `Executor` would. Every other effect (`TreeScan`, `PickerQuery`,
    /// `PickerClose`, ...) is a non-RPC worker/matcher effect this driver
    /// has no worker for and deliberately drops: this test proves engine
    /// state parity, not picker/tree functional behavior, which the compat
    /// scenarios already cover.
    fn apply(&mut self, msg: Msg) {
        for effect in update(&mut self.model, msg) {
            if let Effect::Rpc(RpcCall::Input { notation }) = effect {
                self.engine.handle.input(&notation).unwrap();
            }
        }
    }

    /// Drains the raw channel, applying every `Msg` through [`Self::apply`]
    /// as it arrives, until `extract` finds one worth returning or
    /// [`ARRIVAL`] elapses.
    fn wait_for<T>(&mut self, extract: impl Fn(&Msg) -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + ARRIVAL;
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

/// Loads `corpus/native/<name>.toml`, plus the feature id its invoking key
/// must produce -- derived from the file's own `native-` prefix rather than
/// carried as a second field, since the corpus schema's
/// `#[serde(deny_unknown_fields)]` has no room for one and a derived name
/// cannot drift from the file it names.
fn load(name: &str) -> (CorpusEntry, String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../corpus/native")
        .join(format!("{name}.toml"));
    let entry = corpus::load_file(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let feature = name
        .strip_prefix("native-")
        .unwrap_or_else(|| panic!("{name} must carry the native- prefix"))
        .to_string();
    (entry, feature)
}

/// Runs one feature's full non-interference proof: seed a baseline with
/// something in every field a mark/register/cursor drift could show up in,
/// snapshot it, open the feature by its corpus-declared key, close it with
/// `<Esc>`, snapshot again, and assert the two are the same fact.
fn assert_no_interference(entry_name: &str) {
    let (entry, expect_feature) = load(entry_name);

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
    assert_no_interference("native-picker");
}

#[test]
fn opening_and_closing_the_tree_leaves_the_engine_untouched() {
    assert_no_interference("native-tree");
}

#[test]
fn opening_and_closing_message_history_leaves_the_engine_untouched() {
    assert_no_interference("native-notifications");
}
