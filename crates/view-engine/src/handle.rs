use crate::damage::PumpShared;
use crate::rpc::{RpcError, RpcMessage};
use crate::ui_events::decode_redraw;
use rmpv::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;
use view_core::msg::{
    DeleteConfirmOutcome, EngineRequest, Msg, RegisterType, ReplyToken, ReplyValue,
};
use view_core::native::mappings::MappingClaim;

/// Errors produced by [`EngineHandle`] operations.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// RPC message encoding/decoding error.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// I/O error reading from or writing to the engine connection.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The engine returned a non-nil error value for a request.
    #[error("engine returned error: {0:?}")]
    Remote(Value),
    /// The engine connection was closed before the response arrived.
    #[error("engine connection closed")]
    Closed,
    /// No response arrived within the caller-supplied timeout. Raised only
    /// by [`EngineHandle::request_timeout`]; the plain
    /// [`request`](EngineHandle::request) call has no timeout and cannot
    /// produce this variant.
    #[error("no response to {method} within {timeout:?}")]
    Timeout {
        /// The RPC method that timed out.
        method: String,
        /// The timeout duration that elapsed without a response.
        timeout: Duration,
    },
}

/// A notification received from the engine (e.g., a `redraw` event).
///
/// Notifications are fire-and-forget messages that do not correlate to any
/// specific request. They are delivered on the receiver returned by
/// [`EngineHandle::start`] in the order they arrive from the engine.
#[derive(Debug)]
pub struct EngineNotification {
    /// The name of the RPC method being notified.
    pub method: String,
    /// The positional arguments for the notification.
    pub params: Vec<Value>,
}

/// What a pending request's `msgid` resolves to once its `Response` arrives.
/// Two shapes share one map (rather than two separate maps keyed by the same
/// `msgid` space) so `close_and_drain`'s single lock-and-drain step covers
/// both kinds atomically: a connection loss must fail every synchronous
/// waiter *and* silently drop every in-flight probe in the same critical
/// section, or a probe registered between the drain and the flag flip could
/// survive as a leaked map entry no future `Response` will ever remove.
enum Waiter {
    /// A synchronous [`EngineHandle::request`]/`request_timeout` caller
    /// blocked on `rx.recv()`.
    Reply(mpsc::Sender<Result<Value, EngineError>>),
    /// An async `nvim_get_hl` default-colors probe (see
    /// [`EngineHandle::request_probe`]): nothing is blocked on this `msgid`,
    /// so its `Response` is decoded and routed to `pump` as
    /// `Msg::HlProbeReply` instead of sent anywhere synchronous.
    HlProbe { generation: u64 },
    /// An async read-side liveness probe (see
    /// [`EngineHandle::request_heartbeat`]): nothing is blocked on this
    /// `msgid` either, and its `Response` carries no value this connection
    /// cares about -- an acknowledgement is a "still answering" signal, not
    /// a reading -- so it is routed to `pump` as `Msg::HeartbeatReply`,
    /// tagged with `generation` so a reply arriving out of order cannot
    /// move a liveness verdict backwards.
    Heartbeat { generation: u64 },
    /// An async mapping registration (see
    /// [`EngineHandle::request_mappings`]): nothing is blocked on this
    /// `msgid` either, and its `Response` carries every key the chunk
    /// claimed, routed to `pump` as `Msg::MappingsClaimed`.
    MappingClaims,
    /// An async read of what this engine recovered while starting (see
    /// [`EngineHandle::probe_swap_recovery`]): nothing is blocked on this
    /// `msgid`, so its `Response` is decoded and routed to `pump` as
    /// `Msg::SwapRecovered`.
    ///
    /// Tagged with `generation` like every async reply beside it, and for the
    /// same reason: a restart hands the replacement engine's pump the sink
    /// the dead one wrote into, so a reading that already crossed into that
    /// sink can be folded after the cutover and speak for an engine that is
    /// gone. Draining a closed connection's waiters covers the reply that
    /// never arrives, not the one already in flight.
    SwapRecovery { generation: u64 },
    /// An async buffer-list enumeration for a picker's `Source::Buffers`
    /// (see [`EngineHandle::request_buffer_list`]): nothing is blocked on
    /// this `msgid`, so its `Response` is decoded and routed to `pump` as
    /// `Msg::PickerBufferList`, tagged with `generation` so a reply a later
    /// query has since superseded can be dropped by `update()` instead of
    /// clobbering it.
    BufferList { generation: u64 },
    /// An async picker-preview lookup for a candidate path (see
    /// [`EngineHandle::request_preview`]): nothing is blocked on this
    /// `msgid`, so its `Response` is decoded and routed to `pump` as
    /// `Msg::PickerPreviewReply`, tagged with `generation` (dropped by
    /// `update()` if a later query has since superseded it) and `path`
    /// (echoed back since the reply itself carries no path -- the picker's
    /// selection may have moved on by the time this lands).
    Preview { generation: u64, path: String },
    /// An async file-tree rename (see [`EngineHandle::rename_file`]):
    /// nothing is blocked on this `msgid`, so its `Response` is decoded and
    /// routed to `pump` as `Msg::TreeRenameReply`, tagged with `generation`
    /// -- which names the rename request itself, not a scan or a git
    /// refresh, so `update()` does not compare it against the tree's own
    /// counters: a successful reply always triggers an unconditional
    /// rescan, and a refused one surfaces as a notice with the tree left
    /// exactly as it was.
    Rename { generation: u64 },
    /// An async tree-sidebar create-file prompt (see
    /// [`EngineHandle::request_create_prompt`]): nothing is blocked on this
    /// `msgid`, so its `Response` is decoded and routed to `pump` as
    /// `Msg::TreeCreatePromptReply`, tagged with `generation` so a reply for
    /// a prompt issued before the tree was closed and reopened is dropped by
    /// `update()` rather than creating a file nothing asked for anymore.
    CreatePrompt { generation: u64 },
    /// An async tree-sidebar rename prompt (see
    /// [`EngineHandle::request_rename_prompt`]): nothing is blocked on this
    /// `msgid`, so its `Response` is decoded and routed to `pump` as
    /// `Msg::TreeRenamePromptReply`, tagged with `generation` (the same
    /// stale-reply guard `CreatePrompt` carries) and `old_path` (echoed back
    /// since the reply itself carries only the typed name, and the rename
    /// this eventually issues needs to know what it is renaming from).
    RenamePrompt { generation: u64, old_path: String },
    /// An async tree-sidebar delete confirmation (see
    /// [`EngineHandle::request_delete_confirm`]): nothing is blocked on this
    /// `msgid`, so its `Response` is decoded and routed to `pump` as
    /// `Msg::TreeDeleteConfirmReply`, tagged with `generation` (the same
    /// stale-reply guard `CreatePrompt` carries) and `path` (echoed back
    /// since `vim.fn.confirm`'s reply carries only the chosen button index).
    DeleteConfirm { generation: u64, path: String },
}

/// The set of in-flight request waiters plus a `closed` flag, guarded by a
/// single lock so a reader or writer thread that discovers the connection
/// is gone can mark it closed and drain every waiter in one atomic step.
/// Without sharing the lock, a request could insert itself into the map
/// after the draining thread has already run, leaking a waiter that will
/// never be resolved (the original hang this type exists to close).
struct PendingState {
    waiters: HashMap<u32, Waiter>,
    /// Shared with [`EngineHandle::is_closed`] rather than owned outright,
    /// so an observer that must not wait on this lock -- a paint loop
    /// asking whether the connection is still there is asking precisely
    /// because it may not be -- can read the flag without taking it. Every
    /// *write* still happens inside the critical section below, so the
    /// insert-versus-drain race the shared lock exists to close is
    /// unaffected; the lock-free read is only ever one observation stale,
    /// which costs the observer one more pass and nothing else.
    closed: Arc<AtomicBool>,
}

type Pending = Arc<Mutex<PendingState>>;

/// An RPC client for the embedded Neovim process, with request correlation
/// and a flood-proof notification reader.
///
/// `EngineHandle` is cheap to clone (all state is `Arc`- or channel-backed)
/// and `Send`, so requests can be issued from several threads while another
/// thread owns the notification receiver. Cloning does not spawn new
/// threads; every clone shares the same reader/writer pair created by
/// [`start`](Self::start).
///
/// [`start`](Self::start) spawns two internal threads:
/// - A reader thread that decodes incoming messages, correlates responses
///   to requests, forwards notifications to a receiver, and dispatches the
///   one nvim-to-client request the runtime understands
///   (`view_vim_enter`, routed to the runtime loop as
///   [`Msg::EngineRequest`](view_core::msg::Msg::EngineRequest) and answered
///   once the runtime replies). Every other incoming `Request` gets an
///   immediate `"method not supported"` error, since the msgpack-RPC
///   contract requires a reply of some kind or the peer's main loop blocks
///   forever waiting for one.
/// - A writer thread that owns the write half and serializes every
///   outgoing message (requests, the auto-replies above, and fire-and-forget
///   notifications) fed to it over an internal channel. Callers never touch
///   the pipe directly, so a write that blocks on a full OS pipe buffer
///   blocks only the writer thread, never the caller's timeout.
///
/// Both threads share one `closed` flag with the pending-waiters map (see
/// `PendingState`): whichever thread notices the connection is gone first
/// marks it closed and drains every waiter with
/// [`EngineError::Closed`] in the same critical
/// section that flips the flag, so a request racing the shutdown either
/// lands before the flag (and gets drained) or after it (and is rejected
/// before it ever touches the pipe).
///
/// A connection started via [`start`](Self::start) routes notifications
/// through an unbounded channel, ensuring that a flood of notifications
/// (e.g., a large `redraw` burst) never blocks the delivery of pending
/// responses. A connection started via `start_pumped` routes `redraw`
/// notifications through the bounded, compacted `PumpShared` instead and
/// allocates no notification channel at all; the two routing modes are
/// mutually exclusive per connection.
pub struct EngineHandle {
    next_msgid: Arc<AtomicU32>,
    pending: Pending,
    closed: Arc<AtomicBool>,
    announced_exit: Arc<AtomicBool>,
    /// Signalled once by whichever of the two threads discovers the
    /// connection is gone, right after it publishes `closed`. What a caller
    /// resolving a stop waits on when it needs the reader's own findings
    /// (`announced_exit`) and not merely the child's exit status.
    settled: Arc<Condvar>,
    outbox: Arc<crate::outbox::Outbox>,
}

impl Clone for EngineHandle {
    fn clone(&self) -> Self {
        Self {
            next_msgid: Arc::clone(&self.next_msgid),
            pending: Arc::clone(&self.pending),
            closed: Arc::clone(&self.closed),
            announced_exit: Arc::clone(&self.announced_exit),
            settled: Arc::clone(&self.settled),
            outbox: Arc::clone(&self.outbox),
        }
    }
}

