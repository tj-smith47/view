//! The agent's filesystem requests, folded into effects.
//!
//! Three hops per request, each a plain `Effect` this returns and the loop's
//! executor carries out: resolve the path onto a buffer
//! (`RpcCall::LoadHidden`), read or write that buffer
//! (`RpcCall::AiFsRead`/`AiFsWrite`), release the hold and answer the agent
//! (`RpcCall::ReleaseHidden` plus `Effect::Ai`). Nothing here awaits
//! anything, so a slow or wedged nvim costs the agent its answer and the
//! paint loop nothing at all.
//!
//! The gates run before the first hop, never between them. A request this
//! session may not answer is refused while it is still a value -- no
//! generation allocated, no hold taken, no buffer touched -- so a refusal
//! cannot leak what the path it named contains.

use crate::model::Model;
use crate::msg::{BufferHandle, Effect, RpcCall};
use crate::native::ai_event::{AiCommand, FsError};
use crate::native::ai_fs::{split_content, FsIntent, PendingFsOp};
use std::path::PathBuf;

/// Answers an `AiEvent::FsReadRequested` by starting its round trip, or
/// refuses it outright.
pub(super) fn on_read_requested(
    model: &mut Model,
    request_id: u64,
    path: PathBuf,
    line: Option<u32>,
    limit: Option<u32>,
) -> Vec<Effect> {
    start(
        model,
        request_id,
        path,
        FsIntent::Read { line, limit },
        read_refusal,
    )
}

/// Answers an `AiEvent::FsWriteRequested` by starting its round trip, or
/// refuses it outright.
pub(super) fn on_write_requested(
    model: &mut Model,
    request_id: u64,
    path: PathBuf,
    content: &str,
) -> Vec<Effect> {
    let (lines, eol) = split_content(content);
    start(
        model,
        request_id,
        path,
        FsIntent::Write { lines, eol },
        write_refusal,
    )
}

/// The gates and the first hop, shared by both sides: a client that
/// advertises safe file access must not let a read reach anywhere a write
/// may not, so neither the checks nor their order differ between them.
fn start(
    model: &mut Model,
    request_id: u64,
    path: PathBuf,
    intent: FsIntent,
    refuse: fn(u64, FsError) -> Effect,
) -> Vec<Effect> {
    // An untrusted project is one no agent may read or write through this
    // client at all. Checked here rather than only where the session is
    // launched: trust can be revoked while a session is running, and a
    // request already in the agent's tool loop must not outlive it.
    if !model.ai_trusted {
        return vec![refuse(request_id, FsError::PermissionDenied)];
    }
    // The wire states `path` is absolute for both methods
    // (`docs/acp-v1-wire-capture.md`, `fs/read_text_file` case 1). A
    // relative spelling is refused rather than resolved against this
    // process's own working directory: nvim resolves one against *its*
    // cwd, which `:cd` moves and this process never observes, so any answer
    // given here would be for a file nobody named.
    //
    // A deliberate early-out, not this client's authority on unusable
    // paths: `view_engine::nvim_api::hidden_path_refusal` is, and it
    // refuses blank and trailing-separator spellings this never sees, which
    // reach the agent as `NotFound` through the buffer-less resolve
    // instead. The invariant that keeps the two from drifting is that
    // whatever this refuses, that set refuses too -- pinned in the one
    // crate that can name both (`view::runtime`'s
    // `the_cores_own_path_refusal_is_a_subset_of_the_engines`). Widening
    // this predicate without widening that one would have this client
    // refuse a path nvim would have opened.
    if !path.is_absolute() {
        return vec![refuse(
            request_id,
            FsError::Other {
                message: "the path must be absolute".to_owned(),
            },
        )];
    }
    let generation = model.next_hidden_generation();
    let op = PendingFsOp {
        request_id,
        generation,
        path: path.clone(),
        intent,
    };
    if !model.ai_fs.open(op) {
        return vec![refuse(
            request_id,
            FsError::Other {
                message: "too many filesystem requests are already in flight".to_owned(),
            },
        )];
    }
    vec![Effect::Rpc(RpcCall::LoadHidden {
        path: path.to_string_lossy().into_owned(),
        generation,
    })]
}

