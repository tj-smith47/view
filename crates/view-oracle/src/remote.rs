//! The remote-spawn side of the differential stack: view's own engine
//! reached over an `ssh` client instead of started on this host, held
//! against the local path it is supposed to be indistinguishable from.
//!
//! # Why a stand-in client, and what makes one honest
//!
//! A CI host cannot be assumed to have a reachable `sshd` or any network
//! egress at all, so the gated coverage runs against
//! [`stub_client`] -- a committed POSIX script that stands in for the
//! OpenSSH client without a network hop.
//!
//! The one client behaviour that matters here is the one it reproduces.
//! Real `ssh` parses its own connection options locally, then joins
//! **everything trailing the destination** with spaces into a single string
//! and hands that string to the remote user's shell to re-parse; it does
//! not preserve the caller's argument boundaries. A double that instead
//! exec'd its trailing arguments directly would be strictly more forgiving
//! than the client it stands for: it would run a caller whose quoting is
//! wrong and a real remote shell would reject, and every assertion made
//! through it about a value surviving to the far side would be vacuous.
//! [`RemoteCase::StubFlattening`] is what keeps that from being a claim --
//! it drives the stub with two trailing arguments and reads back whether a
//! shell re-parsed the join.
//!
//! What the stub does not reproduce is equally worth naming: authentication,
//! host-key verification, `~/.ssh/config` resolution, the remote user's
//! login environment, and the network itself. The command runs on this host,
//! as this user. `crates/view-oracle/tests/remote_real_ssh.rs` is the
//! opt-in variant that covers those against a real target, and its own doc
//! comment says which of them it proves.
//!
//! # What is compared
//!
//! Nothing about a remote session is supposed to differ from a local one
//! past the transport, so the cases here are equality checks rather than
//! feature assertions: the same script, the same file, and the same probe on
//! both paths, diffed with the corpus runner's own comparison layer
//! ([`crate::compare`]). The corpus itself is re-run through this path by
//! the `oracle` runner, which is the broadest form of the same claim.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use view_engine::process::{EngineConfig, RemoteSpec};

use crate::{compare, masked_rows, snapshot, Divergence, EngineSession, ReferenceSide, ViewSide};
use crate::{OracleError, Screen};

#[cfg(test)]
mod tests;

/// Terminal size every case runs at, matching the corpus runner's own fixed
/// canvas so a report line from either reads against the same geometry.
const COLS: u16 = 60;
const ROWS: u16 = 12;

/// The quiesce window every case settles on, matching the corpus runner's
/// own defaults: a comparison made at a different settle point than the
/// corpus makes is not comparable with the corpus's own verdict.
const SILENCE: Duration = Duration::from_millis(120);
const DEADLINE: Duration = Duration::from_secs(10);

/// The destination the stub is given. It is never resolved or connected to
/// -- the stub parses it and drops it, exactly as a real client parses a
/// destination and never sends it -- but it must still look like a hostname,
/// because a leading dash is refused by the spawn path as option injection.
const STUB_TARGET: &str = "view-oracle-stub-host";

/// The file the parentless-parent case opens, on both paths. Absolute, so
/// neither session's own working directory can decide what it names, and
/// under a first component nothing creates: the case asserts its absence
/// before it proves anything with it.
const PARENTLESS_FILE: &str = "/no/such/dir/view-remote-oracle.txt";

/// The committed stand-in for the OpenSSH client
/// (`scripts/test-fixtures/fake-ssh`).
///
/// A committed file rather than one written at run time: a program written
/// and then executed by a parallel test binary is a race, since a sibling's
/// `fork` between the write and the `exec` holds the file open and the
/// `exec` fails with `ETXTBSY`.
#[must_use]
pub fn stub_client() -> PathBuf {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop(); // crates/
    root.pop(); // workspace root
    root.join("scripts").join("test-fixtures").join("fake-ssh")
}

/// Whether this host can run a case at all: the stub is a POSIX shell
/// script, so a Windows host has no client to stand in and no shell to
/// re-parse a joined command line.
///
/// A predicate rather than a `cfg!` at each call site, so a runner reports
/// the leg it skipped instead of a caller silently compiling it out.
#[must_use]
pub fn stub_available() -> bool {
    cfg!(unix) && stub_client().is_file()
}

/// The spec every case and the corpus runner's own remote leg spawn
/// through: the committed stub, at a destination that is parsed and
/// discarded.
#[must_use]
pub fn stub_spec() -> RemoteSpec {
    RemoteSpec::new(STUB_TARGET).with_ssh_bin(stub_client())
}

