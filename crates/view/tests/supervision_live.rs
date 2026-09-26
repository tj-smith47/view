//! Live-nvim wire facts behind the busy modal's `Interrupt` choice.
//!
//! The choice sends one keystroke and claims it reaches an engine that has
//! stopped answering. Whether it does is a property of the pinned engine and
//! not of view: nvim notices an interrupt only where its own break check
//! runs, and the two shapes of synchronous work an engine can be stuck in
//! differ on exactly that point. Nothing headless can answer it, and
//! `INTERRUPT_NOTATION`'s doc comment states the answer as fact, so the
//! answer is pinned here against a real engine rather than assumed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use view_core::model::Model;
use view_core::msg::{Effect, Key, Msg, RpcCall};
use view_core::native::supervision::{
    SinceStamp, WedgeKind, ENGINE_BUSY_MODAL_THRESHOLD, INTERRUPT_NOTATION,
};
use view_core::update::update;
use view_engine::{wedge_kind, OutboxStallWatch};
use view_oracle::hang::detection_deadline;

/// How long the engine is told to stay busy for. Far longer than any budget
/// below, so a probe that answers cannot be the busy work finishing on its
/// own. Never actually waited out: the engine is killed with the session.
///
/// A real bound on both loops, not a number in a comment: each one reads a
/// clock its own iterations advance ([`vimscript_loop`], [`lua_loop`]), so a
/// child that outlives the run it belongs to still ends on its own.
const BUSY_SECS: u64 = 60;

/// How long an answer that proves the interrupt landed is waited for on an
/// idle host. An interrupted engine answers in about a millisecond; this is
/// slack, and `view_test_support::host_deadline` widens it with the load at
/// the assertion -- at the 3x ceiling still an order of magnitude short of
/// `BUSY_SECS`, which is what keeps the bound a discriminator.
const INTERRUPTED: Duration = Duration::from_secs(5);

/// How long past the detection bound the idle wait is left running before a
/// message is put on its channel to end it.
///
/// Not slack on the bound and never a wakeup the loop may rely on: it exists
/// so a wait that the watch failed to bound ends with a verdict about that
/// failure instead of hanging the test binary. Wide enough that a host slow
/// enough to need it has already failed the assertion it would otherwise
/// hide.
const WATCHDOG_MARGIN: Duration = Duration::from_secs(5);

/// How long the engine is given to get inside the loop it was told to run.
/// A budget, not a wait: the wait below ends the moment the engine actually
/// stops answering, so a fast host spends milliseconds here and a loaded one
/// spends what it needs.
const ENTERS_LOOP: Duration = Duration::from_secs(30);

/// A live engine with a UI attached and nothing in its config, put to work on
/// `busy` and blocked before this returns.
///
/// The readiness condition is the engine's own silence, never a sleep: a
/// fixed pause races the engine on any loaded host, and losing that race
/// makes every assertion below vacuous in the passing direction -- an engine
/// still reading the command line answers nothing either.
fn busy_engine(label: &str, busy: &str) -> view_engine::process::Engine {
    let dir = common::fixture(&format!("supervision-live-{label}"), "");
    let engine = common::spawn_with_drained_pump(common::isolated_reading(&dir.join("init.lua")));
    put_to_work(label, &engine, busy);
    engine
}

/// [`busy_engine`] with the pump's channel handed to the caller instead of
/// to a background drain, plus a sender for putting a message of its own on
/// it. The wait a wedge is timed against is a wait on this channel, so a
/// caller proving something about that wait needs the channel itself rather
/// than only the engine behind it.
fn busy_session(
    label: &str,
    busy: &str,
) -> (
    view_engine::process::Engine,
    std::sync::mpsc::SyncSender<Msg>,
    std::sync::mpsc::Receiver<Msg>,
) {
    let dir = common::fixture(&format!("supervision-live-{label}"), "");
    let (engine, _pump, tx, rx) =
        common::spawn_with_wired_pump(common::isolated_reading(&dir.join("init.lua")), 64);
    put_to_work(label, &engine, busy);
    (engine, tx, rx)
}

