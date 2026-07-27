//! Host environment variables that redirect where a spawned Neovim looks
//! for configuration, the hermetic values that neutralize them, and the
//! allowlist deciding which of everything else on the host reaches a
//! hermetic child at all.
//!
//! Pointing the four `XDG_*_HOME` variables at private directories does not
//! by itself detach a child from the host's editor setup. A handful of
//! other variables re-point the same lookups from outside those
//! directories, run commands before any of them are consulted, or (for the
//! two search-path variables) fall back to system-wide defaults when unset,
//! so that clearing them selects a host path rather than no path at all.
//!
//! The lists are enumerated from the pinned engine's own documentation
//! (`:help starting`, `:help standard-path`, `:help remote-plugin-manifest`,
//! `:help 'ttyfast'`, `:help $NVIM`), extended with the two LuaJIT search
//! paths that sit outside that documentation but reach the same child, and
//! every entry was confirmed against the pinned binary. Deliberately
//! absent:
//!
//! - `HOME`: once the `XDG_*_HOME` variables are set, Neovim derives none of
//!   its own directories from it, and overriding it would break the
//!   unrelated host tooling (Cargo, git) a harness runs alongside the child.
//! - `XDG_RUNTIME_DIR`: names where the child writes its server socket, not
//!   where it reads configuration or code from, and a private replacement
//!   deep inside a scratch tree risks overflowing the 104-byte limit on a
//!   Unix socket path, turning a hygiene measure into a spawn failure.
//! - `LUA_INIT` and `LUA_PATH_5_1`: LuaJIT honours neither inside Neovim
//!   (confirmed against the pinned binary: an `LUA_INIT` print never runs,
//!   and a module reachable only through `LUA_PATH_5_1` fails to resolve),
//!   unlike `LUA_PATH`/`LUA_CPATH`, which are in the removal list below.
//! - `LANG`, `LANGUAGE`, `LC_ALL`, `LC_MESSAGES`: the locale reaches the
//!   child, but the pinned binary ships no message catalogs at all (no
//!   `lang/` directory under its `$VIMRUNTIME` on either supported host),
//!   so a non-English locale leaves every message the screen-scraping
//!   oracles match in English. Confirmed by running the pinned binary under
//!   `LC_ALL=de_DE.UTF-8`: `v:lang` reported the locale as active while
//!   `E149` and `-- INSERT --` stayed English. Pinning `LC_ALL=C` anyway
//!   would trade that non-effect for a real one, since the `ctype` rules it
//!   selects are what the non-ASCII screen assertions read through.
//! - `TERM`, `SHELL`, `TMPDIR`: terminal and subprocess behavior, no
//!   configuration lookup. A measurement harness that needs them pinned
//!   pins them itself, as the value it wants is a property of the
//!   measurement, not a fixed hermetic constant. `TMPDIR` in particular is
//!   inert here only because [`empty_search_path`] deliberately does not
//!   live under it: a hermetic path selected by a host variable would be a
//!   host path with extra steps.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// Environment variables removed outright from a hermetic child's
/// environment: each one either injects startup commands or redirects where
/// the child finds configuration, runtime files, plugin manifests, or Lua
/// modules.
///
/// Removal is the correct neutralizer for every entry here, since Neovim or
/// LuaJIT derives its own value for each when it is unset. Contrast
/// [`HOST_SEARCH_PATH_VARS`], where unset means a system-wide default.
pub const HOST_REDIRECT_VARS: &[&str] = &[
    // Ex command lines the child runs at startup, ahead of any config file:
    // arbitrary host code inside an otherwise hermetic child
    "VIMINIT",
    "EXINIT",
    // Neovim resolves this itself only when the host has not already set
    // it, so a host value survives into the child and is what a plugin's
    // `:source $MYVIMRC` reaches
    "MYVIMRC",
    // the GUI counterpart of MYVIMRC, inert in a child with no GUI to
    // source it. Carried because it belongs to the same documented set as
    // MYVIMRC and costs one string: an enumeration built from a
    // documentation sweep that silently omits one of its entries reads as
    // one whose other omissions were considered too, and they were not
    "MYGVIMRC",
    // both locate the runtime files that seed 'runtimepath'
    "VIM",
    "VIMRUNTIME",
    // redirects every standard directory below the XDG homes, so a host
    // value voids the config directory an otherwise hermetic
    // XDG_CONFIG_HOME just established, silently leaving the child with no
    // configuration at all
    "NVIM_APPNAME",
    // names the remote-plugin manifest, which is sourced as vimscript
    "NVIM_RPLUGIN_MANIFEST",
    // both tell the child it is a nested child of a live Neovim, which
    // changes what a plugin does at startup
    "NVIM",
    "NVIM_LISTEN_ADDRESS",
    // diverts the child's own log writes to a host path
    "NVIM_LOG_FILE",
    // forces 'nottyfast', changing what the TUI does during startup
    "NVIM_NOTTYFAST",
    // LuaJIT's own module search paths, outside the set Neovim documents
    // and therefore outside what a `:help` sweep finds: a host value lands
    // ahead of every compiled-in default in `package.path`/`package.cpath`
    // (confirmed against the pinned binary), so a `require()` of a name
    // absent from 'runtimepath' resolves against the host instead of
    // failing. A plugin's `pcall(require, "optional-dep")` probe taking the
    // other branch inside a measured process is the shape of that. Removal
    // rather than an empty override because an unset value yields LuaJIT's
    // compiled-in defaults, which the child's own modules need, while an
    // empty one would delete them
    "LUA_PATH",
    "LUA_CPATH",
];

