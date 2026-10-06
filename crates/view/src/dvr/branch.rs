//! Replaces the editor with one brought to a recorded frame.

use std::path::PathBuf;

use view_core::model::Model;
use view_core::msg::Msg;
use view_core::native::dvr::BranchPlan;
use view_engine::{Engine, EngineConfig};

use crate::native::NativeSession;
use crate::recovery::{replace_engine, Bound, LoopChannels, Restarted};
use crate::startup::AttachFailure;

/// Stops `engine` and replaces it with one opening the launch's files,
/// staging the recorded input `plan` replays. Once the replacement starts,
/// the input log ends at the branch point and the frames after it can no
/// longer be branched from. A replacement that fails leaves the recording
/// as it was.
///
/// # Latency
///
/// The bound [`crate::recovery::restart_engine`] states: the stop waits up
/// to the engine's `shutdown_timeout` and the spawn up to its
/// `handshake_timeout`, on a frame the person asked to stall by confirming.
/// Ahead of the stop, the swap writes (`preserve`) and the list of changed
/// buffers' swaps are two requests bounded at 5 s each. An engine too busy
/// to answer them stalls the branch up to their joint 10 s bound. The
/// replay folds once per recorded input, once the replacement's takeover
/// has claimed view's keys ([`due_replay`]).
pub(crate) fn replace(
    engine: &mut Engine,
    respawn: &dyn Fn(&[String]) -> EngineConfig,
    model: &mut Model,
    channels: &LoopChannels,
    bound: Bound<'_>,
    plan: BranchPlan,
) -> Result<Restarted, AttachFailure> {
    for effect in view_core::update::prepare_branch(model) {
        // a close owes only local work, which answers no flow of its own
        let _ = bound.2.run(effect);
    }
    // the abandoned timeline's swap files go with it: `qa!` deletes them,
    // and the replacement would otherwise offer to recover each one. A copy
    // waits for the replacement, so the restart after a failed one offers
    // the unsaved text back
    let kept = keep_swaps(engine);
    let _ = engine.wait_exit();
    let fresh = replace_engine(engine, respawn, model, channels, bound, &[]);
    if fresh.is_ok() {
        model.dvr.branched(plan.at_frame, plan.replay);
        for (_, copy) in kept.copied {
            let _ = std::fs::remove_file(copy);
        }
    } else {
        model.dvr.branch_failed();
        for (swap, copy) in kept.copied {
            let _ = std::fs::rename(copy, swap);
        }
        for swap in kept.failed {
            let text = format!(
                "view: DVR could not copy the swap file {}, so its unsaved text is lost",
                swap.display()
            );
            for effect in model.engine.record_native_notice(text, false) {
                let _ = bound.2.run(effect);
            }
        }
    }
    fresh
}

/// The swap file of every loaded buffer with unsaved changes, as an
/// absolute path per line.
const CHANGED_SWAPS: &str = "join(map(filter(getbufinfo({'bufloaded': 1}), \
    'v:val.changed && swapname(v:val.bufnr) != \"\"'), 'fnamemodify(swapname(v:val.bufnr), \":p\")'), \"\\n\")";

/// Every directory of `'directory'` a swap file can be found in by name,
/// as an absolute path per line. An entry starting with `.` names each
/// file's own directory, which holds no copy a sweep could list.
const SWAP_DIRS: &str = r#"join(map(filter(split(&directory, ','), {_, d -> d[0] != '.'}), {_, d -> fnamemodify(expand(substitute(d, '/\+$', '', '')), ':p')}), "\n")"#;

/// What a branch kept of the unsaved text: each swap with its copy, and
/// each swap that could not be copied.
#[derive(Default)]
struct KeptSwaps {
    copied: Vec<(PathBuf, PathBuf)>,
    failed: Vec<PathBuf>,
}

/// The name of the copy a branch keeps of `swap`.
fn copy_of(swap: &std::path::Path) -> PathBuf {
    let mut copy = swap.as_os_str().to_owned();
    copy.push(COPY_SUFFIX);
    PathBuf::from(copy)
}

/// The suffix a swap copy carries, which nvim lists as no swap of its own.
const COPY_SUFFIX: &str = ".view-branch";