/// Attaches a UI, types `busy`, and returns once the engine has stopped
/// answering -- the readiness condition both spawns above share.
fn put_to_work(label: &str, engine: &view_engine::process::Engine, busy: &str) {
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    // typed rather than requested: `nvim_command` is a blocking request, and
    // a caller waiting on the reply to work that never returns could not go
    // on to interrupt it
    engine.handle.input(busy).unwrap();

    let deadline = Instant::now() + view_test_support::host_deadline(ENTERS_LOOP);
    // an engine that has not yet entered the loop answers this promptly;
    // one that has answers it not at all, which is the whole signal
    while engine.handle.get_mode().is_ok() {
        assert!(
            Instant::now() < deadline,
            "{label}: the engine kept answering for {ENTERS_LOOP:?} after being              told to run a {BUSY_SECS}s loop, so it never entered it"
        );
    }
}

/// A Vimscript `while`, whose break check pumps the event loop and so sees
/// input that arrived over RPC.
fn vimscript_loop() -> String {
    format!(
        ":let g:t=reltime() | while {BUSY_SECS} - reltimefloat(reltime(g:t)) > 0 | endwhile<CR>"
    )
}

/// A Lua `while`, which pumps nothing.
///
/// Timed on `vim.uv.hrtime`, never `vim.uv.now`: the latter reads libuv's
/// loop-cached time, which only `uv_update_time` moves and which no
/// iteration of a loop holding the main thread ever runs. Measured against
/// the pinned engine, `vim.uv.now()` returned the same millisecond across a
/// spin of tens of millions of iterations, so a bound written against it
/// never expires and the engine spins until something kills it -- which is
/// how two of these ended up reparented to init with a core pinned for days.
fn lua_loop() -> String {
    format!(
        ":lua local t=vim.uv.hrtime() while {} - (vim.uv.hrtime()-t) > 0 do end<CR>",
        BUSY_SECS * 1_000_000_000
    )
}

/// The interrupt reaching the wedge class it can reach, with a control that
/// rules out the loop simply having finished: the identical session left
/// uninterrupted still owes its answer when the budget runs out.
#[test]
fn the_interrupt_notation_aborts_an_engine_stuck_in_a_vimscript_loop() {
    let interrupted = busy_engine("vim-interrupted", &vimscript_loop());
    interrupted.handle.input(INTERRUPT_NOTATION).unwrap();
    let start = Instant::now();
    let answered = interrupted.handle.eval_str("1");
    assert!(
        answered.is_ok(),
        "the interrupt did not abort a Vimscript loop: {answered:?}"
    );
    assert!(
        start.elapsed() < view_test_support::host_deadline(INTERRUPTED),
        "the engine answered only after {:?}, which is not an abort",
        start.elapsed()
    );

    let control = busy_engine("vim-control", &vimscript_loop());
    let answered = control.handle.eval_str("1");
    assert!(
        answered.is_err(),
        "the same loop answered with no interrupt sent, so the test above proves \
         nothing about the interrupt: {answered:?}"
    );
}

/// The wedge class the interrupt cannot reach, and the reason the modal
/// offers `Restart` beside it rather than presenting an interrupt as the
/// answer. The liveness probe goes unanswered here too, which is what makes
/// this shape detectable at all.
#[test]
fn a_synchronous_lua_loop_answers_neither_the_liveness_probe_nor_the_interrupt() {
    let engine = busy_engine("lua-interrupted", &lua_loop());

    let probed = engine.handle.get_mode();
    assert!(
        probed.is_err(),
        "the liveness probe answered during a synchronous Lua loop, so this wedge \
         would never be detected: {probed:?}"
    );

    engine.handle.input(INTERRUPT_NOTATION).unwrap();
    let answered = engine.handle.eval_str("1");
    assert!(
        answered.is_err(),
        "the interrupt reached a synchronous Lua loop, so INTERRUPT_NOTATION's doc \
         comment understates what it can do: {answered:?}"
    );
}