/// Environment variables that must be *overridden* with an empty directory
/// rather than removed: Neovim substitutes system-wide defaults (`/etc/xdg`
/// and `/usr/local/share:/usr/share`) when they are unset, so clearing them
/// selects a host path instead of no path.
///
/// Both feed 'runtimepath' with a directory whose `plugin/` scripts the
/// child sources at startup, and `--clean` does not exclude them: it drops
/// the *user* directories only. Confirmed against the pinned binary, which
/// sourced a plugin from each of them under `--clean` (the two layouts
/// differ: `$XDG_CONFIG_DIRS/nvim/plugin/` and
/// `$XDG_DATA_DIRS/nvim/site/plugin/`).
pub const HOST_SEARCH_PATH_VARS: &[&str] = &["XDG_CONFIG_DIRS", "XDG_DATA_DIRS"];

/// Environment variables overridden so that the programs a hermetic child
/// *itself* spawns read no configuration out of the host's `HOME`, which
/// [`HERMETIC_PASSTHROUGH_VARS`] keeps because the editor needs it.
///
/// The editor's own lookups are diverted by the `XDG_*_HOME` overrides a
/// caller sets, but a subprocess does not consult those and resolves its own
/// configuration from `HOME` directly. Git is the live case: a fixture that
/// installs plugins runs `git clone` from inside a supposedly hermetic
/// child, and `$HOME/.gitconfig` can carry an `insteadOf` rewrite, an
/// `http.proxy`, `http.sslVerify = false`, or a credential helper -- so
/// which repository the child fetches, over what transport, and with whose
/// credentials all become properties of the operator's machine. That is
/// host state deciding what a measurement measures, and it reports nothing:
/// the clone succeeds, the plugins load, and the run is green.
///
/// Both layers are named because either alone leaves the other live, and
/// both are pointed at [`absent_config_file`] rather than emptied, since
/// Git skips a configuration file it cannot read and reads an empty one.
pub const HOST_SUBPROCESS_CONFIG_VARS: &[&str] = &["GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"];

