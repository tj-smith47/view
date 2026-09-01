//! The toast stack's model: one message log, its entries, and the
//! selection that decides which of them a stack of a given height shows.
//!
//! Split out of `model.rs` under the module-size ceiling; the types are
//! re-exported from there, so nothing outside this crate names this module.

use crate::native::views::Span;

/// A locally-assigned identity for one [`MessageEntry`], stamped by
/// [`Messages::push`] from a monotonic per-session counter. Exists to name
/// "the same entry, later": `Msg::ToastExpired`'s idle-timeout callback
/// fires well after the push that scheduled it, by which time
/// `Messages::push` may have appended or replaced other entries at
/// arbitrary positions, so the id -- not an index -- is what the expiry
/// handler matches against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId(u64);

/// One shown message: an echo, an error, a search-count indicator, and
/// so on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEntry {
    pub kind: String,
    pub content: Vec<(u64, String)>,
    /// `Messages::flush_generation` at the moment this entry was pushed.
    /// Not part of nvim's wire contract -- purely local bookkeeping for
    /// `Messages::dismiss_transient_on_keypress`'s "at least one visible
    /// frame before dismissal" guarantee. Never set directly; every
    /// `MessageEntry` is built by `Messages::push`, which stamps this from
    /// its own counter.
    shown_at_flush: u64,
    /// Whether this entry is the one locally-raised condition notice (see
    /// `Messages::set_native_condition`) rather than a record of something
    /// that happened. Marked rather than matched on text or kind, so
    /// retracting the condition can never take a real message with it.
    condition: bool,
    /// This entry's identity; see [`MessageId`]. Never set directly --
    /// stamped by `Messages::push` from its own counter.
    id: MessageId,
}

impl MessageEntry {
    /// This entry's identity, stamped when it was pushed. `toast`'s
    /// idle-expiry timer names the entry it was scheduled for by this id,
    /// since positions in `Messages::entries` shift as later messages
    /// arrive.
    #[must_use]
    pub fn id(&self) -> MessageId {
        self.id
    }

