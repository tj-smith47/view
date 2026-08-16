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
//! - `HOME` -- absent from the *removal* list only: unset, libc and LuaJIT
//!   cannot resolve a home at all and the child's own `expand('~')` fails,
//!   so removal is the wrong neutralizer. An earlier revision passed the
//!   host's value through so the tooling a hermetic child spawns (git above
//!   all) would keep resolving its own configuration; that reasoning
//!   survives only where these variables are never set -- the shipped
//!   editor's default spawn -- because for a hermetic child, a subprocess
//!   resolving credentials and configuration out of the operator's home is
//!   exactly the channel being closed. [`HERMETIC_HOME_VAR`] records where
//!   a hermetic child's `HOME` points instead.
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
/// *itself* spawns read none of git's configuration *files* from the host.
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
/// These two close the configuration-file layers and nothing below them:
/// what a subprocess resolves through `HOME` without consulting any
/// configuration file at all -- `$HOME/.netrc`, `$HOME/.ssh/`, the
/// `core.excludesFile` default -- is closed by [`HERMETIC_HOME_VAR`]
/// instead, because no variable of git's or curl's diverts those.
pub const HOST_SUBPROCESS_CONFIG_VARS: &[&str] = &["GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"];

/// The variable every remaining `HOME`-resolved lookup rides, re-pointed at
/// a guarded directory of the harness's own for a hermetic child rather
/// than passed through from the host or removed.
///
/// [`HOST_SUBPROCESS_CONFIG_VARS`] diverts git's configuration files and
/// nothing else. Below that layer, git's http transport asks libcurl for
/// `$HOME/.netrc` unconditionally, a default `ssh` reads `$HOME/.ssh/`
/// (config, known hosts, identities), and the `core.excludesFile` default
/// falls back to `$HOME/.config/git/ignore` -- none selectable by an
/// environment variable of its own, so overriding `HOME` itself is the only
/// move that closes the family rather than its enumerated members. The
/// credential half is the one that matters: an authenticated fetch succeeds
/// where an anonymous one would 404 or rate-limit, so a fixture can be
/// green on the operator's machine and red everywhere else, silently.
///
/// The value is [`hermetic_home`], a directory of the harness's own whose
/// preparation ([`prepare_hermetic_home`]) refuses every entry outside the
/// state a child legitimately leaves (`.cache`, `.local/state` -- read by
/// nothing that resolves configuration, credentials, or code): the paths a
/// subprocess derives from this `HOME` are therefore absent at every spawn,
/// and a plant is a refused spawn rather than a silent read. Removal
/// instead of override would leave libc, LuaJIT and the child's own
/// `expand('~')` with no home at all.
///
/// On Windows this diverts the tooling that resolves its home through
/// `HOME` first (git and the curl it bundles do); tooling that resolves the
/// *profile* instead -- Windows OpenSSH reads `~/.ssh` from the account
/// profile, not `HOME` -- still reaches the operator's, because the profile
/// variables stay passthrough: process creation and the child's own
/// profile-shaped lookups need them. That residual is accepted, not closed.
pub const HERMETIC_HOME_VAR: &str = "HOME";

/// What a hermetic spawn points [`HOST_SEARCH_PATH_VARS`] and
/// [`HOST_SUBPROCESS_CONFIG_VARS`] at when the child runs on a *remote*
/// host, in place of [`empty_search_path`] and [`absent_config_file`].
///
/// Those two are directories this host creates, empties and makes
/// unwritable before every spawn ([`prepare_empty_search_path`]), which is
/// a preparation a caller holding one command string for a far-side shell
/// cannot perform. Naming them anyway would point the remote child at
/// whatever happens to sit at those absolute paths *there*, which is the
/// opposite of the guarantee their names carry.
///
/// The character device POSIX requires every host to have is the same
/// neutralizer without the preparation: `XDG_CONFIG_DIRS=/dev/null` puts
/// `/dev/null/nvim` on 'runtimepath', which is not a directory, so it
/// contributes no `plugin/` script and no `mkdir` can ever make it
/// contribute one -- unplantable by construction rather than by a
/// permission bit somebody has to set. Git reads the same path as an empty
/// configuration file, which selects none of the host's, exactly as a
/// missing one does.
///
/// It is a path, not a removal, for the reason those two lists exist:
/// unset, Neovim substitutes the remote system's own `/etc/xdg` and
/// `/usr/share`, so clearing them would select the far side's system-wide
/// configuration instead of no configuration.
pub const REMOTE_UNPLANTABLE_PATH: &str = "/dev/null";

