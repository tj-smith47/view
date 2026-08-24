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
        // Strictly after every completed binding above: a key bound both on
        // its own and as some chord's first press steps the sidebar rather
        // than waiting for a second key that may never be typed.
        let opens_a_chord = self
            .wider
            .iter()
            .chain(&self.narrower)
            .any(|(first, second)| second.is_some() && first == notation);
        (pending.is_none() && opens_a_chord).then_some(Resolved::Pending)
    }
}

/// The one or two keys `spelling` names, or `None` when it names none at
/// all or more than a chord's two.
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
