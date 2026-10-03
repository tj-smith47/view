//! The `compat [PATH]` subcommand: the plugin-compatibility scenario runner.
//!
//! A different subject from the file above it, sharing only the binary they
//! ship in. The corpus runner drives key-notation scripts through two
//! embedded engines and compares them; this drives the real `view` binary
//! over a pty against a pinned plugin fixture and asks whether the plugin
//! still works, per `view_harness::scenario`'s own schema. What they do
//! share -- the engine pin, the workspace roots, the report-then-exit-code
//! contract -- comes from `view_harness` rather than from each other.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use portable_pty::CommandBuilder;
use view_harness::fixture::{
    cache_root, copy_dir_recursive, current_engine_pin, fixtures_root, lockfile_cache_key,
    scratch_root, target_root, verify_nvim_matches_pin, workspace_root,
};
use view_harness::results::{
    today_date_string, write_results, ResultsFile, ScenarioResult, ScenarioStatus,
};
use view_harness::scenario::{self, Panes, ScenarioFile, ScenarioStateEntry};
use view_oracle::compat::{
    engine_error_reference, reset_hermetic_home, run_plugin_bootstrap, CompatSession,
    ErrorBaseline, PluginClass, ScenarioState,
};

/// Terminal size every compat scenario runs at: roomier than the
/// differential oracle's own fixed [`COLS`]x[`ROWS`] canvas, since a
/// compat scenario is driving real plugin UI (a statusline, a floating
/// picker) rather than a bare grid comparison and needs realistic room to
/// render in.
const COMPAT_COLS: u16 = 100;
const COMPAT_ROWS: u16 = 30;

/// Bound on [`CompatSession::prime_probe_channel`]/`await_probe_channel`'s
/// own bounded retry: generous relative to a `serverstart` call (the first
/// statement any committed fixture's `init.lua` runs, so this is really
/// bounding `view`'s own spawn + `ui_attach` handshake time, not any
/// plugin's), short enough that a session that never got that far still
/// fails a scenario promptly.
const PROBE_CHANNEL_TIMEOUT: Duration = Duration::from_secs(15);

/// [`CompatSession::wait_for_screen_quiescence`]'s window for a
/// fixture-less (daily-config) scenario: how long the screen must stay
/// unchanged, and the overall bound, before typing the priming command.
const SCREEN_QUIESCE_SILENCE: Duration = Duration::from_millis(500);
const SCREEN_QUIESCE_DEADLINE: Duration = Duration::from_secs(10);

/// The stricter silence bar a fixture-less scenario's steps wait behind
/// after priming. The pre-priming wait's 500ms window can latch onto a
/// mid-startup gap: a daily config keeps arranging its UI (auto-opened
/// file trees, dashboards, notification popups) in bursts separated by
/// more than that, and the priming keystrokes themselves pop up further
/// notifications the earlier wait cannot have seen. Steps typed into that
/// churn land in a window the startup then replaces, leaving the scenario
/// asserting against a grid that never shows them. Two seconds outlasts
/// the burst gaps observed with a real tree + dashboard + notifier config
/// while [`SCREEN_QUIESCE_DEADLINE`] still bounds a screen that never
/// settles (an animated dashboard), which then fails on its own merits.
const DAILY_STEPS_SILENCE: Duration = Duration::from_secs(2);

/// Disambiguates concurrently-generated scratch paths (a hermetic XDG home,
/// a probe socket) within one process, the same role
/// `view-oracle/tests/common::ScratchPaths`' own atomic counter plays.
static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Parent of every compat scenario's scratch world and probe socket; why
/// `target/` and not the system temp dir is documented on
/// [`view_harness::fixture::scratch_root`].
fn compat_scratch_root() -> PathBuf {
    scratch_root("compat-scratch")
}

/// Builds the `view` binary (always, not gated on an existence check -- see
/// `view-oracle/tests/common::view_bin_path`'s own doc comment for why a
/// stale binary is worse than one extra up-to-date `cargo build`) and
/// returns its path.
///
/// # Errors
///
/// Returns an error if `cargo build -p view` cannot be invoked or fails.
fn ensure_view_bin() -> Result<PathBuf> {
    let profile_dir = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let path = target_root().join(profile_dir).join("view");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = std::process::Command::new(&cargo)
        .args(["build", "-p", "view"])
        .status()
        .context("failed to invoke cargo build -p view")?;
    if !status.success() {
        bail!("cargo build -p view failed");
    }
    Ok(path)
}

/// Removes the scratch state a compat scenario run created once the
/// scenario finishes, on every path (success, failure, or an early `?`
/// return) via `Drop` rather than a manual cleanup call at each return
/// site. `cold_cache_dir` is only ever `Some` for a `cold_bootstrap`
/// scenario's own run-unique cache key -- the normal, shared
/// `compat/.cache/<hash>/` a warm run reuses is never touched here.
struct ScenarioScratch {
    hermetic_dir: PathBuf,
    cold_cache_dir: Option<PathBuf>,
    sock_path: PathBuf,
}

impl Drop for ScenarioScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.hermetic_dir);
        if let Some(dir) = &self.cold_cache_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

/// A fixture resolved to concrete XDG homes a `view` invocation can be
/// spawned against.
struct ReadyFixture {
    xdg_config_home: PathBuf,
    xdg_data_home: PathBuf,
    xdg_state_home: PathBuf,
    xdg_cache_home: PathBuf,
    /// The config a fixture-less (daily-config) scenario runs, set only for
    /// that scenario. Its `init.lua` is one this harness does not own, so
    /// the driver types `:call serverstart(...)` itself, and a failure to
    /// prime names this directory.
    daily_config: Option<PathBuf>,
    /// Held only for its `Drop` cleanup; never read.
    _scratch: ScenarioScratch,
}

/// [`resolve_fixture`]'s result: either a [`ReadyFixture`] to spawn `view`
/// against, or a reason to report the scenario SKIPPED without spawning
/// anything (the fixture-less scenario's reasons: see
/// [`resolve_daily_config`]).
enum FixtureResolution {
    // Boxed so the enum is not sized to this large variant next to the tiny
    // `Skipped`: on the msvc target `PathBuf` is wide enough that the four here
    // trip clippy::large_enum_variant, which `-D warnings` makes a Windows CI
    // hard error while linux stays just under the threshold.
    Ready(Box<ReadyFixture>),
    Skipped { notice: String },
}

/// The shared plugin cache key `fixture` reads, or `None` when it ships no
/// `lazy-lock.json` and so installs nothing worth sharing.
///
/// One function for both the scenario loop's own resolution and the warm
/// step ahead of it: two spellings of the same hash would let a fixture be
/// warmed into a directory no scenario ever reads, which looks exactly like
/// a warm step that works.
///
/// # Errors
///
/// Returns an error if the fixture's lockfile exists but cannot be read.
fn fixture_cache_key(fixture: &str) -> Result<Option<String>> {
    let path = fixtures_root()
        .join(fixture)
        .join("nvim")
        .join("lazy-lock.json");
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(lockfile_cache_key(&bytes)))
}

/// The plugin directory names `fixture`'s `lazy-lock.json` pins.
///
/// lazy.nvim installs each plugin into a directory named by the lockfile's
/// own key (its `name`, defaulted to the repository name -- the fixture's
/// own comment on why it declares no `name =` aliases is the other half of
/// that), so this set is exactly what a fully populated cache holds.
///
/// # Errors
///
/// Returns an error if the lockfile cannot be read or is not a JSON object.
fn fixture_lockfile_plugins(fixture: &str) -> Result<BTreeSet<String>> {
    let path = fixtures_root()
        .join(fixture)
        .join("nvim")
        .join("lazy-lock.json");
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let pinned: BTreeMap<String, serde_json::Value> =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    Ok(pinned.into_keys().collect())
}

/// Whether `cache_dir` already holds every plugin `plugins` names, as a
/// complete clone.
///
/// Read from the directories themselves rather than from a stamp file a
/// previous run wrote: a cache half-populated by an interrupted run then
/// reads as cold and is filled, where a stamp would report it warm and hand
/// the remaining clone straight back to a scenario's timed wait.
///
/// The marker is what makes "populated" mean "cloned": `git clone` creates
/// the target directory before it has fetched anything into it, so a run
/// interrupted mid-clone leaves a directory that a presence test alone
/// calls warm on every later run -- and the scenario then fails its
/// `wait_for` on a plugin that never loaded, which says nothing about the
/// plugin under test.
///
/// `.git/index` rather than `.git` itself, because `.git` appears at the
/// start of a clone and not at the end of one: an interrupted clone
/// observed here left `.git` holding `objects/`, `refs/` and `FETCH_HEAD`
/// alone, with no worktree, and a check on the directory read it as warm on
/// every later run. The index is written when the checkout that finishes
/// the clone writes the worktree, so it is present for every complete
/// install and for no partial one -- true of all 54 plugin directories in
/// this tree's four cache keys. `lazy.nvim` installs with `git clone` and
/// always checks out, so no cached plugin is ever index-less by design.
fn cache_is_warm(cache_dir: &Path, plugins: &BTreeSet<String>) -> bool {
    let installed = cache_dir.join("nvim").join("lazy");
    plugins
        .iter()
        .all(|name| clone_is_complete(&installed, name))
}

/// Whether `installed/name` is a plugin directory a clone actually
/// finished writing. See [`cache_is_warm`] for why the index is the marker.
fn clone_is_complete(installed: &Path, name: &str) -> bool {
    installed.join(name).join(".git").join("index").exists()
}

/// Removes every plugin directory under `cache_dir` that exists without the
/// clone marker [`cache_is_warm`] requires, and answers with the names it
/// took away.
///
/// The other half of that check: an interrupted clone leaves a directory
/// `Lazy! restore` treats as an installed plugin, so a re-run repairs
/// nothing until the remains are gone.
///
/// # Errors
///
/// Returns an error if a directory that fails the marker cannot be removed.
fn drop_incomplete_clones(cache_dir: &Path, plugins: &BTreeSet<String>) -> Result<Vec<String>> {
    let installed = cache_dir.join("nvim").join("lazy");
    let mut dropped = Vec::new();
    for name in plugins {
        let dir = installed.join(name);
        if !dir.exists() || clone_is_complete(&installed, name) {
            continue;
        }
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("removing the incomplete clone {}", dir.display()))?;
        dropped.push(name.clone());
    }
    Ok(dropped)
}

