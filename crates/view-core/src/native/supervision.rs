//! The busy/interrupt/restart affordance's own state.
//!
//! Pure and headless-testable: nothing here owns an RPC handle, reads a
//! clock, or knows that a connection exists. What reaches this module is a
//! verdict somebody else folded and a duration somebody else measured, and
//! what leaves it is the notice text, the modal's contents, and the choice
//! a keystroke resolved to.
//!
//! # What the affordance claims, and what it must not
//!
//! Two failures with two recoveries, never one blurred into the other. An
//! engine that has stopped answering may still be working through something
//! that finishes; a connection that has closed never will. The notices below
//! are worded to that distinction and to nothing beyond it: an engine that
//! has gone quiet is reported as quiet, not as crashed, because the verdict
//! behind it measures unresponsiveness as observed from the loop thread and
//! a loop that stalled for its own reasons reads the same way (see
//! `view_engine::heartbeat`'s "What a verdict measures").

use std::time::Duration;

use crate::msg::ExitInfo;
use crate::native::views::PromptView;

/// How long a wedge must persist before the sticky banner escalates into the
/// interrupt/restart modal.
///
/// The banner raises the moment a wedge is seen; this is the second, longer
/// patience the modal is worth. Thirty seconds is long enough that the
/// operations which legitimately hold an editor -- a large plugin's
/// synchronous startup hook, a `:%s/.../.../ge` over a huge buffer -- run to
/// completion without a modal interrupting a user for something that was
/// always going to finish, and short enough that a genuine hang is not
/// something to sit through. It buys patience for a condition that may
/// resolve itself, which is why [`WedgeKind::Dead`] does not spend it.
pub const ENGINE_BUSY_MODAL_THRESHOLD: Duration = Duration::from_secs(30);

/// How often a visible readout changes, and so how often a loop showing one
/// has something new to paint.
///
/// The whole point of an elapsed readout is that it moves: a user looking at
/// a frozen `(41s)` learns nothing about whether the wait is still going.
/// Nothing else wakes a loop while the engine is quiet -- that is what being
/// quiet means -- so this is the cadence a wedged session has to ask for,
/// and it is asked for only while a wedge is on screen.
pub const READOUT_RESOLUTION: Duration = Duration::from_secs(1);

/// How long after an interrupt the modal starts saying the engine has not
/// reacted to it.
///
/// Long enough that a working interrupt is never reported as a failed one:
/// an engine aborted by `<C-c>` is observed to be alive again by the next
/// liveness probe that gets an answer, and the probe cadence -- not the
/// abort, which is immediate -- is what bounds how soon that can happen.
/// Kept comfortably above that cadence rather than tied to it, since this
/// crate holds no engine constant (see this module's own doc on what
/// reaches it).
pub const INTERRUPT_REACTION_WINDOW: Duration = Duration::from_secs(5);

/// The keystroke [`SupervisionChoice::Interrupt`] sends, in nvim's own
/// notation, and equally the key that picks it: the modal names the key a
/// user would already have reached for at a frozen editor, and picking the
/// choice is that key arriving at nvim.
///
/// Live-verified against the pinned engine rather than assumed (see
/// `crates/view/tests/supervision_live.rs`): fed through `nvim_input`, this
/// aborts an engine stuck inside a Vimscript loop, whose break-check pumps
/// the event loop and so sees the queued input. It does not reach an engine
/// inside a synchronous Lua loop, which pumps nothing and answers neither
/// this nor the liveness probe -- that wedge's only recovery is
/// [`SupervisionChoice::Restart`], which is why the modal lists the two
/// side by side and why an interrupt that lands on the wedge it cannot
/// reach says so ([`EngineBusyState::note_interrupt`]) instead of leaving a
/// user pressing it again. A third live-verified fact: nvim discards its
/// unread typeahead when the interrupt lands, so keys still queued through
/// `nvim_input` at that moment are lost by nvim itself -- identically with
/// or without the modal on screen.
pub const INTERRUPT_NOTATION: &str = "<C-c>";

/// The key that picks [`SupervisionChoice::Restart`].
///
/// No key means "restart the editor" to nvim, so this one is chosen by the
/// other half of the rule [`SupervisionChoice::key`] states: a key nobody
/// types by reflex at an editor that is merely slow. Function keys are
/// unbound in a stock nvim and are not on the path of any editing gesture,
/// which the printable keys, `<CR>`, `<Tab>`, `<Space>`, `<BS>` and the
/// arrows all are.
pub const RESTART_NOTATION: &str = "<F5>";

/// The key that picks [`SupervisionChoice::Quit`], offered only for a
/// connection that has closed. Nothing this key could collide with is
/// reachable there: no keystroke arrives at an engine that is gone.
pub const QUIT_NOTATION: &str = "<C-q>";

/// How long the current wedge has been continuously observed.
///
/// Stamped by the runtime and carried in, never measured here: `view-core`
/// holds no clock, and a threshold compared against a duration the caller
/// supplies keeps the escalation rule provable without one. Two readings
/// that render the same are the same as far as a repaint is concerned, which
/// is what [`readout`](Self::readout) exists to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct SinceStamp(Duration);

