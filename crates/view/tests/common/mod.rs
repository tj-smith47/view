//! Scaffolding shared by this crate's live-nvim integration test binaries
//! (`bridge_live.rs`, `mappings_live.rs`, `picker_preview_live.rs`,
//! `supersede_live.rs`, `supervision_live.rs`): a scratch config-fixture writer, an
//! engine-plus-pump spawn, and the predicate-drain wait loop each of those
//! files re-derived on its own before this existed.
//!
//! Compiled separately into each test binary that declares `mod common;`,
//! so a helper only one of them needs reads as dead code in the others --
//! `dead_code` is allowed here for that reason alone, the same rationale
//! `view-oracle/tests/common/mod.rs` states for itself.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use view_core::msg::Msg;
use view_engine::process::{Engine, EngineConfig};
use view_engine::DamagePump;
use view_test_support::ScratchDir;

/// A scratch config directory holding `init.lua` with `contents` -- the
/// fixture shape `bridge_live`, `mappings_live`, and `supersede_live` each
/// wrote out by hand (their own `env::temp_dir()`-plus-pid-nonce, or their
/// own copy of this exact `ScratchDir` write) before this existed.
pub fn fixture(label: &str, contents: &str) -> ScratchDir {
    let dir = ScratchDir::new(label).unwrap();
    std::fs::write(dir.join("init.lua"), contents).unwrap();
    dir
}

/// Spawns `cfg`'s engine with its pump wired into a freshly created
/// `channel_capacity`-deep channel, returning the engine, the pump handle
/// (a caller that folds redraw traffic through `update()`, like
/// `bridge_live`, needs `DamagePump::take_damage`; one that does not can
/// discard it with `_`), and the receiving end.
pub fn spawn_with_pump(
    cfg: EngineConfig,
    channel_capacity: usize,
) -> (Engine, DamagePump, Receiver<Msg>) {
    let mut engine = Engine::spawn(cfg).unwrap();
    let (tx, rx): (SyncSender<Msg>, Receiver<Msg>) =
        std::sync::mpsc::sync_channel(channel_capacity);
    let (pump, _cutover) = engine.start_pump(tx);
    (engine, pump, rx)
}

/// Spawns `cfg`'s engine with its pump drained on a background thread for
/// the engine's lifetime, for a caller that has no need to inspect what the
/// pump delivers, only that it never backs up -- `Engine::start_pump`'s own
/// contract requires the sink stay drained, and an undropped, undrained
/// receiver stalls the reader thread. This is the shape that keeps a future
/// live test from reintroducing that stall.
pub fn spawn_with_drained_pump(cfg: EngineConfig) -> Engine {
    let mut engine = Engine::spawn(cfg).unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let (_pump, _cutover) = engine.start_pump(tx);
    std::thread::spawn(move || while rx.recv().is_ok() {});
    engine
}

/// The first `Msg` `rx` delivers that `want` answers, within `budget` -- the
/// raw predicate-drain shape `mappings_live` and `picker_preview_live` each
/// wrote out independently. A caller that must also fold every message
/// (matched or not) through `update()`, like `bridge_live`, needs a richer
/// variant and keeps its own.
pub fn drain_until<T>(
    rx: &Receiver<Msg>,
    budget: Duration,
    want: impl Fn(&Msg) -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(msg) => {
                if let Some(found) = want(&msg) {
                    return Some(found);
                }
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// A live nvim config directory's `init.lua` argument pair
/// (`EngineConfig::isolated().with_arg("-u").with_arg(...)` builds this
/// same shape at every call site that reads a fixture rather than running
/// `--clean`), factored out so the flag pairing itself cannot drift between
/// callers.
#[must_use]
pub fn isolated_reading(init_lua: &Path) -> EngineConfig {
    EngineConfig::isolated().with_arg("-u").with_arg(init_lua)
}
