# view P1 — Engine Seam Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `view <file>` opens a real editable Neovim in the terminal: spawn `nvim --embed`, speak our own msgpack-RPC, attach with `ext_linegrid`, paint the grid via ratatui, forward keys — plus a first paired latency measurement against bare nvim.

**Architecture:** Blocking reader/writer threads own the child's stdio (the reader can never stall a response behind a redraw flood); an `EngineHandle` correlates requests; decoded `UiEvent`s flow over an unbounded channel to the main loop, which applies them to a `Grid` in view-core and repaints on `Flush`. The bin wires crossterm input → key notation → `nvim_input`.

**Tech Stack:** rmpv (msgpack), blocking std threads at the pipe seam (reader + writer; no async runtime this phase), ratatui + crossterm, portable-pty + vt100 (bench), thiserror.

## Global Constraints

- All P0 constraints apply (lints, Taskfile usage, commit trailer, WHY-only comments).
- Attach options this phase: `ext_linegrid` only. Other ext layers land in P2 per the spec.
- nvim ≥ 0.11 on PATH is required for integration tests; tests fail loudly if missing — no silent skips.
- Add dependencies with `cargo add <crate> -p <member>` (e.g. `cargo add rmpv -p view-engine`); never hand-write guessed version numbers.
- Spec anchors for this phase: engine seam (spec 5.1–5.3), RPC flow control (spec 5.2), budgets (spec 3.1, measurement defs 3.4).

---

### Task 1: msgpack-RPC message codec

**Files:**
- Create: `crates/view-engine/src/rpc/mod.rs`
- Create: `crates/view-engine/src/rpc/msg.rs`
- Modify: `crates/view-engine/src/lib.rs` (add `pub mod rpc;`)

**Interfaces:**
- Consumes: nothing.
- Produces: `RpcMessage` enum with `to_value() -> rmpv::Value`, `from_value(rmpv::Value) -> Result<RpcMessage, RpcError>`; `RpcError` (thiserror). Task 2 reads/writes these over the wire.

- [ ] **Step 1: Add dependencies**

Run: `cargo add rmpv -p view-engine && cargo add thiserror -p view-engine`

- [ ] **Step 2: Write the failing tests** (`crates/view-engine/src/rpc/msg.rs`, bottom)

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use rmpv::Value;

    #[test]
    fn request_roundtrips() {
        let m = RpcMessage::Request {
            msgid: 7,
            method: "nvim_get_api_info".into(),
            params: vec![],
        };
        assert_eq!(RpcMessage::from_value(m.to_value()).unwrap(), m);
    }

    #[test]
    fn response_roundtrips() {
        let m = RpcMessage::Response {
            msgid: 7,
            error: Value::Nil,
            result: Value::from(42),
        };
        assert_eq!(RpcMessage::from_value(m.to_value()).unwrap(), m);
    }

    #[test]
    fn notification_roundtrips() {
        let m = RpcMessage::Notification {
            method: "redraw".into(),
            params: vec![Value::from("x")],
        };
        assert_eq!(RpcMessage::from_value(m.to_value()).unwrap(), m);
    }

    #[test]
    fn garbage_is_a_typed_error() {
        assert!(matches!(
            RpcMessage::from_value(Value::from("nope")),
            Err(RpcError::Malformed(_))
        ));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p view-engine`
Expected: FAIL to compile ("cannot find type `RpcMessage`").

- [ ] **Step 4: Implement**

```rust
// crates/view-engine/src/rpc/mod.rs
pub mod msg;
pub use msg::{RpcError, RpcMessage};
```

```rust
// crates/view-engine/src/rpc/msg.rs
use rmpv::Value;

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("malformed rpc message: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RpcMessage {
    Request { msgid: u32, method: String, params: Vec<Value> },
    Response { msgid: u32, error: Value, result: Value },
    Notification { method: String, params: Vec<Value> },
}

impl RpcMessage {
    pub fn to_value(&self) -> Value {
        match self {
            Self::Request { msgid, method, params } => Value::Array(vec![
                0.into(),
                (*msgid).into(),
                method.as_str().into(),
                Value::Array(params.clone()),
            ]),
            Self::Response { msgid, error, result } => Value::Array(vec![
                1.into(),
                (*msgid).into(),
                error.clone(),
                result.clone(),
            ]),
            Self::Notification { method, params } => Value::Array(vec![
                2.into(),
                method.as_str().into(),
                Value::Array(params.clone()),
            ]),
        }
    }

    pub fn from_value(v: Value) -> Result<Self, RpcError> {
        let Value::Array(items) = v else {
            return Err(RpcError::Malformed("not an array".into()));
        };
        let kind = items
            .first()
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::Malformed("missing kind".into()))?;
        let arity = items.len();
        match (kind, arity) {
            (0, 4) => Ok(Self::Request {
                msgid: as_u32(&items[1])?,
                method: as_str(&items[2])?,
                params: as_array(&items[3])?,
            }),
            (1, 4) => Ok(Self::Response {
                msgid: as_u32(&items[1])?,
                error: items[2].clone(),
                result: items[3].clone(),
            }),
            (2, 3) => Ok(Self::Notification {
                method: as_str(&items[1])?,
                params: as_array(&items[2])?,
            }),
            _ => Err(RpcError::Malformed(format!("kind={kind} arity={arity}"))),
        }
    }
}

fn as_u32(v: &Value) -> Result<u32, RpcError> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| RpcError::Malformed("bad msgid".into()))
}

fn as_str(v: &Value) -> Result<String, RpcError> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| RpcError::Malformed("bad string".into()))
}

