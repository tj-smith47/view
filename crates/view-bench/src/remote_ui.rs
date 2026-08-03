//! The out-of-process UI control arm: nvim's own TUI driving a headless
//! nvim over the same msgpack-rpc UI protocol view speaks.
//!
//! The echo row measures view against bare nvim and reports view ~1.2x
//! slower at steady typing. That number answers "what does a user pay to
//! run view", but it cannot say *why*, because it varies two things at
//! once: the UI implementation (Rust, view's) and the UI's location (a
//! separate process, reached over a socket). Bare nvim changes both.
//!
//! Splitting one nvim into a headless server plus a `--remote-ui` client
//! varies only the location. Whatever that arm costs against bare nvim is
//! the price of being an out-of-process UI at all, charged to nvim's own C
//! TUI; the remainder is view's to answer for.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::session::SpawnSpec;
use crate::BenchError;

/// How long [`RemoteUiServer::start`] waits for the headless server to
/// bind its socket before giving up. Generous because the server runs the
/// fixture's full config, plugin manager included, before it listens.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(120);

/// How often that wait rechecks. Short enough not to add measurable
/// latency to a run's setup, long enough not to spin a core while a
/// plugin-heavy fixture starts.
const LISTEN_RECHECK: Duration = Duration::from_millis(20);

/// A headless nvim holding the buffer, with the socket a `--remote-ui`
/// client attaches to. Killed on drop, process group and all: a server
/// outliving its run would hold the fixture's plugin cache open and leak
/// into the next spawn, and anything the server itself spawned would
/// otherwise outlive the kill.
#[derive(Debug)]
pub struct RemoteUiServer {
    child: Child,
    socket: PathBuf,
    // a reaped pid can be recycled, so the drop guard's group kill must be
    // skipped once the exit status has been collected
    reaped: bool,
}

/// The headless server's argument list for a bare-nvim spec.
///
/// The fixture's own arguments follow the listen flags rather than
/// preceding them, so the file the row opens is still positional and the
/// server holds exactly the buffer the other two arms hold.
#[must_use]
pub fn server_args(nvim: &SpawnSpec, socket: &Path) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("--headless"),
        OsString::from("--listen"),
        socket.as_os_str().to_os_string(),
    ];
    args.extend(nvim.args.iter().cloned());
    args
}

/// The client's argument list: a UI and nothing else.
///
/// No file argument and no config: a `--remote-ui` client sources no
/// user configuration, and every buffer it shows belongs to the server.
/// Passing the fixture's file here would open a second, local copy that
/// the server knows nothing about.
#[must_use]
pub fn client_args(socket: &Path) -> Vec<OsString> {
    vec![
        OsString::from("--server"),
        socket.as_os_str().to_os_string(),
        OsString::from("--remote-ui"),
    ]
}

impl RemoteUiServer {
    /// Starts the headless half of the control arm, returning once its
    /// socket is accepting connections.
    ///
    /// `nvim` is the bare-nvim spec the row already builds, so the server
    /// runs the identical fixture config, environment and working
    /// directory as the arm it is being compared against.
    ///
    /// # Errors
    ///
    /// Returns [`BenchError::Desync`] if the server cannot be spawned, or
    /// if it never binds `socket` within [`LISTEN_TIMEOUT`].
    pub fn start(nvim: &SpawnSpec, socket: PathBuf) -> Result<Self, BenchError> {
        let mut command = Command::new(&nvim.program);
        command.args(server_args(nvim, &socket));
        for (key, value) in &nvim.env {
            command.env(key, value);
        }
        // the same rule the pty funnel applies to the two arms this one is
        // compared against, applied after the caller's own variables
        // exactly as `spawn_configured` does
        view_oracle::make_hermetic(&mut command)?;
        if let Some(cwd) = &nvim.cwd {
            command.current_dir(cwd);
        }
        // a headless server writes startup diagnostics to the inherited
        // stderr, which is the bench's own report stream
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        // its own process group, so the drop guard can group-kill whatever
        // the server spawned without signalling the bench's own group; a
        // pty-hosted child gets the same isolation from setsid, but this
        // spawn has no pty to do it
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn().map_err(|err| BenchError::Desync {
            context: format!(
                "spawning the headless control server {}: {err}",
                nvim.program.display()
            ),
        })?;
        let mut server = Self {
            child,
            socket,
            reaped: false,
        };
        server.await_listening()?;
        Ok(server)
    }

    /// The spawn spec for the pty-hosted client, sharing the server's
    /// environment and working directory so the two halves agree on
    /// every path the fixture resolves.
    #[must_use]
    pub fn client_spec(&self, nvim: &SpawnSpec) -> SpawnSpec {
        SpawnSpec {
            program: nvim.program.clone(),
            args: client_args(&self.socket),
            env: nvim.env.clone(),
            cwd: nvim.cwd.clone(),
        }
    }

    /// Blocks until the socket exists, the server has exited, or the
    /// timeout expires. Presence of the socket file is the readiness
    /// signal nvim itself publishes; connecting to probe would consume a
    /// UI attach slot the client is about to want.
    fn await_listening(&mut self) -> Result<(), BenchError> {
        let deadline = Instant::now() + LISTEN_TIMEOUT;
        loop {
            if self.socket.exists() {
                return Ok(());
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                self.reaped = true;
                return Err(BenchError::Desync {
                    context: format!(
                        "the headless control server exited with {status} before binding {}",
                        self.socket.display()
                    ),
                });
            }
            if Instant::now() >= deadline {
                return Err(BenchError::Desync {
                    context: format!(
                        "the headless control server never bound {} within {LISTEN_TIMEOUT:?}",
                        self.socket.display()
                    ),
                });
            }
            std::thread::sleep(LISTEN_RECHECK);
        }
    }
}

impl Drop for RemoteUiServer {
    fn drop(&mut self) {
        if !self.reaped {
            view_oracle::kill_process_group(self.child.id());
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn spec() -> SpawnSpec {
        SpawnSpec {
            program: PathBuf::from("/usr/bin/nvim"),
            args: vec![OsString::from("/scratch/scratch.txt")],
            env: vec![(OsString::from("TERM"), OsString::from("xterm-256color"))],
            cwd: Some(PathBuf::from("/scratch")),
        }
    }

    #[test]
    fn the_server_keeps_the_rows_file_argument_after_the_listen_flags() {
        let args = server_args(&spec(), Path::new("/scratch/ui.sock"));
        assert_eq!(
            args,
            vec![
                OsString::from("--headless"),
                OsString::from("--listen"),
                OsString::from("/scratch/ui.sock"),
                OsString::from("/scratch/scratch.txt"),
            ],
            "the control arm must hold the same buffer as the arms it is compared against"
        );
    }

    #[test]
    fn the_client_opens_no_file_of_its_own() {
        let args = client_args(Path::new("/scratch/ui.sock"));
        assert!(
            !args.iter().any(|arg| arg == "/scratch/scratch.txt"),
            "a file argument here would open a second buffer the server never sees, so the arm \
             would measure a local edit rather than a round trip"
        );
        assert_eq!(args.last().unwrap(), "--remote-ui");
    }
}
