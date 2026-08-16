//! The echo row's out-of-process control: nvim's own TUI, attached to a
//! headless nvim over the UI protocol, measured against bare nvim by the
//! identical protocol the echo row uses.
//!
//! The echo row reports view slower than bare nvim at steady typing and
//! cannot say why, because it varies the UI implementation and the UI's
//! location together. This row varies only the location, so its ratio is
//! the price of being an out-of-process UI charged to nvim's own C TUI.
//! Read against the echo row's ratio from the same class, it splits that
//! row's overhead into the part any out-of-process UI pays and the part
//! that is view's.
//!
//! Both ratios are measured against a bare-nvim arm sampled in the same
//! interleaved run as their own, so each cancels host drift internally
//! before the two are compared.

use std::path::PathBuf;
use std::time::Duration;

use crate::remote_ui::RemoteUiServer;
use crate::scenarios::echo::{self, EchoOutcome};
use crate::scenarios::Protocol;
use crate::session::{NvimSpec, ViewSpec};
use crate::BenchError;

/// The measured side's name in this row's report lines.
pub const MEASURED_SIDE: &str = "remote-ui";

/// Runs the control row: a headless server started from `control_spec`, its
/// `--remote-ui` client paired against the bare nvim `nvim_spec` describes.
///
/// `control_spec` and `nvim_spec` are two separately resolved sides of the
/// same fixture, so the control's server and the bare arm never share a
/// config directory, plugin state or probe socket. The control arm takes
/// the measured side's type because that is the role it plays here: this
/// row's ratio is nvim's own TUI charged against the same bare-nvim
/// baseline the echo row uses.
///
/// # Errors
///
/// Returns [`BenchError::Desync`] if the headless server never listens, or
/// any error the echo protocol itself raises.
pub fn run(
    control_spec: ViewSpec<'_>,
    nvim_spec: NvimSpec<'_>,
    socket: PathBuf,
    protocol: &Protocol,
    settle_deadline: Duration,
) -> Result<EchoOutcome, BenchError> {
    let ViewSpec(control) = control_spec;
    let server = RemoteUiServer::start(control, socket)?;
    // the client takes the server's side, not the bare arm's: the two arms
    // hold separate config directories and probe-socket addresses, and a
    // client resolving the bare arm's paths would contend for both
    let client = server.client_spec(control);
    let outcome = echo::run(
        ViewSpec(&client),
        nvim_spec,
        protocol,
        settle_deadline,
        echo::DEFAULT_STARTUP_QUIET,
    );
    // the server outlives the sampling deliberately: dropping it mid-run
    // would tear the client's buffer out from under a sample in flight and
    // report the resulting desync as a latency reading
    drop(server);
    outcome
}
