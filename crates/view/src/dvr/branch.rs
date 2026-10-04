//! Replaces the editor with one brought to a recorded frame.

use view_core::model::Model;
use view_core::native::dvr::BranchPlan;
use view_engine::{Engine, EngineConfig};

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
/// The replay folds once per recorded input, after the replacement's
/// `VimEnter`, so the takeover's key hold orders the keys it maps.
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
    // and the replacement would otherwise offer to recover each one
    let _ = engine.wait_exit();
    let fresh = replace_engine(engine, respawn, model, channels, bound, &[]);
    if fresh.is_ok() {
        model.dvr.branched(plan.at_frame, plan.replay);
    } else {
        model.dvr.branch_failed();
    }
    fresh
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
                ai: crate::ai_worker::AiWorker::new(
                    view_ai::AgentSpec::Id("claude-code".to_string()),
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
                for msg in view_core::update::due_replay(&mut self.model) {
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
            let _ = view_core::update::update(&mut self.model, scrub);
            self.model.dvr.show(at);
            let _ = view_core::update::update(&mut self.model, key("b"));
            let _ = view_core::update::update(&mut self.model, checked);
            self.model.note_frame_painted();
            let _ = view_core::update::update(&mut self.model, key("y"));
            let plan = std::iter::from_fn(|| self.model.dvr.take_request())
                .find_map(|r| match r {
                    DvrRequest::Branch(plan) => Some(plan),
                    _ => None,
                })
                .expect("a confirmed branch is queued");
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

        let fresh = rig.branch(
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
        assert_eq!(std::fs::read_dir(&swaps).unwrap().count(), 1);

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