/// The names a hermetic plan removes from a child running on a *remote*
/// host, in place of [`hermetic_sweep`]'s inversion of the passthrough
/// allowlist.
///
/// The inversion is exhaustive for a local child, whose environment is this
/// process's own: sweeping every non-allowlisted name here leaves the
/// allowlist and nothing else. A remote child inherits the *far side's*
/// login environment instead -- what its sshd, PAM and non-interactive shell
/// startup files export -- and of this host's own names only the handful in
/// [`CLIENT_FORWARDED_VARS`] can cross at all, so a name enumerated here is
/// almost always a name that does not exist there. Carrying the inversion
/// across therefore buys no coverage and costs two things worth keeping: the
/// remote command line
/// stops being a function of the invoking shell (the same
/// `view --remote host:path` builds a different string on two machines, so a
/// user's bug report cannot be reproduced from it), and the complete list of
/// variable *names* this host exports -- `AWS_PROFILE`, `KUBECONFIG`, a
/// vendor token's name -- crosses to the remote account and sits in its `ps`
/// output for the life of the exec.
///
/// What is enumerated here instead comes from the two directions a variable
/// can arrive at the far-side child by:
///
/// - The family this module opens by naming: the four standard-path
///   variables, which a remote login profile does set and which redirect
///   every `stdpath()` lookup the far-side editor makes, out from under the
///   directories a caller pointed at private ones. Removal is their
///   neutralizer for the reason it is locally -- unset, the editor derives
///   all four from the home it was given, and the remote child's home is
///   the one place the far side's own state legitimately lives.
/// - Whatever [`CLIENT_FORWARDED_VARS`] can carry *from this host* that a
///   local hermetic child would not receive. Only `NO_COLOR` is in that
///   position: it is not passthrough, so a local hermetic child never sees
///   the host's, and a forwarded one would change what the far-side editor
///   renders. The rest of that list is passthrough, so a client forwarding
///   it makes the same decision the local plan already made, and removing
///   it here would leave a remote child with a *different* locale from the
///   local child it is supposed to be indistinguishable from.
///
/// The list is deliberately short of an inversion, and
/// [`crate::process::EngineConfig::env_plan`] states what that leaves live
/// on the far side.
pub const REMOTE_SWEEP_VARS: &[&str] = &[
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_CACHE_HOME",
    "NO_COLOR",
];

/// The variables an `ssh` client can carry from this host to the far side
/// on its own, before any plan of this module's reaches the command line.
///
/// The client sends nothing it is not configured to send, but stock
/// configurations do configure some: this host's own
/// `/etc/ssh/ssh_config` carries `SendEnv LANG LC_* COLORTERM NO_COLOR`, and
/// a server admits exactly what its `AcceptEnv` names (a stock `sshd`
/// admits `LANG` and `LC_*`; `AcceptEnv *` is common in CI images). So the
/// premise that a remote child's environment is purely the far side's is
/// false for this handful, and every name here is either hermetic
/// passthrough -- in which case forwarding it is the same decision
/// [`HERMETIC_PASSTHROUGH_VARS`] already made -- or listed in
/// [`REMOTE_SWEEP_VARS`] so the plan removes it on arrival.
///
/// The `LC_` names are the POSIX categories plus the glibc extensions,
/// enumerated rather than derived: `SendEnv LC_*` is a glob over whatever
/// this host exports, and a list built by asking the local client
/// (`ssh -G`) would put the invoking machine's configuration back into the
/// remote command line, which is the property [`REMOTE_SWEEP_VARS`] exists
/// to hold. A locally-configured `SendEnv` naming something exotic is the
/// residual, and `EngineConfig::env_plan` says so.
pub const CLIENT_FORWARDED_VARS: &[&str] = &[
    "LANG",
    "LANGUAGE",
    "COLORTERM",
    "NO_COLOR",
    "LC_ALL",
    "LC_COLLATE",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_ADDRESS",
    "LC_IDENTIFICATION",
    "LC_MEASUREMENT",
    "LC_NAME",
    "LC_PAPER",
    "LC_TELEPHONE",
];

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
/// - `USER`, `LOGNAME`, `SHELL` and the Windows profile variables: identity
///   resolution that libc and the child's tooling read. `HOME` is
///   deliberately not among them: a child needs one, but everything a
///   subprocess resolves through the host's -- credentials first -- is host
///   state, so the hermetic layer supplies its own value
///   ([`HERMETIC_HOME_VAR`]) instead of passing the host's through.
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
/// two can differ for characters added between their versions. The spawn
/// builders fold differently again -- `portable_pty` keys its map with a
/// full `str::to_lowercase`, `std`'s `Command` defers to the OS upcase --
/// so an exotic pair that one-to-many lowercasing merges but simple
/// uppercasing keeps apart can be one variable to a builder and two here.
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
/// "hermetic" child would then source it.
///
/// A build-machine path baked into a released binary never resolves at run
/// time, and never has to: only [`crate::process::EngineConfig::isolated`]
/// consults this, and the shipped editor spawns through
/// `EngineConfig::default` (pinned by test in the `view` binary itself).
#[must_use]
pub fn empty_search_path() -> PathBuf {
    build_target_dir().join("view-hermetic-empty")
}