impl SinceStamp {
    /// A stamp for a wedge observed for `elapsed`.
    #[must_use]
    pub const fn new(elapsed: Duration) -> Self {
        Self(elapsed)
    }

    /// The full duration this stamp carries.
    #[must_use]
    pub const fn elapsed(self) -> Duration {
        self.0
    }

    /// The whole seconds a readout shows. Coarser than
    /// [`elapsed`](Self::elapsed) on purpose: the modal prints seconds, so
    /// two stamps agreeing here would repaint the same frame.
    ///
    /// A loop that wants the readout to actually count must look again at
    /// least once per [`READOUT_RESOLUTION`], since a wedged engine sends
    /// nothing that would otherwise wake it.
    #[must_use]
    pub const fn readout(self) -> u64 {
        self.0.as_secs()
    }

    /// Whether the wedge has lasted long enough to escalate the banner into
    /// the modal.
    #[must_use]
    pub fn past_modal_threshold(self) -> bool {
        self.0 >= ENGINE_BUSY_MODAL_THRESHOLD
    }
}

/// Which half of the engine connection has stopped, as folded from the two
/// watches that read it.
///
/// Three verdicts rather than one "engine broken", because the recoveries
/// differ: two of them may resolve on their own and one never will.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WedgeKind {
    /// The connection is open and draining, and the engine has answered no
    /// liveness probe for longer than the threshold.
    ReadSide,
    /// The engine has stopped reading view's output, so keystrokes are
    /// queueing behind a write that cannot complete.
    WriteSide,
    /// The connection itself is closed. Nothing can be sent to this engine
    /// and nothing more will arrive from it.
    ///
    /// Raised only for an engine that died -- never for one that stopped
    /// because it was told to. What makes it reachable at all is that the
    /// loop now routes a stopped engine through supervision instead of
    /// straight to a quit, what makes that routing honest is the restart
    /// behind [`SupervisionChoice::Restart`], and what keeps it from
    /// respawning an editor its user just closed is
    /// [`SupervisionState::note_engine_stop`].
    Dead,
}

impl WedgeKind {
    /// The sticky banner's text for this wedge.
    ///
    /// Worded to the observation the verdict behind it is made of, never to
    /// a diagnosis of who is at fault. The read-side verdict in particular
    /// covers two failures that look identical from here -- an engine that
    /// has stopped answering, and a view loop too stalled to fold the
    /// answers it was given -- and the one thing true of both is that no
    /// reply has been seen. Only [`Dead`](Self::Dead) has observed anything
    /// about the engine itself, and it says only that the connection ended.
    #[must_use]
    pub const fn notice(self) -> &'static str {
        match self {
            Self::ReadSide => "view has not seen a reply from nvim",
            Self::WriteSide => "keystrokes queued: nvim has stopped reading view's output",
            Self::Dead => "the nvim connection has closed; no further input reaches it",
        }
    }

    /// The modal's title for this wedge.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::ReadSide | Self::WriteSide => "Engine busy",
            Self::Dead => "Engine gone",
        }
    }

    /// Whether the banner escalates into the modal on the same observation
    /// that raised it, spending no patience first.
    ///
    /// True only for [`Dead`](Self::Dead). The threshold exists to give a
    /// possibly self-resolving condition room to resolve; a closed
    /// connection is never going to, so waiting would only delay the one
    /// recovery that exists.
    #[must_use]
    pub const fn escalates_immediately(self) -> bool {
        matches!(self, Self::Dead)
    }

    /// The choices the modal offers for this wedge, in the order it lists
    /// them.
    ///
    /// [`SupervisionChoice::Interrupt`] is absent for [`Dead`](Self::Dead):
    /// no input path survives a closed connection, so offering it would
    /// present a button that cannot do anything. It is the *first* choice
    /// for both open-connection wedges, because it is the recovery that
    /// keeps the session -- a restart costs whatever nvim's swap file did
    /// not hold, and an engine that is merely busy may well answer.
    ///
    /// [`SupervisionChoice::Dismiss`] is absent for [`Dead`](Self::Dead),
    /// and [`SupervisionChoice::Quit`] appears only there. Dismissing an
    /// annunciator leaves the condition it announced alone, which is a real
    /// choice for a wedge that may resolve itself and no choice at all for a
    /// connection that has closed: no keystroke reaches the engine
    /// afterwards, including the ones that would quit it, so a dismissed
    /// dead-engine modal would be an editor with no way out. The two things
    /// that genuinely remain are bringing an engine back and leaving.
    #[must_use]
    pub fn choices(self) -> Vec<SupervisionChoice> {
        match self {
            Self::ReadSide | Self::WriteSide => vec![
                SupervisionChoice::Interrupt,
                SupervisionChoice::Restart,
                SupervisionChoice::Dismiss,
            ],
            Self::Dead => vec![SupervisionChoice::Restart, SupervisionChoice::Quit],
        }
    }

    /// Whether the user can put this wedge's modal away and leave the
    /// condition running, which is what makes a modal offered once and never
    /// again the right behaviour for it.
    ///
    /// False for [`Dead`](Self::Dead), and that is the whole reason this
    /// exists. A dead-engine modal leaves the screen only by being answered:
    /// `Quit` ends the session and `Restart` asks for a replacement, so a
    /// modal that is gone while the connection is still closed means the
    /// replacement did not arrive. Suppressing the second offer there would
    /// leave a user holding a dead engine with nothing on screen to act on
    /// and no keystroke that reaches anything -- an editor with no way out.
    #[must_use]
    pub fn dismissible(self) -> bool {
        self.choices().contains(&SupervisionChoice::Dismiss)
    }
}

