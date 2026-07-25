//! Typed convenience wrappers for the specific nvim RPC calls the terminal
//! frontend needs, so no caller outside this crate has to construct an
//! `rmpv::Value` by hand. `scripts/audit-deps.sh` forbids the bin crate
//! `view` from depending on `rmpv` directly; these methods are the sanctioned
//! way for it to reach the same calls.

use crate::handle::{EngineError, EngineHandle};
use crate::rpc::RpcError;
use rmpv::Value;
use std::time::Duration;

/// Upper bound on how long [`EngineHandle::ui_attach`] waits for nvim's
/// reply before giving up.
///
/// The caller issues this request after the terminal has already entered
/// raw mode; an unbounded wait against a wedged engine would leave the
/// terminal in that state with no way out short of killing the process from
/// outside.
const UI_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on how long [`EngineHandle::register_vim_enter_autocmd`]
/// waits for nvim's reply. Same rationale as [`UI_ATTACH_TIMEOUT`]: this
/// runs during startup, before the paint loop's own unbounded-notify regime
/// begins, so it still needs a bound.
const REGISTER_VIM_ENTER_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on how long [`EngineHandle::eval_str`] waits for nvim's
/// reply. Callers of this probe are test/oracle harnesses driving their own
/// bounded polling loops (see `view-oracle`'s `EngineSession`), never the
/// paint loop itself, but an unbounded wait against a wedged engine would
/// still hang whatever harness is blocked on the answer.
const EVAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on how long [`EngineHandle::get_mode`] waits for nvim's
/// reply. `nvim_get_mode` is answered on receipt even while nvim's main
/// loop is busy or blocked (see [`EngineHandle::get_mode`]), so a healthy
/// engine replies near-instantly; this bound only covers a dead or wedged
/// connection.
const GET_MODE_TIMEOUT: Duration = Duration::from_secs(5);

/// The `ext_*` UI capabilities [`EngineHandle::ui_attach`] requests. Public
/// so a corpus/oracle runner attaching its own reference connection can
/// request the identical set nvim sees from the real paint loop, rather
/// than restating the list and risking the two drifting apart.
pub const UI_EXT_OPTIONS: &[&str] = &[
    "ext_linegrid",
    "ext_cmdline",
    "ext_popupmenu",
    "ext_messages",
    "ext_tabline",
];

impl EngineHandle {
    /// Attaches this connection as nvim's UI at `width` x `height` cells
    /// with the full set of native-rendering extensions enabled:
    /// `ext_linegrid`, `ext_cmdline`, `ext_popupmenu`, `ext_messages`, and
    /// `ext_tabline`. Without these, nvim falls back to painting cmdline,
    /// messages, popupmenu, and tabline content directly into the grid,
    /// which this frontend has no way to distinguish from ordinary buffer
    /// text; attaching all five up front is what makes
    /// [`crate::ui_events::decode_redraw`]'s mode/cmdline/messages/tabline/
    /// popupmenu variants reachable at all.
    ///
    /// A `request`, not a `notify`: the caller needs to know attach succeeded
    /// before entering the paint loop. This is the only request the paint
    /// loop's setup makes; every nvim call issued once the loop is running
    /// goes through `notify` instead, so a slow response never stalls a
    /// frame. Bounded by `UI_ATTACH_TIMEOUT` rather than unbounded, since
    /// the caller has typically already put the terminal into raw mode by
    /// this point, and an unresponsive engine must not freeze it forever.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// nvim rejects the attach, or the reply does not arrive within
    /// `UI_ATTACH_TIMEOUT`.
    pub fn ui_attach(&self, width: u16, height: u16) -> Result<(), EngineError> {
        let opts = UI_EXT_OPTIONS
            .iter()
            .map(|&name| (Value::from(name), Value::from(true)))
            .collect();
        self.request_timeout(
            "nvim_ui_attach",
            vec![Value::from(width), Value::from(height), Value::Map(opts)],
            UI_ATTACH_TIMEOUT,
        )?;
        Ok(())
    }

