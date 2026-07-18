//! Live-nvim proof that [`decode_redraw`] matches the real wire formats,
//! not just fixtures constructed by hand. Attaches to a real spawned nvim
//! with the full ext set ([`view_engine::handle::EngineHandle::ui_attach`])
//! and asserts its actual `redraw` traffic decodes to typed events instead
//! of falling through to `Unknown`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};
use view_core::grid::Cell;
use view_core::model::Model;
use view_core::msg::Msg;
use view_core::update::update;
use view_engine::process::{Engine, EngineConfig};
use view_engine::ui_events::{decode_redraw, UiEvent};

#[test]
fn decodes_grid_line_and_flush_from_real_nvim_redraw() {
    let mut engine = Engine::spawn(EngineConfig::default()).unwrap();
    let notifications = engine.take_notifications().unwrap();
    engine.handle.ui_attach(80, 24).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_grid_line = false;
    let mut saw_flush = false;
    while Instant::now() < deadline && !(saw_grid_line && saw_flush) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(note) = notifications.recv_timeout(remaining) else {
            break;
        };
        if note.method != "redraw" {
            continue;
        }
        for event in decode_redraw(&note.params) {
            match event {
                UiEvent::GridLine { .. } => saw_grid_line = true,
                UiEvent::Flush => saw_flush = true,
                _ => {}
            }
        }
    }

    assert!(
        saw_grid_line,
        "expected at least one decoded GridLine from real nvim redraw traffic \
         (decoder likely mismatches the live wire arity again)"
    );
    assert!(
        saw_flush,
        "expected at least one decoded Flush from real nvim redraw traffic"
    );
    // no manual detach/kill: Engine's Drop reaps the child
}

#[test]
fn decodes_mode_change_and_cmdline_show_from_real_nvim_redraw() {
    let mut engine = Engine::spawn(EngineConfig::default()).unwrap();
    let notifications = engine.take_notifications().unwrap();
    engine.handle.ui_attach(80, 24).unwrap();
    // entering cmdline mode is what proves the full ext set actually took:
    // without ext_cmdline, nvim paints ":" straight into the grid instead
    // of emitting mode_change + cmdline_show.
    engine.handle.input(":").unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_mode_change = false;
    let mut saw_cmdline_show = false;
    while Instant::now() < deadline && !(saw_mode_change && saw_cmdline_show) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(note) = notifications.recv_timeout(remaining) else {
            break;
        };
        if note.method != "redraw" {
            continue;
        }
        for event in decode_redraw(&note.params) {
            match event {
                UiEvent::ModeChange { .. } => saw_mode_change = true,
                UiEvent::CmdlineShow { .. } => saw_cmdline_show = true,
                _ => {}
            }
        }
    }

    assert!(
        saw_mode_change,
        "expected at least one decoded ModeChange after entering cmdline mode \
         (decoder likely mismatches the live wire arity again)"
    );
    assert!(
        saw_cmdline_show,
        "expected at least one decoded CmdlineShow after `nvim_input(\":\")`"
    );
}

