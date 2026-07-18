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
    /// frame. Bounded by [`UI_ATTACH_TIMEOUT`] rather than unbounded, since
    /// the caller has typically already put the terminal into raw mode by
    /// this point, and an unresponsive engine must not freeze it forever.
    ///
    /// # Errors
    ///
    /// Returns the `EngineError` from the underlying request if it fails,
    /// nvim rejects the attach, or the reply does not arrive within
    /// [`UI_ATTACH_TIMEOUT`].
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
}