    /// Registers a one-shot `VimEnter` autocmd whose callback issues a
    /// BLOCKING `rpcrequest(channel_id, 'view_vim_enter')` back to this
    /// connection -- the end-to-end proof that `update()`'s
    /// `Msg::EngineRequest(EngineRequest::VimEnter)` arm and its
    /// `Effect::Reply` actually unblock nvim's own main loop, not merely
    /// that the message decodes (a deadlock here hangs startup forever).
    ///
    /// # Ordering: call this BEFORE [`ui_attach`](Self::ui_attach), never
    /// after
    ///
    /// Live-verified against a real `nvim --clean --embed` (see
    /// `.superpowers/sdd/p2-task-10-report.md` for the captured transcript):
    /// registering this autocmd immediately AFTER `ui_attach` returns loses
    /// the race entirely -- a `--clean` startup's config sourcing and
    /// `VimEnter` dispatch were both already complete (300+ redraw damage
    /// events already staged) by the time the registration request even
    /// reached nvim's main loop. The embed contract's "attach precedes
    /// config sourcing" guarantee protects exactly the window BEFORE
    /// `ui_attach`: nvim services ordinary requests on this connection
    /// freely while blocked waiting for a UI to attach, but cannot begin
    /// sourcing config (and thus cannot fire `VimEnter`) until `ui_attach`
    /// itself returns. Registering here, before that call, is what actually
    /// wins the race; after it is not "usually late", it is unconditionally
    /// too late for a `--clean`-speed startup.
    ///
    /// `channel_id` is this connection's own id from `nvim_get_api_info`
    /// (captured in [`crate::process::Engine::api_info`] at spawn time): a
    /// self-targeted `rpcrequest` needs an explicit channel number, and nvim
    /// has no "loopback" shorthand for dispatching a request back to the
    /// very connection asking.
    ///
    /// A `request`, not a `notify`, for the same reason [`ui_attach`]
    /// (Self::ui_attach) is: the caller needs to know the autocmd is live
    /// before it dares call `ui_attach`, or config sourcing could start
    /// racing an unregistered hook.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// nvim rejects the command, or the reply does not arrive within
    /// [`REGISTER_VIM_ENTER_TIMEOUT`].
    pub fn register_vim_enter_autocmd(&self, channel_id: u64) -> Result<(), EngineError> {
        let cmd =
            format!("autocmd VimEnter * ++once call rpcrequest({channel_id}, 'view_vim_enter')");
        self.request_timeout(
            "nvim_command",
            vec![Value::from(cmd)],
            REGISTER_VIM_ENTER_TIMEOUT,
        )?;
        Ok(())
    }

