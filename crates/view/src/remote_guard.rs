//! Why a remote session did not start, in the terms a user can act on: the
//! ssh client this host does not have, and the connection the far side
//! refused.
//!
//! Two checks, one before the spawn and one after it fails. The first
//! resolves the client on `PATH` while the terminal is still the user's
//! own, because a remote session that cannot even start its client should
//! never take the screen over first. The second runs only once a spawn has
//! already failed, and answers the question the raw failure cannot: an
//! `ssh` that exits before ever answering the handshake looks, from the
//! engine's side, exactly like an editor that died -- the connection it
//! carried is invisible there.
//!
//! Deliberately standalone and small. The same two answers belong in a
//! `doctor` report as rows alongside every other environment check, and
//! this module is written to be absorbed by one rather than to become one:
//! the classification ([`Reachability`]) is separate from the prose, and
//! nothing here touches the terminal or the model.

use crate::startup::AttachFailure;
use anyhow::Result;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use view_engine::handle::EngineError;
use view_engine::process::RemoteSpec;

/// The command a diagnostic connection runs on the far side: the cheapest
/// thing a POSIX shell can be asked for, so its exit status reports on the
/// connection rather than on what it ran. `RemoteSpec` already assumes a
/// POSIX remote shell for the spawn's own command line.
const PROBE_COMMAND: &str = "true";

/// The exit status an OpenSSH client reserves for its own failures --
/// authentication, host-key verification, name resolution, connection
/// refused -- as opposed to the exit status of whatever ran on the far
/// side.
const SSH_CLIENT_FAILURE: i32 = 255;

/// How long a diagnostic connection is given to answer. The spawn's own
/// handshake budget, for the same reason it has one: the caller is a user
/// waiting on an editor that has already failed to start, and a diagnosis
/// that takes longer to obtain than the attempt it explains is not one
/// worth waiting for.
///
/// It is a ceiling, not an expectation. This connection is only ever opened
/// after a client already exited inside that same window, so the ordinary
/// case answers in the time that client took; a client that instead sits on
/// a port silently dropping packets is the case this bound exists for.
const PROBE_LIMIT: Duration = Duration::from_secs(5);

/// How often the diagnostic connection is checked for having ended, matching
/// the engine's own shutdown poll.
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How much of the client's own last line is quoted back. Long enough for
/// the diagnoses OpenSSH actually writes (`Permission denied (publickey).`,
/// `Host key verification failed.`), short enough that a chatty banner
/// cannot bury the message it is attached to.
const DETAIL_LIMIT: usize = 200;

/// Which host's absent-client remediation an absent-client message names.
///
/// A parameter rather than a `cfg!` read at the point of use so both
/// messages are assertable from any host: the Windows branch exists because
/// its remediation is a Windows optional feature, and a branch only its own
/// platform can ever see the text of is a branch no test on any other
/// platform can prove correct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientPlatform {
    /// A host where an ssh client is normally already installed, and an
    /// absent one is a package to install.
    Posix,
    /// Windows, where OpenSSH's client ships as an optional feature that is
    /// not present on every machine.
    Windows,
}

impl ClientPlatform {
    /// The platform this build runs on.
    fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else {
            Self::Posix
        }
    }
}

/// Whether the client named by a spec can be found before anything is
/// spawned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presence {
    /// A file exists at a resolved location.
    Found,
    /// Every location the search covers was checked, and none held it.
    Absent,
    /// The search could not be performed at all, so nothing is claimed.
    Undetermined,
}

/// How a client was looked for, which decides what a message about not
/// finding it is entitled to say.
///
/// The three cases are not interchangeable prose: telling a user their
/// client is missing from `PATH` when a path named it directly, or when no
/// `PATH` existed to search, sends them to fix something that was never
/// consulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientLookup {
    /// A bare program name, searched across the entries of a `PATH` this
    /// process could read.
    SearchedPath,
    /// A bare program name with no `PATH` set to search. Whatever default
    /// list the platform's own exec fallback consulted is not a list view
    /// saw, so nothing may be claimed about its contents.
    NoPathToSearch,
    /// A path, which is checked where it points and nowhere else.
    NamedPath,
}

/// Which of the three [`ClientLookup`] cases `program` falls under.
fn client_lookup(program: &Path, path_var: Option<&OsStr>) -> ClientLookup {
    if program.components().count() > 1 {
        ClientLookup::NamedPath
    } else if path_var.is_some() {
        ClientLookup::SearchedPath
    } else {
        ClientLookup::NoPathToSearch
    }
}

/// What a diagnostic connection to the destination found.
///
/// Separate from the prose that reports it: a `doctor` row wants the
/// finding, not a paragraph, and the classification is the part worth
/// keeping when this module is absorbed by one.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reachability {
    /// The client exited with its own failure status without running
    /// anything on the far side. `detail` is the client's own last line,
    /// when it wrote one.
    Rejected { detail: Option<String> },
    /// The client connected, authenticated, and ran a command that
    /// succeeded: whatever failed is on the far side, not in between.
    Reached,
    /// The client connected, but the far side answered the trivial command
    /// with a failure of its own.
    ShellRefused { code: i32 },
    /// Nothing can be concluded: the client could not be run, or ended on a
    /// signal rather than an exit status.
    Unknown,
}

