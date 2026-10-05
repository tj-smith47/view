//! The compile-time tables of what `:View` answers: the default keys that
//! reach a native feature, and the forms that no key reaches at all.
//!
//! Pure data, like [`super::registry`]: nothing here does I/O or
//! serialization. The crate that reads `view.toml` decides which of these
//! specs a session registers, and the crate that speaks RPC turns each one
//! into a real nvim mapping so user remaps, key browsers and plugin
//! introspection all see view's keys the way they see any other plugin's.

use std::borrow::Cow;

/// The ex-command every feature is reachable through, registered regardless
/// of any feature's enabled bit so a user who turned the default keys off
/// still has a path in. Rendered into the command a mapping's help text
/// names and into the docs table, from here, once.
pub const COMMAND: &str = "View";

/// What a mapping does when its key is pressed.
///
/// `Keys` is the desktop chords' shape (`native::chords`): the omarchy
/// table binds most of its rows straight to an existing nvim key (`<C-w>h`,
/// `1gt`), and setting that key with no remapping costs the chord nothing
/// beyond what typing those keys already costs. Every [`MappingSpec`] built
/// before the chords existed is `Invoke`, the bridge call every registered
/// key used to be the only shape of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rhs {
    /// `<Cmd>call rpcnotify(channel, 'view_invoke', feature, verb)<CR>`.
    #[default]
    Invoke,
    /// nvim keys, set with no remapping.
    Keys(&'static str),
}

/// One default key view registers for a feature, and the entry point that
/// key invokes.
///
/// Normal mode only, for every spec: there is no `mode` field because the
/// registration path has no mode vocabulary to carry -- it snapshots the
/// user's existing normal-mode maps and sets each key with
/// `vim.keymap.set('n', ...)`. Every entry point here is a "leave what you
/// are doing and pick something" action, which is a normal-mode gesture in
/// the editors a switching user comes from too. Registering the same key in
/// insert or visual mode would need a per-spec mode set, a mode-aware claim
/// report, and a matching off switch; that is a schema change, not a value
/// a caller may set today.
// Constructible by design (no `#[non_exhaustive]`): this is the payload
// callers assemble for `RpcCall::RegisterMappings`, and a test that cannot
// build a spec cannot exercise the registration path with one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingSpec {
    /// The [`registry`](super::registry) feature id this key reaches. The
    /// same id the `[native]` table keys on, so the key a user loses and the
    /// switch that gives it back are spelled identically.
    pub feature: &'static str,
    /// The key in nvim notation, e.g. `<leader>ff`. `<leader>` is left
    /// unresolved for nvim to expand at registration time, which is why
    /// registration waits for `VimEnter`: before it, `mapleader` is still
    /// nvim's default rather than the user's.
    ///
    /// Owned because a `[keys.desktop]` row supplies it for a chord: the
    /// resolved override (or the derived spelling under whichever modifier
    /// the session settled on) has nowhere `'static` to live the way a
    /// compiled-in [`DEFAULT_MAPS`] row does.
    pub lhs: Cow<'static, str>,
    /// Which of the feature's entry points the key invokes, e.g. `files`.
    /// Also the second word of the ex-command form (`:View picker files`).
    pub verb: &'static str,
    /// What the key does once registered.
    pub rhs: Rhs,
}

/// A default key that view set over an existing user mapping: what was
/// claimed, and (through its feature id) the off switch that gives it back.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MappingClaim {
    /// The feature id whose default key this is.
    pub feature: String,
    /// The key in nvim notation, as registered.
    pub lhs: String,
    /// Whether the key was already mapped by the user's config when view
    /// registered over it. `false` for a key that landed on nothing, which
    /// is not news and is never announced.
    pub had_user_mapping: bool,
    /// The keys nvim matches for a key that invokes view, with the leader
    /// resolved (`\ai`, `<Space>ai`), as `keytrans()` spells them. `None`
    /// for a chord that sends nvim keys of its own, which invokes nothing.
    pub keys: Option<String>,
    /// The verb the key invokes, as [`MappingSpec::verb`] spells it. Empty
    /// on a claim built without [`MappingClaim::with_verb`].
    pub verb: String,
    /// The user's own mapping the key was set over, when nvim could still
    /// answer for it at registration.
    pub displaced: Option<MappingOwner>,
}

