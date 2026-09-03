//! Falsifiable proof that the C0 bytes a terminal sends for `<C-\>`,
//! `<C-]>`, `<C-^>` and `<C-_>` fire the mapping a user wrote against those
//! names in `view`, by reading the pinned engine's own decoding rather than
//! a table transcribed from it.
//!
//! Each byte is typed at two ptys -- one hosting the wired `view` binary,
//! one hosting the pinned `nvim` directly -- with the same normal-mode
//! mapping installed in both, and the two screens must agree. A transcribed
//! byte-to-name table can only ever be checked against itself; this asks
//! the engine what it calls the byte by seeing which mapping it runs, so a
//! name view invents fails here whatever the table says.
//!
//! The bare-nvim side is the terminal case for the same reason the `view`
//! side is: an `--embed` child never decodes a terminal byte at all, and it
//! is that decode -- nvim's own tty input layer -- that names the byte.
//!
//! Both sessions run over [`view_oracle::pty::QueryPolicy::AnswerDa1`], so
//! neither editor pushes the kitty keyboard protocol and the bytes below
//! are what a real terminal would then send. A terminal that speaks the
//! protocol reports these chords as `CSI u` instead and never sends these
//! bytes at all, which is a decode this harness cannot pose: the responder
//! answers queries, it does not re-encode keystrokes. That crossing is
//! pinned in `view-tui/src/keys.rs` against the same engine's answers.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use view_oracle::PtySession;

const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Budget for anything either session has to do: a cold `view` spawn plus
/// an nvim spawn on a loaded box is the slow part, and every wait here is
/// satisfied in milliseconds by a healthy session.
const BUDGET: Duration = Duration::from_secs(20);

/// The byte a terminal sends, the notation a user's mapping is written in,
/// and the text that mapping writes into line 1.
///
/// Each marker is a word nothing else on either screen spells, so a screen
/// carrying it can only be the mapping having run.
const CHORDS: [(u8, &str, &str); 4] = [
    (0x1c, r"<C-\>", "REACHEDBACKSLASH"),
    (0x1d, "<C-]>", "REACHEDBRACKET"),
    (0x1e, "<C-^>", "REACHEDCARET"),
    (0x1f, "<C-_>", "REACHEDUNDERSCORE"),
];

/// A `view` session with every native feature off, so the screen it paints
/// is nvim's own content and the two sides are comparable row for row.
fn view_session(paths: &common::ScratchPaths) -> PtySession {
    let mut cmd = portable_pty::CommandBuilder::new(common::view_bin_path());
    cmd.arg(&paths.scratch);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against target/debug/view");
    assert!(
        session.wait_for("~", BUDGET),
        "view never painted nvim's real content; screen:\n{}",
        session.screen()
    );
    session
}

/// The pinned `nvim` in a pty of its own, started on the same scratch file
/// with none of the host's configuration.
fn nvim_session(paths: &common::ScratchPaths) -> PtySession {
    let cfg = view_engine::EngineConfig::isolated();
    let mut cmd = portable_pty::CommandBuilder::new(&cfg.nvim_bin);
    for arg in &cfg.extra_args {
        cmd.arg(arg);
    }
    cmd.arg(&paths.scratch);
    common::isolate_xdg_native_off(&mut cmd, &paths.isolated_home);
    let mut session = PtySession::spawn_configured(cmd, COLS, ROWS)
        .expect("PtySession::spawn_configured against the pinned nvim");
    assert!(
        session.wait_for("~", BUDGET),
        "nvim never painted its own content; screen:\n{}",
        session.screen()
    );
    session
}

/// Which of the four markers the session is showing.
///
/// The comparison the two sides can hold each other to: view frames nvim's
/// content in chrome of its own, so the rendered rows are not comparable
/// cell for cell, while the buffer text under them is exactly what the two
/// editors must agree on. A marker on screen is one editor having run one
/// mapping, and the set names which.
fn markers_shown(session: &mut PtySession) -> Vec<&'static str> {
    let screen = session.screen();
    CHORDS
        .iter()
        .filter(|(_, _, marker)| screen.contains(marker))
        .map(|(_, _, marker)| *marker)
        .collect()
}

/// Installs one normal-mode mapping and waits for the command line it was
/// typed on to clear, so the byte sent next cannot land on a cmdline.
fn install_mapping(session: &mut PtySession, notation: &str, marker: &str) {
    let command = format!(":nnoremap {notation} :call setline(1,'{marker}')<CR>\r");
    session.send(command.as_bytes()).unwrap();
    assert!(
        session.wait_for_screen(BUDGET, |screen| !screen.contents().contains("nnoremap")),
        "the mapping command never left the command line; screen:\n{}",
        session.screen()
    );
}

/// The whole point: the same byte, the same mapping, the same screen.
///
/// Both sides are asserted against the marker as well as against each
/// other. Two sessions agreeing on a line that says nothing would be a
/// pass for a `view` that forwarded the byte as a key nvim ignores.
#[test]
fn the_control_bytes_fire_the_mappings_their_nvim_names_are_written_against() {
    let view_paths = common::ScratchPaths::new("control-byte-view");
    let nvim_paths = common::ScratchPaths::new("control-byte-nvim");
    let mut under_test = view_session(&view_paths);
    let mut reference = nvim_session(&nvim_paths);

    for (byte, notation, marker) in CHORDS {
        install_mapping(&mut under_test, notation, marker);
        install_mapping(&mut reference, notation, marker);

        under_test.send(&[byte]).unwrap();
        reference.send(&[byte]).unwrap();

        let reached = |screen: &vt100::Screen| screen.contents().contains(marker);
        let seen_by_nvim = reference.wait_for_screen(BUDGET, reached);
        let seen_by_view = under_test.wait_for_screen(BUDGET, reached);
        let by_nvim = markers_shown(&mut reference);
        let by_view = markers_shown(&mut under_test);

        assert!(
            seen_by_nvim,
            "the pinned engine did not run its own {notation} mapping for byte \
             {byte:#04x}, so this leg proves nothing about view; screen:\n{}",
            reference.screen()
        );
        assert_eq!(
            by_view,
            by_nvim,
            "byte {byte:#04x}: nvim ran the mapping a user wrote as {notation} and view \
             forwarded the byte under some other name (view saw the marker: \
             {seen_by_view}); view's screen:\n{}",
            under_test.screen()
        );
        assert_eq!(
            by_nvim,
            vec![marker],
            "byte {byte:#04x}: both sides agree on a screen that is not the mapping's own \
             marker, so neither ran it"
        );
    }

    for session in [&mut under_test, &mut reference] {
        session.send(b"\x1b:qa!\r").unwrap();
        let _ = session.wait_for_exit(BUDGET);
    }
}
