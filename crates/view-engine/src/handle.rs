use crate::rpc::{RpcError, RpcMessage};
use rmpv::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, PoisonError};
use std::time::Duration;

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

/// The set of in-flight request waiters plus a `closed` flag, guarded by a
/// single lock so a reader or writer thread that discovers the connection
/// is gone can mark it closed and drain every waiter in one atomic step.
/// Without sharing the lock, a request could insert itself into the map
/// after the draining thread has already run, leaking a waiter that will
/// never be resolved (the original hang this type exists to close).
#[derive(Default)]
struct PendingState {
    waiters: HashMap<u32, mpsc::Sender<Result<Value, EngineError>>>,
    closed: bool,
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
///   to requests, forwards notifications to a receiver, and immediately
///   answers any incoming `Request` from the peer with a
///   `"method not supported"` error (dispatching nvim-to-client requests is
///   not implemented yet, but the msgpack-RPC contract still requires a
///   reply or the peer's main loop blocks forever waiting for one).
/// - A writer thread that owns the write half and serializes every
///   outgoing message (requests, the auto-replies above, and fire-and-forget
///   notifications) fed to it over an internal channel. Callers never touch
///   the pipe directly, so a write that blocks on a full OS pipe buffer
///   blocks only the writer thread, never the caller's timeout.
///
/// Both threads share one `closed` flag with the pending-waiters map (see
/// `PendingState`): whichever thread notices the connection is gone first
/// marks it closed and drains every waiter with
/// [`EngineError::Closed`](EngineError::Closed) in the same critical
/// section that flips the flag, so a request racing the shutdown either
/// lands before the flag (and gets drained) or after it (and is rejected
/// before it ever touches the pipe).
///
/// The reader thread uses an unbounded channel for notifications, ensuring
/// that a flood of notifications (e.g., a large `redraw` burst) never blocks
/// the delivery of pending responses.
pub struct EngineHandle {
    next_msgid: Arc<AtomicU32>,
    pending: Pending,
    write_tx: mpsc::Sender<Vec<u8>>,
}

impl Clone for EngineHandle {
    fn clone(&self) -> Self {
        Self {
            next_msgid: Arc::clone(&self.next_msgid),
            pending: Arc::clone(&self.pending),
            write_tx: self.write_tx.clone(),
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
    pub fn start(
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> (Self, mpsc::Receiver<EngineNotification>) {
        let pending: Pending = Arc::new(Mutex::new(PendingState::default()));
        // unbounded so the reader thread can never stall a pending response
        // behind a redraw flood; compaction lands with the surface damage model
        let (notif_tx, notif_rx) = mpsc::channel();
        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>();

        let writer_pending = Arc::clone(&pending);
        std::thread::spawn(move || {
            let mut w = writer;
            while let Ok(bytes) = write_rx.recv() {
                let sent = w.write_all(&bytes).and_then(|()| w.flush());
                if sent.is_err() {
                    // the pipe is broken (peer gone, or wedged past
                    // recovery): fail every pending waiter instead of
                    // leaving them to hang on a response that can never
                    // arrive, and reject every future request up front
                    close_and_drain(&writer_pending);
                    break;
                }
            }
        });

        let reader_pending = Arc::clone(&pending);
        let reader_write_tx = write_tx.clone();
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(reader);
            while let Ok(value) = rmpv::decode::read_value(&mut r) {
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
                        if let Some(tx) = waiter {
                            let outcome = if error == Value::Nil {
                                Ok(result)
                            } else {
                                Err(EngineError::Remote(error))
                            };
                            let _ = tx.send(outcome);
                        }
                    }
                    Ok(RpcMessage::Notification { method, params }) => {
                        if notif_tx
                            .send(EngineNotification { method, params })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(RpcMessage::Request { msgid, .. }) => {
                        // msgpack-RPC obliges a reply to every Request, or
                        // the peer's main loop blocks forever waiting for
                        // one; dispatching these to real handlers is not
                        // implemented yet, so answer with a typed error
                        let resp = RpcMessage::Response {
                            msgid,
                            error: Value::from("method not supported"),
                            result: Value::Nil,
                        };
                        if let Ok(bytes) = encode_message(&resp) {
                            let _ = reader_write_tx.send(bytes);
                        }
                    }
                    Err(_) => {
                        // malformed message shape: not fatal on its own: a
                        // future well-formed message can still arrive on
                        // the same connection
                    }
                }
            }
            // engine is gone: fail every in-flight request instead of hanging
            close_and_drain(&reader_pending);
        });
        let handle = Self {
            next_msgid: Arc::new(AtomicU32::new(1)),
            pending,
            write_tx,
        };
        (handle, notif_rx)
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
        self.write_tx.send(bytes).map_err(|_| EngineError::Closed)
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
            if p.closed {
                return Err(EngineError::Closed);
            }
            p.waiters.insert(msgid, tx);
        }
        if self.write_tx.send(bytes).is_err() {
            // the writer thread is gone, so nothing will ever write this
            // request or fail it on our behalf; undo the insert ourselves
            let mut p = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            p.waiters.remove(&msgid);
            return Err(EngineError::Closed);
        }
        Ok((msgid, rx))
    }
}