    /// This entry's content chunks joined into one string, then split into
    /// one entry per physical line. A `msg_show` content chunk can carry an
    /// embedded `\n` for a genuinely multi-line message (a long `emsg`'s
    /// wrapped continuation, live-observed from a real autocommand error,
    /// and documented in nvim's own `api-ui-events.txt`: "Messages can
    /// contain line breaks") rather than always being exactly one visual
    /// line; a caller that joins the chunks and paints the result as a
    /// single row squashes every line break into one toast row wide enough
    /// to hold all of them concatenated. `view_surface::render` (layer
    /// width/height) and `view_tui::paint::paint_messages` (per-row text)
    /// both call this instead of joining `content` themselves, so sizing
    /// and painting can never disagree about how many rows -- or how wide
    /// -- this entry needs. Always yields at least one (possibly empty)
    /// line, so an entry with no content still reserves its own row.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        let joined: String = self.content.iter().map(|(_, t)| t.as_str()).collect();
        joined.split('\n').map(str::to_string).collect()
    }

    /// Whether nvim's own `msg_show` `kind` (per `api-ui-events.txt`'s kind
    /// table) names this an error or a warning: `"emsg"`, `"echoerr"`,
    /// `"wmsg"`, `"lua_error"`, `"rpc_error"`, `"shell_err"`. These must be
    /// read, not silently lost, so they persist until explicitly cleared or
    /// replaced -- never auto-dismissed by incidental user activity and
    /// never evicted from the visible toast stack merely because other
    /// messages arrived after them (`Messages::visible_lines`) -- matching
    /// real nvim's own hit-enter-prompt convention that an error blocks
    /// until acknowledged. The acknowledgement itself is
    /// [`Messages::dismiss_sticky`], which is a deliberate gesture rather
    /// than the ambient activity a transient entry dies of. Every other kind
    /// is transient.
    ///
    /// `"shell_err"` is a `:!cmd`'s stderr: the one channel a failing
    /// external command has to explain itself, and the only reason to look
    /// at the output of a command that went wrong.
    ///
    /// A locally-raised condition notice (`Messages::set_native_condition`)
    /// is persistent by the same argument arrived at from the other side:
    /// it describes a state that is still true, and the user activity that
    /// dismisses a transient entry is exactly the activity a stalled engine
    /// is swallowing, so dismissing on a keypress would erase the notice
    /// with the very keystroke it is there to explain. It is retracted by
    /// whoever raised it, when the condition ends.
    /// Whether this entry is the session's one raised condition notice
    /// (see [`Messages::set_native_condition`]), rather than a record of
    /// something that happened.
    #[must_use]
    pub(crate) fn is_condition(&self) -> bool {
        self.condition
    }

    #[must_use]
    pub fn is_persistent(&self) -> bool {
        self.condition || Self::is_persistent_kind(&self.kind)
    }

    /// The kind-only half of [`is_persistent`]: whether `kind` alone (no
    /// locally-raised condition flag, since none exists yet) names a kind
    /// that stands until it is replaced or dismissed. `toast::route` matches
    /// on this directly -- it classifies the `kind` string before any
    /// `MessageEntry` exists to ask.
    ///
    /// nvim's error/warning kinds, plus the one view raises itself:
    /// `"native_sticky"` is not a wire kind and can arrive only through
    /// [`EngineModel::record_native_notice_sticky_once`], whose doc carries
    /// the argument for why that notice cannot be transient.
    #[must_use]
    pub fn is_persistent_kind(kind: &str) -> bool {
        matches!(
            kind,
            "emsg" | "echoerr" | "wmsg" | "lua_error" | "rpc_error" | "shell_err" | "native_sticky"
        )
    }

    /// Whether this entry is the question text of a cmdline prompt that is
    /// still waiting for an answer -- nvim's `"confirm"` kind, which its own
    /// kind table defines as "message preceding a prompt".
    ///
    /// A third lifetime, neither persistent nor transient. nvim emits the
    /// question once as `msg_show` and the answer line separately as
    /// `cmdline_show`; a key that answers none of the offered choices
    /// re-arms the prompt by re-emitting `cmdline_show` ALONE, so a question
    /// dismissed on that keypress leaves an answer line with nothing to
    /// answer. Persistence is equally wrong in the other direction: nvim
    /// sends no `msg_clear` when the prompt resolves, so a question kept
    /// until explicitly cleared would occlude the buffer forever. Its
    /// lifetime is therefore the prompt's: dismissable by user activity,
    /// but only once the cmdline has closed.
    #[must_use]
    pub fn is_prompt(&self) -> bool {
        self.kind == "confirm"
    }

    /// Whether view raised this entry itself rather than decoding it from
    /// nvim's `msg_show`.
    ///
    /// The `"native"` kinds are the marker -- `"native_sticky"` differing
    /// only in lifetime ([`EngineModel::record_native_notice_sticky_once`]),
    /// so every family withdrawal and every de-duplication ranges over both.
    /// Both are reachable from outside `model.rs` only through
    /// [`EngineModel::record_native_notice`] and its siblings and
    /// [`Messages::set_native_condition`], so no wire message can wear one.
    #[must_use]
    pub fn is_native(&self) -> bool {
        Self::is_native_kind(&self.kind)
    }

    /// The kind-only half of [`is_native`](Self::is_native), for the same
    /// reason [`is_persistent_kind`](Self::is_persistent_kind) exists:
    /// `toast::route_under_hold` classifies a `kind` string before any
    /// `MessageEntry` has been built to ask.
    #[must_use]
    pub fn is_native_kind(kind: &str) -> bool {
        matches!(kind, "native" | "native_sticky")
    }

    /// Whether this entry keeps its rows when the toast box overflows.
    /// An unanswered question ranks with the errors: a burst of info
    /// messages must not push it off screen while the editor is blocked
    /// waiting for it.
    #[must_use]
    fn outranks_transient(&self) -> bool {
        self.is_persistent() || self.is_prompt()
    }
}

