//! Settles which view feature holds a key that two of them claim: a
//! default map, a `[keys]` entry, a `[keys.desktop]` chord, or a key view's
//! own surfaces take before nvim sees it.

use std::borrow::Cow;

use view_core::msg::RpcCall;
use view_core::native::chords::{DesktopModifier, KeyProfile, DESKTOP_CHORD_COUNT};
use view_core::native::keys::{canonical_keys, Action, KeyBindings};
use view_core::native::mappings::MappingSpec;

use crate::config::{
    profile, ConfigKey, NativeConfig, Resolved, ResolvedConfig, Source, KEY_ACTIONS, UI_KEYS,
};
use crate::mappings::register_plan;

/// A key nvim answers in a buffer, through a registered mapping.
const BUFFER: u8 = 1;
/// A key view's own surfaces answer before nvim sees it.
const SURFACES: u8 = 2;

/// Everything that decides which keys view holds this run.
#[derive(Debug, Clone, Copy)]
pub struct Inputs<'a> {
    /// The `[native]` switches.
    pub cfg: &'a NativeConfig,
    /// The key each of [`UI_KEYS`] registers under, in its order.
    pub ui_lhs: &'a [String; UI_KEYS.len()],
    /// `[keys.desktop]`'s resolved rows.
    pub desktop: &'a [Resolved<String>; DESKTOP_CHORD_COUNT],
    /// The bindings `[keys]` resolved to, before any collision is settled.
    pub bindings: &'a KeyBindings,
    /// The profile whose chords are in play.
    pub profile: KeyProfile,
    /// The modifier the chords are spelled with.
    pub modifier: DesktopModifier,
    /// Whether the agent panel is on.
    pub ai: bool,
    /// Whether the recording is on.
    pub dvr: bool,
}

/// The keys view holds once every collision is settled.
#[derive(Debug, Default)]
pub struct Settled {
    /// The mappings to register, one per key, in registration order.
    pub specs: Vec<MappingSpec>,
    /// The bindings view's own surfaces answer.
    pub bindings: KeyBindings,
    /// One for each collision settled.
    pub notices: Vec<String>,
    /// Each config row put back on its default.
    pub put_back: Vec<PutBack>,
    /// Whether any desktop chord is registered.
    pub chords: bool,
}

/// A config row whose keys were put back on its default because another
/// view feature holds the key it moved onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutBack {
    /// The row's table, `keys` or `keys.desktop`.
    pub table: &'static str,
    /// The row's key in that table.
    pub key: &'static str,
    /// The keys it holds this run, as a report prints them.
    pub value: String,
}

/// Every key view holds under `inputs`, with each key two view features
/// claim in one scope settled.
///
/// A feature moved onto a key another one holds goes back on its defaults.
/// A feature on its defaults keeps the key, and of two moved onto one key
/// the first keeps it. A buffer mapping and a key view's own surfaces
/// answer never collide, since the surface answers it where the mapping
/// does not.
#[must_use]
pub fn settle(inputs: &Inputs<'_>) -> Settled {
    let mut claimants = claimants(inputs);
    let (notices, put_back) = settle_claimants(&mut claimants);
    let chords = claimants
        .iter()
        .any(|c| c.row.is_some_and(|(table, _)| table == "keys.desktop"));
    let mut bindings = inputs.bindings.clone();
    let specs = apply(claimants, &mut bindings);
    Settled {
        specs,
        bindings,
        notices,
        put_back,
        chords,
    }
}

impl ResolvedConfig {
    /// The keys this configuration holds once every collision is settled,
    /// under `modifier` and with the agent panel on when `ai` is.
    #[must_use]
    pub fn held_keys(&self, modifier: DesktopModifier, ai: bool) -> Settled {
        settle(&Inputs {
            cfg: &self.tables.native,
            ui_lhs: self.tables.keys.ui_lhs(),
            desktop: &self.desktop,
            bindings: self.tables.keys.bindings(),
            profile: self.profile.value,
            modifier,
            ai,
            dvr: self.tables.dvr.enabled,
        })
    }