/// What the user chose on the interrupt/restart modal.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisionChoice {
    /// Send the engine an interrupt and leave it running.
    Interrupt,
    /// Tear the engine down and bring a fresh one up, recovering whatever
    /// nvim's own swap file held.
    Restart,
    /// Close the modal and leave the condition alone.
    Dismiss,
    /// End the session, for a connection nothing can be sent to any more.
    Quit,
}

impl SupervisionChoice {
    /// The key that picks this choice, in nvim's own notation.
    ///
    /// Constrained by the modal taking no focus: the key that picks a choice
    /// still reaches the engine as ordinary input, so a listed choice may
    /// only bind a key whose meaning to nvim is the thing the choice does,
    /// or one no user types by reflex. A bare printable key would fail both
    /// halves -- typing `i` at an editor that is merely slow means "insert",
    /// and answering it with an abort would destroy the operation the user
    /// was waiting out.
    ///
    /// The first half is what [`Interrupt`](Self::Interrupt) and
    /// [`Dismiss`](Self::Dismiss) satisfy: each sends nvim the very key that
    /// means what the choice does. The second is what
    /// [`Restart`](Self::Restart) has to satisfy instead, since no keystroke
    /// means "restart the editor" to nvim -- and being bracketed is not
    /// enough on its own, because `<CR>`, `<Tab>`, `<Space>`, `<BS>` and the
    /// arrows are all bracketed *and* reflexive (see
    /// [`RESTART_NOTATION`]).
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Interrupt => INTERRUPT_NOTATION,
            Self::Restart => RESTART_NOTATION,
            Self::Dismiss => "<Esc>",
            Self::Quit => QUIT_NOTATION,
        }
    }

    /// This choice's row in the modal, key included, so the label a user
    /// reads and the key they press come from the same place.
    #[must_use]
    pub fn label(self) -> String {
        let name = match self {
            Self::Interrupt => "Interrupt",
            Self::Restart => "Restart",
            Self::Dismiss => "Dismiss",
            Self::Quit => "Quit view",
        };
        format!("[{}] {name}", self.key())
    }
}

/// The interrupt/restart modal's state: which wedge opened it and how long
/// that wedge had lasted when the runtime last stamped it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineBusyState {
    /// How long the wedge has been observed, refreshed as the runtime keeps
    /// reading it, for the modal's own readout -- never for a timing
    /// decision made here.
    pub since: SinceStamp,
    /// Which failure this modal is offering recovery from.
    pub kind: WedgeKind,
    /// How far into the wedge an interrupt was sent, if one was.
    ///
    /// The whole state behind [`interrupt_unanswered`](Self::interrupt_unanswered):
    /// this modal is closed the moment the wedge clears, so a modal still
    /// holding this stamp some time later is itself the evidence that the
    /// interrupt changed nothing.
    interrupted: Option<SinceStamp>,
}

impl EngineBusyState {
    /// The modal for `kind`, opened after `since` of it.
    #[must_use]
    pub const fn new(kind: WedgeKind, since: SinceStamp) -> Self {
        Self {
            since,
            kind,
            interrupted: None,
        }
    }

    /// Records that the user sent an interrupt `at` this point in the wedge.
    ///
    /// Keeps the first: a user pressing `<C-c>` three times has sent three
    /// interrupts to an engine that reacted to none of them, and the honest
    /// reading of how long that has been true anchors on the first one.
    pub fn note_interrupt(&mut self, at: SinceStamp) {
        self.interrupted.get_or_insert(at);
    }

    /// Whether an interrupt has now gone long enough without the wedge
    /// clearing that saying so is a statement about the engine rather than
    /// about a reply still in flight (see [`INTERRUPT_REACTION_WINDOW`]).
    #[must_use]
    pub fn interrupt_unanswered(&self) -> bool {
        self.interrupted.is_some_and(|sent| {
            self.since.elapsed().saturating_sub(sent.elapsed()) >= INTERRUPT_REACTION_WINDOW
        })
    }

    /// The choices this modal offers, which is [`WedgeKind::choices`] for
    /// the wedge that opened it.
    #[must_use]
    pub fn choices(&self) -> Vec<SupervisionChoice> {
        self.kind.choices()
    }

    /// Whether this modal offers `choice` at all. A choice the wedge does
    /// not offer is not merely unlabelled: its key resolves to nothing, so
    /// pressing it cannot act.
    #[must_use]
    pub fn offers(&self, choice: SupervisionChoice) -> bool {
        self.choices().contains(&choice)
    }

    /// The choice `notation` picks, or `None` for a key this modal does not
    /// answer to.
    #[must_use]
    pub fn choose(&self, notation: &str) -> Option<SupervisionChoice> {
        self.choices()
            .into_iter()
            .find(|choice| choice.key() == notation)
    }