impl MappingClaim {
    /// A claim of `lhs` for `feature` that invokes nothing until
    /// [`MappingClaim::with_keys`] names the keys nvim matches for it.
    #[must_use]
    pub fn new(feature: impl Into<String>, lhs: impl Into<String>, had_user_mapping: bool) -> Self {
        Self {
            feature: feature.into(),
            lhs: lhs.into(),
            had_user_mapping,
            keys: None,
            verb: String::new(),
            displaced: None,
        }
    }

    /// The claim with the keys nvim matches for it, as `keytrans()` spells
    /// them.
    #[must_use]
    pub fn with_keys(mut self, keys: Option<String>) -> Self {
        self.keys = keys;
        self
    }

    /// The claim naming the verb its key invokes.
    #[must_use]
    pub fn with_verb(mut self, verb: impl Into<String>) -> Self {
        self.verb = verb.into();
        self
    }

    /// The claim naming the user mapping its key was set over.
    #[must_use]
    pub fn with_displaced(mut self, displaced: Option<MappingOwner>) -> Self {
        self.displaced = displaced;
        self
    }
}

/// Whose a normal-mode mapping is, as a person reads it: the mapping's
/// `desc`, else its rhs, else `<Lua callback>`, and the script that
/// defined it when nvim reports one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MappingOwner {
    /// The mapping's description, its rhs, or `<Lua callback>`.
    pub label: String,
    /// The defining script, relative to the config directory when it sits
    /// under it.
    pub script: Option<String>,
    /// Whether the mapping is set on the current buffer alone, where it
    /// shadows a global mapping of the same keys.
    pub buffer: bool,
}

impl MappingOwner {
    /// An owner described by `label`, defined in `script`.
    #[must_use]
    pub fn new(label: impl Into<String>, script: Option<String>) -> Self {
        Self {
            label: label.into(),
            script,
            buffer: false,
        }
    }

    /// The owner of a mapping set on the current buffer alone, when
    /// `buffer` is set.
    #[must_use]
    pub fn with_buffer(self, buffer: bool) -> Self {
        Self { buffer, ..self }
    }

    /// `label`, followed by the script in parentheses when there is one.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.script {
            Some(script) => format!("{} ({script})", self.label),
            None => self.label.clone(),
        }
    }
}

// spelled to match the ecosystem's own default keys, the way a switching
// user already has them in muscle memory; a claim over a user's own
// `<leader>f` or `<leader>e` prefix is reported so it stays visible.
static DEFAULT_MAPS: [MappingSpec; 28] = [
    MappingSpec {
        feature: "picker",
        lhs: Cow::Borrowed("<leader>ff"),
        verb: "files",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "picker",
        lhs: Cow::Borrowed("<leader>fb"),
        verb: "buffers",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "picker",
        lhs: Cow::Borrowed("<leader>fg"),
        verb: "grep",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "tree",
        lhs: Cow::Borrowed("<leader>e"),
        verb: "toggle",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "notifications",
        lhs: Cow::Borrowed("<leader>fm"),
        verb: "history",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "notifications",
        lhs: Cow::Borrowed("<leader>fp"),
        verb: "pause",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "ai",
        lhs: Cow::Borrowed("<leader>ai"),
        verb: "toggle",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "palette",
        lhs: Cow::Borrowed("<leader><leader>"),
        verb: "open",
        rhs: Rhs::Invoke,
    },
    // routed through the claimed-mapping path (nvim's own multi-key
    // mapping tree), past the raw terminal-keystroke `KeyBindings`
    // intercept: that intercept can resolve at most a two-raw-keystroke
    // chord ([`split_keys`](super::keys)), and the design's own defaults
    // here are three (`<leader>`, `u`, `g`/`w`).
    MappingSpec {
        feature: "ui",
        lhs: Cow::Borrowed("<leader>ug"),
        verb: "gaps",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "ui",
        lhs: Cow::Borrowed("<leader>uw"),
        verb: "cycle_surfaces",
        rhs: Rhs::Invoke,
    },
    // the six window verbs and the nine `to_tabpage_<N>` rows are every
    // `Rhs::Invoke` desktop chord's own twin (`native::chords::DESKTOP_CHORDS`)
    // that names no row above: `full_width`'s twin is the raw `<C-w>|` and
    // `gaps`'s is the `ui`/`gaps` row already here, so neither needs one.
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>wn"),
        verb: "new",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>wz"),
        verb: "zoom",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>wf"),
        verb: "fit",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>ws"),
        verb: "flip",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>uf"),
        verb: "float",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<C-w>m"),
        verb: "resize_mode",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "notifications",
        lhs: Cow::Borrowed("<leader>fd"),
        verb: "dismiss",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>w1"),
        verb: "to_tabpage_1",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>w2"),
        verb: "to_tabpage_2",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>w3"),
        verb: "to_tabpage_3",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>w4"),
        verb: "to_tabpage_4",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>w5"),
        verb: "to_tabpage_5",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>w6"),
        verb: "to_tabpage_6",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>w7"),
        verb: "to_tabpage_7",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>w8"),
        verb: "to_tabpage_8",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed("<leader>w9"),
        verb: "to_tabpage_9",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "keys",
        lhs: Cow::Borrowed("<leader>fk"),
        verb: "log",
        rhs: Rhs::Invoke,
    },
    MappingSpec {
        feature: "dvr",
        lhs: Cow::Borrowed("<leader>fv"),
        verb: "scrub",
        rhs: Rhs::Invoke,
    },
];

