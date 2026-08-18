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

pub use acp::session::AiSession;

/// Where the agent comes from and where it runs.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct AiConfig {
    /// The agent executable.
    pub command: String,
    /// Arguments passed to it, in order.
    pub args: Vec<String>,
    /// The working directory the agent is started in, and the one the
    /// session is created against.
    pub cwd: std::path::PathBuf,
}

impl AiConfig {
    /// An agent started from `command` with no arguments, in `cwd`.
    #[must_use]
    pub fn new(command: impl Into<String>, cwd: impl Into<std::path::PathBuf>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: cwd.into(),
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