    /// [`Self::rows`] with each key row the run puts back on its default
    /// showing that default as derived, and the notice each collision owes.
    #[must_use]
    pub fn held_rows(
        &self,
        modifier: DesktopModifier,
        ai: bool,
    ) -> (Vec<(&'static ConfigKey, String, Source)>, Vec<String>) {
        let held = self.held_keys(modifier, ai);
        let rows = self
            .rows()
            .into_iter()
            .map(|(key, value, source)| {
                match held
                    .put_back
                    .iter()
                    .find(|back| (back.table, back.key) == (key.table, key.key))
                {
                    Some(back) => (key, back.value.clone(), Source::Derived),
                    None => (key, value, source),
                }
            })
            .collect();
        (rows, held.notices)
    }
}

/// One view feature's keys this run, and the keys it holds when no config
/// row moves them.
#[derive(Debug, Clone)]
struct Claimant {
    /// The config row that moves the keys, `None` for a default map no row
    /// moves.
    row: Option<(&'static str, &'static str)>,
    /// The feature and verb a notice names, `ui gaps` or `sidebar wider`.
    name: String,
    keys: Vec<String>,
    /// [`Self::keys`], each in its canonical spelling.
    canonical: Vec<Vec<String>>,
    defaults: Vec<String>,
    /// [`BUFFER`], [`SURFACES`], or both.
    scope: u8,
    /// The mapping each key registers, `None` for a key only view's own
    /// surfaces answer.
    spec: Option<MappingSpec>,
    /// The binding view's own surfaces resolve the keys through.
    action: Option<Action>,
}

impl Claimant {
    /// A claimant registering `spec` under each of `keys`, holding the
    /// spec's own key when `row` moves nothing.
    fn spec(
        row: Option<(&'static str, &'static str)>,
        spec: MappingSpec,
        keys: Vec<String>,
    ) -> Self {
        Self {
            row,
            name: format!("{} {}", spec.feature, spec.verb),
            defaults: vec![spec.lhs.to_string()],
            canonical: Vec::new(),
            keys,
            scope: BUFFER,
            spec: Some(spec),
            action: None,
        }
        .canonicalized()
    }

    fn canonicalized(mut self) -> Self {
        self.canonical = self.keys.iter().map(|key| same(key)).collect();
        self
    }

    fn set_keys(&mut self, keys: Vec<String>) {
        self.keys = keys;
        self.canonical = self.keys.iter().map(|key| same(key)).collect();
    }

    /// Whether a config row moved it onto a key outside its defaults.
    fn moved(&self) -> bool {
        let defaults: Vec<Vec<String>> = self.defaults.iter().map(|key| same(key)).collect();
        !self.canonical.iter().all(|key| defaults.contains(key))
    }