/// Refuses a remote session whose ssh client is not on this host's `PATH`,
/// before the terminal is taken over and before anything is spawned, with a
/// message naming the remediation this platform actually has.
///
/// A one-sided check by design: it refuses only when every location the
/// search covers has been checked and none held the client. A `PATH` that
/// cannot be read at all, and a client named by path rather than by name,
/// are left to the spawn to report -- a wrong refusal here blocks a session
/// that would have worked, which is worse than the generic spawn error this
/// exists to improve on.
///
/// Executability is not checked, only existence: a client that is present
/// and not runnable is a different failure with a different message, and
/// this one would be a lie about it.
///
/// # Errors
///
/// The absent-client refusal itself, carrying the platform's own
/// remediation.
pub(crate) fn deny_absent_ssh(remote: &RemoteSpec) -> Result<()> {
    let path_var = std::env::var_os("PATH");
    let presence = resolve_client(
        &remote.ssh_bin,
        path_var.as_deref(),
        std::env::var_os("PATHEXT").as_deref(),
    );
    if presence == Presence::Absent {
        anyhow::bail!(absent_client_message(
            remote,
            ClientPlatform::current(),
            client_lookup(&remote.ssh_bin, path_var.as_deref()),
        ));
    }
    Ok(())
}

/// The context a failed remote spawn is reported with: the same
/// absent-client message when the client itself could not be found, a
/// connection diagnosis when the client ran and its child died before the
/// handshake, and a general pointer otherwise.
///
/// Called only for a session that asked for a remote host. A local spawn
/// keeps its own message, which names `--nvim-bin` and this host's `PATH` --
/// neither of which a remote session runs anything through.
pub(crate) fn spawn_failure_context(remote: &RemoteSpec, err: &EngineError) -> String {
    match err {
        // the client was resolved before the spawn, so reaching this means
        // it went away in between, or it sits somewhere the pre-spawn
        // search deliberately does not claim to cover
        EngineError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => absent_client_message(
            remote,
            ClientPlatform::current(),
            client_lookup(&remote.ssh_bin, std::env::var_os("PATH").as_deref()),
        ),
        // the client started and its stdout reached end of file without a
        // handshake response: the process behind the RPC channel is gone.
        // For a remote session that process is the client, and a client
        // that exits without running anything is the shape an authentication
        // or host-key refusal takes under BatchMode
        EngineError::Closed => reachability_message(remote, probe_connection(remote)),
        // the client was still working when view stopped waiting, so nothing
        // has failed yet and there is nothing to diagnose: a second
        // connection would sit in the same wait, and the caller is already
        // past the point of waiting for one
        EngineError::Timeout { timeout, .. } => format!(
            "view: nothing answered the editor handshake on {} within \
             {timeout:?}, and the ssh client was still working when view \
             stopped waiting. A client that has neither connected nor failed \
             by then is usually waiting on a port that silently drops packets \
             rather than refusing them (a firewall in between), or on a far \
             side that accepted the connection and is slow to start `{}`. \
             `ssh -o BatchMode=yes -o ConnectTimeout=5 {} '{} --version'` \
             separates the two: it fails the same way, and prints the \
             client's own reason for it.",
            remote.target, remote.remote_nvim_bin, remote.target, remote.remote_nvim_bin
        ),
        _ => general_message(remote),
    }
}

/// [`spawn_failure_context`] for a whole [`AttachFailure`], so a caller
/// matches the two arms once rather than re-deriving which of them a remote
/// diagnosis applies to.
///
/// Only the spawn arm is diagnosed. The attach arm describes a process that
/// started and answered the handshake, so its connection is already proven
/// and a second one would be answering a question nobody asked.
pub(crate) fn attach_failure_context(remote: &RemoteSpec, failure: &AttachFailure) -> String {
    match failure {
        AttachFailure::Spawn(err) => spawn_failure_context(remote, err),
        AttachFailure::Attach(_) => format!(
            "view: the remote editor on {} started and then failed to attach \
             or answer in time (the connection itself is up: it carried the \
             handshake). Check what the far side's own nvim configuration \
             does at startup -- a config that prompts or blocks there leaves \
             an embedded editor with a question no UI can answer.",
            remote.target
        ),
    }
}

/// The absent-client refusal: what was looked for and where, then the
/// remediation for that combination of `lookup` and `platform`.
///
/// The message claims exactly what `lookup` says was consulted. A client
/// named by path was never searched for on `PATH`, and a process with no
/// `PATH` set searched nothing at all; sending either of those users to
/// their `PATH` or their package manager points at something that had no
/// part in the failure, which is the kind of confident wrong answer a
/// generic spawn error at least does not give.
fn absent_client_message(
    remote: &RemoteSpec,
    platform: ClientPlatform,
    lookup: ClientLookup,
) -> String {
    let program = remote.ssh_bin.display();
    let missing = match lookup {
        ClientLookup::SearchedPath => {
            format!("no `{program}` was found on this host's PATH")
        }
        ClientLookup::NoPathToSearch => format!(
            "no `{program}` could be started, and this process has no PATH \
             set to look for one on"
        ),
        ClientLookup::NamedPath => {
            format!("nothing could be started at `{program}`, the client this session names")
        }
    };
    let opening = format!(
        "view: `--remote {}` needs the system ssh client, and {missing}. view \
         opens a remote session by running that client -- it speaks no ssh \
         protocol of its own -- so there is nothing here to connect with.",
        remote.target
    );
    // a named path is a client this session chose, so the remediation is
    // that choice and not the host's packaging: an install that puts `ssh`
    // on PATH changes nothing for a spawn pointed somewhere else
    if lookup == ClientLookup::NamedPath {
        return format!(
            "{opening} Name a client that exists at that path, or name none \
             and let the `ssh` on this host's PATH be the one that runs."
        );
    }
    match platform {
        ClientPlatform::Posix => format!(
            "{opening} Install the OpenSSH client (`apt install \
             openssh-client`, `dnf install openssh-clients`, `pacman -S \
             openssh`, `brew install openssh`), or put the client already on \
             this host onto PATH, and retry."
        ),
        ClientPlatform::Windows => format!(
            "{opening} Windows ships the OpenSSH client as an optional \
             feature, and it is not installed on every machine. Add it from \
             an elevated PowerShell with `Add-WindowsCapability -Online \
             -Name OpenSSH.Client~~~~0.0.1.0`, or through Settings > Apps > \
             Optional features > Add a feature > OpenSSH Client, then retry. \
             `Get-WindowsCapability -Online -Name OpenSSH.Client*` reports \
             whether it is present."
        ),
    }
}

