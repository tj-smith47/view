//! Fixture and engine-pin plumbing shared by the harness bins: workspace
//! paths, the `.engine-pin` read + binary verification, the hermetic
//! fixture-copy primitive, and the lockfile-keyed plugin cache. One home
//! for these because two consumers spawn editors against the same
//! `compat/fixtures/` trees (the compat driver and the bench matrix), and
//! a second copy of the cache-key or copy logic would let the two drift
//! into measuring different worlds.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Errors resolving fixtures or verifying the pinned engine.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FixtureError {
    #[error("reading engine pin from {path}: {source}")]
    PinRead {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("running {bin} --version: {source}")]
    NvimVersionProbe {
        bin: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "nvim binary {bin} reports version {reported:?} but .engine-pin names {pin:?}; \
         install/select the pinned nvim before running"
    )]
    PinMismatch {
        bin: PathBuf,
        reported: String,
        pin: String,
    },
    #[error("copying {path}: {source}")]
    Copy {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing generated fixture file {path}: {source}")]
    Generate {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "the shared plugin cache at {path} holds no installed plugin, so the generated \
         {USER_FIXTURE:?} fixture would measure a login with an empty plugin set; populate it \
         first with `task compat` or any bench cell on the `heavy` fixture, which install the \
         pinned stack there"
    )]
    EmptyPluginCache { path: PathBuf },
    #[error(
        "the plugin cache at {path} holds the entry {name:?}, whose name is not a plain plugin \
         directory name; the generated {USER_FIXTURE:?} fixture writes each name into a Lua \
         spec and will not quote an arbitrary one"
    )]
    UnnamablePlugin { path: PathBuf, name: String },
    #[error(
        "{USER_FIXTURE_SLOW_MS_ENV} is set to {value:?}, which is not a whole millisecond count"
    )]
    SlowKnob { value: String },
}

/// Where cargo puts what it builds for this tree, which is where every
/// binary a harness spawns is looked for. Re-exported from `view-oracle`
/// so the compat runner, the bench binary and the acceptance tests all
/// resolve it one way.
pub use view_oracle::target_root;

/// Resolved from this crate's own manifest dir rather than the caller's
/// cwd: `task` targets always run from the repo root today, but a direct
/// `cargo run -p view-harness` invocation from a subdirectory must not
/// silently read a stale or absent path instead.
#[must_use]
pub fn workspace_root() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // workspace root
    path
}

/// Parent directory for one harness bin's scratch worlds (`name` keeps
/// the bins' scratch trees apart). Lives under `target/` rather than the
/// system temp dir because /tmp is commonly a RAM-backed tmpfs (a leaked
/// fixture copy would cost memory, not disk) and `target/` is already the
/// disk-backed, gitignored home of build byproducts.
#[must_use]
pub fn scratch_root(name: &str) -> PathBuf {
    workspace_root().join("target").join(name)
}

/// Path to the repo-root `.engine-pin` file.
#[must_use]
pub fn engine_pin_path() -> PathBuf {
    workspace_root().join(".engine-pin")
}

/// Reads and trims the current `.engine-pin` value -- the single source of
/// truth `scripts/check-engine-pin.sh` gates CI against -- never a
/// hardcoded version literal here, so a pin bump does not require a
/// harness code change to stay accurate.
///
/// # Errors
///
/// Returns [`FixtureError::PinRead`] if `.engine-pin` cannot be read.
pub fn current_engine_pin() -> Result<String, FixtureError> {
    let path = engine_pin_path();
    let raw = std::fs::read_to_string(&path).map_err(|source| FixtureError::PinRead {
        path: path.clone(),
        source,
    })?;
    Ok(raw.trim().to_string())
}