/// The message log built from `msg_show`/`msg_clear`. A log rather than a
/// single `Option`, since nvim can show several messages in sequence
/// (`:messages` history) before any are cleared.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Messages {
    pub entries: Vec<MessageEntry>,
    /// Foreign transient messages parked by the startup hold: recorded to
    /// scrollback like any other, but taking no toast slot and painting
    /// nothing until `resolve_startup_hold` decides their fate. See
    /// [`crate::native::toast::StartupHold`].
    held: Vec<MessageEntry>,
    /// Whether a message arriving now is parked. `Pending` from
    /// `Default::default`, which is what makes this mechanism win a race it
    /// would otherwise start after: the first redraw batch after attach can
    /// already carry a plugin's setup-time complaint.
    startup_hold: crate::native::toast::StartupHold,
    /// Bumped by `note_flush` on every `Flush` UI event; stamped onto each
    /// new entry as `MessageEntry::shown_at_flush`. See
    /// `dismiss_transient_on_keypress`.
    flush_generation: u64,
    /// The next [`MessageId`] `push` stamps; bumped on every call, replace
    /// included, so every pushed entry -- even one that overwrites another
    /// in place -- gets an identity distinct from what stood there before.
    next_message_id: u64,
    /// The entry [`Self::arm_top_slot`] last handed a dismissal timer, so it
    /// can tell a top slot that has changed hands from one that has merely
    /// been asked about again. `None` both before the first toast and
    /// whenever the queue is empty.
    armed_slot: Option<MessageId>,
    /// The armed entry's lines, taken while it still stood.
    ///
    /// The model reaches its final state on the frame a notice leaves, so by
    /// the time anything can ask what to slide out to the right edge the
    /// entry is already gone from `entries` and this is the only copy left.
    /// Paid once per toast, when the slot changes hands, never per update.
    armed_lines: Vec<Vec<Span>>,
    /// How many entries stood ahead of the armed one, so the boxes that
    /// slide up are exactly the ones that were below it: a sticky entry
    /// takes no slot but does take a box, so the vacated box is not always
    /// the topmost one.
    armed_index: usize,
}

/// Which of `items` fit in `budget`, given each one's cost and whether it
/// outranks transient text.
///
/// Eviction order is the toast stack's whole priority rule in one place:
/// transient items go first, oldest first, and only once every one of them
/// is gone does eviction reach into the persistent ones (again oldest
/// first). Shared by the two budgets the stack is selected against -- a box
/// costing its lines plus its frame, and a bare line costing one row --
/// because a stack sized by one rule and painted by another is how a kept
/// error line ends up behind a frame that has no room for it.
fn keep_within(items: &[(bool, usize)], budget: usize) -> Vec<bool> {
    let mut keep = vec![true; items.len()];
    let mut total: usize = items.iter().map(|(_, cost)| *cost).sum();
    for target in [false, true] {
        for (i, (persistent, cost)) in items.iter().enumerate() {
            if total <= budget {
                return keep;
            }
            if *persistent == target {
                keep[i] = false;
                total = total.saturating_sub(*cost);
            }
        }
    }
    keep
}

