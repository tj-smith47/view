//! The precedence chain spec section 11 states, answered in one place: a
//! command-line flag beats an environment variable, which beats the config
//! file, which beats the value view derives on its own.
//!
//! Every key answers through the same [`layer`] call, so no key can resolve
//! in a different order than its neighbour, and every answer carries the
//! layer it came from -- provenance is the half that makes a chain usable
//! for triage, since "the picker is off" and "your `view.toml` turns the
//! picker off" are different answers to the same question.

use std::path::PathBuf;

pub use view_core::config::Source;
use view_core::config::{
    BOOL_EXPECTED, COLOR_EXPECTED, KEYS_EXPECTED, PANES_EXPECTED, TIER_EXPECTED, WIDTH_EXPECTED,
};
use view_core::model::{Panes, Tier};
use view_core::native::geometry;
use view_core::native::keys::{Action, Direction, KeyBindings};
use view_core::native::registry;

use super::keys::{env_name, keys, ConfigKey};
use super::{
    parse_color, parse_nvim_bin, parse_panes, parse_theme, parse_tier, KeysConfig, NativeConfig,
    SupervisionConfig, UiTokens, ViewConfig, AUTO, BUNDLED,
};

/// One resolved answer and the reason it is that answer.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<T> {
    /// What the key resolved to.
    pub value: T,
    /// Which layer supplied it.
    pub source: Source,
}

/// Which rendering tier a user asked for. There is no `Auto` variant:
/// "auto" is the absence of a choice, carried as `None`, so this layer
/// never has to name a value it cannot compute -- only a caller holding a
/// terminal can probe one.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierChoice {
    /// Every capability this build draws with.
    Full,
    /// The middle tier.
    Standard,
    /// The floor every terminal can render.
    Basic,
}

impl TierChoice {
    /// The word a user writes for this tier, and the word a report prints.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Standard => "standard",
            Self::Basic => "basic",
        }
    }
}

impl From<TierChoice> for Tier {
    fn from(choice: TierChoice) -> Self {
        match choice {
            TierChoice::Full => Self::Full,
            TierChoice::Standard => Self::Standard,
            TierChoice::Basic => Self::Basic,
        }
    }
}

/// Every value a flag may override, taken from the parsed command line.
///
/// The keys decision 6a names, and no more: `[native]`'s switches are
/// reached through `--clean` as a whole table, so a `None` flag on a
/// registry row is a stated state rather than a gap.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// `--tier`.
    pub tier: Option<TierChoice>,
    /// `--theme`.
    pub theme: Option<String>,
    /// `--nvim-bin`.
    pub nvim_bin: Option<PathBuf>,
    /// `--appname`.
    pub appname: Option<String>,
    /// `--single-grid`.
    pub single_grid: Option<bool>,
    /// `--panes`. Doubly optional the way the file layer is: `Some(None)`
    /// is the word `auto`.
    pub panes: Option<Option<Panes>>,
}

/// The `[ui]` table's resolved answers.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUi {
    /// Which tier to render at, or `None` for the terminal's own answer.
    pub tier: Resolved<Option<TierChoice>>,
    /// Which colorscheme to ask nvim for, or `None` to derive the chrome
    /// from whatever the user's own config ended on.
    pub theme: Resolved<Option<String>>,
    /// How window layout is drawn.
    pub panes: Resolved<Panes>,
    /// The marker that decided `panes` under `"auto"`, for the report row.
    pub panes_marker: Option<&'static str>,
    /// What `"auto"` answered on this environment, whatever layer won.
    /// `:View ui panes auto` switches back to it mid-session.
    pub detected_panes: Panes,
    /// Whether a gap separates neighbouring frames under tiles.
    pub gaps: Resolved<bool>,
    /// The colours the user named for themselves.
    pub tokens: Resolved<UiTokens>,
}

/// The `[engine]` table's resolved answers.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEngine {
    /// The editor to spawn, or `None` for the bundled layout beside this
    /// executable -- which only a caller that can look beside itself can
    /// resolve to a path.
    pub nvim_bin: Resolved<Option<PathBuf>>,
    /// The `NVIM_APPNAME` to set in the child, or `None` to inherit
    /// whatever the process already carries.
    pub appname: Resolved<Option<String>>,
    /// Whether to attach without `ext_multigrid`.
    pub single_grid: Resolved<bool>,
}

/// Every key this crate resolves, each with the reason it is that answer.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// The `[ui]` answers.
    pub ui: ResolvedUi,
    /// The `[engine]` answers.
    pub engine: ResolvedEngine,
    /// The tables this crate parses, with every layer applied: what a
    /// session attaches, binds keys and supervises from. Whole configs
    /// rather than a value per key, so a consumer that already takes a
    /// [`ViewConfig`] keeps taking one.
    pub tables: ViewConfig,
    /// One line per environment value view could not read, in the order the
    /// registry lists the keys they were set for. The file layer already
    /// owes a user a notice for a value it had to discard, and an
    /// environment that silently stopped applying is the same mistake with
    /// less to look at.
    notices: Vec<String>,
    /// Where each feature's switch came from, in `registry::features()`
    /// order.
    native: Vec<Source>,
    /// Where `[native] tree_width` came from.
    tree_width: Source,
    /// Where each `[keys]` action's bindings came from, in [`KEY_ACTIONS`]
    /// order.
    keys: [Source; KEY_ACTIONS.len()],
    /// Where `[supervision] auto_restart` came from.
    supervision: Source,
    /// The `NVIM_APPNAME` this process already carries, which is what an
    /// unset `[engine] appname` resolves to in the child. Captured here
    /// rather than read at render time so a report is a statement about the
    /// session it describes.
    inherited_appname: Option<String>,
}

/// The profile nvim runs under when nothing names one, and therefore what
/// an inherited-and-unset `[engine] appname` resolves to.
const DEFAULT_APPNAME: &str = "nvim";

/// The one name outside the `VIEW_*` namespace this resolver reads, and it
/// reads it to *report* rather than to decide: an absent `[engine] appname`
/// means the child inherits, and a report that printed nothing there would
/// leave a user to go find out what it inherited.
const INHERITED_APPNAME_ENV: &str = "NVIM_APPNAME";