/// Confirms `nvim_bin` (resolved via `PATH` unless overridden) actually
/// reports the version `.engine-pin` names, before any run stamps a result
/// row, a corpus entry, or a baseline with that pin. `.engine-pin` alone
/// only says what version a run is *supposed* to use; a stale or wrong
/// `nvim` on `PATH` could otherwise silently produce evidence claiming a
/// pin the run never really exercised.
///
/// # Errors
///
/// Returns [`FixtureError::NvimVersionProbe`] if `nvim_bin --version`
/// cannot be run, or [`FixtureError::PinMismatch`] if its reported version
/// does not match `pin`.
pub fn verify_nvim_matches_pin(nvim_bin: &Path, pin: &str) -> Result<(), FixtureError> {
    let output = std::process::Command::new(nvim_bin)
        .arg("--version")
        .output()
        .map_err(|source| FixtureError::NvimVersionProbe {
            bin: nvim_bin.to_path_buf(),
            source,
        })?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported = stdout
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    if reported != pin {
        return Err(FixtureError::PinMismatch {
            bin: nvim_bin.to_path_buf(),
            reported: reported.to_string(),
            pin: pin.to_string(),
        });
    }
    Ok(())
}

/// `compat/fixtures/`, the directory every named fixture is a
/// subdirectory of.
#[must_use]
pub fn fixtures_root() -> PathBuf {
    workspace_root().join("compat").join("fixtures")
}

/// `compat/.cache/`, the shared, persistent (never per-run-hermetic)
/// plugin install cache a heavy-style fixture's `XDG_DATA_HOME` is pointed
/// at, keyed by its own `lazy-lock.json` hash. Gitignored: this is a
/// build/test cache, not a durable artifact the way `corpus/quarantine/`
/// is.
#[must_use]
pub fn cache_root() -> PathBuf {
    workspace_root().join("compat").join(".cache")
}

/// Hashes `bytes` (a fixture's `lazy-lock.json` content) into a stable,
/// filesystem-safe cache-directory name. [`DefaultHasher`] rather than a
/// cryptographic hash: this key only has to agree with itself across runs
/// of the *same* compiled harness binary (a toolchain upgrade invalidating
/// the cache and forcing one re-clone is an acceptable, self-healing cost,
/// not a correctness bug -- lazy.nvim's own "already installed?" check
/// still governs whether that re-clone actually happens).
#[must_use]
pub fn lockfile_cache_key(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Recursively copies `src` into `dst`, creating `dst` and any needed
/// subdirectories. Regular files only: none of the committed fixture trees
/// contain a symlink, so one found unexpectedly is skipped rather than
/// copied as a symlink or followed, to avoid silently escaping `src`.
///
/// # Errors
///
/// Returns [`FixtureError::Copy`] if any file or directory under `src`
/// cannot be read, or cannot be created/written under `dst`.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), FixtureError> {
    let ctx = |path: &Path| {
        let path = path.to_path_buf();
        move |source| FixtureError::Copy { path, source }
    };
    std::fs::create_dir_all(dst).map_err(ctx(dst))?;
    for entry in std::fs::read_dir(src).map_err(ctx(src))? {
        let entry = entry.map_err(ctx(src))?;
        let file_type = entry.file_type().map_err(ctx(&entry.path()))?;
        let dst_path = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else if file_type.is_file() {
            std::fs::copy(entry.path(), &dst_path).map_err(ctx(&entry.path()))?;
        }
    }
    Ok(())
}

/// The one bench fixture that is generated at run time instead of
/// committed: a login-shaped lazy.nvim config over whatever plugin set the
/// shared compat cache already holds.
///
/// Committed as a name only, never as content. The plugin set it loads is
/// the cache's, so committing the fixture would mean committing either the
/// plugin sources or a second copy of the `heavy` fixture's lockfile that
/// could drift from the cache the bench actually points `XDG_DATA_HOME` at.
pub const USER_FIXTURE: &str = "user";

/// Environment override that plants a deliberate stall in the generated
/// `user` fixture's init, in milliseconds.
///
/// The gate this fixture arms is "a real login's attach window got worse".
/// Proving a gate can fire needs a slowdown the gate has never seen, and a
/// slowdown compiled into the fixture would be one every run pays. A knob
/// read from the environment lets one run wear it and no other.
pub const USER_FIXTURE_SLOW_MS_ENV: &str = "VIEW_BENCH_USER_FIXTURE_SLOW_MS";

