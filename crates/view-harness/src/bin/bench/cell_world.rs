//! Hermetic per-cell scratch worlds: `CellWorld`/`SideSetup`, the settle
//! bound, and the two spawn-spec builders every row in this crate's bench
//! binary composes from. Split out of bench.rs -- not because this unit
//! needs a unix-only mechanism like `taps_rows.rs`/`remote_rows.rs` do, but
//! because bench.rs sits at its own god-file ceiling with no headroom left
//! for the next row this matrix grows, and this is the largest cohesive
//! unit that moves cleanly: every row driver already reaches these names
//! through `use super::*;`, so pulling them one level down costs no call
//! site a rewrite.

use super::*;

/// Hermetic scratch world for one cell run: per-side XDG homes, scratch
/// files, and sockets, removed on drop.
pub(crate) struct CellWorld {
    pub(crate) hermetic_dir: PathBuf,
}

impl Drop for CellWorld {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.hermetic_dir);
    }
}

/// One side's resolved spawn inputs.
pub(crate) struct SideSetup {
    pub(crate) env: Vec<(OsString, OsString)>,
    pub(crate) cwd: PathBuf,
    pub(crate) scratch_file: PathBuf,
}

impl CellWorld {
    pub(crate) fn create(fixture: &str) -> Result<Self> {
        let id = format!(
            "{}-{}",
            std::process::id(),
            SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let hermetic_dir = scratch_root().join(format!("view-bench-{id}"));
        std::fs::create_dir_all(&hermetic_dir)
            .with_context(|| format!("creating {}", hermetic_dir.display()))?;
        let world = Self { hermetic_dir };

        let fixture_dir = fixtures_root().join(fixture);
        if !fixture_dir.join("nvim").join("init.lua").exists() {
            bail!(
                "fixture {fixture:?} has no {}/nvim/init.lua",
                fixture_dir.display()
            );
        }
        Ok(world)
    }

    /// Resolves one side's hermetic environment: a private copy of the
    /// fixture config (a plugin manager may rewrite its own lockfile in
    /// place), private state/cache homes, and a data home pointed at the
    /// shared lockfile-keyed plugin cache so both sides (and the compat
    /// harness) reuse one plugin install instead of cloning per run.
    ///
    /// The four directories are all this resolves. Every environment
    /// variable that redirects an editor's configuration from outside them
    /// (`$NVIM_APPNAME` voids the config directory below even after it is
    /// pointed at the fixture, `$VIMINIT` runs host commands inside the
    /// measured process) is dropped by `PtySession::spawn_configured`,
    /// which every spawn on both sides of a pair goes through.
    pub(crate) fn side(&self, fixture: &str, side_tag: &str) -> Result<SideSetup> {
        let side_dir = self.hermetic_dir.join(side_tag);
        std::fs::create_dir_all(&side_dir)
            .with_context(|| format!("creating {}", side_dir.display()))?;
        let fixture_dir = fixtures_root().join(fixture);

        let xdg_config_home = side_dir.join("xdg_config_home");
        copy_dir_recursive(&fixture_dir, &xdg_config_home)
            .with_context(|| format!("copying fixture {fixture:?} for the {side_tag} side"))?;

        let lockfile_path = fixture_dir.join("nvim").join("lazy-lock.json");
        let xdg_data_home = if lockfile_path.exists() {
            let bytes = std::fs::read(&lockfile_path)
                .with_context(|| format!("reading {}", lockfile_path.display()))?;
            cache_root().join(lockfile_cache_key(&bytes))
        } else {
            side_dir.join("xdg_data_home")
        };

        let sock = side_dir.join("compat.sock");
        let env: Vec<(OsString, OsString)> = [
            ("XDG_CONFIG_HOME", xdg_config_home.as_os_str()),
            ("XDG_DATA_HOME", xdg_data_home.as_os_str()),
            (
                "XDG_STATE_HOME",
                side_dir.join("xdg_state_home").as_os_str(),
            ),
            (
                "XDG_CACHE_HOME",
                side_dir.join("xdg_cache_home").as_os_str(),
            ),
            ("VIEW_COMPAT_SOCK", sock.as_os_str()),
            ("TERM", "xterm-256color".as_ref()),
            // the only input to view's truecolor bit, and `Tier::Full` --
            // the tier the budget rows name -- requires it, so a session
            // without it measures a child that never reached the stated
            // condition. Today only the sync bit changes emitted bytes, so
            // this costs nothing measurable; it is set now because the
            // alternative is a bench that starts measuring the cheap path
            // silently on the day theming consumes the bit. Set on both
            // sides of a pair, because the two arms must face one terminal
            ("COLORTERM", "truecolor".as_ref()),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v.to_os_string()))
        .collect();

        Ok(SideSetup {
            env,
            cwd: side_dir.clone(),
            scratch_file: side_dir.join("scratch.txt"),
        })
    }
}