/// Folds the resolve for one agent request, or reports that `generation`
/// belongs to no such request.
///
/// `None` is the caller's signal to try the next owner of the same message
/// -- the diff review, which draws its own generations from the identical
/// counter, so exactly one of the two ever claims a given reply.
pub(super) fn on_hidden_buffer_loaded(
    model: &mut Model,
    generation: u64,
    buf: Option<BufferHandle>,
    changedtick: u64,
) -> Option<Vec<Effect>> {
    let op = model.ai_fs.by_generation(generation)?;
    let (request_id, intent) = (op.request_id, op.intent.clone());
    let Some(buf) = buf else {
        // nvim named no buffer: the path is a directory, unreadable, or a
        // spelling the engine refuses. The hold still has to come back --
        // `load_hidden` takes it at issue time, before any reply could have
        // said otherwise.
        let op = model.ai_fs.take_by_generation(generation)?;
        let refuse = match op.intent {
            FsIntent::Read { .. } => read_refusal,
            FsIntent::Write { .. } => write_refusal,
        };
        return Some(vec![release(&op), refuse(request_id, FsError::NotFound)]);
    };
    Some(vec![Effect::Rpc(match intent {
        FsIntent::Read { line, limit } => RpcCall::AiFsRead {
            request_id,
            buf,
            line,
            limit,
        },
        FsIntent::Write { lines, eol } => RpcCall::AiFsWrite {
            request_id,
            buf,
            lines,
            eol,
            expected_changedtick: changedtick,
        },
    })])
}

/// Answers the agent's read and gives back the hold it rode.
pub(super) fn on_read_reply(
    model: &mut Model,
    request_id: u64,
    result: Result<String, FsError>,
) -> Vec<Effect> {
    let Some(op) = model.ai_fs.take_by_request(request_id) else {
        return Vec::new();
    };
    vec![
        release(&op),
        Effect::Ai(AiCommand::FsReadReply { request_id, result }),
    ]
}

/// Answers the agent's write and gives back the hold it rode.
pub(super) fn on_write_reply(
    model: &mut Model,
    request_id: u64,
    result: Result<(), FsError>,
) -> Vec<Effect> {
    let Some(op) = model.ai_fs.take_by_request(request_id) else {
        return Vec::new();
    };
    vec![
        release(&op),
        Effect::Ai(AiCommand::FsWriteReply { request_id, result }),
    ]
}

/// Gives back every hold still outstanding when the session that raised
/// them ends.
///
/// No `Effect::Ai` accompanies them: the session those answers would cross
/// into is gone, and its own correlation map drops the channels the agent
/// is waiting on, which the transport reports as a cancelled request. The
/// holds are this process's own and have to come back regardless, or the
/// buffers they pin outlive every session for the rest of the run.
pub(super) fn on_session_ended(model: &mut Model) -> Vec<Effect> {
    model.ai_fs.drain().iter().map(release).collect()
}

fn release(op: &PendingFsOp) -> Effect {
    Effect::Rpc(RpcCall::ReleaseHidden {
        path: op.path.to_string_lossy().into_owned(),
    })
}

/// A read this client will not answer, as the command that crosses back.
///
/// Carries the reason and nothing else -- there is no shape in which a
/// refused read reports content, so no byte of the file it named can travel
/// on it (`docs/acp-v1-wire-capture.md`, `fs/read_text_file` case 2).
fn read_refusal(request_id: u64, error: FsError) -> Effect {
    Effect::Ai(AiCommand::FsReadReply {
        request_id,
        result: Err(error),
    })
}