/// The workspace `target/` directory, where every hermetic directory this
/// module hands a child lives: resolved from this crate's own manifest dir
/// because the crate that owns the same derivation for the harness bins
/// sits above this one in the dependency order.
fn build_target_dir() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    root.join("target")
}

/// The directory a hermetic child receives as its `HOME`
/// ([`HERMETIC_HOME_VAR`]), prepared by [`prepare_hermetic_home`] at the
/// same funnels that prepare [`empty_search_path`].
///
/// A directory of its own rather than [`empty_search_path`], because a home
/// is *written* by its legitimate holders: an embedded Neovim creates
/// `$HOME/.local/state/nvim` for its log the moment it starts when no
/// `XDG_STATE_HOME` redirects it, so pointing `HOME` at the directory whose
/// emptiness every spawn re-checks would let the first child's state veto
/// every spawn after it. Under the build tree for the same reason as the
/// search path: the system temp dir is world-writable with a guessable
/// name, and a home is the last directory to leave plantable.
#[must_use]
pub fn hermetic_home() -> PathBuf {
    build_target_dir().join("view-hermetic-home")
}

/// Creates [`hermetic_home`] if absent and verifies it holds nothing a
/// child's subprocess would read configuration, credentials, or code out
/// of, returning the path a hermetic spawn points [`HERMETIC_HOME_VAR`] at.
///
/// Emptiness is the wrong invariant for a home -- its holders write state
/// under it, see [`hermetic_home`] -- so the check tolerates exactly the
/// entries a child leaves behind (`.cache`, and `.local` holding only
/// `state`) and refuses everything else. The entries that must never appear
/// are the ones a subprocess resolves *inputs* through (`.netrc`,
/// `.gitconfig`, `.ssh/`, `.config/`, and `.local/share`, whose
/// `nvim/site/plugin/` sits on 'runtimepath'), and rather than enumerate
/// those and miss one, an unexpected name is treated as a plant: the spawn
/// fails naming it, never runs against it.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the directory cannot be
/// created or read, and an [`io::Error::other`] naming the offending entry
/// and the recovery if it holds anything outside the tolerated state.
pub fn prepare_hermetic_home() -> io::Result<PathBuf> {
    let path = hermetic_home();
    prepare_home_dir(&path)?;
    Ok(path)
}