impl EngineHandle {
    /// Starts a new engine handle, spawning reader and writer threads.
    ///
    /// # Arguments
    ///
    /// * `reader` - An unbuffered read source (typically one end of a pipe).
    ///   The handle wraps it in a `BufReader` internally.
    /// * `writer` - An unbuffered write sink (typically the other end of the
    ///   pipe pair). Owned entirely by the writer thread; nothing outside
    ///   that thread ever calls into it.
    ///
    /// # Returns
    ///
    /// A tuple of:
    /// - The `EngineHandle` for issuing requests.
    /// - An `mpsc::Receiver<EngineNotification>` for receiving notifications
    ///   from the engine. The receiver is unbounded to prevent the reader
    ///   thread from stalling under a notification flood.
    ///
    /// # Panics
    ///
    /// Never panics. Errors on the internal reader or writer thread (I/O,
    /// decode, a broken pipe) cause the affected thread to exit cleanly,
    /// marking the connection closed and sending `Closed` to any pending
    /// requests.
    ///
    /// # Notification receiver lifetime
    ///
    /// The reader thread sends notifications to the returned `Receiver`
    /// without buffering elsewhere. If the receiver is dropped, the next
    /// `notif_tx.send` fails and the reader thread exits its loop; after
    /// that, every in-flight and future [`request`](Self::request) call
    /// fails with [`EngineError::Closed`] instead of hanging, since nothing
    /// remains to read responses off the wire. Keep the receiver alive (or
    /// drain it) for the lifetime of the handle.
    ///
    /// The reader and writer threads spawned here are detached: `start`
    /// does not return join handles, and there is no shutdown signal beyond
    /// dropping the notification receiver or closing the underlying
    /// reader/writer. Orderly shutdown is the responsibility of the owning
    /// Engine type, which controls the lifetime of the pipe endpoints.
    ///
    /// Test-only: [`Engine::spawn`](crate::process::Engine::spawn) always
    /// uses the crate-private, pumped [`start_pumped`](Self::start_pumped)
    /// instead, so this unbounded-channel constructor has no production
    /// caller. Gated behind the `test-support` feature (which this crate's
    /// own `Cargo.toml` enables for itself during `cargo test` via a self
    /// dev-dependency) so it never ships in an ordinary build.
    #[cfg(any(test, feature = "test-support"))]
    pub fn start(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> (Self, mpsc::Receiver<EngineNotification>) {
        // unbounded so the reader thread can never stall a pending response
        // behind a redraw flood; the bounded, compacted path is `pump`,
        // used instead of this channel by `start_pumped`
        let (notif_tx, notif_rx) = mpsc::channel();
        let handle = Self::start_with_pipe(
            reader,
            writer,
            None,
            Some(notif_tx),
            #[cfg(any(unix, windows))]
            None,
        );
        (handle, notif_rx)
    }

    /// Same as [`start`](Self::start), but the reader thread also decodes
    /// every `redraw` notification and folds it into `pump`'s
    /// [`DamageBuffer`](crate::damage::DamageBuffer), and dispatches known
    /// engine-initiated requests (currently `view_vim_enter`) as
    /// `Msg::EngineRequest` through `pump` instead of auto-erroring them.
    /// A pumped connection routes every `redraw` notification through `pump`
    /// exclusively: no unbounded notification channel is allocated for it at
    /// all, since nothing consumes one.
    ///
    /// Crate-private: only [`Engine::spawn`](crate::process::Engine::spawn)
    /// needs the pump wired up. Direct `EngineHandle` construction (tests,
    /// or any future caller with no damage/runtime loop) uses plain
    /// [`start`](Self::start) and keeps the as-built auto-error-every-request
    /// behavior, since there is nowhere to route a dispatched request
    /// without a pump.
    /// [`start_pumped`](Self::start_pumped) for a writer whose pipe this
    /// platform can ask about writability, enabling the outbox's inline
    /// fast path (see [`crate::outbox`]). `pipe` must be a handle on the
    /// same pipe `writer` writes to, or the fast path would consult one
    /// pipe's readiness before writing to another.
    pub(crate) fn start_pumped(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        pump: Arc<PumpShared>,
        #[cfg(any(unix, windows))] pipe: Option<crate::outbox::PipeHandle>,
    ) -> Self {
        Self::start_with_pipe(
            reader,
            writer,
            Some(pump),
            None,
            #[cfg(any(unix, windows))]
            pipe,
        )
    }

    fn start_with_pipe(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
        pump: Option<Arc<PumpShared>>,
        notif_tx: Option<mpsc::Sender<EngineNotification>>,
        #[cfg(any(unix, windows))] pipe: Option<crate::outbox::PipeHandle>,
    ) -> Self {
        let closed = Arc::new(AtomicBool::new(false));
        let announced_exit = Arc::new(AtomicBool::new(false));
        let settled = Arc::new(Condvar::new());
        let pending: Pending = Arc::new(Mutex::new(PendingState {
            waiters: HashMap::new(),
            closed: Arc::clone(&closed),
        }));
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>();
        let outbox = Arc::new(crate::outbox::Outbox::new(
            Box::new(writer),
            write_tx,
            #[cfg(any(unix, windows))]
            pipe,
        ));

        let writer_pending = Arc::clone(&pending);
        let writer_settled = Arc::clone(&settled);
        let writer_outbox = Arc::clone(&outbox);
        std::thread::spawn(move || {
            while let Ok(bytes) = write_rx.recv() {
                if !writer_outbox.write_from_thread(&bytes) {
                    // the pipe is broken (peer gone, or wedged past
                    // recovery): fail every pending waiter instead of
                    // leaving them to hang on a response that can never
                    // arrive, and reject every future request up front
                    close_and_drain(&writer_pending, &writer_settled);
                    break;
                }
            }
        });

        let reader_pending = Arc::clone(&pending);
        let reader_outbox = Arc::clone(&outbox);
        let reader_pump = pump;
        let reader_announced_exit = Arc::clone(&announced_exit);
        let reader_settled = Arc::clone(&settled);
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(reader);
            let mut fatal_reason: Option<String> = None;
            'read: while let Ok(value) = rmpv::decode::read_value(&mut r) {
                match RpcMessage::from_value(value) {
                    Ok(RpcMessage::Response {
                        msgid,
                        error,
                        result,
                    }) => {
                        let waiter = {
                            let mut p = reader_pending
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner);
                            p.waiters.remove(&msgid)
                        };
                        match waiter {
                            Some(Waiter::Reply(tx)) => {
                                let outcome = if error == Value::Nil {
                                    Ok(result)
                                } else {
                                    Err(EngineError::Remote(error))
                                };
                                let _ = tx.send(outcome);
                            }
                            Some(Waiter::HlProbe { generation }) => {
                                if let Some(pump) = &reader_pump {
                                    // an error reply (e.g. a malformed probe
                                    // params shape) degrades to "confirmed
                                    // unset" rather than leaving this
                                    // generation permanently unresolved: the
                                    // safe default is the terminal's own
                                    // background showing through, not a
                                    // stuck-forever ambiguous state
                                    let (fg, bg) = if error == Value::Nil {
                                        decode_hl_probe_reply(&result)
                                    } else {
                                        (None, None)
                                    };
                                    pump.route_probe_reply(Msg::HlProbeReply {
                                        generation,
                                        fg,
                                        bg,
                                    });
                                }
                            }
                            Some(Waiter::Heartbeat { generation }) => {
                                if let Some(pump) = &reader_pump {
                                    // an error reply still answers the only
                                    // question this probe asks -- the engine
                                    // was reachable enough to reply -- so it
                                    // acknowledges the generation exactly as
                                    // a successful one does rather than
                                    // leaving the watch to time it out as
                                    // silence
                                    pump.route_heartbeat(Msg::HeartbeatReply { generation });
                                }
                            }
                            Some(Waiter::MappingClaims) => {
                                if let Some(pump) = &reader_pump {
                                    // an error reply degrades to "claimed
                                    // nothing" rather than leaving the report
                                    // permanently unanswered: the chunk is
                                    // constant and its arguments are static
                                    // table data, so the only way here is a
                                    // registration that did not happen, and a
                                    // registration that did not happen took
                                    // no key it could name
                                    let claimed = if error == Value::Nil {
                                        decode_mapping_claims(&result)
                                    } else {
                                        Vec::new()
                                    };
                                    pump.route_claims(Msg::MappingsClaimed { claimed });
                                }
                            }
                            Some(Waiter::SwapRecovery { generation }) => {
                                if let Some(pump) = &reader_pump {
                                    // an error reply degrades to "recovered
                                    // nothing, reported nothing, failed at
                                    // nothing", the same safe default every
                                    // decode beside this one takes: the
                                    // expression is constant, so the only way
                                    // here is an engine that could not answer
                                    // at all, and announcing a recovery,
                                    // redrawing over a report or naming a
                                    // failure would each be a claim about a
                                    // session nobody read
                                    let reading = if error == Value::Nil {
                                        decode_swap_recovery_reply(&result)
                                    } else {
                                        SwapRecoveryReading::default()
                                    };
                                    pump.route_swap_recovery(Msg::SwapRecovered {
                                        generation,
                                        count: reading.count,
                                        reported: reading.reported,
                                        failure: reading.failure,
                                    });
                                }
                            }
                            Some(Waiter::BufferList { generation }) => {
                                if let Some(pump) = &reader_pump {
                                    // an error reply degrades to an empty
                                    // buffer list, the same "safe default
                                    // over a stuck generation" precedent
                                    // decode_hl_probe_reply and
                                    // decode_mapping_claims already follow
                                    // -- see
                                    // docs/picker-buffer-list-wire-capture.md
                                    let names = if error == Value::Nil {
                                        decode_buffer_list_reply(&result)
                                    } else {
                                        Vec::new()
                                    };
                                    pump.route_buffer_list(Msg::PickerBufferList {
                                        generation,
                                        names,
                                    });
                                }
                            }
                            Some(Waiter::Preview { generation, path }) => {
                                if let Some(pump) = &reader_pump {
                                    // an error reply degrades to "no buffer
                                    // loaded" (loaded: false), the same
                                    // "safe default over a stuck generation"
                                    // precedent decode_buffer_list_reply and
                                    // decode_hl_probe_reply already follow --
                                    // see docs/picker-preview-wire-capture.md
                                    let (loaded, lines) = if error == Value::Nil {
                                        decode_preview_reply(&result)
                                    } else {
                                        (false, Vec::new())
                                    };
                                    pump.route_preview(Msg::PickerPreviewReply {
                                        generation,
                                        path,
                                        loaded,
                                        lines,
                                    });
                                }
                            }
                            Some(Waiter::Rename { generation }) => {
                                if let Some(pump) = &reader_pump {
                                    // an error reply (including the chunk's
                                    // own explicit refusal to overwrite an
                                    // existing destination -- see
                                    // docs/tree-rename-wire-capture.md)
                                    // degrades to ok: false, the same "safe
                                    // default over a stuck generation"
                                    // precedent every other async reply here
                                    // follows; either way the file and its
                                    // buffer, if any, are exactly where they
                                    // were before this rename was issued
                                    let ok = error == Value::Nil && decode_rename_reply(&result);
                                    pump.route_rename(Msg::TreeRenameReply { generation, ok });
                                }
                            }
                            Some(Waiter::CreatePrompt { generation }) => {
                                if let Some(pump) = &reader_pump {
                                    // an error reply (including the user
                                    // cancelling the prompt, which nvim
                                    // reports as an empty-string result, not
                                    // an error) degrades to `None` the same
                                    // way; either reading means "no file was
                                    // named" and the executor must not create
                                    // one
                                    let name = if error == Value::Nil {
                                        decode_prompt_reply(&result)
                                    } else {
                                        None
                                    };
                                    pump.route_create_prompt(Msg::TreeCreatePromptReply {
                                        generation,
                                        name,
                                    });
                                }
                            }
                            Some(Waiter::RenamePrompt {
                                generation,
                                old_path,
                            }) => {
                                if let Some(pump) = &reader_pump {
                                    // same degrade as `CreatePrompt`: an
                                    // error or a cancelled prompt both mean
                                    // "no new name was given", so the rename
                                    // this would otherwise issue must not
                                    // happen
                                    let name = if error == Value::Nil {
                                        decode_prompt_reply(&result)
                                    } else {
                                        None
                                    };
                                    pump.route_rename_prompt(Msg::TreeRenamePromptReply {
                                        generation,
                                        old_path,
                                        name,
                                    });
                                }
                            }
                            Some(Waiter::DeleteConfirm { generation, path }) => {
                                if let Some(pump) = &reader_pump {
                                    // an error reply degrades to Declined,
                                    // the same safe-default precedent every
                                    // other async reply here follows: a
                                    // delete this decoder cannot confirm the
                                    // user chose must not happen
                                    let outcome = if error == Value::Nil {
                                        decode_delete_confirm_reply(&result)
                                    } else {
                                        DeleteConfirmOutcome::Declined
                                    };
                                    pump.route_delete_confirm(Msg::TreeDeleteConfirmReply {
                                        generation,
                                        path,
                                        outcome,
                                    });
                                }
                            }
                            None => {}
                        }
                    }
                    Ok(RpcMessage::Notification { method, params }) => {
                        if let Some(pump) = &reader_pump {
                            // a pumped connection routes exclusively through
                            // `pump`: nothing else consumes this connection's
                            // notifications, so a method outside this closed
                            // vocabulary is simply dropped rather than routed
                            // anywhere
                            if method == "redraw" {
                                let events = decode_redraw(&params);
                                #[cfg(all(unix, feature = "bench-taps"))]
                                crate::tap::tap(crate::tap::TAG_REDRAW_PARSED);
                                pump.fold_redraw(events);
                            } else if method == "view_invoke" {
                                // best-effort, unlike `view_vim_enter`
                                // below: nvim is not blocked on this one, so
                                // a runtime channel that refuses it costs
                                // the user one keypress, exactly as a
                                // dropped key would, and is not worth
                                // tearing the connection down for
                                if let Some(msg) = decode_feature_invoke(&params) {
                                    let _ = pump.route_msg(msg);
                                }
                            } else if method == "view_bridge" {
                                // best-effort for the same reason
                                // `view_invoke` is: nvim is not blocked on an
                                // autocommand's notification, and every
                                // consumer of one recomputes from live state
                                // on the next frame anyway
                                if let Some(msg) = decode_bridge_event(&params) {
                                    let _ = pump.route_msg(msg);
                                }
                            }
                        } else if let Some(tx) = &notif_tx {
                            if tx.send(EngineNotification { method, params }).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(RpcMessage::Request {
                        msgid,
                        method,
                        params,
                    }) => {
                        if method == "view_leaving" {
                            // recorded and answered here, on the reader
                            // thread, rather than routed: nothing acts on it
                            // while the engine is still up, and by the time
                            // anything asks, the connection is gone and the
                            // loop needs the answer synchronously. Answering
                            // from this thread is also what keeps nvim's exit
                            // off the runtime channel entirely -- a loop that
                            // is wedged, or gone, must not be able to hold an
                            // exiting editor inside `VimLeavePre`.
                            reader_announced_exit.store(true, Ordering::Release);
                            let resp = RpcMessage::Response {
                                msgid,
                                error: Value::Nil,
                                result: Value::Nil,
                            };
                            // a reply that fails to encode is dropped, and
                            // nvim then waits inside `VimLeavePre` until this
                            // thread's own stop routes through
                            // `graceful_kill`'s 500ms backstop -- bounded,
                            // never wedged
                            if let Ok(bytes) = encode_message(&resp) {
                                let _ = reader_outbox.send(bytes);
                            }
                            continue 'read;
                        }
                        let token = ReplyToken {
                            msgid: u64::from(msgid),
                        };
                        let routed = if method == "view_vim_enter" {
                            Some(Msg::EngineRequest(EngineRequest::VimEnter { token }))
                        } else if method == "view_clipboard_get" {
                            decode_clipboard_get(token, &params)
                        } else if method == "view_clipboard_set" {
                            decode_clipboard_set(token, &params)
                        } else {
                            None
                        };
                        if let Some(msg) = routed {
                            if let Some(pump) = &reader_pump {
                                if pump.route_msg(msg).is_err() {
                                    // the runtime loop is gone or wedged
                                    // behind a full channel: no compaction
                                    // can recover a dropped request, and
                                    // leaving it unanswered hangs nvim's
                                    // blocking rpcrequest forever, so stop
                                    // reading rather than keep queuing work
                                    // nothing will ever consume. Carried out
                                    // via Msg::EngineStopped's payload rather
                                    // than a direct stderr write here: this
                                    // thread runs headless behind the
                                    // terminal's raw-mode alternate screen,
                                    // where a write would be invisible or
                                    // corrupt the screen; the caller reports
                                    // it once the terminal is restored.
                                    fatal_reason = Some(format!(
                                        "dropping engine request {method:?}, \
                                         runtime channel gone"
                                    ));
                                    break 'read;
                                }
                                continue 'read;
                            }
                        }
                        // msgpack-RPC obliges a reply to every Request, or
                        // the peer's main loop blocks forever waiting for
                        // one; unknown methods (or no pump attached to
                        // dispatch a known one to) answer with a typed error
                        let resp = RpcMessage::Response {
                            msgid,
                            error: Value::from("method not supported"),
                            result: Value::Nil,
                        };
                        if let Ok(bytes) = encode_message(&resp) {
                            let _ = reader_outbox.send(bytes);
                        }
                    }
                    Err(_) => {
                        // malformed message shape: not fatal on its own: a
                        // future well-formed message can still arrive on
                        // the same connection
                    }
                }
            }
            if let Some(pump) = &reader_pump {
                // blocking send: the reader is already exiting either way,
                // and a dropped EngineStopped is unrecoverable, not merely
                // best-effort (see damage.rs module docs' bounded channel
                // contract)
                pump.route_engine_stopped(fatal_reason);
            }
            // engine is gone: fail every in-flight request instead of hanging
            close_and_drain(&reader_pending, &reader_settled);
        });
        Self {
            next_msgid: Arc::new(AtomicU32::new(1)),
            pending,
            closed,
            announced_exit,
            settled,
            outbox,
        }
    }

    /// Blocks until this connection has published everything it found, or
    /// `timeout` elapses; `true` when it settled.
    ///
    /// Only the paths that resolve a stop need it, and they need it for one
    /// reason: [`announced_exit`](Self::announced_exit) is a *finding*, not
    /// a status. A stop discovered through the reader's own terminal message
    /// already carries the ordering for free (same thread, message after
    /// finding). A stop discovered because a *write* failed does not -- the
    /// writer can lose its pipe while the reader is still draining the
    /// bytes that say why -- and reading the finding then would call an
    /// engine that announced its exit a death and respawn it.
    ///
    /// Bounded rather than unbounded: the caller reaches here having already
    /// reaped the child, so the reader is at EOF and its remaining work is a
    /// drain, but a loop that could block forever on a thread that never
    /// wakes is not a trade a paint loop can make.
    pub fn wait_until_settled(&self, timeout: std::time::Duration) -> bool {
        if self.is_closed() {
            return true;
        }
        let guard = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let (_guard, outcome) = self
            .settled
            .wait_timeout_while(guard, timeout, |p| !p.closed.load(Ordering::Acquire))
            .unwrap_or_else(PoisonError::into_inner);
        !outcome.timed_out()
    }

    /// Whether the engine announced, on this connection, that it was on its
    /// way out -- nvim's own `VimLeavePre`, relayed by the `view_bridge`
    /// group ([`register_bridge`](Self::register_bridge)) as a blocking
    /// request this thread answers itself.
    ///
    /// The platform-neutral evidence that a stop was intentional. An exit
    /// status cannot supply it: a signal death is only *reported* as one on
    /// Unix (`ExitStatusExt`), and even there an engine that exited `1`
    /// because its own Lua config threw is indistinguishable by status from
    /// a user who typed `:cq 1`. What is not ambiguous is nvim saying so
    /// itself, which it does on every ordinary exit path -- `:q`, `:qa`,
    /// `:cq`, `:wq` -- and cannot do when it is killed.
    ///
    /// Ordered before the stop it qualifies, by construction: the reader
    /// thread records this while decoding the request, and the same thread
    /// is the one that later routes `Msg::EngineStopped` when the stream
    /// ends. A loop that has the stop in hand therefore has this answer too.
    ///
    /// Delivered as a request rather than a notification for a reason that
    /// only shows up off Linux: a notification is queued into nvim's event
    /// loop and lost if the process exits before the loop writes it, which
    /// on Windows happens often enough to read roughly one deliberate quit
    /// in three as a crash. A request holds nvim inside `VimLeavePre` until
    /// the reply lands, so the announcement either arrives or the engine
    /// never left.
    #[must_use]
    pub fn announced_exit(&self) -> bool {
        self.announced_exit.load(Ordering::Acquire)
    }

    /// Whether the connection has been marked closed: its peer is gone and
    /// every request from here on fails with
    /// [`EngineError::Closed`] rather than waiting for a reply.
    ///
    /// One atomic load, taking no lock, so a paint loop can afford to ask
    /// on every pass -- and must be able to, since the answer it wants most
    /// is the one a lock-taking version could not give: the connection's
    /// own lock is held by whichever thread is discovering the loss.
    /// Correspondingly one observation stale at worst, never wrong in the
    /// other direction: the flag is set inside the same critical section
    /// that fails every waiter, once the last of them is already out of the
    /// pending map and before any of them is told, and this load acquires
    /// what that store released. So `true` here means no request is still
    /// waiting on a reply that can never arrive, and a caller handed
    /// [`EngineError::Closed`] can never read `false` here afterwards.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Sends a synchronous RPC request to the engine and waits for the response.
    ///
    /// # Arguments
    ///
    /// * `method` - The name of the RPC method to invoke (e.g.,
    ///   `"nvim_get_api_info"`).
    /// * `params` - Positional arguments for the method.
    ///
    /// # Returns
    ///
    /// - `Ok(Value)` with the engine's result (when `error` is nil).
    /// - `Err(EngineError::Remote(error_value))` when the engine returns a
    ///   non-nil error.
    /// - `Err(EngineError::Io(_))` on write errors.
    /// - `Err(EngineError::Closed)` if the engine connection closes before
    ///   the response arrives, including if it was already closed before
    ///   this call started.
    ///
    /// This function blocks until the response is received. A response is
    /// never starved by a flood of notifications on the same connection.
    ///
    /// # msgid wraparound
    ///
    /// The correlation id is a `u32` allocated via a monotonically
    /// increasing counter and wraps at `u32::MAX`. Wraparound is not
    /// guarded against: it takes over four billion requests to occur, which
    /// is acceptable for an interactive editor session and not worth the
    /// extra bookkeeping to prevent.
    pub fn request(&self, method: &str, params: Vec<Value>) -> Result<Value, EngineError> {
        let (_msgid, rx) = self.send_request(method, params)?;
        rx.recv().map_err(|_| EngineError::Closed)?
    }

    /// Sends a synchronous RPC request and waits for the response, but gives
    /// up after `timeout` instead of blocking forever.
    ///
    /// # Arguments
    ///
    /// * `method`, `params` - Same as [`request`](Self::request).
    /// * `timeout` - Maximum time to wait for the response. Bounds the
    ///   entire call, including the time spent writing the request: the
    ///   write happens on a dedicated writer thread fed by a channel, so
    ///   this call never blocks inside a `write()` syscall itself, even
    ///   against a peer that never reads its end of the pipe.
    ///
    /// # Returns
    ///
    /// Same as [`request`](Self::request), plus:
    /// - `Err(EngineError::Timeout { method, timeout })` if no response
    ///   arrives within `timeout`. The pending waiter is removed before
    ///   returning, so a late response from the engine (if one ever arrives)
    ///   is silently dropped by the reader thread rather than leaking the
    ///   waiter for the handle's lifetime.
    ///
    /// A `Timeout` does not mean the request was never sent: the encoded
    /// bytes stay queued for the writer thread, so a peer that stops
    /// reading and later recovers may still receive and execute the call
    /// arbitrarily late. Do not retry side-effectful methods on `Timeout`
    /// assuming the first attempt never happened.
    ///
    /// Use this instead of [`request`](Self::request) for any call where an
    /// unresponsive engine must not hang the caller, e.g. the
    /// `nvim_get_api_info` handshake during [`Engine::spawn`](crate::process::Engine::spawn).
    pub fn request_timeout(
        &self,
        method: &str,
        params: Vec<Value>,
        timeout: Duration,
    ) -> Result<Value, EngineError> {
        let (msgid, rx) = self.send_request(method, params)?;
        match rx.recv_timeout(timeout) {
            Ok(outcome) => outcome,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // the reader thread may still resolve this msgid later; removing
                // the waiter now means that late response is dropped instead of
                // leaking the waiter for the handle's lifetime
                let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
                p.waiters.remove(&msgid);
                drop(p);
                Err(EngineError::Timeout {
                    method: method.to_owned(),
                    timeout,
                })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(EngineError::Closed),
        }
    }

    /// How many messages the writer thread still owes the peer, paired with
    /// how many it has delivered since the connection opened.
    ///
    /// A reading, not a verdict: a queue is only alarming if it stops
    /// draining, which takes a second reading to establish (see
    /// [`OutboxStallWatch`](crate::stall::OutboxStallWatch), which is what
    /// callers should reach for). Lock-free and non-blocking on both loads,
    /// so it answers while the writer is parked inside a write that cannot
    /// finish.
    #[must_use]
    pub fn write_progress(&self) -> (usize, u64) {
        self.outbox.write_progress()
    }

    /// Sends a fire-and-forget notification: encodes and enqueues it on the
    /// writer thread's channel, same as a request, but returns as soon as
    /// it is queued rather than waiting for any reply (notifications have
    /// none). Used for calls where the caller does not need to observe the
    /// result, such as the `qa!` sent during [`Engine`](crate::process::Engine) shutdown.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` only if the writer thread itself has
    /// exited, and any error from encoding the message. It can still
    /// succeed after the reader thread has died (requests already fail with
    /// `Closed` at that point) as long as the writer is alive: shutdown
    /// relies on exactly that window to deliver its final `qa!`.
    pub fn notify(&self, method: &str, params: Vec<Value>) -> Result<(), EngineError> {
        let msg = RpcMessage::Notification {
            method: method.to_owned(),
            params,
        };
        let bytes = encode_message(&msg)?;
        #[cfg(all(unix, feature = "bench-taps"))]
        crate::tap::tap(crate::tap::TAG_RPC_HANDOFF);
        if self.outbox.send(bytes) {
            Ok(())
        } else {
            Err(EngineError::Closed)
        }
    }

    /// Answers a request the engine is blocked on: encodes `[1, msgid, nil,
    /// value]` and enqueues it on the writer thread's channel, same as
    /// [`notify`](Self::notify). Never blocks the caller; the actual write
    /// happens on the writer thread.
    ///
    /// `token` identifies which pending request this answers (see
    /// [`Msg::EngineRequest`]): it
    /// carries the msgid the reader thread captured when it dispatched the
    /// request instead of auto-erroring it. nvim's `rpcrequest` blocks the
    /// engine's main loop until this reply arrives, so a request routed to
    /// `update()` as `Msg::EngineRequest` must always produce exactly one
    /// `reply` call.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Rpc` if `token.msgid` does not fit in the
    /// wire's `u32` msgid (unreachable in practice: every `ReplyToken` this
    /// crate constructs is built from a msgid the reader thread already
    /// decoded as `u32`). Returns `EngineError::Closed` if the writer
    /// thread has already exited.
    pub fn reply(&self, token: ReplyToken, value: ReplyValue) -> Result<(), EngineError> {
        let msgid = u32::try_from(token.msgid)
            .map_err(|_| RpcError::Malformed("reply token exceeds u32 msgid range".into()))?;
        let msg = RpcMessage::Response {
            msgid,
            error: Value::Nil,
            result: reply_value_to_wire(&value),
        };
        let bytes = encode_message(&msg)?;
        if self.outbox.send(bytes) {
            Ok(())
        } else {
            Err(EngineError::Closed)
        }
    }

    /// Allocates a msgid, registers the pending waiter, and enqueues the
    /// encoded request on the writer thread's channel. Shared by
    /// [`request`](Self::request) and
    /// [`request_timeout`](Self::request_timeout), which differ only in how
    /// they wait on the returned receiver.
    fn send_request(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<(u32, mpsc::Receiver<Result<Value, EngineError>>), EngineError> {
        let msgid = self.next_msgid.fetch_add(1, Ordering::Relaxed);
        let msg = RpcMessage::Request {
            msgid,
            method: method.to_owned(),
            params,
        };
        // encode before registering the waiter: an encode error must not
        // leave a map entry behind that only close_and_drain could reclaim
        let bytes = encode_message(&msg)?;
        let (tx, rx) = mpsc::channel();
        {
            let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            if p.closed.load(Ordering::Relaxed) {
                return Err(EngineError::Closed);
            }
            p.waiters.insert(msgid, Waiter::Reply(tx));
        }
        if !self.outbox.send(bytes) {
            // the writer thread is gone, so nothing will ever write this
            // request or fail it on this call's behalf; undo the insert here
            let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            p.waiters.remove(&msgid);
            return Err(EngineError::Closed);
        }
        Ok((msgid, rx))
    }

    /// Issues `method`/`params` as a request, but registers no synchronous
    /// waiter and returns as soon as it is queued for the writer thread --
    /// the async counterpart to [`send_request`](Self::send_request), used
    /// where the caller must never block on a reply (the paint loop). The
    /// eventual `Response` is decoded and routed to the connection's pump as
    /// `Msg::HlProbeReply` (see [`Waiter::HlProbe`]), tagged with
    /// `generation` so a stale reply can be dropped by `update()` rather
    /// than clobbering a newer probe's result. Only meaningful on a pumped
    /// connection ([`Engine::spawn`](crate::process::Engine::spawn)'s only
    /// production connection kind); on a bare [`EngineHandle::start`]
    /// connection (test-only), the reply is decoded but has nowhere to
    /// route, so it is silently dropped once it arrives.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited; the request is never written in
    /// either case.
    pub fn request_probe(
        &self,
        method: &str,
        params: Vec<Value>,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.request_async(method, params, Waiter::HlProbe { generation })
    }

    /// Issues `nvim_get_mode` as a fire-and-forget liveness probe tagged
    /// with `generation`, whose acknowledgement is routed to the
    /// connection's pump as `Msg::HeartbeatReply` (see
    /// [`Waiter::Heartbeat`]). Async on the same terms as
    /// [`request_probe`](Self::request_probe).
    ///
    /// `nvim_get_mode` rather than any other method because the pinned
    /// engine answers it on receipt even while its main loop is blocked
    /// waiting for a key (see [`get_mode`](Self::get_mode)), so the states
    /// in which an engine is deliberately waiting read as answering rather
    /// than as silence.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited; the probe is never written in
    /// either case.
    pub fn request_heartbeat(&self, generation: u64) -> Result<(), EngineError> {
        self.request_async("nvim_get_mode", vec![], Waiter::Heartbeat { generation })
    }

    /// Issues `method`/`params` as a request whose `Response` is decoded
    /// into every claimed key and routed to the connection's pump as
    /// `Msg::MappingsClaimed` (see [`Waiter::MappingClaims`]). Async on the
    /// same terms as [`request_probe`](Self::request_probe): the caller is
    /// the runtime loop, which must never block on a reply.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited; the request is never written in
    /// either case.
    pub fn request_mappings(&self, method: &str, params: Vec<Value>) -> Result<(), EngineError> {
        self.request_async(method, params, Waiter::MappingClaims)
    }

    /// Issues `method`/`params` as a request whose `Response` is decoded into
    /// a swap-recovery reading and routed to the connection's pump as
    /// `Msg::SwapRecovered`, tagged with `generation` (see
    /// [`Waiter::SwapRecovery`]). Async on the same terms as
    /// [`request_probe`](Self::request_probe).
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited; the request is never written in
    /// either case.
    pub fn request_swap_recovery(
        &self,
        method: &str,
        params: Vec<Value>,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.request_async(method, params, Waiter::SwapRecovery { generation })
    }

    /// Issues `method`/`params` as a request whose `Response` is decoded
    /// into the picker's buffer-list corpus and routed to the connection's
    /// pump as `Msg::PickerBufferList` (see [`Waiter::BufferList`]). Async
    /// on the same terms as [`request_probe`](Self::request_probe):
    /// `Source::Buffers`'s picker query is issued from the runtime loop,
    /// which must never block on a reply.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited; the request is never written in
    /// either case.
    pub fn request_buffer_list(
        &self,
        method: &str,
        params: Vec<Value>,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.request_async(method, params, Waiter::BufferList { generation })
    }

    /// Issues `method`/`params` as a request whose `Response` is decoded
    /// into the picker preview pane's text for `path` and routed to the
    /// connection's pump as `Msg::PickerPreviewReply` (see
    /// [`Waiter::Preview`]). Async on the same terms as
    /// [`request_probe`](Self::request_probe). Takes `path` in addition to
    /// `generation` (unlike [`request_buffer_list`](Self::request_buffer_list)):
    /// the eventual reply carries no path of its own, so the waiter must
    /// hold it to echo back.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited; the request is never written in
    /// either case.
    pub fn request_preview(
        &self,
        method: &str,
        params: Vec<Value>,
        generation: u64,
        path: String,
    ) -> Result<(), EngineError> {
        self.request_async(method, params, Waiter::Preview { generation, path })
    }

    /// Issues `method`/`params` as a request whose `Response` is decoded
    /// into a rename's success flag and routed to the connection's pump as
    /// `Msg::TreeRenameReply` (see [`Waiter::Rename`]). Async on the same
    /// terms as [`request_probe`](Self::request_probe): the tree overlay's
    /// rename is issued from the runtime loop, which must never block on a
    /// reply.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited; the request is never written in
    /// either case.
    pub fn request_rename(
        &self,
        method: &str,
        params: Vec<Value>,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.request_async(method, params, Waiter::Rename { generation })
    }

    /// Issues `method`/`params` as a request whose `Response` is decoded
    /// into the typed name (or `None` on cancel) and routed to the
    /// connection's pump as `Msg::TreeCreatePromptReply` (see
    /// [`Waiter::CreatePrompt`]). Async on the same terms as
    /// [`request_probe`](Self::request_probe): the tree overlay's create
    /// prompt is issued from the runtime loop, which must never block on a
    /// reply.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited; the request is never written in
    /// either case.
    pub fn request_create_prompt(
        &self,
        method: &str,
        params: Vec<Value>,
        generation: u64,
    ) -> Result<(), EngineError> {
        self.request_async(method, params, Waiter::CreatePrompt { generation })
    }

    /// Issues `method`/`params` as a request whose `Response` is decoded
    /// into the typed name (or `None` on cancel) and routed to the
    /// connection's pump as `Msg::TreeRenamePromptReply` (see
    /// [`Waiter::RenamePrompt`]). Async on the same terms as
    /// [`request_probe`](Self::request_probe). Takes `old_path` in addition
    /// to `generation` (the same reason [`request_preview`](Self::request_preview)
    /// takes `path`): the eventual reply carries only the typed name, and the
    /// rename this prompt exists to gather input for needs to know what it
    /// is renaming from.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited; the request is never written in
    /// either case.
    pub fn request_rename_prompt(
        &self,
        method: &str,
        params: Vec<Value>,
        generation: u64,
        old_path: String,
    ) -> Result<(), EngineError> {
        self.request_async(
            method,
            params,
            Waiter::RenamePrompt {
                generation,
                old_path,
            },
        )
    }

    /// Issues `method`/`params` as a request whose `Response` is decoded
    /// into a confirmed flag and routed to the connection's pump as
    /// `Msg::TreeDeleteConfirmReply` (see [`Waiter::DeleteConfirm`]). Async
    /// on the same terms as [`request_probe`](Self::request_probe). Takes
    /// `path` in addition to `generation` (the same reason
    /// [`request_preview`](Self::request_preview) takes `path`): the
    /// eventual reply carries only the chosen button index, and the delete
    /// this confirmation exists to gate needs to know which file it applies
    /// to.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited; the request is never written in
    /// either case.
    pub fn request_delete_confirm(
        &self,
        method: &str,
        params: Vec<Value>,
        generation: u64,
        path: String,
    ) -> Result<(), EngineError> {
        self.request_async(method, params, Waiter::DeleteConfirm { generation, path })
    }

    /// Allocates a msgid, registers `waiter`, and enqueues the encoded
    /// request, without a synchronous receiver for anything to block on.
    /// Shared by every async request wrapper; how the eventual `Response` is
    /// decoded and where it is routed is the `waiter`'s to say.
    fn request_async(
        &self,
        method: &str,
        params: Vec<Value>,
        waiter: Waiter,
    ) -> Result<(), EngineError> {
        let msgid = self.next_msgid.fetch_add(1, Ordering::Relaxed);
        let msg = RpcMessage::Request {
            msgid,
            method: method.to_owned(),
            params,
        };
        let bytes = encode_message(&msg)?;
        {
            let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            if p.closed.load(Ordering::Relaxed) {
                return Err(EngineError::Closed);
            }
            p.waiters.insert(msgid, waiter);
        }
        if !self.outbox.send(bytes) {
            let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            p.waiters.remove(&msgid);
            return Err(EngineError::Closed);
        }
        Ok(())
    }
}