impl Messages {
    /// Stamps and appends one entry: `kind`/`content` as decoded off the
    /// wire or synthesized locally, no classification, no history record,
    /// no expiry. `replace_last` overwrites the most recent entry instead
    /// of appending, matching nvim's progress-indicator convention (e.g.
    /// successive search-match counts share one line); with no prior entry
    /// to replace, it appends instead.
    ///
    /// "Most recent" means the most recent entry nvim itself produced.
    /// Anything view synthesized locally is skipped over, whether it is a
    /// raised condition notice (see `set_native_condition`) or a one-shot
    /// one, because nvim's replace targets nvim's own previous line:
    /// overwriting a view notice would both drop something nvim never put
    /// there and leave the line nvim meant to replace standing as a
    /// duplicate. A one-shot notice reaches the tail as readily as a
    /// condition does -- `clear` keeps it across an `msg_clear` that empties
    /// everything around it.
    ///
    /// Crate-private on purpose: this is the raw primitive
    /// [`EngineModel::record_message`] is built from, not an entry point of
    /// its own. `EngineModel::record_message`/`record_native_notice` and
    /// `Messages::push_native`/`set_native_condition` are the only callers,
    /// all inside this crate -- nothing outside it can reach a `MessageEntry`
    /// without also going through classification.
    pub(crate) fn push(
        &mut self,
        kind: String,
        content: Vec<(u64, String)>,
        replace_last: bool,
    ) -> MessageId {
        let id = MessageId(self.next_message_id);
        // saturating rather than wrapping: an id that stops advancing at
        // `u64::MAX` collides only with entries nothing is still waiting on,
        // while a wrap would hand a live entry the id of one a timer thread
        // is asleep holding
        self.next_message_id = self.next_message_id.saturating_add(1);
        let entry = MessageEntry {
            kind,
            content,
            shown_at_flush: self.flush_generation,
            condition: false,
            id,
        };
        if replace_last {
            if let Some(last) = self
                .entries
                .iter_mut()
                .rev()
                .find(|e| !e.condition && !e.is_native())
            {
                *last = entry;
                return id;
            }
        }
        self.entries.push(entry);
        id
    }

    /// The entry occupying the top slot of the toast stack -- the oldest
    /// entry still standing that takes a slot at all -- or `None` when
    /// nothing does.
    ///
    /// The dismissal timer belongs to this slot rather than to any particular
    /// notice, per the spec's motion rules and its `ext_messages` routing
    /// table: a notice that arrived behind others has not been read yet, and
    /// a timer started on arrival retires it before it was ever at the front.
    /// New toasts enter at the bottom, so the queue is arrival order and the
    /// head of it is what expires next.
    ///
    /// Three classes of entry are deliberately outside the queue. A
    /// persistent one (nvim's error/warning kinds, view's own
    /// `"native_sticky"`, a raised condition) is dismissed deliberately and
    /// never by a timer, so a sticky notice standing ahead of the stack
    /// would otherwise freeze every transient behind it for the rest of the
    /// session. An unanswered prompt's question is ended by its own answer.
    /// And a message the startup hold parked
    /// ([`crate::native::toast::Route::HistoryOnly`]) is not in `entries` at
    /// all, so it takes no slot by construction and takes one the moment the
    /// hold releases it onto the stack.
    #[must_use]
    pub fn top_slot(&self) -> Option<MessageId> {
        self.entries
            .iter()
            .find(|e| !e.outranks_transient())
            .map(MessageEntry::id)
    }

    /// Arms a dismissal timer for the top slot if it has changed hands since
    /// the last call, and reports the effect that does so.
    ///
    /// Idempotent, and meant to be called on every path that can add to or
    /// remove from the stack: an unchanged top slot answers `None`, so a
    /// second call after a message that touched no toast costs a comparison
    /// and arms nothing. At most one slot is *armed* at a time, which is what
    /// makes the promoted entry's timeout a full one rather than the
    /// remainder of a timer armed while it was still queued.
    ///
    /// Not the same as one timer being in flight. `Effect::ScheduleToastExpiry`
    /// has no cancellation, so when the armed entry leaves by any route other
    /// than its own expiry -- a keypress, an `msg_clear`, a sticky dismissal,
    /// a replace -- the successor is armed while the superseded thread is
    /// still sleeping. That is the condition the top-slot test in
    /// `Msg::ToastExpired` exists to survive: it is what makes the surviving
    /// timer's arrival a no-op instead of an unread notice retired early.
    pub(crate) fn arm_top_slot(&mut self) -> Option<crate::msg::Effect> {
        let top = self.top_slot();
        if top == self.armed_slot {
            return None;
        }
        self.armed_slot = top;
        let armed = top.and_then(|id| self.entries.iter().position(|e| e.id() == id));
        self.armed_index = armed.unwrap_or(0);
        self.armed_lines = armed
            .and_then(|i| self.entries.get(i))
            .map(|e| {
                e.lines()
                    .into_iter()
                    .map(|l| vec![Span::plain(l)])
                    .collect()
            })
            .unwrap_or_default();
        let id = top?;
        // the queue holds only entries `route` calls `Route::Transient`,
        // which is the one route that owns an idle timeout at all
        let after = crate::native::toast::timeout_for(crate::native::toast::Route::Transient)?;
        Some(crate::msg::Effect::ScheduleToastExpiry { id, after })
    }

