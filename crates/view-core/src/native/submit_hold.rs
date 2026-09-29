//! Input held behind a submitted `:View` command line, or a key nvim maps
//! to a view invocation, until view has run the invocation.
//!
//! nvim runs `:View ai open` or `<leader>ai` and only then tells view
//! about it, while the keys typed behind them are already on their way.
//! Routed as they arrive, they reach nvim as normal-mode commands in the
//! buffer the panel was opened from. Holding them until the invocation's
//! notification comes back lets the focus it sets decide where they go.

use std::time::Duration;

use crate::events::UiEvent;
use crate::model::{CmdlineState, Model};
use crate::msg::{Effect, Msg};
use crate::native::keys::{key_tokens, notation_char};
use crate::native::speculate::SpecStamp;

/// The longest a tracked command line grows before it is given up on: a
/// `:` typed in insert mode is text, and the tracker would otherwise keep
/// every character after it.
const TRACKED_MAX: usize = 256;

/// Normal-mode keys that leave normal mode: into insert, replace, visual
/// or a command line, or into an operator's pending motion. A key typed
/// behind one of them is no normal-mode command, so it starts no mapping.
/// `Q` replays the last recorded register and stays in normal mode.
const LEAVES_NORMAL: [&str; 25] = [
    ":", "/", "?", "o", "O", "a", "A", "i", "I", "s", "S", "C", "R", "c", "d", "y", "<", ">", "=",
    "!", "v", "V", "<C-v>", "<C-q>", "<Insert>",
];

/// The keys that leave normal mode as the argument of the key before
/// them: `gi`, `gv` and `gn` enter insert and visual, and the `g` and `z`
/// operators (nvim's default `gc` among them) wait for a motion.
/// A `gn` or `gN` with no search pattern stays in normal mode, so no mode
/// report clears the doubt it raised, and a view key typed after it is
/// not recognised until the next mode change.
const LEAVES_NORMAL_AFTER: [(&str, &str); 19] = [
    ("g", "n"),
    ("g", "N"),
    ("g", "c"),
    ("g", "i"),
    ("g", "I"),
    ("g", "v"),
    ("g", "h"),
    ("g", "H"),
    ("g", "<C-h>"),
    ("g", "R"),
    ("g", "Q"),
    ("g", "u"),
    ("g", "U"),
    ("g", "~"),
    ("g", "?"),
    ("g", "q"),
    ("g", "w"),
    ("g", "@"),
    ("z", "f"),
];

/// What armed a standing hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Armed {
    /// A submitted `:View` command line.
    Command,
    /// A key sequence nvim maps to a view invocation.
    Sequence,
}

/// What view knows of a `:` command line it has sent the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Typed {
    /// Every key since the `:` was one whose edit is known.
    Known(String),
    /// A key since the `:` edited the line in a way view does not model
    /// (a completion, a history recall). The engine's own `cmdline_show`
    /// is read at the `<CR>` in its place.
    Unknown {
        /// The expression line open at the second level, with this line
        /// waiting beneath it.
        nested: Option<Box<Typed>>,
        /// The key the next one is read as the argument of.
        argument: Option<Argument>,
    },
}

impl Typed {
    fn unknown() -> Self {
        Self::Unknown {
            nested: None,
            argument: None,
        }
    }

    /// Folds a key that types into or deletes from the line, and says
    /// whether it was a backspace on an empty line, which leaves it.
    fn edit(&mut self, notation: &str) -> bool {
        let Self::Known(text) = self else {
            return false;
        };
        // `<Del>` at the end of the line, where the modelled cursor always
        // stands, deletes the character before it as a backspace does
        if line_key(notation) == LineKey::Delete {
            return text.pop().is_none();
        }
        match notation_char(notation) {
            Some(c) if text.len() < TRACKED_MAX => text.push(c),
            _ => *self = Self::unknown(),
        }
        false
    }
}

/// A command-line key that reads the key after it as its argument, so an
/// `<Esc>` or a `<CR>` there ends no line. A `<C-c>` there interrupts
/// every one of them but a literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Argument {
    /// `<C-r>`: the register to insert, where `=` opens an expression line.
    Register,
    /// `<C-\>`: `e` opens an expression line, and `<C-n>` or `<C-g>` leave.
    /// nvim inserts the `<C-\>` before any other key and reads that key
    /// as typed on its own.
    Backslash,
    /// `<C-v>` and `<C-q>`: one key inserted as it is.
    Literal,
    /// `<C-k>`: the first key of a digraph, abandoned by an escape. A first
    /// key whose base key is special is inserted by its name, and ends it.
    Digraph,
    /// The second key of a digraph.
    DigraphSecond,
}

/// What a key does to the command line it is typed into, judged by its
/// base key with the modifiers set aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKey {
    /// An `<Esc>` with any modifiers, or a Ctrl `[`: leaves the line, and
    /// abandons a digraph.
    Escape,
    /// `<C-c>` alone: leaves the line, and interrupts a key waiting for its
    /// argument.
    Interrupt,
    /// A Ctrl `c` with other modifiers: leaves the line, and is an ordinary
    /// key as an argument.
    Cancel,
    Submit,
    /// A backspace, or a delete at the end of the line. A shifted `<Del>`
    /// is inserted by its name.
    Delete,
    Other,
}

impl LineKey {
    fn leaves(self) -> bool {
        matches!(self, Self::Escape | Self::Interrupt | Self::Cancel)
    }
}

fn line_key(notation: &str) -> LineKey {
    let Some(key) = modified(notation) else {
        return LineKey::Other;
    };
    let named = |names: &[&str]| names.iter().any(|name| key.base.eq_ignore_ascii_case(name));
    let ctrl_only = key.ctrl && !(key.shift || key.alt || key.meta || key.cmd);
    if named(&["Esc"]) || key.ctrl && key.base == "[" {
        LineKey::Escape
    } else if key.ctrl && named(&["c"]) {
        if ctrl_only {
            LineKey::Interrupt
        } else {
            LineKey::Cancel
        }
    } else if named(&["CR", "NL", "kEnter"]) || key.ctrl && named(&["m", "j"]) {
        LineKey::Submit
    } else if named(&["BS", "kDel"]) || named(&["Del"]) && !key.shift || key.ctrl && named(&["h"]) {
        LineKey::Delete
    } else {
        LineKey::Other
    }
}

/// A command-line mapping or abbreviation the user's config defines, as
/// the engine reads it from `maplist()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdlineMap {
    /// The keys it is typed as, spelled by `keytrans()`.
    pub lhs: String,
    /// What nvim puts on the line in their place, in key notation.
    pub rhs: String,
    /// An abbreviation, expanded when the word ends.
    pub abbr: bool,
    /// Whether nvim leaves `rhs` unmapped.
    pub noremap: bool,
    /// An `<expr>` or Lua-callback mapping, whose text only nvim knows.
    pub expr: bool,
}

/// A [`CmdlineMap`] reduced to the text it matches on a line.
#[derive(Debug, Clone)]
struct Expansion {
    lhs: String,
    /// `None` for an expansion only nvim can compute.
    rhs: Option<String>,
    abbr: bool,
    noremap: bool,
}

