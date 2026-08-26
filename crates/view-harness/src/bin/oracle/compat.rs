//! The `compat [PATH]` subcommand: the plugin-compatibility scenario runner.
//!
//! A different subject from the file above it, sharing only the binary they
//! ship in. The corpus runner drives key-notation scripts through two
//! embedded engines and compares them; this drives the real `view` binary
//! over a pty against a pinned plugin fixture and asks whether the plugin
//! still works, per `view_harness::scenario`'s own schema. What they do
//! share -- the engine pin, the workspace roots, the report-then-exit-code
//! contract -- comes from `view_harness` rather than from each other.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use portable_pty::CommandBuilder;
use view_harness::fixture::{
    cache_root, copy_dir_recursive, current_engine_pin, fixtures_root, lockfile_cache_key,
    scratch_root, target_root, verify_nvim_matches_pin, workspace_root,
};
use view_harness::results::{write_results, ResultsFile, ScenarioResult, ScenarioStatus};
use view_harness::scenario::{self, ScenarioFile, ScenarioStateEntry};
use view_oracle::compat::{
    reset_hermetic_home, state_name, CompatSession, ErrorBaseline, PluginClass, ScenarioState,
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

/// `YYYY-MM-DD` for the current instant, in UTC. Hand-rolled rather than a
/// `chrono`/`time` dependency: this is the only date computation anywhere
/// in the workspace, for one report-row stamp
/// ([`view_harness::results::ScenarioResult::date`]).
fn today_date_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's days-since-epoch -> proleptic Gregorian civil date
/// algorithm (public domain: <http://howardhinnant.github.io/date_algorithms.html>),
/// pinned by [`civil_from_days_matches_known_dates`] against independently
/// computed reference values rather than trusted from transcription alone.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
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
    /// Whether the driver itself must type `:call serverstart(...)` --
    /// true only for a fixture-less (daily-config) scenario, whose
    /// `init.lua` this harness does not own and so cannot rely on already
    /// carrying that call.
    needs_priming: bool,
    /// Held only for its `Drop` cleanup; never read.
    _scratch: ScenarioScratch,
}

/// [`resolve_fixture`]'s result: either a [`ReadyFixture`] to spawn `view`
/// against, or a reason to report the scenario SKIPPED without spawning
/// anything (today, only "fixture-less and `$VIEW_DAILY_CONFIG` unset").
enum FixtureResolution {
    // Boxed so the enum is not sized to this large variant next to the tiny
    // `Skipped`: on the msvc target `PathBuf` is wide enough that the four here
    // trip clippy::large_enum_variant, which `-D warnings` makes a Windows CI
    // hard error while linux stays just under the threshold.
    Ready(Box<ReadyFixture>),
    Skipped { notice: String },
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
/// hermetic": its `XDG_DATA_HOME` is the maintainer's ambient data home
/// (see [`ambient_data_home`]), not a fresh scratch directory, because the
/// scenario exists to exercise the maintainer's real lazy.nvim-managed
/// config against its already-installed plugins.
///
/// # Errors
///
/// Returns an error if a named fixture has no `nvim/init.lua`, its
/// `lazy-lock.json` cannot be read, the fixture cannot be copied into a
/// hermetic config dir, `$VIEW_DAILY_CONFIG` names a directory with no
/// `init.lua`/`init.vim`, or (fixture-less, non-Unix host) the isolated
/// config symlink cannot be created.
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
            let lockfile_path = fixture_dir.join("nvim").join("lazy-lock.json");
            let xdg_data_home = if lockfile_path.exists() {
                let bytes = std::fs::read(&lockfile_path)
                    .with_context(|| format!("reading {}", lockfile_path.display()))?;
                let key = if cold_bootstrap {
                    format!("cold-{scratch_id}")
                } else {
                    lockfile_cache_key(&bytes)
                };
                cache_root().join(key)
            } else {
                hermetic_dir.join("xdg_data_home")
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
                needs_priming: false,
                _scratch: ScenarioScratch {
                    hermetic_dir,
                    cold_cache_dir,
                    sock_path: sock_path.to_path_buf(),
                },
            })))
        }
        None => {
            let Ok(daily) = std::env::var("VIEW_DAILY_CONFIG") else {
                let _ = std::fs::remove_dir_all(&hermetic_dir);
                return Ok(FixtureResolution::Skipped {
                    notice: "VIEW_DAILY_CONFIG is unset; fixture-less scenario skipped".to_string(),
                });
            };
            let daily_path = PathBuf::from(&daily);
            if !daily_path.join("init.lua").exists() && !daily_path.join("init.vim").exists() {
                bail!("VIEW_DAILY_CONFIG={daily} has no init.lua/init.vim");
            }
            let xdg_config_home = hermetic_dir.join("xdg_config_home");
            std::fs::create_dir_all(&xdg_config_home)
                .with_context(|| format!("creating {}", xdg_config_home.display()))?;
            symlink_daily_config(&daily_path, &xdg_config_home.join("nvim"))?;
            Ok(FixtureResolution::Ready(Box::new(ReadyFixture {
                xdg_config_home,
                xdg_data_home: ambient_data_home(),
                xdg_state_home,
                xdg_cache_home,
                needs_priming: true,
                _scratch: ScenarioScratch {
                    hermetic_dir,
                    cold_cache_dir: None,
                    sock_path: sock_path.to_path_buf(),
                },
            })))
        }
    }
}

