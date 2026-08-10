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

/// The keystroke [`SupervisionChoice::Interrupt`] sends, in nvim's own
/// notation.
///
/// Live-verified against the pinned engine rather than assumed (see
/// `crates/view/tests/supervision_live.rs`): fed through `nvim_input`, this
/// aborts an engine stuck inside a Vimscript loop, whose break-check pumps
/// the event loop and so sees the queued input. It does not reach an engine
/// inside a synchronous Lua loop, which pumps nothing and answers neither
/// this nor the liveness probe -- that wedge's only recovery is
/// [`SupervisionChoice::Restart`], which is why the modal offers both rather
/// than presenting an interrupt as the answer.
pub const INTERRUPT_NOTATION: &str = "<C-c>";

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
    Dead,
}

impl WedgeKind {
    /// The sticky banner's text for this wedge.
    ///
    /// Worded to what the verdict behind it actually measures and no
    /// further: an engine that has gone quiet is described as quiet, never
    /// as crashed, since only [`Dead`](Self::Dead) has observed the
    /// connection end.
    #[must_use]
    pub const fn notice(self) -> &'static str {
        match self {
            Self::ReadSide => "nvim has stopped answering; view is still running",
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
    /// present a button that cannot do anything.
    #[must_use]
    pub fn choices(self) -> Vec<SupervisionChoice> {
        match self {
            Self::ReadSide | Self::WriteSide => vec![
                SupervisionChoice::Interrupt,
                SupervisionChoice::Restart,
                SupervisionChoice::Dismiss,
            ],
            Self::Dead => vec![SupervisionChoice::Restart, SupervisionChoice::Dismiss],
        }
    }
}

/// What the user chose on the interrupt/restart modal.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisionChoice {
    /// Send the engine an interrupt and leave it running.
    Interrupt,
    /// Tear the engine down and bring a fresh one up.
    Restart,
    /// Close the modal and leave the condition alone.
    Dismiss,
}

impl SupervisionChoice {
    /// The key that picks this choice, in nvim's own notation.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Interrupt => "i",
            Self::Restart => "r",
            Self::Dismiss => "<Esc>",
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
}

impl EngineBusyState {
    /// The modal for `kind`, opened after `since` of it.
    #[must_use]
    pub const fn new(kind: WedgeKind, since: SinceStamp) -> Self {
        Self { since, kind }
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

    /// The modal's message line: what is wrong, and for how long it has been
    /// wrong.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{} ({}s)", self.kind.notice(), self.since.readout())
    }
}

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
    /// `Restart`; the manual choice works either way.
    pub auto_restart: bool,
    /// The wedge the user has already been offered a choice for, if any.
    offered: Option<WedgeKind>,
}

impl Default for SupervisionState {
    /// Automatic recovery on, no episode offered yet.
    fn default() -> Self {
        Self {
            auto_restart: true,
            offered: None,
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
    pub fn forget_episode(&mut self) {
        self.offered = None;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

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
        assert!(dead.offers(SupervisionChoice::Restart));
        assert!(dead.offers(SupervisionChoice::Dismiss));
        assert_eq!(dead.choose(SupervisionChoice::Interrupt.key()), None);
    }

    #[test]
    fn an_open_connection_offers_every_recovery() {
        for kind in [WedgeKind::ReadSide, WedgeKind::WriteSide] {
            let busy = EngineBusyState::new(kind, SinceStamp::default());
            assert!(busy.offers(SupervisionChoice::Interrupt), "{kind:?}");
            assert_eq!(
                busy.choose("i"),
                Some(SupervisionChoice::Interrupt),
                "{kind:?}"
            );
            assert_eq!(
                busy.choose("r"),
                Some(SupervisionChoice::Restart),
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

    #[test]
    fn the_threshold_is_crossed_only_once_it_is_reached() {
        assert!(
            !SinceStamp::new(ENGINE_BUSY_MODAL_THRESHOLD - Duration::from_millis(1))
                .past_modal_threshold()
        );
        assert!(SinceStamp::new(ENGINE_BUSY_MODAL_THRESHOLD).past_modal_threshold());
    }

    #[test]
    fn no_notice_claims_a_crash_the_verdict_did_not_observe() {
        for kind in [WedgeKind::ReadSide, WedgeKind::WriteSide] {
            let notice = kind.notice();
            assert!(
                !notice.contains("crash") && !notice.contains("died"),
                "{kind:?} overclaims: {notice}"
            );
        }
    }

    #[test]
    fn the_modal_labels_carry_the_keys_that_pick_them() {
        let busy = EngineBusyState::new(WedgeKind::ReadSide, SinceStamp::new(Duration::ZERO));
        let view = busy.view();
        assert_eq!(view.title, "Engine busy");
        assert_eq!(
            view.choices,
            vec![
                "[i] Interrupt".to_string(),
                "[r] Restart".to_string(),
                "[<Esc>] Dismiss".to_string(),
            ]
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
        assert!(state.auto_restart);
        assert!(!state.already_offered(WedgeKind::ReadSide));
        state.note_offered(WedgeKind::ReadSide);
        assert!(state.already_offered(WedgeKind::ReadSide));
        assert!(!state.already_offered(WedgeKind::Dead));
        state.forget_episode();
        assert!(!state.already_offered(WedgeKind::ReadSide));
    }
}
