//! The `remote_memory` row: view's own process footprint, read the exact
//! way [`memory::run`] reads it, against a session whose engine is spawned
//! over `--remote` instead of locally -- the resource-claim ladder's fourth
//! rung, spec 3.1's "Remote editing: view's own local footprint once the
//! engine runs remotely". The claim under test is narrow and falsifiable:
//! the engine moving to a remote host must not grow view's *own* process,
//! since view holds no more buffer state remotely than it does locally.
//!
//! # Two legs, one driver
//!
//! [`run`] is a pass-through to [`memory::run`] -- the same workload
//! ([`memory::workload_files`], [`memory::workload_content`]), the same
//! settle/sample loop, the same own-process reader. The only thing that can
//! differ between this row and `memory.minimal` is which [`SpawnSpec`] the
//! caller built, which is exactly the property the falsifiable check needs:
//! two drivers sampling the same workload the same way would drift the
//! moment one changed and the other did not, while one function measured
//! through two specs cannot.
//!
//! The gated CI leg arms that spec against the committed stand-in
//! `ssh` client ([`view_oracle::remote`]) via [`arm_stub_ssh_path`]: a
//! `PATH` entry whose `ssh` resolves to the stub instead of a real client,
//! the same trick view's own `RemoteSpec::ssh_bin` documents as the only
//! lever the `view` binary's `--remote` flag leaves for selecting a
//! double, since the CLI exposes no `--ssh-bin` of its own. Because the
//! stub's far side is this host (`view_oracle::remote`'s own module doc),
//! the pinned local `nvim` binary's absolute path resolves there exactly
//! as it does locally.
//!
//! The opt-in leg reads [`REMOTE_HOST_ENV`] -- the same env var
//! `crates/view-oracle/tests/remote_real_ssh.rs` reads, so one exported
//! name configures both legs' real-SSH coverage -- and is exercised
//! **only** by this module's own `#[ignore]`d test. The gated bench matrix
//! (`crates/view-harness/src/bin/bench/remote_rows.rs`) does not read
//! [`REMOTE_HOST_ENV`] at all, unconditionally: a `--record`/`--gate` run
//! that measured a real network hop whenever an operator's shell happened
//! to carry a leftover export from an oracle acceptance run would silently
//! ratchet this class's bar against a different transport, on no evidence
//! any log line names. A real target is local/acceptance infrastructure the
//! CI host cannot be assumed to have, exactly as `remote_real_ssh.rs`'s own
//! doc explains, and it stays that way by construction: the gated path has
//! no branch that could read the var, not a branch that happens not to
//! today.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use crate::scenarios::memory::{self, MemoryOutcome};
use crate::scenarios::Protocol;
use crate::session::ViewSpec;
use crate::BenchError;

/// The destination `--remote` is given for the CI leg: parsed and dropped
/// by the stub exactly as `view_oracle::remote::stub_spec`'s own target is
/// (see that module for why it must still look like a hostname).
pub const STUB_TARGET: &str = "view-bench-stub-host";

/// Env var naming a real remote host for the opt-in leg, in `[user@]host`
/// syntax -- the same name `crates/view-oracle/tests/remote_real_ssh.rs`
/// reads.
pub const REMOTE_HOST_ENV: &str = "VIEW_REMOTE_TEST_HOST";

/// Env var naming the far-side `nvim` for the opt-in leg, mirroring
/// `remote_real_ssh.rs`'s own `VIEW_REMOTE_TEST_NVIM`: a real remote
/// account's `PATH` is not this host's, so a value here is the only way to
/// point at an `nvim` that PATH does not carry.
pub const REMOTE_NVIM_ENV: &str = "VIEW_REMOTE_TEST_NVIM";

/// Env var naming the `view` binary the opt-in leg's own test spawns,
/// mirroring the other two: the test has no other way to locate a release
/// build, since it is a `#[cfg(test)]` unit test rather than a bench-matrix
/// row that already carries `--view-bin` on its own command line.
pub const REMOTE_VIEW_BIN_ENV: &str = "VIEW_REMOTE_TEST_VIEW_BIN";

/// Reads view's own process memory after the standard workload, the same
/// way [`memory::run`] does, against whatever [`crate::session::SpawnSpec`]
/// the caller built. See this module's doc for why a pass-through is the
/// whole driver rather than a second one.
///
/// # Errors
///
/// Same as [`memory::run`].
pub fn run(view_spec: ViewSpec<'_>, protocol: &Protocol) -> Result<MemoryOutcome, BenchError> {
    memory::run(view_spec, protocol)
}