/// Every default key this build ships, in registration order.
#[must_use]
pub fn default_maps() -> &'static [MappingSpec] {
    &DEFAULT_MAPS
}

/// One `:View feature verb` form that no default key reaches, so the
/// command line is the whole of its discoverability.
///
/// Not a [`MappingSpec`] with an empty `lhs`: such a spec is not
/// [`is_spellable`], and a table the registration path has to filter rows
/// out of is a table that will one day register a key nobody asked for.
/// The two facts a completion entry needs are the only two here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandForm {
    /// The feature id the form invokes, spelled the way the dispatch that
    /// answers it matches on.
    pub feature: &'static str,
    /// The entry point, i.e. the second word of `:View <feature> <verb>`.
    pub verb: &'static str,
}

/// The diff review's verbs. Its keys are buffer-local nvim mappings on the
/// file under review, installed and removed with the review itself, so they
/// can never be [`DEFAULT_MAPS`] rows -- and without these rows `:View`
/// would complete neither `review` nor a verb of it, leaving the command
/// form the design names as the always-available way out of a review
/// something a user would have to already know to type.
///
/// The feature deliberately has no registry row and no
/// [`REGISTRY_EXEMPT_FEATURES`] entry: both exist to report a key claim and
/// to carry the off switch that gives the key back, and a form that claims
/// no key has neither to answer for.
static COMMAND_ONLY_FORMS: [CommandForm; 18] = [
    CommandForm {
        feature: "ai",
        verb: "open",
    },
    CommandForm {
        feature: "ai",
        verb: "focus",
    },
    CommandForm {
        feature: "ai",
        verb: "close",
    },
    CommandForm {
        feature: "ai",
        verb: "dismiss",
    },
    CommandForm {
        feature: "ui",
        verb: "panes",
    },
    CommandForm {
        feature: "dvr",
        verb: "close",
    },
    CommandForm {
        feature: "dvr",
        verb: "export",
    },
    CommandForm {
        feature: "dvr",
        verb: "play",
    },
    CommandForm {
        feature: "keys",
        verb: "profile",
    },
    CommandForm {
        feature: "keys",
        verb: "focus",
    },
    CommandForm {
        feature: "review",
        verb: "accept",
    },
    CommandForm {
        feature: "review",
        verb: "accept_all",
    },
    CommandForm {
        feature: "review",
        verb: "reject",
    },
    CommandForm {
        feature: "review",
        verb: "reject_all",
    },
    CommandForm {
        feature: "review",
        verb: "rediff",
    },
    CommandForm {
        feature: "review",
        verb: "next",
    },
    CommandForm {
        feature: "review",
        verb: "prev",
    },
    CommandForm {
        feature: "review",
        verb: "leave",
    },
];