/// The wedge that opens while nobody is at the keyboard, timed end to end
/// against a live engine.
///
/// Detection here is the loop's own doing and nothing else's: a wedged
/// engine answers no probe and emits no redraw, so from the moment the
/// engine goes quiet the only thing that can wake this wait is a wakeup the
/// read-side watch asked for. Nothing is typed, nothing is sent, and the
/// only other message that can arrive is the watchdog's -- which exists to
/// end a wait that would otherwise run forever, and whose arrival is itself
/// the failure being guarded against, since being woken by a message is
/// exactly the keystroke a user should not have to type.
///
/// The shape is the runtime loop's own: read both sides of the connection,
/// fold them into a verdict, then sleep for exactly as long as the watches
/// allow. A loop that bounded its sleep only while a probe happened to be
/// outstanding at the moment it went to sleep would sleep through this
/// entire window.
#[test]
fn a_wedge_that_opens_while_the_session_is_idle_still_raises_the_notice() {
    let (mut engine, watchdog, rx) = busy_session("lua-idle-wedge", &lua_loop());
    let bound = detection_deadline();

    // the state a healthy idle session sits in: every probe issued has been
    // answered, and the next one has not gone out yet. Arming here forgives
    // the probes the readiness wait above already left outstanding, so the
    // window below opens where a user's own idle session opens it rather
    // than partway into a silence that had already started.
    engine.heartbeat.resume();
    while rx.try_recv().is_ok() {}
    let mut write = OutboxStallWatch::default();
    let quiet_since = Instant::now();
    assert_eq!(
        wedge_kind(
            write.observe(&engine.handle),
            engine.heartbeat.observe(engine.handle.is_closed()),
            false
        ),
        None,
        "the verdict was already reached before any waiting happened, so nothing \
         below is evidence about the wait"
    );

    std::thread::spawn(move || {
        std::thread::sleep(bound + WATCHDOG_MARGIN);
        let _ = watchdog.send(Msg::RedrawReady);
    });

    let noticed = loop {
        let stalled = write.observe(&engine.handle);
        assert!(
            !stalled,
            "the write side stalled, so the verdict below would name it rather than \
             the read side this is timing"
        );
        let verdict = wedge_kind(
            stalled,
            engine.heartbeat.observe(engine.handle.is_closed()),
            false,
        );
        if let Some(kind) = verdict {
            break Some((kind, quiet_since.elapsed()));
        }
        if quiet_since.elapsed() >= bound + WATCHDOG_MARGIN {
            break None;
        }
        assert_eq!(
            write.poll_deadline(),
            None,
            "the write side armed the wakeup, so this proves nothing about the read side"
        );
        let received = match engine.heartbeat.poll_deadline() {
            Some(deadline) => rx.recv_timeout(deadline).ok(),
            // what the runtime loop does with "wait as long as you like":
            // there is no timer of its own anywhere in this loop
            None => rx.recv().ok(),
        };
        if let Some(Msg::HeartbeatReply { generation }) = received {
            engine.heartbeat.record_ack(generation);
        }
    };

    let Some((kind, after)) = noticed else {
        panic!(
            "the wedge was never noticed at all, {:?} after the engine went quiet",
            quiet_since.elapsed()
        );
    };
    assert_eq!(
        kind,
        WedgeKind::ReadSide,
        "a live engine spinning in synchronous Lua was classified as something else"
    );
    assert!(
        after <= bound,
        "the wedge was noticed only after {after:?}, past the {bound:?} detection \
         bound: with nothing sent and nothing arriving, the wait ran on past the \
         deadline the watch owed it and ended at the watchdog instead"
    );

    // and the verdict the wait reached is the one the user is shown
    let mut model = Model::with_term_size(80, 24);
    let _ = update(
        &mut model,
        Msg::EngineLiveness {
            wedge: Some(kind),
            observed_for: after,
        },
    );
    assert_eq!(
        notices(&model),
        WedgeKind::ReadSide.banner(SinceStamp::new(after)).to_vec(),
        "the verdict never reached the notice a waiting user reads"
    );
}