    /// What this modal puts on screen.
    ///
    /// Rendered through [`PromptView`] rather than a shape of its own: a
    /// titled box with a message and a fixed list of answers is exactly what
    /// a confirm prompt already is, and a second view type carrying the same
    /// four fields would be two layouts free to drift apart.
    #[must_use]
    pub fn view(&self) -> PromptView {
        PromptView::new(self.kind.title(), self.message()).with_choices(
            self.choices()
                .into_iter()
                .map(SupervisionChoice::label)
                .collect(),
        )
    }

    /// The modal's message line: what is wrong, for how long it has been
    /// wrong, and -- once an interrupt has gone unanswered for longer than
    /// a reply could still be in flight -- that the interrupt did not
    /// reach it either.
    ///
    /// That last clause is the one thing this modal can say about a
    /// synchronous Lua wedge, which swallows `<C-c>` silently: without it a
    /// user presses the interrupt, sees the same frozen screen and the same
    /// readout, and has no way to tell a key that did nothing from a key
    /// that never arrived.
    #[must_use]
    pub fn message(&self) -> String {
        let base = format!("{} ({}s)", self.kind.notice(), self.since.readout());
        match self.interrupted {
            Some(sent) if self.interrupt_unanswered() => format!(
                "{base}; interrupt sent {}s ago, nothing has answered since",
                self.since.readout().saturating_sub(sent.readout())
            ),
            _ => base,
        }
    }
}

/// Where a reconnect sequence has got to, as the runtime that schedules it
/// reports it: the attempt now owed, and the cap it is counted against.
///
/// Carried rather than computed here for the same reason [`SinceStamp`] is:
/// the waits between attempts are wall-clock, this module holds no clock,
/// and the transport that decides how long to wait is not this module's
/// business either. What is this module's business is what the banner says
/// while that is going on, and what happens when the attempts run out.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectProgress {
    /// The attempt now owed, counted from one. Past
    /// [`max_attempts`](Self::max_attempts) it names no attempt at all: the
    /// sequence is over (see [`exhausted`](Self::exhausted)).
    pub attempt: u32,
    /// How many attempts the sequence is allowed in total.
    pub max_attempts: u32,
}

impl ReconnectProgress {
    /// A sequence on `attempt` of `max_attempts`.
    #[must_use]
    pub const fn new(attempt: u32, max_attempts: u32) -> Self {
        Self {
            attempt,
            max_attempts,
        }
    }

    /// Whether every allowed attempt has been spent, so nothing further is
    /// owed automatically and the failure belongs to the user.
    #[must_use]
    pub const fn exhausted(self) -> bool {
        self.attempt > self.max_attempts
    }

    /// The banner's text while attempts remain, and `None` once they do not
    /// -- an exhausted sequence has nothing left to announce that
    /// [`WedgeKind::Dead`]'s own notice does not already say.
    ///
    /// Deliberately cause-neutral. A remote editor crashing and a network
    /// connection dropping reach view as the identical signal, ordinary
    /// read-side EOF, so naming either one would be a diagnosis nothing
    /// here observed; the recovery is the same sequence either way.
    #[must_use]
    pub fn notice(self) -> Option<String> {
        (!self.exhausted()).then(|| {
            format!(
                "connection lost -- reconnecting ({}/{})",
                self.attempt, self.max_attempts
            )
        })
    }
}

/// What a session says once its replacement engine has replayed `count` swap
/// files on the user's behalf.
///
/// A recovery view performed silently is a recovery the user cannot trust:
/// the buffer comes back holding text the file on disk does not have, and
/// nothing on screen says where that text came from. The engine's own
/// multi-line report would have said it, but view answers the swap prompt
/// before nvim can ask (see `view-engine`'s `SWAP_RECOVERY_CMD`) and then
/// redraws that report away, so this line is the only account of it the user
/// ever gets.
///
/// Says what came back, not what nvim did to bring it back: "swap file" is
/// nvim's own vocabulary and belongs in the engine, while the fact the user
/// needs is that the buffer holds unsaved work again.
///
/// Callers pass a count they have already established is nonzero; a zero is
/// not a recovery to announce and this returns `None` for it rather than
/// wording a notice about nothing.
#[must_use]
pub fn swap_recovery_notice(count: u64) -> Option<String> {
    (count > 0).then(|| match count {
        1 => "view: unsaved changes recovered from the swap file".to_string(),
        n => format!("view: unsaved changes recovered from {n} swap files"),
    })
}

/// How many times one session recovers a dead engine without asking before
/// it starts asking.
///
/// Automatic recovery answers the failure it was built for -- an engine that
/// dies once -- and cannot answer an engine that dies every time it starts,
/// which is what a broken plugin or a corrupt swap file produces. A session
/// that has already spent this many silent recoveries has evidence the
/// automatic half is not working, so the next death raises the modal and
/// lets the user decide instead of restarting forever behind their back.
pub const AUTOMATIC_RECOVERY_ATTEMPTS: u32 = 3;

