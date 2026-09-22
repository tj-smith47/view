//! Live proof that a desktop chord reaches view through a real terminal and
//! moves real nvim focus, on both modifier spellings, and that the editor
//! profile turns the chord off while nvim's own window key keeps working.
//!
//! Everything up to the byte a terminal sends is pinned in `view-tui`'s
//! `keys.rs` unit tests (`a_super_chord_decodes_with_its_modifier` and its
//! siblings) and everything from the registered spec onward is pinned in
//! `view-native`'s `chord_plan` tests and `view-engine`'s
//! `register_mappings_live.rs`. Neither proves the two joined: a byte a
//! terminal actually sends, decoded by the real binary, moving the focus of
//! two real nvim windows.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use view_oracle::{PtySession, QueryPolicy};

const COLS: u16 = 80;
const ROWS: u16 = 24;
const BUDGET: Duration = Duration::from_secs(20);

/// `<D-Left>`, the CSI-u form a terminal speaking the kitty keyboard
/// protocol sends for `Super`+`Left`: modifier field `9` is base `1` plus
/// the `0b1000` super bit `keys.rs`'s `modifiers()` now reads.
const SUPER_LEFT: &[u8] = b"\x1b[1;9D";

/// `<M-Left>`, the legacy escape-prefixed spelling every terminal sends for
/// `Alt`+`Left`, with no protocol in force.
const ALT_LEFT: &[u8] = b"\x1b\x1b[D";

/// `<C-w>h`, nvim's own window-left key, typed as the two raw bytes a
/// terminal sends for it: `Ctrl-w` (0x17) then `h`.
const CTRL_W_H: &[u8] = b"\x17h";

/// `<C-w>l`, the mirror of [`CTRL_W_H`].
const CTRL_W_L: &[u8] = b"\x17l";

/// `<M-,>`, the legacy escape-prefixed spelling of `Alt`+`comma` -- the
/// `notifications`/`dismiss` chord's alt spelling, and every terminal's own.
const ALT_COMMA: &[u8] = b"\x1b,";

/// Plants `[ui] panes = "nvim"`, every registry `[native]` feature off, and
/// `[keys] profile = "desktop"`: the desktop profile forced rather than
/// derived, so this leg proves the chord itself rather than the
/// environment guess `detect_profile` makes (already pinned in
/// `view-native::config::profile`). The registry features are off so no
/// native chrome (the statusline, a picker overlay) parks the terminal's
/// real cursor somewhere this leg's column read does not expect -- this
/// leg's subject is nvim's own window focus, which `cursor_position` reads
/// directly only when nvim, not a native surface, owns the cursor.
fn plant_desktop_profile(home: &std::path::Path) {
    let dir = common::xdg_home(home, "XDG_CONFIG_HOME").join("view");
    std::fs::create_dir_all(&dir).expect("the isolated config home must be creatable");
    let mut text = String::from("[ui]\npanes = \"nvim\"\n\n[native]\n");
    for feature in view_core::native::registry::features() {
        text.push_str(feature.id);
        text.push_str(" = false\n");
    }
    text.push_str("\n[keys]\nprofile = \"desktop\"\n");
    std::fs::write(dir.join("view.toml"), text).expect("the isolated view.toml must be writable");
}

/// Spawns `view` on a fresh scratch buffer under the desktop profile,
/// answering the terminal's capability probe at `policy`, opens a vertical
/// split with distinct text in each half, and leaves focus on the right
/// window.
fn spawn_split(label: &str, policy: QueryPolicy) -> (common::ScratchPaths, PtySession) {
    let paths = common::ScratchPaths::new(label);
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.cwd(
        paths
            .scratch
            .parent()
            .expect("the scratch file always sits inside the scratch root"),
    );
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
    plant_desktop_profile(&paths.isolated_home);

    let mut session = PtySession::spawn_configured_with(cmd, COLS, ROWS, policy)
        .expect("PtySession::spawn_configured_with against target/debug/view");
    assert!(
        session.wait_for("~", BUDGET),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:vsplit\r").unwrap();
    session.send(b"\x1b:enew\rileft\x1b").unwrap();
    session.send(CTRL_W_L).unwrap();
    session.send(b"\x1b:enew\riright\x1b").unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| screen.contents().contains("left")
            && screen.contents().contains("right")),
        "the split never showed both windows' own text; screen:\n{}",
        session.screen()
    );
    (paths, session)
}