/// Resolves an effective `fixture` name (a state's own override, or the
/// scenario's default, or `None` for a fixture-less scenario) into a
/// [`FixtureResolution`]: XDG homes to spawn `view` against, plus a
/// [`ScenarioScratch`] guard that cleans up every scratch path this
/// function created once the caller's session finishes and the guard
/// drops. `sock_path` is threaded in (not generated here) so the caller's
/// own `CompatSession::spawn_configured` and this resolution agree on
/// exactly one socket path.
///
/// A named fixture's `XDG_CONFIG_HOME` always points at a per-run copy
/// under `hermetic_dir`, never `compat/fixtures/<name>` itself: a plugin
/// manager sourced from its own config directory can rewrite files inside
/// it in place (lazy.nvim's own lockfile, in particular), so spawning
/// `view` with the checked-in fixture tree itself as its config home would
/// leave the committed fixture modified on disk after every run.
///
/// The fixture-less arm is the one exception to "every XDG home is
/// hermetic": on a Unix host its `XDG_DATA_HOME` is the host's own data
/// home (`ambient_data_home`), so the config it runs finds the plugins
/// already installed for it.
///
/// # Errors
///
/// Returns an error if a named fixture has no `nvim/init.lua`, its
/// `lazy-lock.json` cannot be read, the fixture cannot be copied into a
/// hermetic config dir, or the fixture-less arm fails as
/// [`resolve_daily_config`] describes.
fn resolve_fixture(
    fixture: Option<&str>,
    cold_bootstrap: bool,
    sock_path: &Path,
) -> Result<FixtureResolution> {
    let scratch_id = format!(
        "{}-{}",
        std::process::id(),
        SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let hermetic_dir = compat_scratch_root().join(format!("view-compat-{scratch_id}"));
    std::fs::create_dir_all(&hermetic_dir)
        .with_context(|| format!("creating scratch dir {}", hermetic_dir.display()))?;
    let xdg_state_home = hermetic_dir.join("xdg_state_home");
    let xdg_cache_home = hermetic_dir.join("xdg_cache_home");

    match fixture {
        Some(name) => {
            let fixture_dir = fixtures_root().join(name);
            let init_lua = fixture_dir.join("nvim").join("init.lua");
            if !init_lua.exists() {
                bail!(
                    "fixture {name:?} has no {} (compat/fixtures/{name}/nvim/init.lua)",
                    init_lua.display()
                );
            }
            let xdg_data_home = match fixture_cache_key(name)? {
                Some(_) if cold_bootstrap => cache_root().join(format!("cold-{scratch_id}")),
                Some(key) => cache_root().join(key),
                None => hermetic_dir.join("xdg_data_home"),
            };
            let cold_cache_dir = cold_bootstrap.then(|| xdg_data_home.clone());

            let xdg_config_home = hermetic_dir.join("xdg_config_home");
            copy_dir_recursive(&fixture_dir, &xdg_config_home)
                .with_context(|| format!("copying fixture {name:?} into a hermetic config dir"))?;

            Ok(FixtureResolution::Ready(Box::new(ReadyFixture {
                xdg_config_home,
                xdg_data_home,
                xdg_state_home,
                xdg_cache_home,
                daily_config: None,
                _scratch: ScenarioScratch {
                    hermetic_dir,
                    cold_cache_dir,
                    sock_path: sock_path.to_path_buf(),
                },
            })))
        }
        None => resolve_daily_config(hermetic_dir, xdg_state_home, xdg_cache_home, sock_path),
    }
}

/// [`resolve_fixture`]'s fixture-less arm: links the config
/// [`daily_config_dir`] picks into a hermetic `XDG_CONFIG_HOME`, or skips
/// with its notice.
///
/// # Errors
///
/// Returns an error if `$VIEW_DAILY_CONFIG` names a directory with no
/// `init.lua`/`init.vim`, or the config link cannot be created.
#[cfg(unix)]
fn resolve_daily_config(
    hermetic_dir: PathBuf,
    xdg_state_home: PathBuf,
    xdg_cache_home: PathBuf,
    sock_path: &Path,
) -> Result<FixtureResolution> {
    let resolved = daily_config_dir().map(|config| {
        config.and_then(|path| {
            ambient_data_home().map(|data| (path, data)).ok_or_else(|| {
                "no nvim data home: neither XDG_DATA_HOME nor HOME is set; \
                 the config's plugins cannot be found"
                    .to_string()
            })
        })
    });
    let (daily_path, xdg_data_home) = match resolved {
        Ok(Ok(found)) => found,
        Ok(Err(notice)) => {
            let _ = std::fs::remove_dir_all(&hermetic_dir);
            return Ok(FixtureResolution::Skipped { notice });
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(&hermetic_dir);
            return Err(err);
        }
    };
    let xdg_config_home = hermetic_dir.join("xdg_config_home");
    std::fs::create_dir_all(&xdg_config_home)
        .with_context(|| format!("creating {}", xdg_config_home.display()))?;
    let link = xdg_config_home.join("nvim");
    std::os::unix::fs::symlink(&daily_path, &link)
        .with_context(|| format!("symlinking {} -> {}", link.display(), daily_path.display()))?;
    Ok(FixtureResolution::Ready(Box::new(ReadyFixture {
        xdg_config_home,
        xdg_data_home,
        xdg_state_home,
        xdg_cache_home,
        daily_config: Some(daily_path),
        _scratch: ScenarioScratch {
            hermetic_dir,
            cold_cache_dir: None,
            sock_path: sock_path.to_path_buf(),
        },
    })))
}

/// [`resolve_fixture`]'s fixture-less arm on a host without symlinks: the
/// config is linked into a hermetic config home, so the leg skips.
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn resolve_daily_config(
    hermetic_dir: PathBuf,
    _xdg_state_home: PathBuf,
    _xdg_cache_home: PathBuf,
    _sock_path: &Path,
) -> Result<FixtureResolution> {
    let _ = std::fs::remove_dir_all(&hermetic_dir);
    Ok(FixtureResolution::Skipped {
        notice: "the daily-config leg links the config and needs a Unix host".to_string(),
    })
}

/// The fixture-less arm's `XDG_DATA_HOME`: `$XDG_DATA_HOME` if set, else
/// `$HOME/.local/share` -- the same default nvim itself falls back to when
/// the variable is unset or empty.
///
/// Unlike every other XDG home in [`resolve_fixture`]'s fixture-less arm,
/// this one is deliberately the maintainer's *live* data home rather than a
/// fresh hermetic directory. The scenario's whole point is to exercise the
/// maintainer's actual daily-driver config, and that config is
/// lazy.nvim-managed: `stdpath("data")/lazy/lazy.nvim` is where its plugins
/// already live. A hermetic, empty data home makes lazy.nvim conclude
/// nothing is installed, so it clones lazy.nvim and the full plugin set from
/// the network at startup -- and that bootstrap holds the editor for far
/// longer than the driver's 15s prime deadline, which is waiting to type
/// `:call serverstart(...)`. The scenario would then be measuring a
/// from-scratch plugin install instead of the maintainer's editor, and would
/// time out doing it. Pointing this one home at the ambient data directory
/// lets lazy.nvim find its already-installed plugins and boot the same way
/// the maintainer's real `nvim` does.
///
/// `None` when neither variable is set, since a relative `.local/share`
/// would land under whatever directory the run started in.
#[cfg(unix)]
fn ambient_data_home() -> Option<PathBuf> {
    let set = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    match set("XDG_DATA_HOME") {
        Some(dir) => Some(PathBuf::from(dir)),
        None => set("HOME").map(|home| PathBuf::from(home).join(".local").join("share")),
    }
}

/// The config the fixture-less arm runs on, or the notice it skips with.
///
/// | `$VIEW_DAILY_CONFIG` | the config the leg runs |
/// |---|---|
/// | a directory | that directory, made absolute |
/// | `off` in any case, or empty | none: the leg skips |
/// | unset | [`ambient_config_dir`] |
///
/// `$NVIM_APPNAME` is ignored. The engine is spawned without it, so a
/// config picked by it would run against another app's data directory.
/// The path is made absolute so the link to it resolves from inside the
/// hermetic config home.
///
/// # Errors
///
/// Returns an error if `$VIEW_DAILY_CONFIG` names a directory with no
/// `init.lua`/`init.vim`, or cannot be made absolute.
#[cfg(unix)]
fn daily_config_dir() -> Result<std::result::Result<PathBuf, String>> {
    let has_init = |dir: &Path| dir.join("init.lua").exists() || dir.join("init.vim").exists();
    match std::env::var_os("VIEW_DAILY_CONFIG") {
        Some(daily) if daily.is_empty() || daily.eq_ignore_ascii_case("off") => Ok(Err(
            "VIEW_DAILY_CONFIG=off; the daily-config leg is switched off".to_string(),
        )),
        Some(daily) => {
            let path = std::path::absolute(&daily)
                .with_context(|| format!("resolving VIEW_DAILY_CONFIG={}", daily.display()))?;
            if !has_init(&path) {
                bail!(
                    "VIEW_DAILY_CONFIG={} has no init.lua/init.vim",
                    path.display()
                );
            }
            Ok(Ok(path))
        }
        None => Ok(match ambient_config_dir() {
            Some(path) if has_init(&path) => Ok(path),
            Some(path) => Err(format!(
                "no nvim config at {}; set VIEW_DAILY_CONFIG to name one",
                path.display()
            )),
            None => Err("no nvim config: neither XDG_CONFIG_HOME nor HOME is set; \
                         set VIEW_DAILY_CONFIG to name one"
                .to_string()),
        }),
    }
}

/// The config directory the engine loads: `$XDG_CONFIG_HOME/nvim`, else
/// `$HOME/.config/nvim`. `$NVIM_APPNAME` is ignored because the engine is
/// spawned without it. `None` when neither home is set, since there is
/// then no config directory to name.
#[cfg(unix)]
fn ambient_config_dir() -> Option<PathBuf> {
    let set = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    match set("XDG_CONFIG_HOME") {
        Some(dir) => Some(PathBuf::from(dir).join("nvim")),
        None => set("HOME").map(|home| PathBuf::from(home).join(".config").join("nvim")),
    }
}

/// The detail of a fixture-less scenario that never opened its probe
/// channel: the cause, what happened to the config it ran, and how to
/// switch the leg off.
///
/// `exit_code` is the engine's exit code when it had already exited, and
/// `screen` is what it last drew, read for nvim's own startup error
/// report.
fn priming_failure_detail(
    cause: &str,
    exit_code: Option<u32>,
    screen: &str,
    config: &Path,
) -> String {
    let config = config.display();
    let errored = screen.lines().map(str::trim).find(|line| {
        line.starts_with("Error detected while processing")
            || line.starts_with("Error in ")
            || nvim_error_code(line)
    });
    match (exit_code, errored) {
        (Some(code), _) => format!(
            "{cause}; nvim exited with status {code} while starting the config \
             at {config}. Set VIEW_DAILY_CONFIG=off to skip this leg"
        ),
        (None, Some(line)) => format!(
            "{cause}; the config at {config} errored while starting: {line}. \
             Fix it, or set VIEW_DAILY_CONFIG=off"
        ),
        (None, None) => format!(
            "{cause}; the config at {config} did not finish starting (a plugin \
             manager installing on first run does this). Run nvim once under \
             it, or set VIEW_DAILY_CONFIG=off"
        ),
    }
}

/// Whether `line` opens with one of nvim's numbered error codes (`E5113:`).
fn nvim_error_code(line: &str) -> bool {
    line.strip_prefix('E')
        .and_then(|rest| rest.split_once(':'))
        .is_some_and(|(digits, _)| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn class_str(class: PluginClass) -> &'static str {
    match class {
        PluginClass::Semantic => "semantic",
        PluginClass::UiAdjacent => "ui-adjacent",
        PluginClass::UiOwning => "ui-owning",
    }
}

/// Best-effort plugin commit lookup from a named fixture's `lazy-lock.json`,
/// for [`ScenarioResult::plugin_version`]'s row in the design spec's own
/// compat-evidence schema ("plugin, version, engine pin, ..."). Tries
/// `plugin` as a literal lockfile key first (a plugin spec'd without
/// lazy.nvim's default `<repo>.nvim` naming), then with a `.nvim`
/// suffix (lazy.nvim's own default when a spec sets no custom `name`),
/// then with a `.lua` suffix (the other repo-naming convention in the
/// committed `heavy` fixture: `nvim-tree.lua`). Returns `None`
/// (never an error) for a fixture-less scenario, a fixture with no
/// lockfile, or a plugin name the lockfile does not contain -- a missing
/// version is a gap in the report, not a reason to fail the scenario that
/// already passed or failed on its own merits. Takes the *effective*
/// fixture (a state's own override, if any, else the scenario's default),
/// since a `native-only` state's plugin-free fixture legitimately has no
/// entry for `plugin` and must report that gap rather than the base
/// fixture's unrelated version.
fn resolve_plugin_version(plugin: &str, fixture: Option<&str>) -> Option<String> {
    let name = fixture?;
    let lockfile_path = fixtures_root()
        .join(name)
        .join("nvim")
        .join("lazy-lock.json");
    let text = std::fs::read_to_string(lockfile_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let obj = json.as_object()?;
    // candidates probed in a fixed preference order (exact name first, then
    // the common repo-naming suffixes) so that a lockfile holding more than
    // one candidate key for a plugin resolves by intent, not map iteration
    // order
    let suffixed_nvim = format!("{plugin}.nvim");
    let suffixed_lua = format!("{plugin}.lua");
    let key = [plugin, &suffixed_nvim, &suffixed_lua]
        .into_iter()
        .find(|candidate| obj.contains_key(*candidate))?;
    let commit = obj.get(key)?.get("commit")?.as_str()?;
    Some(commit.get(..7).unwrap_or(commit).to_string())
}

/// Builds a [`ScenarioResult`] for one `(scenario, state)` pair that never
/// spawned a session (SKIPPED) or whose session failed before or during a
/// step (`failing_step`/`detail` set; `failing_step == Some(state.steps.len())`
/// means the implicit zero-error epilogue is what failed, not a scripted
/// step). Shared by every non-OK exit path in [`run_scenario`] and
/// [`command`]'s own top-level `Err` catch, so the report row shape
/// is defined exactly once.
/// The "what happened" half of a [`ScenarioResult`], grouped into one type
/// so [`scenario_result`] takes a single outcome value instead of four
/// separate trailing parameters that only ever travel together (clippy's
/// `too_many_arguments` floor is 7; identity -- which scenario, which
/// state, which pin -- and outcome are the two things a call site actually
/// reasons about separately, so the split follows that seam).
struct ScenarioOutcome {
    status: ScenarioStatus,
    failing_step: Option<usize>,
    detail: Option<String>,
    elapsed_ms: u128,
}

fn scenario_result(
    scenario_path: &Path,
    scenario: &ScenarioFile,
    state: &ScenarioStateEntry,
    pin: &str,
    outcome: ScenarioOutcome,
) -> ScenarioResult {
    let effective_fixture = state.fixture.as_deref().or(scenario.fixture.as_deref());
    ScenarioResult {
        scenario_path: scenario_path.display().to_string(),
        plugin: scenario.plugin.clone(),
        plugin_version: resolve_plugin_version(&scenario.plugin, effective_fixture),
        class: class_str(scenario.class).to_string(),
        fixture: effective_fixture.map(str::to_string),
        state: state.label(),
        panes: state.panes.as_str().to_string(),
        engine_pin: pin.to_string(),
        status: outcome.status,
        failing_step: outcome.failing_step,
        steps_total: state.steps.len(),
        detail: outcome.detail,
        elapsed_ms: outcome.elapsed_ms,
        date: today_date_string(),
    }
}

/// The binary under test: the `view` build a scenario spawns as its
/// session.
///
/// The two sides carry distinct types so that a call transposing them
/// cannot compile. Both are paths, and a scenario driven with the sides
/// swapped runs bare nvim as the session and hands `view` to it as the
/// engine to embed: the run still spawns, still settles and still reports,
/// with the reference side of the differential recorded as the side under
/// test and every PARITY line describing nvim against itself.
#[derive(Debug, Clone, Copy)]
struct ViewBin<'a>(&'a Path);

/// The pinned engine binary a scenario's session embeds, and the reference
/// side of the differential. See [`ViewBin`] for why the sides are separate
/// types.
#[derive(Debug, Clone, Copy)]
struct NvimBin<'a>(&'a Path);

/// Renders `native` as the `[native]` table body of a `view.toml`: an empty
/// map renders as a bare `[native]` header, matching `NativeConfig`'s own
/// "absent/empty table means every feature stays on" default, so a
/// `superseded` state's `native = {}` and a `native-only` state that omits
/// `native` entirely both materialize into the same all-enabled config the
/// real shipping default resolves to.
fn render_native_toml(native: &BTreeMap<String, bool>) -> String {
    let mut out = String::from("[native]\n");
    for (key, value) in native {
        out.push_str(&format!("{key} = {value}\n"));
    }
    out
}

/// The `view.toml` `[native]` body [`run_scenario`] should write for
/// `state`, or `None` if it should write nothing at all and leave the
/// fixture's own copied `view/view.toml` (already placed by
/// [`resolve_fixture`]'s directory copy) standing as-is.
///
/// Only a `present` state that declares no `native` table takes the `None`
/// path. Every `present`-named scenario file in `compat/scenarios/`
/// declares exactly this shape (no `native` key at all), and the three
/// fixtures they run against each commit their own `[native]` table
/// switching off every feature that only decides who renders a surface --
/// that committed table, not the all-enabled default
/// below, is what a `present` state has always evidenced. A `superseded`/
/// `deferred`/`native-only` state that omits `native` keeps its
/// longstanding meaning instead: a bare `[native]` header, i.e. every
/// feature on, since those three states exist specifically to assert a
/// supersession outcome under an explicit or all-enabled config, never to
/// evidence a fixture's own ambient one.
fn native_toml_override(state: &ScenarioStateEntry) -> Option<String> {
    match (&state.native, state.name) {
        (None, ScenarioState::Present) => None,
        (Some(native), _) => Some(render_native_toml(native)),
        (None, _) => Some(render_native_toml(&BTreeMap::new())),
    }
}

/// The fixture's own `view.toml` with its `[native]` table replaced by
/// `native` (a body [`render_native_toml`] produced) and every other table
/// kept.
///
/// A state's `native` override is about who draws each surface. The
/// fixture's `[ui] panes = "nvim"` is what keeps the screen nvim's own
/// window picture, the one every cell a scenario reads was written
/// against. Writing the override over the whole file dropped that line,
/// and those states ran under the shipped tiles.
fn with_native_table(fixture_toml: &str, native: &str) -> Result<String> {
    let mut table: toml::Table = fixture_toml
        .parse()
        .context("parsing the fixture's view.toml")?;
    table.remove("native");
    let kept = toml::to_string(&table).context("serializing the fixture's other tables")?;
    Ok(format!("{kept}\n{native}"))
}

/// Merges `native` into the fixture `view.toml` at `view_config_path` and
/// writes the result back.
///
/// A missing or unreadable fixture file used to merge as an empty string, so
/// the `[native]` override alone reached the scenario and the fixture's
/// `[ui] panes = "nvim"` was lost -- the same bug [`with_native_table`]'s own
/// doc comment describes, reintroduced one line earlier. Every compat
/// fixture ships a `view.toml`, so a missing one here fails loudly, naming
/// the path.
fn write_native_override(view_config_path: &Path, native: &str) -> Result<()> {
    let fixture_toml = std::fs::read_to_string(view_config_path)
        .with_context(|| format!("reading {}", view_config_path.display()))?;
    let merged = with_native_table(&fixture_toml, native)
        .with_context(|| format!("merging [native] into {}", view_config_path.display()))?;
    std::fs::write(view_config_path, merged)
        .with_context(|| format!("writing {}", view_config_path.display()))
}

/// The environment variable a fixture's `init.lua` reads to decide whether
/// to apply its accommodations. Rides the same path [`run_scenario`] already
/// proves reaches the nvim grandchild with `VIEW_COMPAT_SOCK`: set on the
/// `view` builder, kept by `make_hermetic`'s sweep (which drops a name only
/// while the builder still holds the host's own value for it, and the host
/// exports neither), inherited by the engine nvim spawns.
const ACCOMMODATIONS_VAR: &str = "VIEW_COMPAT_ACCOMMODATIONS";

/// The value [`ACCOMMODATIONS_VAR`] should carry for `state`, or `None` to
/// leave the variable unset entirely.
///
/// Unset is the accommodating case rather than an explicit `"1"`: a fixture
/// reached outside this runner (a hand-run `nvim -u`, a bisect) then behaves
/// exactly as it did before the switch existed, and only a state that
/// actually declared `accommodations = false` changes what the fixture does.
/// Keyed on the field, never on the state's name -- see
/// `scenario::RawState`'s own doc comment for why.
fn accommodations_env(state: &ScenarioStateEntry) -> Option<&'static str> {
    if state.accommodations {
        None
    } else {
        Some("0")
    }
}