/// Supervision's per-session settings and its memory of the current wedge
/// episode.
///
/// The episode memory is what keeps a dismissed modal dismissed: the banner
/// keeps re-asserting for as long as the condition holds, so an escalation
/// rule with no memory would re-open the modal on the very next observation
/// after a user closed it.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisionState {
    /// Whether a connection observed dead is respawned without asking. When
    /// `false`, view surfaces the condition and waits for the modal's own
    /// [`SupervisionChoice::Restart`]; the manual choice works either way.
    pub auto_restart: bool,
    /// The wedge the user has already been offered a choice for, if any.
    offered: Option<WedgeKind>,
    /// How many times this session has already recovered without asking.
    /// Never reset: the count is evidence about the session, not about the
    /// episode (see [`AUTOMATIC_RECOVERY_ATTEMPTS`]).
    recovered: u32,
    /// What the engine's own exit reported, when anything did, so a user who
    /// answers a dead engine with [`SupervisionChoice::Quit`] leaves with
    /// the status nvim actually exited with rather than an invented one.
    exit_code: Option<i32>,
    /// The reconnect sequence the runtime currently has scheduled for this
    /// dead connection, or `None` when it has none.
    reconnect: Option<ReconnectProgress>,
}

impl Default for SupervisionState {
    /// Automatic recovery on, no episode offered yet, nothing recovered and
    /// no exit observed.
    fn default() -> Self {
        Self {
            auto_restart: true,
            offered: None,
            recovered: 0,
            exit_code: None,
            reconnect: None,
        }
    }
}

impl SupervisionState {
    /// Whether the modal has already been offered for `kind` in the current
    /// episode.
    #[must_use]
    pub fn already_offered(&self, kind: WedgeKind) -> bool {
        self.offered == Some(kind)
    }

    /// Records that the modal has been opened for `kind`.
    pub fn note_offered(&mut self, kind: WedgeKind) {
        self.offered = Some(kind);
    }

    /// Forgets the episode, so a later wedge escalates on its own terms.
    /// Leaves the recovery count alone, which is what makes it a bound on
    /// the session rather than on one outage.
    pub fn forget_episode(&mut self) {
        self.offered = None;
    }

    /// Whether a dead engine is recovered without asking, right now: the
    /// user's own switch, and the bound on how often this session may act
    /// on it.
    #[must_use]
    pub fn recovers_unattended(&self) -> bool {
        self.auto_restart && self.recovered < AUTOMATIC_RECOVERY_ATTEMPTS
    }

    /// Records one recovery this session performed without asking.
    pub fn note_unattended_recovery(&mut self) {
        self.recovered = self.recovered.saturating_add(1);
    }

    /// Records what the engine's stop reported, and answers whether it is a
    /// death to recover from or the session's own ending.
    ///
    /// nvim exiting on its own terms is the ordinary way a view session
    /// ends: `:q`, `:qa` and `:cq 3` all reach this seam looking exactly
    /// like the process stopping, because that is what they are. Recovering
    /// from one of those would respawn the editor the user had just closed,
    /// so what distinguishes a death is that the engine never said it was
    /// leaving: `announced_exit` is nvim's own `VimLeavePre`, relayed over
    /// the connection before it closed
    /// ([`EngineHandle::announced_exit`](../../../view_engine/handle/struct.EngineHandle.html#method.announced_exit)).
    /// Every ordinary exit path announces; nothing killed can.
    ///
    /// That is the whole predicate, deliberately: an exit *status* cannot
    /// carry this. A signal death is only reported as one on Unix, so a
    /// predicate resting on it leaves supervision inert on Windows -- no
    /// process death there could ever read as a death, and the
    /// `[supervision]` switch would document behavior that platform cannot
    /// reach. `by_signal` stays as corroboration for the case nvim is denied
    /// the chance to announce anything at all, which is exactly what a
    /// `SIGKILL` is.
    ///
    /// It also closes the case an exit status never could: nvim ending with
    /// `exit(1)` because its own config threw is announced (`VimLeavePre`
    /// runs) and reads as the exit it is, where a status-based reading had
    /// to guess between that and `:cq 1`.
    #[must_use]
    pub fn note_engine_stop(&mut self, exit: ExitInfo, announced_exit: bool) -> bool {
        self.exit_code = exit.code;
        exit.by_signal || !announced_exit
    }

    /// The reconnect sequence currently scheduled for this connection, if
    /// one is.
    #[must_use]
    pub fn reconnect(&self) -> Option<ReconnectProgress> {
        self.reconnect
    }

    /// Records where the runtime's reconnect sequence has got to, answering
    /// whether that changed anything a frame would show.
    ///
    /// The runtime owns the schedule and this owns what is said about it, so
    /// this is written on every pass that reads one rather than only on the
    /// transition -- the same shape the banner itself is re-asserted in.
    pub fn note_reconnect(&mut self, progress: Option<ReconnectProgress>) -> bool {
        let changed = self.reconnect != progress;
        self.reconnect = progress;
        changed
    }

