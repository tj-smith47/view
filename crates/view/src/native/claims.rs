//! Settles which view feature holds a key that two of them claim: a
//! default map, a `[keys]` entry, a `[keys.desktop]` chord, or a key view's
//! own surfaces take before nvim sees it.

use std::borrow::Cow;

use view_core::native::keys::{Action, KeyBindings};
use view_core::native::mappings::MappingSpec;

/// One view feature's keys this run, and the keys it holds when no config
/// entry moves them.
#[derive(Debug, Clone)]
pub(super) struct Claimant {
    /// The config entry that moves the keys, as a notice names it.
    pub(super) entry: String,
    /// The feature and verb a notice names, `ui gaps` or `sidebar wider`.
    pub(super) name: String,
    pub(super) keys: Vec<String>,
    pub(super) defaults: Vec<String>,
    /// The mapping each key registers, `None` for a key only view's own
    /// surfaces answer.
    pub(super) spec: Option<MappingSpec>,
    /// The binding view's own surfaces resolve the keys through.
    pub(super) action: Option<Action>,
}

impl Claimant {
    /// A claimant registering `spec` under each of `keys`, holding
    /// `defaults` when `entry` moves nothing.
    pub(super) fn spec(entry: String, spec: MappingSpec, keys: Vec<String>) -> Self {
        Self {
            entry,
            name: format!("{} {}", spec.feature, spec.verb),
            defaults: vec![spec.lhs.to_string()],
            keys,
            spec: Some(spec),
            action: None,
        }
    }

    fn moved(&self) -> bool {
        self.keys != self.defaults
    }
}

/// Puts every claimant moved onto a key another one holds back on its
/// defaults, with one notice naming both features and the key, and returns
/// the notices. A claimant on its defaults keeps the key, and of two moved
/// ones the first keeps it. Repeats until no key has two claimants, because
/// a default put back can be the key another claimant was moved onto.
pub(super) fn settle(claimants: &mut [Claimant]) -> Vec<String> {
    let mut notices = Vec::new();
    while let Some((holder, moved, key)) = collision(claimants) {
        let Some(name) = claimants.get(holder).map(|h| h.name.clone()) else {
            break;
        };
        let Some(moved) = claimants.get_mut(moved) else {
            break;
        };
        notices.push(format!(
            "view: {} = \"{key}\" is the key `{name}` already holds. `{}` stays on {} this run",
            moved.entry,
            moved.name,
            moved.defaults.join(", ")
        ));
        moved.keys = moved.defaults.clone();
    }
    notices
}

/// The first key two claimants hold where one of them was moved onto it:
/// the claimant that keeps it, the one that goes back, and the key.
fn collision(claimants: &[Claimant]) -> Option<(usize, usize, String)> {
    for (i, first) in claimants.iter().enumerate() {
        for key in &first.keys {
            for (j, other) in claimants.iter().enumerate().skip(i + 1) {
                if !other.keys.contains(key) {
                    continue;
                }
                if other.moved() {
                    return Some((i, j, key.clone()));
                }
                if first.moved() {
                    return Some((j, i, key.clone()));
                }
            }
        }
    }
    None
}

/// The specs `claimants` register, in their order, and `bindings` with
/// each claimant's action answering to its keys.
pub(super) fn apply(claimants: Vec<Claimant>, bindings: &mut KeyBindings) -> Vec<MappingSpec> {
    let mut specs = Vec::with_capacity(claimants.len());
    for claimant in claimants {
        if let Some(action) = claimant.action {
            if bindings.spellings(action) != claimant.keys {
                // a claimant's keys are its config's spellings or this
                // build's defaults, both of which the bindings already read
                let _ = bindings.rebind(action, &claimant.keys);
            }
        }
        let Some(spec) = claimant.spec else { continue };
        specs.extend(claimant.keys.into_iter().map(|key| MappingSpec {
            lhs: Cow::Owned(key),
            ..spec.clone()
        }));
    }
    specs
}
