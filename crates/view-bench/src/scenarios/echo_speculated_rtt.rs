//! The RTT-injection variant of [`crate::scenarios::echo_speculated`]:
//! the identical scenario, driven with the measured `view` side reached
//! over `--remote` through a userspace latency relay instead of spawned
//! directly -- the evidence a real SSH round trip is the one figure
//! nowhere in this tree, and speculation's payoff for it has only ever
//! been arithmetic against the `echo.minimal` bar.
//!
//! # Why a relay, not `tc netem`
//!
//! Real network-namespace latency injection needs privileges a CI host
//! cannot be assumed to hold. [`arm_delay_relay_path`] instead arms a
//! `PATH` entry whose `ssh` resolves to `scripts/test-fixtures/delay-relay`
//! -- a userspace byte relay that wraps the committed stand-in `ssh` client
//! ([`view_oracle::remote::stub_client`]) and sleeps a configurable
//! duration per chunk relayed, in both directions. The trick is the one
//! [`crate::scenarios::remote_memory::arm_stub_ssh_path`] already uses to
//! select a double at all: the `view` CLI's `--remote` flag has no
//! `--ssh-bin` of its own, so `PATH` is the only lever standing between an
//! invocation and which client answers `ssh`.
//!
//! # Why the tap shim is duplicated here rather than imported
//!
//! [`taps::shim_taps_spec`] is imported, not duplicated -- the FD-opening
//! contract it encodes (`VIEW_BENCH_TAP_PATH`, `VIEW_BENCH_TAP_FD`) is
//! this crate's own and this module composes it rather than re-deriving
//! it, same as every other taps-instrumented row.
//!
//! # What stays local
//!
//! Only the measured `view` process's own engine reaches for the far side;
//! `view` itself, and the bare-nvim reference side
//! [`crate::scenarios::echo_speculated::run`] pairs it against, both run on
//! this host exactly as every other row's do. That is what makes the tap
//! FIFO -- a filesystem path, not a wire protocol -- reachable at all: the
//! stub's far side is this host ([`view_oracle::remote`]'s own module doc),
//! so the announcements the instrumented binary writes land on the same
//! filesystem the harness reads them from, unlike a genuinely remote
//! target.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::scenarios::taps;
use crate::session::SpawnSpec;
use crate::BenchError;

/// The destination the delay-relay-wrapped stub is given: parsed and
/// dropped by `fake-ssh` exactly as [`crate::scenarios::remote_memory`]'s
/// own target is.
pub const STUB_TARGET: &str = "view-rtt-acceptance-stub-host";

/// The four RTT tiers the acceptance proof brackets: same-region SSH
/// (tens of ms), cross-region (low hundreds), and a zero-delay control
/// that isolates the relay's own overhead from an injected figure.
pub const RTT_TIERS_MS: [u64; 4] = [0, 25, 100, 300];

/// The committed userspace byte relay (`scripts/test-fixtures/delay-relay`)
/// this module arms in front of the stand-in `ssh` client.
///
/// A committed file rather than one written at run time, for the identical
/// `ETXTBSY` reason [`view_oracle::remote::stub_client`] is one: a program
/// written and then executed by a parallel test binary races a sibling's
/// `fork` holding the file open across the write.
#[must_use]
pub fn delay_relay_client() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    root.join("scripts")
        .join("test-fixtures")
        .join("delay-relay")
}

/// Whether this host can run the RTT-tiered leg at all: the relay is a
/// Python script wrapping a POSIX-shell stand-in client, so both need to
/// be present and executable, and the whole leg is unix-only for the same
/// reason [`crate::scenarios::remote_memory`]'s stub leg is.
#[must_use]
pub fn delay_relay_available() -> bool {
    cfg!(unix) && delay_relay_client().is_file() && view_oracle::remote::stub_available()
}

