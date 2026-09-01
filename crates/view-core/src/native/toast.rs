//! Toast routing: where a `msg_show` (and future sibling redraw event)
//! lands, and the idle-expiry/scrollback state that follows once it has
//! landed as a transient toast. The routing table lives in [`route`]; the
//! reachability of each of its arms through today's `UiEvent::MsgShow`-only
//! call site (three are presently unreachable, pending the `UiEvent`
//! variants that would decode their sibling redraw events) is captured live
//! against the pinned engine in `docs/toast-routing-wire-capture.md` --
//! consult it before changing which kinds route where.

use std::collections::VecDeque;
use std::time::Duration;

use crate::model::MessageEntry;

/// Where a message routes once [`route`] classifies its `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// A confirm-class prompt; rendered as the modal `OverlayKind::Prompt`
    /// overlay elsewhere, not a toast at all.
    Prompt,
    /// One of the kinds that stand until they are taken down
    /// (`MessageEntry::is_persistent_kind`: nvim's error/warning kinds and
    /// view's own `"native_sticky"`), or a locally-raised condition. Never
    /// expires on its own: it stays
    /// until replaced, cleared by nvim, or deliberately dismissed
    /// ([`crate::model::Messages::dismiss_sticky`]).
    Sticky,
    /// A statusline-owned kind (mode/pending-count/ruler/search-count
    /// indicators), meant for the statusline surface rather than the toast
    /// stack once a consumer renders it there.
    Statusline,
    /// Everything else: an ordinary toast that expires on its own after
    /// [`TRANSIENT_TOAST_TIMEOUT`] with no other input.
    Transient,
    /// A [`Route::Transient`] message that arrived while the startup hold
    /// was still open ([`StartupHold`]): recorded to scrollback exactly as
    /// it would have been, and parked instead of taking a toast slot. Only
    /// [`route_under_hold`] answers this, and only for foreign traffic --
    /// view's own notices, prompts, statusline kinds and the persistent
    /// kinds are never held (see that function).
    HistoryOnly,
}

/// Whether a session is still in the window where a foreign startup message
/// is parked rather than toasted, and what the resolution of that window
/// decided.
///
/// Opens at [`Messages::default`](crate::model::Messages) -- before the
/// engine is spawned, before attach, before nvim sources a line of config --
/// because the traffic it exists to catch (a plugin's setup-time complaint)
/// arrives in the first redraw batch after attach, ahead of `VimEnter` and
/// of any RPC round trip view could take to decide with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StartupHold {
    /// Still open: a foreign transient message is parked, and may still be
    /// released onto the stack.
    #[default]
    Pending,
    /// A claimant was found and named, so the parked messages stay in the
    /// history and the ones still arriving from the same startup join them.
    /// The notice standing on screen is what explains where they went.
    Collapsed,
    /// Over: every message routes exactly as it did before this mechanism
    /// existed.
    Off,
}

impl StartupHold {
    /// Whether a foreign transient message arriving now is parked rather
    /// than stacked.
    #[must_use]
    pub fn holds(self) -> bool {
        matches!(self, Self::Pending | Self::Collapsed)
    }
}

/// What resolving the startup hold does with what it parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldOutcome {
    /// A claimant is named on screen: the parked messages stay in the
    /// history ring, reachable from the message-history overlay, and the
    /// hold keeps parking until the first keypress so a late complaint from
    /// the same startup lands beside them rather than on top of the notice.
    Collapse,
    /// Nothing to explain: the parked messages drain onto the stack in
    /// arrival order and the session behaves exactly as it did before this
    /// mechanism existed.
    Release,
}

/// [`route`] under a startup hold: the same table, except that a foreign
/// transient message is parked ([`Route::HistoryOnly`]) while `hold` still
/// holds.
///
/// Four classes are never parked, and the last is what keeps the collapse
/// honest. Persistent kinds, prompts and statusline kinds are excluded by
/// construction -- [`route`] never calls them `Transient` -- and view's own
/// notices are excluded here: view is not a claimant and never speaks in
/// one's name, so a broken-config line, a startup key-buffer warning or any
/// other line view raises about itself paints immediately, conflict or no
/// conflict. Without that a config typo the user needs to see would be
/// demoted behind a notice that never mentions it.
#[must_use]
pub fn route_under_hold(kind: &str, hold: StartupHold) -> Route {
    match route(kind) {
        Route::Transient if hold.holds() && !MessageEntry::is_native_kind(kind) => {
            Route::HistoryOnly
        }
        other => other,
    }
}

/// The routing table, one match -- the table IS the implementation.
///
/// `"msg_showmode"` / `"msg_showcmd"` / `"msg_ruler"` are real arms here but
/// unreachable through today's `UiEvent::MsgShow`-only call site: the wire
/// capture confirms nvim never nests them inside a `msg_show` `kind`, so
/// nothing feeds those literal strings into this function until a future
/// `UiEvent` variant decodes the sibling redraw events they actually arrive
/// as. `"search_count"` (documented) and `"search_cmd"` (observed live) are
/// genuine `msg_show` kinds and route through this today. See
/// `docs/toast-routing-wire-capture.md` for the full evidence.
#[must_use]
pub fn route(kind: &str) -> Route {
    match kind {
        "confirm" | "return_prompt" => Route::Prompt,
        k if MessageEntry::is_persistent_kind(k) => Route::Sticky,
        "msg_showmode" | "msg_showcmd" | "msg_ruler" | "search_count" => Route::Statusline,
        _ => Route::Transient,
    }
}

