//! The hold an open picker puts on the toast stack.

use super::Messages;

impl Messages {
    /// Sets whether an open picker holds the toast stack, idempotent when
    /// the state already matches.
    ///
    /// The picker paints over the toasts, so a notice that arrives while it
    /// is open would spend its timeout where nobody can read it. While held,
    /// [`Self::paused`] is on: the notice waits in the stack with no
    /// dismissal timer, and the falling edge gives the top slot a whole
    /// timeout once the picker closes. Set every fold from the overlay
    /// stack, beside [`Self::set_pane_held`].
    pub(crate) fn set_picker_held(&mut self, held: bool) {
        if self.picker_held == held {
            return;
        }
        let was_paused = self.paused();
        self.picker_held = held;
        self.after_pause_change(was_paused);
    }
}
