//! The key log's place on the model: the ring every fired mapping is pushed
//! onto, whose each of the user's own mappings is, and the open overlay
//! kept level with the ring.

use std::collections::HashMap;
use std::time::SystemTime;

use super::{Model, OverlayKind};
use crate::native::geometry::Anchor;
use crate::native::key_log::{Fired, KeyLog, KeyLogView};
use crate::native::keys::canonical_keys;
use crate::native::mappings::MappingOwner;
use crate::native::submit_hold::canonical_typed;

/// The key log and what it needs to describe a row.
#[derive(Debug, Clone, Default)]
pub(crate) struct KeyLogState {
    pub(crate) log: KeyLog,
    /// Each of the user's own normal-mode key sequences, one canonical key
    /// per entry, to its `keytrans()` spelling and whose it is.
    owners: HashMap<Vec<String>, (String, MappingOwner)>,
    /// The same sequences to their `keytrans()` spelling alone, for a
    /// mapping whose owner has not been read.
    spellings: HashMap<Vec<String>, String>,
    /// Whether the next invocation folded is one view runs again for a
    /// press already logged.
    skip_next: bool,
}

impl KeyLogState {
    /// Logs the user's own mapping `keys` spell, whose last key went to
    /// nvim in normal mode at `at`, written as `keytrans()` writes it.
    pub(crate) fn log_user_mapping(&mut self, keys: &[String], at: SystemTime) {
        let typed = canonical_typed(keys);
        let fired = match self.owners.get(&typed) {
            Some((lhs, owner)) => Fired::User {
                lhs: lhs.clone(),
                owner: Some(owner.clone()),
            },
            None => Fired::User {
                lhs: self
                    .spellings
                    .get(&typed)
                    .cloned()
                    .unwrap_or_else(|| typed.concat()),
                owner: None,
            },
        };
        self.log.push_at(fired, at);
    }
}

impl Model {
    /// The mappings that fired this session, newest first.
    #[must_use]
    pub fn key_log(&self) -> &KeyLog {
        &self.key_log.log
    }

    /// Learns whose each of the user's normal-mode mappings is, from pairs
    /// of the keys as `keytrans()` spells them and the mapping's owner.
    pub(crate) fn learn_user_owners(&mut self, owners: &[(String, MappingOwner)]) {
        self.key_log.owners = owners
            .iter()
            .map(|(spelled, owner)| (canonical_keys(spelled), (spelled.clone(), owner.clone())))
            .collect();
    }

    /// Learns how `keytrans()` spells each of the user's normal-mode key
    /// sequences.
    pub(crate) fn learn_user_spellings(&mut self, keys: &[String]) {
        self.key_log.spellings = keys
            .iter()
            .map(|spelled| (canonical_keys(spelled), spelled.clone()))
            .collect();
    }

    /// Marks the next invocation folded as one view runs again for a press
    /// the log already holds, so it logs no second row.
    pub(crate) fn skip_next_invocation_log(&mut self) {
        self.key_log.skip_next = true;
    }

    /// Logs a view invocation as it is folded. The row names the key that
    /// set it off and the mapping that key displaced, read from the claim
    /// whose keys those are, so a desktop chord and the leader key it
    /// mirrors are told apart. A `:View` typed by hand set off no key and
    /// logs none, and neither do keys no claim of the feature names.
    pub(crate) fn log_invocation(&mut self, feature: &str, verb: &str) {
        if std::mem::take(&mut self.key_log.skip_next) {
            return;
        }
        let keys = self.submit_hold.take_invoked();
        let claim = keys.as_ref().and_then(|keys| {
            self.claimed_keys().iter().find(|claim| {
                claim.feature == feature
                    && claim
                        .keys
                        .as_deref()
                        .is_some_and(|spelled| canonical_keys(spelled) == *keys)
            })
        });
        // keys that match no claim of this feature did not set it off
        let lhs = claim.map(|claim| claim.keys.clone().unwrap_or_else(|| claim.lhs.clone()));
        let fired = Fired::View {
            feature: feature.to_string(),
            verb: verb.to_string(),
            lhs,
            displaced: claim.and_then(|claim| claim.displaced.clone()),
        };
        self.key_log.log.push(fired);
    }

    /// Catches an open key log up with the ring and the clock's offset,
    /// and keeps it clear of the cursor: at the bottom of the screen, or at
    /// the top while the cursor's row falls where the bottom box would sit.
    /// Answers whether its rows or its place changed: two integers and a
    /// rect on a fold that logged nothing.
    #[must_use]
    pub fn refresh_key_log(&mut self) -> bool {
        let offset = self.utc_offset_secs();
        let log = &self.key_log.log;
        let Some(pos) = self
            .overlays
            .iter()
            .position(|overlay| matches!(overlay.kind, OverlayKind::KeyLog(_)))
        else {
            return false;
        };
        let rows = match &mut self.overlays[pos].kind {
            OverlayKind::KeyLog(view) => view.refresh(log, offset),
            _ => false,
        };
        let was = self.overlays[pos].geometry.anchor;
        self.overlays[pos].geometry.anchor = Anchor::Bottom;
        let bottom = self.overlay_rect(&self.overlays[pos]);
        let cursor_row = self
            .chrome_rows()
            .saturating_add(self.look.grid_offset())
            .saturating_add(self.engine.painted_grids().cursor_pos().0);
        let covered = (bottom.row..bottom.row.saturating_add(bottom.height)).contains(&cursor_row);
        let anchor = if covered { Anchor::Top } else { Anchor::Bottom };
        self.overlays[pos].geometry.anchor = anchor;
        rows || anchor != was
    }

    /// The open key log overlay's state and the ring it shows, wherever
    /// the overlay sits in the stack.
    pub(crate) fn key_log_view_mut(&mut self) -> Option<(&mut KeyLogView, &KeyLog)> {
        let log = &self.key_log.log;
        self.overlays
            .iter_mut()
            .find_map(|overlay| match &mut overlay.kind {
                OverlayKind::KeyLog(view) => Some(view),
                _ => None,
            })
            .map(|view| (view, log))
    }

    /// Closes the key log, wherever it sits in the stack, answering whether
    /// one was open.
    pub(crate) fn close_key_log(&mut self) -> bool {
        let Some(pos) = self
            .overlays
            .iter()
            .position(|overlay| matches!(overlay.kind, OverlayKind::KeyLog(_)))
        else {
            return false;
        };
        self.take_overlay_at(pos);
        true
    }
}