/// The `[keys]` actions, each beside the key that names it, in the order
/// the registry lists them.
const KEY_ACTIONS: [(&str, Action); 3] = [
    ("sidebar_wider", Action::Resize(Direction::Wider)),
    ("sidebar_narrower", Action::Resize(Direction::Narrower)),
    ("composer_newline", Action::ComposerNewline),
];

/// Resolves every key against the process environment.
#[must_use]
pub fn resolve(file: &ViewConfig, flags: &Overrides) -> ResolvedConfig {
    resolve_with(file, flags, &|name| std::env::var(name).ok())
}

/// Resolves every key against a caller-supplied environment, so the
/// precedence chain is testable without mutating the process's own -- and
/// so a caller that must suppress the environment layer altogether
/// (`--clean`, whose whole question is "view or your config") passes one
/// that answers nothing.
#[must_use]
pub fn resolve_with(
    file: &ViewConfig,
    flags: &Overrides,
    env: &dyn Fn(&str) -> Option<String>,
) -> ResolvedConfig {
    let mut notices = Vec::new();
    let engine = ResolvedEngine {
        nvim_bin: layer(
            flags.nvim_bin.clone().map(Some),
            env_read(
                env,
                "engine",
                "nvim_bin",
                "",
                always(parse_nvim_bin),
                &mut notices,
            ),
            file.spells("engine", "nvim_bin")
                .then(|| file.engine.nvim_bin.clone()),
            None,
        ),
        appname: layer(
            flags.appname.clone().map(Some),
            env_read(
                env,
                "engine",
                "appname",
                "",
                always(|value| Some(value.to_string())),
                &mut notices,
            ),
            file.spells("engine", "appname")
                .then(|| file.engine.appname.clone()),
            None,
        ),
        single_grid: layer(
            flags.single_grid,
            env_read(
                env,
                "engine",
                "single_grid",
                BOOL_EXPECTED,
                parse_bool,
                &mut notices,
            ),
            file.engine.single_grid,
            false,
        ),
    };
    // resolved after `[engine]` because `single_grid` overrules it: without
    // multigrid nvim announces no window placements at all, so there is
    // nothing for a tile to be drawn around
    let (derived_panes, derived_marker) = detect_panes(env);
    let asked_panes = layer(
        flags.panes,
        env_read(
            env,
            "ui",
            "panes",
            PANES_EXPECTED,
            parse_panes,
            &mut notices,
        ),
        file.ui.panes,
        None,
    );
    let forced = engine.single_grid.value;
    let single_grid_answer = (
        Resolved {
            value: Panes::Nvim,
            source: Source::Derived,
        },
        Some(SINGLE_GRID_MARKER),
    );
    let (panes, panes_marker) = match asked_panes.value {
        // a mode the user asked for and did not get owes a notice; one they
        // asked for and got keeps its own layer, single grid or not
        Some(value) if forced && value != Panes::Nvim => {
            notices.push(SINGLE_GRID_NOTICE.to_string());
            single_grid_answer
        }
        Some(value) => (
            Resolved {
                value,
                source: asked_panes.source,
            },
            None,
        ),
        None if forced => single_grid_answer,
        None => (
            Resolved {
                value: derived_panes,
                source: Source::Derived,
            },
            derived_marker,
        ),
    };
    let ui = ResolvedUi {
        // the file's answers arrive already doubly optional (see `UiFile`):
        // the outer `None` is "this layer named nothing", which is exactly
        // what `layer` falls through on, so a mistyped tier and an absent
        // one reach the derived answer by the same route
        tier: layer(
            flags.tier.map(Some),
            env_read(env, "ui", "tier", TIER_EXPECTED, parse_tier, &mut notices),
            file.ui.tier,
            None,
        ),
        theme: layer(
            flags.theme.as_deref().map(parse_theme),
            env_read(env, "ui", "theme", "", always(parse_theme), &mut notices),
            file.ui.theme.clone(),
            None,
        ),
        panes,
        panes_marker,
        detected_panes: derived_panes,
        gaps: layer(
            None,
            env_read(env, "ui", "gaps", BOOL_EXPECTED, parse_bool, &mut notices),
            file.ui.gaps,
            true,
        ),
        tokens: layer(
            None,
            env_read(
                env,
                "ui.tokens",
                "accent",
                COLOR_EXPECTED,
                parse_color,
                &mut notices,
            )
            .map(|accent| UiTokens { accent }),
            file.ui.tokens.clone(),
            UiTokens::default(),
        ),
    };
    // the file layer's own notices, which an environment value above them
    // neither answers for nor silences: a mistyped tier is still a mistyped
    // tier, the same terms `[native] tree_width`'s notice is carried on
    notices.extend(file.ui.notices.iter().cloned());
    let mut disabled = Vec::new();
    let mut native = Vec::with_capacity(registry::features().len());
    for feature in registry::features() {
        let switch = layer(
            None,
            env_read(
                env,
                "native",
                feature.id,
                BOOL_EXPECTED,
                parse_bool,
                &mut notices,
            ),
            file.spells("native", feature.id)
                .then(|| !file.native.disabled.contains(&feature.id)),
            feature.default_on,
        );
        if !switch.value {
            disabled.push(feature.id);
        }
        native.push(switch.source);
    }
    let tree_width = layer(
        None,
        env_read(
            env,
            "native",
            "tree_width",
            WIDTH_EXPECTED,
            parse_width,
            &mut notices,
        ),
        file.spells("native", "tree_width")
            .then_some(file.native.tree_width),
        geometry::DEFAULT_PANEL_WIDTH_PCT,
    );
    let (bindings, key_sources) = resolve_keys(file, env, &mut notices);
    let auto_restart = layer(
        None,
        env_read(
            env,
            "supervision",
            "auto_restart",
            BOOL_EXPECTED,
            parse_bool,
            &mut notices,
        ),
        file.spells("supervision", "auto_restart")
            .then_some(file.supervision.auto_restart),
        SupervisionConfig::default().auto_restart,
    );
    ResolvedConfig {
        ui,
        engine,
        tables: ViewConfig {
            native: NativeConfig {
                disabled,
                tree_width: tree_width.value,
                // the file layer's own notice, which an environment value
                // above it neither answers for nor silences: a mistyped
                // `tree_width` is still a mistyped `tree_width`
                tree_width_notice: file.native.tree_width_notice,
            },
            supervision: SupervisionConfig {
                auto_restart: auto_restart.value,
            },
            keys: KeysConfig {
                bindings,
                notices: file.keys.notices().to_vec(),
            },
            engine: file.engine.clone(),
            ui: file.ui.clone(),
            spelled: file.spelled.clone(),
        },
        notices,
        native,
        tree_width: tree_width.source,
        keys: key_sources,
        supervision: auto_restart.source,
        inherited_appname: env(INHERITED_APPNAME_ENV).filter(|name| !name.is_empty()),
    }
}