/// File the stall plugin writes into the session's own state home, naming
/// the millisecond count it waited. A stall that lengthens the attach
/// window and one whose spec entry the plugin manager silently dropped are
/// indistinguishable from outside the process; this is what tells them
/// apart.
pub const USER_FIXTURE_STALL_RECEIPT: &str = "view-bench-stall";

/// Milliseconds the stall plants when a proof asks for one.
///
/// Sized against the bar it has to clear rather than picked: the cold
/// absolutes gate at `ABSOLUTE_HEADROOM` times the recorded value, so a
/// stall only fires while it exceeds half of what the cell records. Held
/// here so the run that plants one and the test that reasons about the bar
/// cannot drift to two different numbers.
pub const USER_FIXTURE_STALL_MS: u64 = 50;

/// The fixture whose committed `lazy-lock.json` keys the plugin cache the
/// generated `user` fixture is built from. Naming it here rather than
/// duplicating the lockfile keeps one cache directory shared by the compat
/// driver, the `heavy` bench cells and this fixture: a second lockfile
/// would key a second cache and quietly double both the clone cost and the
/// plugin set under measurement.
const USER_FIXTURE_PLUGIN_SOURCE: &str = "heavy";

/// Where the named fixture's config tree is read from: the committed
/// directory for every fixture but [`USER_FIXTURE`], which is generated
/// (or regenerated) here and handed back from `target/`.
///
/// Every consumer that copies a fixture into a hermetic config home goes
/// through this rather than joining [`fixtures_root`] itself, so a
/// generated fixture is reachable everywhere a committed one is instead of
/// only from the one call site that knew about it.
///
/// # Errors
///
/// Returns whatever [`generate_user_fixture`] returns for the generated
/// fixture; never fails for a committed one (a missing directory is the
/// caller's own error to raise, with its own wording).
pub fn fixture_source_dir(name: &str) -> Result<PathBuf, FixtureError> {
    if name == USER_FIXTURE {
        return generate_user_fixture();
    }
    Ok(fixtures_root().join(name))
}

/// Milliseconds the generated fixture's test-only plugin should stall for,
/// from [`USER_FIXTURE_SLOW_MS_ENV`]; `0` (the default) plants no plugin at
/// all.
///
/// # Errors
///
/// Returns [`FixtureError::SlowKnob`] when the variable is set to something
/// that is not a millisecond count. A typo must not read as "no slowdown"
/// and hand back a run that silently measured the unmodified fixture.
fn user_fixture_slow_ms() -> Result<u64, FixtureError> {
    parse_slow_ms(std::env::var(USER_FIXTURE_SLOW_MS_ENV).ok())
}

/// The knob's parse, split from the environment read so a test can pin the
/// refusal without mutating a process-wide variable other tests read.
fn parse_slow_ms(raw: Option<String>) -> Result<u64, FixtureError> {
    let Some(raw) = raw else { return Ok(0) };
    if raw.trim().is_empty() {
        return Ok(0);
    }
    raw.trim()
        .parse()
        .map_err(|_| FixtureError::SlowKnob { value: raw })
}

/// Plugin directory names installed in `lazy_dir`, sorted, with the plugin
/// manager itself left out (it is bootstrapped by path, not declared as a
/// spec entry).
fn cached_plugin_names(lazy_dir: &Path) -> Result<Vec<String>, FixtureError> {
    let entries = std::fs::read_dir(lazy_dir).map_err(|source| FixtureError::Copy {
        path: lazy_dir.to_path_buf(),
        source,
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| FixtureError::Copy {
            path: lazy_dir.to_path_buf(),
            source,
        })?;
        if !entry
            .file_type()
            .map_err(|source| FixtureError::Copy {
                path: entry.path(),
                source,
            })?
            .is_dir()
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "lazy.nvim" {
            continue;
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(FixtureError::UnnamablePlugin {
                path: lazy_dir.to_path_buf(),
                name,
            });
        }
        names.push(name);
    }
    if names.is_empty() {
        return Err(FixtureError::EmptyPluginCache {
            path: lazy_dir.to_path_buf(),
        });
    }
    names.sort();
    Ok(names)
}

