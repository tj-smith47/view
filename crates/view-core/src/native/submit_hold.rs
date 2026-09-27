//! Input held behind a submitted `:View` command line, or a key nvim maps
//! to a view invocation, until view has run the invocation.
//!
//! nvim runs `:View ai open` or `<leader>ai` and only then tells view
//! about it, while the keys typed behind them are already on their way.
//! Routed as they arrive, they reach nvim as normal-mode commands in the
//! buffer the panel was opened from. Holding them until the invocation's
//! notification comes back lets the focus it sets decide where they go.

use crate::model::Model;
use crate::msg::{Effect, Msg};

/// The longest a tracked command line grows before it is given up on: a
/// `:` typed in insert mode is text, and the tracker would otherwise keep
/// every character after it.
const TRACKED_MAX: usize = 256;

/// What view knows of a `:` command line it has sent the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Typed {
    /// Every key since the `:` was one whose edit is known.
    Known(String),
    /// A key since the `:` edited the line in a way view does not model
    /// (a completion, a history recall). The engine's own `cmdline_show`
    /// is read at the `<CR>` in its place.
    Unknown,
}

/// The command line being typed and the input held behind a submitted
/// `:View` or a key that invokes view.
#[derive(Debug, Clone, Default)]
pub struct SubmitHold {
    typed: Option<Typed>,
    held: Option<Vec<Msg>>,
    generation: u64,
    /// Every key sequence nvim runs a view invocation on, one
    /// [`canonical`] key per entry.
    invoke_keys: Vec<Vec<String>>,
    /// The latest keys sent to nvim in normal mode, as many as the longest
    /// of `invoke_keys`.
    recent: Vec<String>,
}

impl SubmitHold {
    /// Learns the keys nvim runs a view invocation on from the claims the
    /// registration answered with, the default keys and the desktop
    /// chords alike.
    pub fn learn_invoke_keys(&mut self, claims: &[crate::native::mappings::MappingClaim]) {
        self.invoke_keys = claims
            .iter()
            .filter_map(|claim| claim.keys.as_deref())
            .map(|keys| split_keys(keys).map(canonical).collect::<Vec<_>>())
            .filter(|keys| !keys.is_empty())
            .collect();
        self.recent.clear();
    }

    /// Whether `notation` alone is a chord nvim maps to one of view's own
    /// verbs. Only a key carrying a modifier counts, so a text key typed
    /// into a composer stays text whatever a config binds it to.
    #[must_use]
    pub fn invokes(&self, notation: &str) -> bool {
        let key = canonical(notation);
        key.starts_with('<')
            && key.contains('-')
            && self
                .invoke_keys
                .iter()
                .any(|keys| matches!(keys.as_slice(), [only] if *only == key))
    }

    /// Whether `msg` ends a standing hold: the command's own notification,
    /// or the bound this hold armed.
    #[must_use]
    pub fn releases(&self, msg: &Msg) -> bool {
        self.held.is_some()
            && match msg {
                Msg::FeatureInvoke { .. } => true,
                Msg::SubmitHoldExpired { generation } => *generation == self.generation,
                _ => false,
            }
    }

    /// Keeps `msg` when a hold stands and it is input, handing it back
    /// otherwise.
    pub fn hold(&mut self, msg: Msg) -> Option<Msg> {
        match (&mut self.held, &msg) {
            (Some(held), Msg::Key(_) | Msg::Mouse(_) | Msg::Paste(_)) => {
                held.push(msg);
                None
            }
            _ => Some(msg),
        }
    }

    /// Ends the hold, handing back what it kept in the order it arrived.
    pub fn take_held(&mut self) -> Vec<Msg> {
        self.held.take().unwrap_or_default()
    }

    /// Whether input is being held.
    #[must_use]
    pub fn is_holding(&self) -> bool {
        self.held.is_some()
    }
}

/// Folds one key going to the engine into the tracked command line, and
/// arms the hold when the key submits a line that runs `:View` or
/// completes a key nvim maps to a view invocation.
///
/// Called before the key is sent, so the model still describes the editor
/// the key arrives at.
pub fn fold_engine_key(model: &mut Model, notation: &str) -> Vec<Effect> {
    if completes_invoke(model, notation) {
        model.submit_hold.typed = None;
        return arm(model);
    }
    fold_line(model, notation)
}

/// Whether `notation` completes one of the invoking keys, typed in normal
/// mode, where those keys are mapped.
///
/// A key that takes the next one as its argument (`f`, the `a` of `\ai`)
/// is no reason to stop: nvim matches a mapping before it reads a key as
/// an argument.
fn completes_invoke(model: &mut Model, notation: &str) -> bool {
    let normal = model.engine.mode.current == "normal";
    let hold = &mut model.submit_hold;
    let longest = hold.invoke_keys.iter().map(Vec::len).max().unwrap_or(0);
    if !normal || longest == 0 {
        hold.recent.clear();
        return false;
    }
    if hold.recent.len() >= longest {
        hold.recent.remove(0);
    }
    hold.recent.push(canonical(notation));
    let complete = hold
        .invoke_keys
        .iter()
        .any(|keys| hold.recent.ends_with(keys));
    if complete {
        hold.recent.clear();
    }
    complete
}

