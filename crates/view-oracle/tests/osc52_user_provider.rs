//! End-to-end proof that a `"+y` still reaches the terminal's clipboard when
//! the user's own `g:clipboard` has taken the provider slot view would
//! otherwise fill.
//!
//! `REGISTER_CLIPBOARD_CHUNK` (`crates/view-engine/src/nvim_api.rs`) installs
//! view's provider only when `vim.g.clipboard` is nil, so a config that names
//! nvim's built-in OSC 52 provider -- the common shape under `$SSH_TTY`, and
//! what `crates/view-engine/tests/clipboard_precedence.rs` pins as the correct
//! precedence -- takes the copy path away from view entirely. nvim then writes
//! the escape itself through `nvim_ui_send`, which reaches only a UI that
//! attached with the `stdout_tty` option. Every other clipboard leg in this
//! suite runs against an isolated XDG home with no `init.lua` at all, so
//! `vim.g.clipboard` is always nil there and view's own provider always wins:
//! this file drives the other branch, at the one layer where "the bytes left
//! the process" is observable.
//!
//! A real `view` binary on a pty, like `osc52_remote_identity.rs` and for the
//! same reason: the claim is about bytes on a terminal, and neither
//! `EngineSession` nor an `Effect`-layer assertion can see them.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

/// The line yanked into `"+`. Distinct from every other oracle fixture's
/// marker so a raw-output grep cannot cross-match another session.
const MARKER: &str = "view-user-provider-marker";

/// `MARKER` with the trailing newline nvim's own provider receives for a
/// linewise yank, base64-encoded -- the payload the escape must carry.
const EXPECTED_PAYLOAD: &str = "dmlldy11c2VyLXByb3ZpZGVyLW1hcmtlcgo=";

/// Where the config below records every `nvim_ui_send` payload, relative to
/// the session's isolated `XDG_STATE_HOME`.
///
/// Recorded inside nvim rather than read off the pty because the interesting
/// half is what view *refuses* to write out: nvim's own `nvim.tty` defaults
/// query the terminal the moment they find a `stdout_tty` UI, and those bytes
/// would be dropped before reaching any terminal. The only place the query is
/// visible is where it is issued.
const UI_SEND_LOG: &str = "ui-sends";

/// The user config under test: nvim's built-in OSC 52 provider, installed the
/// way `:help clipboard-osc52` documents it, plus the recorder above.
const OSC52_PROVIDER_INIT: &str = "\
local osc52 = require('vim.ui.clipboard.osc52')
vim.g.clipboard = {
  name = 'OSC 52',
  copy = { ['+'] = osc52.copy('+'), ['*'] = osc52.copy('*') },
  paste = { ['+'] = osc52.paste('+'), ['*'] = osc52.paste('*') },
}
local log = vim.env.XDG_STATE_HOME .. '/ui-sends'
local send = vim.api.nvim_ui_send
vim.api.nvim_ui_send = function(content)
  local f = io.open(log, 'a')
  if f then
    f:write(content)
    f:close()
  end
  return send(content)
end
";

/// The first complete OSC 52 escape in `raw`, or `None`.
///
/// A byte-pattern search because OSC 52 leaves no cell on the parsed screen
/// (the same reasoning `osc52_remote_identity.rs` states for its own).
fn extract_osc52(raw: &[u8]) -> Option<&[u8]> {
    const PREFIX: &[u8] = b"\x1b]52;";
    const TERMINATOR: &[u8] = b"\x1b\\";
    let start = raw.windows(PREFIX.len()).position(|w| w == PREFIX)?;
    let rel_end = raw[start..]
        .windows(TERMINATOR.len())
        .position(|w| w == TERMINATOR)?;
    Some(&raw[start..start + rel_end + TERMINATOR.len()])
}

fn wait_for_osc52(session: &mut view_oracle::PtySession, timeout: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(bytes) = extract_osc52(session.raw_output()) {
            return Some(bytes.to_vec());
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_yank_through_the_users_own_osc52_provider_reaches_the_terminal() {
    let bin = common::view_bin_path();
    let paths = common::ScratchPaths::new("osc52-user-provider");
    let nvim_config = common::xdg_home(&paths.isolated_home, "XDG_CONFIG_HOME").join("nvim");
    std::fs::create_dir_all(&nvim_config).expect("the isolated nvim config dir must be creatable");
    let state_home = common::xdg_home(&paths.isolated_home, "XDG_STATE_HOME");
    std::fs::create_dir_all(&state_home).expect("the isolated state dir must be creatable");
    std::fs::write(nvim_config.join("init.lua"), OSC52_PROVIDER_INIT)
        .expect("the provider init.lua must be writable");

    let mut cmd = portable_pty::CommandBuilder::new(&bin);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);
    let mut session = view_oracle::PtySession::spawn_configured(cmd, 80, 24)
        .unwrap_or_else(|err| panic!("PtySession::spawn_configured against {bin:?}: {err}"));
    session.record_raw_output();

    assert!(
        session.wait_for("~", Duration::from_secs(10)),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );
    session
        .send(format!("i{MARKER}\x1b").as_bytes())
        .expect("typed marker must reach the session");
    assert!(
        session.wait_for(MARKER, Duration::from_secs(10)),
        "typed marker never appeared on screen; screen:\n{}",
        session.screen()
    );
    session
        .send(b"0\"+yy")
        .expect("the yank keys must reach the session");

    let escape = wait_for_osc52(&mut session, Duration::from_secs(10)).unwrap_or_else(|| {
        panic!(
            "no OSC 52 escape reached the terminal: nvim's own provider owns this \
             copy and writes it through `nvim_ui_send`, which is delivered only to \
             a UI attached with `stdout_tty`. Raw output:\n{}",
            String::from_utf8_lossy(session.raw_output())
        )
    });

    let expected = format!("\x1b]52;c;{EXPECTED_PAYLOAD}\x1b\\");
    assert_eq!(
        String::from_utf8_lossy(&escape),
        expected,
        "the escape nvim formed must reach the terminal byte for byte"
    );

    // the other half of claiming a terminal: nvim's `nvim.tty` defaults
    // query one the moment they find a `stdout_tty` UI and then block
    // `VimEnter` for 100 ms on a reply view has no channel to give, eating
    // the keystrokes pressed in that window. Claiming it after those defaults
    // have stopped looking is what keeps the query out of this log -- with
    // the claim moved back to attach time, this session's startup measured
    // 123-128 ms against 18-19 ms, and the log opens with `OSC 11 ; ?`
    let sent = std::fs::read_to_string(state_home.join(UI_SEND_LOG)).unwrap_or_default();
    assert_eq!(
        sent, expected,
        "nvim sent something other than the one clipboard write: a terminal \
         query here means the tty was claimed while nvim's own tty defaults \
         were still looking for one"
    );

    let _ = session.send(b"\x1b:q!\r");
    let _ = session.wait_for_exit(Duration::from_secs(5));
}