/// Writes the swap file of every buffer with unsaved changes and copies
/// each beside itself. A remote engine's swaps are on its own host, out of
/// reach, so none is kept.
fn keep_swaps(engine: &Engine) -> KeptSwaps {
    let mut kept = KeptSwaps::default();
    if engine.is_remote() || engine.handle.command("silent! preserve").is_err() {
        return kept;
    }
    let listed = engine.handle.eval_str(CHANGED_SWAPS).unwrap_or_default();
    for swap in listed
        .lines()
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
    {
        let copy = copy_of(&swap);
        match std::fs::copy(&swap, &copy) {
            Ok(_) => kept.copied.push((swap, copy)),
            Err(_) => kept.failed.push(swap),
        }
    }
    kept
}

/// Puts back every swap copy a branch left in `engine`'s swap directories
/// when view stopped between the copy and its removal. A copy whose swap
/// is gone is renamed back to it, so the engine offers the unsaved text
/// again; one whose swap still stands is removed. Run before the engine
/// opens any file, so it reads `'directory'` as it stands ahead of the
/// config.
pub(crate) fn restore_swap_copies(engine: &Engine) {
    if engine.is_remote() {
        return;
    }
    let dirs = engine.handle.eval_str(SWAP_DIRS).unwrap_or_default();
    for dir in dirs.lines().filter(|line| !line.is_empty()) {
        restore_in(std::path::Path::new(dir));
    }
}

/// [`restore_swap_copies`] for one directory.
fn restore_in(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for copy in entries.flatten().map(|entry| entry.path()) {
        let Some(swap) = copy
            .to_str()
            .and_then(|path| path.strip_suffix(COPY_SUFFIX))
            .map(PathBuf::from)
        else {
            continue;
        };
        if swap.exists() {
            let _ = std::fs::remove_file(&copy);
        } else {
            let _ = std::fs::rename(&copy, &swap);
        }
    }
}

