//! Which overlay, if any, takes the keyboard, and which of the sidebars
//! that can either float or take a window of their own currently do.
//!
//! Split out of `model.rs` to keep it under the production-line ceiling
//! (`scripts/audit-god-files.sh`): every windowed surface `Model` learns to
//! draw adds a branch to [`Model::takes_focus_now`] and
//! [`Model::draws_as_overlay`], and both belong beside the accessors that
//! feed them rather than inflating the file that also carries buffer and
//! engine bookkeeping.

use super::{Model, Overlay, OverlayId, OverlayKind};
use crate::native::geometry::NativeSurface;

impl Model {
    /// Whether `kind` takes the keyboard while merely open, with no other
    /// state consulted.
    ///
    /// Every kind does except [`OverlayKind::EngineBusy`] and
    /// [`OverlayKind::Ai`] -- `Ai` unconditionally here, since this form
    /// cannot see [`Model::ai_panel`]'s own `focused` flag. It is what it
    /// is: the pure, kind-only question `open_ai_panel`'s insert-beneath
    /// check needs, evaluated against whatever overlay already sits on top
    /// of the stack -- never `OverlayKind::Ai` itself, since that call only
    /// runs while the panel is closed. Every other caller wants
    /// [`Self::takes_focus_now`] instead, which layers the panel's own
    /// state on top of this for `Ai`.
    ///
    /// `EngineBusy` is raised by view noticing something rather than by the
    /// user asking for it, and is on screen precisely when the engine may
    /// be slow to answer. A user who keeps typing at a long operation has
    /// always had those keystrokes queued and applied on catch-up, so an
    /// annunciator that consumed them would turn a slow operation into lost
    /// work. It answers its own choice keys, and every other key routes as
    /// though it were not there.
    pub(crate) const fn takes_focus(kind: &OverlayKind) -> bool {
        !matches!(kind, OverlayKind::EngineBusy(_) | OverlayKind::Ai)
    }

    /// Whether `kind` takes the keyboard right now, on this model -- what
    /// every focus-resolution method below actually wants.
    ///
    /// Identical to [`Self::takes_focus`] for every kind but
    /// [`OverlayKind::Ai`]: the panel is non-modal by design (see that
    /// variant's own doc), so its mere presence on the stack must not
    /// redirect the engine's own keystrokes. It takes the keyboard only
    /// once the user has deliberately entered it -- `ai_entered`, read from
    /// [`crate::native::ai_panel::AiPanelState::focused`] -- never by side
    /// effect of an agent auto-opening it. Takes the flag as a plain `bool`
    /// rather than `&self`, so [`Self::focused_overlay_mut`] and
    /// [`Self::pop_focused_overlay`] can read `ai_panel.focused` once,
    /// ahead of borrowing `overlays` mutably, instead of needing both
    /// borrows live at the same time.
    const fn takes_focus_now(
        kind: &OverlayKind,
        ai_entered: bool,
        tree_windowed: bool,
        agent_windowed: bool,
        notifications_windowed: bool,
    ) -> bool {
        match kind {
            // a windowed agent panel is a pane nvim's own cursor moves
            // into, the same as the tree below: its keys and pastes route
            // through `Focus::Pane(Agent)` instead, so the overlay must
            // never claim focus out from under that
            OverlayKind::Ai if agent_windowed => false,
            OverlayKind::Ai => ai_entered,
            // a windowed tree is a pane nvim's own cursor moves into, so
            // its state riding the overlay stack must not redirect a key
            // the user aimed at the buffer
            OverlayKind::Tree(_) => !tree_windowed,
            // the windowed notification stream, same reasoning: its keys
            // route through `Focus::Pane(Notifications)` instead
            OverlayKind::MessageHistory(_) if notifications_windowed => false,
            other => Self::takes_focus(other),
        }
    }

    /// Whether the overlay carrying `kind` is drawn as a float. False for
    /// the tree, the agent panel and the message history while any of them
    /// is windowed: their state rides the overlay stack in both placements,
    /// and the compositor paints them into their pane instead.
    #[must_use]
    pub fn draws_as_overlay(&self, kind: &OverlayKind) -> bool {
        match kind {
            OverlayKind::Tree(_) => !self.surfaces.windowed(NativeSurface::Tree),
            OverlayKind::Ai => !self.surfaces.windowed(NativeSurface::Agent),
            OverlayKind::MessageHistory(_) => !self.surfaces.windowed(NativeSurface::Notifications),
            _ => true,
        }
    }

