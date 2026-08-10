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

/// How often a visible readout changes, and so how often a loop showing one
/// has something new to paint.
///
/// The whole point of an elapsed readout is that it moves: a user looking at
/// a frozen `(41s)` learns nothing about whether the wait is still going.
/// Nothing else wakes a loop while the engine is quiet -- that is what being
/// quiet means -- so this is the cadence a wedged session has to ask for,
/// and it is asked for only while a wedge is on screen.
pub const READOUT_RESOLUTION: Duration = Duration::from_secs(1);

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
/// this nor the liveness probe -- that wedge's only recovery is a restart,
/// which the modal cannot yet offer: [`SupervisionChoice::Restart`] is
/// listed nowhere until `Engine::restart()` exists, so the modal presents
/// the interrupt without claiming it answers every wedge. A third
/// live-verified fact: nvim discards its unread typeahead when the
/// interrupt lands, so keys still queued through `nvim_input` at that
/// moment are lost by nvim itself -- identically with or without the
/// modal on screen.
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
    /// No runtime path produces this yet: a connection that closes ends the
    /// session through the intake's own `EngineDown` handling long before a
    /// supervision reading could see it. It becomes reachable with the
    /// respawn, which is what turns a closed connection from the end of a
    /// session into a state a session can be recovered from.
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
    /// present a button that cannot do anything.
    ///
    /// [`SupervisionChoice::Restart`] is absent everywhere, and that is a
    /// fact about what the effect can currently reach rather than about the
    /// choice: nothing yet replaces a live engine, so the only thing it can
    /// currently reach is the shutdown path, which would answer a request
    /// to recover the session by ending it. A button may not be offered
    /// before the thing behind it exists, so the choice stays modelled and
    /// unlisted until the respawn lands.
    #[must_use]
    pub fn choices(self) -> Vec<SupervisionChoice> {
        match self {
            Self::ReadSide | Self::WriteSide => {
                vec![SupervisionChoice::Interrupt, SupervisionChoice::Dismiss]
            }
            Self::Dead => vec![SupervisionChoice::Dismiss],
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
    ///
    /// Constrained by the modal taking no focus: the key that picks a choice
    /// still reaches the engine as ordinary input, so a listed choice may
    /// only bind a key whose meaning to nvim is the thing the choice does,
    /// or one no user types by reflex. A bare printable key would fail both
    /// halves -- typing `i` at an editor that is merely slow means "insert",
    /// and answering it with an abort would destroy the operation the user
    /// was waiting out. [`Restart`](Self::Restart) is unlisted (see
    /// [`WedgeKind::choices`]) and its key is placeholder plumbing that has
    /// to satisfy the same rule before it can be offered.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Interrupt => INTERRUPT_NOTATION,
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

/// Supervision's memory of the current wedge episode.
///
/// It is what keeps a dismissed modal dismissed: the banner keeps
/// re-asserting for as long as the condition holds, so an escalation rule
/// with no memory would re-open the modal on the very next observation
/// after a user closed it.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupervisionState {
    /// The wedge the user has already been offered a choice for, if any.
    offered: Option<WedgeKind>,
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
        assert!(dead.offers(SupervisionChoice::Dismiss));
        assert_eq!(dead.choose(SupervisionChoice::Interrupt.key()), None);
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

    /// No path a user can reach may offer a recovery that does not exist
    /// yet. The choice, its key and its effect all stay modelled -- the
    /// respawn that will consume them is built against them -- so the only
    /// thing standing between a wedged session and a button that ends it
    /// is this list, and this test is what holds the list closed until the
    /// respawn lands and flips it deliberately.
    #[test]
    fn no_wedge_offers_a_restart_while_nothing_can_perform_one() {
        for kind in [WedgeKind::ReadSide, WedgeKind::WriteSide, WedgeKind::Dead] {
            assert!(
                !kind.choices().contains(&SupervisionChoice::Restart),
                "{kind:?} offers Restart, which currently reaches the shutdown path"
            );
            let busy = EngineBusyState::new(kind, SinceStamp::default());
            assert!(!busy.offers(SupervisionChoice::Restart), "{kind:?}");
            assert_eq!(
                busy.choose(SupervisionChoice::Restart.key()),
                None,
                "{kind:?} resolves the restart key"
            );
            assert!(
                !busy
                    .view()
                    .choices
                    .iter()
                    .any(|row| row.contains("Restart")),
                "{kind:?} paints a Restart row"
            );
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
                "[<Esc>] Dismiss".to_string()
            ]
        );
    }

    #[test]
    fn no_offered_choice_binds_a_key_a_user_could_type_by_reflex() {
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
            }
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