fn as_array(v: &Value) -> Result<Vec<Value>, RpcError> {
    match v {
        Value::Array(a) => Ok(a.clone()),
        _ => Err(RpcError::Malformed("bad params".into())),
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p view-engine`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
task commit -- -m "feat(engine): msgpack-rpc message codec

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: EngineHandle — reader/writer threads with request correlation

**Files:**
- Create: `crates/view-engine/src/handle.rs`
- Modify: `crates/view-engine/src/lib.rs` (add `pub mod handle;` and `pub use handle::{EngineHandle, EngineError, EngineNotification};`)

**Interfaces:**
- Consumes: `RpcMessage`, `RpcError` from Task 1.
- Produces:
  - `EngineHandle::start(reader: impl Read + Send + 'static, writer: impl Write + Send + 'static) -> (EngineHandle, std::sync::mpsc::Receiver<EngineNotification>)`
  - `EngineHandle::request(&self, method: &str, params: Vec<Value>) -> Result<Value, EngineError>` (blocking; async wrapper comes later if needed)
  - `pub struct EngineNotification { pub method: String, pub params: Vec<Value> }`
  - `EngineError` variants: `Rpc(RpcError)`, `Io(std::io::Error)`, `Remote(Value)`, `Closed`
- Tasks 3, 4, 7 build on exactly these signatures.

- [ ] **Step 1: Write the failing tests** (`crates/view-engine/src/handle.rs`, bottom)

The fake peer is a thread speaking msgpack-rpc over `std::io::pipe` pairs. The flood test proves the ordering guarantee from the spec: a response arriving after 10,000 notifications is still delivered, and no notification is lost.

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::rpc::RpcMessage;
    use rmpv::Value;
    use std::io::Write as _;

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
        assert!(matches!(h.request("x", vec![]), Err(EngineError::Remote(_))));
    }

    #[test]
    fn response_is_not_starved_by_notification_flood() {
        let (h, n) = fake_peer(|msgid, _| {
            // reply is written only after the flood, forcing the reader to
            // drain 10k notifications before it can complete the request
            RpcMessage::Response { msgid, error: Value::Nil, result: Value::from(1) }
        });
        // flood arrives via the same peer thread before the reply: emulate by
        // sending the request only after peer pre-writes notifications
        for _ in 0..3 {
            // warmup requests keep the pipe hot; the real flood test follows
            h.request("warm", vec![]).unwrap();
        }
        let flood = std::thread::spawn(move || {
            let mut count = 0usize;
            while let Ok(note) = n.recv_timeout(std::time::Duration::from_millis(500)) {
                assert_eq!(note.method, "redraw");
                count += 1;
                if count == 10_000 {
                    break;
                }
            }
            count
        });
        // fake_peer above doesn't emit notifications; this test needs its own
        // peer variant — see fake_flood_peer in the implementation step.
        let _ = flood;
    }
}
```

Note to implementer: the flood test as sketched needs a `fake_flood_peer` that, upon receiving a request, first writes 10,000 `redraw` notifications and then the response. Write it next to `fake_peer` (same shape, different write order) and finish `response_is_not_starved_by_notification_flood` so it asserts: (a) `request()` returns `Ok` within 2 s, (b) all 10,000 notifications arrive on the receiver. Both assertions are the deliverable; the test must fail if either ordering guarantee breaks.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p view-engine`
Expected: FAIL to compile ("cannot find `EngineHandle`").

- [ ] **Step 3: Implement**

```rust
// crates/view-engine/src/handle.rs
use crate::rpc::{RpcError, RpcMessage};
use rmpv::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("engine returned error: {0:?}")]
    Remote(Value),
    #[error("engine connection closed")]
    Closed,
}

#[derive(Debug)]
pub struct EngineNotification {
    pub method: String,
    pub params: Vec<Value>,
}

type Pending = Arc<Mutex<HashMap<u32, mpsc::Sender<Result<Value, EngineError>>>>>;

pub struct EngineHandle {
    next_msgid: AtomicU32,
    pending: Pending,
    writer: Mutex<Box<dyn Write + Send>>,
}