    /// The status view leaves with when a user answers a dead engine by
    /// quitting: nvim's own, or failure when nvim never reported one -- the
    /// same reading `Msg::EngineDown` has always applied.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        self.exit_code.unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The banner a user reads while a dropped connection is being retried,
    /// asserted as the literal string it renders as: the count is the whole
    /// point of it, and a wording that stopped naming which attempt this is
    /// would leave the same unchanging sentence on screen for half a minute.
    #[test]
    fn a_scheduled_reconnect_names_the_attempt_it_is_on() {
        assert_eq!(
            ReconnectProgress::new(1, 5).notice().as_deref(),
            Some("connection lost -- reconnecting (1/5)")
        );
        assert_eq!(
            ReconnectProgress::new(5, 5).notice().as_deref(),
            Some("connection lost -- reconnecting (5/5)")
        );
        for attempt in 1..=5 {
            assert!(
                !ReconnectProgress::new(attempt, 5).exhausted(),
                "attempt {attempt} of 5 is one the sequence still owes"
            );
        }
    }

    /// Past the last attempt the sequence has nothing of its own left to
    /// say, and what stays on screen is the dead engine's own notice: a
    /// banner still counting attempts nobody is going to make would be
    /// announcing a recovery that is not running.
    #[test]
    fn a_spent_reconnect_hands_the_banner_back_to_the_dead_connection() {
        let spent = ReconnectProgress::new(6, 5);
        assert!(spent.exhausted());
        assert_eq!(spent.notice(), None);
    }

    #[test]
    fn a_dead_connection_escalates_at_once_and_a_wedge_waits() {
        assert!(WedgeKind::Dead.escalates_immediately());
        assert!(!WedgeKind::ReadSide.escalates_immediately());
        assert!(!WedgeKind::WriteSide.escalates_immediately());
    }

    #[test]
    fn a_dead_connection_offers_no_interrupt() {
        let dead = EngineBusyState::new(WedgeKind::Dead, SinceStamp::default());
        assert!(!dead.offers(SupervisionChoice::Interrupt));
        assert_eq!(dead.choose(SupervisionChoice::Interrupt.key()), None);
    }

    /// A connection that has closed takes no more keystrokes, including the
    /// ones that would end the session, so an annunciator a user could
    /// close over it would leave an editor with no way out.
    #[test]
    fn a_dead_connection_offers_recovery_or_the_exit_and_nothing_else() {
        let dead = EngineBusyState::new(WedgeKind::Dead, SinceStamp::default());
        assert_eq!(
            dead.choices(),
            vec![SupervisionChoice::Restart, SupervisionChoice::Quit]
        );
        assert!(!dead.offers(SupervisionChoice::Dismiss));
        assert_eq!(
            dead.choose("<Esc>"),
            None,
            "dismissing a dead engine is not a choice a user can make"
        );
        assert_eq!(
            dead.choose(RESTART_NOTATION),
            Some(SupervisionChoice::Restart)
        );
        assert_eq!(dead.choose(QUIT_NOTATION), Some(SupervisionChoice::Quit));
    }

    #[test]
    fn an_open_connection_offers_the_interrupt() {
        for kind in [WedgeKind::ReadSide, WedgeKind::WriteSide] {
            let busy = EngineBusyState::new(kind, SinceStamp::default());
            assert!(busy.offers(SupervisionChoice::Interrupt), "{kind:?}");
            assert_eq!(
                busy.choose(INTERRUPT_NOTATION),
                Some(SupervisionChoice::Interrupt),
                "{kind:?}"
            );
            assert_eq!(
                busy.choose("<Esc>"),
                Some(SupervisionChoice::Dismiss),
                "{kind:?}"
            );
            assert_eq!(busy.choose("q"), None, "{kind:?}");
        }
    }

    /// Every wedge has a recovery that replaces the engine, because every
    /// wedge can be the one an interrupt cannot reach: a synchronous Lua
    /// loop answers neither the liveness probe nor `<C-c>`, and the model
    /// has no way to tell that wedge from the Vimscript one the interrupt
    /// does abort.
    #[test]
    fn every_wedge_offers_the_restart_and_names_it_on_screen() {
        for kind in [WedgeKind::ReadSide, WedgeKind::WriteSide, WedgeKind::Dead] {
            let busy = EngineBusyState::new(kind, SinceStamp::default());
            assert!(busy.offers(SupervisionChoice::Restart), "{kind:?}");
            assert_eq!(
                busy.choose(RESTART_NOTATION),
                Some(SupervisionChoice::Restart),
                "{kind:?} does not resolve the restart key"
            );
            assert!(
                busy.view()
                    .choices
                    .iter()
                    .any(|row| row.contains("Restart")),
                "{kind:?} paints no Restart row: {:?}",
                busy.view().choices
            );
        }
    }

