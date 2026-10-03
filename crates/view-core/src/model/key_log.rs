//! The key log's place on the model: the ring every fired mapping is pushed
//! onto, whose each of the user's own mappings is, and the open overlay
//! kept level with the ring.

use std::collections::HashMap;

use super::{Model, OverlayKind};
use crate::native::key_log::{Fired, KeyLog, KeyLogView};
use crate::native::mappings::MappingOwner;
use crate::native::submit_hold::{canonical_keys, canonical_typed};

/// The key log and what it needs to describe a row.
#[derive(Debug, Clone, Default)]
pub(crate) struct KeyLogState {
    pub(crate) log: KeyLog,
    /// Each of the user's own normal-mode key sequences, one canonical key
    /// per entry, to its `keytrans()` spelling and whose it is.
    owners: HashMap<Vec<String>, (String, Option<MappingOwner>)>,
}

impl Model {
    /// The mappings that fired this session, newest first.
    #[must_use]
    pub fn key_log(&self) -> &KeyLog {
        &self.key_log.log
    }

    /// Learns whose each of the user's normal-mode mappings is, from the
    /// registration's own read of them: `owners[i]` describes `keys[i]`.
    pub(crate) fn learn_user_owners(&mut self, keys: &[String], owners: &[Option<MappingOwner>]) {
        self.key_log.owners = keys
            .iter()
            .enumerate()
            .map(|(i, spelled)| {
                let owner = owners.get(i).cloned().flatten();
                (canonical_keys(spelled), (spelled.clone(), owner))
            })
            .collect();
    }

    /// Logs a view invocation as it is folded. The key is named only when
    /// a key nvim maps to the invocation armed the hold standing behind
    /// it, so a `:View` typed by hand logs no key.
    pub(crate) fn log_invocation(&mut self, feature: &str, verb: &str) {
        let claim = self
            .submit_hold
            .fired_by_key()
            .then(|| {
                self.claimed_keys()
                    .iter()
                    .find(|claim| claim.feature == feature && claim.verb == verb)
            })
            .flatten();
        let fired = Fired::View {
            feature: feature.to_string(),
            verb: verb.to_string(),
            lhs: claim.map(|claim| claim.keys.clone().unwrap_or_else(|| claim.lhs.clone())),
            displaced: claim.and_then(|claim| claim.displaced.clone()),
        };
        self.key_log.log.push(fired);
    }

    /// Logs the user's own mapping `keys` spell, as its last key goes to
    /// nvim in normal mode.
    pub(crate) fn log_user_mapping(&mut self, keys: &[String]) {
        let typed = canonical_typed(keys);
        let fired = match self.key_log.owners.get(&typed) {
            Some((lhs, owner)) => Fired::User {
                lhs: lhs.clone(),
                owner: owner.clone(),
            },
            None => Fired::User {
                lhs: keys.concat(),
                owner: None,
            },
        };
        self.key_log.log.push(fired);
    }

    /// Catches an open key log up with the ring, answering whether a row
    /// arrived: two integers on a fold that logged nothing.
    #[must_use]
    pub fn refresh_key_log(&mut self) -> bool {
        let log = &self.key_log.log;
        self.overlays
            .iter_mut()
            .find_map(|overlay| match &mut overlay.kind {
                OverlayKind::KeyLog(view) => Some(view),
                _ => None,
            })
            .is_some_and(|view| view.refresh(log))
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