/// The keys of `spelling`, one `<...>` token or one character each.
fn split_keys(spelling: &str) -> impl Iterator<Item = &str> {
    let mut rest = spelling;
    std::iter::from_fn(move || {
        let first = rest.chars().next()?;
        let token = rest
            .find('>')
            .map(|end| &rest[..=end])
            .filter(|token| first == '<' && crate::native::keys::well_formed(token))
            .unwrap_or(&rest[..first.len_utf8()]);
        rest = &rest[token.len()..];
        Some(token)
    })
}

/// One spelling for each key nvim reads as the same key: `keytrans()`
/// writes `<M-S-Left>` and `<Space>` where view's input writes
/// `<S-M-Left>` and a bare space, and a shifted letter is its capital.
fn canonical(key: &str) -> String {
    if let Some(c) = typed_char(key) {
        return c.to_string();
    }
    let Some(mut rest) = key.strip_prefix('<').and_then(|k| k.strip_suffix('>')) else {
        return key.to_string();
    };
    let [mut ctrl, mut shift, mut meta, mut sup] = [false; 4];
    while rest.len() > 2 && rest.as_bytes()[1] == b'-' {
        match rest.as_bytes()[0].to_ascii_uppercase() {
            b'C' => ctrl = true,
            b'S' => shift = true,
            b'M' | b'A' => meta = true,
            b'D' => sup = true,
            _ => break,
        }
        rest = &rest[2..];
    }
    let mut chars = rest.chars();
    let name = match (chars.next(), chars.next()) {
        // a Ctrl letter is one key in either case
        (Some(c), None) if c.is_alphabetic() => {
            let capital = (shift || c.is_uppercase()) && !ctrl;
            shift = false;
            if capital {
                c.to_uppercase().collect()
            } else {
                c.to_lowercase().collect()
            }
        }
        (Some(_), None) => rest.to_string(),
        _ => rest.to_ascii_lowercase(),
    };
    let modifiers: String = [(ctrl, "C-"), (shift, "S-"), (meta, "M-"), (sup, "D-")]
        .into_iter()
        .filter_map(|(on, spelled)| on.then_some(spelled))
        .collect();
    format!("<{modifiers}{name}>")
}

/// The command-line half of [`fold_engine_key`].
fn fold_line(model: &mut Model, notation: &str) -> Vec<Effect> {
    let hold = &mut model.submit_hold;
    // an Escape typed quickly before its key arrives as one Meta key, which
    // nvim runs as `<Esc>` and then the key: `<M-:>` opens a command line
    // from any mode, and any other one leaves the line being typed
    if let Some(key) = crate::native::keys::escaped_key(notation) {
        hold.typed = (key == ":").then(|| Typed::Known(String::new()));
        return Vec::new();
    }
    let Some(typed) = hold.typed.as_mut() else {
        if notation == ":" && may_open(model) {
            model.submit_hold.typed = Some(Typed::Known(String::new()));
        }
        return Vec::new();
    };
    match notation {
        "<Esc>" | "<C-c>" | "<C-[>" => hold.typed = None,
        "<CR>" | "<NL>" | "<C-m>" | "<C-j>" | "<kEnter>" => {
            let typed = hold.typed.take();
            if submits_view(model, typed.as_ref()) {
                return arm(model);
            }
        }
        "<BS>" | "<C-h>" => {
            if let Typed::Known(text) = typed {
                // a backspace on an empty line leaves the command line
                if text.pop().is_none() {
                    hold.typed = None;
                }
            }
        }
        _ => {
            if let Typed::Known(text) = typed {
                match typed_char(notation) {
                    Some(c) if text.len() < TRACKED_MAX => text.push(c),
                    _ => *typed = Typed::Unknown,
                }
            }
        }
    }
    Vec::new()
}

/// Whether a `:` reaching the engine now can open a command line: the
/// last mode nvim reported reads one of the modes `:` does that in, or a
/// key still in flight may have put it there. The literal-taking keys
/// read the `:` as their argument.
fn may_open(model: &Model) -> bool {
    !model.engine.literal_pending
        && (model.engine.key_unanswered.is_some()
            || crate::native::speculate::CMDLINE_GATE_MODES
                .contains(&model.engine.mode.current.as_str()))
}

/// The character a single-key notation types into a command line.
fn typed_char(notation: &str) -> Option<char> {
    let mut chars = notation.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => match notation {
            "<Space>" => Some(' '),
            "<lt>" => Some('<'),
            "<Bslash>" => Some('\\'),
            "<Bar>" => Some('|'),
            _ => None,
        },
    }
}