    /// An interrupt that changed nothing is the one thing this modal can
    /// say about a wedge `<C-c>` never reaches, and it may only say it once
    /// a reply could no longer be in flight.
    #[test]
    fn an_interrupt_nothing_answered_is_reported_once_a_reply_is_no_longer_pending() {
        let mut busy = EngineBusyState::new(WedgeKind::ReadSide, SinceStamp::new(Duration::ZERO));
        busy.note_interrupt(SinceStamp::new(Duration::from_secs(31)));
        assert!(!busy.message().contains("interrupt sent"));

        busy.since = SinceStamp::new(
            Duration::from_secs(31) + INTERRUPT_REACTION_WINDOW - Duration::from_millis(1),
        );
        assert!(
            !busy.interrupt_unanswered(),
            "a reply could still be in flight: {}",
            busy.message()
        );

        busy.since = SinceStamp::new(Duration::from_secs(31) + INTERRUPT_REACTION_WINDOW);
        assert!(busy.interrupt_unanswered());
        assert!(
            busy.message()
                .ends_with("interrupt sent 5s ago, nothing has answered since"),
            "{}",
            busy.message()
        );

        // a user pressing it again has not restarted the clock on the first
        busy.note_interrupt(SinceStamp::new(busy.since.elapsed()));
        assert!(busy.interrupt_unanswered(), "{}", busy.message());
    }

    /// The switch is a switch, and the bound behind it is not: a session
    /// whose engine dies every time it starts stops recovering silently and
    /// starts asking.
    #[test]
    fn unattended_recovery_follows_the_switch_and_stops_after_the_bound() {
        let mut state = SupervisionState::default();
        assert!(state.auto_restart, "the absent config recovers on its own");
        for spent in 0..AUTOMATIC_RECOVERY_ATTEMPTS {
            assert!(state.recovers_unattended(), "recovery {spent} refused");
            state.note_unattended_recovery();
        }
        assert!(
            !state.recovers_unattended(),
            "a session that has spent {AUTOMATIC_RECOVERY_ATTEMPTS} silent \
             recoveries must start asking"
        );
        state.forget_episode();
        assert!(
            !state.recovers_unattended(),
            "the bound is on the session, not on one outage"
        );

        let off = SupervisionState {
            auto_restart: false,
            ..SupervisionState::default()
        };
        assert!(!off.recovers_unattended());
    }

    fn stop(code: Option<i32>, by_signal: bool) -> ExitInfo {
        ExitInfo { code, by_signal }
    }

    /// nvim announced it was leaving before the connection went, which is
    /// what every ordinary exit path does.
    const ANNOUNCED: bool = true;
    /// The connection went with nothing said, which is what a death is.
    const SILENT: bool = false;

    #[test]
    fn quitting_a_dead_engine_leaves_with_the_status_nvim_reported() {
        let mut state = SupervisionState::default();
        assert_eq!(state.exit_code(), 1, "an unreported exit is a failed one");
        let _ = state.note_engine_stop(stop(Some(0), false), ANNOUNCED);
        assert_eq!(state.exit_code(), 0);
        let _ = state.note_engine_stop(stop(Some(137), true), SILENT);
        assert_eq!(state.exit_code(), 137);
    }

    /// The single most destructive thing supervision could do is bring back
    /// an editor its user just closed: `:q` reaches the loop looking exactly
    /// like a crash, because in both cases the process stopped.
    #[test]
    fn an_engine_told_to_stop_is_never_one_to_bring_back() {
        let mut state = SupervisionState::default();
        assert!(
            !state.note_engine_stop(stop(Some(0), false), ANNOUNCED),
            ":q must end the session, not restart the engine"
        );
        assert!(
            !state.note_engine_stop(stop(Some(3), false), ANNOUNCED),
            ":cq 3 chose that status deliberately and must be honoured"
        );
        assert_eq!(state.exit_code(), 3);
        // the case an exit status alone cannot answer: nvim's own config
        // threw and it left with 1, which `:cq 1` also produces. The
        // announcement is what separates them, and it is present here
        assert!(
            !state.note_engine_stop(stop(Some(1), false), ANNOUNCED),
            "an engine that said it was leaving ended the session, whatever \
             status it chose"
        );
    }

    #[test]
    fn an_engine_nobody_asked_to_stop_is_one_to_bring_back() {
        let mut state = SupervisionState::default();
        assert!(
            state.note_engine_stop(stop(Some(139), true), SILENT),
            "a signal death is nobody's instruction"
        );
        assert!(
            state.note_engine_stop(stop(None, false), SILENT),
            "an engine that vanished without a word ended no session"
        );
        // the platform half of the same rule: nothing off Unix reports a
        // signal death as one, so a predicate that needed `by_signal` would
        // make supervision unreachable there. The announcement's absence
        // carries it on every platform
        assert!(
            state.note_engine_stop(stop(Some(1), false), SILENT),
            "a death that could not be reported as a signal is still a death"
        );
    }

    #[test]
    fn the_threshold_is_crossed_only_once_it_is_reached() {
        assert!(
            !SinceStamp::new(ENGINE_BUSY_MODAL_THRESHOLD - Duration::from_millis(1))
                .past_modal_threshold()
        );
        assert!(SinceStamp::new(ENGINE_BUSY_MODAL_THRESHOLD).past_modal_threshold());
    }

