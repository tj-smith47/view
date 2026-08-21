//! Agent Client Protocol integration: sessions, panel state, diff review.
//!
//! This crate owns the only tokio runtime in the process and the only
//! JSON-RPC-over-stdio transport. Everything it reports crosses into the
//! rest of the editor as
//! [`AiEvent`](view_core::native::ai_event::AiEvent), and everything asked
//! of it arrives as [`AiCommand`](view_core::native::ai_event::AiCommand):
//! no agent-protocol type, and no JSON, is visible from either side of that
//! boundary.

mod acp;
mod config;
mod context;
mod provision;
mod trust;
mod watch;

pub use acp::session::{AgentAdapter, AiSession, ClaudeCodeAdapter};
pub use config::{AgentSpec, AiConfig, AiConfigError};
pub use context::assemble;
pub use provision::{adapter_is_ready, ensure_adapter, pinned_version, AdapterPin, ProvisionError};
pub use trust::{path_is_contained, TrustError, TrustStore};
pub use watch::{spawn as spawn_watch, WatchError, WatchHandle, FILE_GONE_GRACE};

/// Where the agent comes from and where it runs.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct AgentLaunch {
    /// The agent executable.
    pub command: String,
    /// Arguments passed to it, in order.
    pub args: Vec<String>,
    /// The working directory the agent is started in, and the one the
    /// session is created against.
    pub cwd: std::path::PathBuf,
    /// Whether the session must call `authenticate` when `session/new`
    /// reports the wire's `auth_required` error, rather than treating that
    /// error as terminal. `false` by default: an agent that offers
    /// authentication methods without enforcing them must not be made to
    /// authenticate against its own wishes.
    pub requires_auth: bool,
}

impl AgentLaunch {
    /// An agent started from `command` with no arguments, in `cwd`.
    #[must_use]
    pub fn new(command: impl Into<String>, cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            requires_auth: false,
        }
    }

    /// The same configuration with `args` passed to the agent.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// The same configuration, requiring `authenticate` before `session/new`
    /// is treated as having succeeded or failed for good.
    #[must_use]
    pub fn requiring_auth(mut self) -> Self {
        self.requires_auth = true;
        self
    }

    /// An agent config derived from an [`AgentAdapter`]: its command,
    /// arguments, and authentication requirement carried straight through,
    /// so a caller with an adapter never re-derives what the adapter
    /// already knows.
    #[must_use]
    pub fn from_adapter(adapter: &dyn AgentAdapter, cwd: impl Into<std::path::PathBuf>) -> Self {
        let (command, args) = adapter.command();
        Self {
            command: command.to_string(),
            args: args.to_vec(),
            cwd: cwd.into(),
            requires_auth: adapter.requires_auth(),
        }
    }
}

/// Everything that can stop a session before it starts.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// The agent's own runtime could not be built.
    #[error("could not build the agent runtime: {0}")]
    Runtime(std::io::Error),
    /// The agent executable could not be started.
    #[error("could not start the agent `{command}`: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    /// The agent started without the pipes the stdio transport needs.
    #[error("the agent started without the stdin and stdout the transport needs")]
    ChildPipeMissing,
}