/// Prepares `dir` to stand in for a `PATH` entry whose `ssh` resolves to
/// the delay-relay fixture, and returns the `PATH` value a spawn should
/// carry plus the environment entries the relay itself reads
/// (`DELAY_RELAY_MS`, `DELAY_RELAY_INNER`).
///
/// A symlink rather than a copy, and a stale link replaced rather than
/// trusted, for the same reasons
/// [`crate::scenarios::remote_memory::arm_stub_ssh_path`] gives for its own
/// identical arming step.
///
/// # Errors
///
/// [`BenchError::Desync`] if this host has no delay relay or no stub
/// client to wrap (a `PATH` entry pointing at nothing is a spawn that
/// fails opaquely at the client, not here), or if `dir` cannot be created
/// or the symlink cannot be placed.
pub fn arm_delay_relay_path(
    dir: &Path,
    existing: Option<&OsStr>,
    rtt_ms: u64,
) -> Result<(OsString, Vec<(OsString, OsString)>), BenchError> {
    if !delay_relay_available() {
        return Err(BenchError::Desync {
            context: format!(
                "no delay relay and stub ssh client pair on this host ({} / {} is not an \
                 executable pair), so a PATH entry in {} would point ssh at nothing",
                delay_relay_client().display(),
                view_oracle::remote::stub_client().display(),
                dir.display()
            ),
        });
    }
    std::fs::create_dir_all(dir).map_err(|source| BenchError::Desync {
        context: format!(
            "creating the delay-relay PATH directory {}: {source}",
            dir.display()
        ),
    })?;
    let link = dir.join("ssh");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(delay_relay_client(), &link).map_err(|source| {
        BenchError::Desync {
            context: format!(
                "symlinking {} to the delay-relay fixture: {source}",
                link.display()
            ),
        }
    })?;
    let mut path = OsString::from(dir);
    if let Some(existing) = existing {
        path.push(":");
        path.push(existing);
    }
    let env = vec![
        (
            OsString::from("DELAY_RELAY_MS"),
            OsString::from(rtt_ms.to_string()),
        ),
        (
            OsString::from("DELAY_RELAY_INNER"),
            view_oracle::remote::stub_client().into_os_string(),
        ),
    ];
    Ok((path, env))
}