    /// The overlay [`Self::focus`] names, or `None` while the engine owns
    /// the keyboard.
    ///
    /// The kind-carrying form of [`Self::focus`], for a caller that has to
    /// know *which* feature holds the keys rather than merely that some
    /// overlay does -- `view-surface` places the real terminal caret in the
    /// surface that owns input, and an `OverlayId` alone cannot say which
    /// surface that is.
    #[must_use]
    pub fn focused_overlay(&self) -> Option<&Overlay> {
        let ai_entered = self.ai_panel.focused;
        let tree_windowed = self.tree_is_windowed();
        let agent_windowed = self.agent_is_windowed();
        let notifications_windowed = self.notifications_is_windowed();
        self.overlays.iter().rev().find(|overlay| {
            Self::takes_focus_now(
                &overlay.kind,
                ai_entered,
                tree_windowed,
                agent_windowed,
                notifications_windowed,
            )
        })
    }

    /// The topmost focus-taking overlay, for a feature that needs to fold
    /// its own state forward as input arrives.
    #[must_use]
    pub fn focused_overlay_mut(&mut self) -> Option<&mut Overlay> {
        let ai_entered = self.ai_panel.focused;
        let tree_windowed = self.tree_is_windowed();
        let agent_windowed = self.agent_is_windowed();
        let notifications_windowed = self.notifications_is_windowed();
        self.overlays.iter_mut().rev().find(|overlay| {
            Self::takes_focus_now(
                &overlay.kind,
                ai_entered,
                tree_windowed,
                agent_windowed,
                notifications_windowed,
            )
        })
    }

    /// Closes the overlay [`Model::focus`] names, wherever it sits in the
    /// stack, and returns it.
    ///
    /// Not the top of the stack: a non-focus-taking overlay may be sitting
    /// above it, and popping that instead would close an annunciator the
    /// user never addressed while leaving the overlay they did address open.
    pub fn pop_focused_overlay(&mut self) -> Option<Overlay> {
        let ai_entered = self.ai_panel.focused;
        let tree_windowed = self.tree_is_windowed();
        let agent_windowed = self.agent_is_windowed();
        let notifications_windowed = self.notifications_is_windowed();
        let pos = self.overlays.iter().rposition(|overlay| {
            Self::takes_focus_now(
                &overlay.kind,
                ai_entered,
                tree_windowed,
                agent_windowed,
                notifications_windowed,
            )
        })?;
        Some(self.take_overlay_at(pos))
    }

    /// The topmost overlay covering the terminal cell at `(row, col)`, or
    /// `None` when the cell belongs to the engine grid.
    ///
    /// Mouse input routes through this rather than through [`Model::focus`]:
    /// an open overlay owns the keyboard outright, but it owns only the
    /// cells it covers, so a click on visible grid outside it still reaches
    /// the engine.
    #[must_use]
    pub fn overlay_at(&self, row: u16, col: u16) -> Option<OverlayId> {
        self.overlays
            .iter()
            .rev()
            .find(|overlay| {
                self.draws_as_overlay(&overlay.kind)
                    && self.overlay_rect(overlay).contains(row, col)
            })
            .map(|overlay| overlay.id)
    }

    /// Whether the file tree takes a window in nvim's layout this session.
    #[must_use]
    pub fn tree_is_windowed(&self) -> bool {
        self.surfaces.windowed(NativeSurface::Tree)
    }

    /// Whether the agent panel takes a window in nvim's layout this session.
    #[must_use]
    pub fn agent_is_windowed(&self) -> bool {
        self.surfaces.windowed(NativeSurface::Agent)
    }

    /// Whether the command palette takes a window in nvim's layout this
    /// session.
    #[must_use]
    pub fn palette_is_windowed(&self) -> bool {
        self.surfaces.windowed(NativeSurface::Palette)
    }

    /// Whether the notification stream takes a window in nvim's layout
    /// this session.
    #[must_use]
    pub fn notifications_is_windowed(&self) -> bool {
        self.surfaces.windowed(NativeSurface::Notifications)
    }

    /// Closes the message-history overlay, wherever it sits in the stack,
    /// and reports whether one was found to close -- [`Self::close_tree`]'s
    /// own shape, for the same reason: a windowed placement's close must
    /// reach it even when a prompt has landed on top in the meantime.
    pub fn close_message_history(&mut self) -> bool {
        let Some(pos) = self
            .overlays
            .iter()
            .position(|overlay| matches!(overlay.kind, OverlayKind::MessageHistory(_)))
        else {
            return false;
        };
        self.take_overlay_at(pos);
        true
    }
}