/// The generated fixture's `init.lua`, over the plugin names the cache
/// holds.
///
/// Pure so the shape a bench run measures can be pinned by a test without
/// a populated cache or a spawned editor: everything host-specific reaches
/// the file through `vim.fn.stdpath`, resolved inside the measured process
/// against the hermetic homes the bench sets, never through a path baked
/// in here. That also survives the per-side copy -- an absolute path
/// written at generation time would still point at this source tree while
/// the editor reads a copy of it.
fn render_user_init(plugins: &[String], slow_ms: u64) -> String {
    let mut spec = String::new();
    for name in plugins {
        spec.push_str(&format!("    plugin({name:?}),\n"));
    }
    if slow_ms > 0 {
        spec.push_str(
            "    { dir = vim.fn.stdpath(\"config\") .. \"/slow-init\", lazy = false },\n",
        );
    }
    // highlighting the opened buffer is work a login does between attach
    // and the frame that finally shows the file, and the plugin's own
    // setup() does not enable it -- its `configs` module does. Emitted
    // only when the cache actually holds the plugin, because the cache's
    // set is what this fixture is
    let treesitter = if plugins.iter().any(|name| name == "nvim-treesitter") {
        TREESITTER_HIGHLIGHT_LUA
    } else {
        ""
    };
    let overrides = USER_PLUGIN_OVERRIDES_LUA;
    format!(
        r#"-- Generated by view-harness for the bench matrix's `user` fixture; the
-- next run rewrites it, so editing this copy changes nothing.
--
-- A login, not a compat fixture. The committed `heavy` fixture hands
-- lazy.nvim a spec it resolves and partly defers; this one names every
-- plugin the shared cache holds as a local `dir` entry with lazy = false,
-- so the whole stack is loaded and set up before the opened file can
-- paint. That window is what the startup shell sits in front of, and what
-- a real config makes a user wait through.
--
-- install.missing is off: the cache is the only plugin source here, and a
-- measurement that could reach the network mid-run measures the network.

if vim.env.VIEW_COMPAT_SOCK then
  vim.fn.serverstart(vim.env.VIEW_COMPAT_SOCK)
end

vim.g.mapleader = " "
vim.g.maplocalleader = "\\"

require("config.options")
require("config.keymaps")
require("config.autocmds")

local lazyroot = vim.fn.stdpath("data") .. "/lazy/"
vim.opt.rtp:prepend(lazyroot .. "lazy.nvim")

{overrides}
-- setup() is called for the plugins whose main module is named after their
-- repository, which is most of them and is what a login pays for. pcall'd,
-- and with notifications held, because the spec is whatever the cache
-- holds: a plugin that has no such module must still cost its load time
-- rather than put a message on screen that the content marker would then
-- wait behind -- and it would land on only one arm of the pair, since view
-- externalizes messages and bare nvim draws them over the buffer. The held
-- notify only covers the require-time case; anything a plugin defers past
-- setup() needs its own entry in the tables above.
local function plugin(name)
  return {{
    dir = lazyroot .. name,
    lazy = false,
    config = function()
      local notify = vim.notify
      vim.notify = function() end
      pcall(function()
        local pre = presetup[name]
        if pre then
          pre()
        end
        local main = module[name] or (name:gsub("%.nvim$", ""):gsub("%.lua$", ""))
        require(main).setup(opts[name] or {{}})
      end)
      vim.notify = notify
    end,
  }}
end

require("lazy").setup({{
  spec = {{
{spec}  }},
  -- the state home, not the config copy: lazy rewrites its lockfile in
  -- place, and the bench verifies every per-side fixture copy is still
  -- byte-identical to its source once the run is over
  lockfile = vim.fn.stdpath("state") .. "/lazy-lock.json",
  install = {{ missing = false }},
  checker = {{ enabled = false }},
  change_detection = {{ enabled = false }},
  ui = {{ border = "none" }},
}})

pcall(vim.cmd.colorscheme, "habamax")
{treesitter}"#
    )
}