/// How many times a remapped rhs is expanded again. nvim's own
/// `'maxmapdepth'` bounds a recursive one at 1000 and then errors, which
/// runs no command.
const MAP_DEPTH: usize = 16;

/// The characters in nvim's default `'iskeyword'`, which end an
/// abbreviation where one stops.
fn is_keyword(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || u32::from(c) >= 192
}

/// The command line being typed and the input held behind a submitted
/// `:View` or a key that invokes view.
#[derive(Debug, Clone, Default)]
pub struct SubmitHold {
    typed: Option<Typed>,
    /// Every text the tracked line has held while view knew it, oldest
    /// first. nvim showing one of them is showing keys still in flight,
    /// whatever the edit that led away from it.
    states: Vec<String>,
    /// Whether nvim has shown the tracked line open.
    opened: bool,
    /// Lines the tracker has seen end that nvim has not yet hidden.
    unhidden: u32,
    /// Whether a line end was folded whose key has not yet been sent.
    end_unsent: bool,
    /// When the newest line end went to nvim, while it is younger than
    /// the command-line backstop.
    ended_at: Option<SpecStamp>,
    held: Option<(Armed, Vec<Msg>)>,
    generation: u64,
    /// Every key sequence nvim runs a view invocation on.
    invoke_keys: Vec<Invocation>,
    /// Every key sequence the user's own config maps in normal mode, one
    /// [`canonical`] key per entry.
    user_keys: Vec<Vec<String>>,
    /// The user's command-line mappings and abbreviations whose keys type
    /// text.
    cmdline_maps: Vec<Expansion>,
    /// The latest keys sent to nvim in normal mode, as many as the longest
    /// of `invoke_keys`.
    recent: std::collections::VecDeque<Folded>,
    /// The key whose argument nvim reads the next normal-mode key as: the
    /// key before it takes one, and was not itself an argument.
    argument_of: Option<String>,
    /// Whether a key that leaves normal mode has gone out since nvim last
    /// reported a mode, so the mode view last read may be stale.
    mode_unsure: bool,
    /// Keys a surface of view's own is holding while they spell the start
    /// of a mapped sequence.
    sequence: Vec<String>,
    sequence_generation: u64,
    /// The `'timeoutlen'` the engine reported, or `None` before it has.
    timeoutlen: Option<Duration>,
    /// Whether the user's config turned `'timeout'` off, so nvim waits
    /// for the next key however long it takes.
    timeout_off: bool,
}

/// One key sent to nvim in normal mode.
#[derive(Debug, Clone)]
struct Folded {
    key: String,
    /// Whether nvim read it as the argument of the key before it.
    argument: bool,
    /// Whether a key that leaves normal mode went out ahead of it, with no
    /// mode reported since.
    mode_unsure: bool,
}

/// One key sequence nvim runs a view invocation on.
#[derive(Debug, Clone)]
struct Invocation {
    feature: String,
    /// One [`canonical`] key per entry.
    keys: Vec<String>,
}

