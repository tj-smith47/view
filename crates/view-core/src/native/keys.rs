//! Which keys resize the focused sidebar.
//!
//! The file tree and the agent panel are both sidebars and both answer the
//! same bindings, so the set is resolved once here rather than matched on
//! as a pair of constants inside each surface's own key arm. A binding is
//! one key, or two: a chord's first key decides nothing on its own and the
//! keystroke after it either completes a binding or is handled exactly as
//! it would have been alone. Two keys is the ceiling, which is what keeps
//! this a lookup rather than a keymap engine.
//!
//! Every spelling here is the notation `view-tui`'s `encode_key` emits,
//! since that is the only spelling the update loop ever sees: `<S-Right>`,
//! `<C-w>`, `>`, and `<lt>` for a literal `<`.

/// One binding: a key, and the key that has to follow it for the binding
/// to be a chord rather than a single press.
type Binding = (String, Option<String>);

/// Which way a binding steps the focused sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// One notch wider.
    Wider,
    /// One notch narrower.
    Narrower,
}

impl Direction {
    /// Whether this direction widens, in the form the sidebars' own resize
    /// methods take their argument.
    #[must_use]
    pub const fn widens(self) -> bool {
        matches!(self, Self::Wider)
    }
}

/// What a keystroke means to the focused sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// A whole binding: step the sidebar this way.
    Step(Direction),
    /// The first key of a chord. Nothing moves and nothing else claims the
    /// key; the keystroke after it decides.
    Pending,
}

/// The keys that resize whichever sidebar has the keyboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeKeys {
    wider: Vec<Binding>,
    narrower: Vec<Binding>,
}

impl Default for ResizeKeys {
    /// Two bindings per direction: a shifted arrow, and nvim's own
    /// window-resize chord.
    ///
    /// Two rather than one because a terminal that eats one still leaves
    /// the other -- macOS Terminal and Termius consume the shifted arrows
    /// before view is ever offered them, where Ctrl+w reaches through both
    /// -- and because a user arriving from nvim already resizes a window
    /// with `<C-w><` and `<C-w>>`, so the migration contract asks for no
    /// second reflex. `<C-w>` claims no composer key of its own: the agent
    /// panel swallows it today and always has.
    fn default() -> Self {
        Self {
            wider: vec![
                ("<S-Right>".to_string(), None),
                ("<C-w>".to_string(), Some(">".to_string())),
            ],
            narrower: vec![
                ("<S-Left>".to_string(), None),
                ("<C-w>".to_string(), Some("<lt>".to_string())),
            ],
        }
    }
}

impl ResizeKeys {
    /// Replaces the bindings for `direction` with the keys `spellings`
    /// names, reporting whether every one of them was a key this build can
    /// match.
    ///
    /// All or nothing per direction, and the defaults survive a refusal:
    /// half an applied list would leave a user resizing with a set neither
    /// their config nor this build's defaults describes. An empty list is
    /// applied rather than refused -- it is the way to leave a direction
    /// on no key at all.
    #[must_use]
    pub fn rebind(&mut self, direction: Direction, spellings: &[String]) -> bool {
        let Some(bindings) = spellings
            .iter()
            .map(|spelling| split_keys(spelling))
            .collect::<Option<Vec<Binding>>>()
        else {
            return false;
        };
        match direction {
            Direction::Wider => self.wider = bindings,
            Direction::Narrower => self.narrower = bindings,
        }
        true
    }

    /// What `notation` means, given the chord prefix `pending` that the
    /// previous keystroke left waiting, or `None` for a key these bindings
    /// say nothing about.
    #[must_use]
    pub fn resolve(&self, pending: Option<&str>, notation: &str) -> Option<Resolved> {
        for (direction, bindings) in [
            (Direction::Wider, &self.wider),
            (Direction::Narrower, &self.narrower),
        ] {
            let hit = bindings
                .iter()
                .any(|(first, second)| match (pending, second) {
                    (Some(prefix), Some(follower)) => prefix == first && follower == notation,
                    (None, None) => first == notation,
                    _ => false,
                });
            if hit {
                return Some(Resolved::Step(direction));
            }
        }
        // A follower that finishes no chord is handled as if it had been
        // typed alone, and these bindings are no exception to that: without
        // this, the four keys the whole set exists for would be the one
        // class a waiting prefix locks out, and a doubled first key (nvim
        // muscle memory taps `<C-w>` twice) would drop the chord instead of
        // re-arming it.
        if pending.is_some() {
            return self.resolve(None, notation);
        }
        // Strictly after every completed binding above: a key bound both on
        // its own and as some chord's first press steps the sidebar rather
        // than waiting for a second key that may never be typed.
        self.wider
            .iter()
            .chain(&self.narrower)
            .any(|(first, second)| second.is_some() && first == notation)
            .then_some(Resolved::Pending)
    }
}