    /// A notice may report what the fold observed and nothing else. The
    /// read-side verdict is the one that tempts an author into a diagnosis:
    /// it is raised identically by an engine that stopped answering and by
    /// a view loop too stalled to fold the answers it received, so any
    /// sentence naming a culprit is false half the time it is shown.
    #[test]
    fn no_notice_claims_more_than_the_verdict_observed() {
        /// Phrases that assert a state nothing measured: a crash, a death,
        /// or an affirmative attribution of blame to one side.
        const OVERCLAIMS: [&str; 8] = [
            "crash",
            "died",
            "is dead",
            "hung",
            "frozen",
            "nvim has stopped answering",
            "nvim is not responding",
            "view is still running",
        ];
        for kind in [WedgeKind::ReadSide, WedgeKind::WriteSide] {
            let notice = kind.notice();
            for claim in OVERCLAIMS {
                assert!(
                    !notice.contains(claim),
                    "{kind:?} overclaims {claim:?}: {notice}"
                );
            }
        }
        assert!(
            WedgeKind::ReadSide
                .notice()
                .starts_with("view has not seen"),
            "the read-side notice must report the observation, not its cause: {}",
            WedgeKind::ReadSide.notice()
        );
    }

    #[test]
    fn the_modal_labels_carry_the_keys_that_pick_them() {
        let busy = EngineBusyState::new(WedgeKind::ReadSide, SinceStamp::new(Duration::ZERO));
        let view = busy.view();
        assert_eq!(view.title, "Engine busy");
        assert_eq!(
            view.choices,
            vec![
                "[<C-c>] Interrupt".to_string(),
                "[<F5>] Restart".to_string(),
                "[<Esc>] Dismiss".to_string()
            ]
        );

        let dead = EngineBusyState::new(WedgeKind::Dead, SinceStamp::new(Duration::ZERO)).view();
        assert_eq!(dead.title, "Engine gone");
        assert_eq!(
            dead.choices,
            vec![
                "[<F5>] Restart".to_string(),
                "[<C-q>] Quit view".to_string()
            ]
        );
    }

    #[test]
    fn no_offered_choice_binds_a_key_a_user_could_type_by_reflex() {
        /// Bracketed and reflexive both: being spelled `<...>` says a key is
        /// not a printable character, and says nothing at all about whether
        /// a user waiting out a slow editor would press it. These are the
        /// ones they would.
        const REFLEXIVE: [&str; 14] = [
            "<CR>",
            "<NL>",
            "<Tab>",
            "<S-Tab>",
            "<Space>",
            "<BS>",
            "<Del>",
            "<Esc>",
            "<Up>",
            "<Down>",
            "<Left>",
            "<Right>",
            "<PageUp>",
            "<PageDown>",
        ];
        for kind in [WedgeKind::ReadSide, WedgeKind::WriteSide, WedgeKind::Dead] {
            for choice in kind.choices() {
                let key = choice.key();
                assert!(
                    key.starts_with('<') && key.ends_with('>'),
                    "{kind:?} offers {choice:?} on the bare key {key:?}: the modal \
                     takes no focus, so that key still reaches nvim, and a user \
                     typing it at a merely-slow editor would get the choice by \
                     accident"
                );
                // the other half of the rule the key's own doc states: a
                // reflexive key is allowed only where pressing it by
                // reflex and picking the choice are the same act
                if key_means_to_nvim_what_the_choice_does(choice) {
                    continue;
                }
                assert!(
                    !REFLEXIVE.contains(&key),
                    "{kind:?} offers {choice:?} on {key:?}, which a user waiting \
                     out a slow editor presses without meaning to pick anything, \
                     and which does not itself do what {choice:?} does"
                );
            }
        }
    }

    /// Whether pressing this choice's key at nvim, with no modal on screen
    /// at all, already does what the choice does -- which is what lets
    /// [`SupervisionChoice::Interrupt`] and [`SupervisionChoice::Dismiss`]
    /// bind keys a user does press by reflex.
    fn key_means_to_nvim_what_the_choice_does(choice: SupervisionChoice) -> bool {
        match choice {
            // `<C-c>` interrupts, `<Esc>` cancels: a user who pressed either
            // by reflex asked for exactly what picking it performs
            SupervisionChoice::Interrupt | SupervisionChoice::Dismiss => true,
            SupervisionChoice::Restart | SupervisionChoice::Quit => false,
        }
    }

    #[test]
    fn the_interrupt_choice_is_picked_by_the_very_key_it_sends() {
        assert_eq!(SupervisionChoice::Interrupt.key(), INTERRUPT_NOTATION);
        let busy = EngineBusyState::new(WedgeKind::ReadSide, SinceStamp::new(Duration::ZERO));
        assert_eq!(
            busy.choose(INTERRUPT_NOTATION),
            Some(SupervisionChoice::Interrupt)
        );
    }

    #[test]
    fn the_message_reports_how_long_the_wedge_has_lasted() {
        let busy = EngineBusyState::new(
            WedgeKind::ReadSide,
            SinceStamp::new(Duration::from_secs(42)),
        );
        assert!(busy.message().ends_with("(42s)"), "{}", busy.message());
    }

    #[test]
    fn an_episode_is_offered_once_and_forgotten_on_demand() {
        let mut state = SupervisionState::default();
        assert!(!state.already_offered(WedgeKind::ReadSide));
        state.note_offered(WedgeKind::ReadSide);
        assert!(state.already_offered(WedgeKind::ReadSide));
        assert!(!state.already_offered(WedgeKind::Dead));
        state.forget_episode();
        assert!(!state.already_offered(WedgeKind::ReadSide));
    }
}