    /// What the armed toast slot just lost, if it lost anything: the
    /// departing notice's lines and the box it vacated, or `None` when the
    /// stack ended this message the same size it started it.
    ///
    /// `entries_before` is the entry count taken ahead of the message, and
    /// it is what separates a departure from a replacement. nvim's
    /// progress-indicator convention overwrites the newest entry in place
    /// (`push`'s `replace_last`), which stamps a fresh id and so changes the
    /// top slot whenever that entry was the only one standing -- a search
    /// count updating itself would otherwise read as a dismissal and animate
    /// on every keystroke of the search. A departure shrinks the stack; a
    /// replacement does not.
    ///
    /// Read before [`Self::arm_top_slot`] re-arms, which is what leaves the
    /// leaving entry's copy still in place to be read.
    #[must_use]
    pub(crate) fn departed_toast(&self, entries_before: usize) -> Option<(Vec<Vec<Span>>, usize)> {
        let armed = self.armed_slot?;
        if self.entries.len() >= entries_before || self.entries.iter().any(|e| e.id() == armed) {
            return None;
        }
        Some((self.armed_lines.clone(), self.armed_index))
    }

    /// Whether the startup hold is still parking foreign transient
    /// messages. Read by [`EngineModel::record_message`] on every message,
    /// which is why it is a `Copy` enum read and nothing more.
    #[must_use]
    pub fn startup_hold(&self) -> crate::native::toast::StartupHold {
        self.startup_hold
    }

    /// The messages the startup hold is holding, oldest first. Never
    /// authoritative for anything painted -- `entries` is -- and here for
    /// the assertions that have to see the parking rather than infer it
    /// from an empty stack.
    #[must_use]
    pub fn held(&self) -> &[MessageEntry] {
        &self.held
    }

    /// Moves the entry `id` names out of the visible stack and into the
    /// held set: the second half of a `Route::HistoryOnly` record, run
    /// after [`EngineModel::record_message`] has already written it to
    /// scrollback, so no path through the hold can lose a message.
    ///
    /// Once the hold has collapsed nothing will ever release what it parks,
    /// so a message arriving then is dropped from the stack without being
    /// parked at all -- the scrollback record is the whole of what it gets,
    /// which is what the standing notice tells the user.
    ///
    /// `replace_last` is applied a second time, here, on the same terms
    /// [`Self::push`] applies it to the visible stack. It has to be: parking
    /// takes the entry off that stack, so the next line of a coalescing
    /// sequence finds nothing there to overwrite and appends instead, and a
    /// startup progress line that nvim coalesced into one toast would drain
    /// as one toast per step. The hold decides *when* a message is shown,
    /// never how many of it there are.
    pub(crate) fn hold(&mut self, id: MessageId, replace_last: bool) {
        let Some(index) = self.entries.iter().position(|e| e.id == id) else {
            return;
        };
        let entry = self.entries.remove(index);
        if self.startup_hold != crate::native::toast::StartupHold::Pending {
            return;
        }
        if replace_last {
            if let Some(last) = self
                .held
                .iter_mut()
                .rev()
                .find(|e| !e.condition && !e.is_native())
            {
                *last = entry;
                return;
            }
        }
        self.held.push(entry);
    }