/// Per-plugin values the generated login passes instead of an empty table,
/// every one of them copied from the committed `heavy` fixture's own spec
/// rather than chosen here.
///
/// The three plugins named each write over a surface view externalizes --
/// noice takes the cmdline, the messages and the popupmenu, and both tree
/// plugins hijack netrw and leave an E216 in `v:errmsg` -- and a message
/// drawn on one arm of the pair and not the other lands exactly where the
/// row reads its content marker. mini.nvim is a library whose own top-level
/// module only scolds a caller who requires it, so it is named by the
/// module the sibling fixture uses.
const USER_PLUGIN_OVERRIDES_LUA: &str = r#"
local opts = {
  ["noice.nvim"] = {
    cmdline = { enabled = false },
    messages = { enabled = false },
    popupmenu = { enabled = false },
  },
  ["nvim-tree.lua"] = { hijack_netrw = false },
  ["neo-tree.nvim"] = { filesystem = { hijack_netrw_behavior = "disabled" } },
}

local module = { ["mini.nvim"] = "mini.pairs" }

-- noice's health check raises one ERROR notification per externalized ext
-- from inside setup(), before setup() has read the opts above, and no
-- option gates it. Pre-seeding its own once-only dedup table with exactly
-- those three messages marks them sent and leaves every other diagnostic
-- live.
local presetup = {
  ["noice.nvim"] = function()
    local once = require("noice.util")._once
    for _, ext in ipairs({ "ext_cmdline", "ext_popupmenu", "ext_messages" }) do
      local msg = "You're using a GUI that uses "
        .. ext
        .. ". Noice can't work when the GUI has "
        .. ext
        .. " enabled."
      once[vim.log.levels.ERROR .. msg] = true
    end
  end,
}
"#;

/// The highlight enable a login writes once its treesitter plugin is
/// installed. Kept out of the generic per-plugin `setup({})` pass because
/// the module that enables highlighting is not the one named after the
/// repository.
const TREESITTER_HIGHLIGHT_LUA: &str = r#"
pcall(function()
  require("nvim-treesitter.configs").setup({
    ensure_installed = {},
    sync_install = false,
    auto_install = false,
    highlight = { enable = true },
  })
end)
"#;

/// Options a login sets before its plugins load.
const USER_OPTIONS_LUA: &str = r#"local o = vim.opt
o.number = true
o.relativenumber = true
o.signcolumn = "yes"
o.cursorline = true
o.termguicolors = true
o.expandtab = true
o.shiftwidth = 2
o.tabstop = 2
o.smartindent = true
o.ignorecase = true
o.smartcase = true
o.undofile = true
o.updatetime = 200
o.scrolloff = 4
o.splitright = true
o.splitbelow = true
o.completeopt = "menu,menuone,noselect"
"#;

/// Leader mappings a login installs at startup.
const USER_KEYMAPS_LUA: &str = r#"local map = vim.keymap.set
map("n", "<leader>w", "<cmd>write<cr>", { desc = "write" })
map("n", "<leader>q", "<cmd>quit<cr>", { desc = "quit" })
map("n", "<leader>h", "<cmd>nohlsearch<cr>", { desc = "clear search" })
map("n", "<C-h>", "<C-w>h", { desc = "window left" })
map("n", "<C-l>", "<C-w>l", { desc = "window right" })
"#;

/// Autocommands a login installs at startup.
const USER_AUTOCMDS_LUA: &str = r#"vim.api.nvim_create_autocmd("TextYankPost", {
  callback = function()
    pcall(function()
      vim.hl.on_yank({ timeout = 120 })
    end)
  end,
})

vim.api.nvim_create_autocmd("BufReadPost", {
  callback = function(args)
    local mark = vim.api.nvim_buf_get_mark(args.buf, '"')
    if mark[1] > 0 and mark[1] <= vim.api.nvim_buf_line_count(args.buf) then
      pcall(vim.api.nvim_win_set_cursor, 0, mark)
    end
  end,
})
"#;

