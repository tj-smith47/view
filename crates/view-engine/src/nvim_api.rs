//! Typed convenience wrappers for the specific nvim RPC calls the terminal
//! frontend needs, so no caller outside this crate has to construct an
//! `rmpv::Value` by hand. `scripts/audit-deps.sh` forbids the bin crate
//! `view` from depending on `rmpv` directly; these methods are the sanctioned
//! way for it to reach the same calls.

use crate::handle::{EngineError, EngineHandle};
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
        self.request_timeout(
            "nvim_ui_attach",
            vec![
                Value::from(width),
                Value::from(height),
                Value::Map(vec![
                    (Value::from("ext_linegrid"), Value::from(true)),
                    (Value::from("ext_cmdline"), Value::from(true)),
                    (Value::from("ext_popupmenu"), Value::from(true)),
                    (Value::from("ext_messages"), Value::from(true)),
                    (Value::from("ext_tabline"), Value::from(true)),
                ]),
            ],
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
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::rpc::RpcMessage;
    use std::io::{BufReader, Write};

    /// A minimal fake peer that answers every incoming request with a nil
    /// success response and forwards `(method, params)` to the returned
    /// channel, so a test can assert on the exact wire shape a typed
    /// wrapper sends without a real nvim.
    fn fake_peer_capturing_requests() -> (
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
                        result: Value::Nil,
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
        let (h, cap_rx) = fake_peer_capturing_requests();
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
        let (h, cap_rx) = fake_peer_capturing_requests();
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
}