/// The `[keys]` bindings every layer has had its say over, and where each
/// action's own answer came from.
///
/// The file's bindings are the base rather than a value the chain picks
/// between, because an action the environment names replaces only that
/// action: the same all-or-nothing-per-action rule the file layer already
/// resolves under, one layer up.
fn resolve_keys(
    file: &ViewConfig,
    env: &dyn Fn(&str) -> Option<String>,
    notices: &mut Vec<String>,
) -> (KeyBindings, [Source; KEY_ACTIONS.len()]) {
    let mut bindings = file.keys.bindings().clone();
    let mut sources = [Source::Derived; KEY_ACTIONS.len()];
    for (index, (key, action)) in KEY_ACTIONS.into_iter().enumerate() {
        if file.spells("keys", key) {
            sources[index] = Source::File;
        }
        let Some(spellings) = env_read(env, "keys", key, KEYS_EXPECTED, parse_keys, notices) else {
            continue;
        };
        if bindings.rebind(action, &spellings) {
            sources[index] = Source::Env;
        } else {
            notices.push(discarded("keys", key, &spellings.join(" "), KEYS_EXPECTED));
        }
    }
    (bindings, sources)
}

impl ResolvedConfig {
    /// Every key, its resolved value rendered for display, and where it
    /// came from, in registry order. The doctor's config section is this
    /// walk; nothing re-derives the list.
    ///
    /// Every registry row whose table is not `[ai]`, and no others: that
    /// table is parsed and resolved by the crate that owns it, and a caller
    /// that can name both crates appends its answers to these.
    #[must_use]
    pub fn rows(&self) -> Vec<(&'static ConfigKey, String, Source)> {
        keys()
            .iter()
            .filter_map(|key| self.answer(key).map(|(value, source)| (key, value, source)))
            .collect()
    }

    /// One line per environment value this session could not read. Empty
    /// whenever every `VIEW_*` in play said something view understood,
    /// which is the ordinary case.
    #[must_use]
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// One key's rendered value and layer, or `None` for a key another
    /// crate answers.
    fn answer(&self, key: &ConfigKey) -> Option<(String, Source)> {
        Some(match (key.table, key.key) {
            ("ui", "tier") => (
                self.ui
                    .tier
                    .value
                    .map_or(AUTO, TierChoice::label)
                    .to_string(),
                self.ui.tier.source,
            ),
            ("ui", "theme") => (
                self.ui.theme.value.clone().unwrap_or_else(|| AUTO.into()),
                self.ui.theme.source,
            ),
            ("ui", "panes") => (
                match self.ui.panes_marker {
                    // the marker is what makes a derived answer readable:
                    // "nvim" alone leaves a user to guess which of their
                    // environment said so
                    Some(marker) => format!("{} ({marker})", panes_label(self.ui.panes.value)),
                    None => panes_label(self.ui.panes.value).to_string(),
                },
                self.ui.panes.source,
            ),
            ("ui", "gaps") => (self.ui.gaps.value.to_string(), self.ui.gaps.source),
            ("ui.tokens", "accent") => (
                self.ui
                    .tokens
                    .value
                    .accent
                    .map_or_else(|| AUTO.to_string(), |rgb| format!("#{rgb:06x}")),
                self.ui.tokens.source,
            ),
            ("engine", "nvim_bin") => (
                self.engine
                    .nvim_bin
                    .value
                    .as_ref()
                    .map_or_else(|| BUNDLED.to_string(), |path| path.display().to_string()),
                self.engine.nvim_bin.source,
            ),
            ("engine", "appname") => (
                // an absent choice still runs the child under *some*
                // profile, and the profile it runs under is the answer a
                // report owes -- an empty cell would read as "no appname",
                // which is not a state nvim has
                self.engine
                    .appname
                    .value
                    .clone()
                    .or_else(|| self.inherited_appname.clone())
                    .unwrap_or_else(|| DEFAULT_APPNAME.to_string()),
                self.engine.appname.source,
            ),
            ("engine", "single_grid") => (
                self.engine.single_grid.value.to_string(),
                self.engine.single_grid.source,
            ),
            ("native", "tree_width") => {
                (self.tables.native.tree_width.to_string(), self.tree_width)
            }
            ("keys", name) => {
                let index = KEY_ACTIONS.iter().position(|(key, _)| *key == name)?;
                (
                    self.tables
                        .keys
                        .bindings()
                        .spellings(KEY_ACTIONS[index].1)
                        .join(", "),
                    self.keys[index],
                )
            }
            ("native", id) => (
                (!self.tables.native.disabled.contains(&id)).to_string(),
                *self
                    .native
                    .get(registry::features().iter().position(|f| f.id == id)?)?,
            ),
            ("supervision", "auto_restart") => (
                self.tables.supervision.auto_restart.to_string(),
                self.supervision,
            ),
            _ => return None,
        })
    }
}

/// The environment names a tiling window manager exports, read in this
/// order with the first one set deciding.
///
/// A compositor that lays windows out already gives a person the tiling
/// they want, and a second set of frames inside it is two window managers
/// disagreeing on screen.
const TILING_MARKERS: [&str; 3] = ["HYPRLAND_INSTANCE_SIGNATURE", "SWAYSOCK", "I3SOCK"];

/// The desktop name every other tiling window manager on the list exports,
/// matched against any colon-separated member of `XDG_CURRENT_DESKTOP`,
/// case ignored.
const TILING_DESKTOPS: [&str; 12] = [
    "hyprland",
    "sway",
    "i3",
    "river",
    "niri",
    "bspwm",
    "dwm",
    "awesome",
    "qtile",
    "xmonad",
    "herbstluftwm",
    "leftwm",
];