/// The captured-storm fixture for the damage compactor's property test
/// (`view-engine/src/damage.rs`'s `compaction_preserves_final_grid_and_non_grid_subsequence`
/// is the generative half; this is the real-captured-traffic half).
/// Unlike `ui_events.rs`'s decode fixtures, which are hand-copied from a
/// saved capture log, this test captures its own storm at run time: it
/// spawns a real nvim, opens a generated multi-thousand-line file (a real
/// `grid_line` burst with long runs, not the generator's synthetic ones)
/// and pages through it (real `grid_scroll` traffic, plus whatever
/// `win_viewport`/other `Unknown`-decoded noise nvim actually emits
/// alongside it), then proves the damage compactor against that exact
/// traffic: the same raw stream, folded through the real `Engine::start_pump`
/// pipeline and drained via `DamagePump::take_damage`, must apply to a
/// `Grid` identically to the uncompacted raw stream, and must preserve the
/// non-`GridOp` event subsequence exactly. There is no separate capture
/// file to cite; this test's setup section below is the capture.
#[test]
fn compacted_damage_matches_raw_across_a_real_edit_and_scroll_storm() {
    // `--clean` isolates this test from the host's real nvim config
    // (plugins, statuslines, autocmds): this test's assertions depend on
    // the exact shape of the redraw stream (a real GridScroll must appear),
    // not just "at least one event of some kind", so host-specific config
    // noise would make it nondeterministic across hosts, the same failure
    // mode any live-capture harness against a real nvim is exposed to.
    let cfg = EngineConfig {
        extra_args: vec!["--clean".into()],
        ..EngineConfig::default()
    };
    let mut engine = Engine::spawn(cfg).unwrap();
    let notifications = engine.take_notifications().unwrap();
    let (tx, _rx) = std::sync::mpsc::sync_channel(64);
    // the reader thread folds into this pump for every redraw notification
    // from here on, in parallel with the unchanged dual-path forward to
    // `notifications` above: draining `notifications` below observes
    // exactly the same events the pump already folded, since the fold
    // happens strictly before the reader forwards to the deprecated channel
    let pump = engine.start_pump(tx);
    engine.handle.ui_attach(80, 24).unwrap();

    let dir = std::env::temp_dir().join(format!(
        "view-engine-redraw-live-storm-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file_path = dir.join("big.txt");
    {
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 1..=5000 {
            writeln!(f, "line {i} padding padding padding padding padding").unwrap();
        }
    }

    engine
        .handle
        .notify(
            "nvim_command",
            vec![rmpv::Value::from(format!("e {}", file_path.display()))],
        )
        .unwrap();
    // scroll down one line at a time, pacing each keystroke so nvim
    // actually redraws between them: a same-frame burst of many small
    // scrolls (or one big multi-page jump) leaves no overlap between the
    // old and new viewport, and nvim's diffing correctly prefers a full
    // redraw over a scroll op with nothing to reuse. A single-line scroll
    // shares 23 of 24 rows with the previous frame, giving nvim's redraw
    // batching every incentive to actually emit grid_scroll for it.
    for _ in 0..8 {
        engine.handle.input("<C-e>").unwrap();
        std::thread::sleep(Duration::from_millis(30));
    }

    // drain the dual-path raw channel until it goes quiet for a full
    // settle window, or an overall budget elapses; either way the pump has
    // folded exactly the same notifications by the time this loop exits,
    // since the fold always happens before the forward on the reader thread
    let overall_deadline = Instant::now() + Duration::from_secs(10);
    let settle = Duration::from_millis(500);
    let mut raw: Vec<UiEvent> = Vec::new();
    loop {
        if Instant::now() >= overall_deadline {
            break;
        }
        match notifications.recv_timeout(settle) {
            Ok(note) if note.method == "redraw" => raw.extend(decode_redraw(&note.params)),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => break, // quiet: storm settled
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !raw.is_empty(),
        "captured no redraw traffic from the edit+scroll storm"
    );
    assert!(
        raw.iter().any(|e| matches!(e, UiEvent::GridScroll { .. })),
        "expected at least one real GridScroll from the paced <C-e> scrolling"
    );

    // take_damage only ever drains up to its own last-staged Flush; trim
    // the raw side to the same boundary so both sides compare the same
    // fully-flushed prefix rather than raw's trailing unflushed tail
    let Some(last_flush) = raw.iter().rposition(|e| matches!(e, UiEvent::Flush)) else {
        unreachable!("no Flush observed in a real redraw storm");
    };
    let raw = raw[..=last_flush].to_vec();

    let compacted = pump.take_damage();
    assert!(
        !compacted.is_empty(),
        "compactor drained nothing from a real, non-empty, flushed storm"
    );

    assert_eq!(
        grid_snapshot(raw.clone()),
        grid_snapshot(compacted.clone()),
        "compacted real storm produced a different final grid/cursor than the raw storm"
    );
    let is_grid_op = |e: &UiEvent| {
        matches!(
            e,
            UiEvent::GridResize { .. }
                | UiEvent::GridLine { .. }
                | UiEvent::GridCursorGoto { .. }
                | UiEvent::GridScroll { .. }
                | UiEvent::GridClear { .. }
        )
    };
    let raw_other: Vec<&UiEvent> = raw.iter().filter(|e| !is_grid_op(e)).collect();
    let compacted_other: Vec<&UiEvent> = compacted.iter().filter(|e| !is_grid_op(e)).collect();
    assert_eq!(
        raw_other, compacted_other,
        "non-GridOp subsequence diverged between raw and compacted real storm"
    );
}

/// Applies `events` through `update()` (the real `UiEvent` -> `GridOp`
/// translation) and returns enough of the resulting `Grid` to compare final
/// states: every cell plus the cursor, since `Grid` has no `PartialEq`.
fn grid_snapshot(events: Vec<UiEvent>) -> (Vec<Cell>, (u16, u16), (u16, u16)) {
    let mut model = Model::new();
    let _ = update(&mut model, Msg::Redraw(events));
    let (w, h) = model.engine.grid.size();
    let mut cells = Vec::with_capacity(usize::from(w) * usize::from(h));
    for r in 0..h {
        for c in 0..w {
            cells.push(model.engine.grid.cell(r, c).cloned().unwrap_or_default());
        }
    }
    (cells, (w, h), model.engine.grid.cursor())
}