/// Where a run of keys stands against the mapped sequences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sequence {
    /// The keys are a whole sequence, invoking this feature.
    Complete(String),
    /// The keys are a whole sequence the user's config maps.
    User,
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
                let keys: Vec<_> = key_tokens(claim.keys.as_deref()?).map(canonical).collect();
                (!keys.is_empty()).then(|| Invocation {
                    feature: claim.feature.clone(),
                    keys,
                })
            })
            .collect();
        self.recent.clear();
        self.sequence.clear();
    }

    /// Learns the key sequences the user's own config maps in normal mode,
    /// and how long nvim waits for the rest of one: `timeoutlen` is `None`
    /// where `'timeout'` is off.
    pub fn learn_user_keys(&mut self, keys: &[String], timeoutlen: Option<Duration>) {
        self.user_keys = keys
            .iter()
            .map(|keys| key_tokens(keys).map(canonical).collect::<Vec<_>>())
            .filter(|keys| !keys.is_empty())
            .collect();
        self.timeout_off = timeoutlen.is_none();
        self.timeoutlen = timeoutlen;
        self.sequence.clear();
    }

    /// Learns the user's command-line mappings and abbreviations, which
    /// can make a line `:View` that no typed key spelled. A lhs holding a
    /// key that types no character never matches typed text and is left
    /// out.
    pub fn learn_cmdline_maps(&mut self, maps: &[CmdlineMap]) {
        let text =
            |spelled: &str| -> Option<String> { key_tokens(spelled).map(notation_char).collect() };
        self.cmdline_maps = maps
            .iter()
            .filter_map(|map| {
                let lhs = text(&map.lhs).filter(|lhs| !lhs.is_empty())?;
                // a key in the rhs that types no character (a `<CR>`) ends
                // the text the line holds
                let rhs = (!map.expr).then(|| {
                    key_tokens(&map.rhs)
                        .map_while(notation_char)
                        .collect::<String>()
                });
                Some(Expansion {
                    lhs,
                    rhs,
                    abbr: map.abbr,
                    noremap: map.noremap,
                })
            })
            .collect();
    }

    /// `typed` as nvim puts it on the line once its command-line mappings
    /// have run on the keys at its start, or `typed` itself where an
    /// expansion only nvim can compute stands in the way.
    fn expand_mappings(&self, typed: &str) -> String {
        let mut line = typed.to_string();
        for _ in 0..MAP_DEPTH {
            let Some(map) = self
                .cmdline_maps
                .iter()
                .filter(|map| !map.abbr && line.starts_with(&map.lhs))
                .max_by_key(|map| map.lhs.len())
            else {
                break;
            };
            let Some(rhs) = &map.rhs else {
                return typed.to_string();
            };
            line = format!("{rhs}{}", &line[map.lhs.len()..]);
            // nvim remaps no rhs that begins with its own lhs
            if map.noremap || rhs.starts_with(&map.lhs) {
                break;
            }
        }
        line
    }

    /// `line` with its command word expanded as nvim expands an
    /// abbreviation: at the `<CR>` that ends the line, and, where
    /// `in_flight` says nvim has yet to read the keys after the word, at
    /// the non-keyword character that follows it.
    fn expand_abbreviation(&self, line: &str, in_flight: bool) -> String {
        let rest = line.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        let start = line.len() - rest.len();
        let end = rest.find(|c: char| !is_keyword(c)).unwrap_or(rest.len());
        if !in_flight && end < rest.len() {
            return line.to_string();
        }
        let word = &rest[..end];
        match self
            .cmdline_maps
            .iter()
            .find(|map| map.abbr && map.lhs == word)
            .and_then(|map| map.rhs.as_deref())
        {
            Some(rhs) => format!("{}{rhs}{}", &line[..start], &rest[end..]),
            None => line.to_string(),
        }
    }

    /// The line nvim runs for `typed`, keys still on their way to it: its
    /// mappings, then its abbreviations.
    fn expand_typed(&self, typed: &str) -> String {
        self.expand_abbreviation(&self.expand_mappings(typed), true)
    }

    /// Notes that nvim reported `mode`, which answers every key that left
    /// normal mode before it.
    pub fn note_mode_reported(&mut self, mode: &str) {
        self.mode_unsure = false;
        // a report sent before nvim read the newest end says nothing of it
        if self.ended_at.is_none() {
            self.settle_ends(mode);
        }
    }

    /// Forgets the counted line ends when no line is tracked and `mode`
    /// is outside the command line: a line end counted for a `:` nvim read
    /// as text never gets its hide.
    fn settle_ends(&mut self, mode: &str) {
        if self.typed.is_none() && !crate::native::speculate::is_cmdline_mode(mode) {
            self.unhidden = 0;
        }
    }

    /// Stamps a line end folded from the key now going to nvim at `now`.
    pub(crate) fn note_key_sent(&mut self, now: SpecStamp) {
        if std::mem::take(&mut self.end_unsent) {
            self.ended_at = Some(now);
        }
    }

    /// Lets the newest line end go once it is older than `backstop` at
    /// `now`. nvim has read it by then, so `mode`, the last one reported,
    /// answers it, and a later mode report may close the ends still
    /// counted.
    pub(crate) fn age_line_ends(&mut self, now: SpecStamp, backstop: Duration, mode: &str) {
        if self
            .ended_at
            .is_some_and(|sent| now.age_since(sent) >= backstop)
        {
            self.ended_at = None;
            // a line tracked now opened after every counted end, and its
            // own `:` is still unanswered while the mode is outside the
            // command line
            if !crate::native::speculate::is_cmdline_mode(mode) {
                self.unhidden = 0;
            }
        }
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
    /// sequences, and against the user's own where `user` is set. A whole
    /// invoking sequence wins over a longer one it begins: nvim is handed
    /// the keys, and its own `'timeout'` decides between the two. A whole
    /// user sequence that begins a longer one is a prefix, since nothing
    /// holds the keys behind it once it has gone out.
    #[must_use]
    pub(crate) fn sequence(&self, keys: &[String], user: bool) -> Sequence {
        let typed: Vec<String> = keys.iter().map(|key| canonical(key)).collect();
        let whole = self.whole(&typed, user);
        if matches!(whole, Sequence::Complete(_)) {
            return whole;
        }
        let longer = |keys: &Vec<String>| keys.len() > typed.len() && keys.starts_with(&typed);
        if self
            .invoke_keys
            .iter()
            .any(|invocation| longer(&invocation.keys))
            || (user && self.user_keys.iter().any(longer))
        {
            return Sequence::Prefix;
        }
        whole
    }

    /// The mapped sequence `keys` spell whole, whatever longer one they
    /// begin: never [`Sequence::Prefix`].
    #[must_use]
    pub(crate) fn spelled(&self, keys: &[String], user: bool) -> Sequence {
        let typed: Vec<String> = keys.iter().map(|key| canonical(key)).collect();
        self.whole(&typed, user)
    }

    fn whole(&self, typed: &[String], user: bool) -> Sequence {
        if let Some(whole) = self
            .invoke_keys
            .iter()
            .find(|invocation| invocation.keys == typed)
        {
            return Sequence::Complete(whole.feature.clone());
        }
        if user && self.user_keys.iter().any(|keys| keys == typed) {
            return Sequence::User;
        }
        Sequence::Neither
    }

    /// Hands back the keys a surface is holding, leaving none held.
    pub(crate) fn take_sequence(&mut self) -> Vec<String> {
        std::mem::take(&mut self.sequence)
    }

    /// Holds `keys` as the start of a mapped sequence, bounded by nvim's
    /// own `'timeoutlen'`, after which nvim gives up on the longer
    /// sequence and runs what it has.
    pub(crate) fn keep_sequence(&mut self, keys: Vec<String>) -> Vec<Effect> {
        self.sequence = keys;
        self.sequence_generation = self.sequence_generation.wrapping_add(1);
        if self.timeout_off {
            return Vec::new();
        }
        vec![Effect::ScheduleSequenceExpiry {
            // the engine has not yet reported the user's own value
            after: self.timeoutlen.unwrap_or(crate::msg::DEFAULT_TIMEOUTLEN),
            generation: self.sequence_generation,
        }]
    }

    /// Hands back the held keys when `generation` is the bound the latest
    /// of them armed.
    pub(crate) fn take_expired_sequence(&mut self, generation: u64) -> Vec<String> {
        if generation == self.sequence_generation {
            self.take_sequence()
        } else {
            Vec::new()
        }
    }

    /// Whether `msg` ends a standing hold: the command's own notification,
    /// the bound this hold armed, or, for a hold a key sequence armed, a
    /// mode nvim reports leaving normal mode for, which says the sequence
    /// ran no mapping.
    #[must_use]
    pub fn releases(&self, msg: &Msg) -> bool {
        let Some((armed, _)) = &self.held else {
            return false;
        };
        match msg {
            Msg::FeatureInvoke { .. } => true,
            Msg::SubmitHoldExpired { generation } => *generation == self.generation,
            Msg::Redraw(events) => *armed == Armed::Sequence
                && events.iter().any(
                    |event| matches!(event, UiEvent::ModeChange { mode, .. } if mode != "normal"),
                ),
            _ => false,
        }
    }

    /// Keeps `msg` when a hold stands and it is input, handing it back
    /// otherwise.
    pub fn hold(&mut self, msg: Msg) -> Option<Msg> {
        match (&mut self.held, &msg) {
            (Some((_, held)), Msg::Key(_) | Msg::Mouse(_) | Msg::Paste(_)) => {
                held.push(msg);
                None
            }
            _ => Some(msg),
        }
    }

    /// Ends the hold, handing back what it kept in the order it arrived.
    pub fn take_held(&mut self) -> Vec<Msg> {
        self.held.take().map(|(_, held)| held).unwrap_or_default()
    }

    /// Forgets which key nvim reads the next key as the argument of, where
    /// something other than a key (a click, a paste) went out after it.
    pub fn forget_argument(&mut self) {
        self.argument_of = None;
    }

    /// Whether input is being held.
    #[must_use]
    pub fn is_holding(&self) -> bool {
        self.held.is_some()
    }

    /// Whether a `:` view sent has opened a command line that no key since
    /// has submitted or left.
    #[must_use]
    pub(crate) fn types_a_line(&self) -> bool {
        self.typed.is_some()
    }

    /// Drops the tracked line, so keys typed after it are read where
    /// focus stands.
    pub(crate) fn forget_line(&mut self) {
        self.set_typed(None);
    }

    /// Whether nvim has shown the tracked line open.
    #[must_use]
    pub(crate) fn line_opened(&self) -> bool {
        self.opened
    }

    /// Reads a command line nvim shows. nvim hides every first-level line
    /// that ends before it shows the next, so while a line view saw end is
    /// still unhidden, every show is that older line's. Otherwise a first
    /// level `:` show holding a prefix of the keys typed, or what the
    /// user's command-line mappings made of one, is the tracked line open.
    pub(crate) fn note_line_shown(&mut self, line: &CmdlineState) {
        if self.opened || self.unhidden > 0 || line.level != 1 || line.firstc != ":" {
            return;
        }
        let shown: String = line.content.iter().map(|(_, s)| s.as_str()).collect();
        self.opened = match &self.typed {
            Some(Typed::Known(_)) => self
                .states
                .iter()
                .any(|state| *state == shown || self.expand_mappings(state) == shown),
            Some(_) => true,
            None => false,
        };
    }

    /// Reads nvim closing the command line at `level`, where `line` is the
    /// one it last showed. Only a `:` line, or one hidden before it was
    /// shown, spends a counted end: a search or a prompt ends no line a
    /// `:` of view's opened. A `q:` window and a mapping's own `:` line
    /// read the same as a typed one and spend it all the same.
    pub(crate) fn note_line_hidden(&mut self, level: u64, line: Option<&CmdlineState>) {
        let typed_line = line.is_none_or(|line| line.level == 1 && line.firstc == ":");
        if level == 1 && typed_line {
            self.unhidden = self.unhidden.saturating_sub(1);
        }
    }

    fn set_typed(&mut self, typed: Option<Typed>) {
        self.typed = typed;
        self.opened = false;
        self.states.clear();
        self.note_edited();
    }

    /// Records the tracked line's text after an edit. A line edited more
    /// times than the record holds is one view no longer knows, and nvim's
    /// own line is read at its `<CR>`.
    fn note_edited(&mut self) {
        let Some(Typed::Known(text)) = &self.typed else {
            return;
        };
        if self.states.len() < TRACKED_MAX {
            self.states.push(text.clone());
        } else {
            self.typed = Some(Typed::unknown());
        }
    }

    /// Ends the tracked line, which nvim answers with one hide, and tracks
    /// `next` in its place.
    fn end_line(&mut self, next: Option<Typed>) -> Option<Typed> {
        self.unhidden = self.unhidden.saturating_add(1);
        self.end_unsent = true;
        let ended = self.typed.take();
        self.set_typed(next);
        ended
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
        model.submit_hold.set_typed(None);
        return arm(model, Armed::Sequence);
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
///
/// The mode is read the same way. `o<Space>e` typed inside one round trip
/// reaches nvim in insert mode, where it is text, while the mode view last
/// read still says normal. A sequence whose first key went out behind a
/// key that leaves normal mode, with no mode reported since, completes
/// nothing.
fn completes_invoke(model: &mut Model, notation: &str) -> bool {
    let normal = model.engine.mode.current == "normal";
    let hold = &mut model.submit_hold;
    let argument_of = hold.argument_of.take();
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
    let key = canonical(notation);
    hold.argument_of = (argument_of.is_none()
        && crate::native::speculate::CMDLINE_LITERAL_KEYS.contains(&notation))
    .then(|| key.clone());
    let mode_unsure = hold.mode_unsure;
    hold.mode_unsure |= match argument_of.as_deref() {
        None => LEAVES_NORMAL.contains(&key.as_str()),
        Some(before) => LEAVES_NORMAL_AFTER.contains(&(before, key.as_str())),
    };
    if hold.recent.len() >= longest {
        hold.recent.pop_front();
    }
    hold.recent.push_back(Folded {
        key,
        argument: argument_of.is_some(),
        mode_unsure,
    });
    let recent = &hold.recent;
    let complete = hold.invoke_keys.iter().any(|invocation| {
        let keys = &invocation.keys;
        recent.len().checked_sub(keys.len()).is_some_and(|start| {
            recent
                .get(start)
                .is_some_and(|first| !first.argument && !first.mode_unsure)
                && recent
                    .range(start..)
                    .map(|folded| &folded.key)
                    .eq(keys.iter())
        })
    });
    if complete {
        // the keys inside the sequence were the mapping's, and left no
        // mode behind them
        hold.recent.clear();
        hold.argument_of = None;
        hold.mode_unsure = false;
    }
    complete
}

/// One spelling for each key nvim reads as the same key: `keytrans()`
/// writes `<M-S-Left>` and `<Space>` where view's input writes
/// `<S-M-Left>` and a bare space, and a shifted letter is its capital.
fn canonical(key: &str) -> String {
    if let Some(c) = notation_char(key) {
        return c.to_string();
    }
    let Some(Modified {
        ctrl,
        mut shift,
        alt,
        meta,
        cmd,
        base,
    }) = modified(key)
    else {
        return key.to_string();
    };
    let mut chars = base.chars();
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
        (Some(_), None) => base.to_string(),
        _ => base.to_ascii_lowercase(),
    };
    let modifiers: String = [
        (ctrl, "C-"),
        (shift, "S-"),
        (alt, "M-"),
        (cmd, "D-"),
        (meta, "T-"),
    ]
    .into_iter()
    .filter_map(|(on, spelled)| on.then_some(spelled))
    .collect();
    format!("<{modifiers}{name}>")
}

/// The modifiers a `<>` key notation carries, and the key they modify.
#[derive(Debug, Clone, Copy)]
struct Modified<'a> {
    ctrl: bool,
    shift: bool,
    /// `M-` or `A-`.
    alt: bool,
    /// `T-`.
    meta: bool,
    /// `D-`.
    cmd: bool,
    base: &'a str,
}

