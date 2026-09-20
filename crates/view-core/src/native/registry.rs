//! The compile-time table of native features.
//!
//! The table is closed at compile time rather than registered into at
//! startup. A feature that fails to initialize therefore still appears in
//! every listing that reads this table, reported as present and broken; a
//! runtime registry would let it vanish silently and tell the user nothing
//! was superseded while the renderer they expected to lose is still
//! winning.

/// One native feature's consumer-facing identity: what it is called, what
/// it takes over, and how a user turns it off.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureDesc {
    /// Stable id. This is the consumer-facing key: it is the `[native]`
    /// table key, the doctor row name, and the off-switch name, and it may
    /// never drift between the three.
    pub id: &'static str,
    /// What a config already has for this surface, named the way a user
    /// would name it, or `None` for a feature that supersedes nothing.
    pub supersedes: Option<&'static str>,
    /// The exact line a user writes to turn this feature off, rendered
    /// verbatim by doctor and by the first-run toast. A rendered string
    /// rather than a derived one: a user who is told `native.picker =
    /// false` must be able to paste it, and deriving it at three call sites
    /// is three chances to drift.
    pub off_switch: &'static str,
    /// Whether this build ships default keys that invoke the feature, which
    /// [`mappings::default_maps`](super::mappings::default_maps) must then
    /// carry rows for. Two tables stating the same fact independently is the
    /// point: a key added for a feature nobody declared reachable, or a
    /// feature declared reachable with no key, is drift a test can name.
    /// `false` for a passive surface a user never invokes (a statusline is
    /// drawn, not opened).
    pub entry_keys: bool,
    /// What a session with no `view.toml` resolves this feature to, and so
    /// what the shipped example writes beside the key.
    ///
    /// `true` for a feature whose surface view is better at than the
    /// ecosystem's plugins, which is most of them and the reason `[native]`
    /// reads as an opt-out table. `false` where the plugin ecosystem's own
    /// answer is the one a migrating user already has and already likes: a
    /// default that took a working tabline away and drew a plainer one in
    /// its place would be view failing the painless-migration contract on
    /// row 0 of the first launch.
    pub default_on: bool,
}

// `supersedes` names the thing a config already has for this surface, in a
// user's own words: it is rendered as prose ("your own status line still
// loads") and matched against nothing.
static FEATURES: [FeatureDesc; 6] = [
    FeatureDesc {
        id: "picker",
        supersedes: Some("your own fuzzy finder"),
        off_switch: "native.picker = false",
        entry_keys: true,
        default_on: true,
    },
    FeatureDesc {
        id: "tree",
        supersedes: Some("your own file explorer"),
        off_switch: "native.tree = false",
        entry_keys: true,
        default_on: true,
    },
    FeatureDesc {
        id: "statusline",
        supersedes: Some("your own status line"),
        off_switch: "native.statusline = false",
        entry_keys: false,
        default_on: true,
    },
    FeatureDesc {
        id: "notifications",
        supersedes: Some("your own notifier"),
        off_switch: "native.notifications = false",
        entry_keys: true,
        default_on: true,
    },
    FeatureDesc {
        id: "palette",
        supersedes: Some("your own command-line UI"),
        off_switch: "native.palette = false",
        entry_keys: false,
        default_on: true,
    },
    FeatureDesc {
        id: "tabline",
        supersedes: Some("your own tab line"),
        off_switch: "native.tabline = false",
        entry_keys: false,
        default_on: false,
    },
];

/// Every native feature this build ships, in listing order.
#[must_use]
pub fn features() -> &'static [FeatureDesc] {
    &FEATURES
}

/// Whether `id` names a feature in the table. The `[native]` loader uses
/// this to reject an unknown key loudly instead of silently ignoring a
/// typo: `pickr = false` must not read as "picker stays on".
#[must_use]
pub fn is_feature(id: &str) -> bool {
    FEATURES.iter().any(|f| f.id == id)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn ids_are_unique() {
        for (i, f) in features().iter().enumerate() {
            let dupes = features()
                .iter()
                .skip(i + 1)
                .filter(|o| o.id == f.id)
                .count();
            assert_eq!(dupes, 0, "duplicate feature id {}", f.id);
        }
    }

    #[test]
    fn off_switch_spells_the_id_a_user_can_paste() {
        for f in features() {
            assert_eq!(
                f.off_switch,
                format!("native.{} = false", f.id),
                "off_switch for {} must be the literal config line",
                f.id
            );
        }
    }

    #[test]
    fn is_feature_answers_only_for_table_rows() {
        assert!(is_feature("picker"));
        assert!(!is_feature("pickr"));
        assert!(!is_feature(""));
    }
}