/// The keys `view-tui`'s `encode_key` spells with a name rather than with
/// the character itself, plus the `lt` it escapes a literal `<` as.
///
/// Restated here rather than shared: `view-core` cannot depend on
/// `view-tui` (`scripts/audit-deps.sh`), and the cost of not stating it is
/// what [`well_formed`] exists to stop. `view-tui`'s own tests run every
/// notation its encoder emits back through [`ResizeKeys::rebind`], so a key
/// this list forgets fails there rather than in a user's config.
const NAMED_KEYS: [&str; 16] = [
    "lt", "BS", "CR", "Esc", "Tab", "Up", "Down", "Left", "Right", "Home", "End", "PageUp",
    "PageDown", "Del", "Insert", "Space",
];

/// The modifier prefixes a notation may open with, in any combination.
const MODIFIERS: [&str; 4] = ["S-", "C-", "M-", "A-"];

/// Whether `key` is a notation this build could ever be handed.
///
/// Shape only, never vocabulary: an unbracketed key is one character and is
/// always well formed, and a `<...>` one is modifier prefixes followed by
/// either a single character or a named key. What this refuses is the
/// spelling that cannot be a key at all -- `<S-right>` for `<S-Right>`,
/// `<Ctrl-w>` for `<C-w>` -- because such a spelling is not merely inert:
/// it replaces the direction's defaults with a key nothing will ever send,
/// leaving the user with no way to resize at all and nothing on screen
/// saying why.
fn well_formed(key: &str) -> bool {
    let Some(inner) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>')) else {
        return true;
    };
    let mut rest = inner;
    while let Some(shorter) = MODIFIERS.iter().find_map(|m| rest.strip_prefix(m)) {
        rest = shorter;
    }
    let function_key = rest
        .strip_prefix('F')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    rest.chars().count() == 1 || NAMED_KEYS.contains(&rest) || function_key
}

