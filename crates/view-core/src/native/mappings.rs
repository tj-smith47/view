//! The compile-time table of default keys that reach a native feature.
//!
//! Pure data, like [`super::registry`]: nothing here does I/O or
//! serialization. The crate that reads `view.toml` decides which of these
//! specs a session registers, and the crate that speaks RPC turns each one
//! into a real nvim mapping so user remaps, which-key, and plugin
//! introspection all see view's keys the way they see any other plugin's.

/// The ex-command every feature is reachable through, registered regardless
/// of any feature's enabled bit so a user who turned the default keys off
/// still has a path in. Rendered into the command a mapping's help text
/// names and into the docs table, from here, once.
pub const COMMAND: &str = "View";

/// One default key view registers for a feature, and the entry point that
/// key invokes.
// Constructible by design (no `#[non_exhaustive]`): this is the payload
// callers assemble for `RpcCall::RegisterMappings`, and a test that cannot
// build a spec cannot exercise the registration path with one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingSpec {
    /// The [`registry`](super::registry) feature id this key reaches. The
    /// same id the `[native]` table keys on, so the key a user loses and the
    /// switch that gives it back are spelled identically.
    pub feature: &'static str,
    /// The key in nvim notation, e.g. `<leader>ff`. `<leader>` is left
    /// unresolved for nvim to expand at registration time, which is why
    /// registration waits for `VimEnter`: before it, `mapleader` is still
    /// nvim's default rather than the user's.
    pub lhs: &'static str,
    /// Which of the feature's entry points the key invokes, e.g. `files`.
    /// Also the second word of the ex-command form (`:View picker files`).
    pub verb: &'static str,
}

/// A default key that view set over an existing user mapping: what was
/// claimed, and (through its feature id) the off switch that gives it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingClaim {
    /// The feature id whose default key this is.
    pub feature: String,
    /// The key in nvim notation, as registered.
    pub lhs: String,
    /// Whether the key was already mapped by the user's config when view
    /// registered over it. `false` for a key that landed on nothing, which
    /// is not news and is never announced.
    pub had_user_mapping: bool,
}

// Telescope's and neo-tree's default keys deliberately, spelled the way a
// switching user already has them in muscle memory; a claim over a user's
// own `<leader>f` or `<leader>e` prefix is reported rather than avoided by
// picking keys nobody uses.
static DEFAULT_MAPS: [MappingSpec; 5] = [
    MappingSpec {
        feature: "picker",
        lhs: "<leader>ff",
        verb: "files",
    },
    MappingSpec {
        feature: "picker",
        lhs: "<leader>fb",
        verb: "buffers",
    },
    MappingSpec {
        feature: "picker",
        lhs: "<leader>fg",
        verb: "grep",
    },
    MappingSpec {
        feature: "tree",
        lhs: "<leader>e",
        verb: "toggle",
    },
    MappingSpec {
        feature: "notifications",
        lhs: "<leader>fm",
        verb: "history",
    },
];

/// Every default key this build ships, in registration order.
#[must_use]
pub fn default_maps() -> &'static [MappingSpec] {
    &DEFAULT_MAPS
}

/// Whether `spec`'s tokens can be spelled verbatim inside the mapping the
/// registration chunk generates for it.
///
/// The generated right-hand side is a readable
/// `<Cmd>call rpcnotify(.., 'feature', 'verb')<CR>` string rather than an
/// opaque callback, so a token carrying a quote, a backslash, a bar, or a
/// newline would close that command early and leave the rest of the token
/// running as a command of its own. Stated here, next to the table, and
/// applied again where a caller's specs actually reach the interpolation:
/// [`default_maps`] is static data a test in this module vets, and the
/// engine accepts any spec a caller builds.
#[must_use]
pub fn is_spellable(spec: &MappingSpec) -> bool {
    is_token(spec.feature) && is_token(spec.verb) && !spec.lhs.contains(['"', '\\', '\n', '\''])
}