/// The cursor's column, waited for to cross `left` of the split (`< 35`)
/// or the right (`> 42`), whichever `want_left` asks for. `80` columns
/// split by `:vsplit` puts the separator at column 39 -- left window
/// columns 0..38, right window 40..79 -- so this reads focus on either
/// side of it without needing the exact column nvim chose for the text.
///
/// The last row (`ROWS - 1`) is nvim's own cmdline, not a window: its
/// cursor sits wherever the last typed command ended, a column that can
/// coincidentally fall in either window's range while the row itself never
/// moved for either -- a `:View keys profile editor` flip once read as
/// "focus moved left" purely because its cursor happened to land at
/// column 25 on that row. Excluding it is what makes this predicate a
/// window-focus read rather than a column coincidence.
fn wait_for_focus_side(session: &mut PtySession, want_left: bool, timeout: Duration) -> bool {
    session.wait_for_screen(timeout, |screen| {
        let (row, col) = screen.cursor_position();
        if row == ROWS - 1 {
            return false;
        }
        if want_left {
            col < 35
        } else {
            col > 42
        }
    })
}

/// `Super`+`Left` moves focus left under the kitty keyboard protocol, and
/// `Alt`+`Left` moves it under the legacy fallback a session with no
/// protocol builds -- the same chord, the two spellings `chord_plan`
/// produces for [`DesktopModifier::Super`] and [`DesktopModifier::Alt`].
#[test]
fn a_desktop_chord_moves_focus_under_either_modifier_spelling() {
    {
        let (_paths, mut session) = spawn_split("chord-super", QueryPolicy::AnswerFullTier);
        assert!(
            wait_for_focus_side(&mut session, false, BUDGET),
            "the split must leave focus on the right window before the chord; screen:\n{}",
            session.screen()
        );
        session.send(SUPER_LEFT).unwrap();
        assert!(
            wait_for_focus_side(&mut session, true, BUDGET),
            "Super+Left never moved focus to the left window; screen:\n{}",
            session.screen()
        );
    }
    {
        let (_paths, mut session) = spawn_split("chord-alt", QueryPolicy::AnswerDa1);
        assert!(
            wait_for_focus_side(&mut session, false, BUDGET),
            "the split must leave focus on the right window before the chord; screen:\n{}",
            session.screen()
        );
        session.send(ALT_LEFT).unwrap();
        assert!(
            wait_for_focus_side(&mut session, true, BUDGET),
            "Alt+Left never moved focus to the left window under the no-protocol fallback; screen:\n{}",
            session.screen()
        );
    }
}

/// A live `:View keys profile editor` flip stops the chord from moving
/// focus -- `chord_plan` returns nothing under [`KeyProfile::Editor`], and a
/// reissue that drops every desktop spec is what a flip sends -- while
/// nvim's own `<C-w>h` still does, because that key was never view's to
/// take.
#[test]
fn a_profile_flip_stops_the_chord_while_ctrl_w_h_still_moves_focus() {
    let (_paths, mut session) = spawn_split("chord-flip", QueryPolicy::AnswerFullTier);
    assert!(
        wait_for_focus_side(&mut session, false, BUDGET),
        "the split must leave focus on the right window before the flip; screen:\n{}",
        session.screen()
    );

    session.send(SUPER_LEFT).unwrap();
    assert!(
        wait_for_focus_side(&mut session, true, BUDGET),
        "the chord must still move focus before the flip; screen:\n{}",
        session.screen()
    );
    session.send(CTRL_W_L).unwrap();
    assert!(
        wait_for_focus_side(&mut session, false, BUDGET),
        "moving back to the right window before the flip must still work; screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:View keys profile editor\r").unwrap();
    // the reissue is an asynchronous RegisterMappings call, and nothing on
    // screen changes when it lands (focus never moved to begin with), so
    // there is no predicate to wait on -- a fixed pause stands in for the
    // round trip before the negative assertion below relies on it having
    // landed.
    std::thread::sleep(view_test_support::host_deadline(Duration::from_millis(800)));

    session.send(SUPER_LEFT).unwrap();
    // a negative wait: give the (now unregistered) chord a real chance to
    // fire before concluding it did not
    std::thread::sleep(view_test_support::host_deadline(Duration::from_millis(500)));
    assert!(
        !wait_for_focus_side(&mut session, true, Duration::from_millis(200)),
        "the chord must not move focus once the editor profile is live; screen:\n{}",
        session.screen()
    );

    session.send(CTRL_W_H).unwrap();
    assert!(
        wait_for_focus_side(&mut session, true, BUDGET),
        "<C-w>h must still move focus under the editor profile; screen:\n{}",
        session.screen()
    );
}