    /// Resolves the startup hold, once, and reports whether anything the
    /// user can see changed.
    ///
    /// [`HoldOutcome::Release`](crate::native::toast::HoldOutcome::Release)
    /// drains the held set onto the stack in arrival order and **re-stamps
    /// every drained entry to the current flush generation**. Without the
    /// re-stamp a drained entry carries the generation it was pushed at,
    /// many flushes ago, and `dismiss_transient_on_keypress` -- which keeps
    /// a transient only while `shown_at_flush == current` -- drops it on the
    /// very keypress that released it, painting zero frames. The stamp is
    /// the same convention `UiEvent::Flush` maintains for a freshly pushed
    /// toast.
    ///
    /// [`HoldOutcome::Collapse`](crate::native::toast::HoldOutcome::Collapse)
    /// leaves the held set where it is -- in the history ring alone -- and
    /// keeps parking, so a late complaint from the same startup joins it
    /// rather than landing on top of the notice that explains it. The
    /// `Release` that later ends a collapsed hold discards the held set
    /// instead of draining it: the decision that it stays in the history was
    /// already taken, and re-raising it on the first keypress would restore
    /// the wall the notice replaced.
    ///
    /// A hold already `Off` answers `false` and changes nothing: the three
    /// triggers race each other by design and only the first is the
    /// decision.
    #[must_use]
    pub fn resolve_startup_hold(&mut self, outcome: crate::native::toast::HoldOutcome) -> bool {
        use crate::native::toast::{HoldOutcome, StartupHold};
        if self.startup_hold == StartupHold::Off {
            return false;
        }
        if outcome == HoldOutcome::Collapse {
            if self.startup_hold == StartupHold::Collapsed {
                return false;
            }
            self.startup_hold = StartupHold::Collapsed;
            return false;
        }
        let collapsed = self.startup_hold == StartupHold::Collapsed;
        self.startup_hold = StartupHold::Off;
        if collapsed {
            // a claimant was named and the notice on screen already says
            // where these went; releasing them onto the stack now would put
            // the wall of startup errors back up, one keypress late
            self.held.clear();
            return false;
        }
        if self.held.is_empty() {
            return false;
        }
        let current = self.flush_generation;
        for mut entry in self.held.drain(..) {
            entry.shown_at_flush = current;
            self.entries.push(entry);
        }
        true
    }

    /// Drops every message nvim showed, per `msg_clear`, and keeps every
    /// locally-synthesized one.
    ///
    /// `msg_clear` states that the messages *nvim* put up are over, which is
    /// a fact about nvim's own message state and says nothing about a line
    /// view raised itself. A native notice's lifetime belongs to the
    /// mechanism that raised it -- `Effect::ScheduleToastExpiry` for a
    /// one-shot notice, the condition itself for a raised condition -- and
    /// letting an unrelated engine redraw retract one is how a notice
    /// vanishes before it was ever read.
    ///
    /// The distinction is load-bearing for the notice a swap recovery shows:
    /// the redraw that takes nvim's recovery report off the buffer is
    /// answered with exactly this event, so a wholesale clear would erase the
    /// account of the recovery together with the report it was written to
    /// replace.
    pub fn clear(&mut self) {
        self.entries.retain(MessageEntry::is_native);
    }

    /// Appends a locally-originated notice -- never from nvim's own
    /// `msg_show` wire event -- through the same overlay `msg_show`
    /// populates, so a native warning (e.g. startup's pre-attach key ring
    /// dropping a keystroke) reaches the user through the one message
    /// surface that already exists rather than a parallel toast mechanism.
    /// `replace_last` behaves exactly as it does for `push`: pass `true` to
    /// update an in-place running count instead of stacking a new entry per
    /// occurrence.
    ///
    /// Crate-private on purpose: this only stamps and stores the entry, with
    /// none of `route`/`toast_history`/expiry-scheduling that every other
    /// native notice needs -- correct here solely because
    /// [`Self::set_native_condition`], the one remaining caller, decides
    /// persistence itself via the `condition` flag it sets right after this
    /// call, rather than through `route()`'s kind-based classification. A
    /// one-shot notice wants that classification, so it goes through
    /// [`crate::model::EngineModel::record_native_notice`] instead, which
    /// this method is not reachable around: there is no `pub` path to a
    /// `kind == "native"` entry from outside this module other than through
    /// it.
    pub(crate) fn push_native(&mut self, text: String, replace_last: bool) {
        self.push("native".to_string(), vec![(0, text)], replace_last);
    }