/// An [`EngineSession`] reached through [`stub_spec`], for a caller driving
/// its own script (the corpus runner's remote leg) rather than one of the
/// cases below.
///
/// # Errors
///
/// [`OracleError::Io`] if this host has no stub to run (see
/// [`stub_available`]), and otherwise whatever
/// [`EngineSession::spawn_remote`] reports.
pub fn spawn_stub_session(cols: u16, rows: u16) -> Result<EngineSession, OracleError> {
    refuse_without_stub()?;
    EngineSession::spawn_remote(cols, rows, stub_spec())
}

/// One case of the remote battery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoteCase {
    /// The stand-in client's own fidelity, which everything else here rests
    /// on: two trailing arguments must reach the far side joined and
    /// re-parsed by a shell, never preserved as the argument vector the
    /// caller built.
    StubFlattening,
    /// A file whose parent directory does not exist, opened and then
    /// written through both paths. The remote path is required to fail
    /// exactly as the local one does -- the same error, the same state, the
    /// same screen -- which is the claim that no remote-specific
    /// path-handling exists to get this wrong.
    ParentlessOpen,
}

impl RemoteCase {
    /// Every case, in the order a bare run drives them: the stub's own
    /// fidelity first, since a failure there makes the rest unreadable.
    #[must_use]
    pub const fn all() -> [Self; 2] {
        [Self::StubFlattening, Self::ParentlessOpen]
    }

    /// This case's name on a report line and on the runner's own selector.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::StubFlattening => "stub-flattening",
            Self::ParentlessOpen => "parentless-open",
        }
    }
}

/// What one case found.
///
/// A divergence list rather than a bool, on the corpus runner's own terms:
/// a report that only says something differed leaves whoever reads it to
/// reproduce the case before they can start. [`Divergence::State`]'s two
/// sides are the remote path (`view`) against the local one (`reference`)
/// throughout, including for [`RemoteCase::StubFlattening`], where the
/// reference side is what a real client's own flattening produces.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RemoteReport {
    /// The case this report is for.
    pub case: RemoteCase,
    /// Wall time the case took, for a report line to carry.
    pub elapsed_ms: u128,
    /// Every disagreement found. Empty is the only passing state.
    pub divergences: Vec<Divergence>,
}

impl RemoteReport {
    /// Whether the case passed: an empty divergence list and nothing else.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.divergences.is_empty()
    }

    /// This report's line, in the corpus runner's own report shape so a
    /// remote run reads against a corpus run without translation.
    #[must_use]
    pub fn report_line(&self) -> String {
        let status = if self.is_success() {
            "PARITY"
        } else {
            "DIVERGENCE"
        };
        format!(
            "oracle: remote {} ... {status} ({}ms)",
            self.case.label(),
            self.elapsed_ms
        )
    }
}

/// Runs one case against the stub client and a real pinned engine.
///
/// # Errors
///
/// [`OracleError::Io`] if this host has no stub to run (see
/// [`stub_available`]) or the case's own preconditions do not hold, and
/// otherwise whatever spawning or driving a session reports. A case that
/// runs and finds a disagreement returns `Ok` with a non-empty report --
/// only a case that could not be run at all is an error.
pub fn run_case(case: RemoteCase) -> Result<RemoteReport, OracleError> {
    refuse_without_stub()?;
    let start = Instant::now();
    let divergences = match case {
        RemoteCase::StubFlattening => stub_flattening()?,
        RemoteCase::ParentlessOpen => parentless_open()?,
    };
    Ok(RemoteReport {
        case,
        elapsed_ms: start.elapsed().as_millis(),
        divergences,
    })
}

/// Refuses, by name, on a host with no stub client to run.
fn refuse_without_stub() -> Result<(), OracleError> {
    if stub_available() {
        return Ok(());
    }
    Err(OracleError::Io(std::io::Error::other(format!(
        "no stand-in ssh client to run: {} is not an executable POSIX script \
         on this host, so nothing here can drive a remote path",
        stub_client().display()
    ))))
}