/// Every `:View` form this build answers that no default key reaches.
#[must_use]
pub fn command_only_forms() -> &'static [CommandForm] {
    &COMMAND_ONLY_FORMS
}

/// The third words of the `:View` forms that take one, as their dispatch
/// matches them.
static FORM_ARGUMENTS: [(&str, &str, &[&str]); 2] = [
    ("ui", "panes", &["tiles", "nvim", "auto"]),
    ("keys", "profile", &["desktop", "editor", "auto"]),
];

/// The words `:View <feature> <verb>` takes after the verb, empty for a
/// form that takes none.
#[must_use]
pub fn form_arguments(feature: &str, verb: &str) -> &'static [&'static str] {
    FORM_ARGUMENTS
        .iter()
        .find(|(f, v, _)| *f == feature && *v == verb)
        .map_or(&[], |(_, _, words)| words)
}

/// One key a review installs on the buffer it is drawn in, and the verb it
/// invokes.
///
/// Separate from [`MappingSpec`] because the two are registered by different
/// mechanisms with different lifetimes: a spec is a global, session-long
/// mapping the user can turn off in `[native]`, while these are buffer-local
/// and exist only for as long as the review does. The feature is not a field
/// -- every row here is the review's, and a table of one feature spelling it
/// per row is a table that can disagree with itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewKey {
    /// The key in nvim notation. `<leader>` is left for nvim to expand, as
    /// in [`MappingSpec::lhs`].
    pub lhs: &'static str,
    /// The verb `Msg::FeatureInvoke { feature: "review", .. }` carries when
    /// this key is pressed, spelled as [`COMMAND_ONLY_FORMS`] spells it.
    pub verb: &'static str,
    /// What pressing it does, as the docs pages say it. Carried in the table
    /// rather than written into the pages because the same sentence is on
    /// two of them, rendered from here, and two hand-written copies of one
    /// key's meaning are two copies that drift.
    pub does: &'static str,
}

/// The review's own keys, installed on the reviewed buffer while a review
/// is open and removed with it.
///
/// `<leader>h*` is the hunk prefix the common git-hunk plugins use and
/// `]c`/`[c` are vanilla nvim's own diff-mode change motions, so a
/// switching user already has both in muscle memory. Single letters are
/// deliberately refused: a reviewed buffer stays editable -- the whole
/// stale-hunk machinery presumes the user types in it -- so claiming
/// `a`/`x`/`q` there, even buffer-locally, would break the one contract
/// everything else here is built on.
///
/// `reject_all` reaches no key on purpose: it decides the whole proposal in
/// one keystroke with no undo of its own, and `:View review reject_all` is
/// the deliberate way to ask for that.
static REVIEW_KEYS: [ReviewKey; 7] = [
    ReviewKey {
        lhs: "<leader>ha",
        verb: "accept",
        does: "accept the hunk under the cursor",
    },
    ReviewKey {
        lhs: "<leader>hA",
        verb: "accept_all",
        does: "accept every hunk still fresh, as one write",
    },
    ReviewKey {
        lhs: "<leader>hx",
        verb: "reject",
        does: "reject the hunk under the cursor",
    },
    ReviewKey {
        lhs: "<leader>hR",
        verb: "rediff",
        does: "re-anchor a hunk your own edit moved under",
    },
    ReviewKey {
        lhs: "<leader>hq",
        verb: "leave",
        does: "leave the review, deciding nothing further",
    },
    ReviewKey {
        lhs: "]c",
        verb: "next",
        does: "the next hunk still awaiting a decision",
    },
    ReviewKey {
        lhs: "[c",
        verb: "prev",
        does: "the previous hunk still awaiting a decision",
    },
];

/// Every key a review installs on the buffer it is reviewing.
#[must_use]
pub fn review_keys() -> &'static [ReviewKey] {
    &REVIEW_KEYS
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
    let keys_ok = match spec.rhs {
        Rhs::Invoke => true,
        // the chunk spells `spec.keys` into the same generated command form
        // as `spec.verb`, so a chord's own key sequence needs the same
        // vetting a feature or verb token gets
        Rhs::Keys(keys) => lhs_is_spellable(keys),
    };
    is_token(spec.feature) && is_token(spec.verb) && lhs_is_spellable(spec.lhs.as_ref()) && keys_ok
}

