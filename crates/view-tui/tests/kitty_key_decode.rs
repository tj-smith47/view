//! The keys the kitty keyboard protocol makes reachable, decoded end to end
//! from terminal bytes to nvim input notation.
//!
//! `terminal::enter_bytes` pushing `CSI > 1 u` is only half the contract:
//! the other half is that what a terminal sends back under that flag
//! survives crossterm's parser and view's encoder as a distinct key. In
//! legacy mode a shifted and a plain `<CR>` are the same byte and `<C-i>`
//! is `<Tab>`, so a default binding on `<S-CR>` is unreachable until both
//! hold. This is the second half, driven through a real pty on descriptor 0
//! rather than a hand-built `KeyEvent`, so crossterm's own `CSI u` parse is
//! part of what is proven.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::os::fd::AsFd;
use view_core::msg::Msg;
use view_tui::input::InputSource;
use view_tui::terminal::TermSizeCell;

/// `<S-CR>` and `<C-i>` as a kitty-protocol terminal reports them:
/// `CSI 13;2u` is codepoint 13 (`CR`) with modifier 2 (shift), `CSI 105;5u`
/// codepoint 105 (`i`) with modifier 5 (ctrl). Both are indistinguishable
/// from `<CR>` and `<Tab>` without the protocol.
const SHIFT_ENTER_THEN_CTRL_I: &[u8] = b"\x1b[13;2u\x1b[105;5u";

#[test]
fn csi_u_keys_reach_nvim_notation_as_shift_enter_and_ctrl_i() {
    let (master, slave) = common::stdin_pty();
    let mut input = InputSource::open().unwrap();

    rustix::io::write(&master, SHIFT_ENTER_THEN_CTRL_I).unwrap();
    assert!(
        common::wait_readable(slave.as_fd()),
        "the pty never delivered the bytes written into its master"
    );

    let size = TermSizeCell::default();
    let mut drained = Vec::new();
    input.drain(&size, |msg| drained.push(msg));

    let notations: Vec<&str> = drained
        .iter()
        .filter_map(|msg| match msg {
            Msg::Key(key) => Some(key.notation.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        notations,
        ["<S-CR>", "<C-i>"],
        "the protocol's disambiguated keys must arrive as their own notation, not as <CR> \
         and <Tab>: {drained:?}"
    );

    drop(input);
    drop(master);
}