/// Whether the line a `<CR>` submits runs `:View`, read from the keys view
/// sent, or from the engine's last `cmdline_show` where those keys edited
/// the line in a way view does not model.
fn submits_view(model: &Model, typed: Option<&Typed>) -> bool {
    if let Some(Typed::Known(text)) = typed {
        return names_view(text);
    }
    model.engine.cmdline.as_ref().is_some_and(|line| {
        line.firstc == ":"
            && names_view(
                &line
                    .content
                    .iter()
                    .map(|(_, s)| s.as_str())
                    .collect::<String>(),
            )
    })
}

/// Whether `line`'s command word is `View` or an abbreviation nvim would
/// run as it.
fn names_view(line: &str) -> bool {
    let line = line.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
    let word = line
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default();
    word.starts_with('V') && "View".starts_with(word)
}

/// Starts a hold, bounded by the link's own backstop so a command that
/// never reports back releases the keys to wherever focus stands.
fn arm(model: &mut Model) -> Vec<Effect> {
    let hold = &mut model.submit_hold;
    hold.generation = hold.generation.wrapping_add(1);
    hold.held = Some(Vec::new());
    vec![Effect::ScheduleSubmitHold {
        after: crate::native::speculate::cmdline_backstop(model),
        generation: model.submit_hold.generation,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_word_is_read_with_its_abbreviations() {
        for line in ["View ai open", "Vie ai", "  :View", "View!", "V"] {
            assert!(names_view(line), "{line:?}");
        }
        for line in ["", "vim", "Vex", "Views", "set ft=View", "edit View"] {
            assert!(!names_view(line), "{line:?}");
        }
    }

    fn key(notation: &str) -> Msg {
        Msg::Key(crate::msg::Key {
            notation: notation.to_string(),
        })
    }

    fn normal_mode() -> Model {
        let mut model = Model::new();
        model.engine.mode.current = "normal".to_string();
        model
    }

    fn type_keys(model: &mut Model, keys: &[&str]) -> Vec<Effect> {
        keys.iter()
            .flat_map(|k| crate::update::update(model, key(k)))
            .collect()
    }

    fn inputs(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Rpc(crate::msg::RpcCall::Input { notation }) => Some(notation.clone()),
                _ => None,
            })
            .collect()
    }

    const OPEN_PICKER: [&str; 20] = [
        "<Esc>", ":", "V", "i", "e", "w", "<Space>", "p", "i", "c", "k", "e", "r", "<Space>", "f",
        "i", "l", "e", "s", "<CR>",
    ];

    /// Keys behind the `<CR>` wait for the command's notification, and the
    /// picker it opened is what they are typed into.
    #[test]
    fn keys_behind_a_submitted_view_command_reach_what_it_opened() {
        let mut model = normal_mode();
        let sent = type_keys(&mut model, &OPEN_PICKER);
        assert_eq!(inputs(&sent).len(), OPEN_PICKER.len(), "{sent:?}");
        assert!(
            sent.iter()
                .any(|e| matches!(e, Effect::ScheduleSubmitHold { .. })),
            "{sent:?}"
        );
        let held = type_keys(&mut model, &["m", "a"]);
        assert!(held.is_empty(), "held keys produce nothing: {held:?}");

        let replayed = crate::update::update(
            &mut model,
            Msg::FeatureInvoke {
                feature: "picker".to_string(),
                verb: "files".to_string(),
            },
        );
        assert!(inputs(&replayed).is_empty(), "{replayed:?}");
        assert!(
            replayed
                .iter()
                .any(|e| matches!(e, Effect::PickerQuery { needle, .. } if needle == "ma")),
            "the held keys filter the picker: {replayed:?}"
        );
    }

    /// `<Esc>:` typed quickly arrives as one Meta key, from insert mode as
    /// readily as from normal mode.
    #[test]
    fn a_meta_colon_opens_the_line_it_tracks() {
        let mut model = normal_mode();
        model.engine.mode.current = "insert".to_string();
        let _ = type_keys(&mut model, &["<M-:>", "V", "i", "e", "w", "<CR>"]);
        assert!(model.submit_hold.is_holding());
    }

    /// Any other command line holds nothing.
    #[test]
    fn keys_behind_another_command_go_straight_on() {
        let mut model = normal_mode();
        let sent = type_keys(&mut model, &[":", "e", "d", "i", "t", "<CR>", "j"]);
        assert_eq!(inputs(&sent).len(), 7, "{sent:?}");
        assert!(!model.submit_hold.is_holding());
    }

    /// A `:View` that never reports back releases its keys to the engine at
    /// its own bound, and a bound armed for an earlier hold releases
    /// nothing.
    #[test]
    fn the_bound_releases_the_keys_to_where_focus_stands() {
        let mut model = normal_mode();
        let _ = type_keys(&mut model, &[":", "V", "i", "e", "w", "<CR>", "j"]);
        let generation = model.submit_hold.generation;
        let stale = crate::update::update(
            &mut model,
            Msg::SubmitHoldExpired {
                generation: generation.wrapping_sub(1),
            },
        );
        assert!(stale.is_empty() && model.submit_hold.is_holding());
        let released = crate::update::update(&mut model, Msg::SubmitHoldExpired { generation });
        assert_eq!(inputs(&released), ["j"]);
        assert!(!model.submit_hold.is_holding());
    }

    /// A model that learned `keys` as the invoking keys nvim maps.
    fn claiming(keys: &[Option<&str>]) -> Model {
        let mut model = normal_mode();
        let claims: Vec<_> = keys
            .iter()
            .map(|keys| crate::native::mappings::MappingClaim {
                feature: "picker".to_string(),
                lhs: "<leader>ff".to_string(),
                had_user_mapping: false,
                keys: keys.map(str::to_string),
            })
            .collect();
        model.submit_hold.learn_invoke_keys(&claims);
        model
    }

    fn picker_files() -> Msg {
        Msg::FeatureInvoke {
            feature: "picker".to_string(),
            verb: "files".to_string(),
        }
    }

    /// A default key with the leader resolved holds what is typed behind
    /// it, and the picker it opens is what those keys filter.
    #[test]
    fn keys_behind_a_leader_map_reach_what_it_opened() {
        let mut model = claiming(&[Some("<Space>ff")]);
        let sent = type_keys(&mut model, &[" ", "f", "f"]);
        assert_eq!(inputs(&sent), [" ", "f", "f"], "{sent:?}");
        assert!(model.submit_hold.is_holding(), "{sent:?}");
        let held = type_keys(&mut model, &["m", "a"]);
        assert!(held.is_empty(), "held keys produce nothing: {held:?}");
        let replayed = crate::update::update(&mut model, picker_files());
        assert!(
            replayed
                .iter()
                .any(|e| matches!(e, Effect::PickerQuery { needle, .. } if needle == "ma")),
            "the held keys filter the picker: {replayed:?}"
        );
    }

    /// A desktop chord holds the same way, whichever order the modifiers
    /// are spelled in.
    #[test]
    fn keys_behind_a_desktop_chord_are_held() {
        let mut model = claiming(&[Some("<M-S-Left>"), Some("<D-f>")]);
        let _ = type_keys(&mut model, &["<S-M-Left>"]);
        assert!(model.submit_hold.is_holding());

        let mut model = claiming(&[Some("<M-S-Left>"), Some("<D-f>")]);
        let _ = type_keys(&mut model, &["<D-f>"]);
        assert!(model.submit_hold.is_holding());
    }

    /// A chord sending nvim keys of its own, the same keys typed in insert
    /// mode, and a sequence broken by another key all go straight on.
    #[test]
    fn keys_that_invoke_nothing_hold_nothing() {
        let mut model = claiming(&[None, Some("\\ai")]);
        let sent = type_keys(&mut model, &["\\", "x", "a", "i", "j"]);
        assert_eq!(inputs(&sent).len(), 5, "{sent:?}");
        assert!(!model.submit_hold.is_holding());

        let mut model = claiming(&[Some("\\ai")]);
        model.engine.mode.current = "insert".to_string();
        let _ = type_keys(&mut model, &["<Bslash>", "a", "i"]);
        assert!(!model.submit_hold.is_holding());

        let mut model = claiming(&[Some("\\ai")]);
        let _ = type_keys(&mut model, &["<Bslash>", "a", "i"]);
        assert!(model.submit_hold.is_holding(), "`<Bslash>` is `\\`");
    }

    #[test]
    fn a_key_has_one_spelling_however_it_is_written() {
        for (a, b) in [
            ("<S-M-Left>", "<M-S-Left>"),
            ("<M-T>", "<M-S-t>"),
            ("<C-W>", "<C-w>"),
            ("<Space>", " "),
            ("<CR>", "<cr>"),
            ("<A-x>", "<M-x>"),
        ] {
            assert_eq!(canonical(a), canonical(b), "{a} {b}");
        }
        assert_ne!(canonical("<M-q>"), canonical("<M-Q>"));
        assert_eq!(
            split_keys("<Space>a<lt>\\<M-S-Left>").collect::<Vec<_>>(),
            ["<Space>", "a", "<lt>", "\\", "<M-S-Left>"]
        );
    }

    #[test]
    fn single_key_notations_type_their_character() {
        assert_eq!(typed_char("a"), Some('a'));
        assert_eq!(typed_char("<Space>"), Some(' '));
        assert_eq!(typed_char("<Tab>"), None);
    }
}
