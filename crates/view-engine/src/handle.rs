use crate::rpc::{RpcError, RpcMessage};
use rmpv::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// Errors produced by [`EngineHandle`] operations.
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

type Pending = Arc<Mutex<HashMap<u32, mpsc::Sender<Result<Value, EngineError>>>>>;

/// An RPC client for the embedded Neovim process, with request correlation
/// and a flood-proof notification reader.
///
/// `EngineHandle` spawns two internal threads on creation:
/// - A reader thread that decodes incoming messages, correlates responses to
///   requests, and forwards notifications to a receiver.
/// - The handle itself serializes outgoing requests and maintains pending
///   response waiters.
///
/// The reader thread uses an unbounded channel for notifications, ensuring
/// that a flood of notifications (e.g., a large `redraw` burst) never blocks
/// the delivery of pending responses.
pub struct EngineHandle {
    next_msgid: AtomicU32,
    pending: Pending,
    writer: Mutex<Box<dyn Write + Send>>,
}

impl EngineHandle {
    /// Starts a new engine handle, spawning reader and writer threads.
    ///
    /// # Arguments
    ///
    /// * `reader` - An unbuffered read source (typically one end of a pipe).
    ///   The handle wraps it in a `BufReader` internally.
    /// * `writer` - An unbuffered write sink (typically the other end of the
    ///   pipe pair). Used by the handle thread to send requests.
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
    /// Never panics. Errors on the internal reader thread (I/O, decode) cause
    /// the thread to exit cleanly, sending `Closed` to any pending requests.
    ///
    /// # Notification receiver lifetime
    ///
    /// The reader thread sends notifications to the returned `Receiver`
    /// without buffering elsewhere. If the receiver is dropped, the next
    /// `notif_tx.send` fails and the reader thread exits its loop — after
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
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        // unbounded so the reader thread can never stall a pending response
        // behind a redraw flood; compaction lands with the surface damage model
        let (notif_tx, notif_rx) = mpsc::channel();
        let reader_pending = Arc::clone(&pending);
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(reader);
            while let Ok(value) = rmpv::decode::read_value(&mut r) {
                match RpcMessage::from_value(value) {
                    Ok(RpcMessage::Response {
                        msgid,
                        error,
                        result,
                    }) => {
                        let waiter = reader_pending
                            .lock()
                            .ok()
                            .and_then(|mut p| p.remove(&msgid));
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
                    Ok(RpcMessage::Request { .. }) | Err(_) => {
                        // nvim-to-client requests arrive in P2 (VimEnter
                        // blocking rpcrequest); until then they are ignored
                    }
                }
            }
            // engine is gone: fail every in-flight request instead of hanging
            if let Ok(mut p) = reader_pending.lock() {
                for (_, tx) in p.drain() {
                    let _ = tx.send(Err(EngineError::Closed));
                }
            }
        });
        let handle = Self {
            next_msgid: AtomicU32::new(1),
            pending,
            writer: Mutex::new(Box::new(writer)),
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
    ///   the response arrives.
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
        let msgid = self.next_msgid.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .map_err(|_| EngineError::Closed)?
            .insert(msgid, tx);
        let msg = RpcMessage::Request {
            msgid,
            method: method.to_owned(),
            params,
        };
        if let Err(e) = self.write_request(&msg) {
            // the reader thread will never see this msgid now, so nothing
            // will ever remove the waiter; drop it ourselves or it leaks
            // for the life of the handle
            if let Ok(mut p) = self.pending.lock() {
                p.remove(&msgid);
            }
            return Err(e);
        }
        rx.recv().map_err(|_| EngineError::Closed)?
    }

    fn write_request(&self, msg: &RpcMessage) -> Result<(), EngineError> {
        let mut w = self.writer.lock().map_err(|_| EngineError::Closed)?;
        rmpv::encode::write_value(&mut *w, &msg.to_value())
            .map_err(|e| EngineError::Io(std::io::Error::other(e)))?;
        w.flush()?;
        Ok(())
    }
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
}