/// The body of [`prepare_hermetic_home`], taking its path as an argument so
/// a test can exercise the refusal against a directory of its own instead
/// of planting an entry in the one every concurrent spawn in the same test
/// binary is reading.
fn prepare_home_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    for entry in std::fs::read_dir(path)? {
        // byte-exact comparisons on purpose: on a case-insensitive
        // filesystem a case-variant of a tolerated name reaches the child
        // as the same directory, and refusing it is the safe side of that
        // ambiguity
        let entry = entry?;
        let name = entry.file_name();
        if name == ".cache" {
            continue;
        }
        if name == ".local" {
            // a `.local` that is not a real directory -- a planted file, or
            // a symlink resolving elsewhere -- is not the state a child
            // leaves either, and reading through it would surface a raw
            // io error naming neither the entry nor the recovery
            if !entry.file_type()?.is_dir() {
                return Err(home_refusal(path, Path::new(".local")));
            }
            // tolerated for the state below it only: `.local/share` is
            // `XDG_DATA_HOME`'s fallback, and `<data>/nvim/site/plugin/`
            // sits on 'runtimepath' -- executable ground, unlike the logs
            // and shada a child leaves in `.local/state`
            for entry in std::fs::read_dir(path.join(".local"))? {
                let name = entry?.file_name();
                if name != "state" {
                    return Err(home_refusal(path, &Path::new(".local").join(name)));
                }
            }
            continue;
        }
        return Err(home_refusal(path, Path::new(&name)));
    }
    // An embedded Neovim creates `$HOME/.local/state/nvim/swap` on demand
    // when it opens its first buffer, and two children starting together
    // both find it missing and both mkdir it: the loser's mkdir returns
    // EEXIST, which nvim reports as E303 and then refuses the buffer, so a
    // concurrent spawn fails for a reason that has nothing to do with what
    // it was doing. `create_dir_all` is idempotent under the same race, so
    // creating the directory retires it rather than leaving the outcome to
    // whichever child wins. After the scan above, never before it: a
    // planted non-directory `.local` must be diagnosed by the refusal that
    // names it, not by a raw ENOTDIR from this line.
    std::fs::create_dir_all(path.join(".local").join("state").join("nvim").join("swap"))?;
    Ok(())
}

/// Restores [`hermetic_home`] to the state every spawn accepts by deleting
/// it outright, for a caller that has just run code it does not vet and
/// cannot enumerate what that code's subprocesses wrote under the home:
/// the compat harness's daily-config scenario sources the maintainer's
/// live editor config, whose startup tooling writes real state there (a
/// Go toolchain invoked by a plugin manager creates `$HOME/go`, for one
/// observed case). Deletion rather than a selective sweep because the
/// home's own contract is that it holds nothing durable (see
/// [`prepare_hermetic_home`]'s refusal message), so removing the whole
/// directory is always a correct recovery, and the next spawn's
/// preparation recreates it empty.
///
/// # Errors
///
/// Returns the underlying `std::io::Error` if the directory exists and
/// cannot be removed. An absent directory is already the restored state,
/// not an error.
pub fn reset_hermetic_home() -> io::Result<()> {
    reset_home_dir(&hermetic_home())
}

/// The body of [`reset_hermetic_home`], taking its path as an argument for
/// the same reason [`prepare_home_dir`] does: a test exercising the reset
/// must not delete the home every concurrent spawn in the same test binary
/// is reading.
fn reset_home_dir(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            // The observed contaminant is exactly this shape: a Go module
            // cache marks its directories read-only, and unlinking an entry
            // needs write permission on its parent directory, so a plain
            // recursive delete dies partway through the tree.
            make_tree_deletable(path)?;
            match std::fs::remove_dir_all(path) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            }
        }
        Err(err) if err.kind() != io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Restores owner write (and, on Unix, traverse) permission on every
/// directory under `path`, the precondition [`reset_home_dir`]'s recursive
/// delete needs: unlink permission comes from the entry's parent directory,
/// so only directories matter and files are left untouched. Symlinks are
/// not followed; their targets sit outside the tree being deleted.
fn make_tree_deletable(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Ok(());
    }
    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(permissions.mode() | 0o700);
    }
    #[cfg(not(unix))]
    #[allow(clippy::permissions_set_readonly_false)]
    // Not a world-writability hazard: the tree is being deleted, and on
    // non-Unix the read-only attribute is the only bit blocking that.
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions)?;
    for entry in std::fs::read_dir(path)? {
        make_tree_deletable(&entry?.path())?;
    }
    Ok(())
}