/// The host environment variables a hermetic child keeps. Every other
/// variable the host exports is dropped before the child sees it.
///
/// This inverts [`HOST_REDIRECT_VARS`], which stays as the second layer
/// rather than being subsumed: that list names what a documentation sweep
/// found, and a sweep can only ever be complete about the day it ran. A
/// variable nobody enumerated reaches a child, changes what it loads, and
/// reports nothing -- the child still starts, still measures, and disagrees
/// with the other host only once somebody compares two machines. An
/// allowlist cannot have that failure: an unenumerated variable is dropped,
/// and a child that turns out to need one fails loudly and immediately.
///
/// Each entry earns its place by being something the child needs rather
/// than something the host happens to export:
///
/// - `PATH`, and on Windows `COMSPEC`/`PATHEXT`: how the child resolves the
///   shell that `:terminal` and `system()` run, which the flood row drives
///   directly.
/// - `HOME`, `USER`, `LOGNAME`, `SHELL` and the Windows profile variables:
///   identity and home resolution that libc, LuaJIT and the child's own
///   `expand('~')` read. Keeping `HOME` does open a channel the editor's
///   own `XDG_*_HOME` overrides do not close, because the child's
///   *subprocesses* resolve their configuration through it independently;
///   [`HOST_SUBPROCESS_CONFIG_VARS`] closes that channel rather than
///   leaving it to the entry above to disclaim.
/// - `TERM`, `COLORTERM`: what the child derives its capability tier from,
///   which a measurement pins deliberately.
/// - `TMPDIR`, `XDG_RUNTIME_DIR` and the Windows temp variables: where the
///   child writes scratch files and its server socket. Replacing
///   `XDG_RUNTIME_DIR` risks overflowing the 104-byte Unix socket path
///   limit, turning a hygiene measure into a spawn failure.
/// - `LANG`, `LANGUAGE` and the `LC_` prefix: the locale reaches the child
///   either way, and the module documentation above records why pinning it
///   would trade a measured non-effect for a real one.
/// - The Windows system variables: process creation itself fails without
///   `SYSTEMROOT`, so dropping them would not isolate a child, it would
///   stop there being one.
pub const HERMETIC_PASSTHROUGH_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "COLORTERM",
    "TMPDIR",
    "XDG_RUNTIME_DIR",
    "LANG",
    "LANGUAGE",
    "COMSPEC",
    "PATHEXT",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "USERNAME",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "OS",
];

/// Name prefixes whose every variable passes through, for families whose
/// membership is open-ended: `LC_COLLATE`, `LC_CTYPE`, `LC_MESSAGES` and the
/// rest are one decision, not eleven, and a locale category added by a libc
/// this tree has not seen belongs on the same side as the ones it has.
pub const HERMETIC_PASSTHROUGH_PREFIXES: &[&str] = &["LC_"];

/// Whether `name` survives into a hermetic child.
///
/// A name that is not valid UTF-8 is not passthrough. The lists are ASCII,
/// so such a name cannot be on them, and dropping is the safe answer for a
/// variable this tree cannot even render.
#[must_use]
pub fn is_hermetic_passthrough(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    HERMETIC_PASSTHROUGH_VARS
        .iter()
        .any(|allowed| env_names_eq(OsStr::new(name), OsStr::new(allowed)))
        || HERMETIC_PASSTHROUGH_PREFIXES.iter().any(|prefix| {
            // a fallible slice rather than an index: a prefix length landing
            // inside a multi-byte character would panic, and `LC_` is three
            // bytes into names like `abé` that Unix accepts as variables
            name.get(..prefix.len())
                .is_some_and(|head| env_names_eq(OsStr::new(head), OsStr::new(prefix)))
        })
}

/// Compares two environment variable names under the rule of the host this
/// is running on: Windows folds case, Unix does not, and on Unix `path` and
/// `PATH` are two different variables of which only one is the allowlisted
/// one.
///
/// The two rules are [`names_eq_folding_case`] and plain equality, each a
/// function of its arguments alone so that both are exercised on every
/// platform and only the one-line choice between them is left to `cfg!`.
#[must_use]
pub fn env_names_eq(left: &OsStr, right: &OsStr) -> bool {
    if cfg!(windows) {
        names_eq_folding_case(left, right)
    } else {
        left == right
    }
}