/// The prose for a diagnosis.
fn reachability_message(remote: &RemoteSpec, found: Reachability) -> String {
    let target = &remote.target;
    match found {
        Reachability::Rejected { detail } => {
            let quoted = match detail {
                Some(detail) => format!(" The client's own last words: \"{detail}\"."),
                None => String::new(),
            };
            format!(
                "view: the ssh client could not open a session on {target}: it \
                 exited {SSH_CLIENT_FAILURE} without running anything there, \
                 which is what it does when the connection or the \
                 authentication is refused. view runs the client with \
                 `BatchMode=yes`, because an embedded editor has no way to \
                 render a password or host-key prompt and no keyboard to \
                 answer one with, so anything the client would normally ask \
                 about is refused immediately instead of waiting. The usual \
                 causes, in the order they bite: no key the host accepts (no \
                 agent running, or no identity loaded into it), the host \
                 missing from known_hosts or presenting a changed key, a \
                 destination that does not resolve, or nothing listening on \
                 the port. Reproduce it directly with `ssh -o BatchMode=yes \
                 {target}`, which prints the client's own diagnosis in \
                 full.{quoted}"
            )
        }
        Reachability::Reached => format!(
            "view: the ssh client reached {target} and authenticated -- a \
             plain command ran there and succeeded -- so the connection is \
             not what failed. The remote editor is: `{}` either did not \
             start on that host, or started and did not speak the \
             msgpack-RPC an embedded editor speaks. Check it on the far side \
             with `ssh {target} '{} --version'`, and name a different one \
             with --nvim-bin, which points at the remote editor for a \
             --remote session.",
            remote.remote_nvim_bin, remote.remote_nvim_bin
        ),
        Reachability::ShellRefused { code } => format!(
            "view: the ssh client reached {target}, but `{PROBE_COMMAND}` \
             there exited {code} instead of succeeding, so the remote \
             account's own shell or a forced command is refusing to run what \
             view sends it. view starts the remote editor by handing that \
             shell one command line (`env -- {} --embed ...`). Check what \
             `ssh {target} {PROBE_COMMAND}` does from this host.",
            remote.remote_nvim_bin
        ),
        Reachability::Unknown => general_message(remote),
    }
}

/// The fallback for a failure nothing above classified.
fn general_message(remote: &RemoteSpec) -> String {
    format!(
        "view: failed to start the remote engine on {} (check the ssh client, \
         the destination, and nvim on the far side)",
        remote.target
    )
}

/// Opens a second connection to the same destination, configured exactly as
/// the spawn's was ([`RemoteSpec::connection_args`]), and reports what the
/// client made of it.
///
/// A second connection rather than the first one's exit status because the
/// engine owns that child and reaps it: what reaches a caller is that the
/// RPC channel closed, which is the same observation a locally spawned nvim
/// that died would produce.
///
/// Its output is captured, never inherited: this runs while the alternate
/// screen is still the process's own, and a client writing to the terminal
/// here would paint over it.
fn probe_connection(remote: &RemoteSpec) -> Reachability {
    probe_connection_within(remote, PROBE_LIMIT)
}

/// [`probe_connection`] with the wait spelled out, so the bound itself is
/// testable against a client that never answers rather than only against
/// ones that answer immediately.
///
/// No client option is added on top of what the spec carries -- an injected
/// `ConnectTimeout` would answer for a connection the spawn never made -- so
/// the bound is imposed here, on this process's own waiting, and a client
/// still working when it elapses is killed rather than left running behind
/// an exiting editor. That case reports nothing: a connection that has
/// neither succeeded nor failed is not evidence about the one that did fail.
///
/// The client's stderr is captured into a file and read after it exits,
/// never through a pipe: see [`Capture`] for the client that does not finish
/// against one. Reading afterwards also keeps this free of a second thread,
/// and a file has no buffer for a chatty client to fill and block on.
///
/// A host with no writable scratch directory still gets a classification:
/// the connection runs with its stderr discarded, so the exit status is
/// still read and only the quoted line is lost.
fn probe_connection_within(remote: &RemoteSpec, limit: Duration) -> Reachability {
    probe_capturing(remote, limit, Capture::open())
}

/// [`probe_connection_within`] with the capture handed in rather than
/// opened, so what a host that cannot offer one is told is exercisable from
/// a host that can. `None` is that host: the client's output is discarded
/// and the classification is the exit status alone.
fn probe_capturing(remote: &RemoteSpec, limit: Duration, capture: Option<Capture>) -> Reachability {
    let sink = match capture.as_ref().map(Capture::sink) {
        Some(Ok(sink)) => Stdio::from(sink),
        Some(Err(_)) | None => Stdio::null(),
    };
    let Ok(mut child) = Command::new(&remote.ssh_bin)
        .args(remote.connection_args())
        .arg(PROBE_COMMAND)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(sink)
        .spawn()
    else {
        return Reachability::Unknown;
    };
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => return Reachability::Unknown,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Reachability::Unknown;
        }
        std::thread::sleep(PROBE_POLL_INTERVAL);
    };
    let stderr = capture.as_ref().map(Capture::read).unwrap_or_default();
    match status.code() {
        Some(0) => Reachability::Reached,
        Some(SSH_CLIENT_FAILURE) => Reachability::Rejected {
            detail: last_line(&stderr),
        },
        Some(code) => Reachability::ShellRefused { code },
        None => Reachability::Unknown,
    }
}

