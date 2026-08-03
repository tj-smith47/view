//! One lock ordering this crate's whole test binary around the process
//! environment, plus the handle every environment mutation in these tests
//! goes through.
//!
//! `pty`'s inheritance tests plant variables in the real process
//! environment, because a value set on a [`CommandBuilder`] only proves the
//! builder copied it, not that a child inherits one it never set. That plant
//! is process-wide while it stands, and every other test in this binary runs
//! on the same thread pool: `pty`, `compat`, `parity` and `reference` all
//! compile into one test binary, and each of them spawns children. A child
//! spawned inside the plant's window inherits the planted value.
//!
//! Most children here pass a hermetic funnel that clears exactly the names
//! the plant uses ([`crate::pty::PtySession::spawn_configured`],
//! `EngineConfig::isolated`). One deliberately does not: the control spawn
//! in `pty`'s `a_pty_spawn_sources_no_plugin_from_the_hosts_search_path`
//! exists precisely to be unguarded, and it is the child a concurrent plant
//! reaches. What it inherits there is inert as things stand -- the pinned
//! binary skips `VIMINIT` under `--clean` (confirmed live), and that control
//! sets the two search-path variables itself -- but that is a coincidence
//! between one test's flags and another test's payload, and neither test
//! states it as a property. [`plant`] removes the coincidence from both
//! ends: it holds the lock exclusively for as long as the plant stands, and
//! it puts back what it replaced when the plant is dropped, so no test's
//! environment mutation is visible to another test's child.
//!
//! One spawn stays outside that guarantee for the same structural reason
//! the control spawn above does: `CompatSession::probe` spawns the
//! `--remote-expr` client from library code, which cannot reach this module
//! at all ([`plant`] and [`spawning`] are `#[cfg(test)]`). Nothing calls
//! `probe` outside a test today, and the one test that does wraps it in a
//! plant's own `spawning`, so the gap is structural rather than live -- but
//! it is stated here rather than left for a future caller to discover.
//!
//! Shared/exclusive rather than a plain mutex because that is the shape of
//! the hazard: two concurrent spawns do not disturb each other, only a
//! concurrent plant does, so spawns stay parallel and this costs the suite
//! no wall-clock time.
//!
//! On the torn-`environ` hazard that makes `std::env::set_var` `unsafe` in
//! edition 2024 (this workspace is edition 2021, so it compiles): between
//! the three APIs these tests actually use, the standard library already
//! excludes it on Unix. `std::env::set_var` takes an internal `ENV_LOCK`
//! exclusively, `std::env::vars_os` (which `CommandBuilder::new` snapshots
//! the environment with) takes it shared, and `std::process::Command`'s
//! `fork`/`posix_spawn` paths take it shared across the spawn. Confirmed in
//! the standard library source for the toolchain this tree builds with, and
//! confirmed the other way round by attempting to reproduce a fault at all:
//! a planting thread churning the environment against readers doing exactly
//! `vars_os` and `Command::spawn` never faulted, including under forced
//! reallocation of the environment array and freed-memory poisoning. That
//! exclusion is an implementation detail of the standard library rather than
//! a documented guarantee, and it does not extend to a reader inside a C
//! library reached through FFI, so this lock is still the right thing to
//! hold. It is not, however, what keeps these tests standing up today: what
//! this module actually buys is the inheritance window above.
//!
//! [`CommandBuilder`]: portable_pty::CommandBuilder

// the guard this module hands out is what makes an environment mutation
// bounded, so the calls implementing it are the sanctioned ones
#![allow(clippy::disallowed_methods)]

use std::ffi::{OsStr, OsString};
use std::sync::{PoisonError, RwLock, RwLockWriteGuard};

/// Orders environment mutation ([`plant`], exclusive) against child spawning
/// ([`spawning`], shared) across every test in this crate's binary.
static ENVIRON: RwLock<()> = RwLock::new(());

/// Runs `f` with the process environment held stable, and nothing else.
///
/// Every child spawn in this crate's tests belongs inside one of these,
/// including the [`CommandBuilder`](portable_pty::CommandBuilder)
/// construction that precedes a pty spawn: that constructor, not the spawn
/// call, is where the environment is copied for the child (`get_base_env` in
/// portable-pty 0.9.0).
///
/// Poisoning is ignored: this lock orders two operations and guards no data,
/// so a test that panicked while holding it left nothing behind for the next
/// one to find broken. Propagating the poison instead would turn one real
/// failure into a screenful of unrelated ones, none of which names what
/// actually broke.
pub(crate) fn spawning<R>(f: impl FnOnce() -> R) -> R {
    let _shared = ENVIRON.read().unwrap_or_else(PoisonError::into_inner);
    f()
}