/// Splits a `<>` key notation into its modifiers and its base key, or
/// `None` for a key written as its character.
fn modified(notation: &str) -> Option<Modified<'_>> {
    let mut base = notation.strip_prefix('<')?.strip_suffix('>')?;
    let [mut ctrl, mut shift, mut alt, mut meta, mut cmd] = [false; 5];
    while base.len() > 2 && base.as_bytes()[1] == b'-' {
        match base.as_bytes()[0].to_ascii_uppercase() {
            b'C' => ctrl = true,
            b'S' => shift = true,
            b'M' | b'A' => alt = true,
            b'T' => meta = true,
            b'D' => cmd = true,
            _ => break,
        }
        base = &base[2..];
    }
    Some(Modified {
        ctrl,
        shift,
        alt,
        meta,
        cmd,
        base,
    })
}

/// The command-line half of [`fold_engine_key`].
fn fold_line(model: &mut Model, notation: &str) -> Vec<Effect> {
    let hold = &mut model.submit_hold;
    // an Escape typed quickly before its key arrives as one Meta key, which
    // nvim runs as `<Esc>` and then the key: `<M-:>` opens a command line
    // from any mode, and any other one leaves the line being typed. nvim
    // reads an argument with mappings off, so there it stays one key.
    let argument_pending = matches!(
        hold.typed,
        Some(Typed::Unknown {
            argument: Some(_),
            ..
        })
    );
    if let Some(key) = crate::native::keys::escaped_key(notation).filter(|_| !argument_pending) {
        let next = (key == ":").then(|| Typed::Known(String::new()));
        match &mut hold.typed {
            // the `<Esc>` leaves the expression line, and the key is typed
            // into the line it returns to
            Some(Typed::Unknown {
                nested: nested @ Some(_),
                ..
            }) => {
                *nested = None;
                return fold_line(model, &key);
            }
            Some(_) => {
                hold.end_line(next);
            }
            None => hold.set_typed(next),
        }
        return Vec::new();
    }
    let Some(typed) = hold.typed.as_mut() else {
        if notation == ":" && may_open(model) {
            model
                .submit_hold
                .set_typed(Some(Typed::Known(String::new())));
        }
        return Vec::new();
    };
    if let Typed::Unknown { nested, argument } = typed {
        if let Some(of) = argument.take() {
            let taken = match (of, notation) {
                // these two change how the register is inserted and wait
                // for its name
                (Argument::Register, "<C-r>" | "<C-o>") => {
                    *argument = Some(Argument::Register);
                    true
                }
                (Argument::Register, "=") | (Argument::Backslash, "e") if nested.is_none() => {
                    *nested = Some(Box::new(Typed::Known(String::new())));
                    true
                }
                // an expression line refuses a second one inside it
                (Argument::Register, "=") => true,
                (Argument::Backslash, "<C-n>" | "<C-g>") => {
                    if nested.take().is_none() {
                        hold.end_line(None);
                    }
                    return Vec::new();
                }
                // the interrupt ends the line it was typed into, and the
                // register's reaches the line beneath an expression line too
                (of @ (Argument::Register | Argument::Digraph | Argument::DigraphSecond), key)
                    if line_key(key) == LineKey::Interrupt =>
                {
                    if of == Argument::Register || nested.take().is_none() {
                        hold.end_line(None);
                    }
                    return Vec::new();
                }
                (Argument::Digraph, key)
                    if line_key(key) != LineKey::Escape && !special_key(key) =>
                {
                    *argument = Some(Argument::DigraphSecond);
                    true
                }
                (of, _) => {
                    // whatever was inserted into an expression line is
                    // unknown to view
                    if let Some(line) = nested.as_deref_mut() {
                        *line = Typed::unknown();
                    }
                    of != Argument::Backslash
                }
            };
            if taken {
                return Vec::new();
            }
        }
        if let Some(line) = nested {
            // ending the expression line returns to the line beneath it
            let key = line_key(notation);
            if key.leaves() || key == LineKey::Submit {
                *nested = None;
            } else if let Some(of) = argument_of(notation) {
                *argument = Some(of);
            } else if line.edit(notation) {
                *nested = None;
            }
            return Vec::new();
        }
    }
    if let Some(of) = argument_of(notation) {
        *typed = Typed::Unknown {
            nested: None,
            argument: Some(of),
        };
        return Vec::new();
    }
    let key = line_key(notation);
    if key.leaves() {
        hold.end_line(None);
    } else if key == LineKey::Submit {
        let opened = hold.line_opened();
        let states = std::mem::take(&mut hold.states);
        let typed = hold.end_line(None);
        if submits_view(model, opened, typed.as_ref(), &states) {
            return arm(model, Armed::Command);
        }
    } else if typed.edit(notation) {
        hold.end_line(None);
    } else {
        hold.note_edited();
    }
    Vec::new()
}