/// Whether `lhs` is safe to hand nvim as a mapping's left-hand side: the
/// same restriction every compiled-in [`MappingSpec::lhs`] holds to, so a
/// `view.toml`-supplied override (`[keys] toggle_gaps`, `cycle_surfaces`)
/// can never carry a quote or a newline into the generated
/// `vim.keymap.set` call.
#[must_use]
pub fn lhs_is_spellable(lhs: &str) -> bool {
    !lhs.is_empty() && !lhs.contains(['"', '\\', '\n', '\''])
}

/// Whether `s` is a bare lowercase word: the shape a feature id and a verb
/// share, and the shape that needs no escaping anywhere view spells one.
#[must_use]
pub fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.starts_with(|c: char| c.is_ascii_lowercase())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Every `feature verb` form `:View` answers, the keyed ones in
/// registration order and the command-only ones after them.
///
/// Read from [`default_maps`] and [`command_only_forms`], the same two
/// tables the command's own completion is built from, so a build cannot
/// offer a form in one place and refuse it in another. The docs table is
/// the one surface that reads `default_maps` alone -- it is a table of
/// keys, and a form with no key has no row to occupy there.
#[must_use]
pub fn invocations() -> Vec<String> {
    default_maps()
        .iter()
        .map(|spec| format!("{} {}", spec.feature, spec.verb))
        .chain(
            command_only_forms()
                .iter()
                .map(|form| format!("{} {}", form.feature, form.verb)),
        )
        .collect()
}

