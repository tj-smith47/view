//! Live-nvim proof that [`decode_redraw`] matches the real wire formats,
//! not just fixtures constructed by hand. Attaches to a real spawned nvim
//! with the full ext set ([`view_engine::handle::EngineHandle::ui_attach`])
//! and asserts its actual `redraw` traffic decodes to typed events instead
//! of falling through to `Unknown`.
//!
//! All three tests here drain traffic through [`Engine::start_pump`], the
//! same production path the runtime loop uses: `Engine` routes every
//! connection through its damage pump exclusively (see
//! `view_engine::handle::EngineHandle::start_pumped`'s docs), so there is no
//! separate raw notification channel left on a live `Engine` to observe
//! wire traffic through instead.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};
use view_core::model::Model;
use view_core::msg::Msg;
use view_core::update::update;
use view_engine::process::{Engine, EngineConfig};
use view_engine::ui_events::UiEvent;

#[test]
fn decodes_grid_line_and_flush_from_real_nvim_redraw() {
    let mut engine = Engine::spawn(EngineConfig::default()).unwrap();
    let (tx, rx) = mpsc::sync_channel(64);
    let pump = engine.start_pump(tx);
    engine.handle.ui_attach(80, 24).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_grid_line = false;
    let mut saw_flush = false;
    while Instant::now() < deadline && !(saw_grid_line && saw_flush) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(Msg::RedrawReady) = rx.recv_timeout(remaining) else {
            break;
        };
        for event in pump.take_damage() {
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
    let (tx, rx) = mpsc::sync_channel(64);
    let pump = engine.start_pump(tx);
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
        let Ok(Msg::RedrawReady) = rx.recv_timeout(remaining) else {
            break;
        };
        for event in pump.take_damage() {
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
/// is the generative half; this is the real-captured-traffic half). Unlike
/// `ui_events.rs`'s decode fixtures, which are hand-copied from a saved
/// capture log, this test captures its own storm at run time: it spawns a
/// real nvim, opens a generated multi-thousand-line file (a real `grid_line`
/// burst with long runs, not the generator's synthetic ones) and pages
/// through it (real `grid_scroll` traffic, plus whatever
/// `win_viewport`/other `Unknown`-decoded noise nvim actually emits
/// alongside it), then proves the damage compactor against that exact
/// traffic.
///
/// Correctness is checked against nvim's own ground truth (its window's
/// top-of-screen buffer line, read back via `line('w0')`/`getline`) rather
/// than against a second, uncompacted copy of the same stream: `Engine`
/// routes every connection through its damage pump exclusively (no dual
/// raw+compacted path survives on a live connection, see this file's module
/// docs), so there is no independent "raw" observation of the same traffic
/// left to diff against. Comparing to nvim's actual displayed content is a
/// strictly external check rather than a self-consistency check between two
/// decode paths that could both be wrong the same way.
#[test]
fn compacted_damage_matches_nvim_ground_truth_across_a_real_edit_and_scroll_storm() {
    // `--clean` isolates this test from the host's real nvim config
    // (plugins, statuslines, autocmds): the ground-truth comparison below
    // assumes row 0 of the grid is the window's first buffer line with no
    // winbar/tabline chrome above it, which a plugin-loaded config is not
    // guaranteed to preserve.
    let cfg = EngineConfig {
        extra_args: vec!["--clean".into()],
        ..EngineConfig::default()
    };
    let mut engine = Engine::spawn(cfg).unwrap();
    let (tx, rx) = mpsc::sync_channel(64);
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

    // drain compacted damage into a running model until the storm settles
    // (a full budget elapses with no further RedrawReady token), same
    // production pattern the runtime loop uses
    let mut model = Model::new();
    let mut compacted: Vec<UiEvent> = Vec::new();
    let overall_deadline = Instant::now() + Duration::from_secs(10);
    let settle = Duration::from_millis(500);
    loop {
        if Instant::now() >= overall_deadline {
            break;
        }
        match rx.recv_timeout(settle) {
            Ok(Msg::RedrawReady) => {
                let batch = pump.take_damage();
                compacted.extend(batch.iter().cloned());
                let _ = update(&mut model, Msg::Redraw(batch));
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => break, // quiet: storm settled
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !compacted.is_empty(),
        "captured no compacted damage from the edit+scroll storm"
    );
    assert!(
        compacted
            .iter()
            .any(|e| matches!(e, UiEvent::GridScroll { .. })),
        "expected at least one real GridScroll from the paced <C-e> scrolling"
    );

    let top_line = engine
        .handle
        .request(
            "nvim_call_function",
            vec![
                rmpv::Value::from("line"),
                rmpv::Value::Array(vec![rmpv::Value::from("w0")]),
            ],
        )
        .unwrap();
    let expected = engine
        .handle
        .request(
            "nvim_call_function",
            vec![
                rmpv::Value::from("getline"),
                rmpv::Value::Array(vec![top_line]),
            ],
        )
        .unwrap();
    let expected_text = expected
        .as_str()
        .expect("getline must return a String result");

    assert_eq!(
        model.engine.grid.row_text(0).trim_end(),
        expected_text,
        "compacted damage's final grid row 0 does not match nvim's own \
         reported top-of-window buffer line"
    );
}