/// Windows' own rule for whether two environment variable names denote one
/// variable, as closely as this tree can restate it.
///
/// Windows collapses the names in a process environment with
/// `CompareStringOrdinal(bIgnoreCase = TRUE)`, which applies the system's
/// *simple* uppercase mapping -- one code unit to one code unit, never an
/// expansion. [`simple_uppercase`] is that mapping: `char::to_uppercase`
/// where it yields a single character, and the character unchanged where it
/// would expand, so `ss` and the sharp s stay distinct here exactly as they
/// do to the operating system.
///
/// An ASCII-only fold would be strictly narrower than the rule it claims to
/// implement, leaving a cased non-ASCII pair riding a plan as two entries
/// that the spawned child then collapses into one -- the plan disagreeing
/// with the child, which is the whole failure this comparison exists to
/// prevent. What remains divergent is the Unicode table itself: this reads
/// Rust's, the child is collapsed against the operating system's, and the
/// two can differ for characters added between their versions.
///
/// A name that is not valid UTF-8 compares byte for byte, having no case to
/// fold that this can see.
#[must_use]
pub fn names_eq_folding_case(left: &OsStr, right: &OsStr) -> bool {
    match (left.to_str(), right.to_str()) {
        (Some(left), Some(right)) => left
            .chars()
            .map(simple_uppercase)
            .eq(right.chars().map(simple_uppercase)),
        _ => left == right,
    }
}

/// `c` uppercased where that is a one-to-one mapping, and `c` itself where
/// uppercasing it would produce more than one character.
fn simple_uppercase(c: char) -> char {
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(only), None) => only,
        _ => c,
    }
}

/// The path [`HOST_SUBPROCESS_CONFIG_VARS`] are pointed at: a file that does
/// not exist, so every configuration layer they select is missing rather
/// than empty.
///
/// Inside [`empty_search_path`] rather than beside it, because that is the
/// directory [`prepare_empty_search_path`] makes unwritable. A path under a
/// writable directory with a predictable name is one anybody can create a
/// configuration file at, and a child pointed there would then read it --
/// the same plant this tree refuses to leave open for `plugin/` scripts.
/// Nothing ever creates it, so the emptiness that same function checks is
/// unaffected.
#[must_use]
pub fn absent_config_file() -> PathBuf {
    empty_search_path().join("absent")
}

/// Every variable this process exports that [`is_hermetic_passthrough`]
/// rejects, each paired with the value the host holds for it: what a
/// hermetic spawn must drop before the child inherits it.
///
/// A snapshot taken at the moment of the call, not a lazy view. A spawn
/// builder copies the host environment when it is constructed, so a sweep
/// that read the environment again later would disagree with that copy about
/// any name changed in between -- and a disagreement is exactly the signal a
/// caller's deliberate override produces. A test that mutates the process
/// environment must therefore bracket the builder's construction and the
/// sweep together, which is what an environment-mutation guard is for.
///
/// The host's value travels with each name because a spawn builder cannot be
/// asked whether a caller set a variable: `portable_pty`'s builder
/// pre-populates its map from the whole host environment and answers `Some`
/// for every name, while [`std::process::Command`] reports overrides only.
/// A value that differs from the host's is what a caller override looks like
/// on either of them, and is the one test that means the same thing on both.
#[must_use]
pub fn hermetic_sweep() -> Vec<(OsString, OsString)> {
    std::env::vars_os()
        .filter(|(name, _)| !is_hermetic_passthrough(name))
        .collect()
}

/// The directory [`HOST_SEARCH_PATH_VARS`] are pointed at, whose emptiness
/// [`prepare_empty_search_path`] establishes before a hermetic child is
/// spawned.
///
/// Under the build tree rather than the system temp dir, matching where the
/// harness puts every other scratch tree: the temp dir is world-writable
/// with a guessable name, so a directory merely *expected* to stay empty is
/// a directory anyone can plant a `nvim/plugin/` script in, and every
/// "hermetic" child would then source it. Its path is resolved from this
/// crate's own manifest dir because the crate that owns the same derivation
/// for the harness bins sits above this one in the dependency order.
///
/// A build-machine path baked into a released binary never resolves at run
/// time, and never has to: only [`crate::process::EngineConfig::isolated`]
/// consults this, and the shipped editor spawns through
/// `EngineConfig::default` (pinned by test in the `view` binary itself).
#[must_use]
pub fn empty_search_path() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    root.join("target").join("view-hermetic-empty")
}