fn write_refusal(request_id: u64, error: FsError) -> Effect {
    Effect::Ai(AiCommand::FsWriteReply {
        request_id,
        result: Err(error),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::msg::Msg;
    use crate::native::ai_event::AiEvent;
    use crate::native::ai_fs::MAX_IN_FLIGHT;
    use crate::update::update;

    /// Two absolute paths in this platform's own shape. The requests these
    /// tests drive are refused unless the path is absolute, and a
    /// POSIX-rooted literal is not absolute on windows.
    #[cfg(unix)]
    const FILE_A: &str = "/p/a.rs";
    #[cfg(unix)]
    const FILE_B: &str = "/p/b.rs";
    #[cfg(not(unix))]
    const FILE_A: &str = r"C:\p\a.rs";
    #[cfg(not(unix))]
    const FILE_B: &str = r"C:\p\b.rs";

    fn trusted() -> Model {
        let mut model = Model::new();
        model.ai_trusted = true;
        model
    }

    fn read_request(request_id: u64, path: &str) -> Msg {
        Msg::Ai(AiEvent::FsReadRequested {
            request_id,
            path: PathBuf::from(path),
            line: None,
            limit: None,
        })
    }

    fn write_request(request_id: u64, path: &str, content: &str) -> Msg {
        Msg::Ai(AiEvent::FsWriteRequested {
            request_id,
            path: PathBuf::from(path),
            content: content.to_owned(),
        })
    }

    fn loaded(generation: u64, buf: u64, changedtick: u64) -> Msg {
        Msg::HiddenBufferLoaded {
            generation,
            buf: Some(BufferHandle(buf)),
            created: true,
            changedtick,
        }
    }

    /// `Effect` is deliberately not comparable (it carries closures on
    /// other arms), so these two lift the parts that are.
    fn rpcs(effects: &[Effect]) -> Vec<RpcCall> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Rpc(call) => Some(call.clone()),
                _ => None,
            })
            .collect()
    }

    fn commands(effects: &[Effect]) -> Vec<AiCommand> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Ai(command) => Some(command.clone()),
                _ => None,
            })
            .collect()
    }

    fn generation_of(effects: &[Effect]) -> u64 {
        match effects.first() {
            Some(Effect::Rpc(RpcCall::LoadHidden { generation, .. })) => *generation,
            other => panic!("expected a LoadHidden, got {other:?}"),
        }
    }

    /// The read's whole round trip: the path resolves to a buffer, the
    /// buffer is what gets read, and the answer travels back beside the
    /// release of the hold the resolve took. The buffer handle in the read
    /// call is the assertion that matters -- an answer sourced from disk
    /// would never need one, and would miss the user's unsaved edits.
    #[test]
    fn a_read_resolves_onto_a_buffer_reads_it_and_releases_its_hold() {
        let mut model = trusted();

        let started = update(&mut model, read_request(7, FILE_A));
        let generation = generation_of(&started);
        assert_eq!(
            rpcs(&started),
            vec![RpcCall::LoadHidden {
                path: FILE_A.to_owned(),
                generation,
            }]
        );

        let resolved = update(&mut model, loaded(generation, 4, 11));
        assert_eq!(
            rpcs(&resolved),
            vec![RpcCall::AiFsRead {
                request_id: 7,
                buf: BufferHandle(4),
                line: None,
                limit: None,
            }]
        );

        let answered = update(
            &mut model,
            Msg::AiFsReadReply {
                request_id: 7,
                result: Ok("edited\n".to_owned()),
            },
        );
        assert_eq!(
            rpcs(&answered),
            vec![RpcCall::ReleaseHidden {
                path: FILE_A.to_owned()
            }]
        );
        assert_eq!(
            commands(&answered),
            vec![AiCommand::FsReadReply {
                request_id: 7,
                result: Ok("edited\n".to_owned()),
            }]
        );
        assert!(model.ai_fs.is_empty(), "the request is no longer in flight");
    }

    /// The wire's `line`/`limit` window reaches the engine rather than being
    /// dropped and re-derived from a whole-file answer: a windowed read is
    /// the agent asking nvim for those lines only, and costs nvim only
    /// those lines.
    #[test]
    fn a_windowed_read_carries_its_window_to_the_engine() {
        let mut model = trusted();

        let started = update(
            &mut model,
            Msg::Ai(AiEvent::FsReadRequested {
                request_id: 1,
                path: PathBuf::from(FILE_A),
                line: Some(10),
                limit: Some(5),
            }),
        );
        let resolved = update(&mut model, loaded(generation_of(&started), 2, 0));

        assert_eq!(
            rpcs(&resolved),
            vec![RpcCall::AiFsRead {
                request_id: 1,
                buf: BufferHandle(2),
                line: Some(10),
                limit: Some(5),
            }]
        );
    }

    /// The write's round trip, and the two things a line list alone cannot
    /// carry: the trailing newline the agent's `content` did or did not end
    /// with, and the tick the resolve observed, which is what lets nvim
    /// refuse a write against a buffer that moved underneath it.
    #[test]
    fn a_write_carries_the_split_content_the_eol_flag_and_the_observed_tick() {
        let mut model = trusted();

        let started = update(&mut model, write_request(3, FILE_A, "one\ntwo\n"));
        let resolved = update(&mut model, loaded(generation_of(&started), 9, 42));

        assert_eq!(
            rpcs(&resolved),
            vec![RpcCall::AiFsWrite {
                request_id: 3,
                buf: BufferHandle(9),
                lines: vec!["one".to_owned(), "two".to_owned()],
                eol: true,
                expected_changedtick: 42,
            }]
        );

        let answered = update(
            &mut model,
            Msg::AiFsWriteReply {
                request_id: 3,
                result: Ok(()),
            },
        );
        assert_eq!(
            rpcs(&answered),
            vec![RpcCall::ReleaseHidden {
                path: FILE_A.to_owned()
            }]
        );
        assert_eq!(
            commands(&answered),
            vec![AiCommand::FsWriteReply {
                request_id: 3,
                result: Ok(()),
            }]
        );
    }

    /// An untrusted project answers nothing: no resolve is issued, so no
    /// buffer is ever loaded for the path and no byte of it can reach the
    /// agent. The refusal itself still goes back -- a request left
    /// unanswered blocks the agent's tool loop for the rest of the session.
    #[test]
    fn an_untrusted_project_refuses_both_legs_without_resolving_anything() {
        let mut model = Model::new();

        let read = update(&mut model, read_request(1, FILE_A));
        let write = update(&mut model, write_request(2, FILE_A, "x\n"));

        assert_eq!(
            commands(&read),
            vec![AiCommand::FsReadReply {
                request_id: 1,
                result: Err(FsError::PermissionDenied),
            }]
        );
        assert_eq!(
            commands(&write),
            vec![AiCommand::FsWriteReply {
                request_id: 2,
                result: Err(FsError::PermissionDenied),
            }]
        );
        assert!(
            rpcs(&read).is_empty() && rpcs(&write).is_empty(),
            "a refused request issues no call at all"
        );
        assert!(
            model.ai_fs.is_empty(),
            "a refused request never becomes in-flight state"
        );
    }

    /// A relative path is refused rather than resolved: nvim would resolve
    /// one against its own cwd, which `:cd` moves and this process never
    /// sees, so any answer would be for a file nobody named.
    #[test]
    fn a_relative_path_is_refused_on_both_legs() {
        let mut model = trusted();

        let read = update(&mut model, read_request(1, "a.rs"));
        let write = update(&mut model, write_request(2, "a.rs", "x\n"));

        assert!(matches!(
            commands(&read).as_slice(),
            [AiCommand::FsReadReply {
                result: Err(FsError::Other { .. }),
                ..
            }]
        ));
        assert!(matches!(
            commands(&write).as_slice(),
            [AiCommand::FsWriteReply {
                result: Err(FsError::Other { .. }),
                ..
            }]
        ));
        assert!(
            rpcs(&read).is_empty() && rpcs(&write).is_empty(),
            "a refused request issues no call at all"
        );
    }

    /// A resolve that named no buffer still gives its hold back.
    /// `load_hidden` takes the hold when the call is issued, before any
    /// reply could have said the path was unusable, so the release is owed
    /// whether or not a buffer came back.
    #[test]
    fn a_resolve_that_named_no_buffer_refuses_and_still_releases() {
        let mut model = trusted();

        let started = update(&mut model, read_request(5, FILE_A));
        let generation = generation_of(&started);
        let resolved = update(
            &mut model,
            Msg::HiddenBufferLoaded {
                generation,
                buf: None,
                created: false,
                changedtick: 0,
            },
        );

        assert_eq!(
            rpcs(&resolved),
            vec![RpcCall::ReleaseHidden {
                path: FILE_A.to_owned()
            }]
        );
        assert_eq!(
            commands(&resolved),
            vec![AiCommand::FsReadReply {
                request_id: 5,
                result: Err(FsError::NotFound),
            }]
        );
        assert!(model.ai_fs.is_empty());
    }

    /// Two overlapping requests for the same path each take their own hold
    /// and each give it back, one release per hold: the engine's refcount
    /// deletes the buffer only when the last one lands, so the second
    /// request never reads through a handle the first one's cleanup
    /// destroyed.
    #[test]
    fn two_overlapping_requests_for_one_path_take_and_give_back_two_holds() {
        let mut model = trusted();

        let first = generation_of(&update(&mut model, read_request(1, FILE_A)));
        let second = generation_of(&update(&mut model, read_request(2, FILE_A)));
        assert_ne!(first, second, "each request resolves under its own tag");

        let _ = update(&mut model, loaded(first, 8, 0));
        let _ = update(&mut model, loaded(second, 8, 0));

        let release = RpcCall::ReleaseHidden {
            path: FILE_A.to_owned(),
        };
        for request_id in [2, 1] {
            let answered = update(
                &mut model,
                Msg::AiFsReadReply {
                    request_id,
                    result: Ok("shared\n".to_owned()),
                },
            );
            assert_eq!(rpcs(&answered), vec![release.clone()]);
        }
        assert!(model.ai_fs.is_empty());
    }

    /// The resolve reply is claimed by exactly one owner. A generation no
    /// filesystem request holds falls through untouched, so the diff review
    /// -- which draws from the identical counter -- still folds its own.
    #[test]
    fn a_resolve_for_a_generation_no_request_holds_falls_through() {
        let mut model = trusted();
        let started = update(&mut model, read_request(1, FILE_A));
        let generation = generation_of(&started);

        let stranger =
            on_hidden_buffer_loaded(&mut model, generation + 1, Some(BufferHandle(3)), 0);

        assert!(stranger.is_none(), "the other owner gets its turn");
        assert_eq!(model.ai_fs.len(), 1, "and this request keeps waiting");
    }

    /// A session that ends mid-request gives back every hold it still
    /// holds, and answers nothing: the correlation map those answers would
    /// cross into is gone with the session, while the holds are this
    /// process's own and would otherwise pin their buffers for the rest of
    /// the run.
    #[test]
    fn a_session_ending_releases_every_outstanding_hold_and_answers_none() {
        let mut model = trusted();
        let _ = update(&mut model, read_request(1, FILE_A));
        let _ = update(&mut model, write_request(2, FILE_B, "x\n"));

        let effects = on_session_ended(&mut model);

        assert_eq!(
            rpcs(&effects),
            vec![
                RpcCall::ReleaseHidden {
                    path: FILE_A.to_owned()
                },
                RpcCall::ReleaseHidden {
                    path: FILE_B.to_owned()
                },
            ]
        );
        assert!(
            commands(&effects).is_empty(),
            "the session those answers would cross into is gone"
        );
        assert!(model.ai_fs.is_empty());
    }

    /// The crash *wiring*, not the drain it calls: an agent is most likely
    /// to die mid-tool-call, which is precisely when requests are in
    /// flight, and a `SessionCrashed` that forgot to drain would pin one
    /// hidden buffer per outstanding request for the rest of the editor's
    /// run with nothing left alive to release it.
    #[test]
    fn a_session_crash_releases_every_hold_its_requests_were_riding() {
        let mut model = trusted();
        let _ = update(&mut model, read_request(1, FILE_A));
        let _ = update(&mut model, write_request(2, FILE_B, "x\n"));

        let effects = update(
            &mut model,
            Msg::Ai(AiEvent::SessionCrashed {
                message: "the agent exited".to_owned(),
            }),
        );

        let released: Vec<RpcCall> = rpcs(&effects)
            .into_iter()
            .filter(|call| matches!(call, RpcCall::ReleaseHidden { .. }))
            .collect();
        assert_eq!(released.len(), 2, "one release per hold still outstanding");
        assert!(model.ai_fs.is_empty(), "and nothing left in flight");
        assert!(
            !commands(&effects).iter().any(|command| matches!(
                command,
                AiCommand::FsReadReply { .. } | AiCommand::FsWriteReply { .. }
            )),
            "the session those answers would cross into is gone"
        );
    }

    /// The in-flight cap as a *gate*, not as a container that reports
    /// `false`: past the cap the request must be answered and no resolve
    /// issued. A cap that let the resolve through anyway would take a hold
    /// nothing records -- and so nothing ever releases -- while the agent's
    /// own request went unanswered for the rest of the session, which is
    /// worse than the leak the cap exists to prevent.
    #[test]
    fn a_request_past_the_in_flight_cap_is_answered_and_resolves_nothing() {
        let mut model = trusted();
        for request_id in 0..u64::try_from(MAX_IN_FLIGHT).unwrap_or(u64::MAX) {
            let started = update(&mut model, read_request(request_id, FILE_A));
            assert_eq!(rpcs(&started).len(), 1, "request {request_id} must resolve");
        }

        let refused = update(&mut model, read_request(9999, FILE_A));

        assert!(
            rpcs(&refused).is_empty(),
            "a request past the cap must take no hold at all: {refused:?}"
        );
        assert!(matches!(
            commands(&refused).as_slice(),
            [AiCommand::FsReadReply {
                request_id: 9999,
                result: Err(FsError::Other { .. }),
            }]
        ));
    }

    /// An answer for a request that is no longer in flight releases
    /// nothing. The hold it would name has already been given back, and a
    /// second release for the same path would decrement another holder's
    /// count and delete a buffer still in use.
    #[test]
    fn an_answer_for_an_unknown_request_releases_nothing() {
        let mut model = trusted();

        let effects = update(
            &mut model,
            Msg::AiFsReadReply {
                request_id: 99,
                result: Ok("x".to_owned()),
            },
        );

        assert!(effects.is_empty());
    }
}