/// Builds the tap-instrumented, `--remote`-routed view spawn spec for one
/// RTT tier: a view invocation armed with a `PATH` whose `ssh` resolves
/// through the delay relay to the committed stub client, wrapped in the
/// tap FIFO shim so the row's usual attribution channel still applies.
///
/// `env` carries whatever hermetic overrides the caller's side setup
/// already resolved (XDG homes, `TERM`, ...); `PATH` is appended by this
/// function rather than left to the caller, so the delay-relay arming
/// step and the `PATH` value it produces can never drift apart.
///
/// # Errors
///
/// Whatever [`arm_delay_relay_path`] reports.
pub fn remote_rtt_view_spec(
    cwd: PathBuf,
    mut env: Vec<(OsString, OsString)>,
    view_taps_bin: &Path,
    nvim_bin: &Path,
    scratch_file: &Path,
    tap_path: &Path,
    rtt_ms: u64,
) -> Result<SpawnSpec, BenchError> {
    let stub_dir = cwd.join("delay-relay-ssh-path");
    let (path, extra_env) =
        arm_delay_relay_path(&stub_dir, std::env::var_os("PATH").as_deref(), rtt_ms)?;
    env.push((OsString::from("PATH"), path));
    env.extend(extra_env);
    let inner = SpawnSpec {
        program: view_taps_bin.to_path_buf(),
        args: vec![
            OsString::from("--nvim-bin"),
            nvim_bin.as_os_str().to_os_string(),
            OsString::from("--remote"),
            OsString::from(STUB_TARGET),
            scratch_file.as_os_str().to_os_string(),
        ],
        env,
        cwd: Some(cwd),
    };
    Ok(taps::shim_taps_spec(inner, tap_path))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use view_test_support::ScratchDir;

    /// `cat` on every unix this module already requires (see the crate-wide
    /// `#[cfg(unix)]` on this module): a plain, dependency-free passthrough
    /// to wrap, decoupled from the stub `ssh` client's own argv-reparsing
    /// contract, which this test has no reason to exercise.
    fn cat_path() -> Option<&'static str> {
        ["/bin/cat", "/usr/bin/cat"]
            .into_iter()
            .find(|p| Path::new(p).is_file())
    }

    /// The delay relay's own falsifiable contract: it adds the configured
    /// delay deterministically, within a stated tolerance band -- this is
    /// a scripted proof of the fixture's own behavior, not a bench-grade
    /// latency measurement, so the band is wide on purpose.
    ///
    /// One message makes a there-and-back trip through the relay (write to
    /// the relay's stdin, read its stdout), crossing two relayed hops --
    /// caller-to-`cat` and `cat`-to-caller -- each sleeping the configured
    /// delay once. The band is `[2 * DELAY_MS - 5ms, 2 * DELAY_MS +
    /// 500ms]`: the lower bound is causal (minus a 5ms guard for the clock
    /// resolution the test's own timer and Python's `time.sleep` rounding
    /// can differ by), and the fixed 500ms upper slack is a
    /// scheduler-jitter allowance sized for a shared, possibly loaded
    /// dev/CI host running Python threads under contention -- a fixed
    /// floor of noise rather than a multiple of the configured delay,
    /// which is why it does not scale with `DELAY_MS` below.
    #[test]
    fn delay_relay_adds_the_configured_delay_within_a_stated_tolerance() {
        if !delay_relay_client().is_file() {
            eprintln!("skipped: no delay relay fixture on this host");
            return;
        }
        let Some(cat) = cat_path() else {
            eprintln!("skipped: no cat binary found to wrap");
            return;
        };

        const DELAY_MS: u64 = 40;
        let mut child = Command::new(delay_relay_client())
            .env("DELAY_RELAY_MS", DELAY_MS.to_string())
            .env("DELAY_RELAY_INNER", cat)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawning the delay relay over cat must succeed");
        let mut stdin = child.stdin.take().expect("relay stdin must be piped");
        let stdout = child.stdout.take().expect("relay stdout must be piped");
        let mut reader = BufReader::new(stdout);

        let start = Instant::now();
        stdin
            .write_all(b"hello from the jitter-tolerance test\n")
            .expect("writing the probe line must succeed");
        stdin.flush().expect("flushing the probe line must succeed");
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("reading the relayed line back must succeed");
        let elapsed = start.elapsed();
        drop(stdin);
        let _ = child.wait();

        assert_eq!(
            line.trim_end(),
            "hello from the jitter-tolerance test",
            "the relay must pass bytes through unchanged, not just delayed"
        );
        let floor = Duration::from_millis(2 * DELAY_MS).saturating_sub(Duration::from_millis(5));
        let ceiling = Duration::from_millis(2 * DELAY_MS) + Duration::from_millis(500);
        assert!(
            elapsed >= floor && elapsed <= ceiling,
            "a there-and-back trip through a {DELAY_MS}ms relay took {elapsed:?}, outside the \
             stated tolerance band [{floor:?}, {ceiling:?}]"
        );
    }

    /// [`arm_delay_relay_path`]'s own claim: after arming, `dir/ssh`
    /// resolves (via a `PATH` built from its return value) to the same
    /// file [`delay_relay_client`] names, and the relay's own two
    /// environment reads are both present.
    #[test]
    fn armed_path_resolves_ssh_to_the_committed_relay() {
        if !delay_relay_available() {
            eprintln!("skipped: no delay relay / stub ssh client pair on this host");
            return;
        }
        let scratch = ScratchDir::new("echo-speculated-rtt-arm").expect("creating the scratch dir");
        let (path, env) = arm_delay_relay_path(&scratch, std::env::var_os("PATH").as_deref(), 25)
            .expect("arming a fresh scratch directory must succeed");
        let resolved = std::env::split_paths(&path)
            .find_map(|entry| {
                let candidate = entry.join("ssh");
                candidate.is_file().then_some(candidate)
            })
            .expect("the returned PATH must resolve an executable ssh");
        let resolved = std::fs::canonicalize(&resolved).expect("canonicalizing the resolved ssh");
        let relay = std::fs::canonicalize(delay_relay_client())
            .expect("canonicalizing the committed delay relay");
        assert_eq!(
            resolved, relay,
            "the first ssh the armed PATH resolves must be the committed delay relay"
        );
        assert!(
            env.iter().any(|(k, v)| k == "DELAY_RELAY_MS" && v == "25"),
            "the armed env must carry the configured tier's delay in milliseconds, got {env:?}"
        );
        assert!(
            env.iter().any(|(k, _)| k == "DELAY_RELAY_INNER"),
            "the armed env must point the relay at the stub client to wrap, got {env:?}"
        );
    }

    /// Re-arming the same directory replaces a stale link rather than
    /// leaving it, mirroring
    /// [`crate::scenarios::remote_memory::arm_stub_ssh_path`]'s own test.
    #[test]
    fn rearming_replaces_a_stale_link() {
        if !delay_relay_available() {
            eprintln!("skipped: no delay relay / stub ssh client pair on this host");
            return;
        }
        let scratch =
            ScratchDir::new("echo-speculated-rtt-restale").expect("creating the scratch dir");
        let stale_target = scratch.join("not-a-real-relay");
        std::fs::write(&stale_target, "#!/bin/sh\nexit 1\n").expect("writing a stale target");
        std::os::unix::fs::symlink(&stale_target, scratch.join("ssh"))
            .expect("planting a stale link");

        arm_delay_relay_path(&scratch, None, 0).expect("re-arming an already-populated directory");
        let resolved =
            std::fs::read_link(scratch.join("ssh")).expect("reading the link back after re-arming");
        assert_eq!(
            resolved,
            delay_relay_client(),
            "re-arming must replace the stale link, not leave it pointing at the old target"
        );
    }
}
