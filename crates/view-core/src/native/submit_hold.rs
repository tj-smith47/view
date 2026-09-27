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
    /// Every key sequence nvim runs a view invocation on.
    invoke_keys: Vec<Invocation>,
    /// The latest keys sent to nvim in normal mode, as many as the longest
    /// of `invoke_keys`, each with whether nvim read it as the argument of
    /// the key before it.
    recent: Vec<(String, bool)>,
    /// Whether nvim reads the next normal-mode key as an argument: the key
    /// before it takes one, and was not itself an argument.
    argument_next: bool,
    /// Keys a surface of view's own is holding while they spell the start
    /// of an invoking sequence.
    sequence: Vec<String>,
}

/// One key sequence nvim runs a view invocation on.
#[derive(Debug, Clone)]
struct Invocation {
    feature: String,
    /// One [`canonical`] key per entry.
    keys: Vec<String>,
}

/// Where a run of keys stands against the invoking sequences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sequence {
    /// The keys are a whole sequence, invoking this feature.
    Complete(String),
    /// The keys begin a longer sequence.
    Prefix,
    /// The keys begin no sequence.
    Neither,
}

impl SubmitHold {
    /// Learns the keys nvim runs a view invocation on from the claims the
    /// registration answered with, the default keys and the desktop
    /// chords alike.
    pub fn learn_invoke_keys(&mut self, claims: &[crate::native::mappings::MappingClaim]) {
        self.invoke_keys = claims
            .iter()
            .filter_map(|claim| {
                let keys: Vec<_> = split_keys(claim.keys.as_deref()?).map(canonical).collect();
                (!keys.is_empty()).then(|| Invocation {
                    feature: claim.feature.clone(),
                    keys,
                })
            })
            .collect();
        self.recent.clear();
        self.sequence.clear();
    }

    /// The feature `notation` alone invokes, when it is a chord nvim maps
    /// to one of view's own verbs. Only a key carrying a modifier counts,
    /// so a text key typed into a composer stays text whatever a config
    /// binds it to.
    #[must_use]
    pub fn invokes(&self, notation: &str) -> Option<&str> {
        let key = canonical(notation);
        if !(key.starts_with('<') && key.contains('-')) {
            return None;
        }
        self.invoke_keys
            .iter()
            .find(|invocation| matches!(invocation.keys.as_slice(), [only] if *only == key))
            .map(|invocation| invocation.feature.as_str())
    }

    /// Where `keys`, typed in this order, stand against the invoking
    /// sequences. A whole sequence wins over a longer one it begins: nvim
    /// is handed the keys, and its own `'timeout'` decides between the two.
    #[must_use]
    pub fn sequence(&self, keys: &[String]) -> Sequence {
        let typed: Vec<String> = keys.iter().map(|key| canonical(key)).collect();
        if let Some(whole) = self
            .invoke_keys
            .iter()
            .find(|invocation| invocation.keys == typed)
        {
            return Sequence::Complete(whole.feature.clone());
        }
        if self
            .invoke_keys
            .iter()
            .any(|invocation| invocation.keys.starts_with(&typed))
        {
            return Sequence::Prefix;
        }
        Sequence::Neither
    }

    /// Hands back the keys a surface is holding, leaving none held.
    pub fn take_sequence(&mut self) -> Vec<String> {
        std::mem::take(&mut self.sequence)
    }

    /// Holds `keys` as the start of an invoking sequence.
    pub fn keep_sequence(&mut self, keys: Vec<String>) {
        self.sequence = keys;
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
/// Inside a sequence nvim matches the mapping before it reads a key as an
/// argument, so the `a` of `\ai` still completes it. The first key is the
/// exception: typed as the argument of `f`, `r` or `"`, nvim reads it
/// literally and starts no mapping, so `f<Space>e` is a jump and then a
/// motion. The argument is read off the keys this fold has seen, because
/// the engine's own `literal_pending` is written only once a key is sent,
/// after every key one update folds.
fn completes_invoke(model: &mut Model, notation: &str) -> bool {
    let normal = model.engine.mode.current == "normal";
    let hold = &mut model.submit_hold;
    let argument = std::mem::take(&mut hold.argument_next);
    let longest = hold
        .invoke_keys
        .iter()
        .map(|invocation| invocation.keys.len())
        .max()
        .unwrap_or(0);
    if !normal || longest == 0 {
        hold.recent.clear();
        return false;
    }
    hold.argument_next =
        !argument && crate::native::speculate::CMDLINE_LITERAL_KEYS.contains(&notation);
    if hold.recent.len() >= longest {
        hold.recent.remove(0);
    }
    hold.recent.push((canonical(notation), argument));
    let recent = &hold.recent;
    let complete = hold.invoke_keys.iter().any(|invocation| {
        let keys = &invocation.keys;
        recent.len() >= keys.len() && {
            let tail = &recent[recent.len() - keys.len()..];
            tail.first().is_some_and(|(_, argument)| !argument)
                && tail.iter().map(|(key, _)| key).eq(keys.iter())
        }
    });
    if complete {
        hold.recent.clear();
        hold.argument_next = false;
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
            .map(|keys| {
                crate::native::mappings::MappingClaim::new("picker", "<leader>ff", false)
                    .with_keys(keys.map(str::to_string))
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

    /// A first key typed as the argument of `f` or `r` starts no mapping in
    /// nvim, so `f<Space>e` is a jump and then a motion: the keys behind it
    /// go straight on. The same keys typed as a command still hold, and so
    /// does a sequence whose later key is one that takes an argument.
    #[test]
    fn a_first_key_typed_as_an_argument_holds_nothing() {
        for jump in ["f", "r", "t", "\""] {
            let mut model = claiming(&[Some("<Space>e")]);
            let sent = type_keys(&mut model, &[jump, " ", "e", "w", "o"]);
            assert_eq!(inputs(&sent), [jump, " ", "e", "w", "o"], "{sent:?}");
            assert!(!model.submit_hold.is_holding(), "{jump}<Space>e armed");
        }

        let mut model = claiming(&[Some("<Space>e")]);
        let _ = type_keys(&mut model, &["f", "x", " ", "e"]);
        assert!(model.submit_hold.is_holding(), "fx then <Space>e");

        let mut model = claiming(&[Some("<Space>ai")]);
        let _ = type_keys(&mut model, &[" ", "a", "i"]);
        assert!(model.submit_hold.is_holding(), "<Space>ai");

        let mut model = claiming(&[Some("<Space>ai"), Some("<Space>e")]);
        let _ = type_keys(&mut model, &[" ", "a", "i"]);
        let generation = model.submit_hold.generation;
        let _ = crate::update::update(&mut model, Msg::SubmitHoldExpired { generation });
        let _ = type_keys(&mut model, &[" ", "e"]);
        assert!(
            model.submit_hold.is_holding(),
            "the `a` of the invocation before left the next key an argument"
        );
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