/// The one name outside [`TILING_MARKERS`] the detection reads.
const XDG_CURRENT_DESKTOP: &str = "XDG_CURRENT_DESKTOP";

/// What the report prints beside a look mode `[engine] single_grid` decided.
const SINGLE_GRID_MARKER: &str = "[engine] single_grid";

/// What a session that asked for tiles without multigrid owes the user.
const SINGLE_GRID_NOTICE: &str =
    "view: [engine] single_grid leaves nvim addressing one grid, so there are no window \
     placements to draw tiles from. [ui] panes answers nvim this run";

/// The look `"auto"` resolves to, with the marker that decided it.
///
/// The fallback is tiles, which is also what an ssh session gets: the
/// client's environment never reaches the server, so nothing a remote
/// session can read says what is drawing its terminal.
fn detect_panes(env: &dyn Fn(&str) -> Option<String>) -> (Panes, Option<&'static str>) {
    for marker in TILING_MARKERS {
        if env(marker).is_some_and(|value| !value.trim().is_empty()) {
            return (Panes::Nvim, Some(marker));
        }
    }
    let named = env(XDG_CURRENT_DESKTOP).is_some_and(|desktop| {
        desktop.split(':').any(|member| {
            let member = member.trim().to_ascii_lowercase();
            TILING_DESKTOPS.iter().any(|name| *name == member)
        })
    });
    if named {
        return (Panes::Nvim, Some(XDG_CURRENT_DESKTOP));
    }
    (Panes::Tiles, None)
}

/// The word a look mode is written as, in every layer that carries one.
#[must_use]
const fn panes_label(panes: Panes) -> &'static str {
    match panes {
        Panes::Tiles => "tiles",
        // every other arm is `Panes::Nvim`, and a mode added later reads as
        // the picture nvim paints for itself until this table names it
        _ => "nvim",
    }
}

/// The chain itself, written once so no key can answer in a different
/// order than its neighbour.
fn layer<T>(flag: Option<T>, env: Option<T>, file: Option<T>, derived: T) -> Resolved<T> {
    if let Some(value) = flag {
        return Resolved {
            value,
            source: Source::Flag,
        };
    }
    if let Some(value) = env {
        return Resolved {
            value,
            source: Source::Env,
        };
    }
    if let Some(value) = file {
        return Resolved {
            value,
            source: Source::File,
        };
    }
    Resolved {
        value: derived,
        source: Source::Derived,
    }
}

/// One key's environment value read through `parse`, and a notice for a
/// value `parse` would not have.
///
/// A value view cannot read never fails the session -- an environment is as
/// easy to mistype as a `tree_width`, and neither is a reason to refuse to
/// open a file -- but it never falls through in silence either: the layer
/// below answering while a user watches their own `VIEW_*` do nothing is
/// the shape a notice exists for.
fn env_read<T>(
    env: &dyn Fn(&str) -> Option<String>,
    table: &str,
    key: &str,
    expected: &str,
    parse: impl Fn(&str) -> Option<T>,
    notices: &mut Vec<String>,
) -> Option<T> {
    let raw = env_value(env, table, key)?;
    let parsed = parse(&raw);
    if parsed.is_none() {
        notices.push(discarded(table, key, &raw, expected));
    }
    parsed
}

/// One key's discarded-value notice, with the environment name taken from
/// the key's own registry row rather than restated.
fn discarded(table: &str, key: &str, value: &str, expected: &str) -> String {
    let name = keys()
        .iter()
        .find(|row| row.table == table && row.key == key)
        .map_or_else(String::new, env_name);
    view_core::config::discarded_env(&name, value, expected, table, key)
}

/// A parse that cannot fail, in the shape [`env_read`] takes: the value is
/// always read, and what it reads *to* may still be the absence of a choice.
fn always<T>(parse: impl Fn(&str) -> T) -> impl Fn(&str) -> Option<T> {
    move |value| Some(parse(value))
}

