//! End-to-end proof that a `"+p` returns text, and returns it promptly, when
//! the user's own `g:clipboard` has taken the provider slot view would
//! otherwise fill.
//!
//! The other half of `osc52_user_provider.rs`, which covers the copy. nvim's
//! OSC 52 provider pastes by sending `OSC 52 ; c ; ?` through `nvim_ui_send`
//! and then waiting on a `TermResponse` the UI owes it
//! (`$VIMRUNTIME/lua/vim/ui/clipboard/osc52.lua`). view never forwards that
//! query to the real terminal -- its stdin is a key decoder, so a terminal's
//! answer would be typed into the buffer -- and answers it from view's own
//! clipboard instead, through `nvim_ui_term_event`.
//!
//! Time is half the assertion here, not a convenience. Left unanswered, the
//! provider waits one second, echoes a 73-character notice that raises a
//! hit-enter prompt on any window under 100 columns (measured: an indefinite
//! wedge at 80x24, the size this test runs at), and only then waits nine
//! seconds more. So a paste that lands at all, inside a bounded deadline,
//! *is* the proof the answer arrived inside the provider's first wait.
//!
//! A real `view` binary on a pty, like `osc52_user_provider.rs`, and for one
//! more reason of its own: the copy that feeds the paste crosses three
//! threads (loop, clipboard worker, RPC writer), and no `Effect`-layer
//! assertion sees that seam.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

/// The line yanked into `"+` and pasted back. Distinct from every other
/// oracle fixture's marker so a raw-output grep cannot cross-match another
/// session.
const MARKER: &str = "view-user-provider-paste-marker";

/// Typed after the paste in the empty-clipboard leg: proof the session is
/// still taking keys, which a wedged hit-enter prompt would swallow.
const AFTER_PASTE: &str = "still-alive";

/// Where the config below records every `nvim_ui_send` payload, relative to
/// the session's isolated `XDG_STATE_HOME`.
const UI_SEND_LOG: &str = "ui-sends";

/// The user config under test: nvim's built-in OSC 52 provider, installed
/// the way `:help clipboard-osc52` documents it, plus the recorder. The same
/// fixture `osc52_user_provider.rs` plants, so both legs drive one config.
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

/// A session on the provider config above, already painted and holding an
/// isolated `XDG_STATE_HOME` the recorder writes into.
fn provider_session(
    label: &str,
) -> (
    view_oracle::PtySession,
    std::path::PathBuf,
    common::ScratchPaths,
) {
    let bin = common::view_bin_path();
    let paths = common::ScratchPaths::new(label);
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
    (session, state_home, paths)
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[test]
fn a_paste_through_the_users_own_osc52_provider_returns_the_yanked_text() {
    let (mut session, state_home, _paths) = provider_session("osc52-user-provider-paste");

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
    // the copy has to be through the worker before the paste asks for it;
    // the escape reaching the pty is the last observable step of that trip
    let yank_deadline = Instant::now() + view_test_support::host_deadline(Duration::from_secs(10));
    while !String::from_utf8_lossy(session.raw_output()).contains("\x1b]52;c;") {
        assert!(
            Instant::now() < yank_deadline,
            "the yank's own escape never reached the terminal, so the copy \
             never happened and this leg would prove nothing about the paste"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let started = Instant::now();
    session
        .send(b"\"+p")
        .expect("the paste keys must reach the session");
    let pasted = session.wait_for_screen(Duration::from_secs(8), |screen| {
        occurrences(&screen.contents(), MARKER) >= 2
    });
    let elapsed = started.elapsed();
    assert!(
        pasted,
        "the pasted line never appeared: nvim's provider asked view for the \
         clipboard through an OSC 52 read query and got no answer, so it sat \
         in vim.wait. Screen:\n{}",
        session.screen()
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the paste took {elapsed:?}: anything past the provider's first \
         vim.wait(1000) means the answer did not come from view's own \
         clipboard"
    );

    // the query is answered, never forwarded: a terminal that did answer it
    // would answer onto view's stdin, where the key decoder types the reply
    // into the buffer
    let raw = String::from_utf8_lossy(session.raw_output()).into_owned();
    assert!(
        !raw.contains("\x1b]52;c;?"),
        "the read query must never reach the terminal; raw output:\n{raw}"
    );

    // nvim's side of the same claim: it sent exactly the copy and the query,
    // and view wrote out only the first of the two
    let sent = std::fs::read_to_string(state_home.join(UI_SEND_LOG)).unwrap_or_default();
    assert!(
        sent.contains("\x1b]52;c;?"),
        "nvim's provider must have issued the read query this leg answers; log: {sent:?}"
    );

    let _ = session.send(b"\x1b:q!\r");
    let _ = session.wait_for_exit(Duration::from_secs(5));
}

/// The failure policy, end to end: a paste with nothing to report answers an
/// empty payload rather than falling silent. Silence is not a slow success
/// here -- at this width it is a hit-enter prompt that eats every later
/// keystroke -- so the discriminator is that the session still accepts
/// typing right after the paste.
#[test]
fn a_paste_with_an_empty_clipboard_returns_at_once_instead_of_wedging() {
    let (mut session, _state_home, _paths) = provider_session("osc52-user-provider-paste-empty");

    session
        .send(b"\"+p")
        .expect("the paste keys must reach the session");
    let started = Instant::now();
    // the newlines put the marker below the startup toast stack: a box holds
    // its slot for the transient timeout, so a marker typed onto row 1 is
    // occluded rather than missing, and the wait would read the stack's own
    // expiry as this leg's latency. A wedged paste swallows these keys too,
    // so the discriminator is unchanged
    session
        .send(format!("i\r\r\r\r\r\r\r\r\r\r{AFTER_PASTE}\x1b").as_bytes())
        .expect("the follow-up keys must reach the session");
    let alive = session.wait_for(AFTER_PASTE, Duration::from_secs(8));
    let elapsed = started.elapsed();
    assert!(
        alive,
        "the session stopped taking keys after an unanswerable paste: an \
         unanswered OSC 52 query raises nvim's hit-enter prompt at this \
         width, which swallows everything typed after it. Screen:\n{}",
        session.screen()
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "typing resumed only after {elapsed:?}, which is the provider's own \
         timeout rather than an immediate empty answer"
    );

    let _ = session.send(b"\x1b:q!\r");
    let _ = session.wait_for_exit(Duration::from_secs(5));
}