/// Writes the generated `user` fixture under `target/` and hands back its
/// directory.
///
/// Its `lazy-lock.json` is a byte copy of the source fixture's, which is
/// what makes the two share one cache directory: the bench keys
/// `XDG_DATA_HOME` off the lockfile's own hash, so a copy resolves to the
/// already-installed plugin set instead of cloning a second one.
///
/// Every file lands by rename (see [`write_generated`]), so a peer session
/// generating the same tree at the same time cannot hand this one a
/// half-written config.
///
/// Idempotent rather than torn down and rebuilt: two sides of one pair
/// resolve their fixture directory independently, and a rebuild between
/// them would delete the tree the first side is still reading. The one
/// conditional file (the stall plugin) is removed explicitly when the knob
/// is off, so a run without it can never inherit the previous run's.
///
/// # Errors
///
/// Returns [`FixtureError::EmptyPluginCache`] when the shared cache holds
/// no installed plugin, [`FixtureError::SlowKnob`] for an unparsable stall
/// knob, and [`FixtureError::Generate`] / [`FixtureError::Copy`] for any
/// file the fixture could not be written from or to.
pub fn generate_user_fixture() -> Result<PathBuf, FixtureError> {
    generate_user_fixture_with_stall(user_fixture_slow_ms()?)
}

/// [`generate_user_fixture`] with the stall named outright instead of read
/// from the environment, so a caller proving the stall loads does not have
/// to mutate a process-wide variable to ask for one.
///
/// # Errors
///
/// The same as [`generate_user_fixture`], less the knob's own parse.
pub fn generate_user_fixture_with_stall(slow_ms: u64) -> Result<PathBuf, FixtureError> {
    let template = fixtures_root().join(USER_FIXTURE_PLUGIN_SOURCE);
    let lockfile = template.join("nvim").join("lazy-lock.json");
    let lock_bytes = std::fs::read(&lockfile).map_err(|source| FixtureError::Copy {
        path: lockfile.clone(),
        source,
    })?;
    let lazy_dir = cache_root()
        .join(lockfile_cache_key(&lock_bytes))
        .join("nvim")
        .join("lazy");
    let plugins = cached_plugin_names(&lazy_dir)?;

    let dest = scratch_root("bench-fixtures").join(USER_FIXTURE);
    let nvim = dest.join("nvim");
    let config = nvim.join("lua").join("config");
    write_generated(&nvim.join("init.lua"), render_user_init(&plugins, slow_ms))?;
    write_generated(&nvim.join("lazy-lock.json"), lock_bytes)?;
    write_generated(&config.join("options.lua"), USER_OPTIONS_LUA)?;
    write_generated(&config.join("keymaps.lua"), USER_KEYMAPS_LUA)?;
    write_generated(&config.join("autocmds.lua"), USER_AUTOCMDS_LUA)?;
    let view_toml = "view.toml";
    let source_view = template.join("view").join(view_toml);
    let view_bytes = std::fs::read(&source_view).map_err(|source| FixtureError::Copy {
        path: source_view,
        source,
    })?;
    write_generated(&dest.join("view").join(view_toml), view_bytes)?;
    let stall = nvim.join("slow-init");
    if slow_ms > 0 {
        write_generated(
            &stall.join("plugin").join("stall.lua"),
            // the receipt file is what separates "the stall ran" from "the
            // spec entry was quietly ignored": both look like a fast run
            // from outside, and only one of them is a proof
            format!(
                "local dir = vim.fn.stdpath(\"state\")\n\
                 vim.fn.mkdir(dir, \"p\")\n\
                 vim.fn.writefile({{ \"{slow_ms}\" }}, dir .. \"/{USER_FIXTURE_STALL_RECEIPT}\")\n\
                 vim.wait({slow_ms})\n"
            ),
        )?;
    } else {
        let _ = std::fs::remove_dir_all(&stall);
    }
    Ok(dest)
}