/// Decodes a `view_invoke` notification's `(feature, verb)` positional
/// params into [`Msg::FeatureInvoke`], or `None` when the notification does
/// not carry that pair.
///
/// The pair is not validated here: nvim is where a user types `:View`
/// followed by any two words, so deciding an unknown pair is not actionable
/// belongs to the one arm that knows what this build can act on, not to the
/// reader thread.
fn decode_feature_invoke(params: &[Value]) -> Option<Msg> {
    let [feature, verb, ..] = params else {
        return None;
    };
    Some(Msg::FeatureInvoke {
        feature: feature.as_str()?.to_owned(),
        verb: verb.as_str()?.to_owned(),
    })
}

/// Decodes a `"+p`/`"*p` paste's `(register)` positional param into the
/// message the loop routes to `update()`. `register` must decode to exactly
/// one `char` (`'+'` or `'*'`); anything else falls through to the reader
/// thread's generic "method not supported" response rather than guessing a
/// register, which would silently answer a paste from the wrong clipboard.
fn decode_clipboard_get(token: ReplyToken, params: &[Value]) -> Option<Msg> {
    let [register, ..] = params else {
        return None;
    };
    let register = register.as_str()?.chars().next()?;
    Some(Msg::EngineRequest(EngineRequest::ClipboardGet {
        token,
        register,
    }))
}

