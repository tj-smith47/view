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
    discarded_file, BOOL_EXPECTED, COLOR_EXPECTED, KEYS_EXPECTED, PANES_EXPECTED,
    TABLINE_SHOWS_EXPECTED, TIER_EXPECTED, WIDTH_EXPECTED,
};
use view_core::model::{Panes, Tier};
use view_core::native::chords::{
    self, DesktopChord, KeyProfile, ModifierChoice, DESKTOP_CHORD_COUNT,
};
use view_core::native::geometry;
use view_core::native::geometry::{Anchor, NativeSurface, SurfaceLayout, SurfacePlacement};
use view_core::native::keys::{well_formed, Action, Direction, KeyBindings};
use view_core::native::mappings;
use view_core::native::pill::TablineShows;
use view_core::native::registry;

use super::keys::{env_name, keys, ConfigKey};
use super::profile;
use super::{
    parse_color, parse_nvim_bin, parse_panes, parse_theme, parse_tier, read_key, KeysConfig,
    NativeConfig, SupervisionConfig, UiTokens, ViewConfig, AUTO, BUNDLED,
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

impl<T> Resolved<T> {
    /// Builds a resolved answer directly, for a caller outside this crate
    /// that needs one of its own -- a test fixture, chiefly -- since
    /// `#[non_exhaustive]` refuses the struct literal past this crate's own
    /// boundary.
    #[must_use]
    pub fn new(value: T, source: Source) -> Self {
        Self { value, source }
    }
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
    /// `[keys] profile`, `"auto"` derived through [`profile::detect_profile`]
    /// where nothing named one.
    pub profile: Resolved<KeyProfile>,
    /// The marker that decided [`Self::profile`] under `"auto"`, on the
    /// terms [`ResolvedUi::panes_marker`] states.
    pub profile_marker: Option<&'static str>,
    /// `[keys] desktop_modifier`, still the raw choice: turning it into the
    /// modifier this session's chords are actually spelled with needs the
    /// terminal's own answer ([`profile::modifier_for`]), which a caller
    /// holding `model.caps` supplies at takeover.
    pub desktop_modifier: Resolved<ModifierChoice>,
    /// `[keys.desktop]`'s 46 rows, in [`chords::desktop_chords`] order. A
    /// row whose source is [`Source::Derived`] carries no override --
    /// [`chords::DesktopChord::lhs`] is this session's answer for it -- and
    /// a row from any other layer carries the override verbatim, empty
    /// included, which is how a chord is left unbound.
    pub desktop: [Resolved<String>; DESKTOP_CHORD_COUNT],
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
    /// Where each of view's own surfaces sits, indexed by
    /// [`NativeSurface::index`].
    pub surfaces: [SurfaceLayout; 4],
    /// Where each surface's `placement`, `anchor` and `size` came from, in
    /// that order, indexed by [`NativeSurface::index`].
    surfaces_source: [[Source; 3]; 4],
    /// Where `[native] tree_width` came from.
    tree_width: Source,
    /// Where `[native] tabline_shows` came from.
    tabline_shows: Source,
    /// Where each `[keys]` action's bindings came from, in [`KEY_ACTIONS`]
    /// order.
    keys: [Source; KEY_ACTIONS.len()],
    /// Where `[keys] toggle_gaps` came from. Not a [`KEY_ACTIONS`] row: that
    /// table drives `KeyBindings::rebind`, and this key registers a real
    /// nvim mapping instead (see [`super::resolve_ui_lhs`]), file-layer
    /// only -- no `VIEW_KEYS_TOGGLE_GAPS` exists to read a second layer from.
    ui_gaps_key: Source,
    /// [`Self::ui_gaps_key`]'s own for `[keys] cycle_surfaces`.
    ui_cycle_key: Source,
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
            // the pill is the tiled look's own row and nvim's tab line is
            // the other look's, so the switch nobody set follows `panes`
            // rather than the registry bit, which stays the answer for
            // every other feature
            if feature.id == "tabline" {
                ui.panes.value == Panes::Tiles
            } else {
                feature.default_on
            },
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
    let tabline_shows = layer(
        None,
        env_read(
            env,
            "native",
            "tabline_shows",
            TABLINE_SHOWS_EXPECTED,
            TablineShows::parse,
            &mut notices,
        ),
        file.spells("native", "tabline_shows")
            .then(|| file.native.tabline_shows()),
        TablineShows::default(),
    );
    let file_surfaces = super::surfaces(file, &mut notices);
    let (surfaces, surfaces_source) =
        resolve_surfaces(file, file_surfaces, &tree_width, env, &mut notices);
    let (bindings, key_sources) = resolve_keys(file, env, &mut notices);
    let (gaps_lhs, ui_gaps_key) =
        resolve_ui_lhs_env(file, env, "toggle_gaps", file.keys.gaps_lhs(), &mut notices);
    let (cycle_lhs, ui_cycle_key) = resolve_ui_lhs_env(
        file,
        env,
        "cycle_surfaces",
        file.keys.cycle_lhs(),
        &mut notices,
    );
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
    let (profile, profile_marker) = resolve_profile(file, env, &mut notices);
    let desktop_modifier = layer(
        None,
        env_read(
            env,
            "keys",
            "desktop_modifier",
            DESKTOP_MODIFIER_EXPECTED,
            parse_modifier_choice,
            &mut notices,
        ),
        read_key(
            file.keys.desktop_modifier(),
            ("keys", "desktop_modifier"),
            DESKTOP_MODIFIER_EXPECTED,
            parse_modifier_choice,
            &mut notices,
        ),
        ModifierChoice::Auto,
    );
    let desktop = resolve_desktop(file, env, &mut notices);
    ResolvedConfig {
        ui,
        engine,
        profile,
        profile_marker,
        desktop_modifier,
        desktop,
        surfaces,
        surfaces_source,
        tables: ViewConfig {
            native: NativeConfig {
                disabled,
                tree_width: tree_width.value,
                // the file layer's own notice, which an environment value
                // above it neither answers for nor silences: a mistyped
                // `tree_width` is still a mistyped `tree_width`
                tree_width_notice: file.native.tree_width_notice,
                tabline_shows: tabline_shows.value,
                tabline_shows_notice: file.native.tabline_shows_notice,
            },
            supervision: SupervisionConfig {
                auto_restart: auto_restart.value,
            },
            keys: KeysConfig {
                bindings,
                notices: file.keys.notices().to_vec(),
                gaps_lhs: gaps_lhs.clone(),
                cycle_lhs: cycle_lhs.clone(),
                profile: file.keys.profile().map(str::to_string),
                desktop_modifier: file.keys.desktop_modifier().map(str::to_string),
                desktop: file.keys.desktop().clone(),
            },
            engine: file.engine.clone(),
            ui: file.ui.clone(),
            spelled: file.spelled.clone(),
        },
        notices,
        native,
        tree_width: tree_width.source,
        tabline_shows: tabline_shows.source,
        keys: key_sources,
        ui_gaps_key,
        ui_cycle_key,
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

/// One `[keys]` single-notation action -- `toggle_gaps`/`cycle_surfaces` --
/// through the same file/env/derived chain [`resolve_keys`] runs the
/// rebindable table through. `file_lhs` is the file layer's own answer
/// (already resolved against `lhs_is_spellable`, one crate up), so this
/// only has to place it against `file.spells` and check the environment
/// above it.
fn resolve_ui_lhs_env(
    file: &ViewConfig,
    env: &dyn Fn(&str) -> Option<String>,
    key: &str,
    file_lhs: &str,
    notices: &mut Vec<String>,
) -> (String, Source) {
    let mut lhs = file_lhs.to_string();
    let mut source = if file.spells("keys", key) {
        Source::File
    } else {
        Source::Derived
    };
    if let Some(raw) = env_value(env, "keys", key) {
        if view_core::native::mappings::lhs_is_spellable(&raw) {
            lhs = raw;
            source = Source::Env;
        } else {
            notices.push(discarded("keys", key, &raw, KEYS_EXPECTED));
        }
    }
    (lhs, source)
}

impl ResolvedConfig {
    /// Every key, its resolved value rendered for display, and where it
    /// came from, in registry order. The doctor's config section is this
    /// walk; nothing re-derives the list.
    ///
    /// Every registry row whose table is not `[ai]`, minus `keys.desktop_modifier`,
    /// and no others: `[ai]` is parsed and resolved by the crate that owns
    /// it, and a caller that can name both crates appends its answers to
    /// these; `keys.desktop_modifier`'s real answer needs the terminal's
    /// own probe, which this resolver is never handed, so the caller that
    /// holds it (`crates/view/src/main.rs`'s `caps_notice`) prints that one
    /// row itself, beside this walk.
    #[must_use]
    pub fn rows(&self) -> Vec<(&'static ConfigKey, String, Source)> {
        keys()
            .iter()
            .filter_map(|key| self.answer(key).map(|(value, source)| (key, value, source)))
            .collect()
    }

    /// Whether `[native] tabline` is still the answer `[ui] panes` derived,
    /// so a `:View ui panes` flip derives it again. False once a flag, an
    /// environment variable or a `view.toml` line spelled the key.
    #[must_use]
    pub fn tabline_follows_look(&self) -> bool {
        registry::features()
            .iter()
            .position(|feature| feature.id == "tabline")
            .and_then(|index| self.native.get(index))
            .is_none_or(|source| *source == Source::Derived)
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
            (table, "placement" | "anchor" | "size")
                if NativeSurface::ALL
                    .iter()
                    .any(|surface| surface.dotted_table() == table) =>
            {
                let surface = NativeSurface::ALL
                    .into_iter()
                    .find(|surface| surface.dotted_table() == table)?;
                let index = surface.index();
                let layout = self.surfaces[index];
                let source = self.surfaces_source[index];
                match key.key {
                    "placement" => (layout.placement.label().to_string(), source[0]),
                    "anchor" => (layout.anchor.label().to_string(), source[1]),
                    _ => (layout.size.to_string(), source[2]),
                }
            }
            ("native", "tree_width") => {
                (self.tables.native.tree_width.to_string(), self.tree_width)
            }
            ("native", "tabline_shows") => (
                self.tables.native.tabline_shows().label().to_string(),
                self.tabline_shows,
            ),
            ("keys", "toggle_gaps") => (self.tables.keys.gaps_lhs().to_string(), self.ui_gaps_key),
            ("keys", "cycle_surfaces") => {
                (self.tables.keys.cycle_lhs().to_string(), self.ui_cycle_key)
            }
            ("keys", "profile") => (
                match self.profile_marker {
                    Some(marker) => format!("{} ({marker})", profile_label(self.profile.value)),
                    None => profile_label(self.profile.value).to_string(),
                },
                self.profile.source,
            ),
            // the real answer needs the terminal's own probe, which this
            // resolver never holds -- the caller that does prints it beside
            // this walk rather than through it (`crates/view/src/main.rs`'s
            // `caps_notice`)
            ("keys", "desktop_modifier") => return None,
            ("keys.desktop", id) => {
                let index = chords::desktop_chords().iter().position(|c| c.id == id)?;
                let row = &self.desktop[index];
                (row.value.clone(), row.source)
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

/// What a `[keys] profile` outside the vocabulary is answered with.
const PROFILE_EXPECTED: &str = "one of auto, desktop or editor";

/// [`PROFILE_EXPECTED`]'s own for `[keys] desktop_modifier`.
const DESKTOP_MODIFIER_EXPECTED: &str = "one of auto, super or alt";

/// What a `[keys.desktop]` value that cannot be a chord's left-hand side is
/// answered with. Empty is not that case: it is the way to leave a chord
/// unbound, on the terms [`resolve_desktop_row`] states.
const DESKTOP_KEY_EXPECTED: &str =
    "a key notation with no quote, backslash or newline in it, spelled as nvim spells it, or \
     empty to leave the chord unbound";

/// One `[keys.desktop]` row, file and environment layered over the row's
/// own `with_super` spelling -- which is this row's *placeholder* answer
/// under [`Source::Derived`] rather than the value a caller should bind: a
/// derived row carries no override at all, and [`chords::DesktopChord::lhs`]
/// run against this session's real modifier is the answer for it. A row
/// from any other layer is the override verbatim, empty included, which is
/// how a chord is left unbound.
fn resolve_desktop_row(
    chord: &DesktopChord,
    file: &ViewConfig,
    env: &dyn Fn(&str) -> Option<String>,
    notices: &mut Vec<String>,
) -> Resolved<String> {
    let mut resolved = Resolved {
        value: chord.with_super.to_string(),
        source: Source::Derived,
    };
    if let Some(raw) = file.keys.desktop().get(chord.id) {
        if raw.is_empty() || (mappings::lhs_is_spellable(raw) && well_formed(raw)) {
            resolved = Resolved {
                value: raw.clone(),
                source: Source::File,
            };
        } else {
            notices.push(discarded_file(
                raw,
                DESKTOP_KEY_EXPECTED,
                "keys.desktop",
                chord.id,
            ));
        }
    }
    if let Some(raw) = env_value_desktop(env, chord.id) {
        if raw.is_empty() || (mappings::lhs_is_spellable(&raw) && well_formed(&raw)) {
            resolved = Resolved {
                value: raw,
                source: Source::Env,
            };
        } else {
            notices.push(discarded(
                "keys.desktop",
                chord.id,
                &raw,
                DESKTOP_KEY_EXPECTED,
            ));
        }
    }
    resolved
}

/// What a `[keys.desktop]` id naming no chord this build knows is answered
/// with: the id is ignored, and any chord it might have meant to override
/// answers as if the file had said nothing.
fn desktop_unknown_notice(id: &str, value: &str) -> String {
    format!(
        "view: [keys.desktop] {id} = {value} names no chord this build knows; the id is ignored"
    )
}

/// Every `[keys.desktop]` row, in [`chords::desktop_chords`] order, with a
/// notice for every id the file wrote that names no chord row.
fn resolve_desktop(
    file: &ViewConfig,
    env: &dyn Fn(&str) -> Option<String>,
    notices: &mut Vec<String>,
) -> [Resolved<String>; DESKTOP_CHORD_COUNT] {
    for (id, raw) in file.keys.desktop() {
        if chords::desktop_chord(id).is_none() {
            notices.push(desktop_unknown_notice(id, raw));
        }
    }
    let mut rows = chords::desktop_chords().iter();
    std::array::from_fn(|_| {
        rows.next().map_or_else(
            || Resolved {
                value: String::new(),
                source: Source::Derived,
            },
            |chord| resolve_desktop_row(chord, file, env, notices),
        )
    })
}

/// A profile a user named, `Some(None)` for the word that names the absence
/// of a choice, and `None` for text that names neither -- which falls
/// through to the layer below, the way [`parse_tier`] does.
fn parse_profile_choice(value: &str) -> Option<Option<KeyProfile>> {
    match value.trim().to_ascii_lowercase().as_str() {
        AUTO => Some(None),
        "desktop" => Some(Some(KeyProfile::Desktop)),
        "editor" => Some(Some(KeyProfile::Editor)),
        _ => None,
    }
}

/// A modifier choice a user named, or `None` for text that names none.
/// Unlike [`parse_profile_choice`], `"auto"` is one of [`ModifierChoice`]'s
/// own variants rather than a second `Option` layer -- the enum already
/// carries the absence of a choice.
fn parse_modifier_choice(value: &str) -> Option<ModifierChoice> {
    match value.trim().to_ascii_lowercase().as_str() {
        AUTO => Some(ModifierChoice::Auto),
        "super" => Some(ModifierChoice::Super),
        "alt" => Some(ModifierChoice::Alt),
        _ => None,
    }
}

/// The word a profile is written as, in every layer that carries one.
///
/// `KeyProfile` is `#[non_exhaustive]`, read from another crate; a variant
/// this build has never heard of answers the way the one that binds every
/// omarchy chord does, since a build that cannot name the new variant
/// cannot know it is safe to bind fewer keys either.
#[must_use]
const fn profile_label(profile: KeyProfile) -> &'static str {
    match profile {
        KeyProfile::Editor => "editor",
        _ => "desktop",
    }
}

/// The value [`ResolvedConfig::rows`] would print for `keys.profile`, for a
/// caller that already holds the profile and its marker (`:View keys
/// profile`'s bare-verb report) and has no `ResolvedConfig` left to read it
/// from.
#[must_use]
pub fn profile_report_value(profile: KeyProfile, marker: Option<&'static str>) -> String {
    match marker {
        Some(marker) => format!("{} ({marker})", profile_label(profile)),
        None => profile_label(profile).to_string(),
    }
}

/// `[keys] profile`, `"auto"` derived through [`profile::detect_profile`]
/// where nothing named one, on the terms [`detect_panes`]/`ui.panes`
/// resolves under: an explicit `"auto"` reads exactly like an absent key,
/// since neither layer left a concrete choice standing.
fn resolve_profile(
    file: &ViewConfig,
    env: &dyn Fn(&str) -> Option<String>,
    notices: &mut Vec<String>,
) -> (Resolved<KeyProfile>, Option<&'static str>) {
    let asked = layer(
        None,
        env_read(
            env,
            "keys",
            "profile",
            PROFILE_EXPECTED,
            parse_profile_choice,
            notices,
        ),
        read_key(
            file.keys.profile(),
            ("keys", "profile"),
            PROFILE_EXPECTED,
            parse_profile_choice,
            notices,
        ),
        None,
    );
    let (derived, derived_marker) = profile::detect_profile(env);
    match asked.value {
        Some(value) => (
            Resolved {
                value,
                source: asked.source,
            },
            None,
        ),
        None => (
            Resolved {
                value: derived,
                source: Source::Derived,
            },
            derived_marker,
        ),
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

/// Every surface's own table with the environment layered over the file's
/// answer.
///
/// `[native] tree_width` is the tree's `size` under its older name, so the
/// older key's environment value answers the newer key too. Without that
/// the alias would hold at the file layer and break at the one above it.
/// No other surface carries an older name, so only the tree's `size` reads
/// a second key.
fn resolve_surfaces(
    file: &ViewConfig,
    file_surfaces: [SurfaceLayout; 4],
    tree_width: &Resolved<u16>,
    env: &dyn Fn(&str) -> Option<String>,
    notices: &mut Vec<String>,
) -> ([SurfaceLayout; 4], [[Source; 3]; 4]) {
    let mut surfaces = file_surfaces;
    let mut sources = [[Source::Derived; 3]; 4];
    for surface in NativeSurface::ALL {
        let table = surface.dotted_table();
        let index = surface.index();
        let from_file = file_surfaces[index];
        let spelled = |key: &str| file.spells(table, key);
        let placement = layer(
            None,
            env_read(
                env,
                table,
                "placement",
                PLACEMENT_EXPECTED,
                SurfacePlacement::parse,
                notices,
            ),
            spelled("placement").then_some(from_file.placement),
            SurfacePlacement::default(),
        );
        let allowed = super::surfaces::anchors(surface, placement.value);
        let anchor = layer(
            None,
            env_read(
                env,
                table,
                "anchor",
                &super::surfaces::anchors_expected(surface, placement.value),
                |value| parse_anchor(allowed, value),
                notices,
            ),
            spelled("anchor").then_some(from_file.anchor),
            super::surfaces::default_anchor(surface, placement.value),
        );
        let tree_width_env = (surface == NativeSurface::Tree && tree_width.source == Source::Env)
            .then_some(tree_width.value);
        let size = layer(
            None,
            env_read(env, table, "size", WIDTH_EXPECTED, parse_width, notices).or(tree_width_env),
            (spelled("size")
                || (surface == NativeSurface::Tree && file.spells("native", "tree_width")))
            .then_some(from_file.size),
            SurfaceLayout::default_for(surface).size,
        );
        if let Some(layout) = surfaces.get_mut(index) {
            *layout = SurfaceLayout::new(placement.value, anchor.value, size.value);
        }
        sources[index] = [placement.source, anchor.source, size.source];
    }
    (surfaces, sources)
}

/// What a `[ui.surfaces.<id>] placement` outside the pair is answered with.
const PLACEMENT_EXPECTED: &str = "overlay or windowed";

/// The anchor `allowed` names, or `None` for a word outside that surface's
/// own set.
fn parse_anchor(allowed: &[Anchor], value: &str) -> Option<Anchor> {
    let value = value.trim();
    allowed
        .iter()
        .copied()
        .find(|anchor| anchor.label() == value)
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

/// [`view_core::config::alias_notice`], re-exported at this path so every
/// call site in this crate keeps reading `super::resolve::alias_notice`.
/// The function itself lives in `view-core` because `view-ai` needs the
/// same wording for `[ai] panel_width` and may not depend on this crate.
pub(super) use view_core::config::alias_notice;

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

/// [`env_value`] for one `[keys.desktop]` row, with empty kept rather than
/// read as absent: a `[keys.desktop]` value that is empty is a real answer
/// (leave the chord unbound, [`resolve_desktop_row`]'s own doc comment),
/// not the "no value" `env_value` reads it as for every other key.
fn env_value_desktop(env: &dyn Fn(&str) -> Option<String>, chord_id: &str) -> Option<String> {
    let row = keys()
        .iter()
        .find(|row| row.table == "keys.desktop" && row.key == chord_id)?;
    env(&env_name(row)).map(|value| value.trim().to_string())
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

    /// Whether `rows()` answers `row` at all: every key but `[ai]`'s and
    /// `keys.desktop_modifier`, whose real answer needs the terminal's own
    /// probe (see `rows`'s own rustdoc).
    fn answered_by_rows(row: &ConfigKey) -> bool {
        !is_ai(row) && !(row.table == "keys" && row.key == "desktop_modifier")
    }

    /// An environment carrying a legal value for every key this crate
    /// resolves, so a test that must show a layer being *suppressed* has
    /// something to suppress.
    fn every_key_set(name: &str) -> Option<String> {
        keys()
            .iter()
            .find(|row| env_name(row) == name)
            .map(|row| env_fixture(row, true).to_string())
    }

    /// A legal environment value for one key, per its own type.
    ///
    /// `windowed` is the placement this fixture's own env layer resolves
    /// to for the row's surface: `true` from [`every_key_set`], which
    /// always answers `placement` with `"windowed"` too, `false` from the
    /// one-key-at-a-time walk, which leaves every other key at its file/
    /// derived answer -- `SurfacePlacement::default()`, `Overlay`. An
    /// anchor fixture has to stay inside whichever vocabulary its own
    /// placement will actually resolve to, since a windowed-only word
    /// (notifications' `left`/`right`/`top`/`bottom`) is not one the
    /// overlay corners answer, and the reverse holds for the corners.
    fn env_fixture(row: &ConfigKey, windowed: bool) -> &'static str {
        if let Some(surface) = NativeSurface::ALL
            .into_iter()
            .find(|surface| surface.dotted_table() == row.table)
        {
            let placement = if windowed {
                SurfacePlacement::Windowed
            } else {
                SurfacePlacement::Overlay
            };
            return match row.key {
                "placement" => "windowed",
                "anchor" => super::super::surfaces::anchors(surface, placement)
                    .last()
                    .map_or("right", |anchor| anchor.label()),
                _ => "40",
            };
        }
        match (row.table, row.key) {
            ("ui", "tier") => "basic",
            ("ui", "theme") => "gruvbox",
            ("ui", "panes") => "nvim",
            ("ui.tokens", "accent") => "#89b4fa",
            ("engine", "nvim_bin") => "/opt/nvim/bin/nvim",
            ("engine", "appname") => "work",
            ("native", "tree_width") => "40",
            ("native", "tabline_shows") => "buffers",
            ("keys", "profile") => "desktop",
            ("keys", "desktop_modifier") => "super",
            ("keys.desktop", _) => "<M-x>",
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
        for key in keys().iter().filter(|key| answered_by_rows(key)) {
            let name = env_name(key);
            let env = |asked: &str| (asked == name).then(|| env_fixture(key, false).to_string());
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
                env_fixture(key, false),
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
            // own name, and nothing view could generate would find it. The
            // profile derivation reads its own four markers the same way --
            // `profile::detect_profile`'s own vocabulary, not view's
            if name == INHERITED_APPNAME_ENV
                || name == XDG_CURRENT_DESKTOP
                || TILING_MARKERS.contains(&name.as_str())
                || profile::PROFILE_MARKERS.contains(&name.as_str())
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
            .filter(|key| answered_by_rows(key))
            .map(|key| (key.table, key.key))
            .collect();
        assert_eq!(
            answered, owed,
            "every registry key but the ones `view-ai` resolves, and \
             `keys.desktop_modifier`, owes a row here"
        );
        assert!(
            answered.len() < keys().len(),
            "the `[ai]` rows and `keys.desktop_modifier` are answered elsewhere"
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
    fn tabline_derives_on_under_tiles() {
        let file =
            ViewConfig::from_toml_str("[ui]\npanes = \"tiles\"\n").expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert!(
            resolved.tables.native.enabled("tabline"),
            "the pill is the tiled look's own row"
        );
    }

    #[test]
    fn tabline_derives_off_under_nvim_mode() {
        let file =
            ViewConfig::from_toml_str("[ui]\npanes = \"nvim\"\n").expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert!(
            !resolved.tables.native.enabled("tabline"),
            "nvim mode leaves the row to nvim and the user's own plugins"
        );
    }

    #[test]
    fn an_explicit_tabline_value_beats_the_derivation() {
        let off =
            ViewConfig::from_toml_str("[ui]\npanes = \"tiles\"\n\n[native]\ntabline = false\n")
                .expect("the fixture must parse");
        let resolved = resolve_with(&off, &Overrides::default(), &no_env);
        assert_eq!(
            row(&resolved, "native", "tabline"),
            ("false".to_string(), Source::File)
        );

        let on = ViewConfig::from_toml_str("[ui]\npanes = \"nvim\"\n\n[native]\ntabline = true\n")
            .expect("the fixture must parse");
        let resolved = resolve_with(&on, &Overrides::default(), &no_env);
        assert_eq!(
            row(&resolved, "native", "tabline"),
            ("true".to_string(), Source::File)
        );

        let env = |asked: &str| (asked == "VIEW_NATIVE_TABLINE").then(|| "false".to_string());
        let tiles =
            ViewConfig::from_toml_str("[ui]\npanes = \"tiles\"\n").expect("the fixture must parse");
        assert_eq!(
            row(
                &resolve_with(&tiles, &Overrides::default(), &env),
                "native",
                "tabline"
            ),
            ("false".to_string(), Source::Env)
        );
    }

    #[test]
    fn the_tabline_row_reports_the_derived_source() {
        let tiles =
            ViewConfig::from_toml_str("[ui]\npanes = \"tiles\"\n").expect("the fixture must parse");
        assert_eq!(
            row(
                &resolve_with(&tiles, &Overrides::default(), &no_env),
                "native",
                "tabline"
            ),
            ("true".to_string(), Source::Derived)
        );
        let nvim =
            ViewConfig::from_toml_str("[ui]\npanes = \"nvim\"\n").expect("the fixture must parse");
        assert_eq!(
            row(
                &resolve_with(&nvim, &Overrides::default(), &no_env),
                "native",
                "tabline"
            ),
            ("false".to_string(), Source::Derived)
        );
        // `--panes` is a layer above the file, and the derivation follows
        // whatever answer won rather than the one the file wrote
        let flagged = resolve_with(
            &tiles,
            &Overrides {
                panes: Some(Some(Panes::Nvim)),
                ..Overrides::default()
            },
            &no_env,
        );
        assert_eq!(
            row(&flagged, "native", "tabline"),
            ("false".to_string(), Source::Derived)
        );
    }

    #[test]
    fn tabline_shows_answers_tabs_until_the_file_or_the_environment_says_otherwise() {
        let resolved = resolve_with(&ViewConfig::defaults(), &Overrides::default(), &no_env);
        assert_eq!(
            row(&resolved, "native", "tabline_shows"),
            ("tabs".to_string(), Source::Derived)
        );
        let file = ViewConfig::from_toml_str("[native]\ntabline_shows = \"buffers\"\n")
            .expect("the fixture must parse");
        assert_eq!(
            row(
                &resolve_with(&file, &Overrides::default(), &no_env),
                "native",
                "tabline_shows"
            ),
            ("buffers".to_string(), Source::File)
        );
        let env =
            |asked: &str| (asked == "VIEW_NATIVE_TABLINE_SHOWS").then(|| "buffers".to_string());
        assert_eq!(
            row(
                &resolve_with(&ViewConfig::defaults(), &Overrides::default(), &env),
                "native",
                "tabline_shows"
            ),
            ("buffers".to_string(), Source::Env)
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

    /// `resolve()`'s own `surfaces` array is what `crates/view/src/main.rs`
    /// hands `SurfaceState::set_layouts` at startup, so this is that same
    /// wiring: a document's own placement and anchor words, read all the
    /// way through to the state the model and the ring answer from.
    #[test]
    fn a_configured_placement_and_anchor_reach_the_surface_state() {
        let file = ViewConfig::from_toml_str(
            "[ui.surfaces.agent]\nplacement = \"windowed\"\n\n\
             [ui.surfaces.palette]\nplacement = \"windowed\"\n\n\
             [ui.surfaces.notifications]\nanchor = \"bottom-left\"\n",
        )
        .expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert!(resolved.notices().is_empty(), "{:?}", resolved.notices());

        let from_document = resolved.surfaces;
        let mut state = view_core::native::placement::SurfaceState::default();
        state.set_layouts(from_document);
        assert!(
            state.layout(NativeSurface::Agent).windowed(),
            "[ui.surfaces.agent] placement did not open the agent panel as a window"
        );
        assert!(
            state.layout(NativeSurface::Palette).windowed(),
            "[ui.surfaces.palette] placement did not open the palette as a window"
        );
        assert_eq!(
            state.layout(NativeSurface::Notifications).anchor,
            Anchor::BottomLeft,
            "[ui.surfaces.notifications] anchor did not move the toast stack"
        );

        // the ring's `config` stop returns every surface to the placement
        // the document itself resolved to, whatever the ring did meanwhile
        // -- the fixture's own words, read independently of `state`, so the
        // comparison below can't collapse into `advance_ring`'s output
        // agreeing with itself.
        state.advance_ring(); // windowed
        state.advance_ring(); // overlay
        let config = state.advance_ring(); // config
        for (surface, placement, _) in config {
            assert_eq!(
                placement,
                from_document[surface.index()].placement,
                "{} did not return to its own [ui.surfaces.{}] placement",
                surface.id(),
                surface.id()
            );
        }
        assert!(state.layout(NativeSurface::Agent).windowed());
        assert!(state.layout(NativeSurface::Palette).windowed());
    }

    #[test]
    fn an_explicit_profile_beats_the_derivation() {
        let file = ViewConfig::from_toml_str("[keys]\nprofile = \"editor\"\n")
            .expect("the fixture must parse");
        let env = |name: &str| (name == "SSH_CONNECTION").then(|| "10.0.0.1 1 2 22".to_string());
        let resolved = resolve_with(&file, &Overrides::default(), &env);
        assert_eq!(
            resolved.profile,
            Resolved {
                value: KeyProfile::Editor,
                source: Source::File
            },
            "an explicit editor profile outranks the ssh session's own desktop derivation"
        );
        assert_eq!(resolved.profile_marker, None);
    }

    #[test]
    fn the_profile_row_reports_its_marker() {
        let file = ViewConfig::from_toml_str("").expect("an empty file must parse");
        let env = |name: &str| (name == "SSH_TTY").then(|| "/dev/pts/0".to_string());
        let resolved = resolve_with(&file, &Overrides::default(), &env);
        assert_eq!(
            row(&resolved, "keys", "profile"),
            ("desktop (SSH_TTY)".to_string(), Source::Derived)
        );
    }

    #[test]
    fn every_desktop_chord_is_a_registry_key_and_an_example_key() {
        let registry: Vec<&str> = keys()
            .iter()
            .filter(|row| row.table == "keys.desktop")
            .map(|row| row.key)
            .collect();
        for chord in chords::desktop_chords() {
            assert!(
                registry.contains(&chord.id),
                "{} has no [keys.desktop] row in the registry",
                chord.id
            );
        }
        assert_eq!(
            registry.len(),
            DESKTOP_CHORD_COUNT,
            "the registry carries a different count of [keys.desktop] rows than the chord table"
        );
    }

    #[test]
    fn a_desktop_row_takes_its_env_and_file_layers() {
        let file = ViewConfig::from_toml_str("[keys.desktop]\nfocus_left = \"<M-h>\"\n")
            .expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert_eq!(
            row(&resolved, "keys.desktop", "focus_left"),
            ("<M-h>".to_string(), Source::File)
        );
        let env =
            |name: &str| (name == "VIEW_KEYS_DESKTOP_FOCUS_LEFT").then(|| "<M-y>".to_string());
        let resolved = resolve_with(&file, &Overrides::default(), &env);
        assert_eq!(
            row(&resolved, "keys.desktop", "focus_left"),
            ("<M-y>".to_string(), Source::Env),
            "the environment outranks the file for a desktop row exactly as it does elsewhere"
        );
    }

    #[test]
    fn a_desktop_override_that_is_not_a_key_notices_and_keeps_the_default() {
        let file = ViewConfig::from_toml_str("[keys.desktop]\nfocus_left = 'has \" a quote'\n")
            .expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert_eq!(
            row(&resolved, "keys.desktop", "focus_left"),
            ("<D-Left>".to_string(), Source::Derived),
            "an unspellable override falls through to the chord's own with_super spelling"
        );
        let notices = resolved.notices().join("\n");
        for fact in ["a quote", "keys.desktop", "focus_left"] {
            assert!(notices.contains(fact), "{fact} is missing from {notices:?}");
        }
    }

    #[test]
    fn a_desktop_row_naming_no_chord_notices_and_is_ignored() {
        let file = ViewConfig::from_toml_str("[keys.desktop]\nnot_a_real_chord = \"<M-z>\"\n")
            .expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        let notices = resolved.notices().join("\n");
        assert!(
            notices.contains("not_a_real_chord"),
            "the unknown id is missing from {notices:?}"
        );
        assert_eq!(
            row(&resolved, "keys.desktop", "focus_left"),
            ("<D-Left>".to_string(), Source::Derived),
            "a sibling row this file never named is untouched by the unknown id"
        );
    }

    #[test]
    fn an_empty_desktop_row_binds_nothing() {
        let file = ViewConfig::from_toml_str("[keys.desktop]\nfocus_left = \"\"\n")
            .expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert_eq!(
            row(&resolved, "keys.desktop", "focus_left"),
            (String::new(), Source::File),
            "an empty file row is the way to leave a chord unbound, not an invalid value"
        );
        assert!(
            resolved.notices().is_empty(),
            "leaving a chord unbound owes no notice: {:?}",
            resolved.notices()
        );
    }

    #[test]
    fn a_malformed_desktop_override_notices_and_keeps_the_default() {
        let file = ViewConfig::from_toml_str("[keys.desktop]\nfocus_left = \"<D-left>\"\n")
            .expect("the fixture must parse");
        let resolved = resolve_with(&file, &Overrides::default(), &no_env);
        assert_eq!(
            row(&resolved, "keys.desktop", "focus_left"),
            ("<D-Left>".to_string(), Source::Derived),
            "<D-left> is not a key nvim will ever send; the chord's own spelling stands"
        );
        let notices = resolved.notices().join("\n");
        for fact in ["keys.desktop", "focus_left"] {
            assert!(notices.contains(fact), "{fact} is missing from {notices:?}");
        }
    }

    #[test]
    fn an_empty_env_desktop_row_binds_nothing_over_a_bound_file_row() {
        let file = ViewConfig::from_toml_str("[keys.desktop]\nfocus_left = \"<M-h>\"\n")
            .expect("the fixture must parse");
        let env = |name: &str| (name == "VIEW_KEYS_DESKTOP_FOCUS_LEFT").then(String::new);
        let resolved = resolve_with(&file, &Overrides::default(), &env);
        assert_eq!(
            row(&resolved, "keys.desktop", "focus_left"),
            (String::new(), Source::Env),
            "an empty env row unbinds the chord the same way an empty file row does, \
             outranking the file's own bound value"
        );
        assert!(
            resolved.notices().is_empty(),
            "leaving a chord unbound owes no notice: {:?}",
            resolved.notices()
        );
    }
}