/// The modal is raised by view noticing something, not by the user asking
/// for it, and the thing it notices is very often a long operation that
/// finishes. Anything typed while it is up must land in the buffer exactly
/// as it would have with no modal on screen -- proved here against a real
/// engine and a real buffer rather than against the effect alone, since an
/// effect that never reaches nvim is the failure this is guarding.
///
/// `i` leads the sequence deliberately: it is the reflex keystroke of a
/// user waiting out a slow operation, and a modal that bound it would
/// answer that reflex by aborting the operation being waited for.
#[test]
fn every_key_typed_at_the_modal_still_lands_in_the_buffer() {
    let dir = common::fixture("supervision-live-passthrough", "");
    let engine = common::spawn_with_drained_pump(common::isolated_reading(&dir.join("init.lua")));
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();

    let mut model = Model::with_term_size(80, 24);
    let _ = update(
        &mut model,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: ENGINE_BUSY_MODAL_THRESHOLD,
        },
    );
    assert!(
        !model.overlays().is_empty(),
        "a wedge past the threshold must have opened the modal"
    );

    // typed with the modal up, one keypress at a time, exactly as the input
    // reader delivers them
    for notation in ["i", "x", "y", "z"] {
        let effects = update(
            &mut model,
            Msg::Key(Key {
                notation: notation.to_string(),
            }),
        );
        let [Effect::Rpc(RpcCall::Input { notation: sent })] = &effects[..] else {
            panic!("{notation:?} was swallowed by the modal: {effects:?}");
        };
        assert_eq!(sent, notation);
        engine.handle.input(sent).unwrap();
    }

    // asserted here rather than only at the end: this blocking round-trip is
    // also the barrier the interrupt below needs, since `nvim_input` queues
    // and an interrupt flushes whatever typeahead is still unread
    assert_eq!(
        engine.handle.eval_str("getline(1)").unwrap(),
        "xyz",
        "keystrokes typed at the modal never reached the buffer"
    );

    // the interrupt choice, picked by the very key it sends: the wire input
    // is what the engine would have received with no modal on screen, and
    // the modal stays up because the interrupt may not have landed
    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: INTERRUPT_NOTATION.to_string(),
        }),
    );
    let [Effect::Rpc(RpcCall::Input { notation: sent })] = &effects[..] else {
        panic!("the interrupt choice sent something else: {effects:?}");
    };
    assert_eq!(sent, INTERRUPT_NOTATION);
    engine.handle.input(sent).unwrap();
    assert!(
        !model.overlays().is_empty(),
        "an interrupt that may not land must leave the modal up"
    );

    // <Esc> is the modal's own Dismiss key, and it both closes the modal and
    // goes on to nvim: answering an annunciator the user never asked for may
    // not cost them a keystroke, so this <Esc> is also the one that leaves
    // insert mode
    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: "<Esc>".into(),
        }),
    );
    let [Effect::Rpc(RpcCall::Input { notation: sent })] = &effects[..] else {
        panic!("Dismiss must still deliver its <Esc> to the engine: {effects:?}");
    };
    engine.handle.input(sent).unwrap();
    assert!(model.overlays().is_empty(), "Dismiss must close the modal");

    // and with it gone the identical key routes to the engine the same way,
    // which is the whole claim: the modal changed what was on screen and
    // nothing about what a key does
    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: "<Esc>".into(),
        }),
    );
    let [Effect::Rpc(RpcCall::Input { notation: sent })] = &effects[..] else {
        panic!("a key with no overlay open must reach the engine: {effects:?}");
    };
    engine.handle.input(sent).unwrap();

    assert_eq!(
        engine.handle.eval_str("getline(1)").unwrap(),
        "xyz",
        "answering the modal cost the buffer the keystrokes it was typed"
    );
    assert_eq!(
        engine.handle.eval_str("mode()").unwrap(),
        "n",
        "the dismissal's <Esc> never reached the engine, so insert mode survived it"
    );
}

/// How long a live engine is given to flush a swap, repaint, or report its
/// buffer list, before the host's load widens it.
const SETTLES: Duration = Duration::from_secs(10);

/// A spawn that leaves swap files under `dir` for a replacement to recover:
/// `EngineConfig::isolated` passes `-n`, which writes none.
fn recoverable(dir: &std::path::Path) -> view_engine::process::EngineConfig {
    view_engine::process::EngineConfig::default()
        .with_arg("--clean")
        .with_arg("--cmd")
        .with_arg(format!(
            "lua vim.o.directory = [[{}//]] vim.o.updatetime = 100",
            dir.join("swap").display()
        ))
        .with_env("HOME", dir)
        .with_env("XDG_CONFIG_HOME", dir.join("config"))
        .with_env("XDG_DATA_HOME", dir.join("data"))
        .with_env("XDG_STATE_HOME", dir.join("state"))
        .with_env_remove("VIMINIT")
        .with_env_remove("XDG_CONFIG_DIRS")
        // the engine being replaced is stopped, so its `qa!` goes unread and
        // only the force-kill after this ends it
        .with_shutdown_timeout(Duration::from_secs(1))
}

/// What the engine shows on its first screen row.
fn first_row(engine: &view_engine::process::Engine) -> String {
    engine.handle.command("redraw").unwrap();
    engine
        .handle
        .eval_str("join(map(range(1, &columns), 'screenstring(1, v:val)'), '')")
        .unwrap()
        .trim_end()
        .to_string()
}