/// Decodes a `"+yy`/`"*yy` copy's `(register, lines, regtype)` positional
/// params -- `regtype` is the injected `copy` closure's second argument,
/// forwarded by [`REGISTER_CLIPBOARD_CHUNK`] alongside `lines`. `lines`
/// must decode to an array of strings in full -- one undecodable line
/// drops the whole request rather than silently copying a truncated
/// selection. `regtype` must be present and a string, or the request drops
/// the same way, but an unrecognized string (a blockwise regtype this
/// provider does not keep, see [`RegisterType`]) degrades to
/// [`RegisterType::Charwise`] rather than dropping the request: the copy
/// itself is never in question, only which trailing-newline convention
/// its text gets.
fn decode_clipboard_set(token: ReplyToken, params: &[Value]) -> Option<Msg> {
    let [register, lines, regtype, ..] = params else {
        return None;
    };
    let register = register.as_str()?.chars().next()?;
    let lines = lines
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()?;
    let regtype = RegisterType::from_nvim(regtype.as_str()?);
    Some(Msg::EngineRequest(EngineRequest::ClipboardSet {
        token,
        register,
        lines,
        regtype,
    }))
}

/// Decodes a `view_bridge` notification's `(event, ...payload)` positional
/// params into the message its consumer reads, or `None` when this build
/// has no consumer for the event. Each event names its own payload shape
/// (see [`crate::nvim_api::REGISTER_BRIDGE_CHUNK`]'s doc): `colorscheme`
/// carries the scheme's name alone, `diagnostics` an `(errors, warnings)`
/// count pair, `git` the branch name alone, and `buffer` a `(name,
/// modified)` pair.
///
/// The bridge deliberately carries more triggers than there are consumers
/// today: the group is registered once, and adding a consumer must not mean
/// re-registering autocommands on a running engine. An event with no
/// consumer costs one dropped notification here.
fn decode_bridge_event(params: &[Value]) -> Option<Msg> {
    let [event, first, rest @ ..] = params else {
        return None;
    };
    match event.as_str()? {
        "colorscheme" => Some(Msg::ColorSchemeChanged {
            name: first.as_str().unwrap_or_default().to_owned(),
        }),
        "diagnostics" => {
            let errors = saturate_u32(first.as_u64()?);
            let warnings = saturate_u32(rest.first()?.as_u64()?);
            Some(Msg::DiagnosticsChanged { errors, warnings })
        }
        "git" => Some(Msg::GitBranchChanged {
            branch: first.as_str().unwrap_or_default().to_owned(),
        }),
        "buffer" => {
            let name = first.as_str()?.to_owned();
            let modified = rest.first()?.as_bool()?;
            Some(Msg::BufferChanged { name, modified })
        }
        _ => None,
    }
}