/// The one-line usage for `:View`, naming the command's shape and every
/// form it accepts, with the words a form takes after its verb.
#[must_use]
pub fn render_usage() -> String {
    let forms: Vec<String> = invocations()
        .into_iter()
        .map(|form| {
            let (feature, verb) = form.split_once(' ').unwrap_or((&form, ""));
            match form_arguments(feature, verb) {
                [] => form.clone(),
                words => format!("{form} [{}]", words.join("|")),
            }
        })
        .collect();
    format!(
        ":{COMMAND} needs a feature and a verb. Try: {}",
        forms.join(", ")
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

/// The review's buffer-local keys as a markdown table, for the same reason
/// [`render_table`] exists: a key a page names and the build does not
/// install is a key a reader presses for nothing.
///
/// `reject_all` is absent because it reaches no key -- see [`REVIEW_KEYS`]
/// for why -- and a docs table of keys is not the place to advertise a form
/// that has none.
///
/// Test-only, and private with it: the pages carry the rendered table, and
/// the test below is the only thing that needs to render it again.
#[cfg(test)]
fn render_review_table() -> String {
    let mut out = String::from("| key | does | command |\n| --- | --- | --- |\n");
    for key in review_keys() {
        out.push_str(&format!(
            "| `{}` | {} | `:{COMMAND} review {}` |\n",
            key.lhs, key.does, key.verb
        ));
    }
    out
}

/// The message-history overlay's keys as a markdown table, for the same
/// reason [`render_review_table`] exists. These keys never reach nvim --
/// the overlay answers them itself -- so neither the default-key table nor
/// the review table ranges over them, which is why they went undocumented.
///
/// Test-only, and private with it: the page carries the rendered table, and
/// the test below is the only thing that needs to render it again.
#[cfg(test)]
fn render_history_table() -> String {
    render_overlay_table(crate::update::surfaces::HISTORY_KEYS)
}

/// The key log's own keys as a markdown table, pinned to the page for the
/// reason [`render_history_table`] is.
#[cfg(test)]
fn render_key_log_table() -> String {
    render_overlay_table(crate::update::key_log::KEY_LOG_KEYS)
}

/// The scrub's keys as a markdown table, pinned to the page for the reason
/// [`render_history_table`] is.
#[cfg(test)]
fn render_dvr_keys() -> String {
    render_overlay_table(&crate::update::dvr::DVR_KEYS)
}

/// A table of keys an overlay answers itself, one `(key, does)` row each.
#[cfg(test)]
fn render_overlay_table(keys: &[(&str, &str)]) -> String {
    let mut out = String::from("| key | does |\n| --- | --- |\n");
    for (key, does) in keys {
        out.push_str(&format!("| `{key}` | {does} |\n"));
    }
    out
}

/// Display metadata for a feature in [`REGISTRY_EXEMPT_FEATURES`] -- the same
/// facts [`registry::FeatureDesc`] carries for a feature the registry
/// tracks. A claim on an exempt feature's key needs exactly these to report
/// through the same mechanism a registry feature's claim does, rather than
/// being dropped for lacking a `FeatureDesc` row (see
/// `view_native::report::report`, which now checks this table as a fallback).
///
/// Deliberately NOT `#[non_exhaustive]`: the only place this type is ever
/// built is its own static table, [`REGISTRY_EXEMPT_FEATURES`], right below
/// -- there is no caller outside this module for a hidden field to protect
/// against, and closing it off would just make that one table literal
/// unbuildable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExemptFeatureDesc {
    /// Stable id, spelled identically to the id [`MappingSpec::feature`]
    /// carries for this feature.
    pub id: &'static str,
    /// The exact config line that turns this feature off, verbatim in a
    /// claim's notice, the same contract [`registry::FeatureDesc::off_switch`]
    /// holds.
    pub off_switch: &'static str,
}

/// Features that reach a key in [`DEFAULT_MAPS`] without a
/// [`registry::FeatureDesc`] row -- see [`is_reachable_feature`]'s doc on why
/// a feature lands here.
static REGISTRY_EXEMPT_FEATURES: [ExemptFeatureDesc; 5] = [
    ExemptFeatureDesc {
        id: "ai",
        off_switch: "ai.enabled = false",
    },
    // the recording is switched by its own `[dvr]` table, so `[native]`
    // carries no switch for it
    ExemptFeatureDesc {
        id: "dvr",
        off_switch: "dvr.enabled = false",
    },
    // the key log has no on/off switch: it opens only when asked, so its
    // key can only be rebound, as `ui`'s can
    ExemptFeatureDesc {
        id: "keys",
        off_switch: "keys.key_log in view.toml",
    },
    // `ui` has no on/off switch of its own (see the `Msg::FeatureInvoke`
    // "ui" arm) -- its two `DEFAULT_MAPS` keys can only be rebound, so the
    // line a claim notice offers is the `[keys]` rebind.
    ExemptFeatureDesc {
        id: "ui",
        off_switch: "keys.toggle_gaps / keys.cycle_surfaces in view.toml",
    },
    // `window` carries the whole desktop chord table (`native::chords`)
    // plus its own tile-management `DEFAULT_MAPS` rows, and none of it is a
    // `[native]`-gated surface -- the off switch is the profile.
    ExemptFeatureDesc {
        id: "window",
        off_switch: "keys.profile = \"editor\"",
    },
];

/// Whether `feature` is reachable from somewhere a reviewer, and a
/// `[native]` config loader deciding what to register, can both find it:
/// the registry itself, or [`REGISTRY_EXEMPT_FEATURES`] for a feature that
/// deliberately has no registry row.
///
/// `ai` has no `[native]` entry: its enabled state lives in its own `[ai]`
/// table, not in the registry every other feature shares, so `[native]` can
/// never carry a switch that turns its key off -- a loader that gated
/// registration on `registry::is_feature` alone would read that absence as
/// "disabled" and drop the key from nvim registration, even though
/// completion, usage, and the docs table (all read from [`default_maps`]
/// directly) would still advertise it. A feature lands in this list in the
/// same commit that gives it a mapping row, never a step ahead of it.
#[must_use]
pub fn is_reachable_feature(feature: &str) -> bool {
    crate::native::registry::is_feature(feature) || exempt_feature(feature).is_some()
}

/// The exemption metadata for `feature`, when it is one of
/// [`REGISTRY_EXEMPT_FEATURES`] -- what a claim report needs to announce a
/// takeover the registry itself has no row for.
#[must_use]
pub fn exempt_feature(feature: &str) -> Option<&'static ExemptFeatureDesc> {
    REGISTRY_EXEMPT_FEATURES.iter().find(|f| f.id == feature)
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
                is_reachable_feature(spec.feature),
                "{} maps {} to no feature in the registry or REGISTRY_EXEMPT_FEATURES",
                spec.lhs,
                spec.feature
            );
        }
    }

    #[test]
    fn a_feature_in_neither_the_registry_nor_the_exemption_list_fails_the_check() {
        assert!(!is_reachable_feature("nonexistent-feature"));
        assert!(exempt_feature("nonexistent-feature").is_none());
    }

    #[test]
    fn the_ai_exemption_carries_its_own_off_switch() {
        let exempt = exempt_feature("ai").expect("ai must be a registered exemption");
        assert_eq!(exempt.off_switch, "ai.enabled = false");
    }

    #[test]
    fn the_agent_panel_key_is_discoverable_without_a_registry_entry() {
        assert!(
            default_maps()
                .iter()
                .any(|s| s.feature == "ai" && s.verb == "toggle"),
            "ai toggle is missing from DEFAULT_MAPS"
        );
        assert!(
            !registry::is_feature("ai"),
            "ai is deliberately absent from the native feature registry"
        );
        let usage = render_usage();
        assert!(usage.contains("ai toggle"), "{usage}");
        let table = render_table();
        assert!(table.contains(&format!(":{COMMAND} ai toggle")), "{table}");
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
                lhs: Cow::Borrowed("<leader>ff"),
                verb: "files', 'x')|call system('id",
                rhs: Rhs::Invoke,
            },
            MappingSpec {
                feature: "pick'er",
                lhs: Cow::Borrowed("<leader>ff"),
                verb: "files",
                rhs: Rhs::Invoke,
            },
            MappingSpec {
                feature: "picker",
                lhs: Cow::Borrowed("<leader>'ff"),
                verb: "files",
                rhs: Rhs::Invoke,
            },
            MappingSpec {
                feature: "",
                lhs: Cow::Borrowed("<leader>ff"),
                verb: "files",
                rhs: Rhs::Invoke,
            },
            MappingSpec {
                feature: "window",
                lhs: Cow::Borrowed("<D-h>"),
                verb: "focus_left",
                rhs: Rhs::Keys("<C-w>h'"),
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
        assert_eq!(
            invocations().len(),
            default_maps().len() + command_only_forms().len()
        );
    }

    /// The review is driven from the command line alone -- its keys live
    /// on the reviewed buffer, not in this table -- so the usage line is
    /// the only place a user who has not read the docs can learn the verbs
    /// exist at all.
    #[test]
    fn the_usage_line_offers_the_verbs_no_default_key_reaches() {
        let usage = render_usage();
        for form in command_only_forms() {
            assert!(
                usage.contains(&format!("{} {}", form.feature, form.verb)),
                "{} {} is answered but the usage line omits it: {usage}",
                form.feature,
                form.verb
            );
        }
        assert!(
            command_only_forms().iter().any(|f| f.feature == "review"),
            "the review's verbs are the reason this table exists"
        );
    }

    /// A form with no key is not a row in a table of keys: putting one
    /// there would document a key the user does not have.
    #[test]
    fn a_form_with_no_key_stays_out_of_the_rendered_key_table() {
        let table = render_table();
        for form in command_only_forms() {
            assert!(
                !table.contains(&format!("{} {}", form.feature, form.verb)),
                "{} {} has no key to document: {table}",
                form.feature,
                form.verb
            );
        }
    }

    /// The completion offers these words and the dispatch matches on them,
    /// so a token neither side can spell verbatim is a form nothing
    /// answers.
    #[test]
    fn a_command_only_form_is_spelled_the_way_a_feature_and_a_verb_are() {
        for form in command_only_forms() {
            assert!(is_token(form.feature), "{form:?}");
            assert!(is_token(form.verb), "{form:?}");
        }
    }

    /// The buffer-local keys are set from these rows inside the same kind
    /// of generated `rpcnotify` right-hand side the global registration
    /// builds, and the verb they carry is matched by the review's dispatch:
    /// a row naming a verb nothing answers is a key that does nothing.
    #[test]
    fn every_review_key_names_a_verb_the_command_also_offers() {
        for key in review_keys() {
            assert!(
                command_only_forms()
                    .iter()
                    .any(|form| form.feature == "review" && form.verb == key.verb),
                "{} invokes review {}, which no form offers",
                key.lhs,
                key.verb
            );
            assert!(is_token(key.verb), "{key:?}");
            assert!(
                !key.lhs.contains(['"', '\\', '\n', '\'']),
                "{} carries a character the chunk that sets it cannot spell",
                key.lhs
            );
        }
    }

    /// One key, one verb: a review that set the same key twice would leave
    /// whichever row came last as the only one that answers.
    #[test]
    fn no_review_key_is_claimed_twice() {
        for (i, key) in review_keys().iter().enumerate() {
            for other in review_keys().iter().skip(i + 1) {
                assert_ne!(key.lhs, other.lhs, "{} is set twice", key.lhs);
                assert_ne!(key.verb, other.verb, "{} is reached twice", key.verb);
            }
        }
    }

    /// Two default rows on one key leave whichever registers last as the
    /// only one that answers, and the other feature unreachable by key.
    #[test]
    fn no_two_default_keys_share_a_left_hand_side() {
        let maps = default_maps();
        for (i, spec) in maps.iter().enumerate() {
            for other in maps.iter().skip(i + 1) {
                assert_ne!(
                    spec.lhs, other.lhs,
                    "{} is the default key of both {} {} and {} {}",
                    spec.lhs, spec.feature, spec.verb, other.feature, other.verb
                );
            }
        }
    }

    /// A review's keys are buffer-local and live only as long as the
    /// review, so none of them may also be a session-long default: the
    /// review would silently take the user's `<leader>ff` away for the
    /// length of a decision and hand it back afterwards.
    #[test]
    fn no_review_key_shadows_a_default_key() {
        for key in review_keys() {
            assert!(
                !default_maps()
                    .iter()
                    .any(|spec| spec.lhs.as_ref() == key.lhs),
                "{} is both a default key and a review key",
                key.lhs
            );
        }
    }

    /// Both pages a user learns the review from carry the same generated
    /// table, so a key that changed shape here fails the build rather than
    /// leaving one page right and the other one lying.
    ///
    /// The default-key table on `docs/keymaps.md` is pinned from view-native
    /// instead (`the_keys_page_renders_the_table_this_build_registers`),
    /// where the registration it renders lives: one page, two tables, each
    /// checked beside the thing that generates it.
    #[test]
    fn both_docs_pages_render_the_review_keys_this_build_installs() {
        let table = render_review_table();
        for page in ["../../docs/keymaps.md", "../../docs/ai.md"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(page);
            let text = std::fs::read_to_string(&path).expect("the docs page must be readable");
            assert!(
                text.contains(&table),
                "{page} is stale, it must carry:\n{table}"
            );
        }
    }

    /// The third table on that page, pinned the same way and here for the
    /// same reason: the overlay's keys reach a user only through the page,
    /// since nothing registers them with nvim.
    #[test]
    fn the_keys_page_renders_the_history_overlay_keys_this_build_answers() {
        let table = render_history_table();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/keymaps.md");
        let text = std::fs::read_to_string(&path).expect("docs/keymaps.md must be readable");
        assert!(
            text.contains(&table),
            "docs/keymaps.md is stale, it must carry:\n{table}"
        );
    }

    /// The key log's table, pinned the same way as the history overlay's.
    #[test]
    fn the_keys_page_renders_the_key_log_keys_this_build_answers() {
        let table = render_key_log_table();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/keymaps.md");
        let text = std::fs::read_to_string(&path).expect("docs/keymaps.md must be readable");
        assert!(
            text.contains(&table),
            "docs/keymaps.md is stale, it must carry:\n{table}"
        );
    }

    /// The scrub's table, pinned the same way as the history overlay's.
    #[test]
    fn dvr_keys_table_matches_the_docs() {
        let table = render_dvr_keys();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/keymaps.md");
        let text = std::fs::read_to_string(&path).expect("docs/keymaps.md must be readable");
        assert!(
            text.contains(&table),
            "docs/keymaps.md is stale, it must carry:\n{table}"
        );
    }

    #[test]
    fn rendered_table_carries_every_default_key() {
        let table = render_table();
        for spec in default_maps() {
            assert!(
                table.contains(spec.lhs.as_ref()),
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