/// [`RemoteCase::StubFlattening`]: the stub is handed the same two trailing
/// arguments a caller would build, and what a shell made of them is read
/// back.
///
/// `echo` and one argument holding a space, piped into a word count. Both
/// halves of the result are load-bearing. A real client joins the trailing
/// arguments into `echo hello world | wc -w`, which the remote shell parses
/// as a pipeline and answers `2`: the quoted value word-split, and shell
/// syntax in a trailing argument became shell syntax. A double that
/// preserved argument boundaries would run `echo` with four literal
/// arguments and echo them back.
///
/// `echo` alone cannot tell the two apart -- it joins its own argument
/// vector with spaces, so both spellings print the same line -- which is why
/// the count is read rather than the echo.
fn stub_flattening() -> Result<Vec<Divergence>, OracleError> {
    let output = std::process::Command::new(stub_client())
        .arg("-T")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg(STUB_TARGET)
        .arg("echo")
        .arg("hello world")
        .arg("|")
        .arg("wc")
        .arg("-w")
        .output()?;
    let observed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if observed == "2" {
        return Ok(Vec::new());
    }
    Ok(vec![Divergence::State {
        field: String::from("stub_reparse"),
        view: format!(
            "{observed:?} (stderr {:?})",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        reference: String::from("\"2\""),
    }])
}

/// [`RemoteCase::ParentlessOpen`]: the same nonexistent-parent path opened
/// and written on both paths, and every observable held against the other.
///
/// The write is what produces an error at all -- opening such a path is not
/// itself a failure in nvim, it is a new buffer whose directory happens not
/// to exist -- and it is `silent!` so the two sessions' screens stay
/// comparable rather than both carrying an error line whose agreement would
/// be the only thing measured.
///
/// The remote spawn path resolves nothing about a path locally and adds no
/// error handling of its own, so this is an equality check by construction.
/// It is asserted rather than assumed because the alternative is invisible:
/// a remote path that quietly rewrote a caller's argument against some local
/// working directory would open a different file and report success.
fn parentless_open() -> Result<Vec<Divergence>, OracleError> {
    let parent = Path::new(PARENTLESS_FILE)
        .parent()
        .unwrap_or(Path::new("/"));
    if parent.exists() {
        return Err(OracleError::Io(std::io::Error::other(format!(
            "{} exists on this host, so opening {PARENTLESS_FILE} proves \
             nothing about a parent that does not",
            parent.display()
        ))));
    }

    let opening = |cfg: EngineConfig| -> Result<EngineSession, OracleError> {
        EngineSession::spawn_configured(cfg.with_arg(OsStr::new(PARENTLESS_FILE)), COLS, ROWS)
    };
    let mut remote = opening(EngineConfig::isolated().with_remote(stub_spec()))?;
    let mut local = opening(EngineConfig::isolated())?;

    let mut divergences = Vec::new();
    let (remote_probe, remote_view) = write_and_probe(&mut remote)?;
    let (local_probe, local_view) = write_and_probe(&mut local)?;
    // the local path is the authority this case compares against, so a
    // local path that reported nothing leaves two empty strings agreeing
    // with each other and a case that passes having measured nothing
    if local_probe.is_empty() {
        return Err(OracleError::Io(std::io::Error::other(format!(
            "writing {PARENTLESS_FILE} through a local engine reported no \
             error at all, so there is no failure here for the remote path to \
             be held against"
        ))));
    }
    if remote_probe != local_probe {
        divergences.push(Divergence::State {
            field: String::from("v:errmsg"),
            view: remote_probe,
            reference: local_probe,
        });
    }

    let remote_state = snapshot(&mut remote)?;
    let local_state = snapshot(&mut local)?;
    divergences.extend(compare(
        ViewSide {
            state: &remote_state,
            screen: &remote_view.screen,
        },
        ReferenceSide {
            state: &local_state,
            screen: &local_view.screen,
        },
        &remote_view.mask,
    ));
    Ok(divergences)
}

/// One session's captured frame: what [`compare`] diffs, and the mask the
/// diff runs under, taken from the same settled frame.
struct Captured {
    screen: Screen,
    mask: Vec<u16>,
}

/// Settles `session`, writes its buffer, settles again, and returns
/// `v:errmsg` alongside the frame the settle left.
///
/// `<Cmd>...<CR>` rather than a typed `:` command line, the same
/// mode-agnostic mechanism this crate's quiesce hooks use and for the same
/// reason: it runs without moving the session out of whatever mode it is in,
/// so the state probe afterwards reads the script's own final state.
fn write_and_probe(session: &mut EngineSession) -> Result<(String, Captured), OracleError> {
    let _ = session.quiesce(SILENCE, DEADLINE)?;
    let _ = session.surface();
    session.arm_and_input("<Cmd>silent! write<CR>")?;
    let _ = session.quiesce(SILENCE, DEADLINE)?;
    let errmsg = session.eval_str("v:errmsg")?;
    let surface = session.surface();
    let screen = session.screen();
    Ok((
        errmsg,
        Captured {
            mask: masked_rows(&surface),
            screen,
        },
    ))
}