/// `home` with `-reference` appended: the reference leg's own copy of one of
/// the session's XDG homes, a sibling inside the same hermetic directory so
/// the scenario's existing scratch cleanup reclaims it too.
fn reference_sibling(home: &Path) -> PathBuf {
    let mut name = home.file_name().unwrap_or_default().to_os_string();
    name.push("-reference");
    home.with_file_name(name)
}

/// The [`ErrorBaseline`] the zero-error epilogue should subtract for
/// `state`, or `None` for the strict, subtract-nothing form.
///
/// The seam is `accommodations`, not the state's name: a state running a
/// config this harness adapted has a real guarantee that startup is
/// error-free, and asserting it outright is what makes that guarantee
/// worth something. A state running the user's own configuration has no
/// such guarantee to assert -- so its epilogue takes the pinned engine's
/// own reading of that same configuration as its reference and fails only
/// on what `view` added to it.
fn epilogue_reference(
    state: &ScenarioStateEntry,
    ready: &ReadyFixture,
    nvim_bin: NvimBin<'_>,
    sock_path: &Path,
) -> Result<Option<ErrorBaseline>, view_oracle::compat::CompatError> {
    if state.accommodations {
        return Ok(None);
    }
    // the fixture's first statement is a serverstart on this name; the
    // reference run is never a probe client, but the call must still have
    // somewhere to land, and a name of its own keeps it off the socket the
    // session under test is about to open
    let mut cmd = fixture_nvim_command(
        nvim_bin,
        ready,
        &sock_path.with_extension("reference"),
        accommodations_env(state),
    );
    // The same config and the same plugin cache the session under test
    // reads, so the two legs answer the same question -- but its own state
    // and cache homes: the reference runs first, and a shared state home
    // would let it write the shada and plugin-manager state that make the
    // session's own launch no longer the first one this config ever had.
    cmd.env("XDG_STATE_HOME", reference_sibling(&ready.xdg_state_home));
    cmd.env("XDG_CACHE_HOME", reference_sibling(&ready.xdg_cache_home));
    engine_error_reference(cmd).map(Some)
}

/// One `nvim` invocation against a resolved fixture: the config and plugin
/// homes `ready` names, the socket the fixture's own first statement calls
/// `serverstart` on, and the accommodation switch.
///
/// The one place this file starts an engine of its own, so the epilogue's
/// reference leg and the cache warm-up below it cannot drift into two
/// different environments for the same fixture.
fn fixture_nvim_command(
    nvim_bin: NvimBin<'_>,
    ready: &ReadyFixture,
    sock_path: &Path,
    accommodations: Option<&'static str>,
) -> std::process::Command {
    let NvimBin(nvim_bin) = nvim_bin;
    let mut cmd = std::process::Command::new(nvim_bin);
    cmd.env("XDG_CONFIG_HOME", &ready.xdg_config_home);
    cmd.env("XDG_DATA_HOME", &ready.xdg_data_home);
    cmd.env("XDG_STATE_HOME", &ready.xdg_state_home);
    cmd.env("XDG_CACHE_HOME", &ready.xdg_cache_home);
    cmd.env("VIEW_COMPAT_SOCK", sock_path);
    if let Some(value) = accommodations {
        cmd.env(ACCOMMODATIONS_VAR, value);
    }
    cmd.current_dir(ready.xdg_config_home.join("nvim"));
    cmd
}

/// Bound on one cache key's warm run: generous, because it covers a
/// from-nothing clone of every plugin a fixture pins -- the same work the
/// cold-bootstrap scenario bounds from inside a session with three 60 s
/// waits of its own -- and finite, so an unreachable remote fails the run
/// instead of hanging it before the first scenario.
const WARM_CACHE_TIMEOUT: Duration = Duration::from_secs(300);

/// Every shared plugin cache key the scenario loop will read, mapped to the
/// fixture names that key it.
///
/// A `cold_bootstrap` scenario is deliberately absent: its cache key is
/// run-unique, and paying the clone inside its own waits is the whole
/// measurement that scenario exists to take.
///
/// # Errors
///
/// Returns an error if a named fixture's lockfile cannot be read.
fn warm_cache_targets(
    scenarios: &[(PathBuf, ScenarioFile)],
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut targets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (_, scenario) in scenarios {
        if scenario.cold_bootstrap {
            continue;
        }
        for state in &scenario.states {
            let Some(name) = state.fixture.as_deref().or(scenario.fixture.as_deref()) else {
                continue;
            };
            if let Some(key) = fixture_cache_key(name)? {
                targets.entry(key).or_default().insert(name.to_string());
            }
        }
    }
    Ok(targets)
}