/// The argument `notation` makes the next command-line key.
fn argument_of(notation: &str) -> Option<Argument> {
    match notation {
        "<C-r>" => Some(Argument::Register),
        "<C-\\>" => Some(Argument::Backslash),
        "<C-v>" | "<C-q>" => Some(Argument::Literal),
        "<C-k>" => Some(Argument::Digraph),
        _ => None,
    }
}

/// Whether nvim reads `notation` as a special key, judged by its base key
/// with the modifiers set aside. A modified character is still a
/// character, and the keypad's character keys reach the command line as
/// their characters. `<C-@>` is `<Nul>`, and a shifted `<Tab>` is
/// `<S-Tab>`, both special.
fn special_key(notation: &str) -> bool {
    let Some(Modified {
        ctrl, shift, base, ..
    }) = modified(notation)
    else {
        return false;
    };
    let named = |names: &[&str]| names.iter().any(|name| base.eq_ignore_ascii_case(name));
    if base.chars().count() == 1 {
        return ctrl && base == "@";
    }
    if base.eq_ignore_ascii_case("Tab") {
        return shift;
    }
    let keypad = base
        .strip_prefix('k')
        .is_some_and(|key| key.len() == 1 && key.as_bytes()[0].is_ascii_digit())
        || named(&[
            "kPlus",
            "kMinus",
            "kMultiply",
            "kDivide",
            "kPoint",
            "kComma",
            "kEqual",
            "kEnter",
        ]);
    !(keypad || named(&["Space", "lt", "Bslash", "Bar", "CR", "NL", "Esc"]))
}

/// Whether a `:` reaching the engine now can open a command line: the
/// last mode nvim reported reads one of the modes `:` does that in, or a
/// key still in flight may have put it there. The literal-taking keys
/// read the `:` as their argument.
pub(crate) fn may_open(model: &Model) -> bool {
    !model.engine.literal_pending
        && (model.engine.key_unanswered.is_some()
            || crate::native::speculate::CMDLINE_GATE_MODES
                .contains(&model.engine.mode.current.as_str()))
}