/// Writes one generated fixture file, creating its parent directories.
///
/// Through a private neighbour and a rename rather than in place: this tree
/// hosts concurrent sessions building against one `target/`, and two of
/// them writing the same `init.lua` at once would otherwise hand an editor
/// a half-written config with nothing raising an error. The rename is what
/// makes a reader see the old file or the new one and never a torn one.
/// Per file rather than by swapping the whole directory, because the two
/// sides of one pair resolve this tree independently and a directory
/// swapped out from under the first side is a file the second run deleted.
fn write_generated(path: &Path, content: impl AsRef<[u8]>) -> Result<(), FixtureError> {
    let ctx = |path: &Path| {
        let path = path.to_path_buf();
        move |source| FixtureError::Generate { path, source }
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ctx(parent))?;
    }
    let mut staging = path.as_os_str().to_os_string();
    staging.push(format!(".{}.tmp", std::process::id()));
    let staging = PathBuf::from(staging);
    // a failed write or rename leaves the neighbour behind otherwise, and
    // the next run picks a name off the same pid: one stale byte string in
    // target/ per failure, never cleaned by anything
    let written = std::fs::write(&staging, content)
        .and_then(|()| std::fs::rename(&staging, path))
        .map_err(ctx(path));
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    written
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use view_test_support::ScratchDir;

    #[test]
    fn lockfile_cache_key_is_stable_for_identical_bytes() {
        assert_eq!(lockfile_cache_key(b"abc"), lockfile_cache_key(b"abc"));
    }

    #[test]
    fn lockfile_cache_key_differs_for_different_bytes() {
        assert_ne!(lockfile_cache_key(b"abc"), lockfile_cache_key(b"abd"));
    }

    /// Values, not just keys: the generated login hands the same
    /// neutralising options to the plugins that fight an externalized UI
    /// as the committed fixture beside it does. A plugin that draws its
    /// own cmdline or reroutes messages puts text where this row reads
    /// its content marker, and on one arm of the pair only.
    #[test]
    fn the_generated_login_neutralizes_what_the_sibling_fixture_neutralizes() {
        let sibling = std::fs::read_to_string(
            fixtures_root()
                .join(USER_FIXTURE_PLUGIN_SOURCE)
                .join("nvim")
                .join("init.lua"),
        )
        .unwrap();
        let generated = render_user_init(&["noice.nvim".to_string()], 0);
        for value in [
            "cmdline = { enabled = false }",
            "messages = { enabled = false }",
            "popupmenu = { enabled = false }",
            "hijack_netrw = false",
            "hijack_netrw_behavior = \"disabled\"",
            "require(\"noice.util\")._once",
            "mini.pairs",
        ] {
            assert!(
                sibling.contains(value),
                "{value:?} is meant to be the {USER_FIXTURE_PLUGIN_SOURCE} fixture's own value, \
                 and that fixture no longer carries it"
            );
            assert!(
                generated.contains(value),
                "the generated login dropped {value:?}, which the \
                 {USER_FIXTURE_PLUGIN_SOURCE} fixture passes to keep a plugin off the surface \
                 view externalizes"
            );
        }
    }

    /// Every plugin the cache holds must reach the spec: a name silently
    /// dropped turns the login the row measures into a smaller one, and
    /// the number still records as "a real config".
    #[test]
    fn the_generated_init_names_every_cached_plugin() {
        let plugins = vec![
            "lualine.nvim".to_string(),
            "nvim-tree.lua".to_string(),
            "plenary.nvim".to_string(),
        ];
        let init = render_user_init(&plugins, 0);
        for name in &plugins {
            assert!(
                init.contains(&format!("plugin({name:?})")),
                "{name} is missing from the generated spec:\n{init}"
            );
        }
        assert!(
            init.contains("install = { missing = false }"),
            "the generated login must never clone mid-measurement:\n{init}"
        );
        assert!(
            !init.contains("/opt/") && !init.contains("compat/.cache"),
            "every path must resolve through stdpath inside the measured \
             process, never a host path baked in here:\n{init}"
        );
        assert!(
            init.contains("vim.notify = function() end"),
            "a plugin that greets at require time would otherwise leave a \
             message where the content marker has to appear:\n{init}"
        );
        assert!(
            !init.contains("nvim-treesitter.configs"),
            "highlighting must be enabled only when the cache holds the \
             plugin that provides it:\n{init}"
        );
        let with_treesitter = render_user_init(&["nvim-treesitter".to_string()], 0);
        assert!(
            with_treesitter.contains("highlight = { enable = true }"),
            "a login with treesitter installed highlights the file it \
             opens, which is the largest cost in its attach \
             window:\n{with_treesitter}"
        );
    }

    /// The stall is opt-in and arrives as its own plugin directory, so an
    /// ordinary run measures the unmodified login and the proof run
    /// measures one extra loaded plugin.
    #[test]
    fn the_stall_plugin_is_absent_until_the_knob_asks_for_it() {
        let plugins = vec!["lualine.nvim".to_string()];
        assert!(
            !render_user_init(&plugins, 0).contains("slow-init"),
            "an unset knob must generate the login as it ships"
        );
        let slowed = render_user_init(&plugins, 50);
        assert!(
            slowed.contains(r#"{ dir = vim.fn.stdpath("config") .. "/slow-init", lazy = false }"#),
            "the stall must load as a plugin of the fixture's own copy:\n{slowed}"
        );
    }

    /// A knob nobody can parse must refuse rather than read as zero: a run
    /// that silently measured the unmodified fixture would be reported as
    /// the slowed one.
    #[test]
    fn an_unparsable_stall_knob_refuses_instead_of_defaulting_to_none() {
        assert_eq!(parse_slow_ms(None).unwrap(), 0);
        assert_eq!(parse_slow_ms(Some("  ".to_string())).unwrap(), 0);
        assert_eq!(parse_slow_ms(Some(" 50 ".to_string())).unwrap(), 50);
        let err = parse_slow_ms(Some("50ms".to_string())).unwrap_err();
        assert!(
            matches!(err, FixtureError::SlowKnob { ref value } if value == "50ms"),
            "expected a refusal naming the value, got {err:?}"
        );
    }

    /// The generated fixture is only a fixture because it resolves to the
    /// same plugin install the `heavy` cells and the compat driver already
    /// share: the bench keys `XDG_DATA_HOME` off the lockfile's hash, so a
    /// lockfile that differed by one byte would key an empty second cache
    /// and the row would measure a login with no plugins in it.
    ///
    /// Both outcomes are asserted because the cache is populated by a
    /// compat or bench run, not by the test suite: on a tree that has
    /// never run one, the generator must say so rather than hand back a
    /// plugin-free login.
    #[test]
    fn the_generated_fixture_either_shares_the_plugin_cache_or_says_it_is_empty() {
        let lockfile = fixtures_root()
            .join(USER_FIXTURE_PLUGIN_SOURCE)
            .join("nvim")
            .join("lazy-lock.json");
        let expected = std::fs::read(&lockfile).expect("the plugin-source fixture has a lockfile");
        match generate_user_fixture() {
            Ok(dir) => {
                let generated = std::fs::read(dir.join("nvim").join("lazy-lock.json")).unwrap();
                assert_eq!(
                    lockfile_cache_key(&generated),
                    lockfile_cache_key(&expected),
                    "the generated fixture must key the same plugin cache directory"
                );
                let init = std::fs::read_to_string(dir.join("nvim").join("init.lua")).unwrap();
                assert!(
                    init.contains("plugin("),
                    "a generated login with no plugin spec is not a login:\n{init}"
                );
                assert!(
                    dir.join("view").join("view.toml").exists(),
                    "the fixture must carry the view config every other fixture does"
                );
                assert!(
                    !dir.join("nvim").join("slow-init").exists(),
                    "a run with no knob set must not inherit an earlier run's stall"
                );
            }
            Err(err) => assert!(
                matches!(err, FixtureError::EmptyPluginCache { .. }),
                "an unpopulated cache must say so; got {err:?}"
            ),
        }
    }

    #[test]
    fn copy_dir_recursive_copies_nested_files() {
        let base = ScratchDir::new("fixture-copy").unwrap();
        let src = base.join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("a.txt"), b"top").unwrap();
        std::fs::write(src.join("nested").join("b.txt"), b"deep").unwrap();
        let dst = base.join("dst");
        copy_dir_recursive(&src, &dst).unwrap();
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"top");
        assert_eq!(std::fs::read(dst.join("nested/b.txt")).unwrap(), b"deep");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
