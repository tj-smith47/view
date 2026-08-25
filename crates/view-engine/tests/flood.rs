//! Flood-proofing proofs that deserve their own top-level file rather than
//! living in `handle.rs`'s unit test module: a burst of traffic must never
//! stall a response or the reader thread, on either the deprecated
//! unbounded notification channel or the bounded, compacted pump.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rmpv::Value;
use std::io::Write;
use std::time::{Duration, Instant};
use view_engine::{EngineHandle, EngineNotification, RpcMessage};

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

/// A flood of notifications on the deprecated unbounded channel must never
/// stall a pending response, and every notification must still be buffered
/// for a caller that drains late. `EngineHandle::start` is the plain
/// (non-pumped) constructor: this proves the legacy unbounded-notification
/// path stays unaffected by the reader thread's fold/dispatch logic, which
/// the pumped constructor's dual-write also feeds.
#[test]
fn response_is_not_starved_by_unbounded_notification_flood() {
    let (h, n) = fake_flood_peer(|msgid, _| RpcMessage::Response {
        msgid,
        error: Value::Nil,
        result: Value::from(1),
    });
    // Nobody drains `n` yet. The peer writes 10,000 notifications ahead of
    // the response; they must all be buffered by the channel itself while
    // unread, and the response must still arrive promptly. If the
    // notification channel ever regresses to a bounded one, the peer's
    // writer would block on a full channel and this request would stall
    // past the 2s budget, failing the assertion below structurally rather
    // than relying on a collector racing to keep the channel drained.
    let bound = view_test_support::host_deadline(Duration::from_secs(2));
    let start = Instant::now();
    let result = h.request("test", vec![]);
    let elapsed = start.elapsed();
    assert!(
        elapsed < bound,
        "request() took {elapsed:?} against a {bound:?} budget (2s plus the \
         host's share)"
    );
    assert_eq!(result.unwrap(), Value::from(1));
    // Only now is the channel drained, proving all 10,000 notifications
    // were fully buffered while unread.
    let mut count = 0usize;
    while let Ok(note) =
        n.recv_timeout(view_test_support::host_deadline(Duration::from_millis(500)))
    {
        assert_eq!(note.method, "redraw");
        count += 1;
        if count == 10_000 {
            break;
        }
    }
    assert_eq!(count, 10_000, "expected 10,000 notifications, got {count}");
}
