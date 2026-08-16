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
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::scenarios::taps;
use crate::session::SpawnSpec;
use crate::BenchError;

/// The destination the delay-relay-wrapped stub is given: parsed and
/// dropped by `fake-ssh` exactly as [`crate::scenarios::remote_memory`]'s
/// own target is.
pub const STUB_TARGET: &str = "view-rtt-acceptance-stub-host";

/// The four RTT tiers the acceptance proof brackets: same-region SSH
/// (tens of ms), cross-region (low hundreds), and a `0` row that is a
/// floor-only control, not a zero-latency one -- the relay's own
/// coalescing window and interpreter overhead still apply at `0`, so this
/// row isolates *that* fixed floor (measured ~28ms round trip over `cat`)
/// from the configured delay the other three add on top of it, rather
/// than measuring an actual zero-transport-latency spawn.
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

/// Resolves a working Python interpreter for the delay relay's own
/// `#!/usr/bin/env python3` shebang, trying the same names in the same
/// order `scripts/crosscheck-god-files.sh` already does for its own
/// Python dependency (a Windows runner ships `python`, not `python3`).
/// "Working" means the candidate actually runs, not merely that a file by
/// that name sits on `PATH` -- a stub or a broken alias would otherwise
/// pass and fail later, opaquely, inside the relay's own exec.
fn python_interpreter() -> Option<&'static str> {
    ["python3", "python", "py"].into_iter().find(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// Why this host cannot run the RTT-tiered leg, checked in the order a
/// spawn would actually fail on each precondition -- so the first one that
/// fails is the one reported, instead of a spawn that fails opaquely
/// inside the relay's own exec (a missing interpreter looks identical to a
/// broken `view` spawn from the pty's side) and a contributor re-deriving
/// which precondition was missing by hand. `None` means the leg can run.
#[must_use]
pub fn delay_relay_unavailable_reason() -> Option<String> {
    if !cfg!(unix) {
        return Some("the RTT-tiered leg is unix-only".to_string());
    }
    if !delay_relay_client().is_file() {
        return Some(format!(
            "no delay relay fixture at {}",
            delay_relay_client().display()
        ));
    }
    if !view_oracle::remote::stub_available() {
        return Some(format!(
            "no stub ssh client at {}",
            view_oracle::remote::stub_client().display()
        ));
    }
    if python_interpreter().is_none() {
        return Some(
            "no python3 interpreter on PATH (tried python3, python, py); the delay relay's \
             `#!/usr/bin/env python3` shebang needs one"
                .to_string(),
        );
    }
    None
}

/// Whether this host can run the RTT-tiered leg at all: the relay is a
/// Python script wrapping a POSIX-shell stand-in client, so both need to
/// be present and executable (with a working interpreter to run the
/// former), and the whole leg is unix-only for the same reason
/// [`crate::scenarios::remote_memory`]'s stub leg is. See
/// [`delay_relay_unavailable_reason`] for which precondition failed when
/// this is `false`.
#[must_use]
pub fn delay_relay_available() -> bool {
    delay_relay_unavailable_reason().is_none()
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
/// [`BenchError::Desync`] for any reason [`delay_relay_unavailable_reason`]
/// names (no delay relay, no stub client to wrap, or no Python interpreter
/// to run the relay's own shebang -- a `PATH` entry pointing at nothing or
/// at an unrunnable script is a spawn that fails opaquely at the client,
/// not here), or if `dir` cannot be created or the symlink cannot be
/// placed.
pub fn arm_delay_relay_path(
    dir: &Path,
    existing: Option<&OsStr>,
    rtt_ms: u64,
) -> Result<(OsString, Vec<(OsString, OsString)>), BenchError> {
    if let Some(reason) = delay_relay_unavailable_reason() {
        return Err(BenchError::Desync {
            context: format!(
                "{reason}, so a PATH entry in {} would point ssh at nothing or fail opaquely at \
                 exec",
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

/// `cat` on every unix this module already requires (see the crate-wide
/// `#[cfg(unix)]` on this module): a plain, dependency-free passthrough to
/// wrap, decoupled from the stub `ssh` client's own argv-reparsing
/// contract, which a relay-latency probe has no reason to exercise.
#[must_use]
pub fn cat_path() -> Option<&'static str> {
    ["/bin/cat", "/usr/bin/cat"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
}

/// One there-and-back trip through a delay relay wrapping `cat`, configured
/// with `delay_ms`: writes a fixed probe line to the relay's stdin, reads
/// it back off the relay's stdout, and returns the wall time alongside the
/// line read back so a caller can check payload integrity and timing
/// separately.
///
/// The one latency figure in the RTT-tiered leg that speculation cannot
/// hide: nothing here goes anywhere near
/// [`crate::scenarios::echo_speculated`], whose own metrics are, by
/// design, largely insensitive to transport RTT (that insensitivity is the
/// feature under test) -- so a relay that silently stops adding delay
/// collapses this probe's result across tiers where a working relay's
/// still scales with `delay_ms`, giving a caller a falsifiable per-tier
/// signal the scenario's own outcome cannot.
///
/// # Errors
///
/// [`BenchError::Desync`] if the relay cannot be spawned, its piped stdio
/// cannot be taken, or the probe write/read-back fails.
pub fn round_trip_through_relay(
    delay_ms: u64,
    cat: &str,
) -> Result<(Duration, String), BenchError> {
    let mut child = Command::new(delay_relay_client())
        .env("DELAY_RELAY_MS", delay_ms.to_string())
        .env("DELAY_RELAY_INNER", cat)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|source| BenchError::Desync {
            context: format!(
                "spawning the delay relay over {cat} at DELAY_RELAY_MS={delay_ms}: {source}"
            ),
        })?;
    let mut stdin = child.stdin.take().ok_or_else(|| BenchError::Desync {
        context: "the delay relay's stdin was not piped".to_string(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| BenchError::Desync {
        context: "the delay relay's stdout was not piped".to_string(),
    })?;
    let mut reader = BufReader::new(stdout);

    let start = Instant::now();
    stdin
        .write_all(b"hello from the jitter-tolerance test\n")
        .map_err(|source| BenchError::Desync {
            context: format!("writing the relay probe line: {source}"),
        })?;
    stdin.flush().map_err(|source| BenchError::Desync {
        context: format!("flushing the relay probe line: {source}"),
    })?;
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|source| BenchError::Desync {
            context: format!("reading the relayed probe line back: {source}"),
        })?;
    let elapsed = start.elapsed();
    drop(stdin);
    let _ = child.wait();
    Ok((elapsed, line))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use view_test_support::ScratchDir;

    /// The delay relay's own falsifiable contract: it adds a delay that
    /// *tracks the configuration*, not a constant sleep and not an
    /// environment variable it silently ignores. A single measurement
    /// inside a wide tolerance band cannot tell "adds 40ms" apart from
    /// "adds a fixed ~40ms no matter what `DELAY_RELAY_MS` says" -- both
    /// land inside the same band -- so this measures at two settings and
    /// asserts on the *difference* between them, which only a relay that
    /// actually reads and applies the configuration produces.
    ///
    /// `LOW_MS = 0` and `HIGH_MS = 80`: `0` is reused rather than picking
    /// two arbitrary nonzero points because it is also this crate's own
    /// zero-delay tier ([`RTT_TIERS_MS`]), one fewer magic number, and the
    /// relay's own fixed overhead (interpreter startup, thread scheduling,
    /// the coalescing window -- a measured ~28ms round trip even at `0`,
    /// see the module doc) is present at both settings and cancels out of
    /// a difference regardless of which two points are chosen.
    ///
    /// Medians, not single points, on each side of the difference: this
    /// test runs under `cargo test --workspace`, sharing the host with
    /// every other test binary `task ci` runs in parallel, and a single
    /// trial's relay-subprocess spawn can be delayed by scheduler
    /// contention that has nothing to do with `DELAY_RELAY_MS` at all --
    /// observed in practice, where a single `LOW_MS` trial came back at
    /// 62ms against an idle-host baseline of ~28ms, pulling the diff below
    /// a single-trial band's floor on an otherwise-correct relay. A median
    /// of [`TRIALS`] independent trials per side cancels one-off spikes
    /// the same way this crate's own scenarios reduce to `_p50` medians
    /// rather than trusting a single sample.
    ///
    /// The band is `[2 * (HIGH_MS - LOW_MS) - 5ms, 2 * (HIGH_MS - LOW_MS) +
    /// 1000ms]`: the lower bound is causal minus the same 5ms
    /// clock-resolution guard the single-point band this test replaces
    /// used. The upper slack is doubled to 1000ms rather than the
    /// single-point band's 500ms, because a *difference* of two
    /// independent medians can still carry up to two independent
    /// scheduler-jitter excursions instead of one -- still a fixed floor of
    /// noise, not a multiple of the configured difference, for the same
    /// reason the single-point band's slack was fixed rather than scaled.
    #[test]
    fn delay_relay_scales_the_added_delay_with_the_configured_value() {
        if !delay_relay_client().is_file() {
            eprintln!("skipped: no delay relay fixture on this host");
            return;
        }
        let Some(cat) = cat_path() else {
            eprintln!("skipped: no cat binary found to wrap");
            return;
        };

        const LOW_MS: u64 = 0;
        const HIGH_MS: u64 = 80;
        const TRIALS: usize = 5;

        let mut low_elapsed = Vec::with_capacity(TRIALS);
        let mut high_elapsed = Vec::with_capacity(TRIALS);
        for _ in 0..TRIALS {
            let (elapsed, line) =
                round_trip_through_relay(LOW_MS, cat).expect("low-tier relay round trip");
            assert_eq!(
                line.trim_end(),
                "hello from the jitter-tolerance test",
                "the relay must pass bytes through unchanged, not just delayed (at \
                 DELAY_RELAY_MS={LOW_MS})"
            );
            low_elapsed.push(elapsed);

            let (elapsed, line) =
                round_trip_through_relay(HIGH_MS, cat).expect("high-tier relay round trip");
            assert_eq!(
                line.trim_end(),
                "hello from the jitter-tolerance test",
                "the relay must pass bytes through unchanged, not just delayed (at \
                 DELAY_RELAY_MS={HIGH_MS})"
            );
            high_elapsed.push(elapsed);
        }
        low_elapsed.sort_unstable();
        high_elapsed.sort_unstable();
        let low_median = low_elapsed[TRIALS / 2];
        let high_median = high_elapsed[TRIALS / 2];

        let diff = high_median.saturating_sub(low_median);
        let floor =
            Duration::from_millis(2 * (HIGH_MS - LOW_MS)).saturating_sub(Duration::from_millis(5));
        let ceiling = Duration::from_millis(2 * (HIGH_MS - LOW_MS)) + Duration::from_millis(1000);
        assert!(
            diff >= floor && diff <= ceiling,
            "a {HIGH_MS}ms relay's median round trip ({high_median:?} across {TRIALS} trials) \
             minus a {LOW_MS}ms relay's ({low_median:?} across {TRIALS} trials) was {diff:?}, \
             outside the stated tolerance band [{floor:?}, \
             {ceiling:?}] for the *difference* the configured delay must produce -- a relay that \
             ignores DELAY_RELAY_MS or adds a constant delay instead of scaling with it would \
             fail this the same way a relay that scales correctly would pass it"
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