    /// Forwards one encoded key `notation` (see `view_tui::keys::encode_key`)
    /// to nvim via `nvim_input`.
    ///
    /// Fire-and-forget: the paint loop calls this once per keystroke and must
    /// never block waiting for nvim to process it, or one slow keystroke
    /// stalls every frame queued behind it.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn input(&self, notation: &str) -> Result<(), EngineError> {
        self.notify("nvim_input", vec![Value::from(notation)])
    }

    /// Notifies nvim of a terminal resize to `width` x `height` cells via
    /// `nvim_ui_try_resize`.
    ///
    /// Fire-and-forget for the same reason as [`input`](Self::input): resize
    /// events arrive inside the paint loop and must not block it.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn try_resize(&self, width: u16, height: u16) -> Result<(), EngineError> {
        self.notify(
            "nvim_ui_try_resize",
            vec![Value::from(width), Value::from(height)],
        )
    }

    /// Streams `text` into nvim via `nvim_paste` as a single non-streamed
    /// call (`phase = -1`, per `nvim --api-info`'s
    /// `nvim_paste(String data, Boolean crlf, Integer phase)` signature),
    /// with no line-ending translation (`crlf = false`): terminal input
    /// already arrives with the pty's own newline convention, so nvim must
    /// not perform an additional CRLF fixup on top of it. Routing paste
    /// through `nvim_paste` rather than replaying it as `nvim_input`
    /// keystrokes avoids mid-paste mappings, autoindent mangling, and a
    /// separate undo unit per line.
    ///
    /// Fire-and-forget for the same reason as [`input`](Self::input): a
    /// bracketed paste must not block the paint loop waiting for nvim to
    /// finish inserting it.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn paste(&self, text: &str) -> Result<(), EngineError> {
        self.notify(
            "nvim_paste",
            vec![Value::from(text), Value::from(false), Value::from(-1)],
        )
    }

    /// Forwards one mouse event to nvim via `nvim_input_mouse`, per
    /// `nvim --api-info`'s `nvim_input_mouse(String button, String action,
    /// String modifier, Integer grid, Integer row, Integer col)` signature
    /// (verified with a live capture, not memory: the parameter order and
    /// names come straight from that decode). `grid` is hardcoded to `0`
    /// (single-grid semantics per the same doc: "0 to let Nvim decide
    /// positioning of windows"), since this frontend has no multigrid
    /// window layout of its own to report.
    ///
    /// Fire-and-forget for the same reason as [`input`](Self::input): a
    /// mouse event arrives inside the paint loop and must not block it.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection's writer thread has
    /// already exited.
    pub fn input_mouse(
        &self,
        button: &str,
        action: &str,
        modifier: &str,
        row: u16,
        col: u16,
    ) -> Result<(), EngineError> {
        self.notify(
            "nvim_input_mouse",
            vec![
                Value::from(button),
                Value::from(action),
                Value::from(modifier),
                Value::from(0),
                Value::from(row),
                Value::from(col),
            ],
        )
    }

    /// Evaluates `expr` via `nvim_eval` and renders the result as a string,
    /// the state-parity probe engine-attached oracles use to compare their
    /// decoded screen state against nvim's own ground truth (buffer text,
    /// cursor position, mode, register contents -- any vimscript expression
    /// a probe needs to read back).
    ///
    /// `nvim_eval(String expr) -> Object` (verified via a live `nvim
    /// --api-info` capture, decoded with `rmpv`: a single positional string
    /// argument, a msgpack `Object` result of whatever type the expression
    /// itself evaluates to -- `getline(1)` returns a String, `line('.')`
    /// returns an Integer, `mode()` returns a String). Rendered by
    /// [`value_to_string`] into a plain `String` rather than leaking
    /// `rmpv::Value` past the engine boundary: `scripts/audit-deps.sh`
    /// confines `rmpv` to `view-engine`, and this is the sanctioned way a
    /// typed caller (the oracle's `EngineSession`) reaches the same result
    /// without constructing or matching on a wire value itself.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// nvim rejects the expression (a vimscript error), or the reply does
    /// not arrive within [`EVAL_TIMEOUT`].
    pub fn eval_str(&self, expr: &str) -> Result<String, EngineError> {
        let value = self.request_timeout("nvim_eval", vec![Value::from(expr)], EVAL_TIMEOUT)?;
        Ok(value_to_string(&value))
    }

    /// Reads nvim's current mode name and blocked flag via `nvim_get_mode`
    /// (`nvim_get_mode() -> {"mode": String, "blocking": Boolean}`, the
    /// mode string in `mode(1)`'s own format). Unlike every other request
    /// here, `nvim_get_mode` is one of the few the pinned nvim documents as
    /// non-deferred, or `fast` (`:help api-fast` names it outright): nvim
    /// answers it immediately on receipt, even while its main loop is
    /// blocked waiting for a key -- a hit-enter prompt, a pending
    /// `t`/`f`/`r` character argument, a register name after `"` -- states
    /// in which a deferred request like `nvim_eval` waits until the key
    /// arrives (live-verified against the pinned nvim: `nvim_eval` times
    /// out in every `blocking = true` state this reply reports, while this
    /// call still answers). That makes it the one probe an embedded driver
    /// can use to distinguish "engine is wedged" from "engine is
    /// deliberately waiting for a key", which is what `view-oracle`'s
    /// quiesce and snapshot machinery calls it for.
    ///
    /// The API metadata is no second opinion on any of that, in either
    /// direction: the pinned engine reports no per-function `fast` flag at
    /// all (absent on every entry, including functions its own
    /// documentation names as `fast`), so a method's flag reading as unset
    /// there is the absence of an answer rather than a negative one.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// the reply does not arrive within [`GET_MODE_TIMEOUT`], or the reply
    /// is not the documented map shape (surfaced as
    /// [`RpcError::Malformed`] rather than degraded to a placeholder a
    /// differential comparison could silently accept on both sides).
    pub fn get_mode(&self) -> Result<(String, bool), EngineError> {
        let value = self.request_timeout("nvim_get_mode", vec![], GET_MODE_TIMEOUT)?;
        let malformed =
            || EngineError::Rpc(RpcError::Malformed(format!("nvim_get_mode reply: {value}")));
        let Value::Map(pairs) = &value else {
            return Err(malformed());
        };
        let mut mode = None;
        let mut blocking = None;
        for (key, val) in pairs {
            match key.as_str() {
                Some("mode") => mode = val.as_str().map(str::to_string),
                Some("blocking") => blocking = val.as_bool(),
                _ => {}
            }
        }
        match (mode, blocking) {
            (Some(mode), Some(blocking)) => Ok((mode, blocking)),
            _ => Err(malformed()),
        }
    }

    /// Issues `nvim_get_hl(0, {name = "Normal"})` as an async probe tagged
    /// with `generation`, resolving the wire ambiguity in
    /// `default_colors_set`'s `rgb_bg`/`rgb_fg` (nvim sends `0` both for
    /// "unset" and for "genuinely black/default-fg-colored"; a probe
    /// reply's `fg`/`bg` map key presence is what disambiguates the two --
    /// see [`crate::handle::EngineHandle::request_probe`]'s doc comment for
    /// the live-verified reply shapes). Async by construction: this issues
    /// the request via [`EngineHandle::request_probe`] and returns
    /// immediately; the reply crosses back as `Msg::HlProbeReply` through
    /// the connection's pump, never by blocking this call.
    ///
    /// # Errors
    ///
    /// Returns `EngineError::Closed` if the connection is already closed or
    /// the writer thread has already exited.
    pub fn probe_default_hl(&self, generation: u64) -> Result<(), EngineError> {
        self.request_probe(
            "nvim_get_hl",
            vec![
                Value::from(0),
                Value::Map(vec![(Value::from("name"), Value::from("Normal"))]),
            ],
            generation,
        )
    }
}

