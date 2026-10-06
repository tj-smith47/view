//! The swap-off request a spawn passed `-n` sends before anything else.
//!
//! nvim applies `-n` and every `--cmd` only once a UI has attached, and a
//! child driven over RPC alone runs each command ahead of that with
//! `'updatecount'` at its default, writing swap files that concurrent
//! children with the same swap directory collide on.

use std::ffi::OsString;
use std::time::Duration;

use rmpv::Value;

use crate::handle::{EngineError, EngineHandle};

/// Sets `'updatecount'` to 0 when `args` pass `-n` ahead of any `--`, which
/// ends nvim's options and starts its file names.
pub(crate) fn follow_no_swap(
    handle: &EngineHandle,
    args: &[OsString],
    timeout: Duration,
) -> Result<(), EngineError> {
    if !passes_no_swap(args) {
        return Ok(());
    }
    let off = vec![Value::from("set updatecount=0")];
    handle
        .request_timeout("nvim_command", off, timeout)
        .map(drop)
}

fn passes_no_swap(args: &[OsString]) -> bool {
    args.iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "-n")
}

#[cfg(test)]
mod tests {
    use super::passes_no_swap;
    use std::ffi::OsString;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_file_named_dash_n_after_the_double_dash_asks_for_nothing() {
        assert!(passes_no_swap(&args(&["--clean", "-n"])));
        assert!(!passes_no_swap(&args(&["--", "-n"])));
        assert!(!passes_no_swap(&args(&["--clean", "--", "-n"])));
        assert!(!passes_no_swap(&args(&["--cmd", "set nu"])));
    }
}