/// Populates every shared plugin cache the scenario loop will read, before
/// that loop starts timing anything.
///
/// A scenario's `wait_for` is sized for a plugin doing its work, never for
/// git fetching that plugin: the first scenario to reach a cold shared
/// cache otherwise pays the whole install inside its own timed wait and
/// fails on a deadline that has nothing to say about the plugin under test
/// -- an install has been observed running several times the wait it was
/// charged to.
///
/// # Errors
///
/// Returns an error if a fixture cannot be resolved or its lockfile read,
/// the bootstrap engine cannot be spawned, it exits nonzero or outlives
/// [`WARM_CACHE_TIMEOUT`], or the cache is still incomplete afterwards.
fn warm_plugin_caches(scenarios: &[(PathBuf, ScenarioFile)], nvim_bin: NvimBin<'_>) -> Result<()> {
    for (key, fixtures) in warm_cache_targets(scenarios)? {
        let named = fixtures.iter().cloned().collect::<Vec<_>>().join(", ");
        // every fixture sharing a key hashed the same lockfile bytes, so
        // any one of them names the same plugin set
        let Some(fixture) = fixtures.iter().next() else {
            continue;
        };
        let plugins = fixture_lockfile_plugins(fixture)?;
        let cache_dir = cache_root().join(&key);
        if cache_is_warm(&cache_dir, &plugins) {
            println!("compat: plugin cache {key} for {named} ... already warm");
            continue;
        }
        println!("compat: warming plugin cache {key} for {named} ...");
        let start = Instant::now();
        warm_one_cache(fixture, &cache_dir, &plugins, nvim_bin)?;
        if !cache_is_warm(&cache_dir, &plugins) {
            bail!(
                "the plugin bootstrap for fixture {fixture:?} reported success but \
                 {} still does not hold every plugin its lockfile pins",
                cache_dir.display()
            );
        }
        println!(
            "compat: warming plugin cache {key} for {named} ... done ({:.1}s)",
            start.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

/// Runs one fixture's plugin install headless, into the shared cache
/// directory its own lockfile keys.
///
/// `Lazy! restore` rather than a bare startup: lazy.nvim installs what is
/// missing during startup either way, but the bang makes this run wait for
/// that install and for the lockfile checkout behind it, instead of
/// quitting out from under a clone still in flight.
///
/// What a previous run left half-cloned is removed first
/// ([`drop_incomplete_clones`]): lazy.nvim reads a directory as an
/// installed plugin, so a restore over the remains of an interrupted clone
/// repairs nothing.
///
/// # Errors
///
/// Returns an error if an incomplete clone cannot be removed, the fixture
/// does not resolve, or the bootstrap engine fails.
fn warm_one_cache(
    fixture: &str,
    cache_dir: &Path,
    plugins: &BTreeSet<String>,
    nvim_bin: NvimBin<'_>,
) -> Result<()> {
    for name in drop_incomplete_clones(cache_dir, plugins)? {
        println!("compat: dropping the incomplete clone {name} before re-cloning it");
    }
    let sock_path = compat_scratch_root().join(format!(
        "view-compat-warm-{}-{}.sock",
        std::process::id(),
        SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let FixtureResolution::Ready(ready) = resolve_fixture(Some(fixture), false, &sock_path)? else {
        bail!("fixture {fixture:?} did not resolve for its own cache warm-up");
    };
    let mut cmd = fixture_nvim_command(nvim_bin, &ready, &sock_path, None);
    cmd.arg("--headless")
        .arg("-c")
        .arg("Lazy! restore")
        .arg("-c")
        .arg("qa!");
    let output = run_plugin_bootstrap(cmd, WARM_CACHE_TIMEOUT)
        .with_context(|| format!("warming the plugin cache for fixture {fixture:?}"))?;
    if !output.status.success() {
        bail!(
            "the plugin bootstrap for fixture {fixture:?} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Drives one `(scenario, state)` pair end to end: resolves the state's
/// effective fixture, materializes its `[native]` table into a hermetic
/// `view.toml` and spawns `view --config <that path>` against it (the same
/// real shipping config flag a user invokes, not a test-only backdoor),
/// opens the probe channel, runs every step in order, then the implicit
/// zero-error epilogue, and always
/// attempts a clean `:qa!` shutdown regardless of outcome. Never propagates
/// a step/probe failure as an `Err` -- those become a
/// [`ScenarioStatus::Failed`] result, the same tolerance `run_tokens`'s own
/// callers apply to a corpus entry's failure, so one state's wedge cannot
/// abort the whole compat run. Only a resolution failure that means no
/// session could even be attempted (a missing fixture, an unreadable
/// lockfile, an unwritable hermetic config dir) surfaces as `Err`.
///
/// # Errors
///
/// Returns an error if the state's effective fixture cannot be resolved, or
/// its materialized `view.toml` cannot be written.
fn run_scenario(
    scenario_path: &Path,
    scenario: &ScenarioFile,
    state: &ScenarioStateEntry,
    pin: &str,
    view_bin: ViewBin<'_>,
    nvim_bin: NvimBin<'_>,
) -> Result<ScenarioResult> {
    let ViewBin(view_bin) = view_bin;
    let NvimBin(nvim_bin) = nvim_bin;
    let start = Instant::now();
    let sock_path = compat_scratch_root().join(format!(
        "view-compat-{}-{}.sock",
        std::process::id(),
        SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let effective_fixture = state.fixture.as_deref().or(scenario.fixture.as_deref());
    let ready = match resolve_fixture(effective_fixture, scenario.cold_bootstrap, &sock_path)? {
        FixtureResolution::Ready(ready) => ready,
        FixtureResolution::Skipped { notice } => {
            return Ok(scenario_result(
                scenario_path,
                scenario,
                state,
                pin,
                ScenarioOutcome {
                    status: ScenarioStatus::Skipped,
                    failing_step: None,
                    detail: Some(notice),
                    elapsed_ms: 0,
                },
            ));
        }
    };

    // The fixture-less arm has no fixture whose accommodations a state could
    // decline, and it baselines against the session's own startup instead of
    // a reference leg -- so a state pairing the two would carry the
    // differential epilogue's name with none of its evidence. The scenario
    // loader rejects that pairing outright; this arm is what keeps the two
    // halves from drifting apart quietly if it ever stops.
    if ready.daily_config.is_some() && !state.accommodations {
        return Ok(scenario_result(
            scenario_path,
            scenario,
            state,
            pin,
            ScenarioOutcome {
                status: ScenarioStatus::Failed,
                failing_step: None,
                detail: Some(
                    "a fixture-less scenario cannot decline accommodations: there is no \
                     reference leg to subtract, and self-baselining would answer the \
                     question with the session under test"
                        .to_string(),
                ),
                elapsed_ms: start.elapsed().as_millis(),
            },
        ));
    }
    // Before the session under test starts, so an engine that cannot even
    // read this config is reported as that, rather than as whatever the
    // session then fails on.
    let reference = match epilogue_reference(state, &ready, NvimBin(nvim_bin), &sock_path) {
        Ok(reference) => reference,
        Err(err) => {
            return Ok(scenario_result(
                scenario_path,
                scenario,
                state,
                pin,
                ScenarioOutcome {
                    status: ScenarioStatus::Failed,
                    failing_step: None,
                    detail: Some(err.to_string()),
                    elapsed_ms: start.elapsed().as_millis(),
                },
            ));
        }
    };
    let subtracted = reference
        .as_ref()
        .map(ErrorBaseline::subtracted_errors)
        .unwrap_or_default();

    let view_config_path = ready.xdg_config_home.join("view").join("view.toml");
    if let Some(rendered) = native_toml_override(state) {
        if let Some(parent) = view_config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        write_native_override(&view_config_path, &rendered)?;
    }

    let mut cmd = CommandBuilder::new(view_bin);
    cmd.env("XDG_CONFIG_HOME", &ready.xdg_config_home);
    cmd.env("XDG_DATA_HOME", &ready.xdg_data_home);
    cmd.env("XDG_STATE_HOME", &ready.xdg_state_home);
    cmd.env("XDG_CACHE_HOME", &ready.xdg_cache_home);
    cmd.env("VIEW_COMPAT_SOCK", &sock_path);
    // set on every row, nvim included: the fixture-less daily config's own
    // view.toml may choose tiles, and a row records the layout it ran under
    cmd.env("VIEW_UI_PANES", state.panes.as_str());
    // the one host variable a launch's own wire trace needs, allow-listed
    // here because `make_hermetic` sweeps `VIEW_LOG` along with everything
    // else: the routing evidence in docs/toast-routing-wire-capture.md was
    // read out of a file this produced, and a trace nobody can re-obtain is
    // evidence that expires. One file per (plugin, state, panes), so the
    // states of one run do not interleave into something unreadable
    if let Some(dir) = std::env::var_os("VIEW_COMPAT_LOG") {
        let dir = std::path::PathBuf::from(dir);
        if std::fs::create_dir_all(&dir).is_ok() {
            cmd.env(
                "VIEW_LOG",
                dir.join(format!(
                    "{}-{}{}.log",
                    scenario.plugin,
                    state.label().replace('/', "-"),
                    match state.panes {
                        Panes::Nvim => "",
                        Panes::Tiles => "-tiles",
                    }
                )),
            );
        }
    }
    if let Some(value) = accommodations_env(state) {
        cmd.env(ACCOMMODATIONS_VAR, value);
    }
    cmd.arg("--config");
    cmd.arg(&view_config_path);
    // view's own process cwd seeds `model.cwd` (see main.rs), which the
    // native tree sidebar opens rooted on; nvim, spawned as its child,
    // inherits the same cwd for its own ambient directory. Pinning both to
    // the fixture's nvim config dir here is what actually makes a
    // scenario's rendered file listing hermetic -- a bare `:cd` in a
    // scenario's own steps only ever moved nvim's side of that pair.
    cmd.cwd(ready.xdg_config_home.join("nvim"));

    let mut session = match CompatSession::spawn_configured(
        cmd,
        COMPAT_COLS,
        COMPAT_ROWS,
        nvim_bin.to_path_buf(),
        sock_path.clone(),
    ) {
        Ok(session) => session,
        Err(err) => {
            return Ok(scenario_result(
                scenario_path,
                scenario,
                state,
                pin,
                ScenarioOutcome {
                    status: ScenarioStatus::Failed,
                    failing_step: None,
                    detail: Some(err.to_string()),
                    elapsed_ms: start.elapsed().as_millis(),
                },
            ));
        }
    };

    let channel_result = if let Some(config) = &ready.daily_config {
        // Best-effort settle before typing the priming command: a daily
        // config's own startup content is unknown to this harness (see
        // wait_for_screen_quiescence's own doc comment), so an unsettled
        // screen here does not itself abort the scenario -- the priming
        // retry loop right below is what actually confirms success.
        let _ = session.wait_for_screen_quiescence(SCREEN_QUIESCE_SILENCE, SCREEN_QUIESCE_DEADLINE);
        session
            .prime_probe_channel(PROBE_CHANNEL_TIMEOUT)
            .map_err(|err| {
                let screen = session.pty().screen();
                let exit_code = session
                    .pty()
                    .wait_for_exit(Duration::ZERO)
                    .map(|status| status.exit_code());
                priming_failure_detail(&err.to_string(), exit_code, &screen, config)
            })
            .and_then(|()| {
                // Settle again, and behind a stricter bar, before any step
                // types: see DAILY_STEPS_SILENCE for the startup-burst race
                // this closes. The error baseline is captured after that
                // settle so the config's own startup noise lands inside it
                // and only what the steps add can fail the epilogue.
                let _ = session
                    .wait_for_screen_quiescence(DAILY_STEPS_SILENCE, SCREEN_QUIESCE_DEADLINE);
                session.error_baseline().map_err(|err| err.to_string())
            })
    } else {
        session
            .await_probe_channel(PROBE_CHANNEL_TIMEOUT)
            .map(|()| reference.unwrap_or_default())
            .map_err(|err| err.to_string())
    };
    let baseline = match channel_result {
        Ok(baseline) => baseline,
        Err(detail) => {
            // kill alone only requests termination; reaping (bounded, matching
            // PtySession::wait_for_exit's own kill-then-wait standard) is what
            // keeps a channel-failure exit from leaving a zombie entry in the
            // process table for the rest of this run
            session.pty().kill();
            let _ = session.pty().wait_for_exit(Duration::from_secs(2));
            return Ok(scenario_result(
                scenario_path,
                scenario,
                state,
                pin,
                ScenarioOutcome {
                    status: ScenarioStatus::Failed,
                    failing_step: None,
                    detail: Some(detail),
                    elapsed_ms: start.elapsed().as_millis(),
                },
            ));
        }
    };

    let mut failing_step = None;
    let mut detail = None;
    for (index, step) in state.steps.iter().enumerate() {
        if let Err(err) = session.drive_step(step) {
            failing_step = Some(index);
            detail = Some(err.to_string());
            // The report line names the step and the needle; what it cannot
            // carry is the 100x30 grid the needle was missing from, which is
            // usually the whole diagnosis (an overlay standing over the cells
            // the assertion reads, rather than the state never arriving).
            // Behind an opt-in name so an ordinary gate run keeps its
            // one-line-per-row shape.
            if std::env::var_os("VIEW_COMPAT_DUMP").is_some() {
                eprintln!("--- screen at failure ---\n{}\n---", session.pty().screen());
            }
            break;
        }
    }
    if failing_step.is_none() {
        if let Err(err) = session.zero_error_check_since(&baseline) {
            failing_step = Some(state.steps.len());
            detail = Some(err.to_string());
        }
    }

    // Best-effort clean shutdown regardless of outcome, so a scenario never
    // leaves a `view` process running past its own run; failures here are
    // not this scenario's own result (a session that already failed a step
    // may well fail to reach a cmdline prompt to type `:qa!` into).
    let _ = session.pty().send(b"\x1b:qa!\r");
    let _ = session.pty().wait_for_exit(Duration::from_secs(5));

    // The fixture-less arm just sourced the maintainer's live config, whose
    // startup tooling may write entries under the shared hermetic home that
    // the next spawn's preparation rightly refuses (a Go toolchain invoked
    // by a plugin manager creates $HOME/go, for one observed case). The
    // home holds nothing durable by contract, so restoring it by deletion
    // keeps one maintainer-config scenario from vetoing every spawn after
    // it, in this run and the next. A failed reset is loud twice: here, and
    // in the refusal the next spawn raises against the leftover entry.
    if ready.daily_config.is_some() {
        if let Err(err) = reset_hermetic_home() {
            eprintln!(
                "compat: resetting the hermetic home after the fixture-less scenario failed: {err}"
            );
        }
    }

    let status = if failing_step.is_some() {
        ScenarioStatus::Failed
    } else {
        ScenarioStatus::Ok
    };
    // A passing row that only passed because the engine's own reading of
    // this config excused something has to say so, in the report and in the
    // evidence file: a subtraction nobody can see is the suppression this
    // whole state exists to end, moved one layer down.
    if status == ScenarioStatus::Ok && !subtracted.is_empty() {
        let note = format!("engine-noise subtracted: {}", subtracted.join("; "));
        // appended, never assigned: whatever else a row has to say about
        // itself is the row's own evidence too, and a note that overwrites
        // it trades one visible fact for another
        detail = Some(match detail {
            Some(existing) => format!("{existing}; {note}"),
            None => note,
        });
    }
    Ok(scenario_result(
        scenario_path,
        scenario,
        state,
        pin,
        ScenarioOutcome {
            status,
            failing_step,
            detail,
            elapsed_ms: start.elapsed().as_millis(),
        },
    ))
}

/// Resolves `path` into a sorted list of `(file path, parsed scenario)`
/// pairs, mirroring [`collect_entries`]'s own non-recursive directory walk.
fn collect_scenarios(path: &Path) -> Result<Vec<(PathBuf, ScenarioFile)>> {
    let mut files: Vec<PathBuf> = if path.is_dir() {
        std::fs::read_dir(path)
            .with_context(|| format!("reading scenario directory {}", path.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    files.sort();

    files
        .into_iter()
        .map(|path| {
            let scenario = scenario::load_file(&path)
                .with_context(|| format!("loading scenario {}", path.display()))?;
            Ok((path, scenario))
        })
        .collect()
}

/// The `(scenario stem, state, clearing task, what clearing it means)` rows
/// this suite expects to be red, each with the task whose landing turns it
/// green.
///
/// Empty, and that is a state this list is meant to reach rather than a
/// disabled mechanism: every scenario the suite carries now passes on its
/// own. The reconciliation below is still exercised, against a manifest the
/// tests own rather than this one, so an empty list here cannot make those
/// legs pass by reconciling nothing (`apply_red_expectation_over`).
///
/// A row here is not a waiver. Red-and-listed reports distinctly and does
/// not fail the run; green-and-listed is a hard failure, so the task that
/// turns a row green has to remove it in the same commit, and red-and-
/// unlisted fails as it always did. What the list may never hold is a row
/// red only because the engine makes the same noise on its own -- that is
/// [`epilogue_reference`]'s job, and a row parked here instead would be
/// exactly the suppression the unaccommodated state exists to end.
///
/// The fourth field is what a reader of the published evidence page needs:
/// the row's cell there says what has to become true for it to go green, in
/// terms of the product, since the plan marker beside it means nothing
/// outside this repo's own planning notes.
const EXPECTED_RED: [RedRow; 0] = [];

/// One [`EXPECTED_RED`] row: scenario stem, state, the layout it runs under
/// (`nvim` or `tiles`), the task that clears it, and what a reader of the
/// evidence page is told has to become true.
type RedRow = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

/// The scenario file's own stem -- not `plugin`, since two scenario files
/// can name the same plugin (`lualine.toml` and `cold-bootstrap.toml` both
/// do), which would make them indistinguishable to the report and to
/// [`EXPECTED_RED`] alike.
fn scenario_stem(result: &ScenarioResult) -> &str {
    Path::new(&result.scenario_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(result.scenario_path.as_str())
}

/// What has to become true for `result`'s row to go green, if
/// [`EXPECTED_RED`] names it -- the row's own words, not the plan marker
/// beside them, since these strings are stamped into an evidence page read
/// outside this repo's planning notes.
fn expected_red_clears_when(result: &ScenarioResult, manifest: &[RedRow]) -> Option<&'static str> {
    let stem = scenario_stem(result);
    manifest
        .iter()
        .find(|(scenario, state, panes, _, _)| {
            *scenario == stem && *state == result.state && *panes == result.panes
        })
        .map(|(_, _, _, _, clears_when)| *clears_when)
}

/// Reconciles one row against [`EXPECTED_RED`], in place: a listed red
/// becomes [`ScenarioStatus::ExpectedFailure`] carrying what clears it, and
/// a listed row that passed becomes a [`ScenarioStatus::Failed`] naming the
/// manifest as the thing to fix. Everything else is left exactly as the run
/// reported it.
fn apply_red_expectation(result: &mut ScenarioResult) {
    apply_red_expectation_over(result, &EXPECTED_RED);
}

/// [`apply_red_expectation`] against an arbitrary manifest, so the
/// reconciliation tests own a row of their own instead of borrowing
/// whichever one [`EXPECTED_RED`] happens to hold: a task that clears the
/// last row would otherwise leave every positive leg passing vacuously,
/// having reconciled nothing.
fn apply_red_expectation_over(result: &mut ScenarioResult, manifest: &[RedRow]) {
    let Some(clears_when) = expected_red_clears_when(result, manifest) else {
        return;
    };
    match result.status {
        ScenarioStatus::Failed => {
            result.status = ScenarioStatus::ExpectedFailure;
            result.detail = Some(format!(
                "expected red until {clears_when}: {}",
                result.detail.as_deref().unwrap_or("unknown failure")
            ));
        }
        ScenarioStatus::Ok => {
            result.status = ScenarioStatus::Failed;
            result.detail = Some(format!(
                "listed in EXPECTED_RED as red until {clears_when}, but this run passed; drop \
                 the row from EXPECTED_RED in the commit that turned it green"
            ));
        }
        ScenarioStatus::Skipped => {
            // A manifest row has to name a state that actually runs. Honoring
            // a skip would let a row survive its own scenario becoming
            // unrunnable, and the manifest would then be asserting a red
            // nothing has produced in months.
            result.status = ScenarioStatus::Failed;
            result.detail = Some(format!(
                "listed in EXPECTED_RED as red until {clears_when}, but this run skipped it \
                 ({}); a manifest row must name a state that runs",
                result.detail.as_deref().unwrap_or("no reason given")
            ));
        }
        ScenarioStatus::ExpectedFailure => {}
    }
}

/// The name of the step a failed row stopped at: its index, `epilogue` for
/// the zero-error check after the last step, and for a row that failed
/// before any step ran, the real-config priming or the fixture's startup.
fn step_label(result: &ScenarioResult) -> String {
    match result.failing_step {
        Some(index) if index == result.steps_total => "epilogue".to_string(),
        Some(index) => index.to_string(),
        None if result.fixture.is_none() => "real-config priming".to_string(),
        None => "startup".to_string(),
    }
}

/// Prints one scenario's report line in a fixed shape:
///
/// ```text
/// compat: lualine (heavy, present, nvim) ... OK (4 steps, 2.1s)
/// ```
fn print_scenario_result(result: &ScenarioResult) {
    let fixture = result.fixture.as_deref().unwrap_or("none");
    let place = format!("{fixture}, {}, {}", result.state, result.panes);
    // the scenario file's own stem, not result.plugin: more than one
    // scenario file can share a plugin name (lualine.toml and
    // cold-bootstrap.toml are both "lualine"), which would otherwise make
    // the two indistinguishable in the report
    let scenario = Path::new(&result.scenario_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(result.scenario_path.as_str());
    let secs = result.elapsed_ms as f64 / 1000.0;
    match result.status {
        ScenarioStatus::Ok => println!(
            "compat: {scenario} ({place}) ... OK ({} steps, {secs:.1}s){}",
            result.steps_total,
            result
                .detail
                .as_deref()
                .map_or_else(String::new, |detail| format!(" [{detail}]"))
        ),
        ScenarioStatus::Failed => {
            let step_label = step_label(result);
            println!(
                "compat: {scenario} ({place}) ... FAILED at step {step_label} ({} steps total, {secs:.1}s): {}",
                result.steps_total,
                result.detail.as_deref().unwrap_or("unknown failure")
            );
        }
        ScenarioStatus::ExpectedFailure => {
            let step_label = step_label(result);
            println!(
                "compat: {scenario} ({place}) ... RED-AS-EXPECTED at step {step_label} ({} steps total, {secs:.1}s): {}",
                result.steps_total,
                result.detail.as_deref().unwrap_or("unknown failure")
            );
        }
        ScenarioStatus::Skipped => println!(
            "compat: {scenario} ({place}) ... SKIPPED: {}",
            result.detail.as_deref().unwrap_or("")
        ),
    }
}

/// The `compat [PATH]` subcommand: every scenario under `path` (default
/// `compat/scenarios`), reported per [`print_scenario_result`] and written
/// to `compat/results.json` for the `page` subcommand to render.
///
/// Every shared plugin cache the run will read is filled by
/// [`warm_plugin_caches`] first, so no scenario's timed wait carries a
/// clone. The cache lives at `compat/.cache/` unless
/// `view_harness::fixture::CACHE_ROOT_ENV` (`VIEW_COMPAT_CACHE_ROOT`)
/// names another directory -- which is how a run observes a cold cache
/// without emptying the one every other session on this tree shares.
/// Exit code: 0 unless at least one scenario reports
/// [`ScenarioStatus::Failed`] -- a SKIPPED scenario (no daily config on
/// this host, the expected state in CI) does not fail the run, since there
/// is no daily config on that host for the scenario to actually exercise,
/// and neither does a row [`EXPECTED_RED`] names, which reports
/// RED-AS-EXPECTED with the task that clears it.
///
/// # Errors
///
/// Returns an error if no scenario files are found under `path`, a
/// scenario file fails to parse, `.engine-pin` cannot be read, the `nvim`
/// on `PATH` does not report the version `.engine-pin` names, `view`
/// cannot be built, or `compat/results.json` cannot be written.
pub(crate) fn command(path: &Path) -> Result<()> {
    let scenarios = collect_scenarios(path)?;
    if scenarios.is_empty() {
        bail!(
            "no scenario files found under {} (expected *.toml files)",
            path.display()
        );
    }

    let pin = current_engine_pin()?;
    let nvim_bin = PathBuf::from("nvim");
    verify_nvim_matches_pin(&nvim_bin, &pin)?;
    let view_bin = ensure_view_bin()?;
    std::fs::create_dir_all(cache_root()).context("creating compat/.cache")?;

    // Drop-based cleanup is skipped when a run is killed by a signal, so
    // stale scratch worlds from interrupted runs would otherwise pile up
    // silently. Clearing the whole parent is safe because concurrent
    // compat runs are already out of contract: both would rewrite
    // compat/results.json wholesale, clobbering each other's evidence.
    let _ = std::fs::remove_dir_all(compat_scratch_root());
    std::fs::create_dir_all(compat_scratch_root())
        .with_context(|| format!("creating scratch root {}", compat_scratch_root().display()))?;

    warm_plugin_caches(&scenarios, NvimBin(&nvim_bin))?;

    let mut results = ResultsFile::default();
    let mut any_failed = false;
    for (scenario_path, scenario) in &scenarios {
        for state in &scenario.states {
            let mut result = match run_scenario(
                scenario_path,
                scenario,
                state,
                &pin,
                ViewBin(&view_bin),
                NvimBin(&nvim_bin),
            ) {
                Ok(result) => result,
                Err(err) => scenario_result(
                    scenario_path,
                    scenario,
                    state,
                    &pin,
                    ScenarioOutcome {
                        status: ScenarioStatus::Failed,
                        failing_step: None,
                        detail: Some(err.to_string()),
                        elapsed_ms: 0,
                    },
                ),
            };
            apply_red_expectation(&mut result);
            print_scenario_result(&result);
            if result.status == ScenarioStatus::Failed {
                any_failed = true;
            }
            results.results.push(result);
        }
    }

    write_results(
        &workspace_root().join("compat").join("results.json"),
        &results,
    )
    .context("writing compat/results.json")?;

    if any_failed {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // the env-mutation sites below are the ones ENV_MUTATION_LOCK exists to
    // bound; each holds the guard across its own restore
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::disallowed_methods,
        clippy::panic
    )]
    use super::*;
    #[cfg(unix)]
    use view_test_support::ScratchDir;
    /// The committed heavy fixture pins nvim-tree under its real repo name
    /// `nvim-tree.lua`, while the scenario names the plugin `nvim-tree`:
    /// the lockfile lookup must bridge the `.lua` repo-naming convention
    /// the same way it bridges lazy.nvim's default `.nvim` suffix, or the
    /// evidence page's version cell goes blank for a plugin the lockfile
    /// does pin.
    #[test]
    fn plugin_version_resolves_lua_suffixed_lockfile_key() {
        let version = resolve_plugin_version("nvim-tree", Some("heavy"));
        assert_eq!(
            version.as_deref(),
            Some("4213bd6"),
            "heavy fixture's lazy-lock.json pins nvim-tree.lua at 4213bd6..."
        );
    }

    /// Pins the fix for the silent flip a runner bug once introduced: a
    /// `present`-named state that declares no `[native]` table must
    /// materialize its fixture's own committed `view/view.toml` verbatim,
    /// not the all-enabled default `render_native_toml` falls back to for
    /// every other omitted case. `native_toml_override` returning `None`
    /// is the mechanism -- `run_scenario` then never touches the file
    /// `resolve_fixture`'s directory copy already placed -- so this test
    /// exercises both halves together: the `None` return, and that the
    /// file `resolve_fixture` actually leaves behind is the fixture's own,
    /// read from the same committed source a hand-duplicated constant
    /// could silently drift from.
    #[test]
    fn present_state_with_no_native_table_leaves_the_fixtures_own_view_toml_untouched() {
        let state = ScenarioStateEntry {
            name: ScenarioState::Present,
            native: None,
            fixture: None,
            accommodations: true,
            panes: Panes::Nvim,
            variant: None,
            steps: Vec::new(),
        };
        assert_eq!(
            native_toml_override(&state),
            None,
            "a present state with no native table must not override view.toml at all"
        );

        let committed = std::fs::read_to_string(
            fixtures_root()
                .join("minimal")
                .join("view")
                .join("view.toml"),
        )
        .expect("committed minimal fixture must carry view/view.toml");

        let sock_path = compat_scratch_root().join(format!(
            "view-harness-oracle-test-native-override-{}.sock",
            std::process::id()
        ));
        let resolved =
            resolve_fixture(Some("minimal"), false, &sock_path).expect("minimal fixture resolves");
        let FixtureResolution::Ready(ready) = resolved else {
            panic!("minimal fixture must resolve to Ready, never Skipped");
        };
        let materialized =
            std::fs::read_to_string(ready.xdg_config_home.join("view").join("view.toml"))
                .expect("resolve_fixture's own directory copy must have placed view.toml");

        assert_eq!(
            materialized, committed,
            "with no run_scenario write, the copied view.toml must still read exactly what \
             compat/fixtures/minimal/view/view.toml commits"
        );
    }

    fn red_row(scenario: &str, state: &str, status: ScenarioStatus) -> ScenarioResult {
        ScenarioResult {
            scenario_path: format!("compat/scenarios/{scenario}.toml"),
            plugin: scenario.to_string(),
            plugin_version: None,
            class: "ui-owning".to_string(),
            fixture: Some("heavy".to_string()),
            state: state.to_string(),
            panes: "nvim".to_string(),
            engine_pin: "v0.0.0".to_string(),
            status,
            failing_step: Some(2),
            steps_total: 8,
            detail: Some("some failure".to_string()),
            elapsed_ms: 1,
            date: "2026-08-31".to_string(),
        }
    }

    /// The manifest the three reconciliation tests below reconcile against:
    /// their own, never the shipped [`EXPECTED_RED`]. Borrowing the shipped
    /// one couples them to whichever rows happen to be listed -- they fail
    /// when a task clears the row they named (which is how they failed on
    /// the commit that turned `noice`/`deferred` green), and once the last
    /// row clears they would have no subject at all and pass having
    /// asserted nothing.
    const SYNTHETIC: [RedRow; 1] = [(
        "smoke-minimal",
        "present",
        "nvim",
        "T0",
        "the fixture this row invents is fixed",
    )];

    #[test]
    fn a_listed_red_row_reports_as_expected_and_names_its_clearing_task() {
        let (scenario, state, _, _, clears_when) = SYNTHETIC[0];
        let mut result = red_row(scenario, state, ScenarioStatus::Failed);
        apply_red_expectation_over(&mut result, &SYNTHETIC);
        assert_eq!(result.status, ScenarioStatus::ExpectedFailure);
        let detail = result.detail.expect("an expected-red row keeps its detail");
        assert!(
            detail.contains(clears_when) && detail.contains("some failure"),
            "the row must say what clears it and keep the original failure: {detail}"
        );
    }

    #[test]
    fn a_listed_row_that_passes_fails_the_run_as_a_stale_manifest() {
        // The half that keeps the manifest from becoming a permanent
        // waiver: a row nobody has to remove is a failure nobody has to fix.
        let (scenario, state, _, _, _) = SYNTHETIC[0];
        let mut result = red_row(scenario, state, ScenarioStatus::Ok);
        apply_red_expectation_over(&mut result, &SYNTHETIC);
        assert_eq!(result.status, ScenarioStatus::Failed);
        assert!(result
            .detail
            .expect("a stale-manifest row must explain itself")
            .contains("EXPECTED_RED"));
    }

    #[test]
    fn a_listed_row_that_skips_fails_rather_than_counting_as_its_own_red() {
        // The row asserts something about a run; a state that did not run
        // asserts nothing, and honoring the skip would let the manifest
        // outlive the scenario it names.
        let (scenario, state, _, _, _) = SYNTHETIC[0];
        let mut result = red_row(scenario, state, ScenarioStatus::Skipped);
        let notice = "VIEW_DAILY_CONFIG=off; the daily-config leg is switched off";
        result.detail = Some(notice.to_string());
        apply_red_expectation_over(&mut result, &SYNTHETIC);
        assert_eq!(result.status, ScenarioStatus::Failed);
        let detail = result
            .detail
            .expect("a skipped listed row must explain itself");
        assert!(
            detail.contains("skipped") && detail.contains(notice),
            "the row must say it skipped and keep the skip's own reason: {detail}"
        );
    }

    #[test]
    fn an_unlisted_red_row_still_fails() {
        let mut result = red_row("lualine", "unaccommodated", ScenarioStatus::Failed);
        apply_red_expectation(&mut result);
        assert_eq!(result.status, ScenarioStatus::Failed);
        assert_eq!(result.detail.as_deref(), Some("some failure"));
    }

    /// A manifest may only name rows that exist: a scenario renamed or a
    /// state dropped would otherwise leave an entry that excuses nothing
    /// and that no run can ever contradict.
    #[test]
    fn every_expected_red_row_names_a_state_that_actually_exists() {
        for (scenario, state, panes, task, _) in EXPECTED_RED {
            let path = workspace_root()
                .join("compat")
                .join("scenarios")
                .join(format!("{scenario}.toml"));
            let loaded = scenario::load_file(&path)
                .unwrap_or_else(|err| panic!("EXPECTED_RED names {scenario}.toml: {err}"));
            assert!(
                loaded
                    .states
                    .iter()
                    .any(|entry| entry.label() == state && entry.panes.as_str() == panes),
                "EXPECTED_RED names {scenario}/{state} under {panes} (clears with {task}), which \
                 the scenario file does not declare"
            );
        }
    }

    #[test]
    fn a_listed_nvim_row_leaves_its_tiles_twin_to_fail_on_its_own() {
        let (scenario, state, _, _, _) = SYNTHETIC[0];
        let mut result = red_row(scenario, state, ScenarioStatus::Failed);
        result.panes = "tiles".to_string();
        apply_red_expectation_over(&mut result, &SYNTHETIC);
        assert_eq!(result.status, ScenarioStatus::Failed);
        assert_eq!(result.detail.as_deref(), Some("some failure"));
    }

    /// States that run under nvim's own window layout only, each with the
    /// reason its subject does not survive the tiled layout: it reads the
    /// statusline row nvim's bar owns or a flag the tiled layout sets on its
    /// own.
    const NVIM_ONLY: [(&str, &str, &str); 7] = [
        ("lualine", "deferred", "reads the statusline row's cells"),
        (
            "lualine",
            "deferred/all-off",
            "reads the statusline row's cells",
        ),
        ("lualine", "superseded", "reads the statusline row's cells"),
        (
            "lualine",
            "native-only",
            "reads the statusline's scroll indicator; a tile frame carries its own ruler",
        ),
        (
            "lualine",
            "unaccommodated",
            "reads the statusline row's cells",
        ),
        (
            "noice",
            "deferred",
            "probes the ext_tabline flag the tiled layout sets",
        ),
        (
            "smoke-minimal",
            "native-only",
            "probes the ext_tabline flag the tiled layout sets",
        ),
    ];

    /// Every nvim state either has a tiles twin or is listed in
    /// [`NVIM_ONLY`] with its reason, and every listed row still names an
    /// nvim state with no twin, so a new scenario state has to answer the
    /// question and a stale row fails.
    #[test]
    fn every_nvim_state_has_a_tiles_twin_or_a_reason_it_has_none() {
        let dir = workspace_root().join("compat").join("scenarios");
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|err| panic!("reading {}: {err}", dir.display()))
            .map(|entry| entry.expect("scenario dir entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        entries.sort();
        let mut untwinned = BTreeSet::new();
        for path in &entries {
            let stem = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("scenario file stem")
                .to_string();
            let loaded = scenario::load_file(path)
                .unwrap_or_else(|err| panic!("loading {}: {err}", path.display()));
            for entry in &loaded.states {
                let has_twin = loaded.states.iter().any(|other| {
                    other.panes == Panes::Tiles
                        && other.name == entry.name
                        && other.variant == entry.variant
                });
                if entry.panes == Panes::Nvim && !has_twin {
                    untwinned.insert((stem.clone(), entry.label()));
                }
            }
        }
        let listed: BTreeSet<(String, String)> = NVIM_ONLY
            .iter()
            .map(|(stem, state, _)| ((*stem).to_string(), (*state).to_string()))
            .collect();
        let unexplained: Vec<_> = untwinned.difference(&listed).collect();
        let stale: Vec<_> = listed.difference(&untwinned).collect();
        assert!(
            unexplained.is_empty(),
            "these nvim states have no tiles twin and no NVIM_ONLY row: {unexplained:?}"
        );
        assert!(
            stale.is_empty(),
            "these NVIM_ONLY rows name no untwinned nvim state: {stale:?}"
        );
    }

    /// A plugin directory is warm only as a complete clone, and the warm
    /// step takes away what fails that before cloning again.
    ///
    /// The directory `git clone` creates before it has fetched anything is
    /// the shape this exists for: read as installed, it is warm forever and
    /// the failure surfaces a minute later as a scenario `wait_for` timeout
    /// naming a plugin that never loaded.
    #[cfg(unix)]
    #[test]
    fn a_half_cloned_plugin_is_cold_and_is_taken_away_before_the_re_clone() {
        let root = ScratchDir::new("harness-compat-half-clone").unwrap();
        let cache_dir = root.join("2895d052d37fc12c");
        let lazy = cache_dir.join("nvim").join("lazy");
        let plugins: BTreeSet<String> = ["dressing.nvim".to_string()].into_iter().collect();

        std::fs::create_dir_all(lazy.join("dressing.nvim").join("lua")).unwrap();
        assert!(
            !cache_is_warm(&cache_dir, &plugins),
            "a directory with no clone marker is what an interrupted clone \
             leaves behind, and it is not a plugin"
        );

        assert_eq!(
            drop_incomplete_clones(&cache_dir, &plugins).unwrap(),
            vec!["dressing.nvim".to_string()],
            "the warm step has to take the remains away, or the re-clone \
             finds a directory lazy.nvim reads as installed"
        );
        assert!(!lazy.join("dressing.nvim").exists());

        // the shape an interrupted clone actually leaves: `.git` is there,
        // and nothing a checkout would have written is
        std::fs::create_dir_all(lazy.join("dressing.nvim").join(".git").join("objects")).unwrap();
        assert!(
            !cache_is_warm(&cache_dir, &plugins),
            "a clone that got as far as .git and no further is not an \
             installed plugin"
        );
        assert_eq!(
            drop_incomplete_clones(&cache_dir, &plugins).unwrap(),
            vec!["dressing.nvim".to_string()],
        );

        std::fs::create_dir_all(lazy.join("dressing.nvim").join(".git")).unwrap();
        std::fs::write(lazy.join("dressing.nvim").join(".git").join("index"), "x").unwrap();
        assert!(
            cache_is_warm(&cache_dir, &plugins),
            "a finished checkout writes the index, and nothing an \
             interrupted clone leaves does"
        );
        assert!(
            drop_incomplete_clones(&cache_dir, &plugins)
                .unwrap()
                .is_empty(),
            "a warm cache must survive the sweep untouched: removing it \
             would re-clone every plugin on every run"
        );
    }

    /// The warm step fills exactly the shared cache keys the scenario loop
    /// goes on to read, so a fixture added to a scenario cannot arrive
    /// unwarmed and pay its clone inside a timed wait.
    ///
    /// The expected set is derived from each scenario's own effective
    /// fixture and that fixture's lockfile bytes rather than from
    /// `fixture_cache_key`, so this is a second reading of the same
    /// question and not a restatement of the first.
    #[test]
    fn the_warm_step_fills_every_shared_cache_key_the_scenario_loop_reads() {
        let dir = workspace_root().join("compat").join("scenarios");
        let scenarios = collect_scenarios(&dir).expect("the committed scenarios must load");

        let mut expected: BTreeSet<String> = BTreeSet::new();
        for (_, scenario) in &scenarios {
            if scenario.cold_bootstrap {
                continue;
            }
            for state in &scenario.states {
                let Some(name) = state.fixture.as_deref().or(scenario.fixture.as_deref()) else {
                    continue;
                };
                let lockfile = fixtures_root()
                    .join(name)
                    .join("nvim")
                    .join("lazy-lock.json");
                if let Ok(bytes) = std::fs::read(&lockfile) {
                    expected.insert(lockfile_cache_key(&bytes));
                }
            }
        }

        let warmed: BTreeSet<String> = warm_cache_targets(&scenarios)
            .expect("every committed fixture's lockfile must be readable")
            .into_keys()
            .collect();

        assert!(
            !expected.is_empty(),
            "the committed scenarios must name at least one lockfile-keyed fixture, or this \
             pin proves nothing"
        );
        assert_eq!(
            warmed, expected,
            "a shared plugin cache key the scenario loop resolves is not one the warm step fills"
        );

        let cold: Vec<(PathBuf, ScenarioFile)> = collect_scenarios(&dir)
            .expect("the committed scenarios must load")
            .into_iter()
            .filter(|(_, scenario)| scenario.cold_bootstrap)
            .collect();
        assert!(
            !cold.is_empty(),
            "the mandatory cold-bootstrap scenario must exist for its exclusion to mean anything"
        );
        assert!(
            warm_cache_targets(&cold)
                .expect("the cold scenario's fixture lockfile must be readable")
                .is_empty(),
            "a cold-bootstrap scenario must contribute no warm key: its cache is run-unique and \
             paying the clone is the measurement"
        );
    }

    #[test]
    fn an_unaccommodated_state_clears_the_accommodation_env() {
        // Named `superseded` on purpose: the switch must follow the field,
        // not the state's name, or `deferred` could never prove a remedy
        // works on a config nobody pre-adjusted.
        let declining = ScenarioStateEntry {
            name: ScenarioState::Superseded,
            native: None,
            fixture: None,
            accommodations: false,
            panes: Panes::Nvim,
            variant: None,
            steps: Vec::new(),
        };
        assert_eq!(accommodations_env(&declining), Some("0"));
    }

    #[test]
    fn a_state_that_asks_for_accommodations_keeps_them() {
        // Likewise named for the opposite side of the same point.
        let accommodating = ScenarioStateEntry {
            name: ScenarioState::Unaccommodated,
            native: None,
            fixture: None,
            accommodations: true,
            panes: Panes::Nvim,
            variant: None,
            steps: Vec::new(),
        };
        assert_eq!(
            accommodations_env(&accommodating),
            None,
            "the variable must stay unset, so a fixture run outside this runner is unchanged"
        );
    }

    /// Every other state -- `superseded`/`deferred`/`native-only`, and even
    /// a hypothetical `present` state that does set `native` -- keeps its
    /// longstanding materialization: an explicit table renders as given,
    /// and an omitted one (only reachable for a non-`present` state) still
    /// renders the bare, all-enabled `[native]` header.
    #[test]
    fn non_present_states_keep_the_established_native_rendering() {
        let mut explicit = BTreeMap::new();
        explicit.insert("tree".to_string(), false);
        let with_table = ScenarioStateEntry {
            name: ScenarioState::Deferred,
            native: Some(explicit.clone()),
            fixture: None,
            accommodations: true,
            panes: Panes::Nvim,
            variant: None,
            steps: Vec::new(),
        };
        assert_eq!(
            native_toml_override(&with_table),
            Some(render_native_toml(&explicit))
        );

        let omitted = ScenarioStateEntry {
            name: ScenarioState::Superseded,
            native: None,
            fixture: None,
            accommodations: true,
            panes: Panes::Nvim,
            variant: None,
            steps: Vec::new(),
        };
        assert_eq!(
            native_toml_override(&omitted),
            Some(render_native_toml(&BTreeMap::new())),
            "an omitted native table on a non-present state must still render the \
             all-enabled bare [native] header"
        );
    }

    /// Serializes every test in this module that calls `std::env::set_var`/
    /// `remove_var` on `VIEW_DAILY_CONFIG`, `XDG_DATA_HOME`, or `HOME`.
    /// `cargo test` runs a module's tests on multiple threads by default,
    /// and these tests set and then restore the *same* process-global
    /// names, so two of them overlapping would interleave one's restore
    /// with another's plant and leave the loser reading a value it never
    /// set. The lock is held for the whole body, restore included, because
    /// releasing it between the mutation and the restore is what opens
    /// that window.
    static ENV_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// [`ENV_MUTATION_LOCK`], with poisoning ignored: it orders two
    /// operations and guards no data, so a test that panicked while
    /// holding it left nothing behind for the next one to find broken.
    fn env_mutation_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTATION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Plants (or clears) one environment variable and restores whatever it
    /// held beforehand when dropped -- including when the drop happens
    /// during an unwinding panic (a failed `assert_eq!` partway through a
    /// test's body), so a failing assertion still leaves the next test to
    /// acquire [`ENV_MUTATION_LOCK`] with a clean environment instead of
    /// compounding one failure into an unrelated one downstream.
    struct EnvRestore {
        name: &'static str,
        prior: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn set(name: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let prior = std::env::var_os(name);
            std::env::set_var(name, value);
            Self { name, prior }
        }

        #[cfg(unix)]
        fn unset(name: &'static str) -> Self {
            let prior = std::env::var_os(name);
            std::env::remove_var(name);
            Self { name, prior }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var(self.name, v),
                None => std::env::remove_var(self.name),
            }
        }
    }

    /// A scratch daily config directory with a bare `init.lua`, real enough
    /// to pass [`resolve_fixture`]'s "has `init.lua`/`init.vim`" check
    /// without needing an actual lazy.nvim-managed config on the test host.
    #[cfg(unix)]
    fn scratch_daily_config(label: &str) -> ScratchDir {
        let dir = ScratchDir::new(&format!("harness-oracle-daily-config-{label}"))
            .expect("failed to create scratch daily config dir");
        std::fs::write(dir.join("init.lua"), "").expect("failed to write scratch init.lua");
        dir
    }

    /// Pins the requirement this scenario exists for: with `XDG_DATA_HOME`
    /// set, the fixture-less arm must hand `view` that ambient data home
    /// rather than a fresh hermetic one, or a lazy.nvim-managed daily
    /// config finds no already-installed plugins and re-bootstraps from
    /// the network, outrunning the driver's prime deadline.
    ///
    /// Unix-only: every other host skips the fixture-less arm.
    #[cfg(unix)]
    #[test]
    fn resolve_fixture_fixture_less_arm_uses_ambient_xdg_data_home() {
        let _guard = env_mutation_guard();

        let daily_dir = scratch_daily_config("env-set");
        let names = ScratchDir::new("harness-oracle-daily-names")
            .expect("a directory to name the two paths under");
        let ambient_data = names.join("data-home");
        let _daily_env = EnvRestore::set("VIEW_DAILY_CONFIG", daily_dir.path());
        let _data_home_env = EnvRestore::set("XDG_DATA_HOME", &ambient_data);

        let sock_path = names.join("daily.sock");
        let resolution =
            resolve_fixture(None, false, &sock_path).expect("resolve_fixture must not error");
        let ready = match resolution {
            FixtureResolution::Ready(ready) => Some(ready),
            FixtureResolution::Skipped { .. } => None,
        }
        .expect("expected a Ready resolution with VIEW_DAILY_CONFIG set, not Skipped");
        assert_eq!(
            ready.xdg_data_home, ambient_data,
            "the fixture-less arm must pass through $XDG_DATA_HOME, not a hermetic directory"
        );

        let _ = std::fs::remove_dir_all(&daily_dir);
    }

    /// The fallback half of the same requirement: with `XDG_DATA_HOME`
    /// unset, [`ambient_data_home`] must derive `$HOME/.local/share` --
    /// nvim's own default -- rather than leaving the daily-config
    /// scenario with no ambient home to point at.
    #[cfg(unix)]
    #[test]
    fn ambient_data_home_falls_back_to_home_dot_local_share_when_xdg_unset() {
        let _guard = env_mutation_guard();
        let _data_home_env = EnvRestore::unset("XDG_DATA_HOME");
        let _home_env = EnvRestore::set("HOME", "/home/daily-config-test");

        assert_eq!(
            ambient_data_home(),
            Some(PathBuf::from("/home/daily-config-test/.local/share"))
        );
    }

    /// With neither `XDG_DATA_HOME` nor `HOME` set there is no data home to
    /// name, and the leg skips saying so.
    #[cfg(unix)]
    #[test]
    fn no_data_home_skips_saying_neither_is_set() {
        let _guard = env_mutation_guard();
        let config = scratch_daily_config("no-data-home");
        let _daily_env = EnvRestore::set("VIEW_DAILY_CONFIG", config.path());
        let _data_home_env = EnvRestore::unset("XDG_DATA_HOME");
        let _home_env = EnvRestore::unset("HOME");
        assert_eq!(ambient_data_home(), None);
        assert_eq!(
            fixture_less_notice(&config.join("daily.sock")).as_deref(),
            Some(
                "no nvim data home: neither XDG_DATA_HOME nor HOME is set; \
                 the config's plugins cannot be found"
            )
        );
    }

    /// Resolves the fixture-less arm and returns its skip notice, or `None`
    /// when it came back `Ready`.
    fn fixture_less_notice(sock_path: &Path) -> Option<String> {
        match resolve_fixture(None, false, sock_path).expect("resolve_fixture must not error") {
            FixtureResolution::Ready(_) => None,
            FixtureResolution::Skipped { notice } => Some(notice),
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_ambient_config_is_the_default_when_view_daily_config_is_unset() {
        let _guard = env_mutation_guard();
        let root = ScratchDir::new("harness-oracle-ambient-config").expect("scratch dir");
        std::fs::create_dir_all(root.join("nvim")).expect("config dir");
        std::fs::write(root.join("nvim").join("init.lua"), "").expect("init.lua");
        let _daily_env = EnvRestore::unset("VIEW_DAILY_CONFIG");
        let _app_env = EnvRestore::unset("NVIM_APPNAME");
        let _config_env = EnvRestore::set("XDG_CONFIG_HOME", root.path());

        let resolution = resolve_fixture(None, false, &root.join("daily.sock"))
            .expect("resolve_fixture must not error");
        let FixtureResolution::Ready(ready) = resolution else {
            panic!("an ambient config with an init.lua must resolve Ready");
        };
        assert_eq!(
            std::fs::read_link(ready.xdg_config_home.join("nvim")).expect("a linked config"),
            root.join("nvim"),
            "the hermetic config home must link the ambient config"
        );
    }

    #[cfg(unix)]
    #[test]
    fn no_ambient_config_skips_naming_the_path_it_read() {
        let _guard = env_mutation_guard();
        let root = ScratchDir::new("harness-oracle-no-ambient-config").expect("scratch dir");
        let _daily_env = EnvRestore::unset("VIEW_DAILY_CONFIG");
        let _app_env = EnvRestore::unset("NVIM_APPNAME");
        let _config_env = EnvRestore::set("XDG_CONFIG_HOME", root.path());

        assert_eq!(
            fixture_less_notice(&root.join("daily.sock")),
            Some(format!(
                "no nvim config at {}; set VIEW_DAILY_CONFIG to name one",
                root.join("nvim").display()
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn view_daily_config_off_skips() {
        let _guard = env_mutation_guard();
        let root = ScratchDir::new("harness-oracle-daily-config-off").expect("scratch dir");
        std::fs::create_dir_all(root.join("nvim")).expect("config dir");
        std::fs::write(root.join("nvim").join("init.lua"), "").expect("init.lua");
        let _app_env = EnvRestore::unset("NVIM_APPNAME");
        let _config_env = EnvRestore::set("XDG_CONFIG_HOME", root.path());
        for value in ["off", "", "OFF", "Off"] {
            let _daily_env = EnvRestore::set("VIEW_DAILY_CONFIG", value);
            assert_eq!(
                fixture_less_notice(&root.join("daily.sock")).as_deref(),
                Some("VIEW_DAILY_CONFIG=off; the daily-config leg is switched off"),
                "VIEW_DAILY_CONFIG={value:?} must switch the leg off over an ambient config"
            );
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn a_host_without_symlinks_skips_the_daily_config_leg() {
        let _guard = env_mutation_guard();
        let _daily_env = EnvRestore::set("VIEW_DAILY_CONFIG", "no-such-config");
        assert_eq!(
            fixture_less_notice(Path::new("daily.sock")).as_deref(),
            Some("the daily-config leg links the config and needs a Unix host")
        );
    }

    #[cfg(unix)]
    #[test]
    fn nvim_appname_leaves_the_ambient_config_dir_alone() {
        let _guard = env_mutation_guard();
        let _config_env = EnvRestore::set("XDG_CONFIG_HOME", "/xdg-config");
        let _home_env = EnvRestore::set("HOME", "/home/daily-config-test");
        let _app_env = EnvRestore::unset("NVIM_APPNAME");
        assert_eq!(
            ambient_config_dir(),
            Some(PathBuf::from("/xdg-config/nvim"))
        );

        // the engine is spawned without NVIM_APPNAME, so the leg ignores it
        let _app_env = EnvRestore::set("NVIM_APPNAME", "work");
        assert_eq!(
            ambient_config_dir(),
            Some(PathBuf::from("/xdg-config/nvim"))
        );

        let _config_env = EnvRestore::unset("XDG_CONFIG_HOME");
        assert_eq!(
            ambient_config_dir(),
            Some(PathBuf::from("/home/daily-config-test/.config/nvim"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn no_home_and_no_xdg_config_home_skips_saying_neither_is_set() {
        let _guard = env_mutation_guard();
        let names = ScratchDir::new("harness-oracle-no-homes").expect("scratch dir");
        let _daily_env = EnvRestore::unset("VIEW_DAILY_CONFIG");
        let _config_env = EnvRestore::unset("XDG_CONFIG_HOME");
        let _home_env = EnvRestore::unset("HOME");
        assert_eq!(
            fixture_less_notice(&names.join("daily.sock")).as_deref(),
            Some(
                "no nvim config: neither XDG_CONFIG_HOME nor HOME is set; \
                 set VIEW_DAILY_CONFIG to name one"
            )
        );
    }

    /// A relative `VIEW_DAILY_CONFIG` is read against the current
    /// directory, so the link in the hermetic config home has to carry the
    /// absolute path: a relative link target resolves against the link's
    /// own directory and dangles.
    #[cfg(unix)]
    #[test]
    fn a_relative_view_daily_config_is_linked_by_its_absolute_path() {
        let _guard = env_mutation_guard();
        let config = scratch_daily_config("relative");
        let cwd = std::env::current_dir().expect("cwd");
        let ups = cwd.components().count().saturating_sub(1);
        let relative: PathBuf = std::iter::repeat_n(Path::new(".."), ups)
            .collect::<PathBuf>()
            .join(
                config
                    .path()
                    .strip_prefix("/")
                    .expect("an absolute scratch dir"),
            );
        assert!(relative.is_relative());
        let _daily_env = EnvRestore::set("VIEW_DAILY_CONFIG", &relative);

        let resolution = resolve_fixture(None, false, &config.join("daily.sock"))
            .expect("resolve_fixture must not error");
        let FixtureResolution::Ready(ready) = resolution else {
            panic!("a relative path to a config with an init.lua must resolve Ready");
        };
        let link = ready.xdg_config_home.join("nvim");
        assert!(
            std::fs::read_link(&link)
                .expect("a linked config")
                .is_absolute(),
            "the link must carry an absolute target"
        );
        assert!(
            link.join("init.lua").exists(),
            "the link must reach the config"
        );
    }

    #[test]
    fn a_config_that_never_primes_names_itself_and_the_off_switch() {
        let cause =
            view_oracle::compat::CompatError::ProbeChannelNeverOpened(PROBE_CHANNEL_TIMEOUT)
                .to_string();
        let config = Path::new("/home/me/.config/nvim");
        assert_eq!(
            priming_failure_detail(&cause, None, "~\n~\n", config),
            "probe channel never opened within 15s of priming; the config at \
             /home/me/.config/nvim did not finish starting (a plugin manager \
             installing on first run does this). Run nvim once under it, or set \
             VIEW_DAILY_CONFIG=off"
        );
    }

    /// A priming failure says what happened: the engine exited, or the
    /// config reported an error on screen.
    #[test]
    fn a_priming_failure_says_what_happened() {
        let config = Path::new("/c");
        assert_eq!(
            priming_failure_detail("x", Some(1), "", config),
            "x; nvim exited with status 1 while starting the config at /c. \
             Set VIEW_DAILY_CONFIG=off to skip this leg"
        );
        let screen = "~\nError detected while processing /c/init.lua:\nE5113: boom\n";
        assert_eq!(
            priming_failure_detail("x", None, screen, config),
            "x; the config at /c errored while starting: Error detected while \
             processing /c/init.lua:. Fix it, or set VIEW_DAILY_CONFIG=off"
        );
        assert_eq!(
            priming_failure_detail("x", None, "E5113: Error while calling lua chunk", config),
            "x; the config at /c errored while starting: E5113: Error while \
             calling lua chunk. Fix it, or set VIEW_DAILY_CONFIG=off"
        );
    }

    /// A row that failed before any step names where it stopped: the
    /// real-config priming for the fixture-less leg, the startup for a
    /// fixture, and `epilogue` for the check after the last step.
    #[test]
    fn a_failed_row_names_the_step_it_stopped_at() {
        let mut row = red_row("lualine", "present", ScenarioStatus::Failed);
        assert_eq!(step_label(&row), "2");
        row.failing_step = Some(row.steps_total);
        assert_eq!(step_label(&row), "epilogue");
        row.failing_step = None;
        assert_eq!(step_label(&row), "startup");
        row.fixture = None;
        assert_eq!(step_label(&row), "real-config priming");
    }
    /// Every scenario file this repo commits, `broken/`'s deliberately-red
    /// entry included (excluded from `collect_scenarios`'s own walk, but
    /// still required to be well-formed TOML against the current schema),
    /// must parse -- a schema change that silently breaks a committed
    /// scenario should fail this test, not first surface as a mysterious
    /// `task compat` error against a file nobody suspected.
    #[test]
    fn every_committed_scenario_file_parses() {
        let scenarios_dir = workspace_root().join("compat").join("scenarios");
        let mut checked = 0usize;
        for dir in [scenarios_dir.clone(), scenarios_dir.join("broken")] {
            for entry in
                std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
            {
                let path = entry.expect("dir entry").path();
                if path.extension().is_some_and(|ext| ext == "toml") {
                    scenario::load_file(&path)
                        .unwrap_or_else(|e| panic!("{} failed to parse: {e}", path.display()));
                    checked += 1;
                }
            }
        }
        assert!(
            checked >= 17,
            "expected at least 17 committed scenario files, checked {checked}"
        );
    }

    /// Replacing the `[native]` table keeps the fixture's `[ui]` table and
    /// drops the fixture's own `[native]` keys.
    #[test]
    fn a_native_override_keeps_the_fixtures_other_tables() {
        let fixture = "[ui]\npanes = \"nvim\"\n\n[native]\npicker = false\ntree = false\n";
        let mut native = BTreeMap::new();
        native.insert("statusline".to_string(), false);
        let merged: toml::Table = with_native_table(fixture, &render_native_toml(&native))
            .expect("merges")
            .parse()
            .expect("the merged file parses");
        assert_eq!(merged["ui"]["panes"].as_str(), Some("nvim"), "{merged}");
        let native = merged["native"].as_table().expect("a [native] table");
        assert_eq!(native.len(), 1, "only the state's own keys: {native}");
        assert_eq!(native["statusline"].as_bool(), Some(false));
    }

    /// A state with a native override whose fixture `view.toml` cannot be
    /// read fails naming the path.
    #[test]
    fn a_missing_fixtures_view_toml_fails_naming_the_path() {
        let dir =
            view_test_support::ScratchDir::new("compat-missing-fixture").expect("scratch dir");
        let missing = dir.join("view.toml");
        let mut native = BTreeMap::new();
        native.insert("statusline".to_string(), false);
        let err = write_native_override(&missing, &render_native_toml(&native))
            .expect_err("a missing fixture file is an error");
        let message = format!("{err:#}");
        assert!(
            message.contains(&missing.display().to_string()),
            "expected the path in the error, got: {message}"
        );
    }

    /// Every state of every committed scenario runs under nvim's own window
    /// picture, whatever `native` override it carries. The scenarios read
    /// cells nvim draws at fixed rows and columns, and tiles move them.
    /// Read through the same two functions `run_scenario` writes with, so
    /// a new state or a new fixture is graded without a list to update.
    #[test]
    fn every_scenario_state_materializes_the_fixtures_look_mode() {
        let scenarios_dir = workspace_root().join("compat").join("scenarios");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&scenarios_dir).expect("reading compat/scenarios") {
            let path = entry.expect("dir entry").path();
            if !path.extension().is_some_and(|ext| ext == "toml") {
                continue;
            }
            let scenario = scenario::load_file(&path).expect("scenario parses");
            for state in &scenario.states {
                let Some(fixture) = state.fixture.as_deref().or(scenario.fixture.as_deref()) else {
                    continue;
                };
                let committed = std::fs::read_to_string(
                    fixtures_root().join(fixture).join("view").join("view.toml"),
                )
                .unwrap_or_else(|e| panic!("{fixture}'s view.toml: {e}"));
                let materialized = match native_toml_override(state) {
                    Some(native) => with_native_table(&committed, &native).expect("merges"),
                    None => committed,
                };
                let table: toml::Table = materialized.parse().expect("parses");
                assert_eq!(
                    table
                        .get("ui")
                        .and_then(|ui| ui.get("panes"))
                        .and_then(toml::Value::as_str),
                    Some("nvim"),
                    "{} state {} on fixture {fixture} runs under tiles:\n{materialized}",
                    path.display(),
                    state.label(),
                );
                checked += 1;
            }
        }
        assert!(
            checked >= 40,
            "expected every state to be read, read {checked}"
        );
    }
}