/// Renders an `nvim_eval` result as plain text for [`EngineHandle::eval_str`].
///
/// `Value`'s own `Display` impl is unsuitable: `rmpv::Utf8String::fmt`
/// formats through `Debug`, so a vimscript string result like `getline(1)`'s
/// `"hello"` would round-trip as the quoted literal `"\"hello\""` rather
/// than the bare `hello` a text-comparison oracle needs (`s.as_str()`
/// returning `None`, an ill-formed UTF-8 string on the wire, falls back to
/// a lossy conversion rather than silently dropping the reply). `Array`/
/// `Map`/`Binary`/`Ext` results (no probe this crate exposes evaluates to
/// one today) fall through to `Value`'s own `Display` rendering, which is
/// still total -- just not this function's primary concern.
fn value_to_string(value: &Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::String(s) => s.as_str().map_or_else(
            || String::from_utf8_lossy(s.as_bytes()).into_owned(),
            str::to_string,
        ),
        Value::Integer(i) => i.to_string(),
        Value::F32(f) => f.to_string(),
        Value::F64(f) => f.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::rpc::RpcMessage;
    use std::io::{BufReader, Write};

    /// A minimal fake peer that answers every incoming request with
    /// `result` and forwards `(method, params)` to the returned channel, so
    /// a test can assert on the exact wire shape a typed wrapper sends
    /// without a real nvim. Pass `Value::Nil` for tests that only care
    /// about the outgoing request shape, not the reply.
    fn fake_peer_replying_with(
        result: Value,
    ) -> (
        EngineHandle,
        std::sync::mpsc::Receiver<(String, Vec<Value>)>,
    ) {
        let (peer_read, our_write) = std::io::pipe().unwrap();
        let (our_read, mut peer_write) = std::io::pipe().unwrap();
        let (cap_tx, cap_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(peer_read);
            while let Ok(v) = rmpv::decode::read_value(&mut r) {
                if let Ok(RpcMessage::Request {
                    msgid,
                    method,
                    params,
                }) = RpcMessage::from_value(v)
                {
                    let _ = cap_tx.send((method, params));
                    let resp = RpcMessage::Response {
                        msgid,
                        error: Value::Nil,
                        result: result.clone(),
                    };
                    if rmpv::encode::write_value(&mut peer_write, &resp.to_value()).is_err() {
                        break;
                    }
                    if peer_write.flush().is_err() {
                        break;
                    }
                }
            }
        });
        let (h, _notif_rx) = EngineHandle::start(our_read, our_write);
        (h, cap_rx)
    }

    /// Pins the exact vimscript shape live-verified against a real `nvim
    /// --clean --embed` (see `.superpowers/sdd/p2-task-10-report.md`):
    /// `++once` (self-clearing, never fires twice), plain `rpcrequest` (not
    /// `rpcnotify` -- the spec mandates blocking here), targeting
    /// `channel_id` explicitly (nvim has no loopback shorthand).
    #[test]
    fn register_vim_enter_autocmd_sends_the_exact_verified_vimscript_shape() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        h.register_vim_enter_autocmd(7).unwrap();
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_command");
        assert_eq!(
            params,
            vec![Value::from(
                "autocmd VimEnter * ++once call rpcrequest(7, 'view_vim_enter')"
            )]
        );
    }

    #[test]
    fn ui_attach_sends_the_full_ext_set() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        h.ui_attach(80, 24).unwrap();
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_ui_attach");
        assert_eq!(params[0], Value::from(80));
        assert_eq!(params[1], Value::from(24));
        let Value::Map(opts) = &params[2] else {
            unreachable!("expected an options map, got {:?}", params[2]);
        };
        for ext in [
            "ext_linegrid",
            "ext_cmdline",
            "ext_popupmenu",
            "ext_messages",
            "ext_tabline",
        ] {
            assert!(
                opts.iter()
                    .any(|(k, v)| k.as_str() == Some(ext) && v.as_bool() == Some(true)),
                "missing or false {ext} in ui_attach options"
            );
        }
    }

    #[test]
    fn eval_str_sends_the_expression_as_a_single_positional_string() {
        let (h, cap_rx) = fake_peer_replying_with(Value::Nil);
        let _ = h.eval_str("getline(1)");
        let (method, params) = cap_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "nvim_eval");
        assert_eq!(params, vec![Value::from("getline(1)")]);
    }

    #[test]
    fn eval_str_renders_a_string_result_bare() {
        let (h, _cap_rx) = fake_peer_replying_with(Value::from("hello"));
        assert_eq!(h.eval_str("getline(1)").unwrap(), "hello");
    }

    #[test]
    fn eval_str_renders_an_integer_result_as_decimal() {
        let (h, _cap_rx) = fake_peer_replying_with(Value::from(42));
        assert_eq!(h.eval_str("line('.')").unwrap(), "42");
    }
}