/// The refusal [`prepare_home_dir`] raises, naming the entry and the way
/// out: the directory holds nothing durable, so deleting it is always a
/// correct recovery, and a message that only cried "plant" would leave the
/// realistic other cause -- a child's own subprocess writing an unexpected
/// dotfile, `ssh` creating `known_hosts` first of all -- looking
/// unrecoverable.
fn home_refusal(home: &Path, entry: &Path) -> io::Error {
    io::Error::other(format!(
        "the hermetic home {} holds {:?}, which is not among the state \
         entries a child leaves behind: whether it was planted or written by \
         a child's own subprocess, a subprocess would read configuration or \
         credentials from it, so the spawn is refused. The directory holds \
         nothing durable; delete that entry (or the whole directory) to \
         recover",
        home.display(),
        entry
    ))
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

    /// Every name a client can carry across on its own is accounted for: it
    /// is either something a hermetic child is meant to receive anyway, or
    /// something the remote plan removes on arrival.
    ///
    /// The gap this closes is silent by construction. A forwarded name that
    /// is neither reaches an `isolated` remote child carrying the invoking
    /// host's value, while the local child of the same config never sees it
    /// -- one machine's environment deciding what the far side does, with
    /// nothing failing.
    #[test]
    fn every_name_a_client_forwards_is_kept_on_purpose_or_removed_on_arrival() {
        for name in CLIENT_FORWARDED_VARS {
            let kept = is_hermetic_passthrough(OsStr::new(name));
            let removed = REMOTE_SWEEP_VARS
                .iter()
                .any(|swept| env_names_eq(OsStr::new(swept), OsStr::new(name)));
            assert!(
                kept != removed,
                "{name} is {}: a client can forward it, so it must either be \
                 passthrough (a hermetic child is meant to have it) or on the \
                 remote removal list (the plan takes it back), and exactly one \
                 of those",
                if kept {
                    "both passthrough and removed remotely"
                } else {
                    "neither passthrough nor removed remotely"
                }
            );
        }
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
    /// and must sit inside the hardened directory: a readable file there is
    /// a host configuration reaching the child's own subprocesses, which is
    /// the channel those layers exist to close.
    ///
    /// Parentage is what this pins on every platform; what parentage buys
    /// differs. On Unix the prepared directory is unwritable
    /// (`a_prepared_search_path_is_not_writable`), so planting fails
    /// outright. Windows has no cheap equivalent -- the readonly attribute
    /// does not stop file creation inside a directory -- so there the
    /// enforcement is the emptiness refusal at every later spawn, pinned by
    /// `a_file_planted_at_the_absent_config_path_refuses_the_next_spawn`.
    #[test]
    fn the_absent_config_file_is_absent_and_sits_in_the_hardened_directory() {
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
            "the absent config path sits outside the directory whose \
             emptiness every spawn re-checks, so a planted config file \
             there would never be noticed"
        );
    }

    /// The state a child derives from its `HOME` when nothing redirects it
    /// -- an embedded Neovim's log directory is the live case -- must not
    /// veto the spawns that come after it.
    #[test]
    fn a_hermetic_home_tolerates_the_state_a_child_writes_under_it() {
        let dir = scratch("home-state");
        std::fs::create_dir_all(dir.join(".local/state/nvim")).unwrap();
        std::fs::create_dir_all(dir.join(".cache/nvim")).unwrap();
        prepare_home_dir(&dir).unwrap();
    }

    /// An embedded Neovim mkdirs its swap directory on first buffer open,
    /// so two children starting together race for it and the loser's
    /// EEXIST surfaces as E303 with the buffer refused. Preparation owns
    /// the directory instead, which is only a fix while it holds for a
    /// home in every starting state -- absent, freshly made, and already
    /// carrying the state a previous child left.
    #[test]
    fn the_swap_directory_exists_before_any_child_can_race_for_it() {
        let swap = Path::new(".local/state/nvim/swap");
        let dir = scratch("home-swap-race");

        prepare_home_dir(&dir).unwrap();
        assert!(
            dir.join(swap).is_dir(),
            "a fresh home left {} to whichever child mkdirs it first",
            swap.display()
        );

        // idempotent over the state it just wrote, since every spawn
        // re-prepares the one home the whole workspace shares
        prepare_home_dir(&dir).unwrap();
        assert!(dir.join(swap).is_dir());

        // and over a home a child already wrote its own state into,
        // where `.local/state/nvim` exists but the swap directory does not
        let used = scratch("home-swap-race-used");
        std::fs::create_dir_all(used.join(".local/state/nvim")).unwrap();
        std::fs::write(used.join(".local/state/nvim/log"), b"startup").unwrap();
        prepare_home_dir(&used).unwrap();
        assert!(
            used.join(swap).is_dir(),
            "a home carrying a previous child's state still races for the swap directory"
        );
    }

    /// A home an unvetted child contaminated must come back to the state
    /// every spawn accepts, and resetting an already-absent home is the
    /// restored state, not an error.
    #[test]
    fn resetting_a_contaminated_home_lets_the_next_spawn_prepare_it() {
        let dir = scratch("home-reset");
        std::fs::create_dir_all(dir.join("go/pkg")).unwrap();
        prepare_home_dir(&dir).unwrap_err();
        reset_home_dir(&dir).unwrap();
        assert!(!dir.exists(), "the reset left the contaminated home behind");
        reset_home_dir(&dir).unwrap();
        prepare_home_dir(&dir).unwrap();
    }

    /// The observed contaminant is a Go module cache, which marks its
    /// directories read-only and thereby blocks a plain recursive delete.
    #[cfg(unix)]
    #[test]
    fn resetting_a_home_holding_read_only_directories_still_deletes_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("home-reset-read-only");
        let cache_dir = dir.join("go/pkg/mod");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("module.txtar"), b"cached").unwrap();
        std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        reset_home_dir(&dir).unwrap();
        assert!(!dir.exists(), "the reset left the read-only home behind");
    }

    /// Anything else under a hermetic home is input a child's subprocess
    /// would read -- a credential file first of all -- and the spawn must
    /// refuse it by name rather than run against it. The refusal must also
    /// name the recovery, because it stops every spawn workspace-wide until
    /// someone acts on it.
    #[test]
    fn a_credential_planted_in_the_hermetic_home_refuses_the_next_spawn() {
        let dir = scratch("home-planted");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".netrc"), "machine github.com").unwrap();
        let refused = prepare_home_dir(&dir).unwrap_err();
        assert!(
            refused.to_string().contains(".netrc"),
            "the refusal does not name the planted credential file: {refused}"
        );
        assert!(
            refused.to_string().contains("delete"),
            "the refusal names no recovery, leaving a workspace-wide spawn \
             stop that only source-reading resolves: {refused}"
        );
    }

    /// `.local` is tolerated as the directory a child's state lives under;
    /// the same name as a file (or a symlink resolving elsewhere) is not
    /// that state, and the refusal must name it and its recovery rather
    /// than surface the raw error reading through it would raise.
    #[test]
    fn a_home_whose_local_is_not_a_directory_refuses_the_next_spawn() {
        let dir = scratch("home-local-file");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".local"), "not a directory").unwrap();
        let refused = prepare_home_dir(&dir).unwrap_err();
        assert!(
            refused.to_string().contains(".local"),
            "the refusal does not name the non-directory entry: {refused}"
        );
        assert!(
            refused.to_string().contains("delete"),
            "the refusal names no recovery: {refused}"
        );
    }

    /// `.local` earns its tolerance for the state below it, not wholesale:
    /// `.local/share` is `XDG_DATA_HOME`'s fallback, and a file under
    /// `<data>/nvim/site/plugin/` is sourced by any child that neither
    /// redirects the data home nor passes `--clean` -- executable ground,
    /// not state.
    #[test]
    fn a_plugin_planted_under_the_homes_data_fallback_refuses_the_next_spawn() {
        let dir = scratch("home-data-planted");
        std::fs::create_dir_all(dir.join(".local/share/nvim/site/plugin")).unwrap();
        let refused = prepare_home_dir(&dir).unwrap_err();
        assert!(
            refused.to_string().contains("share"),
            "the refusal does not name the planted data entry: {refused}"
        );
    }

    /// The half of the plant defense that holds on every platform: a file
    /// created at the absent-config path makes the hardened directory
    /// non-empty, and the next spawn is refused naming it rather than run
    /// against a plant.
    #[test]
    fn a_file_planted_at_the_absent_config_path_refuses_the_next_spawn() {
        let dir = scratch("planted-absent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("absent"), "[url]").unwrap();
        let refused = prepare_empty_dir(&dir).unwrap_err();
        assert!(
            refused.to_string().contains("absent"),
            "the refusal does not name the planted config file: {refused}"
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
