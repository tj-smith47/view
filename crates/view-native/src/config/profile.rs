//! `[keys] profile` and `[keys] desktop_modifier`, derived where a user
//! writes neither (spec section 9).
//!
//! This module carries the two pure derivations only: what `"auto"`
//! resolves to for the profile, from the environment, and what `"auto"`
//! resolves to for the modifier, from the kitty keyboard protocol probe.
//! Layering an explicit `view.toml`/environment override on top of the
//! profile derivation, and building a takeover's [`chord_plan`] from the
//! result, are `resolve_with`'s and the takeover site's own job.
//!
//! [`chord_plan`]: view_core::native::chords::DesktopChord

use view_core::native::chords::{DesktopModifier, KeyProfile, ModifierChoice};

/// The `SSH_CONNECTION` marker: an ssh session resolves [`KeyProfile::Desktop`]
/// because the chords a remote desktop holds are held on the client, not on
/// the box this session runs on.
const SSH_CONNECTION: &str = "SSH_CONNECTION";
const SSH_TTY: &str = "SSH_TTY";
const WAYLAND_DISPLAY: &str = "WAYLAND_DISPLAY";
const DISPLAY: &str = "DISPLAY";

/// The four names above, for the sibling test that proves the resolver
/// reads no environment name outside its own registry and this module's own
/// vocabulary.
#[cfg(test)]
pub(crate) const PROFILE_MARKERS: [&str; 4] = [SSH_CONNECTION, SSH_TTY, WAYLAND_DISPLAY, DISPLAY];

/// What `[keys] profile = "auto"` derives, read in the order spec section 9
/// states, first match wins. The marker names which environment variable
/// decided it, for the report row; the bare-tty fallback and the two
/// `cfg!(target_os)` rows carry no marker, the way `[ui] panes` prints a
/// bare `tiles` with nothing beside it.
#[must_use]
pub fn detect_profile(env: &dyn Fn(&str) -> Option<String>) -> (KeyProfile, Option<&'static str>) {
    if env(SSH_CONNECTION).is_some() {
        return (KeyProfile::Desktop, Some(SSH_CONNECTION));
    }
    if env(SSH_TTY).is_some() {
        return (KeyProfile::Desktop, Some(SSH_TTY));
    }
    if env(WAYLAND_DISPLAY).is_some() {
        return (KeyProfile::Editor, Some(WAYLAND_DISPLAY));
    }
    if env(DISPLAY).is_some() {
        return (KeyProfile::Editor, Some(DISPLAY));
    }
    if cfg!(target_os = "macos") || cfg!(target_os = "windows") {
        return (KeyProfile::Editor, None);
    }
    (KeyProfile::Desktop, None)
}

/// `[keys] desktop_modifier = "super"` written with no kitty keyboard
/// protocol in force: the chords answer Alt this run, and the session owes
/// a startup notice saying so.
const SUPER_WITHOUT_PROTOCOL_NOTICE: &str =
    "view: [keys] desktop_modifier = super needs the kitty keyboard protocol, which this \
     terminal did not answer; the chords answer Alt this run";