/// The one or two keys `spelling` names, or `None` when it names none at
/// all, more than a chord's two, or one this build could never be handed
/// ([`well_formed`]).
///
/// A bare `<` is read as the `<lt>` the encoder emits for it, so the chord
/// a user writes the way nvim documents it (`<C-w><`) resolves to the same
/// binding as its fully spelled form.
fn split_keys(spelling: &str) -> Option<Binding> {
    let mut keys: Vec<String> = Vec::new();
    let mut rest = spelling;
    while let Some(head) = rest.chars().next() {
        if keys.len() == 2 {
            return None;
        }
        match (head == '<').then(|| rest.find('>')).flatten() {
            Some(end) => {
                keys.push(rest[..=end].to_string());
                rest = &rest[end + 1..];
            }
            None => {
                keys.push(if head == '<' {
                    "<lt>".to_string()
                } else {
                    head.to_string()
                });
                rest = &rest[head.len_utf8()..];
            }
        }
    }
    if !keys.iter().all(|key| well_formed(key)) {
        return None;
    }
    let mut keys = keys.into_iter();
    let first = keys.next()?;
    Some((first, keys.next()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_defaults_are_the_shifted_arrows_and_nvims_own_window_chord() {
        let keys = ResizeKeys::default();
        assert_eq!(
            keys.resolve(None, "<S-Right>"),
            Some(Resolved::Step(Direction::Wider))
        );
        assert_eq!(
            keys.resolve(None, "<S-Left>"),
            Some(Resolved::Step(Direction::Narrower))
        );
        assert_eq!(keys.resolve(None, "<C-w>"), Some(Resolved::Pending));
        assert_eq!(
            keys.resolve(Some("<C-w>"), ">"),
            Some(Resolved::Step(Direction::Wider))
        );
        assert_eq!(
            keys.resolve(Some("<C-w>"), "<lt>"),
            Some(Resolved::Step(Direction::Narrower))
        );
    }

    #[test]
    fn a_key_no_binding_names_resolves_to_nothing() {
        let keys = ResizeKeys::default();
        assert_eq!(keys.resolve(None, "<Up>"), None);
        assert_eq!(
            keys.resolve(None, ">"),
            None,
            "the chord's second key alone"
        );
        assert_eq!(keys.resolve(Some("<C-w>"), "x"), None);
    }

    #[test]
    fn rebinding_one_direction_leaves_the_other_at_its_defaults() {
        let mut keys = ResizeKeys::default();
        assert!(keys.rebind(Direction::Wider, &["<M-.>".to_string()]));
        assert_eq!(
            keys.resolve(None, "<M-.>"),
            Some(Resolved::Step(Direction::Wider))
        );
        assert_eq!(keys.resolve(None, "<S-Right>"), None, "replaced, not added");
        assert_eq!(
            keys.resolve(None, "<S-Left>"),
            Some(Resolved::Step(Direction::Narrower)),
            "the direction nobody rebound is untouched"
        );
    }

    #[test]
    fn a_spelling_this_build_cannot_match_keeps_the_defaults() {
        let mut keys = ResizeKeys::default();
        assert!(!keys.rebind(
            Direction::Wider,
            &["<S-Right>".to_string(), "<C-w>abc".to_string()]
        ));
        assert_eq!(
            keys.resolve(None, "<S-Right>"),
            Some(Resolved::Step(Direction::Wider)),
            "a refused list applies none of itself"
        );
        assert!(!keys.rebind(Direction::Narrower, &[String::new()]));
        assert_eq!(
            keys.resolve(None, "<S-Left>"),
            Some(Resolved::Step(Direction::Narrower))
        );
    }

    #[test]
    fn an_empty_list_leaves_a_direction_on_no_key() {
        let mut keys = ResizeKeys::default();
        assert!(keys.rebind(Direction::Narrower, &[]));
        assert_eq!(keys.resolve(None, "<S-Left>"), None);
        assert_eq!(keys.resolve(Some("<C-w>"), "<lt>"), None);
        assert_eq!(
            keys.resolve(None, "<C-w>"),
            Some(Resolved::Pending),
            "the widening chord still opens on the same key"
        );
    }

    #[test]
    fn a_follower_that_finishes_no_chord_is_read_as_if_it_stood_alone() {
        let keys = ResizeKeys::default();
        assert_eq!(
            keys.resolve(Some("<C-w>"), "<S-Right>"),
            Some(Resolved::Step(Direction::Wider)),
            "the single-key binding is not locked out by a waiting prefix"
        );
        assert_eq!(
            keys.resolve(Some("<C-w>"), "<C-w>"),
            Some(Resolved::Pending),
            "and a doubled first key re-arms rather than dropping the chord"
        );
    }

    #[test]
    fn a_single_key_binding_outranks_the_chord_spelled_on_the_same_key() {
        let mut keys = ResizeKeys::default();
        assert!(keys.rebind(Direction::Narrower, &["<C-w>".to_string()]));
        assert_eq!(
            keys.resolve(None, "<C-w>"),
            Some(Resolved::Step(Direction::Narrower)),
            "a key that would otherwise wait for a second press"
        );
    }

    #[test]
    fn a_bare_left_angle_is_the_same_key_as_its_spelled_form() {
        let mut spelled = ResizeKeys::default();
        assert!(spelled.rebind(Direction::Narrower, &["<C-w><lt>".to_string()]));
        let mut bare = ResizeKeys::default();
        assert!(bare.rebind(Direction::Narrower, &["<C-w><".to_string()]));
        assert_eq!(spelled, bare);
    }

    /// The typo that costs the most: a well-formed *looking* notation with
    /// the wrong case is a key nothing sends, and taken at face value it
    /// takes the direction's defaults with it. Refused, the notice fires
    /// and the defaults stand.
    #[test]
    fn a_notation_that_cannot_be_a_key_keeps_the_defaults_and_is_reported() {
        for typo in [
            "<S-right>",
            "<Ctrl-w>",
            "<C-Rihgt>",
            "<>",
            "<S->",
            "<leader>x",
        ] {
            let mut keys = ResizeKeys::default();
            assert!(
                !keys.rebind(Direction::Wider, &[typo.to_string()]),
                "{typo} cannot be a key this build is handed"
            );
            assert_eq!(
                keys.resolve(None, "<S-Right>"),
                Some(Resolved::Step(Direction::Wider)),
                "a refused spelling leaves {typo}'s direction on its defaults"
            );
        }
    }

    /// The other half: a shape check that refused real keys would be worse
    /// than none at all.
    #[test]
    fn every_shape_a_key_really_takes_is_accepted() {
        for spelling in [
            "<S-Right>",
            "<C-w>",
            "<M-.>",
            "<A-x>",
            "<C-S-Left>",
            "<lt>",
            "<CR>",
            "<Esc>",
            "<Space>",
            "<F12>",
            "<PageDown>",
            "g",
            ">",
        ] {
            let mut keys = ResizeKeys::default();
            assert!(
                keys.rebind(Direction::Wider, &[spelling.to_string()]),
                "{spelling} is a key this build can be handed"
            );
        }
    }

    #[test]
    fn a_chord_is_two_keys_and_no_more() {
        assert_eq!(
            split_keys("<C-w>>"),
            Some(("<C-w>".to_string(), Some(">".to_string())))
        );
        assert_eq!(split_keys("g"), Some(("g".to_string(), None)));
        assert_eq!(split_keys("ggg"), None);
        assert_eq!(split_keys(""), None);
    }
}