/// Marks the connection closed and drains every pending waiter with
/// [`EngineError::Closed`] in one critical section, so a `send_request`
/// racing this call either lands before the flag flips (and gets drained
/// here) or observes `closed == true` and never inserts at all.
fn close_and_drain(pending: &Pending) {
    let mut p = pending.lock().unwrap_or_else(PoisonError::into_inner);
    p.closed = true;
    for (_, tx) in p.waiters.drain() {
        let _ = tx.send(Err(EngineError::Closed));
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

    fn fake_flood_peer(
        mut respond: impl FnMut(u32, &str) -> RpcMessage + Send + 'static,
    ) -> (EngineHandle, std::sync::mpsc::Receiver<EngineNotification>) {
        let (peer_read, our_write) = std::io::pipe().unwrap();
        let (our_read, mut peer_write) = std::io::pipe().unwrap();
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(peer_read);
            while let Ok(v) = rmpv::decode::read_value(&mut r) {
                if let Ok(RpcMessage::Request { msgid, method, .. }) = RpcMessage::from_value(v) {
                    // emit 10,000 notifications before the response
                    for _ in 0..10_000 {
                        let notif = RpcMessage::Notification {
                            method: "redraw".into(),
                            params: vec![],
                        };
                        rmpv::encode::write_value(&mut peer_write, &notif.to_value()).unwrap();
                    }
                    let reply = respond(msgid, &method);
                    rmpv::encode::write_value(&mut peer_write, &reply.to_value()).unwrap();
                    peer_write.flush().unwrap();
                }
            }
        });
        EngineHandle::start(our_read, our_write)
    }

    #[test]
    fn response_is_not_starved_by_notification_flood() {
        let (h, n) = fake_flood_peer(|msgid, _| RpcMessage::Response {
            msgid,
            error: Value::Nil,
            result: Value::from(1),
        });
        // Nobody drains `n` yet. The peer writes 10,000 notifications ahead
        // of the response; they must all be buffered by the channel itself
        // while unread, and the response must still arrive promptly. If the
        // notification channel ever regresses to a bounded one, the peer's
        // writer would block on a full channel and this request would stall
        // past the 2s budget, failing the assertion below structurally
        // rather than relying on a collector racing to keep the channel
        // drained.
        let start = std::time::Instant::now();
        let result = h.request("test", vec![]);
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "request() took {elapsed:?}, should be under 2s"
        );
        assert_eq!(result.unwrap(), Value::from(1));
        // Only now do we drain the channel, proving all 10,000 notifications
        // were fully buffered while unread.
        let mut count = 0usize;
        while let Ok(note) = n.recv_timeout(std::time::Duration::from_millis(500)) {
            assert_eq!(note.method, "redraw");
            count += 1;
            if count == 10_000 {
                break;
            }
        }
        assert_eq!(count, 10_000, "expected 10,000 notifications, got {count}");
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
    /// subsequent call from our side deadlocks against it. Dispatching
    /// these to real handlers is P2 work; until then, the reply is a typed
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
}