/// Whether `s` is a bare lowercase word: the shape a feature id and a verb
/// share, and the shape that needs no escaping anywhere view spells one.
fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Every `feature verb` form `:View` answers, in registration order.
///
/// Read from [`default_maps`] like the docs table and the command's own
/// completion, so a build cannot offer a form in one place and refuse it in
/// another.
#[must_use]
pub fn invocations() -> Vec<String> {
    default_maps()
        .iter()
        .map(|spec| format!("{} {}", spec.feature, spec.verb))
        .collect()
}

/// The one-line usage for `:View`, naming the command's shape and every
/// form it accepts.
#[must_use]
pub fn render_usage() -> String {
    format!(
        ":{COMMAND} needs a feature and a verb -- try: {}",
        invocations().join(", ")
    )
}

/// The default map set as a markdown table, so the docs and the registered
/// keys cannot disagree: both read [`default_maps`].
#[must_use]
pub fn render_table() -> String {
    let mut out = String::from("| key | feature | command |\n| --- | --- | --- |\n");
    for spec in default_maps() {
        out.push_str(&format!(
            "| `{}` | `{}` | `:{} {} {}` |\n",
            spec.lhs, spec.feature, COMMAND, spec.feature, spec.verb
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::native::registry;

    #[test]
    fn every_spec_names_a_registry_feature() {
        for spec in default_maps() {
            assert!(
                registry::is_feature(spec.feature),
                "{} maps {} to no feature in the registry",
                spec.lhs,
                spec.feature
            );
        }
    }

    #[test]
    fn every_feature_that_declares_entry_keys_has_at_least_one() {
        for feature in registry::features() {
            let specs = default_maps().iter().filter(|s| s.feature == feature.id);
            assert_eq!(
                specs.count() > 0,
                feature.entry_keys,
                "{}'s entry_keys bit and its default_maps() rows disagree",
                feature.id
            );
        }
    }

    #[test]
    fn one_key_invokes_one_feature() {
        for (i, spec) in default_maps().iter().enumerate() {
            for other in default_maps().iter().skip(i + 1) {
                assert_ne!(spec.lhs, other.lhs, "{} is registered twice", spec.lhs);
            }
        }
    }

    // The chunk that registers these keys passes feature and verb as Lua
    // string literals inside one constant chunk, so a token carrying a
    // quote, a backslash, or a newline would break out of it. Static data
    // makes that a table review rather than an escaping problem, and this is
    // the review.
    #[test]
    fn tokens_are_safe_to_spell_in_a_lua_string() {
        for spec in default_maps() {
            assert!(
                is_spellable(spec),
                "{} carries a token the registration chunk cannot spell",
                spec.lhs
            );
        }
    }

    #[test]
    fn a_token_that_could_close_the_generated_command_is_not_spellable() {
        let hostile = [
            MappingSpec {
                feature: "picker",
                lhs: "<leader>ff",
                verb: "files', 'x')|call system('id",
            },
            MappingSpec {
                feature: "pick'er",
                lhs: "<leader>ff",
                verb: "files",
            },
            MappingSpec {
                feature: "picker",
                lhs: "<leader>'ff",
                verb: "files",
            },
            MappingSpec {
                feature: "",
                lhs: "<leader>ff",
                verb: "files",
            },
        ];
        for spec in &hostile {
            assert!(
                !is_spellable(spec),
                "{spec:?} would break out of the generated mapping"
            );
        }
    }

    #[test]
    fn the_usage_line_offers_exactly_the_forms_this_build_answers() {
        let usage = render_usage();
        assert!(usage.contains(&format!(":{COMMAND}")), "{usage}");
        for spec in default_maps() {
            assert!(
                usage.contains(&format!("{} {}", spec.feature, spec.verb)),
                "{} {} is registered but the usage line omits it: {usage}",
                spec.feature,
                spec.verb
            );
        }
        assert_eq!(invocations().len(), default_maps().len());
    }

    #[test]
    fn rendered_table_carries_every_default_key() {
        let table = render_table();
        for spec in default_maps() {
            assert!(
                table.contains(spec.lhs),
                "{} is missing from the rendered table",
                spec.lhs
            );
            assert!(
                table.contains(&format!(":{} {} {}", COMMAND, spec.feature, spec.verb)),
                "{}'s command form is missing from the rendered table",
                spec.lhs
            );
        }
    }
}