/// Prepares `dir` to stand in for a `PATH` entry whose `ssh` resolves to
/// the committed stand-in `ssh` client, and returns the `PATH` value a
/// spawn should carry: `dir` prepended to `existing`.
///
/// A symlink rather than a copy: the stub is a script this tree owns and
/// commits, so a symlink names it once and can never drift from a copy
/// taken at some earlier commit. Any stale link already at `dir/ssh` (an
/// earlier scratch world's leftover, or one pointing at a since-moved
/// checkout) is replaced rather than trusted, since an existence check
/// would hide exactly that drift.
///
/// # Errors
///
/// [`BenchError::Desync`] if this host has no stub client at all (a `PATH`
/// entry pointing at nothing is a spawn that fails opaquely at the client,
/// not here), or if `dir` cannot be created or the symlink cannot be
/// placed.
pub fn arm_stub_ssh_path(dir: &Path, existing: Option<&OsStr>) -> Result<OsString, BenchError> {
    if !view_oracle::remote::stub_available() {
        return Err(BenchError::Desync {
            context: format!(
                "no stand-in ssh client on this host ({} is not an executable POSIX script), \
                 so a PATH entry in {} would point ssh at nothing",
                view_oracle::remote::stub_client().display(),
                dir.display()
            ),
        });
    }
    std::fs::create_dir_all(dir).map_err(|source| BenchError::Desync {
        context: format!(
            "creating the stub-ssh PATH directory {}: {source}",
            dir.display()
        ),
    })?;
    let link = dir.join("ssh");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(view_oracle::remote::stub_client(), &link).map_err(|source| {
        BenchError::Desync {
            context: format!(
                "symlinking {} to the stub ssh client: {source}",
                link.display()
            ),
        }
    })?;
    let mut path = OsString::from(dir);
    if let Some(existing) = existing {
        path.push(":");
        path.push(existing);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::session::SpawnSpec;
    use std::path::PathBuf;
    use view_test_support::ScratchDir;

    /// `arm_stub_ssh_path`'s own claim: after arming, `dir/ssh` resolves
    /// (via a `PATH` built from its return value) to the same file
    /// [`view_oracle::remote::stub_client`] names.
    #[test]
    fn armed_path_resolves_ssh_to_the_committed_stub() {
        if !view_oracle::remote::stub_available() {
            eprintln!("skipped: no stub ssh client on this host");
            return;
        }
        let scratch = ScratchDir::new("remote-memory-test").expect("creating the scratch dir");
        let path = arm_stub_ssh_path(&scratch, std::env::var_os("PATH").as_deref())
            .expect("arming a fresh scratch directory must succeed");
        let resolved = std::env::split_paths(&path)
            .find_map(|entry| {
                let candidate = entry.join("ssh");
                candidate.is_file().then_some(candidate)
            })
            .expect("the returned PATH must resolve an executable ssh");
        let resolved = std::fs::canonicalize(&resolved).expect("canonicalizing the resolved ssh");
        let stub = std::fs::canonicalize(view_oracle::remote::stub_client())
            .expect("canonicalizing the committed stub client");
        assert_eq!(
            resolved, stub,
            "the first ssh the armed PATH resolves must be the committed stub, not some other \
             ssh already on PATH"
        );
    }

    /// Re-arming the same directory replaces a stale link rather than
    /// leaving it: the doc's own claim about what an existence check would
    /// hide.
    #[test]
    fn rearming_replaces_a_stale_link() {
        if !view_oracle::remote::stub_available() {
            eprintln!("skipped: no stub ssh client on this host");
            return;
        }
        let scratch = ScratchDir::new("remote-memory-restale").expect("creating the scratch dir");
        let stale_target = scratch.join("not-a-real-client");
        std::fs::write(&stale_target, "#!/bin/sh\nexit 1\n").expect("writing a stale target");
        std::os::unix::fs::symlink(&stale_target, scratch.join("ssh"))
            .expect("planting a stale link");

        arm_stub_ssh_path(&scratch, None).expect("re-arming an already-populated directory");
        let resolved =
            std::fs::read_link(scratch.join("ssh")).expect("reading the link back after re-arming");
        assert_eq!(
            resolved,
            view_oracle::remote::stub_client(),
            "re-arming must replace the stale link, not leave it pointing at the old target"
        );
    }

    /// [`REMOTE_HOST_ENV`]'s own module doc promises: a real remote target
    /// plus the view binary this crate's tests have no other way to
    /// locate, mirroring `remote_real_ssh.rs`'s loud-skip-when-unset
    /// convention exactly.
    ///
    /// ```sh
    /// export VIEW_REMOTE_TEST_HOST=a-dev-box
    /// export VIEW_REMOTE_TEST_VIEW_BIN=/path/to/release/view
    /// # optional, only when the far side's PATH does not carry nvim:
    /// export VIEW_REMOTE_TEST_NVIM=/opt/homebrew/bin/nvim
    /// cargo test -p view-bench remote_memory:: -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs VIEW_REMOTE_TEST_HOST, VIEW_REMOTE_TEST_VIEW_BIN, and a reachable ssh target"]
    fn remote_footprint_over_a_real_ssh_target_is_readable() {
        fn env_var(name: &str) -> Option<String> {
            match std::env::var(name) {
                Ok(value) if !value.is_empty() => Some(value),
                _ => None,
            }
        }

        let Some(host) = env_var(REMOTE_HOST_ENV) else {
            eprintln!("skipped: {REMOTE_HOST_ENV} is unset (see this module's doc)");
            return;
        };
        let Some(view_bin) = env_var(REMOTE_VIEW_BIN_ENV) else {
            eprintln!("skipped: {REMOTE_VIEW_BIN_ENV} is unset (see this module's doc)");
            return;
        };

        let mut args = vec![OsString::from("--remote"), OsString::from(host)];
        if let Some(nvim) = env_var(REMOTE_NVIM_ENV) {
            args.push(OsString::from("--nvim-bin"));
            args.push(OsString::from(nvim));
        }
        let spec = SpawnSpec {
            program: PathBuf::from(view_bin),
            args,
            env: Vec::new(),
            cwd: None,
        };
        let outcome = run(
            ViewSpec(&spec),
            &Protocol {
                samples: 20,
                warmup: 5,
                ..Protocol::default()
            },
        )
        .expect("a reachable remote target must produce a memory reading");
        assert!(
            outcome.gated_mb > 0.0,
            "memory::run's own positivity floor already guarantees this; restated here as the \
             acceptance leg's own claim about a real connection"
        );
    }
}