    /// Raises (`Some`) or retracts (`None`) the one locally-raised
    /// *condition* notice, through the same overlay as `push_native` and
    /// for the same reason: a native condition reaches the user over the
    /// message surface that already exists, never a second one built
    /// alongside it.
    ///
    /// A condition differs from `push_native`'s notice in lifetime, not in
    /// origin. `push_native` records that something happened, and the
    /// record stays true forever; a condition asserts that something *is
    /// true now* -- an engine that has stopped reading view's output, say --
    /// and must disappear by itself the moment it stops being true. At most
    /// one is ever shown, since a second simultaneous condition would need
    /// its own retraction and there is nothing to key one off. It is
    /// persistent while raised (see `MessageEntry::is_persistent`), so the
    /// keypresses that dismiss ordinary transient text leave it alone.
    ///
    /// Idempotent, and cheap enough to call unconditionally on every loop
    /// pass: re-asserting the text already showing changes nothing and
    /// reports so. Returns whether the visible set changed, which is the
    /// caller's cue to repaint.
    #[must_use]
    pub fn set_native_condition(&mut self, text: Option<&str>) -> bool {
        let Some(text) = text else {
            let before = self.entries.len();
            self.entries.retain(|e| !e.condition);
            return self.entries.len() != before;
        };
        if let Some(raised) = self.entries.iter_mut().find(|e| e.condition) {
            let content = vec![(0, text.to_string())];
            if raised.content == content {
                return false;
            }
            raised.content = content;
            return true;
        }
        // raised through `push_native` and marked afterwards, rather than
        // built here: the flush stamp every entry carries keeps exactly one
        // source, and a condition is a native notice in every respect but
        // its lifetime
        self.push_native(text.to_string(), false);
        if let Some(raised) = self.entries.last_mut() {
            raised.condition = true;
        }
        true
    }

    /// Marks one full paint cycle as having happened -- one call per
    /// `Flush` UI event -- so that a transient entry's age in frames, and
    /// therefore whether it has survived long enough to be dismissable, is
    /// answerable at all.
    pub fn note_flush(&mut self) {
        self.flush_generation = self.flush_generation.wrapping_add(1);
    }

    /// Drops every transient entry that has already survived at least one
    /// full paint cycle since it was shown, leaving `is_persistent` entries
    /// and -- while `cmdline_open` -- `is_prompt` ones in place. Called
    /// from `update` on the user's next keypress: gives an info-level toast
    /// a readable duration bounded by real user activity -- an event the
    /// clockless model already receives -- rather than a wall-clock
    /// timer the runtime never delivers to `update`. An entry pushed in the same
    /// flush generation as the pending keypress has not necessarily been
    /// painted even once yet, so it survives this pass and is only
    /// dismissed on the *next* keypress instead, guaranteeing every
    /// transient toast is visible for at least one frame. Returns whether
    /// anything was actually dropped, so the caller knows whether to mark
    /// the model dirty for a repaint.
    #[must_use]
    pub fn dismiss_transient_on_keypress(&mut self, cmdline_open: bool) -> bool {
        let before = self.entries.len();
        let current = self.flush_generation;
        self.entries.retain(|e| {
            e.is_persistent() || (cmdline_open && e.is_prompt()) || e.shown_at_flush == current
        });
        self.entries.len() != before
    }