/// What the environment says about one key, or `None` when it says
/// nothing.
///
/// The name is generated from the key's own registry row and never read
/// from anywhere else, which is what keeps the rest of the `VIEW_*`
/// namespace -- `VIEW_LOG`, and the harness's own names -- out of config
/// entirely. An empty value is no value: `VIEW_UI_THEME=` names no
/// colorscheme, and reading it as one would be a layer a user cannot
/// unset.
fn env_value(env: &dyn Fn(&str) -> Option<String>, table: &str, key: &str) -> Option<String> {
    let row = keys()
        .iter()
        .find(|row| row.table == table && row.key == key)?;
    let value = env(&env_name(row))?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// A switch's value, or `None` for text that is not one.
///
/// A value this cannot read falls through to the layer below rather than
/// failing the session: an environment view cannot parse is the same kind
/// of mistake as a mistyped `tree_width`, and neither is a reason to
/// refuse to open a file.
fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// A sidebar width, clamped the way the file layer clamps one, or `None`
/// for text that is not a whole number -- which falls through to the layer
/// below, for the reason [`parse_bool`] states. A number outside the range
/// is not that case: it resolves at the nearest end, exactly as a
/// `view.toml` asking for 5 or 95 does.
fn parse_width(value: &str) -> Option<u16> {
    value.parse::<i64>().ok().map(geometry::clamp_panel_width)
}

/// The key notations a value names, split on whitespace -- the one
/// separator a key notation can never contain, which is what lets a list
/// live in a variable that holds no lists. Whether the notations name keys
/// this build can match is [`KeyBindings::rebind`]'s answer, not this one.
fn parse_keys(value: &str) -> Option<Vec<String>> {
    Some(value.split_whitespace().map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// An environment that answers nothing, which is what `--clean` hands
    /// the resolver.
    fn no_env(_: &str) -> Option<String> {
        None
    }

    /// Whether a row belongs to the table the sibling crate owns, nested
    /// sub-tables included.
    fn is_ai(row: &ConfigKey) -> bool {
        row.table == "ai" || row.table.starts_with("ai.")
    }

    /// An environment carrying a legal value for every key this crate
    /// resolves, so a test that must show a layer being *suppressed* has
    /// something to suppress.
    fn every_key_set(name: &str) -> Option<String> {
        keys()
            .iter()
            .find(|row| env_name(row) == name)
            .map(|row| env_fixture(row).to_string())
    }

    /// A legal environment value for one key, per its own type.
    fn env_fixture(row: &ConfigKey) -> &'static str {
        match (row.table, row.key) {
            ("ui", "tier") => "basic",
            ("ui", "theme") => "gruvbox",
            ("ui", "panes") => "nvim",
            ("ui.tokens", "accent") => "#89b4fa",
            ("engine", "nvim_bin") => "/opt/nvim/bin/nvim",
            ("engine", "appname") => "work",
            ("native", "tree_width") => "40",
            ("keys", _) => "<C-w>>",
            _ => "false",
        }
    }

    /// One row's rendered value and layer, by key path.
    fn row(resolved: &ResolvedConfig, table: &str, key: &str) -> (String, Source) {
        resolved
            .rows()
            .into_iter()
            .find(|(row, _, _)| row.table == table && row.key == key)
            .map(|(_, value, source)| (value, source))
            .unwrap_or_else(|| panic!("[{table}] {key} has no resolved row"))
    }

    /// The chain's four layers, proven across the two keys that between
    /// them carry all four: `[ui] tier` is reachable by flag and by
    /// environment, and `[supervision] auto_restart` by environment and by
    /// file. Every adjacent pair of the chain therefore has a key that
    /// shows the higher layer winning. No single key spans all four,
    /// because `[ui]` is not a table `ViewFile` carries.
    ///
    /// Both halves of every arm are asserted -- the value *and* the
    /// `Source` -- since a chain that is right for the wrong reason
    /// answers with the right value from the wrong layer, and only the
    /// provenance can tell the two apart.
    ///
    /// Flag-beats-file is `[engine] nvim_bin`'s own arm, and it is the only
    /// key that can show that adjacency directly: it is the one key this
    /// build both accepts a flag for and parses out of the file, so the two
    /// layers meet with nothing between them.
    #[test]
    fn flag_beats_env_beats_file_beats_derived() {
        let file = ViewConfig::from_toml_str(
            "[supervision]\nauto_restart = true\n\n[engine]\nnvim_bin = \"/from/file/nvim\"\n",
        )
        .expect("the fixture must parse");
        let env = |name: &str| match name {
            "VIEW_UI_TIER" => Some("basic".to_string()),
            "VIEW_SUPERVISION_AUTO_RESTART" => Some("false".to_string()),
            _ => None,
        };
        let flags = Overrides {
            tier: Some(TierChoice::Full),
            ..Overrides::default()
        };

        assert_eq!(
            resolve_with(
                &file,
                &Overrides {
                    nvim_bin: Some(PathBuf::from("/from/flag/nvim")),
                    ..Overrides::default()
                },
                &no_env,
            )
            .engine
            .nvim_bin,
            Resolved {
                value: Some(PathBuf::from("/from/flag/nvim")),
                source: Source::Flag
            },
            "the flag outranks the file"
        );
        assert_eq!(
            resolve_with(&file, &Overrides::default(), &no_env)
                .engine
                .nvim_bin,
            Resolved {
                value: Some(PathBuf::from("/from/file/nvim")),
                source: Source::File
            },
            "and with no flag the file is what named the editor"
        );

        let resolved = resolve_with(&file, &flags, &env);
        assert_eq!(
            resolved.ui.tier,
            Resolved {
                value: Some(TierChoice::Full),
                source: Source::Flag
            },
            "the flag outranks the environment"
        );
        assert_eq!(
            row(&resolved, "supervision", "auto_restart"),
            ("false".to_string(), Source::Env),
            "the environment outranks the file"
        );

        let resolved = resolve_with(&file, &Overrides::default(), &env);
        assert_eq!(
            resolved.ui.tier,
            Resolved {
                value: Some(TierChoice::Basic),
                source: Source::Env
            },
            "with no flag, the environment answers"
        );

        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert_eq!(
            row(&resolved, "supervision", "auto_restart"),
            ("true".to_string(), Source::File),
            "with no environment, the file answers -- and a key written at \
             its own default is still the file's answer"
        );
        assert_eq!(
            resolved.ui.tier,
            Resolved {
                value: None,
                source: Source::Derived
            },
            "with nothing named at all, the sentinel a caller probes for"
        );
        assert_eq!(
            row(
                &resolve_with(&ViewConfig::defaults(), &Overrides::default(), &no_env),
                "supervision",
                "auto_restart"
            ),
            ("true".to_string(), Source::Derived),
            "and an absent file leaves the same key derived"
        );
    }

    #[test]
    fn every_key_has_a_derived_default() {
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &no_env);
        for (key, value, source) in resolved.rows() {
            assert_eq!(
                source,
                Source::Derived,
                "[{}] {} has no derived answer",
                key.table,
                key.key
            );
            // the registry's own statement of the default, held to what the
            // resolver actually answers -- so the text a user reads in the
            // doctor's `derived` column and the text the chain produces are
            // one fact, not two that drift
            match key.derived {
                Some(stated) => assert_eq!(
                    value, stated,
                    "[{}] {} states a derived default the chain does not produce",
                    key.table, key.key
                ),
                None => assert!(
                    !value.is_empty(),
                    "[{}] {} renders nothing and states no derived default either",
                    key.table,
                    key.key
                ),
            }
        }
        // the `[ai]` rows are resolved a crate away, so what this crate can
        // hold them to is the registry's own statement of their defaults
        for key in keys().iter().filter(|key| is_ai(key)) {
            assert!(
                key.derived.is_some(),
                "[{}] {} is required, and no key may be",
                key.table,
                key.key
            );
        }
    }

    #[test]
    fn a_flagless_key_still_resolves_through_env_and_file() {
        let key = keys()
            .iter()
            .find(|key| key.table == "native" && key.key == "picker")
            .expect("the picker is a registry feature");
        assert_eq!(key.flag, None, "[native] switches are reached by --clean");

        let file = ViewConfig::from_toml_str("[native]\npicker = false\n")
            .expect("the fixture must parse");
        assert_eq!(
            row(
                &resolve_with(&file, &Overrides::default(), &no_env),
                "native",
                "picker"
            ),
            ("false".to_string(), Source::File),
            "a key with no flag still reads the file"
        );
        let env = |name: &str| (name == "VIEW_NATIVE_PICKER").then(|| "true".to_string());
        assert_eq!(
            row(
                &resolve_with(&file, &Overrides::default(), &env),
                "native",
                "picker"
            ),
            ("true".to_string(), Source::Env),
            "and still loses to the environment"
        );
    }

    #[test]
    fn native_and_supervision_keys_resolve_from_env() {
        let env = |name: &str| match name {
            "VIEW_NATIVE_PICKER" => Some("false".to_string()),
            "VIEW_SUPERVISION_AUTO_RESTART" => Some("false".to_string()),
            _ => None,
        };
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
        assert!(
            !resolved.tables.native.enabled("picker"),
            "VIEW_NATIVE_PICKER=false disables the picker with no file present"
        );
        assert!(
            resolved.tables.native.enabled("tree"),
            "one key's environment must not answer for another"
        );
        assert!(
            !resolved.tables.supervision.auto_restart,
            "VIEW_SUPERVISION_AUTO_RESTART=false surfaces a dead engine instead"
        );
    }

    /// Every key this crate resolves reads the name its own registry row
    /// generates, and no other. Walked rather than sampled: a lookup that
    /// misspells one table or key would otherwise resolve to the derived
    /// answer forever, with nothing to say so.
    #[test]
    fn every_resolved_key_reads_its_own_environment_name() {
        for key in keys().iter().filter(|key| !is_ai(key)) {
            let name = env_name(key);
            let env = |asked: &str| (asked == name).then(|| env_fixture(key).to_string());
            let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
            let (value, source) = row(&resolved, key.table, key.key);
            assert_eq!(
                source,
                Source::Env,
                "{name} does not reach [{}] {}",
                key.table,
                key.key
            );
            assert_eq!(
                value,
                env_fixture(key),
                "{name} resolved to something other than what it said"
            );
        }
    }

    #[test]
    fn view_log_is_never_read_as_config() {
        let asked = std::cell::RefCell::new(Vec::new());
        let env = |name: &str| {
            asked.borrow_mut().push(name.to_string());
            // every VIEW_* name answers, so a resolver reading one it does
            // not own would resolve to it rather than to its own default
            Some("false".to_string())
        };
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
        let generated: Vec<String> = keys().iter().map(env_name).collect();
        // the claim is over the `VIEW_*` namespace, which is the one the
        // registry generates into and the one a user's other `VIEW_*` names
        // live in. A name outside it is another program's vocabulary, and
        // the resolver reads exactly one such name -- `NVIM_APPNAME`, to
        // report the profile an unset `[engine] appname` inherits, never to
        // decide anything
        for name in asked.borrow().iter() {
            // the look-mode detection is the other reader of names outside
            // the namespace: a window manager announces itself under its
            // own name, and nothing view could generate would find it
            if name == INHERITED_APPNAME_ENV
                || name == XDG_CURRENT_DESKTOP
                || TILING_MARKERS.contains(&name.as_str())
            {
                continue;
            }
            assert!(
                name.starts_with("VIEW_") && generated.contains(name),
                "the resolver read {name}, which no registry row generates"
            );
        }
        for name in ["VIEW_LOG", "VIEW_LOG_FILE", "VIEW_HARNESS_TAP"] {
            assert!(
                !asked.borrow().iter().any(|read| read == name),
                "{name} is diagnostics, never config"
            );
        }
        assert!(
            !resolved.tables.native.enabled("picker"),
            "the names the registry does generate are still read"
        );
    }

    /// `--clean` asks whether view or a user's own config is at fault, and
    /// an answer that let the environment through would be answering a
    /// different question than the one the mode exists for. What
    /// the mode passes is an environment that says nothing; both halves
    /// are pinned here, so a suppression that stopped suppressing fails.
    #[test]
    fn clean_resolves_every_key_to_its_derived_default() {
        let file = ViewConfig::from_toml_str("[native]\npicker = false\n")
            .expect("the fixture must parse");
        let dirty = resolve_with(&file, &Overrides::default(), &every_key_set);
        assert!(
            dirty
                .rows()
                .iter()
                .all(|(_, _, source)| *source == Source::Env),
            "the fixture environment must reach every key: {:?}",
            dirty.rows()
        );

        // what `--clean` resolves: no config path at all, so the defaults
        // stand in for the file, and no environment layer either
        let clean = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &no_env);
        for (key, _, source) in clean.rows() {
            assert_eq!(
                source,
                Source::Derived,
                "[{}] {} survived --clean",
                key.table,
                key.key
            );
        }
        assert!(clean.tables.native.enabled("picker"));
        assert!(clean.tables.supervision.auto_restart);
    }

    #[test]
    fn rows_answer_every_key_this_crate_resolves() {
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &no_env);
        let answered: Vec<(&str, &str)> = resolved
            .rows()
            .into_iter()
            .map(|(key, _, _)| (key.table, key.key))
            .collect();
        let owed: Vec<(&str, &str)> = keys()
            .iter()
            .filter(|key| !is_ai(key))
            .map(|key| (key.table, key.key))
            .collect();
        assert_eq!(
            answered, owed,
            "every registry key but the ones `view-ai` resolves owes a row here"
        );
        assert!(
            answered.len() < keys().len(),
            "the `[ai]` rows are another crate's to answer"
        );
    }

    #[test]
    fn a_value_the_environment_cannot_be_read_as_falls_through() {
        let file = ViewConfig::from_toml_str("[native]\npicker = false\n")
            .expect("the fixture must parse");
        let env = |name: &str| match name {
            "VIEW_NATIVE_PICKER" => Some("off".to_string()),
            "VIEW_UI_TIER" => Some("turbo".to_string()),
            _ => None,
        };
        let resolved = resolve_with(&file, &Overrides::default(), &env);
        assert_eq!(
            row(&resolved, "native", "picker"),
            ("false".to_string(), Source::File),
            "text that is not a switch leaves the file's answer standing"
        );
        assert_eq!(
            resolved.ui.tier.source,
            Source::Derived,
            "and text that names no tier leaves the sentinel"
        );
    }

    /// Falling through is only half the answer a discarded value owes. The
    /// file layer already says so when it has to discard a `tree_width`,
    /// and an environment that silently stopped applying leaves a user
    /// staring at a variable they can see is set and cannot see doing
    /// anything.
    #[test]
    fn a_value_the_environment_cannot_be_read_as_says_so() {
        let env = |name: &str| match name {
            "VIEW_UI_TIER" => Some("turbo".to_string()),
            "VIEW_NATIVE_TREE_WIDTH" => Some("40%".to_string()),
            "VIEW_KEYS_SIDEBAR_WIDER" => Some("<Nope-Right>".to_string()),
            _ => None,
        };
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
        let notices = resolved.notices().join("\n");
        for (name, value) in [
            ("VIEW_UI_TIER", "turbo"),
            ("VIEW_NATIVE_TREE_WIDTH", "40%"),
            ("VIEW_KEYS_SIDEBAR_WIDER", "<Nope-Right>"),
        ] {
            assert!(
                notices.contains(name) && notices.contains(value),
                "{name}={value} was discarded in silence: {notices}"
            );
        }
        for (key, value, source) in resolved.rows() {
            let Some(derived) = key.derived else {
                continue;
            };
            assert_eq!(
                (value.as_str(), source),
                (derived, Source::Derived),
                "[{}] {} took an answer from a value view could not read",
                key.table,
                key.key
            );
        }
    }

    #[test]
    fn auto_and_bundled_are_the_absence_of_a_choice_at_every_layer() {
        let env = |name: &str| match name {
            "VIEW_UI_THEME" => Some("auto".to_string()),
            "VIEW_ENGINE_NVIM_BIN" => Some("bundled".to_string()),
            _ => None,
        };
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
        assert_eq!(
            resolved.ui.theme,
            Resolved {
                value: None,
                source: Source::Env
            },
            "a user who wrote `auto` still wrote something"
        );
        assert_eq!(
            resolved.engine.nvim_bin,
            Resolved {
                value: None,
                source: Source::Env
            },
            "and `bundled` is the same shape for the engine"
        );
        assert_eq!(row(&resolved, "ui", "theme").0, "auto");
        assert_eq!(row(&resolved, "engine", "nvim_bin").0, "bundled");
    }

    /// An unset `appname` is answered by the chain, not by a special case
    /// inside the table: the resolved value stays `None` so the spawn adds
    /// no `NVIM_APPNAME` and the child inherits whatever the process
    /// carries, while the report still names the profile that will be.
    #[test]
    fn appname_absent_inherits_the_environment() {
        let file = ViewConfig::from_toml_str("[engine]\nnvim_bin = \"bundled\"\n")
            .expect("the fixture must parse");
        let inherited = |name: &str| (name == "NVIM_APPNAME").then(|| "review".to_string());

        let resolved = resolve_with(&file, &Overrides::default(), &inherited);
        assert_eq!(
            resolved.engine.appname,
            Resolved {
                value: None,
                source: Source::Derived
            },
            "a table that named no profile sets no NVIM_APPNAME in the child"
        );
        assert_eq!(
            row(&resolved, "engine", "appname"),
            ("review".to_string(), Source::Derived),
            "and the report names the profile the child will actually run under"
        );

        let named = ViewConfig::from_toml_str("[engine]\nappname = \"work\"\n")
            .expect("the fixture must parse");
        assert_eq!(
            resolve_with(&named, &Overrides::default(), &inherited)
                .engine
                .appname,
            Resolved {
                value: Some("work".to_string()),
                source: Source::File
            },
            "a named profile replaces the inherited one"
        );
    }

    /// The chain, exercised at `[ui] tier`'s own call site rather than only
    /// where the chain is written: this key is reachable from all four
    /// layers, so the file's answer standing where no flag and no
    /// environment named one -- and losing the moment either does -- is
    /// what makes the file layer real for it.
    #[test]
    fn tier_from_file_loses_to_the_flag() {
        let file = ViewConfig::from_toml_str("[ui]\ntier = \"basic\"\ntheme = \"gruvbox\"\n")
            .expect("the fixture must parse");
        assert_eq!(
            resolve_with(&file, &Overrides::default(), &no_env).ui.tier,
            Resolved {
                value: Some(TierChoice::Basic),
                source: Source::File
            },
            "with nothing above it the file is what named the tier"
        );
        assert_eq!(
            resolve_with(&file, &Overrides::default(), &no_env).ui.theme,
            Resolved {
                value: Some("gruvbox".to_string()),
                source: Source::File
            },
            "and the colorscheme beside it"
        );
        let flags = Overrides {
            tier: Some(TierChoice::Full),
            theme: Some("auto".to_string()),
            ..Overrides::default()
        };
        assert_eq!(
            resolve_with(&file, &flags, &no_env).ui.tier,
            Resolved {
                value: Some(TierChoice::Full),
                source: Source::Flag
            },
            "the flag outranks the file"
        );
        assert_eq!(
            resolve_with(&file, &flags, &no_env).ui.theme,
            Resolved {
                value: None,
                source: Source::Flag
            },
            "and `--theme auto` is a flag that names the absence of a choice, which still \
             outranks a file that named one"
        );
        let env = |name: &str| (name == "VIEW_UI_TIER").then(|| "standard".to_string());
        assert_eq!(
            resolve_with(&file, &Overrides::default(), &env).ui.tier,
            Resolved {
                value: Some(TierChoice::Standard),
                source: Source::Env
            },
            "and the environment sits between them"
        );
    }

    /// A tier the file spelled at a value this build cannot read answers
    /// from the layer below and says so, rather than resolving to the
    /// file's own `auto` -- which would be a provenance row naming a layer
    /// that chose nothing.
    #[test]
    fn a_file_tier_view_cannot_read_falls_through_with_a_notice() {
        let file =
            ViewConfig::from_toml_str("[ui]\ntier = \"turbo\"\n").expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert_eq!(
            resolved.ui.tier,
            Resolved {
                value: None,
                source: Source::Derived
            }
        );
        let notices = resolved.notices().join("\n");
        assert!(
            notices.contains("turbo") && notices.contains("[ui] tier"),
            "the file's own discarded value must reach the same notice list an \
             environment's does: {notices:?}"
        );
    }

    #[test]
    fn the_flag_layer_carries_every_key_that_has_a_flag() {
        let flags = Overrides {
            tier: Some(TierChoice::Standard),
            theme: Some("gruvbox".to_string()),
            nvim_bin: Some(PathBuf::from("/opt/nvim/bin/nvim")),
            appname: Some("work".to_string()),
            single_grid: Some(true),
            // nvim rather than tiles: without multigrid there are no window
            // placements to draw tiles from, so `--single-grid --panes
            // tiles` is a contradiction view resolves by ignoring the
            // second, and a fixture that spelled it could not show the
            // second reaching its key
            panes: Some(Some(Panes::Nvim)),
        };
        let resolved = resolve_with(&ViewConfig::defaults(), &flags, &every_key_set);
        for key in keys().iter().filter(|key| key.flag.is_some()) {
            let (_, source) = row(&resolved, key.table, key.key);
            assert_eq!(
                source,
                Source::Flag,
                "{} does not reach [{}] {}",
                key.flag.unwrap_or("<none>"),
                key.table,
                key.key
            );
        }
        assert_eq!(resolved.engine.appname.value.as_deref(), Some("work"));
        assert_eq!(
            resolved.engine.nvim_bin.value,
            Some(PathBuf::from("/opt/nvim/bin/nvim"))
        );
        assert!(resolved.engine.single_grid.value);
        assert_eq!(resolved.ui.panes.value, Panes::Nvim);
    }

    /// One case per row of the detection table, with the marker the row
    /// answers under kept on the answer: a report that said `nvim` without
    /// naming what decided it leaves a user to guess which of their
    /// environment view read.
    #[test]
    fn each_tiling_marker_resolves_to_nvim_mode() {
        let mut cases: Vec<(&str, String)> = TILING_MARKERS
            .iter()
            .map(|marker| (*marker, "1".to_string()))
            .collect();
        for desktop in TILING_DESKTOPS {
            cases.push((XDG_CURRENT_DESKTOP, format!("ubuntu:{desktop}:GNOME")));
        }
        for (marker, value) in cases {
            let env = |asked: &str| (asked == marker).then(|| value.clone());
            let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
            assert_eq!(
                (resolved.ui.panes.value, resolved.ui.panes_marker),
                (Panes::Nvim, Some(marker)),
                "{marker}={value} must leave the layout to the window manager"
            );
            assert_eq!(
                row(&resolved, "ui", "panes"),
                (format!("nvim ({marker})"), Source::Derived),
                "the report row names the marker that decided it"
            );
        }
    }

    #[test]
    fn an_ssh_session_with_no_marker_resolves_to_tiles() {
        // the client's environment never reaches the server, so nothing a
        // remote session can read says what is drawing its terminal
        let env =
            |asked: &str| matches!(asked, "SSH_CONNECTION" | "SSH_TTY").then(|| "1".to_string());
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env);
        assert_eq!(
            (resolved.ui.panes.value, resolved.ui.panes_marker),
            (Panes::Tiles, None)
        );
        assert_eq!(
            row(&resolved, "ui", "panes"),
            ("tiles".to_string(), Source::Derived)
        );
    }

    #[test]
    fn an_explicit_panes_value_beats_every_marker() {
        let env = |asked: &str| (asked == "HYPRLAND_INSTANCE_SIGNATURE").then(|| "1".to_string());
        let file =
            ViewConfig::from_toml_str("[ui]\npanes = \"tiles\"\n").expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &env);
        assert_eq!(
            (resolved.ui.panes.value, resolved.ui.panes_marker),
            (Panes::Tiles, None),
            "a value the user wrote outranks what view would have detected"
        );
        assert_eq!(
            row(&resolved, "ui", "panes"),
            ("tiles".to_string(), Source::File)
        );
        let flagged = resolve_with(
            &file,
            &Overrides {
                panes: Some(Some(Panes::Nvim)),
                ..Overrides::default()
            },
            &env,
        );
        assert_eq!(
            row(&flagged, "ui", "panes"),
            ("nvim".to_string(), Source::Flag)
        );
    }

    #[test]
    fn a_bad_panes_value_notices_and_falls_back_to_auto() {
        let file = ViewConfig::from_toml_str("[ui]\npanes = \"mosaic\"\n")
            .expect("a mode this build cannot read must not fail the file");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert_eq!(
            (resolved.ui.panes.value, resolved.ui.panes_marker),
            (Panes::Tiles, None),
            "an unreadable mode answers from the layer below rather than the file"
        );
        let notices = resolved.notices().join("\n");
        for fact in ["mosaic", PANES_EXPECTED, "[ui] panes"] {
            assert!(notices.contains(fact), "{fact} is missing from {notices:?}");
        }
    }

    #[test]
    fn single_grid_forces_nvim_mode_with_a_notice() {
        let file =
            ViewConfig::from_toml_str("[ui]\npanes = \"tiles\"\n\n[engine]\nsingle_grid = true\n")
                .expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert_eq!(
            (resolved.ui.panes.value, resolved.ui.panes_marker),
            (Panes::Nvim, Some(SINGLE_GRID_MARKER)),
            "without multigrid there are no window placements to draw tiles from"
        );
        assert!(
            resolved.notices().iter().any(|n| n == SINGLE_GRID_NOTICE),
            "a mode the user asked for and did not get owes a notice: {:?}",
            resolved.notices()
        );
        // and a session that never asked for tiles is told nothing
        let quiet = resolve_with(
            &ViewConfig::from_toml_str("[engine]\nsingle_grid = true\n")
                .expect("the fixture must parse"),
            &Overrides::default(),
            &no_env,
        );
        assert_eq!(quiet.ui.panes.value, Panes::Nvim);
        assert!(!quiet.notices().iter().any(|n| n == SINGLE_GRID_NOTICE));
    }

    #[test]
    fn a_tier_choice_is_the_terminals_own_tier() {
        for (choice, tier) in [
            (TierChoice::Full, Tier::Full),
            (TierChoice::Standard, Tier::Standard),
            (TierChoice::Basic, Tier::Basic),
        ] {
            assert_eq!(Tier::from(choice), tier, "{}", choice.label());
        }
    }
}
