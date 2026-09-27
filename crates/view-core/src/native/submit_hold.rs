//! Input held behind a submitted `:View` command line until view has run
//! the command.
//!
//! nvim runs `:View ai open` and only then tells view about it, while the
//! keys typed behind the `<CR>` are already on their way. Routed as they
//! arrive, they reach nvim as normal-mode commands in the buffer the panel
//! was opened from. Holding them until the command's notification comes
//! back lets the focus that command sets decide where they go.

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
/// `:View`.
#[derive(Debug, Clone, Default)]
pub struct SubmitHold {
    typed: Option<Typed>,
    held: Option<Vec<Msg>>,
    generation: u64,
}

impl SubmitHold {
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
/// arms the hold when the key submits a line that runs `:View`.
///
/// Called before the key is sent, so the model still describes the editor
/// the key arrives at.
pub fn fold_engine_key(model: &mut Model, notation: &str) -> Vec<Effect> {
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

    #[test]
    fn single_key_notations_type_their_character() {
        assert_eq!(typed_char("a"), Some('a'));
        assert_eq!(typed_char("<Space>"), Some(' '));
        assert_eq!(typed_char("<Tab>"), None);
    }
}