/// The fixture-less arm's `XDG_DATA_HOME`: `$XDG_DATA_HOME` if set, else
/// `$HOME/.local/share` -- the same default nvim itself falls back to when
/// the variable is unset.
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
fn ambient_data_home() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".local").join("share")
}

/// Links `link` (inside a per-run hermetic `XDG_CONFIG_HOME`) to `target`
/// (`$VIEW_DAILY_CONFIG`'s real path), so the maintainer's actual nvim
/// config is what `view` sources while every *other* XDG home
/// (state/cache) stays per-run hermetic, and `XDG_DATA_HOME` is the
/// maintainer's own live data home (see [`ambient_data_home`]) rather than
/// hermetic like the rest. Unix-only (symlinks): the daily-config scenario
/// is a maintainer-machine standing scenario, not a CI-gated one, so a
/// non-Unix host simply cannot run it yet.
///
/// # Errors
///
/// Returns an error if the symlink cannot be created, or (non-Unix) always.
#[cfg(unix)]
fn symlink_daily_config(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("symlinking {} -> {}", link.display(), target.display()))
}

#[cfg(not(unix))]
fn symlink_daily_config(_target: &Path, _link: &Path) -> Result<()> {
    bail!("VIEW_DAILY_CONFIG scenarios require a Unix symlink-capable host")
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
        state: state_name(state.name).to_string(),
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
/// fixtures they run against each commit their own `[native]` table with
/// every feature off -- that committed table, not the all-enabled default
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

    let view_config_path = ready.xdg_config_home.join("view").join("view.toml");
    if let Some(rendered) = native_toml_override(state) {
        if let Some(parent) = view_config_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&view_config_path, rendered)
            .with_context(|| format!("writing {}", view_config_path.display()))?;
    }

    let mut cmd = CommandBuilder::new(view_bin);
    cmd.env("XDG_CONFIG_HOME", &ready.xdg_config_home);
    cmd.env("XDG_DATA_HOME", &ready.xdg_data_home);
    cmd.env("XDG_STATE_HOME", &ready.xdg_state_home);
    cmd.env("XDG_CACHE_HOME", &ready.xdg_cache_home);
    cmd.env("VIEW_COMPAT_SOCK", &sock_path);
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

    let channel_result = if ready.needs_priming {
        // Best-effort settle before typing the priming command: a daily
        // config's own startup content is unknown to this harness (see
        // wait_for_screen_quiescence's own doc comment), so an unsettled
        // screen here does not itself abort the scenario -- the priming
        // retry loop right below is what actually confirms success.
        let _ = session.wait_for_screen_quiescence(SCREEN_QUIESCE_SILENCE, SCREEN_QUIESCE_DEADLINE);
        session
            .prime_probe_channel(PROBE_CHANNEL_TIMEOUT)
            .and_then(|()| {
                // Settle again, and behind a stricter bar, before any step
                // types: see DAILY_STEPS_SILENCE for the startup-burst race
                // this closes. The error baseline is captured after that
                // settle so the config's own startup noise lands inside it
                // and only what the steps add can fail the epilogue.
                let _ = session
                    .wait_for_screen_quiescence(DAILY_STEPS_SILENCE, SCREEN_QUIESCE_DEADLINE);
                session.error_baseline()
            })
    } else {
        session
            .await_probe_channel(PROBE_CHANNEL_TIMEOUT)
            .map(|()| ErrorBaseline::default())
    };
    let baseline = match channel_result {
        Ok(baseline) => baseline,
        Err(err) => {
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
                    detail: Some(err.to_string()),
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
    if ready.needs_priming {
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

/// Prints one scenario's report line in a fixed shape:
/// `compat: lualine (heavy, present) ... OK (4 steps, 2.1s)`.
fn print_scenario_result(result: &ScenarioResult) {
    let fixture = result.fixture.as_deref().unwrap_or("none");
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
            "compat: {scenario} ({fixture}, {}) ... OK ({} steps, {secs:.1}s)",
            result.state, result.steps_total
        ),
        ScenarioStatus::Failed => {
            let step_label = result
                .failing_step
                .map_or_else(|| "epilogue".to_string(), |i| i.to_string());
            println!(
                "compat: {scenario} ({fixture}, {}) ... FAILED at step {step_label} ({} steps total, {secs:.1}s): {}",
                result.state,
                result.steps_total,
                result.detail.as_deref().unwrap_or("unknown failure")
            );
        }
        ScenarioStatus::Skipped => println!(
            "compat: {scenario} ({fixture}, {}) ... SKIPPED: {}",
            result.state,
            result.detail.as_deref().unwrap_or("")
        ),
    }
}

/// The `compat [PATH]` subcommand: every scenario under `path` (default
/// `compat/scenarios`), reported per [`print_scenario_result`] and written
/// to `compat/results.json` for the `page` subcommand to render.
/// Exit code: 0 unless at least one scenario reports
/// [`ScenarioStatus::Failed`] -- a SKIPPED scenario (no daily config on
/// this host, the expected state in CI) does not fail the run, since there
/// is no daily config on that host for the scenario to actually exercise.
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

    let mut results = ResultsFile::default();
    let mut any_failed = false;
    for (scenario_path, scenario) in &scenarios {
        for state in &scenario.states {
            let result = match run_scenario(
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
    /// Reference values independently computed via Python's
    /// `datetime.date` (`epoch + timedelta(days=N)`), not transcribed from
    /// the Hinnant algorithm's own worked examples -- an independent
    /// derivation path, per this codebase's own re-derive-don't-recognize
    /// standard, catches a transcription bug a self-referential check
    /// would not.
    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        assert_eq!(civil_from_days(366), (1971, 1, 2));
        assert_eq!(civil_from_days(1000), (1972, 9, 27));
        assert_eq!(civil_from_days(19570), (2023, 8, 1));
        assert_eq!(civil_from_days(20653), (2026, 7, 19));
    }

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
    /// Unix-only: the fixture-less arm's config symlink
    /// ([`symlink_daily_config`]) is itself Unix-only and returns an error
    /// on every other host, which is a real property of that arm today, not
    /// something this test should paper over by skipping the symlinked-path
    /// exercise on the hosts that can't take it.
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
    #[test]
    fn ambient_data_home_falls_back_to_home_dot_local_share_when_xdg_unset() {
        let _guard = env_mutation_guard();
        let _data_home_env = EnvRestore::unset("XDG_DATA_HOME");
        let _home_env = EnvRestore::set("HOME", "/home/daily-config-test");

        assert_eq!(
            ambient_data_home(),
            PathBuf::from("/home/daily-config-test/.local/share")
        );
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
}