impl EngineHandle {
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
            loop {
                let value = match rmpv::decode::read_value(&mut r) {
                    Ok(v) => v,
                    Err(_) => break,
                };
                match RpcMessage::from_value(value) {
                    Ok(RpcMessage::Response { msgid, error, result }) => {
                        let waiter = reader_pending.lock().ok().and_then(|mut p| p.remove(&msgid));
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
                        if notif_tx.send(EngineNotification { method, params }).is_err() {
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
        {
            let mut w = self.writer.lock().map_err(|_| EngineError::Closed)?;
            rmpv::encode::write_value(&mut *w, &msg.to_value())
                .map_err(|e| EngineError::Io(std::io::Error::other(e)))?;
            w.flush()?;
        }
        rx.recv().map_err(|_| EngineError::Closed)?
    }
}
```

- [ ] **Step 4: Finish the flood test per the implementer note, then run all tests**

Run: `cargo test -p view-engine`
Expected: all pass, including the completed flood test with both assertions.

- [ ] **Step 5: Commit**

```bash
task commit -- -m "feat(engine): rpc handle with correlation and flood-proof reader

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Spawn nvim --embed + version handshake

**Files:**
- Create: `crates/view-engine/src/process.rs`
- Modify: `crates/view-engine/src/lib.rs` (add `pub mod process;` and re-export `Engine`, `EngineConfig`, `ApiInfo`)

**Interfaces:**
- Consumes: `EngineHandle` from Task 2.
- Produces:
  - `pub struct EngineConfig { pub nvim_bin: std::path::PathBuf, pub extra_args: Vec<std::ffi::OsString>, pub handshake_timeout: std::time::Duration, pub shutdown_timeout: std::time::Duration }` with `Default` using `"nvim"` (PATH lookup; bundled resolution replaces this default at release packaging), `handshake_timeout: Duration::from_secs(5)`, and `shutdown_timeout: Duration::from_millis(500)`.
  - `pub struct Engine { pub handle: EngineHandle, notifications: Option<Receiver<EngineNotification>>, child: Child, shutdown_timeout: Duration, pub api_info: ApiInfo }` — fix round 1 (concurrency review) privatized `notifications` and `child` and added `shutdown_timeout`. `Engine` owns the child for its whole lifetime: `Drop` sends a graceful `qa!` notification, polls `try_wait` up to `shutdown_timeout`, then force-kills and reaps, so no path out of `spawn()` (early-return or long-lived) can leak a zombie and a responsive nvim gets a chance to flush shada/remove its swapfile first. A private `ChildGuard` wrapper covers the pre-handshake window inside `spawn()` itself (always SIGKILL — nothing to save pre-handshake), before the child is handed to `Engine`.
  - `Engine::spawn(cfg: EngineConfig) -> Result<Engine, EngineError>` — the `nvim_get_api_info` handshake uses `EngineHandle::request_timeout` (not `request`) with `cfg.handshake_timeout`, so a wedged nvim fails `spawn()` with `EngineError::Timeout` instead of hanging the caller forever.
  - `Engine::take_notifications(&mut self) -> Option<Receiver<EngineNotification>>` — takes the notification receiver once; `Engine` cannot hold it as a plain field because a borrowed `Receiver` is `!Sync`, which would make `Engine` (and `Arc<Engine>`) not even `Send`.
  - `Engine::pid(&self) -> u32` — the child's OS pid, for diagnostics/tests; does not block or observe exit status.
  - `Engine::shutdown(self) -> std::io::Result<std::process::ExitStatus>` — consumes `self` and runs the same graceful-then-forced sequence as `Drop`, returning the real exit status (`Drop` alone discards it). Use this when the caller needs to distinguish a graceful exit from a forced kill or forward the real exit code.
  - `pub struct ApiInfo { pub channel_id: u64, pub version_major: u64, pub version_minor: u64 }`
- Task 7 consumes `Engine::spawn`; the version fields feed the doctor warning later. `child` is a private field as of fix round 1 (concurrency review, M1: a public `child` let callers call the blocking `Child::wait()` directly and hang, since `handle`'s writer keeps the child's stdin open). Task 7's exit-code path calls `engine.shutdown()` instead of `engine.child.wait()` to read the real exit status.

- [ ] **Step 1: Write the failing integration test** (`crates/view-engine/tests/spawn.rs`)

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
use view_engine::process::{Engine, EngineConfig};

#[test]
fn spawns_and_handshakes_with_real_nvim() {
    let engine = Engine::spawn(EngineConfig::default()).unwrap();
    assert!(engine.api_info.channel_id >= 1);
    // floor from the spec: engine must be at least 0.11
    assert!(
        (engine.api_info.version_major, engine.api_info.version_minor) >= (0, 11),
        "nvim >= 0.11 required, found {}.{}",
        engine.api_info.version_major,
        engine.api_info.version_minor
    );
    let echoed = engine
        .handle
        .request("nvim_eval", vec![rmpv::Value::from("21 * 2")])
        .unwrap();
    assert_eq!(echoed.as_u64(), Some(42));
    // no manual kill: Engine's Drop impl kills and reaps the child, and it
    // runs even if an earlier assert above panics and unwinds through here
}
```

Also run `cargo add rmpv -p view-engine --dev` if the dev-dependency is not yet present for the test.

Fix round 1 also added `handshake_failure_reaps_child` to this file, covering the
case a plain `unwrap()`-based test cannot: a wedged nvim that never replies to the
handshake. See `crates/view-engine/tests/spawn.rs` for the current test, and
`crates/view-engine/tests/fixtures/fake_hang_nvim.sh` for the fake binary it spawns.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p view-engine --test spawn`
Expected: FAIL to compile ("no module `process`").

- [ ] **Step 3: Implement**

```rust
// crates/view-engine/src/process.rs
//
// Current shape as of fix round 1 (concurrency review): `notifications` and
// `child` are private, `EngineConfig` gained `shutdown_timeout`, and
// `Engine` gained `take_notifications`/`pid`/`shutdown` accessors plus a
// graceful-then-forced `Drop`. See crates/view-engine/src/process.rs for
// the authoritative implementation; this snippet is illustrative only.
use crate::handle::{EngineError, EngineHandle, EngineNotification};
use rmpv::Value;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

pub struct EngineConfig {
    pub nvim_bin: PathBuf,
    pub extra_args: Vec<OsString>,
    pub handshake_timeout: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            nvim_bin: PathBuf::from("nvim"),
            extra_args: vec![],
            handshake_timeout: Duration::from_secs(5),
            shutdown_timeout: Duration::from_millis(500),
        }
    }
}

pub struct ApiInfo {
    pub channel_id: u64,
    pub version_major: u64,
    pub version_minor: u64,
}

pub struct Engine {
    pub handle: EngineHandle,
    notifications: Option<Receiver<EngineNotification>>,
    child: Child,
    shutdown_timeout: Duration,
    pub api_info: ApiInfo,
}

// Owns the child during spawn() so every early-return path (pipe capture
// failure, handshake error or timeout) reaps it instead of leaking a
// zombie. Disarmed via `.0.take()` once spawn() has everything it needs to
// build the long-lived Engine, which then owns reaping itself. Always
// force-kills: the child has never answered anything at this point, so
// there is no session state worth a graceful qa!.
struct ChildGuard(Option<Child>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // best-effort: the child may already be gone, so errors here
            // are not actionable and are discarded
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Engine {
    pub fn spawn(cfg: EngineConfig) -> Result<Self, EngineError> {
        let mut guard = ChildGuard(Some(
            Command::new(&cfg.nvim_bin)
                .arg("--embed")
                .args(&cfg.extra_args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        ));
        let child = guard.0.as_mut().ok_or(EngineError::Closed)?;
        let stdout = child.stdout.take().ok_or(EngineError::Closed)?;
        let stdin = child.stdin.take().ok_or(EngineError::Closed)?;
        let (handle, notifications) = EngineHandle::start(stdout, stdin);
        // request_timeout, not request: a wedged nvim must not hang spawn()
        // forever, and any error here (including Timeout) drops `guard`,
        // reaping the child before the error propagates
        let api_info = decode_api_info(handle.request_timeout(
            "nvim_get_api_info",
            vec![],
            cfg.handshake_timeout,
        )?)?;
        let Some(child) = guard.0.take() else { return Err(EngineError::Closed) };
        Ok(Self {
            handle,
            notifications: Some(notifications),
            child,
            shutdown_timeout: cfg.shutdown_timeout,
            api_info,
        })
    }

    // `Engine` cannot hold the receiver as a plain field: a borrowed
    // `Receiver<EngineNotification>` is `!Sync`, which would make `Engine`
    // (and `Arc<Engine>`) not even `Send` — inexpressible against polling
    // notifications on one thread while issuing requests from others via a
    // cloned `EngineHandle`.
    pub fn take_notifications(&mut self) -> Option<Receiver<EngineNotification>> {
        self.notifications.take()
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    // Consumes self and returns the real exit status, which Drop (discards
    // errors, no return value) cannot surface.
    pub fn shutdown(mut self) -> std::io::Result<ExitStatus> {
        graceful_kill(&self.handle, &mut self.child, self.shutdown_timeout)
    }
}

impl Drop for Engine {
    // Errors are discarded — a kill() on an already-exited process (e.g.
    // one a caller already collected the exit status of via shutdown()) is
    // not actionable and must not panic or be surfaced from a Drop impl.
    fn drop(&mut self) {
        let _ = graceful_kill(&self.handle, &mut self.child, self.shutdown_timeout);
    }
}

// Sends qa! as a fire-and-forget notification, polls try_wait until the
// child exits or shutdown_timeout elapses, then force-kills and reaps.
// Shared by Engine::shutdown and Drop so the two sequences can't drift.
fn graceful_kill(
    handle: &EngineHandle,
    child: &mut Child,
    shutdown_timeout: Duration,
) -> std::io::Result<ExitStatus> {
    let _ = handle.notify("nvim_command", vec![Value::from("qa!")]);
    let deadline = Instant::now() + shutdown_timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill()?;
    child.wait()
}

fn decode_api_info(v: Value) -> Result<ApiInfo, EngineError> {
    let bad = || EngineError::Remote(Value::from("unexpected api_info shape"));
    let Value::Array(parts) = v else { return Err(bad()) };
    let channel_id = parts.first().and_then(Value::as_u64).ok_or_else(bad)?;
    let meta = parts.get(1).ok_or_else(bad)?;
    let version = map_get(meta, "version").ok_or_else(bad)?;
    Ok(ApiInfo {
        channel_id,
        version_major: map_get(&version, "major").and_then(|v| v.as_u64()).ok_or_else(bad)?,
        version_minor: map_get(&version, "minor").and_then(|v| v.as_u64()).ok_or_else(bad)?,
    })
}

fn map_get(v: &Value, key: &str) -> Option<Value> {
    let Value::Map(pairs) = v else { return None };
    pairs
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, val)| val.clone())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p view-engine --test spawn`
Expected: PASS (requires nvim on PATH; a loud failure here means the prerequisite is missing — do not skip).

- [ ] **Step 5: Commit**

```bash
task commit -- -m "feat(engine): spawn nvim --embed with api-info handshake

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: ext_linegrid UiEvent decoding

**Files:**
- Create: `crates/view-engine/src/ui_events.rs`
- Modify: `crates/view-engine/src/lib.rs` (add `pub mod ui_events;`)

**Interfaces:**
- Consumes: `rmpv::Value` redraw payloads.
- Produces (Task 5 and the P2 plan consume these exact shapes):

```rust
pub enum UiEvent {
    GridResize { grid: u64, width: u64, height: u64 },
    GridLine { grid: u64, row: u64, col_start: u64, cells: Vec<GridCell> },
    GridCursorGoto { grid: u64, row: u64, col: u64 },
    GridScroll { grid: u64, top: u64, bot: u64, left: u64, right: u64, rows: i64 },
    GridClear { grid: u64 },
    HlAttrDefine { id: u64, fg: Option<u32>, bg: Option<u32>, bold: bool, italic: bool, underline: bool, reverse: bool },
    DefaultColorsSet {
        fg: Option<u32>,
        bg: Option<u32>,
        sp: Option<u32>,
    },
    Flush,
    Unknown { name: String },
}
pub struct GridCell { pub text: String, pub hl_id: u64, pub repeat: u64 }
pub fn decode_redraw(params: &[rmpv::Value]) -> Vec<UiEvent>
```

- [ ] **Step 1: Write the failing tests** (`crates/view-engine/src/ui_events.rs`, bottom)

The redraw wire format is `["event_name", [args...], [args...], ...]` batched. `grid_line` cells are `[text, hl_id?, repeat?]` where a missing `hl_id` carries the previous cell's value within the line — the decoder resolves that carry-over so consumers never re-implement it. **`grid_line`'s own tuple is 5 elements on the real wire — `[grid, row, col_start, cells, wrap]` — `wrap` is mandatory, not optional; a fixture built with 4 elements does not exercise the real shape.** All slice patterns below use a trailing `..` rather than an exact-length pattern, so a nvim minor-version arity bump degrades gracefully instead of silently falling through to `Unknown`.

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use rmpv::Value;

    fn arr(v: Vec<Value>) -> Value {
        Value::Array(v)
    }

    #[test]
    fn decodes_grid_line_with_hl_carryover_and_repeat() {
        // real nvim's grid_line tuple is [grid, row, col_start, cells, wrap]
        // (5 elements) -- the trailing `wrap` is mandatory on the wire even
        // though this decoder doesn't consume it.
        let params = vec![arr(vec![
            Value::from("grid_line"),
            arr(vec![
                Value::from(1),
                Value::from(0),
                Value::from(0),
                arr(vec![
                    arr(vec![Value::from("a"), Value::from(5)]),
                    arr(vec![Value::from("b")]), // carries hl 5
                    arr(vec![Value::from(" "), Value::from(0), Value::from(3)]),
                ]),
                Value::from(false),
            ]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![UiEvent::GridLine {
                grid: 1,
                row: 0,
                col_start: 0,
                cells: vec![
                    GridCell { text: "a".to_string(), hl_id: 5, repeat: 1 },
                    GridCell { text: "b".to_string(), hl_id: 5, repeat: 1 },
                    GridCell { text: " ".to_string(), hl_id: 0, repeat: 3 },
                ],
            }]
        );
    }

    #[test]
    fn decodes_scroll_resize_clear_cursor_flush() {
        let params = vec![
            arr(vec![Value::from("grid_resize"), arr(vec![1.into(), 80.into(), 24.into()])]),
            arr(vec![Value::from("grid_scroll"), arr(vec![1.into(), 0.into(), 24.into(), 0.into(), 80.into(), 3.into(), 0.into()])]),
            arr(vec![Value::from("grid_clear"), arr(vec![1.into()])]),
            arr(vec![Value::from("grid_cursor_goto"), arr(vec![1.into(), 2.into(), 5.into()])]),
            arr(vec![Value::from("flush"), arr(vec![])]),
        ];
        let evs = decode_redraw(&params);
        assert!(matches!(evs[0], UiEvent::GridResize { grid: 1, width: 80, height: 24 }));
        assert!(matches!(evs[1], UiEvent::GridScroll { rows: 3, .. }));
        assert!(matches!(evs[2], UiEvent::GridClear { grid: 1 }));
        assert!(matches!(evs[3], UiEvent::GridCursorGoto { row: 2, col: 5, .. }));
        assert!(matches!(evs[4], UiEvent::Flush));
    }

    #[test]
    fn unknown_events_are_preserved_not_dropped() {
        let params = vec![arr(vec![Value::from("win_viewport"), arr(vec![])])];
        let evs = decode_redraw(&params);
        assert!(matches!(&evs[0], UiEvent::Unknown { name } if name == "win_viewport"));
    }

    #[test]
    fn decodes_hl_attr_define_with_partial_attrs() {
        // real wire args are [id, rgb_attrs, cterm_attrs, info]; rgb_attrs
        // only carries keys nvim actually set for this attribute, so the
        // decoder must default absent keys rather than requiring all six.
        let rgb_attrs = Value::Map(vec![
            (Value::from("foreground"), Value::from(0x00_ff00_u32)),
            (Value::from("bold"), Value::from(true)),
            (Value::from("underline"), Value::from(true)),
            // background, italic, reverse deliberately absent
        ]);
        let params = vec![arr(vec![
            Value::from("hl_attr_define"),
            arr(vec![Value::from(3), rgb_attrs, Value::Map(vec![]), arr(vec![])]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![UiEvent::HlAttrDefine {
                id: 3,
                fg: Some(0x00_ff00),
                bg: None,
                bold: true,
                italic: false,
                underline: true,
                reverse: false,
            }]
        );
    }

    #[test]
    fn decodes_default_colors_set_with_unset_sentinel() {
        // nvim sends -1 for an unset color; the decoder maps it to None so
        // no consumer can mistake it for a valid 24-bit RGB value.
        let params = vec![arr(vec![
            Value::from("default_colors_set"),
            arr(vec![
                Value::from(-1),
                Value::from(0x0000_0000_u32),
                Value::from(0x00ff_ffff_u32),
                Value::from(0),
                Value::from(15),
            ]),
        ])];
        let evs = decode_redraw(&params);
        assert_eq!(
            evs,
            vec![UiEvent::DefaultColorsSet { fg: None, bg: Some(0), sp: Some(0x00ff_ffff) }]
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p view-engine ui_events`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `decode_redraw`**

Implementation notes that are part of the deliverable, not suggestions:
- Iterate each batch entry; entry[0] is the event name; every following element is one argument tuple for that event (one `grid_line` batch entry can carry many lines — emit one `UiEvent` per tuple).
- Every slice pattern is tolerant of trailing fields (`let [grid, ..] = args else { ... }`, not an exact-length pattern) — verified against a live nvim v0.12.4's `nvim_get_api_info().ui_events` metadata, not just the header comment, since `grid_line` alone carries a 5th mandatory `wrap` field beyond `[grid, row, col_start, cells]`.
- `hl_attr_define` args are `[id, rgb_attrs_map, cterm_attrs_map, info]`; read `foreground`/`background`/`bold`/`italic`/`underline`/`reverse` from the rgb map; absent keys → `None`/`false`.
- `default_colors_set` args are `[rgb_fg, rgb_bg, rgb_sp, cterm_fg, cterm_bg]`; decode the first three as `Option<u32>` (nvim sends -1 for unset; the decoder maps any negative to `None`) — unset is unrepresentable as a color, so no consumer can treat -1 as RGB.
- `grid_scroll` args are `[grid, top, bot, left, right, rows, cols]`; `cols` is always 0 and is discarded.
- Anything unrecognized → `UiEvent::Unknown { name }` — the P2 plan turns several of these into typed events; dropping them silently would hide that work.
- Malformed tuples inside a recognized event decode to `Unknown` with the same name rather than panicking (lint wall forbids panics anyway).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p view-engine ui_events`
Expected: 5 passed.

- [ ] **Step 4b: Live-nvim proof (new, `crates/view-engine/tests/redraw_live.rs`)**

Fixture-built tests prove the decoder matches a hand-transcribed wire shape; they do not prove it matches nvim's *actual* wire shape. Spawn a real `Engine`, `nvim_ui_attach` with `ext_linegrid`, drain `notifications` for up to 5s collecting `redraw` batches, run `decode_redraw` over them, and assert at least one `UiEvent::GridLine` and one `UiEvent::Flush` decode (not `Unknown`). This is the test that would have caught the original 4-element `grid_line` pattern.

Run: `cargo test -p view-engine --test redraw_live`
Expected: 1 passed.

- [ ] **Step 5: Commit**

```bash
task commit -- -m "feat(engine): typed ext_linegrid redraw event decoding

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Grid model in view-core

**Files:**
- Create: `crates/view-core/src/grid.rs`
- Modify: `crates/view-core/src/lib.rs` (add `pub mod grid;`)

**Interfaces:**
- Consumes: nothing from other crates — view-core stays dependency-free, so Task 6's bin does the `UiEvent → GridOp` mapping. This task defines the ops.
- Produces:

```rust
pub struct Grid { /* private */ }
pub struct Cell { pub text: String, pub hl_id: u64 }
pub enum GridOp {
    Resize { width: u16, height: u16 },
    Clear,
    CursorGoto { row: u16, col: u16 },
    PutLine { row: u16, col_start: u16, cells: Vec<(String, u64, u64)> }, // (text, hl_id, repeat)
    Scroll { top: u16, bot: u16, left: u16, right: u16, rows: i32 },
}
impl Grid {
    pub fn new() -> Self;
    pub fn apply(&mut self, op: GridOp);
    pub fn size(&self) -> (u16, u16);
    pub fn cursor(&self) -> (u16, u16);
    pub fn cell(&self, row: u16, col: u16) -> Option<&Cell>;
    pub fn row_text(&self, row: u16) -> String; // debug/test convenience
}
```

- [ ] **Step 1: Write the failing tests** (`crates/view-core/src/grid.rs`, bottom)

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn grid_10x3() -> Grid {
        let mut g = Grid::new();
        g.apply(GridOp::Resize { width: 10, height: 3 });
        g
    }

    #[test]
    fn put_line_writes_cells_with_repeat() {
        let mut g = grid_10x3();
        g.apply(GridOp::PutLine {
            row: 0,
            col_start: 2,
            cells: vec![("h".into(), 1, 1), ("i".into(), 1, 1), (".".into(), 0, 3)],
        });
        assert_eq!(g.row_text(0), "  hi...   ");
    }

    #[test]
    fn scroll_up_moves_rows_and_clears_vacated() {
        let mut g = grid_10x3();
        for (i, s) in ["aaaa", "bbbb", "cccc"].iter().enumerate() {
            g.apply(GridOp::PutLine {
                row: i as u16,
                col_start: 0,
                cells: s.chars().map(|c| (c.to_string(), 0, 1)).collect(),
            });
        }
        // rows: 1 means content moves up by one row within the region
        g.apply(GridOp::Scroll { top: 0, bot: 3, left: 0, right: 10, rows: 1 });
        assert_eq!(g.row_text(0).trim_end(), "bbbb");
        assert_eq!(g.row_text(1).trim_end(), "cccc");
        assert_eq!(g.row_text(2).trim_end(), "");
    }

    #[test]
    fn resize_preserves_overlapping_content_and_clamps_cursor() {
        let mut g = grid_10x3();
        g.apply(GridOp::CursorGoto { row: 2, col: 9 });
        g.apply(GridOp::Resize { width: 5, height: 2 });
        assert_eq!(g.size(), (5, 2));
        assert_eq!(g.cursor(), (1, 4));
    }

    #[test]
    fn out_of_bounds_ops_are_ignored_not_panicking() {
        let mut g = grid_10x3();
        g.apply(GridOp::PutLine { row: 99, col_start: 0, cells: vec![("x".into(), 0, 1)] });
        g.apply(GridOp::CursorGoto { row: 99, col: 99 });
        assert_eq!(g.cursor(), (2, 9)); // clamped to bounds
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p view-core`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `Grid`**

Storage: `Vec<Cell>` of `width * height`, row-major; `Cell { text: String, hl_id: u64 }` with `" "`/0 default. Implementation requirements: `PutLine` expands repeats and truncates at the right edge; `Scroll` with positive `rows` copies row `r+rows → r` inside the region top..bot (exclusive), then fills vacated rows with default cells (negative `rows` mirrors downward); `Resize` allocates fresh cells, copies the overlapping rectangle, clamps the cursor; every op bounds-checks and ignores rather than panics. Per-cell `String` is deliberate for correctness-first; the perf pass happens against the P3 bench harness, not by guessing now.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p view-core`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
task commit -- -m "feat(core): grid model with put/scroll/resize semantics

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Key encoding — crossterm KeyEvent → nvim input notation

**Files:**
- Create: `crates/view-tui/src/keys.rs`
- Modify: `crates/view-tui/src/lib.rs` (add `pub mod keys;`)

**Interfaces:**
- Consumes: `crossterm::event::KeyEvent`.
- Produces: `pub fn encode_key(ev: &crossterm::event::KeyEvent) -> Option<String>` — the exact string for `nvim_input`. Task 7 calls this for every key.

- [ ] **Step 1: Add dependencies**

Run: `cargo add crossterm -p view-tui && cargo add ratatui -p view-tui`

- [ ] **Step 2: Write the failing tests** (`crates/view-tui/src/keys.rs`, bottom)

```rust
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn plain_chars_pass_through() {
        assert_eq!(encode_key(&key(KeyCode::Char('a'), KeyModifiers::NONE)).unwrap(), "a");
        assert_eq!(encode_key(&key(KeyCode::Char('A'), KeyModifiers::SHIFT)).unwrap(), "A");
    }

    #[test]
    fn special_chars_nvim_escapes() {
        assert_eq!(encode_key(&key(KeyCode::Char('<'), KeyModifiers::NONE)).unwrap(), "<lt>");
    }

    #[test]
    fn named_keys() {
        assert_eq!(encode_key(&key(KeyCode::Enter, KeyModifiers::NONE)).unwrap(), "<CR>");
        assert_eq!(encode_key(&key(KeyCode::Esc, KeyModifiers::NONE)).unwrap(), "<Esc>");
        assert_eq!(encode_key(&key(KeyCode::Backspace, KeyModifiers::NONE)).unwrap(), "<BS>");
        assert_eq!(encode_key(&key(KeyCode::Tab, KeyModifiers::NONE)).unwrap(), "<Tab>");
        assert_eq!(encode_key(&key(KeyCode::Up, KeyModifiers::NONE)).unwrap(), "<Up>");
        assert_eq!(encode_key(&key(KeyCode::F(5), KeyModifiers::NONE)).unwrap(), "<F5>");
    }

    #[test]
    fn modifier_wrapping() {
        assert_eq!(encode_key(&key(KeyCode::Char('x'), KeyModifiers::CONTROL)).unwrap(), "<C-x>");
        assert_eq!(encode_key(&key(KeyCode::Char('x'), KeyModifiers::ALT)).unwrap(), "<M-x>");
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::CONTROL | KeyModifiers::SHIFT)).unwrap(),
            "<C-S-CR>"
        );
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p view-tui`
Expected: FAIL to compile.

- [ ] **Step 4: Implement `encode_key`**

Requirements: map `KeyCode` to its nvim name (`Char` → the char, with `<` → `<lt>` and `\` staying literal; `Enter/Esc/Backspace/Tab/Up/Down/Left/Right/Home/End/PageUp/PageDown/Delete/Insert/F(n)` → `<CR>/<Esc>/<BS>/<Tab>/<Up>/.../<F{n}>`). If any of CONTROL/ALT/SHIFT are set, wrap as `<{mods}-{name}>` with mods ordered `C`, `S`, `M` skipping SHIFT for plain chars (crossterm already delivers the shifted char). Return `None` for key kinds view doesn't forward (e.g. release events when the kitty protocol reports them). Keep it a pure match — no allocation tricks yet.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p view-tui`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
task commit -- -m "feat(tui): crossterm-to-nvim key notation encoding

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: Terminal guard, paint loop, and the wired `view` binary

**Files:**
- Create: `crates/view-tui/src/terminal.rs` (guard)
- Create: `crates/view-tui/src/paint.rs`
- Modify: `crates/view-tui/src/lib.rs` (add modules)
- Modify: `crates/view/Cargo.toml` (deps: view-core, view-engine, view-tui, crossterm, ratatui, clap, anyhow via `cargo add`)
- Modify: `crates/view/src/main.rs` (full rewrite below)

**Interfaces:**
- Consumes: `Engine::spawn` (Task 3), `decode_redraw` (Task 4), `Grid`/`GridOp` (Task 5), `encode_key` (Task 6).
- Produces: the `view [FILE] --nvim-bin <path>` binary; `TerminalGuard::enter() -> Result<TerminalGuard>` restoring the terminal on drop AND on panic; `paint(grid, hl_table, frame)` used again by P2.

- [ ] **Step 1: Implement `TerminalGuard`** (`crates/view-tui/src/terminal.rs`)

```rust
use std::io::Write;

pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        // a panic must restore the terminal before the message prints, or the
        // user is left with a broken shell and an invisible error
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = std::io::stdout().flush();
}
```

- [ ] **Step 2: Implement `paint`** (`crates/view-tui/src/paint.rs`)

```rust
use ratatui::style::{Color, Style};
use view_core::grid::Grid;

pub struct HlAttr {
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub reverse: bool,
}

pub struct HlTable {
    pub default_fg: Option<u32>,
    pub default_bg: Option<u32>,
    pub attrs: std::collections::HashMap<u64, HlAttr>,
}

/// Largest grid dimension accepted from the wire. Far beyond any physical
/// terminal, small enough that a malformed or desynced `grid_resize`
/// cannot make the grid allocate unboundedly.
const MAX_GRID_DIM: u16 = 2048;

fn clamp_dim(dim: u64) -> u16 {
    saturate_u16(dim).min(MAX_GRID_DIM)
}

// a plain `as u16` cast would wrap out-of-range wire values back into
// range (65536 becomes 0), turning a malformed coordinate into a write at
// a real cell; saturating keeps it out of range so Grid ignores it
fn saturate_u16(v: u64) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

pub fn paint(grid: &Grid, hl: &HlTable, frame: &mut ratatui::Frame<'_>) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    let (w, h) = grid.size();
    for row in 0..h.min(area.height) {
        for col in 0..w.min(area.width) {
            if let Some(cell) = grid.cell(row, col) {
                let out = &mut buf[(area.x + col, area.y + row)];
                out.set_symbol(if cell.text.is_empty() { " " } else { &cell.text });
                out.set_style(style_for(cell.hl_id, hl));
            }
        }
    }
}

fn style_for(hl_id: u64, table: &HlTable) -> Style {
    let mut fg = table.default_fg;
    let mut bg = table.default_bg;
    let mut style = Style::default();
    if let Some(a) = table.attrs.get(&hl_id) {
        if a.fg.is_some() {
            fg = a.fg;
        }
        if a.bg.is_some() {
            bg = a.bg;
        }
        if a.reverse {
            std::mem::swap(&mut fg, &mut bg);
        }
        if a.bold {
            style = style.add_modifier(ratatui::style::Modifier::BOLD);
        }
        if a.italic {
            style = style.add_modifier(ratatui::style::Modifier::ITALIC);
        }
    }
    if let Some(c) = fg {
        style = style.fg(rgb(c));
    }
    if let Some(c) = bg {
        style = style.bg(rgb(c));
    }
    style
}

fn rgb(c: u32) -> Color {
    Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}
```

- [ ] **Step 3: Rewrite `crates/view/src/main.rs`**

Structure (write it exactly; this is the whole file):

```rust
use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{Event, KeyEventKind};
use rmpv::Value;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use view_core::grid::{Grid, GridOp};
use view_engine::process::{Engine, EngineConfig};
use view_engine::ui_events::{decode_redraw, UiEvent};
use view_tui::keys::encode_key;
use view_tui::paint::{paint, HlAttr, HlTable};
use view_tui::terminal::TerminalGuard;

#[derive(Parser)]
#[command(name = "view", about = "A modern terminal editor powered by Neovim")]
struct Cli {
    /// File to open
    file: Option<std::path::PathBuf>,
    /// Path to the nvim binary (defaults to PATH lookup)
    #[arg(long)]
    nvim_bin: Option<std::path::PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut cfg = EngineConfig::default();
    if let Some(bin) = cli.nvim_bin {
        cfg.nvim_bin = bin;
    }
    if let Some(file) = &cli.file {
        cfg.extra_args.push(file.as_os_str().to_owned());
    }
    let mut engine = Engine::spawn(cfg).context("failed to start nvim engine")?;
    // take the receiver once, up front: a Receiver field is !Sync and would
    // make Engine, and Arc<Engine>, not even Send
    let notifications = engine
        .take_notifications()
        .context("notification receiver already taken")?;

    let _guard = TerminalGuard::enter().context("failed to enter raw mode")?;
    let mut terminal = ratatui::init();
    let size = terminal.size()?;

    engine
        .handle
        .request(
            "nvim_ui_attach",
            vec![
                Value::from(size.width),
                Value::from(size.height),
                Value::Map(vec![(Value::from("ext_linegrid"), Value::from(true))]),
            ],
        )
        .context("ui attach failed")?;

    let mut grid = Grid::new();
    let mut hl = HlTable { default_fg: None, default_bg: None, attrs: Default::default() };
    let mut dirty = false;

    loop {
        // engine events: drain whatever is queued, then paint once on flush
        loop {
            match notifications.recv_timeout(Duration::from_millis(4)) {
                Ok(note) if note.method == "redraw" => {
                    for ev in decode_redraw(&note.params) {
                        match ev {
                            UiEvent::GridResize { width, height, .. } => {
                                // clamp untrusted wire dimensions: a desynced or
                                // malformed grid_resize must not allocate
                                // unboundedly, and a plain `as u16` cast would
                                // silently truncate 65536 to 0
                                grid.apply(GridOp::Resize {
                                    width: clamp_dim(width),
                                    height: clamp_dim(height),
                                });
                            }
                            UiEvent::GridLine { row, col_start, cells, .. } => {
                                grid.apply(GridOp::PutLine {
                                    row: saturate_u16(row),
                                    col_start: saturate_u16(col_start),
                                    cells: cells.into_iter().map(|c| (c.text, c.hl_id, c.repeat)).collect(),
                                });
                            }
                            UiEvent::GridCursorGoto { row, col, .. } => {
                                grid.apply(GridOp::CursorGoto { row: saturate_u16(row), col: saturate_u16(col) });
                            }
                            UiEvent::GridScroll { top, bot, left, right, rows, .. } => {
                                grid.apply(GridOp::Scroll {
                                    top: saturate_u16(top),
                                    bot: saturate_u16(bot),
                                    left: saturate_u16(left),
                                    right: saturate_u16(right),
                                    rows: i32::try_from(rows).unwrap_or(if rows > 0 { i32::MAX } else { i32::MIN }),
                                });
                            }
                            UiEvent::GridClear { .. } => grid.apply(GridOp::Clear),
                            UiEvent::HlAttrDefine { id, fg, bg, bold, italic, underline: _, reverse } => {
                                hl.attrs.insert(id, HlAttr { fg, bg, bold, italic, reverse });
                            }
                            UiEvent::DefaultColorsSet { fg, bg, .. } => {
                                hl.default_fg = fg;
                                hl.default_bg = bg;
                            }
                            UiEvent::Flush => dirty = true,
                            UiEvent::Unknown { .. } => {}
                        }
                    }
                }
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    // engine exited (:q). Restore terminal via guards, then
                    // propagate nvim's exit code so :cq flows work. shutdown()
                    // consumes engine and returns the real exit status; Drop
                    // alone (kill+reap, no return value) cannot surface it.
                    // The graceful qa! shutdown() sends is a harmless no-op
                    // here since the connection is already closed — the
                    // child has typically already exited by this point, so
                    // try_wait picks it up on the first poll.
                    ratatui::restore();
                    let status = engine.shutdown()?;
                    std::process::exit(status.code().unwrap_or(0));
                }
            }
        }

        if dirty {
            terminal.draw(|f| paint(&grid, &hl, f))?;
            let (row, col) = grid.cursor();
            terminal.set_cursor_position((col, row))?;
            terminal.show_cursor()?;
            dirty = false;
        }

        // input: poll without blocking the paint path
        while crossterm::event::poll(Duration::ZERO)? {
            match crossterm::event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    if let Some(notation) = encode_key(&k) {
                        // fire-and-forget: a blocking request here would hold
                        // the paint path hostage to nvim's response latency
                        engine.handle.notify("nvim_input", vec![Value::from(notation)])?;
                    }
                }
                Event::Resize(w, h) => {
                    engine.handle.notify(
                        "nvim_ui_try_resize",
                        vec![Value::from(w), Value::from(h)],
                    )?;
                }
                _ => {}
            }
        }
    }
}
```

Add dependencies first: `cargo add view-core view-engine view-tui --path-hint` equivalents — concretely:

```bash
cargo add view-core -p view --path crates/view-core
cargo add view-engine -p view --path crates/view-engine
cargo add view-tui -p view --path crates/view-tui
cargo add clap -p view --features derive
cargo add anyhow -p view
cargo add rmpv -p view
cargo add crossterm -p view
cargo add ratatui -p view
```

- [ ] **Step 4: Verify by hand — the P1 milestone moment**

Run: `cargo run -p view -- README.md`
Expected: README opens in a modal editor; `i`, typing, `<Esc>`, `:q<CR>` all work; on exit the shell is intact. Then verify the panic path: temporarily insert `panic!("test")` after attach, run, confirm the terminal is restored and the message is visible, remove it.

- [ ] **Step 5: Automated smoke test via pty** (`crates/view-oracle/tests/smoke.rs`)

```bash
cargo add portable-pty -p view-oracle --dev
cargo add vt100 -p view-oracle --dev
```

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
use portable_pty::{CommandBuilder, NativePtySize, PtySystem};

#[test]
fn view_paints_typed_text_in_a_pty() {
    let pty = portable_pty::native_pty_system();
    let pair = pty
        .openpty(portable_pty::PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_view"));
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();

    std::thread::sleep(std::time::Duration::from_millis(1500));
    writer.write_all(b"ihello from view").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(700));

    let mut parser = vt100::Parser::new(24, 80, 0);
    let mut buf = [0u8; 65536];
    // non-blocking-ish drain: read whatever arrived
    if let Ok(n) = reader.read(&mut buf) {
        parser.process(&buf[..n]);
    }
    let screen = parser.screen().contents();
    assert!(screen.contains("hello from view"), "screen was:\n{screen}");

    writer.write_all(b"\x1b:q!\r").unwrap();
    writer.flush().unwrap();
    let _ = child.wait();
}
```

Implementer note: `CARGO_BIN_EXE_view` requires `view` as a dev-dependency-visible binary — add `[dev-dependencies] view = { path = "../view" }` to `view-oracle/Cargo.toml` if the env var is absent, or run the binary via `cargo build -p view` + `target/debug/view` path resolution. Fix the imports to match portable-pty's current API surface (names above are indicative; the crate's API is authoritative — check its docs, do not guess).

Run: `cargo test -p view-oracle --test smoke`
Expected: PASS. If timing flakes, raise sleeps — this is a smoke test; the real quiesce protocol arrives in P3.

- [ ] **Step 6: Commit**

```bash
task commit -- -m "feat: view binary paints an editable embedded nvim in the terminal

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: Latency measurement v0 — paired view vs nvim

**Files:**
- Create: `crates/view-bench/src/bin/latency.rs`
- Modify: `crates/view-bench/Cargo.toml` (portable-pty, vt100 as regular deps; `[[bin]] name = "latency"`)
- Modify: `Taskfile.yml` (add `bench-latency` task)

**Interfaces:**
- Consumes: the `view` binary from Task 7; system `nvim`.
- Produces: `task bench-latency` printing a paired p50/p99 table; the seed of the spec-3.4 harness P3 formalizes.

- [ ] **Step 1: Implement the harness**

One binary, run twice per invocation (once against `view`, once against `nvim`), same procedure: spawn the target in a pty (24×80), wait 2 s for readiness, enter insert mode, then N=200 iterations of: write one character, poll the pty output through vt100 until that character appears on screen, record elapsed; between samples sleep 10 ms. Print:

```
target  p50_ms  p99_ms  max_ms  samples
view     x.xx    x.xx    x.xx     200
nvim     x.xx    x.xx    x.xx     200
ratio    x.xx    x.xx
```

Implementation requirements: reuse the pty+vt100 pattern from Task 7 Step 5; measure with `std::time::Instant`; sort samples for percentiles; take the target binary path and label from argv (`latency <label> <path-to-binary>`), and add the Taskfile target:

```yaml
  bench-latency:
    desc: Paired keypress-to-paint latency, view vs nvim
    cmds:
      - cargo build --release -p view
      - cargo run --release -p view-bench --bin latency -- view target/release/view
      - cargo run --release -p view-bench --bin latency -- nvim nvim
```

- [ ] **Step 2: Run it and record the first baseline**

Run: `task bench-latency`
Expected: both tables print with nonzero sensible numbers (single-digit-to-low-tens of ms end-to-end). Save the output to `.claude/bench-baselines/p1-latency.txt` verbatim, with date and machine description appended. These numbers are *informational* this phase — the spec's gates activate in P3 with the real measurement protocol; what matters now is that measurement exists from the first week and any grotesque regression is visible immediately.

- [ ] **Step 3: Commit**

```bash
task commit -- -m "feat(bench): paired keypress-to-paint latency harness v0

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## P1 Exit Checklist

- [ ] Confirm the main loop sends `nvim_input` and `nvim_ui_try_resize` via
      `notify` (fire-and-forget through the writer thread), keeping the
      paint-loop-never-awaits-RPC rule intact with no tracked exception.
- [ ] `task ci` green.
- [ ] Manual session: open a real file, edit, save, quit; terminal intact; `:cq` propagates a nonzero exit code (`view x; echo $?`).
- [ ] `.claude/bench-baselines/p1-latency.txt` exists with real measured output.
- [ ] `.claude/known-bugs.md` drained or user-approved deferrals only.
- [ ] Dogfooding note appended to `.claude/dogfood-journal.md`.
- [ ] P2 plan authored against the interfaces this phase actually produced (update `.claude/plans/INDEX.md`).
