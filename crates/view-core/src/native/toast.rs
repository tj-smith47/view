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
    /// One of nvim's error/warning kinds (`MessageEntry::is_persistent_kind`)
    /// or a locally-raised condition. Stays until explicitly cleared or
    /// replaced -- never expires on its own.
    Sticky,
    /// A statusline-owned kind (mode/pending-count/ruler/search-count
    /// indicators), meant for the statusline surface rather than the toast
    /// stack once a consumer renders it there.
    Statusline,
    /// Everything else: an ordinary toast that expires on its own after
    /// [`TRANSIENT_TOAST_TIMEOUT`] with no other input.
    Transient,
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
/// a sticky entry by an explicit clear/replace, and a statusline entry by
/// nvim's own next update to the same slot.
#[must_use]
pub fn timeout_for(route: Route) -> Option<Duration> {
    match route {
        Route::Transient => Some(TRANSIENT_TOAST_TIMEOUT),
        Route::Prompt | Route::Sticky | Route::Statusline => None,
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
    fn route_matches_the_brief_table_exactly() {
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
