//! Live-nvim proof that `window zoom`'s second press reads real vsplit
//! geometry: `tile_is_zoomed` squeezes a sibling to its layout minimum, not
//! a fixture's guess at what `<C-w>_<C-w>|` leaves behind. Drives the
//! actual `update()` dispatch against redraw traffic a real spawned nvim
//! sends, the same path the runtime loop uses.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use view_core::model::Model;
use view_core::msg::{Effect, Msg, RpcCall};
use view_core::update::update;
use view_engine::process::{Engine, EngineConfig};
use view_engine::ui_events::UiEvent;

/// Drains every `RedrawReady` token until `settle` passes with none
/// arriving, bounded overall by two round trips of [`common::rpc_deadline_for`]:
/// a `:vsplit` or an `input()` answers with more than one `redraw` batch
/// (resize, win_pos, cursor_goto and flush each their own notify), so
/// stopping at the first non-empty one loses whichever of those a
/// follow-up batch still carries. Same idle-then-stop shape
/// `redraw_live.rs`'s scroll-storm drain uses.
fn drain(rx: &mpsc::Receiver<Msg>, pump: &view_engine::damage::DamagePump) -> Vec<UiEvent> {
    let mut events = Vec::new();
    let deadline = Instant::now() + common::rpc_deadline_for(2);
    let settle = view_test_support::host_deadline(Duration::from_millis(150));
    loop {
        if Instant::now() >= deadline {
            return events;
        }
        match rx.recv_timeout(settle) {
            Ok(Msg::RedrawReady) => events.extend(pump.take_damage()),
            Ok(_) => {}
            Err(_) => return events,
        }
    }
}

fn zoom_invoke() -> Msg {
    Msg::FeatureInvoke {
        feature: "window".to_string(),
        verb: "zoom".to_string(),
    }
}

/// The second `window zoom` press equalizes only once nvim's own layout
/// says the focused window is maximized -- proved against a real `vsplit`
/// rather than a hand-built `WinPos` pair.
#[test]
fn a_live_vsplit_zooms_then_equalizes_on_the_second_press() {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let (tx, rx) = mpsc::sync_channel(64);
    let (pump, _cutover) = engine.start_pump(tx);
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS_MULTIGRID)
        .unwrap();
    let _ = drain(&rx, &pump);

    engine.handle.command(":vsplit").unwrap();
    let mut model = Model::new();
    let _ = update(
        &mut model,
        Msg::Resized {
            width: 80,
            height: 24,
        },
    );
    let _ = update(&mut model, Msg::Redraw(drain(&rx, &pump)));

    let first = update(&mut model, zoom_invoke());
    let notation = match first.as_slice() {
        [Effect::Rpc(RpcCall::Input { notation })] => notation.clone(),
        other => panic!("expected a single Input effect from the first zoom press: {other:?}"),
    };
    assert_eq!(
        notation, "<C-w>_<C-w>|",
        "a fresh vsplit is not yet zoomed, so the first press must fill it"
    );

    engine.handle.input(&notation).unwrap();
    let _ = update(&mut model, Msg::Redraw(drain(&rx, &pump)));

    let second = update(&mut model, zoom_invoke());
    assert!(
        matches!(
            second.as_slice(),
            [Effect::Rpc(RpcCall::Input { notation })] if notation == "<C-w>="
        ),
        "nvim's own answer to <C-w>_<C-w>| must read back as already zoomed: {second:?}"
    );
}