    /// Drops the question an answered prompt was asking, so its box leaves
    /// with the prompt instead of lingering over the buffer as ordinary
    /// text. The counterpart to the `cmdline_open` guard in
    /// [`Self::dismiss_transient_on_keypress`], which holds that question
    /// up for exactly as long as the cmdline asking it is open: this is
    /// what ends it at the same moment, rather than at whatever unrelated
    /// keystroke happens next.
    #[must_use]
    pub fn dismiss_answered_prompt(&mut self) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !e.is_prompt());
        self.entries.len() != before
    }

    /// Drops every standing error/warning entry -- the deliberate way out of
    /// a sticky toast. Returns whether anything was actually dropped, so the
    /// caller knows whether to mark the model dirty for a repaint.
    ///
    /// The counterpart to [`Self::dismiss_transient_on_keypress`], and
    /// deliberately not folded into it: that one fires on *any* keypress,
    /// and an error dismissed by the next motion is an error the user never
    /// read. Stickiness is what makes an error legible; a way out is what
    /// keeps it from occluding the buffer forever once it has been.
    ///
    /// A raised condition (see [`Self::set_native_condition`]) survives: it
    /// asserts that something *is currently true*, so clearing it would
    /// state a falsehood until whoever raised it noticed and re-raised it.
    /// It is retracted by that raiser, when the condition ends.
    #[must_use]
    pub fn dismiss_sticky(&mut self) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !e.is_persistent() || e.condition);
        self.entries.len() != before
    }

    /// Every visible toast's lines in one flat list, box structure dropped:
    /// what the stack currently says, for a caller that asks that and not
    /// where any of it is drawn.
    ///
    /// `max_rows` is [`Self::visible_toasts`]' budget, frames included, so
    /// the two can never disagree about which notices are showing. Nothing
    /// paints from this -- `view-surface` builds its layers from
    /// `visible_toasts` directly -- which is why the flattening is here
    /// rather than a second selection with its own eviction rule.
    ///
    /// Each returned line is one span, carrying [`StyleRole::Plain`]
    /// (`crate::native::views::StyleRole`): a toast has no per-segment
    /// structure to preserve, so a single honest span is the whole row --
    /// not a placeholder for styling nobody asked for yet.
    #[must_use]
    pub fn visible_lines(&self, max_rows: usize) -> Vec<Vec<Span>> {
        self.visible_toasts(max_rows)
            .into_iter()
            .flatten()
            .collect()
    }

    /// The toast boxes actually visible in a stack `max_rows` tall, oldest
    /// first: one box per entry, holding that entry's own physical lines
    /// (its content split on its own embedded newlines) and costing those
    /// lines plus the two rows of the frame drawn around them.
    ///
    /// One box per entry rather than one box for the log is what lets the
    /// top slot's notice leave to the right while the ones under it slide
    /// up: two boxes moving in different directions cannot be one rect.
    ///
    /// Eviction is [`keep_within`]'s: an error, a warning or an unanswered
    /// question is never pushed off merely because other messages arrived
    /// after it, and only when those alone overflow the stack does eviction
    /// reach them, oldest first. A single box too tall for the whole stack
    /// is still shown rather than evicted into an empty screen -- the caller
    /// clips it, which is a truncated notice instead of no notice at all.
    #[must_use]
    pub fn visible_toasts(&self, max_rows: usize) -> Vec<Vec<Vec<Span>>> {
        let boxes: Vec<(bool, Vec<Vec<Span>>)> = self
            .entries
            .iter()
            .map(|e| {
                let lines = e
                    .lines()
                    .into_iter()
                    .map(|l| vec![Span::plain(l)])
                    .collect();
                (e.outranks_transient(), lines)
            })
            .collect();
        let costs: Vec<(bool, usize)> = boxes
            .iter()
            .map(|(persistent, lines)| (*persistent, lines.len().saturating_add(2)))
            .collect();
        let mut keep = keep_within(&costs, max_rows);
        // a stack with no room for even one framed box shows the newest
        // notice clipped rather than nothing at all: a truncated line still
        // says something happened, an empty screen says the message was
        // never raised
        if !keep.iter().any(|k| *k) {
            if let Some(last) = keep.last_mut() {
                *last = true;
            }
        }
        boxes
            .into_iter()
            .zip(keep)
            .filter_map(|((_, lines), k)| k.then_some(lines))
            .collect()
    }
}