/// The modifier this session's chords are spelled with, the marker the
/// `keys.desktop_modifier` report row prints, and the notice an
/// unreachable `super` owes.
#[must_use]
pub fn modifier_for(
    choice: ModifierChoice,
    kitty_kbd: bool,
) -> (DesktopModifier, &'static str, Option<&'static str>) {
    match (choice, kitty_kbd) {
        (ModifierChoice::Auto, true) => (
            DesktopModifier::Super,
            "super (kitty keyboard protocol)",
            None,
        ),
        (ModifierChoice::Auto, false) => (
            DesktopModifier::Alt,
            "alt (no kitty keyboard protocol)",
            None,
        ),
        (ModifierChoice::Super, true) => (
            DesktopModifier::Super,
            "super ([keys] desktop_modifier)",
            None,
        ),
        (ModifierChoice::Super, false) => (
            DesktopModifier::Alt,
            "alt ([keys] desktop_modifier, no kitty keyboard protocol)",
            Some(SUPER_WITHOUT_PROTOCOL_NOTICE),
        ),
        // `ModifierChoice::Alt`, and the wildcard `#[non_exhaustive]` forces
        // across the crate boundary: a choice this build does not know
        // resolves the same way `Alt` does, since alt is the modifier every
        // terminal delivers.
        (ModifierChoice::Alt, _) | (_, _) => {
            (DesktopModifier::Alt, "alt ([keys] desktop_modifier)", None)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn a_headless_session_derives_the_desktop_profile() {
        let (profile, marker) = detect_profile(&env_of(&[]));
        assert_eq!(profile, KeyProfile::Desktop);
        assert_eq!(marker, None);
    }

    #[test]
    fn an_ssh_session_derives_the_desktop_profile() {
        let (profile, marker) = detect_profile(&env_of(&[(SSH_CONNECTION, "10.0.0.1 1 2 22")]));
        assert_eq!(profile, KeyProfile::Desktop);
        assert_eq!(marker, Some(SSH_CONNECTION));

        let (profile, marker) = detect_profile(&env_of(&[(SSH_TTY, "/dev/pts/0")]));
        assert_eq!(profile, KeyProfile::Desktop);
        assert_eq!(marker, Some(SSH_TTY));
    }

    #[test]
    fn an_ssh_marker_outranks_a_display_marker() {
        let (profile, marker) = detect_profile(&env_of(&[
            (SSH_CONNECTION, "10.0.0.1 1 2 22"),
            (WAYLAND_DISPLAY, "wayland-0"),
        ]));
        assert_eq!(profile, KeyProfile::Desktop);
        assert_eq!(marker, Some(SSH_CONNECTION));
    }

    #[test]
    fn a_gui_desktop_derives_the_editor_profile() {
        let (profile, marker) = detect_profile(&env_of(&[(WAYLAND_DISPLAY, "wayland-0")]));
        assert_eq!(profile, KeyProfile::Editor);
        assert_eq!(marker, Some(WAYLAND_DISPLAY));

        let (profile, marker) = detect_profile(&env_of(&[(DISPLAY, ":0")]));
        assert_eq!(profile, KeyProfile::Editor);
        assert_eq!(marker, Some(DISPLAY));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_macos_client_derives_the_editor_profile_with_no_marker() {
        let (profile, marker) = detect_profile(&env_of(&[]));
        assert_eq!(profile, KeyProfile::Editor);
        assert_eq!(marker, None);
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn a_windows_client_derives_the_editor_profile_with_no_marker() {
        let (profile, marker) = detect_profile(&env_of(&[]));
        assert_eq!(profile, KeyProfile::Editor);
        assert_eq!(marker, None);
    }

    #[test]
    fn auto_takes_super_where_the_protocol_is_in_force() {
        let (modifier, row, notice) = modifier_for(ModifierChoice::Auto, true);
        assert_eq!(modifier, DesktopModifier::Super);
        assert_eq!(row, "super (kitty keyboard protocol)");
        assert_eq!(notice, None);
    }

    #[test]
    fn desktop_chords_degrade_to_alt_without_the_kitty_protocol() {
        let (modifier, row, notice) = modifier_for(ModifierChoice::Auto, false);
        assert_eq!(modifier, DesktopModifier::Alt);
        assert_eq!(row, "alt (no kitty keyboard protocol)");
        assert_eq!(notice, None);
    }

    #[test]
    fn an_explicit_super_with_the_protocol_reports_the_choice() {
        let (modifier, row, notice) = modifier_for(ModifierChoice::Super, true);
        assert_eq!(modifier, DesktopModifier::Super);
        assert_eq!(row, "super ([keys] desktop_modifier)");
        assert_eq!(notice, None);
    }

    #[test]
    fn an_explicit_super_without_the_protocol_notices() {
        let (modifier, row, notice) = modifier_for(ModifierChoice::Super, false);
        assert_eq!(modifier, DesktopModifier::Alt);
        assert_eq!(
            row,
            "alt ([keys] desktop_modifier, no kitty keyboard protocol)"
        );
        assert_eq!(notice, Some(SUPER_WITHOUT_PROTOCOL_NOTICE));
    }

    #[test]
    fn the_modifier_row_names_why_it_degraded() {
        let (_, row, _) = modifier_for(ModifierChoice::Alt, true);
        assert_eq!(row, "alt ([keys] desktop_modifier)");
        let (_, row, _) = modifier_for(ModifierChoice::Alt, false);
        assert_eq!(row, "alt ([keys] desktop_modifier)");
    }
}