/// Whether the line a `<CR>` submits runs `:View`, read from the engine's
/// last `cmdline_show` of it, and from the keys view sent where the engine
/// has shown none or is showing a text those keys gave the line on the way
/// (`states`), since the keys after it are still in flight.
///
/// A `cnoremap` or a `cabbrev` can put `View` on a line whose keys never
/// spelled it. The engine's line shows a mapping's text once nvim has read
/// its keys, and the user's command-line mappings and abbreviations expand
/// the keys it has not read yet. An abbreviation that ends the line is
/// expanded by the `<CR>` itself, after nvim's last show of the line.
fn submits_view(model: &Model, opened: bool, typed: Option<&Typed>, states: &[String]) -> bool {
    let hold = &model.submit_hold;
    let shown = model
        .engine
        .cmdline
        .as_ref()
        .filter(|line| line.firstc == ":")
        .map(|line| {
            line.content
                .iter()
                .map(|(_, s)| s.as_str())
                .collect::<String>()
        });
    match (typed, shown) {
        (Some(Typed::Known(_)), Some(shown)) if opened && !states.contains(&shown) => {
            names_view(&hold.expand_abbreviation(&shown, false))
        }
        (Some(Typed::Known(text)), _) => names_view(&hold.expand_typed(text)),
        (_, shown) => {
            shown.is_some_and(|shown| names_view(&hold.expand_abbreviation(&shown, false)))
        }
    }
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
fn arm(model: &mut Model, armed: Armed) -> Vec<Effect> {
    let hold = &mut model.submit_hold;
    hold.generation = hold.generation.wrapping_add(1);
    hold.held = Some((armed, Vec::new()));
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

    fn show_line(model: &mut Model, text: &str) {
        let _ = crate::update::update(
            model,
            Msg::Redraw(vec![UiEvent::CmdlineShow {
                content: vec![(0, text.to_string())],
                pos: 0,
                firstc: ":".to_string(),
                prompt: String::new(),
                indent: 0,
                level: 1,
            }]),
        );
    }

    fn arms(effects: &[Effect]) -> bool {
        effects
            .iter()
            .any(|e| matches!(e, Effect::ScheduleSubmitHold { .. }))
    }

    /// A `cnoremap` that turns the typed line into `View ai open` arms the
    /// hold on what the engine shows, and an engine line still showing a
    /// text the keys typed gave the line on the way defers to them.
    #[test]
    fn a_line_the_engine_shows_as_view_holds_whatever_keys_spelled_it() {
        let mut model = normal_mode();
        let _ = type_keys(&mut model, &[":"]);
        show_line(&mut model, "");
        let _ = type_keys(&mut model, &["v", "v"]);
        show_line(&mut model, "View ai open");
        let sent = type_keys(&mut model, &["<CR>"]);
        assert!(arms(&sent), "{sent:?}");

        let mut model = normal_mode();
        let _ = type_keys(&mut model, &[":"]);
        show_line(&mut model, "");
        let sent = type_keys(&mut model, &["V", "i", "e", "w", "<CR>"]);
        assert!(arms(&sent), "{sent:?}");

        let mut model = normal_mode();
        let _ = type_keys(&mut model, &[":"]);
        show_line(&mut model, "");
        let _ = type_keys(&mut model, &["V", "i"]);
        show_line(&mut model, "Vi");
        let sent = type_keys(&mut model, &["m", "<CR>"]);
        assert!(!arms(&sent), "{sent:?}");

        // a typo corrected faster than nvim shows it: the shown `Vx` is a
        // text the keys gave the line, so the typed `View` decides
        let mut model = normal_mode();
        let _ = type_keys(&mut model, &[":"]);
        show_line(&mut model, "");
        let _ = type_keys(&mut model, &["V", "x"]);
        show_line(&mut model, "Vx");
        let sent = type_keys(&mut model, &["<BS>", "i", "e", "w", "<CR>"]);
        assert!(arms(&sent), "{sent:?}");

        let mut model = normal_mode();
        let _ = type_keys(&mut model, &[":"]);
        show_line(&mut model, "");
        let _ = type_keys(&mut model, &["V", "i"]);
        show_line(&mut model, "Vi");
        let sent = type_keys(
            &mut model,
            &["<BS>", "<BS>", "e", "<Space>", "f", "o", "o", "<CR>"],
        );
        assert!(!arms(&sent), "{sent:?}");
    }

    fn cmdline_maps(model: &mut Model, maps: &[(&str, &str, bool, bool, bool)]) {
        let _ = crate::update::update(
            model,
            Msg::UserMappingsRead {
                keys: Vec::new(),
                timeoutlen: None,
                cmdline: maps
                    .iter()
                    .map(|&(lhs, rhs, abbr, noremap, expr)| CmdlineMap {
                        lhs: lhs.to_string(),
                        rhs: rhs.to_string(),
                        abbr,
                        noremap,
                        expr,
                    })
                    .collect(),
            },
        );
    }

    /// `cabbrev vo View ai open`: nvim expands the word at the `<CR>`,
    /// after its last show of the line, so `:vo<CR>` runs `:View` whether
    /// nvim has shown `vo` or none of it.
    #[test]
    fn a_command_line_abbreviation_for_view_arms_the_hold() {
        for shown in [true, false] {
            let mut model = normal_mode();
            cmdline_maps(&mut model, &[("vo", "View ai open", true, false, false)]);
            let _ = type_keys(&mut model, &[":", "v", "o"]);
            if shown {
                show_line(&mut model, "vo");
            }
            let sent = type_keys(&mut model, &["<CR>"]);
            assert!(arms(&sent), "shown {shown}: {sent:?}");
            let held = type_keys(&mut model, &["j"]);
            assert!(held.is_empty(), "shown {shown}: {held:?}");
        }
    }

    /// `cnoremap vv View ai open` with `:vv` reaching nvim in one batch:
    /// nvim's first show is already the expansion, which opens the line,
    /// and the line arms whether that show arrived before the `<CR>` or
    /// not.
    #[test]
    fn a_batched_command_line_mapping_for_view_arms_the_hold() {
        for shown in [true, false] {
            let mut model = normal_mode();
            cmdline_maps(&mut model, &[("vv", "View ai open", false, true, false)]);
            let _ = type_keys(&mut model, &[":", "v", "v"]);
            if shown {
                show_line(&mut model, "View ai open");
                assert!(model.submit_hold.line_opened());
            }
            let sent = type_keys(&mut model, &["<CR>"]);
            assert!(arms(&sent), "shown {shown}: {sent:?}");
        }

        // a remapped rhs is mapped again, and a `noremap` one is not
        for (noremap, armed) in [(false, true), (true, false)] {
            let mut model = normal_mode();
            cmdline_maps(
                &mut model,
                &[
                    ("zz", "vv", false, noremap, false),
                    ("vv", "View ai open", false, true, false),
                ],
            );
            let sent = type_keys(&mut model, &[":", "z", "z", "<CR>"]);
            assert_eq!(arms(&sent), armed, "noremap {noremap}: {sent:?}");
        }
    }

    /// An `<expr>` mapping or abbreviation computes its text inside nvim,
    /// so the keys typed decide, and `ww` names no command view runs. The
    /// expression `View` reads as `:View` if it is taken for text.
    #[test]
    fn an_expr_command_line_mapping_leaves_the_typed_keys_deciding() {
        for abbr in [false, true] {
            let mut model = normal_mode();
            cmdline_maps(&mut model, &[("ww", "View", abbr, false, true)]);
            let sent = type_keys(&mut model, &[":", "w", "w", "<CR>"]);
            assert!(!arms(&sent), "abbr {abbr}: {sent:?}");
        }
    }

    /// An abbreviation to another command arms nothing, even where its own
    /// word is a prefix of `View` nvim would otherwise run as `:View`.
    #[test]
    fn an_abbreviation_for_another_command_arms_nothing() {
        for (lhs, keys) in [
            ("ve", &[":", "v", "e", "<CR>"][..]),
            ("V", &[":", "V", "<CR>"]),
        ] {
            let mut model = normal_mode();
            cmdline_maps(&mut model, &[(lhs, "vsplit", true, false, false)]);
            let sent = type_keys(&mut model, keys);
            assert!(!arms(&sent), "{lhs}: {sent:?}");
        }
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

    /// A key that takes an argument and was itself one leaves the key after
    /// it free: `ff`, `fa` and `"a` each end their argument, and a count
    /// ahead of the sequence is a count, so `<Space>e` behind any of them
    /// still holds.
    #[test]
    fn a_key_typed_as_an_argument_takes_none_of_its_own() {
        for lead in [&["f", "f"][..], &["f", "a"], &["\"", "a"], &["3"]] {
            let mut model = claiming(&[Some("<Space>e")]);
            let _ = type_keys(&mut model, lead);
            let _ = type_keys(&mut model, &[" ", "e"]);
            assert!(model.submit_hold.is_holding(), "{lead:?} then <Space>e");
        }
    }

    fn mode_reported(mode: &str) -> Msg {
        Msg::Redraw(vec![UiEvent::ModeChange {
            mode: mode.to_string(),
            mode_idx: 0,
        }])
    }

    /// A key that leaves normal mode, typed before nvim reports the mode
    /// it left for, makes the keys behind it text or a motion, so a
    /// sequence among them holds nothing and every key goes straight out.
    /// Its own keys inside a whole sequence leave nothing behind.
    #[test]
    fn a_sequence_behind_a_key_that_leaves_normal_mode_holds_nothing() {
        for lead in [
            &["o"][..],
            &["o", "x", "y"],
            &[":"],
            &["/"],
            &["d"],
            &["c"],
            &["v"],
            &["g", "i"],
            &["g", "u"],
        ] {
            let mut model = claiming(&[Some("<Space>e")]);
            let keys: Vec<&str> = lead.iter().copied().chain([" ", "e", "x"]).collect();
            let sent = type_keys(&mut model, &keys);
            assert_eq!(inputs(&sent), keys, "{lead:?}: {sent:?}");
            assert!(!model.submit_hold.is_holding(), "{lead:?}<Space>e armed");
        }

        let mut model = claiming(&[Some("<Space>ai"), Some("<Space>e")]);
        let _ = type_keys(&mut model, &[" ", "a", "i"]);
        let generation = model.submit_hold.generation;
        let _ = crate::update::update(&mut model, Msg::SubmitHoldExpired { generation });
        let _ = type_keys(&mut model, &[" ", "e"]);
        assert!(
            model.submit_hold.is_holding(),
            "the `a` and `i` of the invocation before left the mode unsure"
        );
    }

    /// `Q` replays a register and stays in normal mode, so a sequence
    /// behind it holds. `gn`, `gN` and nvim's default `gc` leave normal
    /// mode, so one behind them holds nothing.
    #[test]
    fn q_stays_in_normal_mode_where_gn_and_gc_leave_it() {
        let mut model = claiming(&[Some("<Space>e")]);
        let _ = type_keys(&mut model, &["Q", " ", "e"]);
        assert!(model.submit_hold.is_holding(), "Q then <Space>e");

        for lead in [&["g", "n"][..], &["g", "N"], &["g", "c"]] {
            let mut model = claiming(&[Some("<Space>e")]);
            let keys: Vec<&str> = lead.iter().copied().chain([" ", "e", "x"]).collect();
            let sent = type_keys(&mut model, &keys);
            assert_eq!(inputs(&sent), keys, "{lead:?}: {sent:?}");
            assert!(!model.submit_hold.is_holding(), "{lead:?}<Space>e armed");
        }
    }

    /// A mode nvim reports answers the key that left normal mode, and a
    /// sequence typed after it holds again.
    #[test]
    fn a_mode_report_answers_the_key_that_left_normal_mode() {
        let mut model = claiming(&[Some("<Space>e")]);
        let _ = type_keys(&mut model, &["v", "<Esc>"]);
        let _ = crate::update::update(&mut model, mode_reported("visual"));
        let _ = crate::update::update(&mut model, mode_reported("normal"));
        let _ = type_keys(&mut model, &[" ", "e"]);
        assert!(model.submit_hold.is_holding());
    }

    /// A hold a key sequence armed ends when nvim reports leaving normal
    /// mode, which a mapping of view's own never does: the held keys go
    /// out on that batch.
    #[test]
    fn a_sequence_hold_ends_when_nvim_leaves_normal_mode() {
        let mut model = claiming(&[Some("<Space>e")]);
        let _ = type_keys(&mut model, &[" ", "e", "x"]);
        assert!(model.submit_hold.is_holding());
        let replayed = crate::update::update(&mut model, mode_reported("insert"));
        assert_eq!(inputs(&replayed), ["x"], "{replayed:?}");
        assert!(!model.submit_hold.is_holding());
    }

    /// A `:View` hold stands through the command line's own mode reports,
    /// which arrive late when the whole line was typed inside one round
    /// trip.
    #[test]
    fn a_view_command_hold_stands_through_a_late_cmdline_report() {
        let mut model = normal_mode();
        let line = [
            ":", "V", "i", "e", "w", " ", "a", "i", " ", "o", "p", "e", "n", "<CR>",
        ];
        let _ = type_keys(&mut model, &line);
        assert!(model.submit_hold.is_holding());
        let _ = type_keys(&mut model, &["j"]);
        let effects = crate::update::update(&mut model, mode_reported("cmdline_normal"));
        assert!(inputs(&effects).is_empty(), "{effects:?}");
        assert!(model.submit_hold.is_holding());
    }

    /// A click after a key that takes an argument is that argument, so the
    /// key after the click starts a command again.
    #[test]
    fn a_click_ends_the_wait_for_an_argument() {
        let mut model = claiming(&[Some("<Space>e")]);
        let _ = type_keys(&mut model, &["m"]);
        crate::native::speculate::fold_engine_call(
            &mut model,
            &crate::msg::RpcCall::InputMouse {
                button: "left".to_string(),
                action: "press".to_string(),
                modifier: String::new(),
                grid: crate::grid::registry::GridId(1),
                row: 0,
                col: 0,
            },
            crate::native::speculate::SpecStamp::new(Duration::ZERO),
        );
        let _ = type_keys(&mut model, &[" ", "e"]);
        assert!(model.submit_hold.is_holding());
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
            key_tokens("<Space>a<lt>\\<M-S-Left>").collect::<Vec<_>>(),
            ["<Space>", "a", "<lt>", "\\", "<M-S-Left>"]
        );
    }

    /// `keytrans()` spells `Ctrl` with `>` as `<C->>` and a `|` as `<Bar>`,
    /// each one key, so a claim written with either holds behind that key.
    #[test]
    fn a_claim_spelling_a_closer_or_a_named_character_is_one_key() {
        for (claim, typed) in [
            ("<C->>", &["<C->>"][..]),
            ("<M->>", &["<M->>"][..]),
            ("<Bar>a", &["|", "a"][..]),
        ] {
            let mut model = claiming(&[Some(claim)]);
            let _ = type_keys(&mut model, typed);
            assert!(model.submit_hold.is_holding(), "{claim}");
        }
    }

    /// A key read as the argument of the one before it ends no line, an
    /// expression line ends back into the line beneath it, and `<C-\>`
    /// `<C-n>` leaves the line. `<C-\>` before any other key, and `<C-k>`
    /// before a special key, take no second key. A key leaves, submits or
    /// deletes by its base key, and `<C-c>` interrupts every argument but
    /// a literal one. Each row is what nvim 0.12's ext_cmdline reports for
    /// those keys: whether the `:` line is still open, and how many
    /// first-level ends went out.
    #[test]
    fn keys_taking_an_argument_end_the_line_only_where_nvim_does() {
        let rows: [(&[&str], bool, u32); 112] = [
            (&["<C-r>", "<CR>"], true, 0),
            (&["<C-r>", "<Esc>"], true, 0),
            (&["<C-r>", "<C-o>", "<CR>"], true, 0),
            (&["<C-v>", "<Esc>"], true, 0),
            (&["<C-q>", "<CR>"], true, 0),
            (&["<C-k>", "<CR>"], true, 0),
            (&["<C-k>", "a", "<Esc>"], true, 0),
            (&["<C-k>", "<Esc>", "<CR>"], false, 1),
            (&["<C-\\>", "<C-n>"], false, 1),
            (&["<C-\\>", "<C-g>"], false, 1),
            (&["<C-\\>", "x", "<CR>"], false, 1),
            (&["<C-\\>", "e", "1", "<CR>"], true, 0),
            (&["<C-r>", "=", "<C-r>", "<CR>"], true, 0),
            (&["<C-r>", "=", "<C-\\>", "<C-n>"], true, 0),
            (&["<C-r>", "=", "1", "<Esc>"], true, 0),
            (&["<C-r>", "=", "<M-x>"], true, 0),
            (&["<C-r>", "=", "1", "<CR>", "<CR>"], false, 1),
            (&["<C-\\>", "<CR>"], false, 1),
            (&["<C-\\>", "<Esc>"], false, 1),
            (&["<C-\\>", "<C-r>", "=", "1", "<CR>"], true, 0),
            (&["<C-\\>", "<M-x>"], true, 0),
            (&["<C-\\>", "<M-x>", "<CR>"], false, 1),
            (&["<C-k>", "<M-x>", "<CR>"], true, 0),
            (&["<C-r>", "=", "<C-\\>", "<M-x>", "<CR>"], true, 0),
            (&["<C-r>", "<C-p>", "<CR>"], false, 1),
            (&["<C-r>", "<C-o>", "<C-p>", "<CR>"], false, 1),
            (&["<C-k>", "<Left>", "<CR>"], false, 1),
            (&["<C-k>", "<BS>", "<CR>"], false, 1),
            (&["<C-k>", "<Tab>", "<CR>"], true, 0),
            (&["<C-k>", "<C-a>", "<CR>"], true, 0),
            (&["<C-k>", "<kEnter>", "<CR>"], true, 0),
            (&["<C-k>", "<S-CR>", "<CR>"], true, 0),
            (&["<C-k>", "<C-S-CR>", "<CR>"], true, 0),
            (&["<C-k>", "<C-CR>", "<CR>"], true, 0),
            (&["<C-k>", "<C-Tab>", "<CR>"], true, 0),
            (&["<C-k>", "<C-Space>", "<CR>"], true, 0),
            (&["<C-k>", "<D-1>", "<CR>"], true, 0),
            (&["<C-k>", "<k1>", "<CR>"], true, 0),
            (&["<C-k>", "<kPlus>", "<CR>"], true, 0),
            (&["<C-k>", "<kMinus>", "<CR>"], true, 0),
            (&["<C-k>", "<C-@>", "<CR>"], false, 1),
            (&["<C-k>", "<S-Tab>", "<CR>"], false, 1),
            (&["<C-k>", "<C-Left>", "<CR>"], false, 1),
            (&["<C-r>", "=", "<BS>", "<CR>"], false, 1),
            (&["<C-r>", "=", "1", "<BS>", "<BS>", "<CR>"], false, 1),
            (&["<C-r>", "=", "<C-r>", "=", "<BS>", "<CR>"], false, 1),
            (
                &["<C-r>", "=", "<C-\\>", "e", "<BS>", "<BS>", "<CR>"],
                true,
                0,
            ),
            (
                &["<C-r>", "=", "<C-v>", "x", "<BS>", "<BS>", "<CR>"],
                true,
                0,
            ),
            // a key leaves, submits or deletes by its base key
            (&["<C-k>", "<M-Esc>", "<CR>"], false, 1),
            (&["<C-k>", "<S-Esc>", "<CR>"], false, 1),
            (&["<C-k>", "<C-Esc>", "<CR>"], false, 1),
            (&["<C-k>", "<C-[>", "<CR>"], false, 1),
            (&["<C-k>", "<C-S-[>"], true, 0),
            (&["<C-\\>", "<M-Esc>"], false, 1),
            (&["<C-\\>", "<S-Esc>"], false, 1),
            (&["<C-\\>", "<M-CR>"], false, 1),
            (&["<C-\\>", "<S-CR>"], false, 1),
            (&["<C-\\>", "<C-CR>"], false, 1),
            (&["<C-r>", "<M-Esc>"], true, 0),
            (&["<C-v>", "<M-Esc>"], true, 0),
            (&["<S-CR>"], false, 1),
            (&["<C-CR>"], false, 1),
            (&["<C-S-CR>"], false, 1),
            (&["<S-NL>"], false, 1),
            (&["<S-kEnter>"], false, 1),
            (&["<C-M>"], false, 1),
            (&["<C-J>"], false, 1),
            (&["<S-Esc>"], false, 1),
            (&["<C-Esc>"], false, 1),
            (&["<C-[>"], false, 1),
            (&["<C-S-[>"], false, 1),
            (&["<C-S-c>"], false, 1),
            (&["a", "b", "<S-BS>", "<S-BS>", "<S-BS>"], false, 1),
            (&["a", "b", "<C-BS>", "<C-BS>", "<C-BS>"], false, 1),
            (&["a", "b", "<D-Del>", "<S-kDel>", "<C-S-BS>"], false, 1),
            (&["<C-kDel>"], false, 1),
            (&["<C-H>"], false, 1),
            (&["<S-Del>"], true, 0),
            (&["a", "b", "<C-S-Del>"], true, 0),
            (&["<C-r>", "=", "<S-Esc>", "<CR>"], false, 1),
            (&["<C-r>", "=", "<S-CR>", "<CR>"], false, 1),
            // `<C-c>` interrupts a key waiting for its argument, but a literal
            (&["<C-k>", "<C-c>"], false, 1),
            (&["<C-k>", "<C-C>"], false, 1),
            (&["<C-k>", "a", "<C-c>"], false, 1),
            (&["<C-k>", "<C-S-c>"], true, 0),
            (&["<C-r>", "<C-c>"], false, 1),
            (&["<C-r>", "<C-o>", "<C-c>"], false, 1),
            (&["<C-r>", "<C-S-c>"], true, 0),
            (&["<C-v>", "<C-c>"], true, 0),
            (&["<C-q>", "<C-c>"], true, 0),
            (&["<C-\\>", "<C-c>"], false, 1),
            (&["<C-r>", "=", "<C-c>"], true, 0),
            (&["<C-r>", "=", "<C-r>", "<C-c>"], false, 1),
            (&["<C-r>", "=", "<C-k>", "<C-c>"], true, 0),
            (&["<C-r>", "=", "<C-k>", "a", "<C-c>"], true, 0),
            (&["<C-r>", "=", "<C-\\>", "<C-c>"], true, 0),
            // a Meta key closes an expression line, then its base key is
            // typed into the line beneath
            (&["<C-r>", "=", "<M-Esc>"], false, 1),
            (&["<C-r>", "=", "<M-CR>"], false, 1),
            (&["<C-r>", "=", "<M-:>"], true, 0),
            // a Ctrl `m`, `j` or `h` submits or deletes with other modifiers
            (&["<C-S-m>"], false, 1),
            (&["<C-S-M>"], false, 1),
            (&["<C-S-j>"], false, 1),
            (&["<C-D-m>"], false, 1),
            (&["<C-T-m>"], false, 1),
            (&["<C-S-D-j>"], false, 1),
            (&["<C-S-h>"], false, 1),
            (&["<C-D-h>"], false, 1),
            (&["a", "b", "<C-S-h>", "<C-S-h>", "<C-S-h>"], false, 1),
            (&["<C-r>", "=", "<C-S-m>", "<CR>"], false, 1),
            (&["<C-r>", "=", "<C-S-h>", "<CR>"], false, 1),
            (&["<C-\\>", "<C-S-m>"], false, 1),
            (&["<C-k>", "<C-S-m>"], true, 0),
        ];
        let mismatches: Vec<String> = rows
            .into_iter()
            .filter_map(|(keys, open, ends)| {
                let mut model = normal_mode();
                let _ = type_keys(&mut model, &[":"]);
                let _ = type_keys(&mut model, keys);
                let hold = &model.submit_hold;
                let got = (hold.types_a_line(), hold.unhidden);
                let expected = (open, ends);
                (got != expected).then(|| format!("{keys:?} nvim {expected:?} model {got:?}"))
            })
            .collect();
        assert!(mismatches.is_empty(), "\n{}", mismatches.join("\n"));
    }
}