/// Waits until `engine`'s first screen row reads `want`, and returns what it
/// read last.
fn first_row_reading(engine: &view_engine::process::Engine, want: &str) -> String {
    let deadline = Instant::now() + view_test_support::host_deadline(SETTLES);
    loop {
        let row = first_row(engine);
        if row == want || Instant::now() >= deadline {
            return row;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Every swap file under `dir` together, as bytes.
fn swapped(dir: &std::path::Path) -> Vec<u8> {
    std::fs::read_dir(dir.join("swap"))
        .map(|entries| {
            entries
                .flatten()
                .flat_map(|entry| std::fs::read(entry.path()).unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}

fn holds(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// A restart as the runtime performs one, off a session whose two listed
/// files `a.txt` and `b.txt` each hold one unsaved line, the second current.
///
/// `configure` is applied to the spawn and to its replacement alike, and
/// `swapped` says whether the session writes swap files to wait on. The
/// model has folded the replacement's `EngineAttached`, so the swap probe
/// generation it answers is the one the model is waiting on.
struct Restarted {
    dir: view_test_support::ScratchDir,
    model: Model,
    replacement: view_engine::process::Engine,
    drained: std::sync::mpsc::Receiver<Msg>,
    generation: u64,
    _pump: view_engine::DamagePump,
}

fn restart_with_unsaved_work(
    label: &str,
    configure: fn(view_engine::process::EngineConfig) -> view_engine::process::EngineConfig,
    swapped_to_disk: bool,
) -> Restarted {
    let dir = view_test_support::ScratchDir::resolved(label).unwrap();
    std::fs::create_dir_all(dir.join("swap")).unwrap();
    let a = dir.join("a.txt");
    let b = dir.join("b.txt");
    std::fs::write(&a, "a on disk\n").unwrap();
    std::fs::write(&b, "b on disk\n").unwrap();

    let (engine, _pump, _tx, rx) =
        common::spawn_with_wired_pump(configure(recoverable(&dir)).with_arg(&a).with_arg(&b), 256);
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    engine
        .handle
        .register_bridge(engine.api_info.channel_id)
        .unwrap();
    engine.handle.input("Oa unsaved<Esc>").unwrap();
    engine.handle.input(":hide bnext<CR>").unwrap();
    engine.handle.input("Ob unsaved<Esc>").unwrap();

    // the list the restart reads is the one the bridge reported, once it
    // reports both files modified with the second one current
    let mut model = Model::with_term_size(80, 24);
    let b_name = b.to_string_lossy().into_owned();
    let listed = common::drain_until(
        &rx,
        view_test_support::host_deadline(SETTLES),
        |msg| match msg {
            Msg::BufferList { buffers }
                if buffers.len() == 2
                    && buffers.iter().all(|entry| entry.modified)
                    && buffers
                        .iter()
                        .any(|entry| entry.current && entry.path == b_name) =>
            {
                Some(msg.clone())
            }
            _ => None,
        },
    )
    .expect("the bridge never reported both files modified with the second one current");
    let _ = update(&mut model, listed);
    std::thread::spawn(move || while rx.recv().is_ok() {});

    let deadline = Instant::now() + view_test_support::host_deadline(SETTLES);
    while swapped_to_disk
        && !(holds(&swapped(&dir), "a unsaved") && holds(&swapped(&dir), "b unsaved"))
    {
        assert!(
            Instant::now() < deadline,
            "the swaps never held both unsaved lines, so there is nothing to recover"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let stopped = std::process::Command::new("kill")
        .args(["-STOP", &engine.pid().to_string()])
        .status()
        .unwrap();
    assert!(stopped.success(), "the engine could not be stopped");

    let _ = update(
        &mut model,
        Msg::EngineLiveness {
            wedge: Some(WedgeKind::ReadSide),
            observed_for: Duration::from_secs(12),
        },
    );
    assert!(
        model.overlays().is_empty(),
        "the modal opened below its threshold"
    );
    let effects = update(
        &mut model,
        Msg::Key(Key {
            notation: "<F5>".into(),
        }),
    );
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::RestartEngine)),
        "<F5> at the banner asked for no restart: {effects:?}"
    );

    let reopen = view_core::model::reopen_order(&model.buffers);
    assert_eq!(reopen, vec![a.to_string_lossy().into_owned(), b_name]);
    model
        .supervision
        .note_restart_unsaved(view_core::model::unsaved_files(&model.buffers));
    let mut replacement = engine
        .restart(configure(recoverable(&dir)).with_arg(&a).reopening(&reopen))
        .unwrap();
    let (sink, drained) = std::sync::mpsc::sync_channel(64);
    let (pump, _cutover) = replacement.start_pump(sink);
    replacement
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    let generation = update(&mut model, Msg::EngineAttached)
        .iter()
        .find_map(|effect| match effect {
            Effect::Rpc(RpcCall::ProbeSwapRecovery { generation }) => Some(*generation),
            _ => None,
        })
        .expect("the replacement's attach armed no swap probe");
    Restarted {
        dir,
        model,
        replacement,
        drained,
        generation,
        _pump: pump,
    }
}

/// Asks the replacement what its start recovered, once its first screen row
/// reads `settled`, and folds the answer into the model.
fn fold_recovery_reading(restarted: &mut Restarted, settled: &str) {
    assert_eq!(first_row_reading(&restarted.replacement, settled), settled);
    restarted
        .replacement
        .handle
        .probe_swap_recovery(restarted.generation)
        .unwrap();
    let reading = common::drain_until(
        &restarted.drained,
        view_test_support::host_deadline(SETTLES),
        |msg| matches!(msg, Msg::SwapRecovered { .. }).then(|| msg.clone()),
    )
    .expect("the replacement never answered the swap probe");
    let _ = update(&mut restarted.model, reading);
}

/// The lines the model's notices show, as a user reads them.
fn notices(model: &Model) -> Vec<String> {
    model
        .engine
        .messages
        .visible_lines(40)
        .into_iter()
        .map(|spans| spans.into_iter().map(|span| span.text).collect::<String>())
        .collect()
}

/// `<F5>` at a wedge the modal has not opened for replaces a stopped engine
/// with one that reopens both listed files, the current one on screen, each
/// with the text only its swap held, and says which ones it recovered.
#[cfg(unix)]
#[test]
fn a_restart_reopens_every_listed_buffer_and_recovers_its_unsaved_text() {
    let mut restarted = restart_with_unsaved_work("supervision-live-reopen", |config| config, true);
    fold_recovery_reading(&mut restarted, "b unsaved");
    assert!(
        notices(&restarted.model)
            .contains(&"view: unsaved changes recovered for a.txt, b.txt".to_string()),
        "the recovery did not name both buffers: {:?}",
        notices(&restarted.model)
    );
    let replacement = &restarted.replacement;
    replacement.handle.command("hide bprevious").unwrap();
    assert_eq!(
        first_row_reading(replacement, "a unsaved"),
        "a unsaved",
        "the other listed file came back without its unsaved line"
    );
    let dir = &restarted.dir;
    assert_eq!(
        std::fs::read_to_string(dir.join("a.txt")).unwrap(),
        "a on disk\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("b.txt")).unwrap(),
        "b on disk\n"
    );
}

/// A launch already carrying the ten `-c` commands nvim accepts restarts
/// with both files recovered: the reopen spends no command slot.
#[cfg(unix)]
#[test]
fn a_launch_with_every_command_slot_taken_still_restarts_with_its_work() {
    let mut restarted = restart_with_unsaved_work(
        "supervision-live-ten-c",
        |config| {
            (0..10).fold(config, |config, slot| {
                config
                    .with_arg("-c")
                    .with_arg(format!("let g:slot{slot} = 1"))
            })
        },
        true,
    );
    fold_recovery_reading(&mut restarted, "b unsaved");
    let replacement = &restarted.replacement;
    assert_eq!(
        replacement.handle.eval_str("string(g:slot9)").unwrap(),
        "1",
        "the tenth -c of the launch never ran on the replacement"
    );
    replacement.handle.command("hide bprevious").unwrap();
    assert_eq!(
        first_row_reading(replacement, "a unsaved"),
        "a unsaved",
        "the other listed file came back without its unsaved line"
    );
}

/// A session run with `noswapfile` has nothing to recover from, and the
/// restart names the buffer whose unsaved changes it lost and the option
/// that kept them out of a swap file.
#[cfg(unix)]
#[test]
fn a_restart_without_swap_files_names_the_buffers_it_reopened_from_disk() {
    let mut restarted = restart_with_unsaved_work(
        "supervision-live-noswap",
        |config| config.with_arg("--cmd").with_arg("set noswapfile"),
        false,
    );
    fold_recovery_reading(&mut restarted, "b on disk");
    let lines = notices(&restarted.model);
    assert!(
        lines.ends_with(&[
            "view: a.txt, b.txt reopened from disk; their unsaved changes had no \
             swap files (swapfile is off)"
                .to_string(),
            "set swapfile gives them back.".to_string(),
        ]),
        "the restart did not name the buffers it lost: {lines:?}"
    );
}