/// Creates [`empty_search_path`] if absent, verifies it holds nothing, and
/// makes it unwritable, returning the path a hermetic spawn points
/// `XDG_CONFIG_DIRS`/`XDG_DATA_DIRS` at.
///
/// Called at the spawn funnels rather than trusted as an invariant of the
/// path itself: "empty because nothing creates it" is a claim about this
/// repository's code, not about a directory on the machine running it, and
/// a child sourcing a planted `plugin/` script produces no error, no
/// warning, and no visible difference from a hermetic one.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the directory cannot be
/// created, read, or have its permissions set, and an
/// [`io::Error::other`] naming the offending entry if it is not empty.
pub fn prepare_empty_search_path() -> io::Result<PathBuf> {
    let path = empty_search_path();
    prepare_empty_dir(&path)?;
    Ok(path)
}

/// The body of [`prepare_empty_search_path`], taking its path as an
/// argument so a test can exercise the refusal against a directory of its
/// own instead of planting an entry in the one every concurrent spawn in
/// the same test binary is reading.
fn prepare_empty_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    // checked before the permission change below, so a directory that turns
    // out to hold something is left exactly as it was found
    if let Some(entry) = std::fs::read_dir(path)?.next() {
        let name = entry?.file_name();
        return Err(io::Error::other(format!(
            "the hermetic search path {} holds {:?}: a child pointed at it would source \
             whatever is planted there, so the spawn is refused rather than silently \
             measured against it",
            path.display(),
            name
        )));
    }
    // read+execute only: the emptiness checked a moment ago is a fact about
    // one instant, and the window between it and the child's startup is
    // exactly what a plant would aim for
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o500))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// A scratch directory of this test's own, outside the shared hermetic
    /// path that live spawns in this same binary are preparing concurrently.
    fn scratch(name: &str) -> PathBuf {
        let dir = empty_search_path()
            .with_file_name("view-env-tests")
            .join(name);
        // permissions restored first: a previous run left the directory
        // unwritable on purpose, and remove_dir_all cannot unlink through it
        #[cfg(unix)]
        if dir.exists() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_hermetic_search_path_is_empty_once_prepared() {
        let path = prepare_empty_search_path().unwrap();
        assert!(path.is_dir(), "{} is not a directory", path.display());
        assert!(
            std::fs::read_dir(&path).unwrap().next().is_none(),
            "{} holds an entry, so a hermetic child can source it",
            path.display()
        );
    }

    #[test]
    fn a_search_path_holding_anything_is_refused() {
        let dir = scratch("planted");
        std::fs::create_dir_all(dir.join("nvim/plugin")).unwrap();
        std::fs::write(dir.join("nvim/plugin/host.lua"), "-- planted").unwrap();
        let refused = prepare_empty_dir(&dir).unwrap_err();
        assert!(
            refused.to_string().contains("nvim"),
            "the refusal does not name what it found: {refused}"
        );
    }

    #[test]
    fn the_allowlist_admits_every_name_it_enumerates() {
        for name in HERMETIC_PASSTHROUGH_VARS {
            assert!(
                is_hermetic_passthrough(OsStr::new(name)),
                "{name} is on the allowlist and is dropped anyway, so a \
                 hermetic child loses a variable it was meant to keep"
            );
        }
        for prefix in HERMETIC_PASSTHROUGH_PREFIXES {
            let member = format!("{prefix}VIEW_MEMBER");
            assert!(
                is_hermetic_passthrough(OsStr::new(&member)),
                "{member} carries an allowlisted prefix and is dropped anyway"
            );
        }
    }

    #[test]
    fn the_allowlist_drops_a_name_no_list_enumerates() {
        assert!(
            !is_hermetic_passthrough(OsStr::new("VIEW_UNENUMERATED_HOST_VAR")),
            "an unenumerated name is treated as passthrough, so the allowlist \
             is a denylist wearing the other name"
        );
        // a name that merely contains an allowlisted one, and one that would
        // match a prefix only after folding case: neither is the variable the
        // allowlist named, and admitting either would widen the list silently
        assert!(!is_hermetic_passthrough(OsStr::new("MY_PATH")));
        assert_eq!(
            is_hermetic_passthrough(OsStr::new("lc_view_member")),
            cfg!(windows),
            "a lowercase prefix must be admitted exactly where the host's own \
             name comparison folds case, and nowhere else"
        );
    }

    /// The `LC_` prefix is three bytes long, which is inside a multi-byte
    /// character for names of this shape. Unix accepts any byte but `=` and
    /// NUL in a variable name, so such a name reaches this from a real host
    /// environment, and slicing a `str` at that index panics.
    #[test]
    fn a_prefix_test_survives_a_name_that_is_not_sliceable_there() {
        assert!(!is_hermetic_passthrough(OsStr::new("abé")));
    }

    #[test]
    fn the_sweep_names_a_host_variable_the_allowlist_rejects() {
        let swept = hermetic_sweep();
        assert!(
            !swept.is_empty(),
            "this host exports nothing the allowlist rejects, so every \
             assertion about the sweep here passes vacuously"
        );
        for (name, value) in &swept {
            assert!(
                !is_hermetic_passthrough(name),
                "{name:?} is allowlisted and swept anyway"
            );
            assert_eq!(
                std::env::var_os(name).as_ref(),
                Some(value),
                "{name:?} is swept with a value the host does not hold, so a \
                 caller's own override cannot be told apart from it"
            );
        }
        for name in HERMETIC_PASSTHROUGH_VARS {
            assert!(
                !swept
                    .iter()
                    .any(|(swept, _)| env_names_eq(swept, OsStr::new(name))),
                "{name} is allowlisted and reached the sweep"
            );
        }
    }

    /// The case-folding rule, exercised on every platform rather than only
    /// on the one that selects it. Written against
    /// [`names_eq_folding_case`] directly because a `cfg!(windows)` arm
    /// tested only under `cfg(windows)` is tested nowhere the suite
    /// ordinarily runs.
    #[test]
    fn the_folding_rule_collapses_a_pair_of_spellings_windows_collapses() {
        for (left, right) in [
            ("PATH", "path"),
            ("Path", "pAtH"),
            ("SystemRoot", "SYSTEMROOT"),
            // beyond ASCII, where an `eq_ignore_ascii_case` fold answers no
            // and the operating system answers yes
            ("VIEWé", "VIEWÉ"),
            ("Ünicode", "ÜNICODE"),
            ("ЯName", "яNAME"),
        ] {
            assert!(
                names_eq_folding_case(OsStr::new(left), OsStr::new(right)),
                "{left} and {right} are one variable to Windows and two here"
            );
        }
    }

    #[test]
    fn the_folding_rule_keeps_apart_what_windows_keeps_apart() {
        for (left, right) in [
            ("PATH", "PATHEXT"),
            ("MY_PATH", "PATH"),
            ("", "PATH"),
            // the sharp s uppercases to two characters, which the system's
            // one-to-one mapping does not do, so these stay two variables
            ("straße", "STRASSE"),
        ] {
            assert!(
                !names_eq_folding_case(OsStr::new(left), OsStr::new(right)),
                "{left} and {right} are two variables to Windows and one here"
            );
        }
    }

    /// The other rule, and the one-line choice between them: whichever arm
    /// this platform selects, the public comparison must answer as that arm
    /// does.
    #[test]
    fn the_host_comparison_is_the_rule_this_platform_actually_applies() {
        assert_eq!(
            env_names_eq(OsStr::new("Path"), OsStr::new("PATH")),
            cfg!(windows),
            "the case rule in force is not the one this platform applies"
        );
        assert!(env_names_eq(OsStr::new("PATH"), OsStr::new("PATH")));
        assert!(!env_names_eq(OsStr::new("PATH"), OsStr::new("PATHEXT")));
    }

    /// The path the subprocess-configuration layers select must be missing,
    /// and must sit where nobody can create it: a readable file there is a
    /// host configuration reaching the child's own subprocesses, which is
    /// the channel those layers exist to close.
    #[test]
    fn the_absent_config_file_is_absent_and_cannot_be_planted() {
        let prepared = prepare_empty_search_path().unwrap();
        let absent = absent_config_file();
        assert!(
            !absent.exists(),
            "{} exists, so a hermetic child's git reads it",
            absent.display()
        );
        assert_eq!(
            absent.parent(),
            Some(prepared.as_path()),
            "the absent config path sits outside the directory that is made \
             unwritable, so anybody can plant a config file at it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_prepared_search_path_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("prepared");
        prepare_empty_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o500,
            "the prepared search path stays writable, so a plugin can still be planted in it"
        );
    }
}