/// Saturates a wire `u64` count into `u32`, clamping to `u32::MAX` instead
/// of truncating, matching `view_core::events::saturate_u16`'s convention
/// for every other untrusted wire integer this crate decodes.
fn saturate_u32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// Decodes a mapping registration's reply: an array of `{feature, lhs,
/// had_user_mapping}` rows, one per key the chunk registered, in
/// registration order.
///
/// A row missing `feature` or `lhs` is dropped rather than reported as a
/// claim naming nothing, and a missing `had_user_mapping` reads as `false`:
/// the flag is what promotes a claim to news, so an undecodable one must not
/// invent an announcement about a user's key.
fn decode_mapping_claims(result: &Value) -> Vec<MappingClaim> {
    let Some(rows) = result.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let pairs = row.as_map()?;
            Some(MappingClaim {
                feature: crate::wire::map_find(pairs, "feature")?
                    .as_str()?
                    .to_owned(),
                lhs: crate::wire::map_find(pairs, "lhs")?.as_str()?.to_owned(),
                had_user_mapping: crate::wire::map_find(pairs, "had_user_mapping")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Decodes a buffer-list reply into each listed buffer's `name`, dropping
/// `bufnr`/`modified` (the picker's `Source::Buffers` corpus is a plain path
/// list; nothing here orders or annotates by either field -- see
/// `docs/picker-buffer-list-wire-capture.md`). A row missing `name` is
/// dropped: an unnamed scratch buffer still round-trips, since nvim replies
/// `name = ""` for one rather than omitting the key (capture #1), so a
/// missing key here can only mean a row shape this crate has never actually
/// seen from the pinned engine.
fn decode_buffer_list_reply(result: &Value) -> Vec<String> {
    let Some(rows) = result.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let pairs = row.as_map()?;
            Some(crate::wire::map_find(pairs, "name")?.as_str()?.to_owned())
        })
        .collect()
}

/// What one connection answered about the recovery it performed while
/// starting, as decoded from [`SWAP_RECOVERY_PROBE`].
///
/// [`SWAP_RECOVERY_PROBE`]: crate::process::SWAP_RECOVERY_PROBE
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SwapRecoveryReading {
    /// Buffers that came back holding work the file on disk does not have.
    count: u64,
    /// Whether the engine wrote a recovery report on screen at all.
    reported: bool,
    /// The engine's own error text when the recovery it was asked for could
    /// not be performed, `None` when it went through.
    failure: Option<String>,
}

/// Decodes [`SWAP_RECOVERY_PROBE`]'s three-element answer.
///
/// A shape this crate has never seen from the pinned engine degrades to the
/// default -- the same "absent or malformed is exactly as informative as an
/// explicit nothing" precedent [`decode_hl_probe_reply`] follows, and the
/// conservative direction for all three: no notice claiming a recovery that
/// was not read, no redraw over a report that may not be there, and no
/// failure attributed to an engine that did not report one.
///
/// [`SWAP_RECOVERY_PROBE`]: crate::process::SWAP_RECOVERY_PROBE
fn decode_swap_recovery_reply(result: &Value) -> SwapRecoveryReading {
    let Some(fields) = result.as_array() else {
        return SwapRecoveryReading::default();
    };
    SwapRecoveryReading {
        count: fields.first().and_then(Value::as_u64).unwrap_or(0),
        // vimscript has no boolean type: `||` and a comparison both answer
        // with the numbers 0 and 1, which is what arrives here
        reported: fields.get(1).and_then(Value::as_u64).unwrap_or(0) != 0,
        failure: fields
            .get(2)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
    }
}

/// Decodes a picker-preview reply's `loaded`/`lines` keys, live-verified
/// against a real `nvim --clean --headless` (see
/// `docs/picker-preview-wire-capture.md`): `loaded: false` carries no
/// `lines` key at all, so `lines` is read only once `loaded` is confirmed
/// `true`, and a non-map/malformed `result` this crate has not actually seen
/// from the pinned engine degrades to `(false, [])` -- the same "absent or
/// malformed is exactly as informative as an explicit false" precedent
/// `decode_hl_probe_reply` follows.
fn decode_preview_reply(result: &Value) -> (bool, Vec<String>) {
    let Some(pairs) = result.as_map() else {
        return (false, Vec::new());
    };
    let loaded = crate::wire::map_find(pairs, "loaded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !loaded {
        return (false, Vec::new());
    }
    let lines = crate::wire::map_find(pairs, "lines")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (true, lines)
}

/// Decodes a rename reply's `ok` key, live-verified against a real `nvim
/// --clean --headless` (see `docs/tree-rename-wire-capture.md`): the chunk
/// returns `{ ok = true }` on a successful `vim.fn.rename` plus buffer
/// retarget, and `{ ok = false }` when it refused to overwrite an existing
/// destination. A non-map/malformed `result` this crate has not actually
/// seen from the pinned engine degrades to `false` -- the same "absent or
/// malformed is exactly as informative as an explicit false" precedent
/// `decode_hl_probe_reply` follows, and the safe reading here besides: a
/// rename this decoder cannot confirm succeeded must not trigger the rescan
/// that assumes it did.
fn decode_rename_reply(result: &Value) -> bool {
    result
        .as_map()
        .and_then(|pairs| crate::wire::map_find(pairs, "ok"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Decodes a tree create/rename prompt's reply, live-verified against a real
/// `nvim --clean --headless` (see `docs/tree-input-prompt-wire-capture.md`):
/// `vim.fn.input` replies with a bare string, not a map -- an empty string
/// both on an outright `<Esc>` cancel and on the user submitting a blank
/// answer, neither of which names a file to create or rename to, so both
/// degrade to `None` the same way a non-string `result` this crate has not
/// actually seen from the pinned engine does.
fn decode_prompt_reply(result: &Value) -> Option<String> {
    result.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Decodes a tree delete confirmation's reply, live-verified against a real
/// `nvim --clean --headless` (see `docs/tree-input-prompt-wire-capture.md`):
/// `TREE_DELETE_CONFIRM_CHUNK` returns `{ buffer_open = true }` when it
/// refused to even offer the prompt, or otherwise `{ choice = N }`, `N`
/// being `vim.fn.confirm`'s own 1-based button index per `:help confirm()`
/// -- `1` for the first (`&Yes`), `2` for the second (`&No`), and `0` when
/// the dialog was force-closed (e.g. `<C-c>`) without a choice. Only an
/// explicit `choice = 1` confirms; every other shape, including a
/// non-table `result` this crate has not actually seen from the pinned
/// engine, degrades to `Declined` -- the same "absent or malformed is
/// exactly as informative as an explicit false" precedent `decode_rename_reply`
/// follows, and the safe reading here besides: a delete this decoder cannot
/// confirm the user chose must not happen.
fn decode_delete_confirm_reply(result: &Value) -> DeleteConfirmOutcome {
    let Some(map) = result.as_map() else {
        return DeleteConfirmOutcome::Declined;
    };
    if crate::wire::map_find(map, "buffer_open").and_then(Value::as_bool) == Some(true) {
        return DeleteConfirmOutcome::BufferOpen;
    }
    if crate::wire::map_find(map, "choice").and_then(Value::as_i64) == Some(1) {
        return DeleteConfirmOutcome::Confirmed;
    }
    DeleteConfirmOutcome::Declined
}

/// Decodes an `nvim_get_hl(0, {name = "Normal"})` reply's `fg`/`bg` map
/// keys, live-verified against a real `nvim --embed`: a transparent
/// `Normal` (`hi Normal guifg=#f8f8f2`, no `guibg`) replies `{fg =
/// 16316658}` with no `bg` key at all; an explicit background (`hi Normal
/// guibg=#282a36`) replies `{fg = 16316658, bg = 2632246}` with both
/// present. A key's absence, not a sentinel value, is what disambiguates
/// "unset" from "genuinely zero" -- the exact ambiguity `default_colors_set`
/// alone cannot resolve (see [`view_core::msg::RpcCall::GetDefaultHl`]).
/// `result` shapes this crate has not seen from a real `nvim_get_hl`
/// (non-map, or present keys of an unexpected wire type) degrade to `None`
/// for that channel rather than erroring: a malformed reply is exactly as
/// informative as an absent key for this probe's purposes.
fn decode_hl_probe_reply(result: &Value) -> (Option<u32>, Option<u32>) {
    let Some(map) = result.as_map() else {
        return (None, None);
    };
    let get = |key: &str| {
        crate::wire::map_find(map, key)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
    (get("fg"), get("bg"))
}

/// Marks the connection closed and drains every pending waiter with
/// [`EngineError::Closed`] in one critical section, so a `send_request`
/// racing this call either lands before that section (and gets drained
/// here) or observes `closed == true` and never inserts at all. An
/// `HlProbe` waiter has nothing to send a `Closed` error to (nothing is
/// blocked on it), so it is dropped silently -- its `Msg::HlProbeReply`
/// simply never arrives, which `update()` already treats identically to any
/// other reply that never lands (the pre-probe fallback in
/// `Theme::from_hl`).
fn close_and_drain(pending: &Pending, settled: &Condvar) {
    let mut p = pending.lock().unwrap_or_else(PoisonError::into_inner);
    let doomed: Vec<Waiter> = p.waiters.drain().map(|(_, waiter)| waiter).collect();
    // flipped between taking the waiters out of the map and failing them,
    // with a release paired against [`EngineHandle::is_closed`]'s acquire,
    // so both readings of the flag hold: nothing is left waiting on a reply
    // once an observer can see `true`, and a caller handed `Closed` can
    // never look at the flag and be told the connection is fine. Ordering
    // the two the other way round makes one of those false whichever way it
    // is written, and both are load-bearing -- the first for an observer
    // deciding the connection is gone, the second for the test that pins
    // it. Still one critical section, so the insert-versus-drain race the
    // shared lock exists to close is unaffected.
    p.closed.store(true, Ordering::Release);
    for waiter in doomed {
        if let Waiter::Reply(tx) = waiter {
            let _ = tx.send(Err(EngineError::Closed));
        }
    }
    drop(p);
    settled.notify_all();
}

// `ReplyValue` is `#[non_exhaustive]` from view-core (rmpv-free by design,
// per the crate dependency direction `scripts/audit-deps.sh` enforces), so
// the wildcard arm is required for a future variant to compile against, not
// reachable with today's variants.
fn reply_value_to_wire(value: &ReplyValue) -> Value {
    match value {
        ReplyValue::Nil => Value::Nil,
        // The `[lines, regtype]` pair form, not a bare list: the wire
        // capture (`docs/clipboard-provider-wire-capture.md`) confirmed
        // nvim's `paste` closure accepts this shape and it is the only one
        // that lets a linewise `"+p` restore the register type a bare list
        // would silently collapse to charwise.
        ReplyValue::ClipboardLines { lines, regtype } => Value::Array(vec![
            Value::Array(lines.iter().map(|l| Value::from(l.as_str())).collect()),
            Value::from(regtype.as_nvim_str()),
        ]),
        #[allow(unreachable_patterns)]
        other => {
            // A future `ReplyValue` variant view-core adds compiles
            // against this wildcard (see the arm's own doc comment above)
            // without anything forcing this function to be updated for
            // it -- fail loud where that omission is cheap to catch, and
            // fall back to the already-safe `Nil` reply in release rather
            // than crash a running engine over an encoding gap.
            debug_assert!(
                false,
                "reply_value_to_wire has no wire encoding for {other:?}; add one"
            );
            Value::Nil
        }
    }
}

fn encode_message(msg: &RpcMessage) -> Result<Vec<u8>, EngineError> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &msg.to_value())
        .map_err(|e| EngineError::Io(std::io::Error::other(e)))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::rpc::RpcMessage;
    use rmpv::Value;

    fn fake_peer(
        mut respond: impl FnMut(u32, &str) -> RpcMessage + Send + 'static,
    ) -> (EngineHandle, std::sync::mpsc::Receiver<EngineNotification>) {
        let (peer_read, our_write) = std::io::pipe().unwrap();
        let (our_read, mut peer_write) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(peer_read);
            while let Ok(v) = rmpv::decode::read_value(&mut r) {
                if let Ok(RpcMessage::Request { msgid, method, .. }) = RpcMessage::from_value(v) {
                    let reply = respond(msgid, &method);
                    rmpv::encode::write_value(&mut peer_write, &reply.to_value()).unwrap();
                    peer_write.flush().unwrap();
                }
            }
        });
        EngineHandle::start(our_read, our_write)
    }

    /// A pumped [`EngineHandle`] over raw pipes, plus the raw ends for a
    /// test to act as the peer (nvim): write a `Request`/`Notification`
    /// into `peer_write`, read whatever the handle writes back off
    /// `peer_read`.
    fn pumped_peer() -> (
        EngineHandle,
        Arc<PumpShared>,
        std::io::PipeReader,
        std::io::PipeWriter,
    ) {
        let (peer_read, our_write) = std::io::pipe().unwrap();
        let (our_read, peer_write) = std::io::pipe().unwrap();
        let pump = PumpShared::new();
        let h = EngineHandle::start_pumped(
            our_read,
            our_write,
            Arc::clone(&pump),
            #[cfg(any(unix, windows))]
            None,
        );
        (h, pump, peer_read, peer_write)
    }

    fn write_request(peer_write: &mut impl Write, msgid: u32, method: &str) {
        let req = RpcMessage::Request {
            msgid,
            method: method.to_owned(),
            params: vec![],
        };
        rmpv::encode::write_value(peer_write, &req.to_value()).unwrap();
        peer_write.flush().unwrap();
    }

    #[test]
    fn view_vim_enter_request_surfaces_as_engine_request_and_reply_correlates() {
        let (h, pump, peer_read, mut peer_write) = pumped_peer();
        let (tx, rx) = mpsc::sync_channel(64);
        let _dpump = pump.attach_sink(tx);

        write_request(&mut peer_write, 42, "view_vim_enter");

        let msg = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let Msg::EngineRequest(EngineRequest::VimEnter { token }) = msg else {
            unreachable!("expected Msg::EngineRequest(VimEnter), got {msg:?}");
        };
        assert_eq!(token.msgid, 42);

        h.reply(token, ReplyValue::Nil).unwrap();
        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        assert_eq!(
            RpcMessage::from_value(v).unwrap(),
            RpcMessage::Response {
                msgid: 42,
                error: Value::Nil,
                result: Value::Nil,
            }
        );
    }

    /// The announcement is answered by the reader thread itself, with no
    /// sink attached at all: nvim blocks inside `VimLeavePre` until this
    /// reply lands, so routing it through the runtime channel would let a
    /// wedged -- or already gone -- loop hold an exiting editor open.
    #[test]
    fn an_exit_announcement_is_answered_by_the_reader_not_the_runtime_loop() {
        let (h, _pump, peer_read, mut peer_write) = pumped_peer();

        assert!(!h.announced_exit(), "nothing had announced anything yet");
        write_request(&mut peer_write, 11, "view_leaving");

        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        assert_eq!(
            RpcMessage::from_value(v).unwrap(),
            RpcMessage::Response {
                msgid: 11,
                error: Value::Nil,
                result: Value::Nil,
            },
            "an announcement answered with an error leaves nvim's exit \
             reporting a failure it did not have"
        );
        assert!(
            h.announced_exit(),
            "the announcement was answered without being recorded"
        );
    }

    /// The ordering the whole supervision decision rests on: whoever holds
    /// the stop holds the announcement that qualifies it, because one
    /// thread produces both in that order.
    #[test]
    fn a_stop_that_followed_an_announcement_carries_it() {
        let (h, pump, _peer_read, mut peer_write) = pumped_peer();
        let (tx, rx) = mpsc::sync_channel(64);
        let _dpump = pump.attach_sink(tx);

        write_request(&mut peer_write, 12, "view_leaving");
        drop(peer_write);

        loop {
            let msg = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            if matches!(msg, Msg::EngineStopped { .. }) {
                break;
            }
        }
        assert!(
            h.announced_exit(),
            "the stop arrived without the announcement that preceded it on \
             the same thread"
        );
    }

    #[test]
    fn unknown_method_still_auto_errors_with_a_pump_attached() {
        let (_h, pump, peer_read, mut peer_write) = pumped_peer();
        let (tx, _rx) = mpsc::sync_channel::<Msg>(64);
        let _dpump = pump.attach_sink(tx);

        write_request(&mut peer_write, 7, "some_other_method");

        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        assert_eq!(
            RpcMessage::from_value(v).unwrap(),
            RpcMessage::Response {
                msgid: 7,
                error: Value::from("method not supported"),
                result: Value::Nil,
            }
        );
    }

    /// Every other test in this module exercises the reader thread's
    /// `route_msg` success path; this one forces the fallible branch a real
    /// `view_vim_enter` request hits when the runtime channel has no room,
    /// so a regression that stopped constructing `fatal_reason` there would
    /// fail this test instead of only ever taking the untested `None` path.
    ///
    /// Fullness is test-owned: an ordinary barrier request/reply round trip
    /// first proves the reader thread is alive and blocked in
    /// `read_value()` waiting for the next message (not merely spawned and
    /// not yet scheduled), then the dummy fill and the `view_vim_enter`
    /// write happen in the test thread's own program order, so the channel
    /// is guaranteed full at the moment the reader thread starts decoding
    /// the new bytes. The one gap this cannot close mechanically is the
    /// reader thread's own decode-plus-`try_send` versus this test's
    /// drain: nothing in `std::sync::mpsc` exposes "is a receiver about to
    /// call `recv`", so the brief wait below (this file's own established
    /// pattern, see `requests_before_start_pump_stage_and_drain_in_arrival_order`)
    /// gives that near-instant attempt (one decode, one mutex, one
    /// `try_send`) a wide, deliberately generous margin before the drain
    /// can free the slot back up.
    #[test]
    fn full_channel_on_view_vim_enter_delivers_fatal_reason_naming_the_method() {
        let (_h, pump, peer_read, mut peer_write) = pumped_peer();
        let (tx, rx) = mpsc::sync_channel::<Msg>(1);
        let _dpump = pump.attach_sink(tx.clone());

        write_request(&mut peer_write, 1, "barrier_method");
        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        assert_eq!(
            RpcMessage::from_value(v).unwrap(),
            RpcMessage::Response {
                msgid: 1,
                error: Value::from("method not supported"),
                result: Value::Nil,
            },
            "barrier request must get the ordinary auto-reply before the reader is trusted \
             to be idle and waiting for the next message"
        );

        tx.try_send(Msg::Resized {
            width: 1,
            height: 1,
        })
        .expect("channel has capacity for the dummy fill");
        write_request(&mut peer_write, 99, "view_vim_enter");
        std::thread::sleep(Duration::from_millis(50));

        assert!(
            matches!(
                rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                Msg::Resized { .. }
            ),
            "dummy fill must be the first message drained"
        );
        let stopped = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("EngineStopped must arrive once the dummy is drained");
        let Msg::EngineStopped {
            reason: Some(reason),
            ..
        } = stopped
        else {
            unreachable!("expected EngineStopped carrying a reason, got {stopped:?}");
        };
        assert!(
            reason.contains("view_vim_enter"),
            "fatal reason must name the method that could not be routed: {reason}"
        );
    }

    #[test]
    fn requests_before_start_pump_stage_and_drain_in_arrival_order() {
        let (_h, pump, _peer_read, mut peer_write) = pumped_peer();

        // both requests arrive before any sink is attached: startup
        // registers the VimEnter autocmd before the runtime loop starts,
        // so a fast peer can fire more than one before start_pump runs
        write_request(&mut peer_write, 1, "view_vim_enter");
        write_request(&mut peer_write, 2, "view_vim_enter");
        // give the reader thread a chance to actually decode both before
        // the sink attaches, so this exercises the pre-sink FIFO and not a
        // race that happens to land after attach
        std::thread::sleep(Duration::from_millis(50));

        let (tx, rx) = mpsc::sync_channel(64);
        let (_dpump, cutover) = pump.attach_sink(tx);

        let msgid_of = |m: &Msg| match m {
            Msg::EngineRequest(EngineRequest::VimEnter { token }) => token.msgid,
            other => unreachable!("expected Msg::EngineRequest(VimEnter), got {other:?}"),
        };
        assert_eq!(cutover.presink.len(), 2);
        assert_eq!(
            msgid_of(&cutover.presink[0]),
            1,
            "arrival order not preserved"
        );
        assert_eq!(
            msgid_of(&cutover.presink[1]),
            2,
            "arrival order not preserved"
        );
        // attach_sink returns staged state instead of sending it: nothing
        // ever reaches a channel with no consumer yet
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn request_gets_matching_response() {
        let (h, _n) = fake_peer(|msgid, method| RpcMessage::Response {
            msgid,
            error: Value::Nil,
            result: Value::from(method.to_owned()),
        });
        let out = h.request("nvim_get_api_info", vec![]).unwrap();
        assert_eq!(out, Value::from("nvim_get_api_info"));
    }

    #[test]
    fn remote_error_surfaces_as_engine_error() {
        let (h, _n) = fake_peer(|msgid, _| RpcMessage::Response {
            msgid,
            error: Value::from("boom"),
            result: Value::Nil,
        });
        assert!(matches!(
            h.request("x", vec![]),
            Err(EngineError::Remote(_))
        ));
    }

    /// Reproduces a critical hang: the reader thread can exit (here,
    /// because its notification receiver was dropped, causing the very
    /// next `notif_tx.send` to fail) while the peer connection
    /// itself is still open and healthy. A `request()` issued after that
    /// point must observe `Closed` instead of blocking forever waiting for
    /// a response nothing will ever deliver. Against the pre-fix code
    /// (no `closed` flag shared with the pending map), this test hangs
    /// past the 2s watchdog budget instead of failing normally.
    #[test]
    fn request_after_reader_exit_returns_closed_not_hang() {
        let (peer_read, our_write) = std::io::pipe().unwrap();
        let (our_read, mut peer_write) = std::io::pipe().unwrap();
        // peer thread: keep draining whatever the handle writes so that no
        // write ever blocks on a full pipe; the peer never answers anything
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(peer_read);
            while rmpv::decode::read_value(&mut r).is_ok() {}
        });

        let (h, n) = EngineHandle::start(our_read, our_write);
        // drop the receiver before any notification is ever sent, so the
        // reader thread's first forwarding attempt after this point is
        // guaranteed to observe a disconnected channel
        drop(n);

        let notif = RpcMessage::Notification {
            method: "redraw".into(),
            params: vec![],
        };
        rmpv::encode::write_value(&mut peer_write, &notif.to_value()).unwrap();
        peer_write.flush().unwrap();

        // request() has no timeout of its own; run it on a watchdog thread
        // so a regression to the old hang fails this test loudly (after 2s)
        // instead of stalling the whole suite forever
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(h.request("nvim_get_api_info", vec![]));
        });
        let outcome = rx.recv_timeout(std::time::Duration::from_secs(2));
        assert!(
            outcome.is_ok(),
            "request() hung after reader exit instead of returning Closed"
        );
        assert!(
            matches!(outcome.unwrap(), Err(EngineError::Closed)),
            "expected Closed after reader exit"
        );
    }

    /// An incoming `Request` from the peer (e.g. a blocking
    /// `rpcrequest` from nvim's init.lua) must get an immediate reply, or
    /// the peer's main loop blocks forever waiting for one and every
    /// subsequent call from this side deadlocks against it. Incoming
    /// requests are not dispatched to real handlers; the reply is a typed
    /// "method not supported" error.
    #[test]
    fn incoming_request_gets_method_not_supported_response() {
        let (peer_read, our_write) = std::io::pipe().unwrap();
        let (our_read, mut peer_write) = std::io::pipe().unwrap();
        let (h, _n) = EngineHandle::start(our_read, our_write);

        let req = RpcMessage::Request {
            msgid: 42,
            method: "some_client_bound_call".into(),
            params: vec![],
        };
        rmpv::encode::write_value(&mut peer_write, &req.to_value()).unwrap();
        peer_write.flush().unwrap();

        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        let resp = RpcMessage::from_value(v).unwrap();
        assert_eq!(
            resp,
            RpcMessage::Response {
                msgid: 42,
                error: Value::from("method not supported"),
                result: Value::Nil,
            }
        );
        // keep the handle alive for the duration of the read above
        drop(h);
    }

    /// Non-msgpack bytes on the wire make `rmpv::decode::read_value` return
    /// `Err`, which the reader loop's `while let Ok(..)` already treats as
    /// end-of-stream. Both an already-in-flight request and any request
    /// issued afterward must resolve to `Closed` rather than hang.
    #[test]
    fn garbage_on_wire_closes_in_flight_and_future_requests() {
        let (peer_read, our_write) = std::io::pipe().unwrap();
        let (our_read, mut peer_write) = std::io::pipe().unwrap();
        // peer thread: drain writes but never answer, so the in-flight
        // request below stays pending until the garbage bytes land
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(peer_read);
            while rmpv::decode::read_value(&mut r).is_ok() {}
        });

        let (h, _n) = EngineHandle::start(our_read, our_write);

        let h2 = h.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(h2.request("nvim_eval", vec![]));
        });

        // give the in-flight request time to land before corrupting the
        // wire: a str8 marker (0xd9) claims a 255-byte string that never
        // follows, then closing the write end forces an EOF mid-read,
        // which read_value surfaces as a decode Err rather than blocking
        // forever waiting for bytes that will never arrive
        std::thread::sleep(std::time::Duration::from_millis(50));
        peer_write.write_all(&[0xd9, 0xff]).unwrap();
        peer_write.flush().unwrap();
        drop(peer_write);

        let in_flight = rx.recv_timeout(std::time::Duration::from_secs(2));
        assert!(
            in_flight.is_ok(),
            "in-flight request() hung after garbage on the wire"
        );
        assert!(matches!(in_flight.unwrap(), Err(EngineError::Closed)));

        let future = h.request("nvim_eval", vec![]);
        assert!(matches!(future, Err(EngineError::Closed)));
    }

    #[test]
    fn decode_preview_reply_loaded_false_carries_no_lines() {
        // wire-verified, docs/picker-preview-wire-capture.md case 1/4:
        // {'loaded': False} with no 'lines' key at all
        let result = Value::Map(vec![(Value::from("loaded"), Value::from(false))]);
        assert_eq!(decode_preview_reply(&result), (false, Vec::new()));
    }

    #[test]
    fn decode_preview_reply_loaded_true_decodes_modified_buffer_lines() {
        // wire-verified, docs/picker-preview-wire-capture.md case 3 (the
        // load-bearing one): a modified-but-unsaved buffer's in-memory
        // content, not the still-unmodified file on disk
        let result = Value::Map(vec![
            (Value::from("loaded"), Value::from(true)),
            (
                Value::from("lines"),
                Value::Array(vec![
                    Value::from("modified line one"),
                    Value::from("modified line two"),
                    Value::from("modified line three"),
                ]),
            ),
        ]);
        assert_eq!(
            decode_preview_reply(&result),
            (
                true,
                vec![
                    "modified line one".to_string(),
                    "modified line two".to_string(),
                    "modified line three".to_string(),
                ]
            )
        );
    }

    #[test]
    fn decode_preview_reply_non_map_result_decodes_to_not_loaded() {
        assert_eq!(decode_preview_reply(&Value::Nil), (false, Vec::new()));
    }

    #[test]
    fn decode_hl_probe_reply_transparent_fixture_has_fg_only() {
        // wire-verified against a real `nvim --embed`, `hi Normal
        // guifg=#f8f8f2` with no `guibg` set (this machine's own config):
        // `nvim_get_hl(0,{name='Normal'})` -> `{fg = 16316658}`
        let result = Value::Map(vec![(Value::from("fg"), Value::from(16316658))]);
        assert_eq!(decode_hl_probe_reply(&result), (Some(16316658), None));
    }

    #[test]
    fn decode_hl_probe_reply_explicit_bg_fixture_has_both_keys() {
        // wire-verified: `hi Normal guibg=#282a36` (on top of the same
        // guifg) -> `{fg = 16316658, bg = 2632246}`; 2632246 == 0x282a36
        let result = Value::Map(vec![
            (Value::from("fg"), Value::from(16316658)),
            (Value::from("bg"), Value::from(2632246)),
        ]);
        assert_eq!(
            decode_hl_probe_reply(&result),
            (Some(16316658), Some(2_632_246))
        );
    }

    /// A genuinely-black theme (`guibg=#000000`) must decode `bg = Some(0)`,
    /// not `None`: the probe's whole point is that key *presence*
    /// disambiguates this from the transparent fixture above, not the
    /// numeric value.
    #[test]
    fn decode_hl_probe_reply_bg_zero_key_present_decodes_to_some_zero() {
        let result = Value::Map(vec![
            (Value::from("fg"), Value::from(16316658)),
            (Value::from("bg"), Value::from(0)),
        ]);
        assert_eq!(decode_hl_probe_reply(&result), (Some(16316658), Some(0)));
    }

    #[test]
    fn decode_hl_probe_reply_non_map_result_decodes_to_none_none() {
        assert_eq!(decode_hl_probe_reply(&Value::Nil), (None, None));
    }

    #[test]
    fn probe_default_hl_sends_the_pinned_wire_shape() {
        let (h, _pump, peer_read, _peer_write) = pumped_peer();
        h.probe_default_hl(3).unwrap();
        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        let RpcMessage::Request { method, params, .. } = RpcMessage::from_value(v).unwrap() else {
            unreachable!("expected a Request");
        };
        assert_eq!(method, "nvim_get_hl");
        assert_eq!(params[0], Value::from(0));
        let Value::Map(opts) = &params[1] else {
            unreachable!("expected a map, got {:?}", params[1]);
        };
        assert_eq!(opts, &vec![(Value::from("name"), Value::from("Normal"))]);
    }

    /// End-to-end through the reader thread's own routing: a transparent-
    /// config reply (fg-only, see `decode_hl_probe_reply`'s fixture doc)
    /// arrives as a `Msg::HlProbeReply` with `bg: None` on the pump's sink,
    /// tagged with the exact generation the probe was issued for.
    #[test]
    fn probe_reply_for_transparent_normal_routes_hlprobereply_with_no_bg() {
        let (h, pump, peer_read, mut peer_write) = pumped_peer();
        let (tx, rx) = mpsc::sync_channel(64);
        let _dpump = pump.attach_sink(tx);

        h.probe_default_hl(5).unwrap();
        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        let RpcMessage::Request { msgid, .. } = RpcMessage::from_value(v).unwrap() else {
            unreachable!("expected a Request");
        };

        let reply = RpcMessage::Response {
            msgid,
            error: Value::Nil,
            result: Value::Map(vec![(Value::from("fg"), Value::from(16316658))]),
        };
        rmpv::encode::write_value(&mut peer_write, &reply.to_value()).unwrap();
        peer_write.flush().unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let Msg::HlProbeReply { generation, fg, bg } = msg else {
            unreachable!("expected HlProbeReply, got {msg:?}");
        };
        assert_eq!(generation, 5);
        assert_eq!(fg, Some(16316658));
        assert_eq!(bg, None);
    }

    /// The counterpart fixture: an explicit-bg reply routes `bg:
    /// Some(0x282a36)`, proving a genuinely-colored (including genuinely
    /// black) background survives the round trip rather than being
    /// conflated with the unset case.
    #[test]
    fn probe_reply_for_explicit_bg_routes_hlprobereply_with_bg_present() {
        let (h, pump, peer_read, mut peer_write) = pumped_peer();
        let (tx, rx) = mpsc::sync_channel(64);
        let _dpump = pump.attach_sink(tx);

        h.probe_default_hl(9).unwrap();
        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        let RpcMessage::Request { msgid, .. } = RpcMessage::from_value(v).unwrap() else {
            unreachable!("expected a Request");
        };

        let reply = RpcMessage::Response {
            msgid,
            error: Value::Nil,
            result: Value::Map(vec![
                (Value::from("fg"), Value::from(16316658)),
                (Value::from("bg"), Value::from(2_632_246)),
            ]),
        };
        rmpv::encode::write_value(&mut peer_write, &reply.to_value()).unwrap();
        peer_write.flush().unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let Msg::HlProbeReply { generation, fg, bg } = msg else {
            unreachable!("expected HlProbeReply, got {msg:?}");
        };
        assert_eq!(generation, 9);
        assert_eq!(fg, Some(16316658));
        assert_eq!(bg, Some(0x282a36));
    }

    /// A remote error on the probe request must resolve the generation
    /// (never leave it permanently unconfirmed) by degrading to "confirmed
    /// unset" -- the safe default -- rather than dropping the reply
    /// entirely.
    #[test]
    fn probe_reply_with_remote_error_degrades_to_confirmed_unset() {
        let (h, pump, peer_read, mut peer_write) = pumped_peer();
        let (tx, rx) = mpsc::sync_channel(64);
        let _dpump = pump.attach_sink(tx);

        h.probe_default_hl(1).unwrap();
        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        let RpcMessage::Request { msgid, .. } = RpcMessage::from_value(v).unwrap() else {
            unreachable!("expected a Request");
        };

        let reply = RpcMessage::Response {
            msgid,
            error: Value::from("boom"),
            result: Value::Nil,
        };
        rmpv::encode::write_value(&mut peer_write, &reply.to_value()).unwrap();
        peer_write.flush().unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let Msg::HlProbeReply { generation, fg, bg } = msg else {
            unreachable!("expected HlProbeReply, got {msg:?}");
        };
        assert_eq!(generation, 1);
        assert_eq!(fg, None);
        assert_eq!(bg, None);
    }

    #[test]
    fn request_heartbeat_sends_the_pinned_wire_shape() {
        let (h, _pump, peer_read, _peer_write) = pumped_peer();
        h.request_heartbeat(3).unwrap();
        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        let RpcMessage::Request { method, params, .. } = RpcMessage::from_value(v).unwrap() else {
            unreachable!("expected a Request");
        };
        assert_eq!(method, "nvim_get_mode");
        assert!(params.is_empty(), "{params:?}");
    }

    /// End-to-end through the reader thread's own routing: the
    /// acknowledgement arrives on the pump's sink tagged with the exact
    /// generation the probe was issued for, which is the whole payload a
    /// liveness watch reads.
    #[test]
    fn heartbeat_reply_routes_heartbeatreply_with_the_generation_it_was_issued_for() {
        let (h, pump, peer_read, mut peer_write) = pumped_peer();
        let (tx, rx) = mpsc::sync_channel(64);
        let _dpump = pump.attach_sink(tx);

        h.request_heartbeat(7).unwrap();
        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        let RpcMessage::Request { msgid, .. } = RpcMessage::from_value(v).unwrap() else {
            unreachable!("expected a Request");
        };

        let reply = RpcMessage::Response {
            msgid,
            error: Value::Nil,
            result: Value::Map(vec![
                (Value::from("mode"), Value::from("n")),
                (Value::from("blocking"), Value::from(false)),
            ]),
        };
        rmpv::encode::write_value(&mut peer_write, &reply.to_value()).unwrap();
        peer_write.flush().unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let Msg::HeartbeatReply { generation } = msg else {
            unreachable!("expected HeartbeatReply, got {msg:?}");
        };
        assert_eq!(generation, 7);
    }

    /// An engine that answers with an error answered: the reply resolves
    /// the generation exactly as a successful one does, since the probe
    /// asks whether anything comes back at all and this did.
    #[test]
    fn a_heartbeat_reply_carrying_a_remote_error_still_acknowledges_the_generation() {
        let (h, pump, peer_read, mut peer_write) = pumped_peer();
        let (tx, rx) = mpsc::sync_channel(64);
        let _dpump = pump.attach_sink(tx);

        h.request_heartbeat(2).unwrap();
        let mut r = std::io::BufReader::new(peer_read);
        let v = rmpv::decode::read_value(&mut r).unwrap();
        let RpcMessage::Request { msgid, .. } = RpcMessage::from_value(v).unwrap() else {
            unreachable!("expected a Request");
        };

        let reply = RpcMessage::Response {
            msgid,
            error: Value::from("boom"),
            result: Value::Nil,
        };
        rmpv::encode::write_value(&mut peer_write, &reply.to_value()).unwrap();
        peer_write.flush().unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let Msg::HeartbeatReply { generation } = msg else {
            unreachable!("expected HeartbeatReply, got {msg:?}");
        };
        assert_eq!(generation, 2);
    }

    /// The lock-free reading and the locked one never disagree: by the time
    /// a drained waiter has been told the connection is closed, the flag an
    /// observer reads without the lock already says so.
    #[test]
    fn is_closed_reads_true_by_the_time_a_pending_waiter_has_been_failed() {
        let (h, _pump, peer_read, peer_write) = pumped_peer();
        assert!(!h.is_closed());
        drop(peer_write);
        drop(peer_read);
        let err = h.request("nvim_get_mode", vec![]).unwrap_err();
        assert!(matches!(err, EngineError::Closed), "{err:?}");
        assert!(h.is_closed());
    }

    /// The reader routes a bridge notification the whole way through to the
    /// runtime's sink, not merely as far as a decoder: the wire method name
    /// and the event name are two separate strings a refactor can drift
    /// apart, and nothing else in the build would notice.
    #[test]
    fn a_bridge_colorscheme_notification_reaches_the_sink_as_a_typed_message() {
        let (_h, pump, _peer_read, mut peer_write) = pumped_peer();
        let (tx, rx) = mpsc::sync_channel(64);
        let _dpump = pump.attach_sink(tx);

        let notif = RpcMessage::Notification {
            method: "view_bridge".into(),
            params: vec![Value::from("colorscheme"), Value::from("dracula")],
        };
        rmpv::encode::write_value(&mut peer_write, &notif.to_value()).unwrap();
        peer_write.flush().unwrap();

        let msg = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let Msg::ColorSchemeChanged { name } = msg else {
            unreachable!("expected ColorSchemeChanged, got {msg:?}");
        };
        assert_eq!(name, "dracula");
    }

    /// A trigger the bridge carries for a consumer this build does not have
    /// yet must be dropped, not routed as something else: the registration
    /// is deliberately wider than the set of consumers.
    #[test]
    fn a_bridge_event_with_no_consumer_decodes_to_nothing() {
        assert!(
            decode_bridge_event(&[Value::from("not-an-event"), Value::from("x")]).is_none(),
            "an unrecognized event has no consumer in this build and must not route"
        );
    }

    #[test]
    fn a_bridge_diagnostics_event_decodes_error_and_warning_counts() {
        let decoded =
            decode_bridge_event(&[Value::from("diagnostics"), Value::from(2), Value::from(1)]);
        assert!(
            matches!(
                decoded,
                Some(Msg::DiagnosticsChanged {
                    errors: 2,
                    warnings: 1
                })
            ),
            "got {decoded:?}"
        );
    }

    #[test]
    fn a_bridge_git_event_decodes_the_branch_name() {
        let decoded = decode_bridge_event(&[Value::from("git"), Value::from("main")]);
        assert!(
            matches!(decoded, Some(Msg::GitBranchChanged { ref branch }) if branch == "main"),
            "got {decoded:?}"
        );
    }

    #[test]
    fn a_bridge_buffer_event_decodes_the_name_and_modified_flag() {
        let decoded = decode_bridge_event(&[
            Value::from("buffer"),
            Value::from("statusline.rs"),
            Value::from(true),
        ]);
        assert!(
            matches!(
                decoded,
                Some(Msg::BufferChanged { ref name, modified: true }) if name == "statusline.rs"
            ),
            "got {decoded:?}"
        );
    }

    /// nvim reports no name at all for a scheme cleared with `:colorscheme
    /// default` in some paths. The switch still happened, so the message
    /// must still arrive -- a consumer keys off the event, not the name.
    #[test]
    fn a_bridge_colorscheme_without_a_usable_name_still_reports_the_switch() {
        let decoded = decode_bridge_event(&[Value::from("colorscheme"), Value::Nil]);
        assert!(
            matches!(decoded, Some(Msg::ColorSchemeChanged { ref name }) if name.is_empty()),
            "a switch with no readable name must still report the switch, got {decoded:?}"
        );
        assert!(
            decode_bridge_event(&[Value::from("colorscheme")]).is_none(),
            "a notification missing its payload entirely is malformed, not a switch"
        );
    }

    #[test]
    fn a_clipboard_get_with_a_valid_register_decodes_to_the_request() {
        let token = ReplyToken { msgid: 5 };
        let decoded = decode_clipboard_get(token, &[Value::from("+")]);
        assert!(matches!(
            decoded,
            Some(Msg::EngineRequest(EngineRequest::ClipboardGet {
                token: t,
                register: '+',
            })) if t.msgid == 5
        ));
    }

    #[test]
    fn a_clipboard_get_with_a_missing_or_malformed_register_decodes_to_nothing() {
        let token = ReplyToken { msgid: 6 };
        assert!(
            decode_clipboard_get(token, &[]).is_none(),
            "no params at all must not guess a register"
        );
        assert!(
            decode_clipboard_get(token, &[Value::Nil]).is_none(),
            "a non-string register must not decode"
        );
        assert!(
            decode_clipboard_get(token, &[Value::from("")]).is_none(),
            "an empty register string has no char to take"
        );
    }

    #[test]
    fn a_clipboard_set_with_valid_params_decodes_to_the_request_with_its_regtype() {
        let token = ReplyToken { msgid: 7 };
        let decoded = decode_clipboard_set(
            token,
            &[
                Value::from("*"),
                Value::Array(vec![Value::from("a"), Value::from("b")]),
                Value::from("V"),
            ],
        );
        let Some(Msg::EngineRequest(EngineRequest::ClipboardSet {
            token: t,
            register,
            lines,
            regtype,
        })) = decoded
        else {
            unreachable!("expected a decoded ClipboardSet, got {decoded:?}");
        };
        assert_eq!(t.msgid, 7);
        assert_eq!(register, '*');
        assert_eq!(lines, vec!["a", "b"]);
        assert_eq!(regtype, RegisterType::Linewise);
    }

    #[test]
    fn a_clipboard_set_with_one_undecodable_line_drops_the_whole_request() {
        let token = ReplyToken { msgid: 8 };
        let decoded = decode_clipboard_set(
            token,
            &[
                Value::from("+"),
                Value::Array(vec![Value::from("a"), Value::Nil]),
                Value::from("v"),
            ],
        );
        assert!(
            decoded.is_none(),
            "one undecodable line must drop the whole request, not silently truncate it"
        );
    }

    #[test]
    fn a_clipboard_set_missing_its_regtype_decodes_to_nothing() {
        let token = ReplyToken { msgid: 9 };
        let decoded = decode_clipboard_set(
            token,
            &[Value::from("+"), Value::Array(vec![Value::from("a")])],
        );
        assert!(
            decoded.is_none(),
            "a request with no regtype param at all must not guess one"
        );
    }

    #[test]
    fn a_clipboard_set_with_an_unrecognized_regtype_string_defaults_to_charwise() {
        let token = ReplyToken { msgid: 10 };
        let decoded = decode_clipboard_set(
            token,
            &[
                Value::from("+"),
                Value::Array(vec![Value::from("a")]),
                Value::from("b"),
            ],
        );
        let Some(Msg::EngineRequest(EngineRequest::ClipboardSet { regtype, .. })) = decoded else {
            unreachable!("expected a decoded ClipboardSet, got {decoded:?}");
        };
        assert_eq!(
            regtype,
            RegisterType::Charwise,
            "an unrecognized regtype (e.g. blockwise 'b') must degrade to charwise, not drop the copy"
        );
    }
}