    /// `[keys] dvr_scrub`, the way a notice names the row.
    fn entry(&self) -> String {
        self.row
            .map_or_else(String::new, |(table, key)| format!("[{table}] {key}"))
    }
}

/// The form two spellings of one key compare equal in.
fn same(key: &str) -> Vec<String> {
    canonical_keys(key)
}

/// Whether two claimants answer a key in one place.
fn shares_scope(a: &Claimant, b: &Claimant) -> bool {
    a.scope & b.scope != 0
}

/// Every view feature that claims a key under `inputs`, in registration
/// order: the default maps, the desktop chords, then the keys only view's
/// own surfaces answer.
fn claimants(inputs: &Inputs<'_>) -> Vec<Claimant> {
    let RpcCall::RegisterMappings { specs, .. } = register_plan(inputs.cfg, 0) else {
        return Vec::new();
    };
    // `[ai]` has no `[native]` switch and the recording's lives in `[dvr]`,
    // so both are applied here, to the chords as well
    let on = |feature: &str| match feature {
        "ai" => inputs.ai,
        "dvr" => inputs.dvr,
        _ => true,
    };
    let mut claimants = Vec::with_capacity(specs.len() + DESKTOP_CHORD_COUNT + KEY_ACTIONS.len());
    for spec in specs.into_iter().filter(|spec| on(spec.feature)) {
        let ui = UI_KEYS
            .iter()
            .zip(inputs.ui_lhs)
            .find(|(ui, _)| (ui.feature, ui.verb) == (spec.feature, spec.verb));
        let claimant = if let Some((ui, lhs)) = ui {
            Claimant::spec(Some(("keys", ui.key)), spec, vec![lhs.clone()])
        } else if (spec.feature, spec.verb) == ("window", "resize_mode") {
            // `[keys] resize_mode` names every key the mode answers to, in
            // a buffer and on view's own surfaces alike
            Claimant {
                defaults: KeyBindings::default().spellings(Action::ResizeMode),
                scope: BUFFER | SURFACES,
                action: Some(Action::ResizeMode),
                ..Claimant::spec(
                    Some(("keys", "resize_mode")),
                    spec,
                    inputs.bindings.spellings(Action::ResizeMode),
                )
            }
            .canonicalized()
        } else {
            let keys = vec![spec.lhs.to_string()];
            Claimant::spec(None, spec, keys)
        };
        claimants.push(claimant);
    }
    let chords = profile::chord_rows(inputs.desktop, inputs.profile, inputs.modifier, inputs.cfg);
    for (chord, spec) in chords.into_iter().filter(|(_, spec)| on(spec.feature)) {
        let keys = vec![spec.lhs.to_string()];
        claimants.push(Claimant {
            defaults: vec![chord.lhs(inputs.modifier).to_string()],
            ..Claimant::spec(Some(("keys.desktop", chord.id)), spec, keys)
        });
    }
    // the tree, the agent panel and a windowed notification stream each
    // answer the sidebar width keys while they hold focus
    let sidebar = inputs.cfg.enabled("tree") || inputs.cfg.enabled("notifications") || inputs.ai;
    for (key, action) in KEY_ACTIONS {
        let answered = match action {
            Action::Resize(_) => sidebar,
            Action::ComposerNewline => inputs.ai,
            // held through its default map above
            Action::ResizeMode => false,
        };
        if !answered {
            continue;
        }
        claimants.push(
            Claimant {
                row: Some(("keys", key)),
                name: key.replacen('_', " ", 1),
                keys: inputs.bindings.spellings(action),
                canonical: Vec::new(),
                defaults: KeyBindings::default().spellings(action),
                scope: SURFACES,
                spec: None,
                action: Some(action),
            }
            .canonicalized(),
        );
    }
    claimants
}

/// How [`settle_claimants`] settles one collision.
enum Collision {
    /// `moved` goes back on its defaults, leaving `key` to `holder`.
    PutBack {
        holder: usize,
        moved: usize,
        key: String,
    },
    /// Two features on their defaults share `key`: `other` gives it up.
    Drop {
        holder: usize,
        other: usize,
        key: String,
    },
}

/// Settles every collision in `claimants` and returns the notices and the
/// rows put back. Repeats until no key has two claimants in one scope,
/// because a default put back can be the key another claimant was moved
/// onto. Each round puts a moved claimant back for good or drops a key,
/// so it ends.
fn settle_claimants(claimants: &mut [Claimant]) -> (Vec<String>, Vec<PutBack>) {
    let mut notices = Vec::new();
    let mut put_back = Vec::new();
    while let Some(collision) = collision(claimants) {
        match collision {
            Collision::PutBack { holder, moved, key } => {
                let Some(name) = claimants.get(holder).map(|h| h.name.clone()) else {
                    break;
                };
                let Some(moved) = claimants.get_mut(moved) else {
                    break;
                };
                let defaults = moved.defaults.join(", ");
                notices.push(format!(
                    "view: {} = \"{key}\" is the key `{name}` already holds. `{}` stays on {defaults} this run",
                    moved.entry(),
                    moved.name,
                ));
                if let Some((table, key)) = moved.row {
                    put_back.push(PutBack {
                        table,
                        key,
                        value: defaults,
                    });
                }
                let keys = moved.defaults.clone();
                moved.set_keys(keys);
            }
            Collision::Drop { holder, other, key } => {
                let Some(name) = claimants.get(holder).map(|h| h.name.clone()) else {
                    break;
                };
                let Some(other) = claimants.get_mut(other) else {
                    break;
                };
                notices.push(format!(
                    "view: `{name}` and `{}` both hold {key} by default. `{name}` keeps it this run",
                    other.name,
                ));
                let dropped = same(&key);
                let keys = other
                    .keys
                    .iter()
                    .filter(|k| same(k) != dropped)
                    .cloned()
                    .collect();
                other.set_keys(keys);
            }
        }
    }
    (notices, put_back)
}

/// The next collision to settle: the first moved claimant on a key one on
/// its defaults holds, then the first two moved onto one key, then the
/// first two defaults that share one.
fn collision(claimants: &[Claimant]) -> Option<Collision> {
    let moved: Vec<bool> = claimants.iter().map(Claimant::moved).collect();
    let mut both_moved = None;
    let mut both_held = None;
    for (i, first) in claimants.iter().enumerate() {
        for (j, other) in claimants.iter().enumerate().skip(i + 1) {
            if !shares_scope(first, other) {
                continue;
            }
            let Some(shared) = other.canonical.iter().find(|k| first.canonical.contains(k)) else {
                continue;
            };
            match (moved[i], moved[j]) {
                (false, true) => {
                    return Some(Collision::PutBack {
                        holder: i,
                        moved: j,
                        key: spelled(other, shared),
                    })
                }
                (true, false) => {
                    return Some(Collision::PutBack {
                        holder: j,
                        moved: i,
                        key: spelled(first, shared),
                    })
                }
                (true, true) => {
                    both_moved.get_or_insert(Collision::PutBack {
                        holder: i,
                        moved: j,
                        key: spelled(other, shared),
                    });
                }
                (false, false) => {
                    both_held.get_or_insert(Collision::Drop {
                        holder: i,
                        other: j,
                        key: spelled(other, shared),
                    });
                }
            }
        }
    }
    both_moved.or(both_held)
}

/// The key `claimant` spells as `canonical`.
fn spelled(claimant: &Claimant, canonical: &[String]) -> String {
    claimant
        .canonical
        .iter()
        .zip(&claimant.keys)
        .find_map(|(k, key)| (k == canonical).then(|| key.clone()))
        .unwrap_or_default()
}

/// The specs `claimants` register, in their order, and `bindings` with
/// each claimant's action answering to its keys.
fn apply(claimants: Vec<Claimant>, bindings: &mut KeyBindings) -> Vec<MappingSpec> {
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

#[cfg(test)]
mod tests;