/// A private directory holding one diagnostic connection's captured
/// stderr, removed when the capture is dropped -- including the path where
/// the client is killed at the bound and nothing is ever read.
///
/// A file rather than a pipe because a pipe is not a shape every ssh client
/// finishes writing to. OpenSSH for Windows (9.5p2, the client shipped in
/// `%SystemRoot%\System32\OpenSSH`) does not exit while its stderr is a
/// pipe: measured against a host that rejects it, the same connection ends
/// in 83ms with a file-backed stderr and was still running after 20s with a
/// pipe-backed one, with pipes on stdin and stdout making no difference
/// either way. Through a pipe that client outlives every bound this module
/// would be willing to wait, so the refusal it did diagnose is never read
/// and the user gets the generic message instead. A file costs one
/// directory per diagnosis and behaves the same on every platform.
///
/// The directory is created, never opened: a name that already exists is an
/// error here rather than a file this process would write a client's output
/// into.
struct Capture {
    dir: PathBuf,
    path: PathBuf,
}

impl Capture {
    /// A capture directory of this process's own under the system scratch
    /// directory, or `None` when that directory cannot hold one.
    fn open() -> Option<Self> {
        Self::open_in(&std::env::temp_dir())
    }

    /// [`open`](Self::open) under a named parent, so the failure of a parent
    /// that cannot hold a directory is reachable without planting a name in
    /// the environment of a process running many diagnoses at once.
    fn open_in(base: &Path) -> Option<Self> {
        // two probes in one process would otherwise collide on the name,
        // and the counter alone repeats across runs of the same binary
        static NONCE: AtomicU64 = AtomicU64::new(0);
        let dir = base.join(format!(
            "view-probe-{}-{}",
            std::process::id(),
            NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).ok()?;
        let path = dir.join("stderr");
        Some(Self { dir, path })
    }

    /// The handle the client writes its stderr into.
    fn sink(&self) -> std::io::Result<File> {
        File::create(&self.path)
    }

    /// Everything the client wrote, or nothing when it wrote nothing and
    /// when the capture cannot be read back.
    fn read(&self) -> Vec<u8> {
        std::fs::read(&self.path).unwrap_or_default()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// The client's own last non-empty line, trimmed and bounded, or `None`
/// when it wrote nothing usable.
///
/// One attributed line, not the client's whole stderr: a raw dump buries
/// the guidance it is attached to under a verbose banner or a `-v` trace,
/// and the line the client ends on is the one carrying its diagnosis.
///
/// What the returned line guarantees, exactly: no C0 or C1 control
/// character and no bidirectional formatting character
/// ([`reorders_display`]) survives it, so nothing a remote host wrote can
/// move the cursor of, or reverse the reading order of, the message it is
/// quoted into -- this text prints to a terminal that has just been handed
/// back to a shell, and the host that wrote it is exactly the host that
/// just refused the connection.
///
/// What it does not guarantee: byte fidelity. The line is decoded lossily,
/// so a banner that is not UTF-8 arrives with replacement characters where
/// its undecodable bytes were. That is the right trade here and the
/// opposite of the rule for a value crossing to the far side (see
/// `view_engine`'s `token_bytes`): this text is prose for a human to read,
/// never a name anything is resolved by, and a message that cannot be
/// rendered at all quotes nothing.
fn last_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let line: String = text
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())?
        .chars()
        .filter(|c| !c.is_control() && !reorders_display(*c))
        .collect();
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    Some(match line.char_indices().nth(DETAIL_LIMIT) {
        Some((cut, _)) => format!("{}...", &line[..cut]),
        None => line.to_string(),
    })
}

/// Whether `c` is one of Unicode's bidirectional formatting characters,
/// which reorder the display of the text around them without being
/// control characters (`char::is_control` covers C0 and C1 only, and none
/// of these are in either range).
///
/// Enumerated rather than derived from a character-property table: the set
/// is fixed and small, and a dependency on a Unicode-tables crate would be
/// a whole new supply chain for one filter in one error message. The marks
/// (`U+061C`, `U+200E`, `U+200F`), the embedding and override controls
/// (`U+202A`-`U+202E`) and the isolates (`U+2066`-`U+2069`) are the
/// complete set as of Unicode 16.
fn reorders_display(c: char) -> bool {
    matches!(c, '\u{61c}' | '\u{200e}' | '\u{200f}')
        || ('\u{202a}'..='\u{202e}').contains(&c)
        || ('\u{2066}'..='\u{2069}').contains(&c)
}

/// Whether `program` resolves, searching `path_var` when it is a bare name
/// and checking it directly when it carries any directory component at all.
///
/// `path_ext` is Windows' own list of extensions an unqualified program
/// name may be spelled without; it is absent on every other platform, and
/// passed in rather than read here so the Windows arm is exercisable from
/// any host.
fn resolve_client(program: &Path, path_var: Option<&OsStr>, path_ext: Option<&OsStr>) -> Presence {
    if program.components().count() > 1 {
        return if is_file(program) {
            Presence::Found
        } else {
            // a named path that does not exist is still not claimed absent:
            // the spawn's own error names the path and the reason, which is
            // more than this check knows
            Presence::Undetermined
        };
    }
    // an unset PATH is not an empty one: a Unix `exec*p` falls back to a
    // built-in default path when the variable is missing entirely, so a
    // client can still resolve through a search this has no view of
    let Some(path_var) = path_var else {
        return Presence::Undetermined;
    };
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in spellings(program, path_ext) {
            if is_file(&dir.join(&name)) {
                return Presence::Found;
            }
        }
    }
    Presence::Absent
}

/// Every filename `program` may be spelled as in a search directory: the
/// name itself, plus one per `PATHEXT` entry where that list exists.
fn spellings(program: &Path, path_ext: Option<&OsStr>) -> Vec<OsString> {
    let bare = program.as_os_str().to_os_string();
    let Some(path_ext) = path_ext else {
        return vec![bare];
    };
    let mut names = vec![bare];
    for ext in path_ext.to_string_lossy().split(';') {
        let ext = ext.trim();
        if ext.is_empty() {
            continue;
        }
        let mut name = program.as_os_str().to_os_string();
        name.push(ext);
        names.push(name);
    }
    names
}