/// Plants `vars` in this process's own environment, excluding every
/// [`spawning`] call in this binary until the returned handle is dropped,
/// which puts back whatever each name held before.
///
/// The only environment mutation in this crate's tests: a bare
/// `std::env::set_var` neither excludes the concurrent spawns that would
/// inherit it nor undoes itself, and the plants here are host-editor
/// configuration (`VIMINIT`, `XDG_CONFIG_DIRS`) that a later test's child
/// must not be left holding.
#[must_use]
pub(crate) fn plant(vars: &[(&str, &str)]) -> Planted {
    let exclusive = ENVIRON.write().unwrap_or_else(PoisonError::into_inner);
    let mut prior = Vec::with_capacity(vars.len());
    for (name, value) in vars {
        prior.push((OsString::from(name), std::env::var_os(name)));
        std::env::set_var(name, value);
    }
    Planted {
        prior,
        _exclusive: exclusive,
    }
}

/// A live plant from [`plant`]: the exclusive half of [`ENVIRON`], held for
/// as long as the planted values stand.
pub(crate) struct Planted {
    /// What each planted name held beforehand, in plant order.
    prior: Vec<(OsString, Option<OsString>)>,
    _exclusive: RwLockWriteGuard<'static, ()>,
}

impl Planted {
    /// [`spawning`], for the planting test's own spawns: the lock is already
    /// held exclusively here, and taking it a second time on the same thread
    /// would deadlock rather than exclude anything new.
    // Only the unix-gated spawn fixtures (compat's probe test, pty's env
    // report) plant-then-spawn; on Windows those are gated off, leaving this
    // method with no caller, so it follows them off the build.
    #[cfg(unix)]
    pub(crate) fn spawning<R>(&self, f: impl FnOnce() -> R) -> R {
        f()
    }
}

impl Drop for Planted {
    fn drop(&mut self) {
        // reverse order, so a name planted twice in one call ends at the
        // value it held before the first of them rather than between them
        for (name, prior) in self.prior.iter().rev() {
            restore(name, prior.as_deref());
        }
    }
}

/// Puts `name` back to `prior`, removing it outright when it had no value:
/// setting an empty string instead would leave a name a child can still see,
/// which for the search-path variables is a different meaning entirely (see
/// [`view_engine::env::HOST_SEARCH_PATH_VARS`]).
fn restore(name: &OsStr, prior: Option<&OsStr>) {
    match prior {
        Some(value) => std::env::set_var(name, value),
        None => std::env::remove_var(name),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    // The inheritance test spawns /bin/sh to read a child's real environment;
    // it is gated off Windows, and these imports serve only it.
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::sync::mpsc;
    #[cfg(unix)]
    use std::time::Duration;

    /// Names of this module's own, cleared by no funnel: a child that
    /// inherits one reports the leak instead of being quietly saved by the
    /// hermetic spawn path the real plants use. One per test, since the
    /// assertions below read them after releasing the lock, where a
    /// concurrent test planting the same name would answer for it.
    #[cfg(unix)]
    const INHERIT: &str = "VIEW_TESTENV_INHERIT";
    const RESTORE: &str = "VIEW_TESTENV_RESTORE";

    // Reads a spawned child's real environment via /bin/sh, so it cannot run
    // on Windows; the plant/restore test below is pure env logic and stays.
    #[cfg(unix)]
    #[test]
    fn a_child_spawned_while_a_plant_stands_inherits_none_of_it() {
        const NAME: &str = INHERIT;
        let (release, released) = mpsc::channel();
        let child_env = std::thread::spawn(move || {
            released.recv().unwrap();
            spawning(|| {
                Command::new("/bin/sh")
                    .arg("-c")
                    .arg(format!("printenv {NAME}"))
                    .output()
                    .expect("failed to spawn /bin/sh")
            })
        });

        let planted = plant(&[(NAME, "leaked-into-another-test")]);
        release.send(()).unwrap();
        // the spawning thread is released here and blocks on the lock; this
        // pause is what makes an unguarded spawn actually land inside the
        // plant's window rather than after it by luck of scheduling
        std::thread::sleep(Duration::from_millis(200));
        drop(planted);

        let output = child_env.join().unwrap();
        let seen = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert!(
            seen.is_empty(),
            "a child spawned by another thread inherited the plant: {NAME}={seen:?}"
        );
        assert!(
            std::env::var_os(NAME).is_none(),
            "the plant outlived its handle, so every later test in this binary carries it"
        );
    }

    #[test]
    fn a_plant_restores_what_each_name_held_before_it() {
        const NAME: &str = RESTORE;
        let planted = plant(&[(NAME, "first"), (NAME, "second")]);
        assert_eq!(
            std::env::var_os(NAME).as_deref(),
            Some(OsStr::new("second")),
            "the later plant of a repeated name did not win"
        );
        drop(planted);
        assert!(
            std::env::var_os(NAME).is_none(),
            "restoring left {NAME}={:?} behind rather than the absence it found",
            std::env::var_os(NAME)
        );
    }
}