// The AI rows are unix-only (they sample through the tap channel), and so
// is the session driver this arming shares its progress path with, so a
// windows build compiles the bench binary with the arming absent rather
// than with a reference it cannot resolve.
#[cfg(unix)]
impl SideSetup {
    /// Turns the agent panel on for this side and points it at `agent`,
    /// by appending an `[ai]` table to the fixture copy this side already
    /// reads. The fixture itself is left alone: every other cell measures
    /// an editor with no agent in it, and a fixture that carried one would
    /// change what those rows measure.
    ///
    /// The trailing argument is the stub fixture's progress path, resolved
    /// by the same function the row's driver reads it back through
    /// (`ai_session::progress_path`); the empty arguments before it are the
    /// fixture's other argument slots, which this row wants none of.
    ///
    /// # Errors
    ///
    /// Returns an error when the copied fixture has no `view.toml` to
    /// extend -- an AI row against a fixture that never configured view is
    /// a row measuring the default agent, which is the real one.
    pub(crate) fn enable_ai_agent(&self, agent: &Path) -> Result<()> {
        let config = self
            .cwd
            .join("xdg_config_home")
            .join("view")
            .join("view.toml");
        ensure!(
            config.exists(),
            "{} has no view.toml for the AI rows to enable the agent in",
            config.display()
        );
        let mut existing = std::fs::read_to_string(&config)
            .with_context(|| format!("reading {}", config.display()))?;
        let progress = view_bench::scenarios::ai_session::progress_path(&self.cwd);
        existing.push_str(&format!(
            "\n[ai]\nenabled = true\nagent = [{:?}, \"\", \"\", \"\", \"\", {:?}]\n",
            agent.to_string_lossy(),
            progress.to_string_lossy()
        ));
        std::fs::write(&config, existing)
            .with_context(|| format!("writing {}", config.display()))?;
        Ok(())
    }
}

/// Settle bound before sampling starts: the heavy fixture's first-ever
/// run may clone plugins into the shared cache, which dwarfs any paint
/// settle; a warm cache settles in a couple of seconds.
pub(crate) fn settle_deadline(fixture: &str) -> Duration {
    if fixture == "heavy" {
        Duration::from_secs(300)
    } else {
        Duration::from_secs(30)
    }
}

/// Builds the view-side spawn spec against one resolved side setup; the
/// engine binary is always passed explicitly so both halves of a pair
/// exercise the same pin-verified nvim.
///
/// Nothing here strips the measured editor down: the fixture config is the
/// subject of the measurement, and `view` spawns its engine through
/// `EngineConfig::default` precisely so that config survives into it. An
/// argument such as `--clean` added on either side, or an isolated engine
/// config swapped in below `view`, would measure a plugin-free editor
/// against baselines recorded with the fixture's full plugin set, report it
/// as a large improvement, and gate green.
///
/// `--nvim-bin` must precede the scratch-file positional: view's CLI
/// forwards every token after the first positional to nvim verbatim
/// (`trailing_var_arg`), so the reverse order hands `--nvim-bin <path>` to
/// nvim itself, which exits on the unknown flag and fails every cell at
/// engine attach.
pub(crate) fn view_spec_from(side: SideSetup, bins: EditorBins<'_>) -> SpawnSpec {
    SpawnSpec {
        program: bins.view.to_path_buf(),
        args: vec![
            OsString::from("--nvim-bin"),
            bins.nvim.as_os_str().to_os_string(),
            side.scratch_file.into_os_string(),
        ],
        env: side.env,
        cwd: Some(side.cwd),
    }
}

/// Builds a bare-nvim spawn spec against one resolved side setup.
pub(crate) fn nvim_spec_from(side: SideSetup, nvim_bin: &Path) -> SpawnSpec {
    SpawnSpec {
        program: nvim_bin.to_path_buf(),
        args: vec![side.scratch_file.into_os_string()],
        env: side.env,
        cwd: Some(side.cwd),
    }
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// The agent writes its progress file and the row's driver reads it:
    /// two processes agreeing on one path only because both resolve it
    /// through `ai_session::progress_path`. A spawn that named a different
    /// one -- or none, leaving the fixture to pick the watched root -- is
    /// what this catches.
    #[test]
    fn the_armed_agent_is_told_the_progress_path_the_driver_reads() {
        let dir = view_test_support::ScratchDir::new("bench-arm-agent").unwrap();
        let cwd = dir.path().join("view");
        std::fs::create_dir_all(cwd.join("xdg_config_home").join("view")).unwrap();
        let config = cwd.join("xdg_config_home").join("view").join("view.toml");
        std::fs::write(&config, "[ui]\n").unwrap();
        let side = SideSetup {
            env: Vec::new(),
            cwd: cwd.clone(),
            scratch_file: cwd.join("scratch.txt"),
        };

        side.enable_ai_agent(Path::new("/nowhere/view-ai-stub-agent"))
            .unwrap();

        let written = std::fs::read_to_string(&config).unwrap();
        let table: toml::Value = written.parse().unwrap();
        let agent = table["ai"]["agent"].as_array().unwrap();
        let progress = view_bench::scenarios::ai_session::progress_path(&cwd);
        assert_eq!(
            agent.last().and_then(toml::Value::as_str),
            Some(progress.to_string_lossy().as_ref()),
            "the fifth argument is the driver's progress path: {written}"
        );
        assert_eq!(agent.len(), 6, "the path lands in the fixture's fifth slot");
    }
}