/// Whether a path names an existing file, following symlinks. Anything that
/// cannot be stat'd is not one.
fn is_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::io::Write as _;

    /// A scratch directory of this test's own, removed when the guard
    /// drops. Directories only: an executable written by a test and then
    /// run is an `ETXTBSY` race against a sibling test's fork, which is why
    /// every program these tests run is a committed fixture.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "view-remote-guard-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&path).expect("a scratch directory for this test");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(name: &str) -> PathBuf {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/test-fixtures")
            .join(name)
            .canonicalize()
            .expect("the test fixtures are committed alongside the crate");
        assert!(path.is_file(), "{path:?} is not a file");
        path
    }

    fn spec() -> RemoteSpec {
        RemoteSpec::new("view-test-host")
    }

    /// A stand-in client that refuses the way OpenSSH refuses under
    /// `BatchMode`, in the form this platform can run: a shell script where
    /// one is executable, a batch file on Windows. Two fixtures rather than
    /// one because the diagnosis is a real spawned process on both -- a
    /// Windows build that classified an exit status no Windows process here
    /// ever produced would prove nothing about Windows.
    fn refusing_client() -> PathBuf {
        if cfg!(windows) {
            fixture("fake-ssh-reject.cmd")
        } else {
            fixture("fake-ssh-reject")
        }
    }

    /// The same, for a client that refuses only after a banner larger than
    /// a pipe will hold.
    fn chatty_client() -> PathBuf {
        if cfg!(windows) {
            fixture("fake-ssh-chatty.cmd")
        } else {
            fixture("fake-ssh-chatty")
        }
    }

    /// The same, for a client that connects and runs what it was given.
    fn connecting_client() -> PathBuf {
        if cfg!(windows) {
            fixture("fake-ssh-accept.cmd")
        } else {
            fixture("fake-ssh")
        }
    }

    /// The Windows branch names the Windows remediation. A message telling a
    /// Windows user to run a package manager they do not have is worse than
    /// no guidance at all: it sends them somewhere that cannot work.
    #[test]
    fn the_windows_absent_client_message_names_the_optional_feature() {
        let message =
            absent_client_message(&spec(), ClientPlatform::Windows, ClientLookup::SearchedPath);
        assert!(
            message.contains("Add-WindowsCapability -Online -Name OpenSSH.Client~~~~0.0.1.0"),
            "the Windows remediation must name the capability command: {message}"
        );
        assert!(
            message.contains("Optional features"),
            "the Windows remediation must also name the Settings route, for \
             a user without an elevated shell: {message}"
        );
        for posix_only in ["apt install", "dnf install", "brew install", "pacman"] {
            assert!(
                !message.contains(posix_only),
                "the Windows message carries POSIX packaging guidance \
                 ({posix_only}): {message}"
            );
        }
    }

    /// And the POSIX branch does not send a Linux or macOS user to a
    /// Windows optional feature.
    #[test]
    fn the_posix_absent_client_message_names_a_package_not_a_windows_feature() {
        let message =
            absent_client_message(&spec(), ClientPlatform::Posix, ClientLookup::SearchedPath);
        assert!(
            message.contains("openssh-client"),
            "the POSIX remediation must name the package: {message}"
        );
        assert!(
            !message.contains("WindowsCapability"),
            "the POSIX message carries the Windows feature-on-demand \
             remediation: {message}"
        );
    }

    /// Both branches say what is missing and which session it stopped,
    /// ahead of whatever remediation follows.
    #[test]
    fn every_absent_client_message_names_the_destination_and_the_client() {
        for platform in [ClientPlatform::Posix, ClientPlatform::Windows] {
            let message = absent_client_message(&spec(), platform, ClientLookup::SearchedPath);
            assert!(
                message.contains("view-test-host") && message.contains("`ssh`"),
                "{platform:?}: {message}"
            );
        }
    }

    #[test]
    fn a_client_in_no_path_entry_is_absent() {
        let scratch = Scratch::new("empty-path");
        let path = std::env::join_paths([scratch.0.clone()]).unwrap();
        assert_eq!(
            resolve_client(Path::new("ssh"), Some(&path), None),
            Presence::Absent
        );
    }

    #[test]
    fn a_client_in_a_later_path_entry_is_found() {
        let scratch = Scratch::new("later-entry");
        let empty = scratch.0.join("empty");
        let holding = scratch.0.join("holding");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&holding).unwrap();
        std::fs::write(holding.join("ssh"), b"").unwrap();
        let path = std::env::join_paths([empty, holding]).unwrap();
        assert_eq!(
            resolve_client(Path::new("ssh"), Some(&path), None),
            Presence::Found
        );
    }

    /// The extension list is Windows' own, and the search that ignores it
    /// would report every Windows host as having no client at all.
    #[test]
    fn a_client_spelled_with_a_pathext_extension_is_found() {
        let scratch = Scratch::new("pathext");
        std::fs::write(scratch.0.join("ssh.EXE"), b"").unwrap();
        let path = std::env::join_paths([scratch.0.clone()]).unwrap();
        assert_eq!(
            resolve_client(
                Path::new("ssh"),
                Some(&path),
                Some(OsStr::new(".COM;.EXE;.BAT"))
            ),
            Presence::Found,
            "an ssh.EXE in a PATH entry is the client a Windows host has"
        );
        assert_eq!(
            resolve_client(Path::new("ssh"), Some(&path), None),
            Presence::Absent,
            "without the extension list there is nothing named plain `ssh` \
             to find, which is what makes the arm above load-bearing"
        );
    }

    /// Two cases this check declines to answer, each because a wrong
    /// refusal costs a working session: a client named by path (the spawn's
    /// own error names it better) and a PATH that is not set at all (a Unix
    /// exec still falls back to a built-in default path).
    #[test]
    fn an_unsearchable_case_is_undetermined_rather_than_absent() {
        let scratch = Scratch::new("undetermined");
        assert_eq!(
            resolve_client(&scratch.0.join("nowhere/ssh"), None, None),
            Presence::Undetermined,
            "a client named by path is left to the spawn to report"
        );
        assert_eq!(
            resolve_client(Path::new("ssh"), None, None),
            Presence::Undetermined,
            "an unset PATH is not an empty PATH"
        );
    }

    /// A named path that does exist is still found, so the refusal never
    /// fires on a spec that pins its own client.
    #[test]
    fn a_client_named_by_an_existing_path_is_found() {
        assert_eq!(
            resolve_client(&fixture("fake-ssh"), None, None),
            Presence::Found
        );
    }

    /// The absent-client message is the same message whether the check
    /// caught it before the spawn or the spawn itself reported it: a client
    /// that disappeared between the two is the same problem with the same
    /// fix.
    ///
    /// Asserted on content rather than against a second call of the
    /// function under test, which would agree with it however wrong both
    /// were. A test binary always runs with a `PATH`, so the searched-path
    /// wording is the one this must render.
    #[test]
    fn a_spawn_that_could_not_find_the_client_reports_the_absent_client() {
        let missing = EngineError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory",
        ));
        let message = spawn_failure_context(&spec(), &missing);
        assert!(
            message.contains("needs the system ssh client")
                && message.contains("no `ssh` was found on this host's PATH"),
            "a NotFound spawn error is the client missing, not the far side \
             refusing: {message}"
        );
    }

    /// A client named by path was never looked for on `PATH`, so the
    /// message about not finding it must not send the user to their `PATH`
    /// or their package manager: both had no part in it.
    #[test]
    fn a_client_named_by_path_is_not_reported_as_missing_from_path() {
        let pinned = RemoteSpec::new("view-test-host").with_ssh_bin("/opt/pinned/ssh");
        let message =
            absent_client_message(&pinned, ClientPlatform::Posix, ClientLookup::NamedPath);
        assert!(
            message.contains("nothing could be started at `/opt/pinned/ssh`"),
            "the message must name the path it actually checked: {message}"
        );
        assert!(
            !message.contains("was found on this host's PATH") && !message.contains("apt install"),
            "a path-named client was never searched for on PATH and needs no \
             package installed: {message}"
        );
    }

    /// And a process with no `PATH` at all searched nothing, so it claims
    /// nothing about one.
    #[test]
    fn a_client_with_no_path_to_search_says_so_instead_of_blaming_one() {
        let message =
            absent_client_message(&spec(), ClientPlatform::Posix, ClientLookup::NoPathToSearch);
        assert!(
            message.contains("no PATH set to look for one on"),
            "the message must say the search never happened: {message}"
        );
        assert!(
            !message.contains("was found on this host's PATH"),
            "a claim about the contents of a PATH this process does not \
             have: {message}"
        );
    }

    /// Which lookup a spec falls under, since the wording above rests
    /// entirely on it.
    #[test]
    fn a_lookup_is_keyed_on_how_the_client_is_named_and_whether_a_path_exists() {
        let path = OsString::from("/usr/bin");
        assert_eq!(
            client_lookup(Path::new("ssh"), Some(&path)),
            ClientLookup::SearchedPath
        );
        assert_eq!(
            client_lookup(Path::new("ssh"), None),
            ClientLookup::NoPathToSearch
        );
        assert_eq!(
            client_lookup(Path::new("/opt/pinned/ssh"), Some(&path)),
            ClientLookup::NamedPath,
            "a path is checked where it points, whatever PATH holds"
        );
    }

    /// Which branch a host renders is the half a parameterised message
    /// cannot prove on its own: both texts are correct, and only one of
    /// them belongs on this machine. The `cfg!` here is a second,
    /// independent read of the same condition, so an inverted
    /// `ClientPlatform::current` fails this on both CI legs rather than
    /// shipping Windows users a package manager they do not have.
    #[test]
    fn a_host_renders_its_own_platforms_remediation() {
        let rendered = absent_client_message(
            &spec(),
            ClientPlatform::current(),
            ClientLookup::SearchedPath,
        );
        if cfg!(windows) {
            assert!(
                rendered.contains("Add-WindowsCapability") && !rendered.contains("apt install"),
                "a Windows host must render the Windows remediation: {rendered}"
            );
        } else {
            assert!(
                rendered.contains("openssh-client") && !rendered.contains("WindowsCapability"),
                "a POSIX host must render the package remediation: {rendered}"
            );
        }
    }

    /// One family, one prefix. All six render at the same call site and in
    /// the same position, so a user (or a wrapper script, or a doc example)
    /// separating view's own refusals from a propagated engine error by
    /// that prefix must not get a different answer depending on which arm
    /// of the same failure family fired.
    #[test]
    fn every_message_in_the_family_opens_the_way_views_own_refusals_do() {
        let spec = spec();
        let mut family = vec![
            absent_client_message(&spec, ClientPlatform::Posix, ClientLookup::SearchedPath),
            absent_client_message(&spec, ClientPlatform::Windows, ClientLookup::NamedPath),
            general_message(&spec),
            reachability_message(&spec, Reachability::Rejected { detail: None }),
            reachability_message(&spec, Reachability::Reached),
            reachability_message(&spec, Reachability::ShellRefused { code: 127 }),
            attach_failure_context(&spec, &AttachFailure::Attach(EngineError::Closed)),
        ];
        family.push(spawn_failure_context(
            &spec,
            &EngineError::Timeout {
                method: String::from("nvim_get_api_info"),
                timeout: Duration::from_secs(5),
            },
        ));
        for message in family {
            assert!(
                message.starts_with("view: "),
                "every message in this family is view's own prose and opens \
                 the way the rest of view's refusals do: {message}"
            );
        }
    }

    /// A handshake that timed out is a client still working, not a client
    /// that failed: opening a second connection would put this process back
    /// into the wait it just gave up on, so nothing is probed and the
    /// message says which two things the wait is usually spent on.
    #[test]
    fn a_handshake_timeout_diagnoses_the_wait_without_opening_a_second_one() {
        let message = spawn_failure_context(
            &spec(),
            &EngineError::Timeout {
                method: String::from("nvim_get_api_info"),
                timeout: std::time::Duration::from_secs(5),
            },
        );
        assert!(
            message.contains("5s") && message.contains("firewall"),
            "the timeout message must name what it waited and what silently \
             swallows a connection: {message}"
        );
        assert!(
            !message.contains("exited 255"),
            "a client that never exited must not be reported as one that \
             refused: {message}"
        );
    }

    /// An error that is neither the client missing nor a channel that
    /// closed says only what it knows, and opens no connection to guess
    /// with.
    #[test]
    fn an_unclassified_spawn_error_falls_back_to_the_general_message() {
        let denied = EngineError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Permission denied",
        ));
        assert_eq!(
            spawn_failure_context(&spec(), &denied),
            general_message(&spec())
        );
    }

    /// The attach arm ran over a connection that already carried a
    /// handshake, so it must not be reported as a connection problem.
    #[test]
    fn an_attach_failure_is_not_reported_as_a_refused_connection() {
        let message = attach_failure_context(
            &spec(),
            &AttachFailure::Attach(EngineError::Timeout {
                method: String::from("nvim_ui_attach"),
                timeout: std::time::Duration::from_secs(5),
            }),
        );
        assert!(
            message.contains("the connection itself is up"),
            "an attach failure must not send a user to look at their keys: \
             {message}"
        );
        assert!(
            !message.contains("known_hosts"),
            "the authentication guidance leaked into the attach arm: \
             {message}"
        );
    }

    #[test]
    fn a_rejection_message_names_the_causes_batch_mode_turns_into_a_refusal() {
        let message = reachability_message(
            &spec(),
            Reachability::Rejected {
                detail: Some(String::from("Permission denied (publickey).")),
            },
        );
        for cause in ["agent", "known_hosts", "resolve", "port"] {
            assert!(
                message.contains(cause),
                "a rejection names its likely causes; {cause} missing from: \
                 {message}"
            );
        }
        assert!(
            message.contains("\"Permission denied (publickey).\""),
            "the client's own diagnosis is quoted back, attributed: {message}"
        );
        assert!(
            message.contains("BatchMode=yes"),
            "the message must say why the client refused instead of \
             prompting: {message}"
        );
    }

    /// A connection that works points at the far side's editor, and must
    /// not send a user to look at keys that are demonstrably fine.
    #[test]
    fn a_reached_host_points_at_the_remote_editor_not_the_connection() {
        let message = reachability_message(
            &RemoteSpec::new("view-test-host").with_remote_nvim_bin("/opt/nvim/bin/nvim"),
            Reachability::Reached,
        );
        assert!(
            message.contains("/opt/nvim/bin/nvim --version"),
            "the message must name the remote editor it could not start: \
             {message}"
        );
        assert!(
            !message.contains("known_hosts"),
            "authentication guidance for a host that authenticated: {message}"
        );
    }

    #[test]
    fn a_detail_line_is_the_last_one_bounded_and_stripped_of_control_bytes() {
        assert_eq!(
            last_line(b"banner line\nPermission denied (publickey).\n"),
            Some(String::from("Permission denied (publickey)."))
        );
        assert_eq!(last_line(b"   \n\n"), None);
        assert_eq!(
            last_line("a\u{1b}[2Jb".as_bytes()),
            Some(String::from("a[2Jb")),
            "an escape sequence from a remote banner must not reach a \
             terminal that is being handed back to a shell"
        );
        let long = last_line(&vec![b'x'; DETAIL_LIMIT + 50]).unwrap();
        assert_eq!(
            long.chars().count(),
            DETAIL_LIMIT + 3,
            "a long line is cut to the limit plus its ellipsis: {long}"
        );
    }

    /// A refusing host's banner is quoted into prose the user reads after
    /// the terminal has been handed back. A bidirectional override there
    /// reverses the rendering of the text it is quoted into -- the
    /// remediation the user is being given -- without ever being a control
    /// character, which is the filter it slips past.
    #[test]
    fn a_bidi_override_from_a_remote_banner_cannot_reorder_the_message() {
        let hostile = "denied \u{202e}drowssap si yek\u{202c} here";
        let quoted = last_line(hostile.as_bytes()).unwrap();
        assert!(
            !quoted.contains('\u{202e}') && !quoted.contains('\u{202c}'),
            "a bidi control survived into the quoted line: {quoted:?}"
        );
        assert_eq!(
            quoted, "denied drowssap si yek here",
            "only the reordering characters are dropped; the client's own \
             words stay exactly as it wrote them"
        );
        for reorderer in [
            '\u{61c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{2066}', '\u{2069}',
        ] {
            assert!(
                reorders_display(reorderer),
                "{reorderer:?} reorders display and must be filtered"
            );
        }
        assert!(
            !reorders_display('e') && !reorders_display('\u{2070}'),
            "the filter must not reach past the bidi controls"
        );
    }

    /// The diagnosis is a real connection through a real client, not a
    /// classification of a string: a stub that refuses the way OpenSSH
    /// refuses is reported as a rejection, with its own last line carried
    /// through.
    #[test]
    fn a_client_that_refuses_the_connection_is_diagnosed_as_a_rejection() {
        let spec = RemoteSpec::new("view-test-host").with_ssh_bin(refusing_client());
        assert_eq!(
            probe_connection(&spec),
            Reachability::Rejected {
                detail: Some(String::from(
                    "view-test-host: Permission denied (publickey)."
                ))
            }
        );
    }

    /// A client's answer is read from a file, so a client that writes more
    /// than a pipe would hold before refusing is still diagnosed. Through a
    /// pipe nobody drains this client blocks on its own write and never
    /// exits, and the diagnosis read after exit never arrives.
    ///
    /// Given its own generous bound rather than the shipped one: the
    /// fixture's banner takes a batch interpreter a while to write, and what
    /// is under test is whether the whole of it is captured, never how long
    /// this module is willing to wait.
    #[test]
    fn a_chatty_client_is_still_diagnosed_after_writing_more_than_a_pipe_will_hold() {
        let spec = RemoteSpec::new("view-test-host").with_ssh_bin(chatty_client());
        assert_eq!(
            probe_connection_within(&spec, Duration::from_secs(30)),
            Reachability::Rejected {
                detail: Some(String::from(
                    "view-test-host: Permission denied (publickey)."
                ))
            }
        );
    }

    /// A host whose scratch directory cannot hold a capture still gets a
    /// diagnosis: the connection runs with its output discarded and is
    /// classified on the exit status alone, which is the whole of what the
    /// fallback promises. Reporting `Unknown` here would throw away a
    /// refusal the client did make, over a file this host could not offer.
    #[test]
    fn a_probe_that_cannot_open_its_capture_still_names_the_rejection() {
        let spec = RemoteSpec::new("view-test-host").with_ssh_bin(refusing_client());
        assert_eq!(
            probe_capturing(&spec, PROBE_LIMIT, None),
            Reachability::Rejected { detail: None },
            "the rejection is the client's exit status, which no scratch \
             directory is needed to read"
        );
    }

    /// The state that fallback stands for is a real one: a parent that
    /// cannot hold a directory yields no capture rather than a capture
    /// nothing can be written into.
    ///
    /// The parent here is a regular file, which refuses `create_dir` for
    /// every user including the one this suite often runs as; a refusal
    /// spelled with permissions would be bypassed by root.
    #[test]
    fn a_capture_under_a_parent_that_cannot_hold_one_is_never_opened() {
        let scratch = Scratch::new("unusable-capture-parent");
        let blocker = scratch.0.join("blocker-file");
        std::fs::write(&blocker, b"").unwrap();
        assert!(Capture::open_in(&blocker).is_none());
        assert!(Capture::open_in(&blocker.join("sub")).is_none());
    }

    /// A capture is one diagnosis's own, and outlives none of them: the
    /// directory it wrote into is gone once the capture is dropped, on the
    /// path where the client answered and on the path where it never did.
    #[test]
    fn a_capture_is_removed_with_the_diagnosis_that_owns_it() {
        let (dir, path) = {
            let capture = Capture::open().expect("a capture directory");
            let mut sink = capture
                .sink()
                .expect("a handle to write the client's answer into");
            sink.write_all(b"view-test-host: Permission denied (publickey).\n")
                .expect("the capture accepts what a client writes");
            drop(sink);
            assert_eq!(
                last_line(&capture.read()).as_deref(),
                Some("view-test-host: Permission denied (publickey).")
            );
            (capture.dir.clone(), capture.path.clone())
        };
        assert!(!path.exists(), "{path:?} outlived the capture that owns it");
        assert!(!dir.exists(), "{dir:?} outlived the capture that owns it");
    }

    /// Two diagnoses in one process do not share a capture: a second one
    /// would otherwise read, and then remove, the first one's answer.
    #[test]
    fn a_capture_is_never_shared_between_two_diagnoses() {
        let first = Capture::open().expect("a capture directory");
        let second = Capture::open().expect("a second capture directory");
        assert_ne!(first.path, second.path);
    }

    /// And a client that connects is not: the stub that stands in for a
    /// working connection runs the probe command on the far side, which
    /// succeeds, and the diagnosis moves to the editor.
    #[test]
    fn a_client_that_connects_is_diagnosed_as_reaching_the_host() {
        let spec = RemoteSpec::new("view-test-host").with_ssh_bin(connecting_client());
        assert_eq!(probe_connection(&spec), Reachability::Reached);
    }

    /// A client that never answers is abandoned, not waited on: the caller
    /// already has a failed session to report, and the user is looking at a
    /// terminal that says nothing until this returns.
    #[cfg(unix)]
    #[test]
    fn a_client_that_never_answers_is_abandoned_at_the_bound() {
        let spec = RemoteSpec::new("view-test-host").with_ssh_bin(fixture("fake-ssh-hang"));
        let started = std::time::Instant::now();
        let found = probe_connection_within(&spec, Duration::from_millis(200));
        let waited = started.elapsed();
        assert_eq!(
            found,
            Reachability::Unknown,
            "a connection that neither succeeded nor failed is not evidence \
             about the one that did"
        );
        assert!(
            waited < Duration::from_secs(5),
            "the bound was not enforced: waited {waited:?} on a client that \
             sits for 30 seconds"
        );
    }

    /// End to end over the failure a rejected connection really produces:
    /// the engine reports a closed channel, and what the user is told names
    /// the authentication, not a missing editor.
    #[test]
    fn a_closed_channel_over_a_refusing_client_is_reported_as_an_auth_failure() {
        let spec = RemoteSpec::new("view-test-host").with_ssh_bin(refusing_client());
        let message = spawn_failure_context(&spec, &EngineError::Closed);
        assert!(
            message.contains("could not open a session on view-test-host"),
            "{message}"
        );
        assert!(
            message.contains("Permission denied (publickey)."),
            "the client's own words must survive into the guidance: {message}"
        );
    }

    /// The same closed channel over a client that connects is the opposite
    /// diagnosis, which is the whole point of running the connection rather
    /// than guessing from the error.
    #[test]
    fn a_closed_channel_over_a_working_client_is_reported_against_the_editor() {
        let spec = RemoteSpec::new("view-test-host").with_ssh_bin(connecting_client());
        let message = spawn_failure_context(&spec, &EngineError::Closed);
        assert!(
            message.contains("reached view-test-host and authenticated"),
            "{message}"
        );
    }
}