/// Plants `notifications` on (every other registry feature off, for the
/// same reason [`plant_desktop_profile`] turns them off) and `profile`
/// asked for by `profile`, so this leg's chord reaches a real `Rhs::Invoke`
/// dispatch rather than the `Rhs::Keys` `focus_left` both legs above press.
fn plant_notifications_under(home: &std::path::Path, profile: &str) {
    let dir = common::xdg_home(home, "XDG_CONFIG_HOME").join("view");
    std::fs::create_dir_all(&dir).expect("the isolated config home must be creatable");
    let mut text = String::from("[native]\n");
    for feature in view_core::native::registry::features() {
        if feature.id != "notifications" {
            text.push_str(feature.id);
            text.push_str(" = false\n");
        }
    }
    text.push_str(&format!("\n[keys]\nprofile = \"{profile}\"\n"));
    std::fs::write(dir.join("view.toml"), text).expect("the isolated view.toml must be writable");
}

/// `notifications dismiss` (`<M-,>`) end to end: a real toast raised, a real
/// chord byte sent, the toast gone -- and, first, the same byte sent under
/// the editor profile, where `chord_plan` registers no desktop chord at
/// all, proving the toast survives the keystroke on its own rather than
/// happening to time out against whatever `wait_for_screen`'s own budget
/// is. Without that leg, a chord that had silently stopped reaching
/// `Rhs::Invoke` at all -- registered, but never firing -- would pass this
/// test exactly as a working one does: nothing here would tell the two
/// apart from a toast that dismissed itself. `focus_left`, the chord both
/// legs above press, is `Rhs::Keys` -- it sets an existing nvim key and
/// costs nothing beyond what typing that key already costs. This is the
/// table's other shape: `Rhs::Invoke` sends `rpcnotify` to view's own
/// bridge, which `Msg::FeatureInvoke` dispatches to
/// `Messages::dismiss_newest` (`update/mod.rs`), a path no `Keys` chord
/// exercises.
#[test]
fn an_invoke_chord_reaches_its_verb_end_to_end() {
    let paths = common::ScratchPaths::new("chord-invoke");
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.cwd(
        paths
            .scratch
            .parent()
            .expect("the scratch file always sits inside the scratch root"),
    );
    common::isolate_xdg_first_launch(&mut cmd, &paths.isolated_home);
    plant_notifications_under(&paths.isolated_home, "editor");

    let mut session = PtySession::spawn_configured_with(cmd, COLS, ROWS, QueryPolicy::AnswerDa1)
        .expect("PtySession::spawn_configured_with against target/debug/view");
    assert!(
        session.wait_for("~", BUDGET),
        "view never painted its startup shell; screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:echoerr 'chordinvoketoken'\r").unwrap();
    assert!(
        session.wait_for("chordinvoketoken", BUDGET),
        "the echoerr toast never appeared; screen:\n{}",
        session.screen()
    );

    session.send(ALT_COMMA).unwrap();
    // a negative wait: give the (unregistered, under editor) chord a real
    // chance to fire before concluding it did not
    std::thread::sleep(view_test_support::host_deadline(Duration::from_millis(500)));
    assert!(
        session.screen().contains("chordinvoketoken"),
        "the toast must still stand under the editor profile, where no \
         desktop chord is registered at all; screen:\n{}",
        session.screen()
    );

    session.send(b"\x1b:View keys profile desktop\r").unwrap();
    // the reissue is an asynchronous RegisterMappings call, and nothing on
    // screen changes when it lands, so there is no predicate to wait on --
    // a fixed pause stands in for the round trip before the chord below
    // relies on it having landed.
    std::thread::sleep(view_test_support::host_deadline(Duration::from_millis(800)));

    session.send(ALT_COMMA).unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| !screen
            .contents()
            .contains("chordinvoketoken")),
        "notifications dismiss (<M-,>) never took the toast down once the \
         desktop profile was live; screen:\n{}",
        session.screen()
    );
}