/// The messages a branch's replay folds now: none while the replacement's
/// takeover holds input, then [`view_core::update::due_replay`]'s. A key
/// folded before view has claimed its mappings is read as nvim's own, so
/// a picker query replayed behind view's key would reach the buffer.
pub(crate) fn due_replay(model: &mut Model, native: &NativeSession) -> Vec<Msg> {
    if !model.dvr.has_replay() || native.holds_input() {
        return Vec::new();
    }
    view_core::update::due_replay(model)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::mpsc;

    use view_core::model::{Focus, OverlayKind};
    use view_core::msg::{Key, Msg};
    use view_core::native::dvr::DvrRequest;
    use view_engine::DamagePump;

    use super::*;
    use crate::native::NativeSession;
    use crate::runtime::{dispatch, Executor, FollowUps};

    /// What the loop holds around one session: the channel every engine
    /// answers on, the routes a replacement is rebound to, and the native
    /// session whose takeover registers view's keys.
    struct Rig {
        rx: mpsc::Receiver<Msg>,
        channels: LoopChannels,
        native: NativeSession,
        theme: crate::bridge::ThemeBridge,
        model: Model,
        frames: u64,
    }

    impl Rig {
        fn new() -> Self {
            let (tx, rx) = mpsc::sync_channel(4096);
            let msg = crate::wake::LoopSender::new(tx);
            let channels = LoopChannels {
                clipboard: mpsc::channel().0,
                osc52: mpsc::channel().0,
                picker: mpsc::channel().0,
                // a program that does not exist, so nothing a test types
                // can start an agent
                ai: crate::ai_worker::AiWorker::new(
                    view_ai::AgentSpec::Command(vec!["view-dvr-test-no-agent".to_string()]),
                    std::path::PathBuf::from("."),
                    msg.clone(),
                ),
                ai_context: mpsc::channel().0,
                msg,
            };
            let mut model = Model::with_term_size(80, 24);
            model.attach_surfaces(view_core::native::ext::shipped_multigrid());
            model.dvr.enable_at(1 << 20, (80, 24));
            Self {
                rx,
                channels,
                native: NativeSession::all_enabled(0, None),
                theme: crate::bridge::ThemeBridge::new(None, None),
                model,
                frames: 0,
            }
        }

        fn dispatch(&mut self, executor: &Executor<view_engine::EngineHandle>, msg: Msg) {
            let mut follow_ups = FollowUps {
                native: &mut self.native,
                theme: &mut self.theme,
                speculate: crate::speculate::SpeculationClock::default(),
            };
            let _ = dispatch(&mut self.model, executor, &mut follow_ups, msg);
        }

        /// Folds what the engine sends, and a staged replay once the
        /// engine has started, as the loop does, until `done` holds.
        fn settle(
            &mut self,
            engine: &Engine,
            pump: &DamagePump,
            executor: &Executor<view_engine::EngineHandle>,
            what: &str,
            mut done: impl FnMut(&mut Self, &Engine) -> bool,
        ) {
            let deadline = std::time::Instant::now()
                + view_test_support::host_deadline(std::time::Duration::from_secs(20));
            loop {
                while let Ok(msg) = self.rx.try_recv() {
                    let msg = match msg {
                        Msg::RedrawReady => Msg::Redraw(pump.take_damage()),
                        msg => msg,
                    };
                    self.dispatch(executor, msg);
                }
                for msg in due_replay(&mut self.model, &self.native) {
                    self.dispatch(executor, msg);
                }
                if done(self, engine) {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "timed out: {what}");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }

        /// Starts the first engine the way a launch does and waits until
        /// it takes keys.
        fn launch(
            &mut self,
            cfg: EngineConfig,
        ) -> (
            Engine,
            DamagePump,
            crate::clipboard::ReplyRoute<view_engine::EngineHandle>,
            crate::ai_context_worker::OpsRoute<view_engine::EngineHandle>,
            Executor<view_engine::EngineHandle>,
        ) {
            let mut engine = Engine::spawn(cfg.with_late_attach(80, 24)).unwrap();
            self.native.rebind(engine.api_info.channel_id);
            let (pump, cutover) = engine.start_pump(self.channels.msg.clone());
            let route = crate::clipboard::ReplyRoute::new(engine.handle.clone());
            let ai_context_route = crate::ai_context_worker::OpsRoute::new(engine.handle.clone());
            let executor = self.channels.executor(engine.handle.clone(), route.epoch());
            for msg in cutover.presink {
                self.dispatch(&executor, msg);
            }
            self.settle(
                &engine,
                &pump,
                &executor,
                "the launch takes keys",
                |rig, e| !rig.model.awaits_attach() && !rig.native.holds_input() && answers(e),
            );
            (engine, pump, route, ai_context_route, executor)
        }

        /// Folds `keys` the way the terminal delivers them, one frame
        /// painted after each.
        fn type_keys(&mut self, executor: &Executor<view_engine::EngineHandle>, keys: &[&str]) {
            for key in keys {
                self.dispatch(
                    executor,
                    Msg::Key(Key {
                        notation: (*key).to_owned(),
                    }),
                );
                self.frames += 1;
                self.model.dvr.note_frame(self.frames, 1);
            }
        }

        /// Confirms a branch from frame `at` and carries it out as the
        /// loop does, returning the replacement once its replay has folded
        /// and `done` holds.
        fn branch(
            &mut self,
            engine: &mut Engine,
            respawn: &dyn Fn(&[String]) -> EngineConfig,
            bound: Bound<'_>,
            at: u64,
            done: impl FnMut(&mut Self, &Engine) -> bool,
        ) -> Restarted {
            let fresh = self
                .start_branch(engine, respawn, bound, at)
                .expect("the replacement starts");
            self.settle(
                &fresh.engine,
                &fresh.pump,
                &fresh.executor,
                "the branch replays",
                done,
            );
            fresh
        }

        /// Confirms a branch from frame `at` and replaces the engine as
        /// the loop does, returning before the replacement's `VimEnter`.
        fn start_branch(
            &mut self,
            engine: &mut Engine,
            respawn: &dyn Fn(&[String]) -> EngineConfig,
            bound: Bound<'_>,
            at: u64,
        ) -> Result<Restarted, AttachFailure> {
            let plan = self.confirm_branch(at);
            let fresh = replace(
                engine,
                respawn,
                &mut self.model,
                &self.channels,
                bound,
                plan,
            )?;
            Ok(self.cut_over(fresh))
        }

        /// Confirms a branch from frame `at` as a person does, returning
        /// the plan the confirmation queued.
        fn confirm_branch(&mut self, at: u64) -> BranchPlan {
            let scrub = Msg::FeatureInvoke {
                generation: None,
                feature: "dvr".to_owned(),
                verb: "scrub".to_owned(),
            };
            let key = |notation: &str| {
                Msg::Key(Key {
                    notation: notation.to_owned(),
                })
            };
            let checked = Msg::DvrIo(view_core::native::dvr::DvrIoReply::DiskChecked {
                changed: Vec::new(),
                unverifiable: false,
            });
            let _ = view_core::update::update(&mut self.model, scrub.clone());
            if self.model.dvr.scrub_frame().is_none() {
                // a scrub invoked with no keys behind it is one the last
                // branch's replay owes, which swallows it once
                let _ = view_core::update::update(&mut self.model, scrub);
            }
            self.model.dvr.show(at);
            let _ = view_core::update::update(&mut self.model, key("b"));
            let _ = view_core::update::update(&mut self.model, checked);
            self.model.note_frame_painted();
            let _ = view_core::update::update(&mut self.model, key("y"));
            std::iter::from_fn(|| self.model.dvr.take_request())
                .find_map(|r| match r {
                    DvrRequest::Branch(plan) => Some(plan),
                    _ => None,
                })
                .expect("a confirmed branch is queued")
        }

        /// Hands a replacement what its startup staged, as the loop does
        /// once an engine has been replaced.
        fn cut_over(&mut self, fresh: Restarted) -> Restarted {
            self.native.rebind(fresh.engine.api_info.channel_id);
            let mut follow_ups = FollowUps {
                native: &mut self.native,
                theme: &mut self.theme,
                speculate: crate::speculate::SpeculationClock::default(),
            };
            let outcome = crate::startup::run_cutover(
                &mut self.model,
                &fresh.executor,
                &mut follow_ups,
                fresh.staged,
                || view_core::msg::ExitInfo {
                    code: None,
                    by_signal: false,
                },
            );
            assert!(matches!(outcome, crate::startup::CutoverOutcome::Continue));
            Restarted {
                staged: crate::startup::CutoverInput {
                    presink: Vec::new(),
                    pending_redraw: Vec::new(),
                    resize: None,
                    keys: Vec::new(),
                },
                ..fresh
            }
        }
    }

    fn answers(engine: &Engine) -> bool {
        engine
            .handle
            .request_timeout(
                "nvim_get_mode",
                vec![],
                std::time::Duration::from_millis(200),
            )
            .is_ok()
    }

    fn line(engine: &Engine, n: u32) -> String {
        engine
            .handle
            .eval_str(&format!("getline({n})"))
            .unwrap_or_default()
    }

    fn eval(engine: &Engine, expr: &str) -> String {
        engine.handle.eval_str(expr).unwrap_or_default()
    }

    fn chars(text: &str) -> Vec<String> {
        text.chars().map(String::from).collect()
    }

    fn keys(spec: &[&str]) -> Vec<String> {
        spec.iter().map(|k| (*k).to_owned()).collect()
    }

    fn typed(rig: &mut Rig, executor: &Executor<view_engine::EngineHandle>, keys: &[String]) {
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        rig.type_keys(executor, &keys);
    }

    #[test]
    fn branch_replays_the_inputs_before_the_frame_into_a_fresh_engine() {
        let mut rig = Rig::new();
        let asked = std::cell::RefCell::new(Vec::new());
        let respawn = |open: &[String]| {
            asked.borrow_mut().push(open.to_vec());
            EngineConfig::isolated()
        };
        let (mut engine, pump, route, ai_route, executor) = rig.launch(EngineConfig::isolated());
        let mut hello = keys(&["i"]);
        hello.extend(chars("hello"));
        hello.push("<Esc>".to_owned());
        typed(&mut rig, &executor, &hello);
        let at = rig.frames;
        let mut world = keys(&["o"]);
        world.extend(chars("world"));
        world.push("<Esc>".to_owned());
        typed(&mut rig, &executor, &world);
        rig.settle(&engine, &pump, &executor, "the session types", |_, e| {
            line(e, 2) == "world"
        });

        let fresh = rig.branch(
            &mut engine,
            &respawn,
            (&route, &ai_route, &executor),
            at,
            |_, e| line(e, 1) == "hello" && eval(e, "mode()") == "n",
        );
        assert_eq!(
            eval(&fresh.engine, "line('$')"),
            "1",
            "the later line is gone"
        );
        assert_eq!(
            *asked.borrow(),
            [Vec::<String>::new()],
            "the launch's files"
        );
        assert_eq!(
            rig.model.dvr.inputs().count(),
            hello.len() + 2,
            "the launch size, the keys once and the closing size"
        );
        assert_eq!(
            rig.model.dvr.dead(),
            std::slice::from_ref(&(at + 1..=rig.frames))
        );
    }

    /// A staged replay on an attached replacement waits while the
    /// takeover holds input, and folds once view's keys are claimed.
    #[test]
    fn a_replay_waits_for_the_takeover_to_claim_view_keys() {
        let mut rig = Rig::new();
        rig.model.engine.mode.current = "normal".to_owned();
        let x = Msg::Key(Key {
            notation: "x".to_owned(),
        });
        let _ = view_core::update::update(&mut rig.model, x);
        rig.model.dvr.note_frame(1, 1);
        let plan = rig.confirm_branch(1);
        rig.model.dvr.branched(plan.at_frame, plan.replay);
        rig.model.rearm_attach();
        let _ = rig.model.takes_attach();
        let mut taking_over = NativeSession::all_enabled(0, None);
        let _ = taking_over.follow_up(&mut rig.model, crate::native::Stage::VimEnter);
        assert!(taking_over.holds_input());
        assert!(due_replay(&mut rig.model, &taking_over).is_empty());
        assert!(rig.model.dvr.has_replay());
        let claimed = NativeSession::all_enabled(0, None);
        let replay = due_replay(&mut rig.model, &claimed);
        assert!(format!("{replay:?}").contains("\"x\""), "{replay:?}");
    }

    #[test]
    fn branch_through_a_picker_query_reproduces_it() {
        let mut rig = Rig::new();
        let respawn = |_: &[String]| EngineConfig::isolated();
        let (mut engine, pump, route, ai_route, executor) = rig.launch(EngineConfig::isolated());
        let mut query = keys(&["\\", "f", "f"]);
        query.extend(chars("abc"));
        typed(&mut rig, &executor, &query);
        let picked = |rig: &mut Rig| rig.model.picker_mut().map(|p| p.query().to_owned());
        // the query keys are held until the picker opens and are logged as
        // they fold, so the frame showing the query is painted after that
        rig.settle(
            &engine,
            &pump,
            &executor,
            "the picker opens on the query",
            |r, _| picked(r).as_deref() == Some("abc"),
        );
        rig.frames += 1;
        rig.model.dvr.note_frame(rig.frames, 1);
        let at = rig.frames;
        typed(&mut rig, &executor, &keys(&["d"]));
        rig.settle(
            &engine,
            &pump,
            &executor,
            "the picker takes the query",
            |r, _| picked(r).as_deref() == Some("abcd"),
        );

        let logged: Vec<Vec<u8>> = rig
            .model
            .dvr
            .inputs()
            .take_while(|input| input.after_frame < at)
            .map(|input| input.body.to_vec())
            .collect();

        let mut fresh = rig.branch(
            &mut engine,
            &respawn,
            (&route, &ai_route, &executor),
            at,
            |r, _| picked(r).as_deref() == Some("abc"),
        );
        assert_eq!(
            line(&fresh.engine, 1),
            "",
            "no query key reached the buffer"
        );
        assert_eq!(eval(&fresh.engine, "mode()"), "n");
        // the query keys wait behind the picker's open, so the engine gets
        // the closing size ahead of them
        let mut expected = logged;
        let held = expected.len() - 3;
        expected.insert(held, vec![80, 0, 24, 0]);
        let after: Vec<Vec<u8>> = rig.model.dvr.inputs().map(|i| i.body.to_vec()).collect();
        assert_eq!(
            after, expected,
            "the recording in the order the engine got it"
        );

        typed(&mut rig, &fresh.executor, &keys(&["d"]));
        rig.settle(
            &fresh.engine,
            &fresh.pump,
            &fresh.executor,
            "the branch takes the query",
            |r, _| picked(r).is_some_and(|q| q.len() >= 4),
        );
        let at = rig.frames;
        let resent: Vec<Vec<u8>> = rig
            .model
            .dvr
            .inputs()
            .take_while(|input| input.after_frame < at)
            .map(|input| input.body.to_vec())
            .collect();
        expected.push(b"d".to_vec());
        assert_eq!(resent, expected, "a second branch sends what the first did");
        let _again = rig.branch(
            &mut fresh.engine,
            &respawn,
            (&route, &ai_route, &fresh.executor),
            at,
            |r, _| picked(r).is_some_and(|q| q.len() >= 4),
        );
        assert_eq!(picked(&mut rig).as_deref(), Some("abcd"));
    }

    #[test]
    fn branch_discards_swaps_of_the_abandoned_timeline() {
        let scratch = view_test_support::ScratchDir::new("dvr-branch-swap").unwrap();
        let file = scratch.path().join("doc.txt");
        std::fs::write(&file, "on disk\n").unwrap();
        let swaps = scratch.path().join("swap");
        std::fs::create_dir_all(&swaps).unwrap();
        let session = || {
            EngineConfig::default()
                .with_arg("--clean")
                .with_arg("--cmd")
                .with_arg(format!("lua vim.o.directory = [[{}//]]", swaps.display()))
                .with_env("XDG_STATE_HOME", scratch.path().join("state"))
                .with_arg(&file)
        };
        let respawn = |_: &[String]| session();
        let mut rig = Rig::new();
        let (mut engine, pump, route, ai_route, executor) = rig.launch(session());
        let at = rig.frames + 1;
        typed(&mut rig, &executor, &keys(&["x"]));
        let mut edit = keys(&["c", "c"]);
        edit.extend(chars("unsaved"));
        edit.push("<Esc>".to_owned());
        typed(&mut rig, &executor, &edit);
        rig.settle(&engine, &pump, &executor, "the session edits", |_, e| {
            line(e, 1) == "unsaved"
        });
        engine.handle.eval_str("execute('preserve')").unwrap();
        assert_eq!(crate::dvr::io::listed(&swaps).len(), 1);

        let fresh = rig.branch(
            &mut engine,
            &respawn,
            (&route, &ai_route, &executor),
            at,
            |_, e| line(e, 1) == "n disk" && eval(e, "mode()") == "n",
        );
        assert_eq!(
            eval(&fresh.engine, "&modified"),
            "1",
            "the replay edited what is on disk"
        );
        let own = eval(&fresh.engine, "fnamemodify(swapname('%'), ':t')");
        let left = crate::dvr::io::listed(&swaps);
        assert_eq!(left, [own], "only the replacement's own swap is left");
    }

    #[test]
    fn a_branch_that_cannot_start_leaves_unsaved_text_to_recover() {
        let scratch = view_test_support::ScratchDir::new("dvr-branch-keep").unwrap();
        let file = scratch.path().join("doc.txt");
        std::fs::write(&file, "on disk\n").unwrap();
        let swaps = scratch.path().join("swap");
        std::fs::create_dir_all(&swaps).unwrap();
        let bare = || {
            EngineConfig::default()
                .with_arg("--clean")
                .with_arg("--cmd")
                .with_arg(format!("lua vim.o.directory = [[{}//]]", swaps.display()))
                .with_env("XDG_STATE_HOME", scratch.path().join("state"))
        };
        let tries = std::cell::Cell::new(0);
        let respawn = |_: &[String]| {
            tries.set(tries.get() + 1);
            match tries.get() {
                1 => bare().with_nvim_bin("/nonexistent/nvim"),
                _ => bare(),
            }
        };
        let mut rig = Rig::new();
        let (mut engine, pump, route, ai_route, executor) = rig.launch(bare().with_arg(&file));
        let at = rig.frames;
        let mut edit = keys(&["c", "c"]);
        edit.extend(chars("unsaved"));
        edit.push("<Esc>".to_owned());
        typed(&mut rig, &executor, &edit);
        rig.settle(&engine, &pump, &executor, "the session edits", |_, e| {
            line(e, 1) == "unsaved"
        });

        let failed = rig.start_branch(&mut engine, &respawn, (&route, &ai_route, &executor), at);
        assert!(matches!(failed, Err(AttachFailure::Spawn(_))));
        let left = crate::dvr::io::listed(&swaps);
        assert_eq!(
            left.len(),
            1,
            "the swap is back under its own name: {left:?}"
        );
        assert!(!left[0].ends_with(".view-branch"), "{left:?}");

        // the restart the loop runs after a failed branch
        let back = crate::recovery::restart_engine(
            &mut engine,
            &respawn,
            &mut rig.model,
            &rig.channels,
            (&route, &ai_route, &executor),
        )
        .expect("an engine comes back");
        let back = rig.cut_over(back);
        rig.settle(
            &back.engine,
            &back.pump,
            &back.executor,
            "the restarted engine answers",
            |_, e| answers(e),
        );
        back.engine
            .handle
            .command(&format!("silent recover {}", file.display()))
            .unwrap();
        assert_eq!(line(&back.engine, 1), "unsaved", "the text comes back");
    }

    #[test]
    fn a_failed_branch_names_a_swap_it_could_not_copy() {
        let scratch = view_test_support::ScratchDir::new("dvr-branch-nocopy").unwrap();
        let file = scratch.path().join("doc.txt");
        std::fs::write(&file, "on disk\n").unwrap();
        let swaps = scratch.path().join("swap");
        std::fs::create_dir_all(&swaps).unwrap();
        let bare = || {
            EngineConfig::default()
                .with_arg("--clean")
                .with_arg("--cmd")
                .with_arg(format!("lua vim.o.directory = [[{}//]]", swaps.display()))
                .with_env("XDG_STATE_HOME", scratch.path().join("state"))
        };
        let respawn = |_: &[String]| bare().with_nvim_bin("/nonexistent/nvim");
        let mut rig = Rig::new();
        let (mut engine, pump, route, ai_route, executor) = rig.launch(bare().with_arg(&file));
        let at = rig.frames;
        let mut edit = keys(&["c", "c"]);
        edit.extend(chars("unsaved"));
        edit.push("<Esc>".to_owned());
        typed(&mut rig, &executor, &edit);
        rig.settle(&engine, &pump, &executor, "the session edits", |_, e| {
            line(e, 1) == "unsaved"
        });
        let swap = eval(&engine, "fnamemodify(swapname('%'), ':p')");
        // a directory standing where the copy goes refuses it, whoever runs
        std::fs::create_dir(copy_of(std::path::Path::new(&swap))).unwrap();

        let failed = rig.start_branch(&mut engine, &respawn, (&route, &ai_route, &executor), at);
        assert!(matches!(failed, Err(AttachFailure::Spawn(_))));
        let told = format!("{:?}", rig.model.engine.messages.entries);
        assert!(
            told.contains(&format!("could not copy the swap file {swap}")),
            "{told}"
        );
    }

    #[test]
    fn a_swap_copy_left_by_a_crash_is_put_back_at_engine_start() {
        let scratch = view_test_support::ScratchDir::new("dvr-branch-litter").unwrap();
        let swaps = scratch.path().join("swap");
        std::fs::create_dir_all(&swaps).unwrap();
        let gone = swaps.join("%w%gone.txt.swp");
        let kept = swaps.join("%w%kept.txt.swp");
        std::fs::write(copy_of(&gone), "copy of gone").unwrap();
        std::fs::write(&kept, "kept swap").unwrap();
        std::fs::write(copy_of(&kept), "stale copy").unwrap();
        let engine = Engine::spawn(EngineConfig::isolated()).unwrap();
        engine
            .handle
            .command(&format!("set directory=.,{}//", swaps.display()))
            .unwrap();

        restore_swap_copies(&engine);
        assert_eq!(std::fs::read_to_string(&gone).unwrap(), "copy of gone");
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "kept swap");
        let left = crate::dvr::io::listed(&swaps);
        assert_eq!(left.len(), 2, "no copy is left: {left:?}");
    }

    #[test]
    fn branch_closes_the_surfaces_opened_after_the_frame() {
        let scratch = view_test_support::ScratchDir::new("dvr-branch-tree").unwrap();
        let mut rig = Rig::new();
        rig.model.cwd = scratch.path().to_path_buf();
        let respawn = |_: &[String]| EngineConfig::isolated();
        let (mut engine, pump, route, ai_route, executor) = rig.launch(EngineConfig::isolated());
        typed(&mut rig, &executor, &keys(&["i", "a", "<Esc>"]));
        let at = rig.frames;
        let _ = rig.model.close_tree();
        let tree = Msg::FeatureInvoke {
            generation: None,
            feature: "tree".to_owned(),
            verb: "toggle".to_owned(),
        };
        rig.dispatch(&executor, tree);
        rig.frames += 1;
        rig.model.dvr.note_frame(rig.frames, 1);
        rig.settle(&engine, &pump, &executor, "the tree opens", |r, _| {
            r.model.focus() != Focus::Engine
        });

        let _fresh = rig.branch(
            &mut engine,
            &respawn,
            (&route, &ai_route, &executor),
            at,
            |r, e| line(e, 1) == "a" && !r.native.holds_input(),
        );
        let tree_open = rig
            .model
            .overlays()
            .iter()
            .any(|ov| matches!(ov.kind, OverlayKind::Tree(_)));
        assert!(!tree_open, "the tree opened after the frame stays closed");
        assert_eq!(rig.model.focus(), Focus::Engine);
        assert!(rig.model.surfaces.take_reopen().is_empty());
    }

    #[test]
    fn a_key_typed_while_the_branch_starts_lands_after_the_replay() {
        let mut rig = Rig::new();
        let respawn = |_: &[String]| EngineConfig::isolated();
        let (mut engine, pump, route, ai_route, executor) = rig.launch(EngineConfig::isolated());
        let mut hello = keys(&["i"]);
        hello.extend(chars("hello"));
        hello.push("<Esc>".to_owned());
        typed(&mut rig, &executor, &hello);
        rig.settle(&engine, &pump, &executor, "the session types", |_, e| {
            line(e, 1) == "hello" && eval(e, "mode()") == "n"
        });

        let mut fresh = rig
            .start_branch(
                &mut engine,
                &respawn,
                (&route, &ai_route, &executor),
                rig.frames,
            )
            .expect("the replacement starts");
        assert!(
            rig.model.dvr.has_replay(),
            "the key below is typed while the replay is owed"
        );
        typed(&mut rig, &fresh.executor, &keys(&["i"]));
        rig.settle(
            &fresh.engine,
            &fresh.pump,
            &fresh.executor,
            "the replay runs, then the key",
            |_, e| line(e, 1) == "hello" && eval(e, "mode()") == "i",
        );
        let mut more = chars("X");
        more.push("<Esc>".to_owned());
        typed(&mut rig, &fresh.executor, &more);
        rig.settle(
            &fresh.engine,
            &fresh.pump,
            &fresh.executor,
            "the branch types",
            |_, e| line(e, 1) == "hellXo" && eval(e, "mode()") == "n",
        );

        let at = rig.frames;
        let _again = rig.branch(
            &mut fresh.engine,
            &respawn,
            (&route, &ai_route, &fresh.executor),
            at,
            |_, e| line(e, 1) == "hellXo" && eval(e, "mode()") == "n",
        );
    }

    #[test]
    fn a_branch_that_cannot_start_leaves_the_recording_as_it_was() {
        use view_core::native::dvr::Marker;
        let tries = std::cell::Cell::new(0);
        let respawn = |_: &[String]| {
            tries.set(tries.get() + 1);
            if tries.get() == 1 {
                EngineConfig::isolated().with_nvim_bin("/nonexistent/nvim")
            } else {
                EngineConfig::isolated()
            }
        };
        let mut rig = Rig::new();
        let (mut engine, pump, route, ai_route, executor) = rig.launch(EngineConfig::isolated());
        typed(&mut rig, &executor, &keys(&["i", "a", "<Esc>"]));
        let at = rig.frames;
        typed(&mut rig, &executor, &keys(&["x"]));
        rig.settle(&engine, &pump, &executor, "the session types", |_, e| {
            line(e, 1).is_empty()
        });
        let logged = rig.model.dvr.inputs().count();

        let failed = rig.start_branch(&mut engine, &respawn, (&route, &ai_route, &executor), at);
        assert!(matches!(failed, Err(AttachFailure::Spawn(_))));
        let dvr = &rig.model.dvr;
        assert_eq!(dvr.inputs().count(), logged, "the log is not cut");
        assert!(
            dvr.dead().is_empty(),
            "every frame can still be branched from"
        );
        assert!(dvr.markers().iter().all(|(_, m)| *m != Marker::Branch));
        assert!(!dvr.is_branching() && !dvr.has_replay());

        // the restart the loop runs after a failed branch
        let back = crate::recovery::restart_engine(
            &mut engine,
            &respawn,
            &mut rig.model,
            &rig.channels,
            (&route, &ai_route, &executor),
        )
        .expect("an engine comes back");
        let back = rig.cut_over(back);
        rig.settle(
            &back.engine,
            &back.pump,
            &back.executor,
            "the restarted engine answers",
            |_, e| answers(e),
        );
        assert!(rig
            .model
            .dvr
            .markers()
            .iter()
            .any(|(_, m)| *m == Marker::EngineRestart));
    }
}