/// How long a `Route::Transient` toast stays visible with no further input
/// before the idle-expiry timer (`Effect::ScheduleToastExpiry`) retires it.
pub const TRANSIENT_TOAST_TIMEOUT: Duration = Duration::from_secs(4);

/// The idle-expiry timeout owed to a route, or `None` when the route never
/// expires on its own. Only `Route::Transient` schedules
/// `Effect::ScheduleToastExpiry`; a prompt is dismissed by an accepted key,
/// a sticky entry by an explicit clear/replace or a deliberate dismissal
/// ([`crate::model::Messages::dismiss_sticky`]), and a statusline entry by
/// nvim's own next update to the same slot.
#[must_use]
pub fn timeout_for(route: Route) -> Option<Duration> {
    match route {
        Route::Transient => Some(TRANSIENT_TOAST_TIMEOUT),
        Route::Prompt | Route::Sticky | Route::Statusline | Route::HistoryOnly => None,
    }
}

/// The default [`ToastHistory`] ring size: enough scrollback for a
/// `:messages`-style view reachable from the palette without holding an
/// unbounded session history.
pub const DEFAULT_CAPACITY: usize = 200;

/// Bounded scrollback of every message that has passed through [`route`],
/// independent of `Messages::entries`' own visible-toast lifetime: an entry
/// dismissed, expired, or evicted from the live toast stack still stays
/// here until the ring itself overflows.
#[non_exhaustive]
pub struct ToastHistory {
    capacity: usize,
    entries: VecDeque<MessageEntry>,
}

impl ToastHistory {
    /// A history ring sized to [`DEFAULT_CAPACITY`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A history ring holding at most `capacity` entries (clamped to at
    /// least one -- a zero-capacity ring would silently discard every push
    /// and defeat the point of a history view entirely).
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: VecDeque::new(),
        }
    }

    /// Appends `e`, evicting the oldest entry once `capacity` is exceeded.
    pub fn push(&mut self, e: &MessageEntry) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(e.clone());
    }

    /// Newest-first; the palette's message-history view reads it.
    pub fn entries(&self) -> impl Iterator<Item = &MessageEntry> {
        self.entries.iter().rev()
    }
}

impl Default for ToastHistory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::model::Messages;

    fn entry(kind: &str, text: &str) -> MessageEntry {
        // MessageEntry has no public constructor -- built the same way
        // `model.rs`'s and `prompt.rs`'s own tests do, through a real
        // `Messages::push`.
        let mut messages = Messages::default();
        messages.push(kind.to_string(), vec![(0, text.to_string())], false);
        messages.entries.into_iter().next().expect("just pushed")
    }

    #[test]
    fn route_matches_the_severity_to_route_mapping_exactly() {
        assert_eq!(route("confirm"), Route::Prompt);
        assert_eq!(route("return_prompt"), Route::Prompt);
        for kind in [
            "emsg",
            "echoerr",
            "wmsg",
            "lua_error",
            "rpc_error",
            "shell_err",
        ] {
            assert_eq!(route(kind), Route::Sticky, "kind: {kind}");
        }
        for kind in ["msg_showmode", "msg_showcmd", "msg_ruler", "search_count"] {
            assert_eq!(route(kind), Route::Statusline, "kind: {kind}");
        }
        for kind in ["echomsg", "echo", "search_cmd", "native", "progress", ""] {
            assert_eq!(route(kind), Route::Transient, "kind: {kind}");
        }
    }

    #[test]
    fn timeout_for_is_some_only_for_transient() {
        assert_eq!(timeout_for(Route::Transient), Some(TRANSIENT_TOAST_TIMEOUT));
        assert_eq!(timeout_for(Route::Prompt), None);
        assert_eq!(timeout_for(Route::Sticky), None);
        assert_eq!(timeout_for(Route::Statusline), None);
    }

    #[test]
    fn history_push_and_entries_are_newest_first() {
        let mut history = ToastHistory::new();
        history.push(&entry("echomsg", "one"));
        history.push(&entry("echomsg", "two"));
        history.push(&entry("echomsg", "three"));
        let texts: Vec<String> = history.entries().map(|e| e.lines().join("")).collect();
        assert_eq!(texts, vec!["three", "two", "one"]);
    }

    #[test]
    fn history_evicts_oldest_once_capacity_is_exceeded() {
        let mut history = ToastHistory::with_capacity(2);
        history.push(&entry("echomsg", "one"));
        history.push(&entry("echomsg", "two"));
        history.push(&entry("echomsg", "three"));
        let texts: Vec<String> = history.entries().map(|e| e.lines().join("")).collect();
        assert_eq!(texts, vec!["three", "two"]);
    }

    #[test]
    fn with_capacity_zero_clamps_to_one_rather_than_discarding_every_push() {
        let mut history = ToastHistory::with_capacity(0);
        history.push(&entry("echomsg", "only"));
        assert_eq!(history.entries().count(), 1);
    }
}
