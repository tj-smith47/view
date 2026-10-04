//! Input held behind a submitted `:View` command line, or a key nvim maps
//! to a view invocation, until view has run the invocation.
//!
//! nvim runs `:View ai open` or `<leader>ai` and only then tells view
//! about it, while the keys typed behind them are already on their way.
//! Routed as they arrive, they reach nvim as normal-mode commands in the
//! buffer the panel was opened from. Holding them until the invocation's
//! notification comes back lets the focus it sets decide where they go.
//!
//! Over a slow link three cases still send a query typed ahead into the
//! buffer as commands: keys that went out before an error from nvim came
//! back, an answer to an earlier key arriving later than the quickest
//! round trip seen, and a view key typed within one round trip of an
//! operator's motion (`dw<Space>ff`).
//!
//! A `gn` or `gN` with no search pattern needs no slow link: it stays in
//! normal mode, and a view key typed after it is not recognised until
//! nvim next reports a mode.
//!
//! A hold armed where nvim needed none keeps the keys for one bound, then
//! sends the same keys in the same order.

pub mod commands;
mod refused;
mod typed_ahead;
mod user_run;

use std::time::Duration;

pub(crate) use typed_ahead::owed_after;
pub(crate) use user_run::canonical_typed;

use commands::names_view;

use crate::events::UiEvent;
use crate::model::{CmdlineState, Model};
use crate::msg::{Effect, Msg};
use crate::native::keys::{canonical, key_tokens, modified, notation_char, Modified};
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
/// operators (nvim's default `gc` and the `zy` yank among them) wait for a
/// motion.
/// A `gn` or `gN` with no search pattern stays in normal mode, so no mode
/// report clears the doubt it raised, and a view key typed after it is
/// not recognised until the next mode change.
const LEAVES_NORMAL_AFTER: [(&str, &str); 20] = [
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
    ("z", "y"),
];

/// The builtin commands of three keys whose second key is read as the
/// argument of the first and still owes the key after it, each with the
/// name the third key is read as the argument of. `g'` and `` g` `` wait
/// for a mark, `gr` for the character it replaces with, `zu` for the
/// `zw` family's last key, and `<C-w>g` and `<C-w><C-g>` for the window
/// command they begin.
pub const OWES_AFTER: [(&str, &str, &str); 6] = [
    ("g", "'", "g'"),
    ("g", "`", "g`"),
    ("g", "r", "gr"),
    ("z", "u", "zu"),
    ("<C-w>", "g", "<C-w>g"),
    ("<C-w>", "<C-g>", "<C-w><C-g>"),
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
    /// A `<nowait>` mapping, which nvim runs without waiting for a longer
    /// lhs it begins.
    pub nowait: bool,
    /// Local to the current buffer, which nvim prefers over a global one
    /// with the same lhs.
    pub buffer: bool,
}

/// A [`CmdlineMap`] reduced to the text it matches on a line.
#[derive(Debug, Clone)]
struct Expansion {
    lhs: String,
    /// `None` for an expansion only nvim can compute.
    rhs: Option<Line>,
    abbr: bool,
    noremap: bool,
    nowait: bool,
}

/// A command line as nvim holds it after its mappings have run.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    text: String,
    /// Whether a `<CR>` the mappings put on the line submitted it.
    submits: bool,
    /// Whether a rhs typed `<C-u>` ahead of `text`, which erases what the
    /// line held before it.
    clears: bool,
}

impl Line {
    fn typed(text: &str) -> Self {
        Self {
            text: text.to_string(),
            submits: false,
            clears: false,
        }
    }
}

/// Which key nvim reads as the end of the command word when it looks for
/// an abbreviation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordEnd {
    /// The `<CR>` that submits the line.
    Submit,
    /// A non-keyword character typed after the word.
    Typed,
    /// Whichever of the two follows the word.
    Either,
}

/// A key waiting for nvim's mappings: a character, or `None` for a
/// `<CR>` a rhs holds, with whether nvim may map it.
type Pending = (Option<char>, bool);

/// What nvim's mappings do at the head of the keys it has not yet read.
enum Match<'a> {
    Unmapped,
    /// A longer lhs may still match once more keys arrive, so nvim waits.
    Waiting,
    /// A `<nowait>` lhs begins a longer one, so which runs depends on how
    /// many keys nvim has read at once.
    Unknowable,
    Map(&'a Expansion),
}

/// How many mappings run in a row with no key reaching the line. nvim's
/// own `'maxmapdepth'` bounds a recursive one at 1000 and then errors,
/// which runs no command.
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
    states: Vec<State>,
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
    /// The most keys any of `user_keys` spells, 0 where there is none.
    user_longest: usize,
    /// The key log's own reading of the keys this hold folds, which no
    /// decision of the hold reads.
    log: user_run::Matcher,
    /// The user's command-line mappings and abbreviations whose keys type
    /// text.
    cmdline_maps: Vec<Expansion>,
    /// The most characters any lhs in `cmdline_maps` that is a mapping
    /// spells, or 0 where there is none.
    longest_lhs: usize,
    /// The latest keys sent to nvim in normal mode, as many as the longest
    /// of `invoke_keys`.
    recent: std::collections::VecDeque<Folded>,
    /// The key whose argument nvim reads the next normal-mode key as: the
    /// key before it takes one, and was not itself an argument.
    argument_of: Option<&'static str>,
    /// Whether a key that leaves normal mode has gone out since nvim last
    /// reported a mode or drew anything a key makes it draw, so the mode
    /// view last read may be stale. Any answering batch clears it, one
    /// answering an earlier key included. A hold armed in error because of
    /// it ends on the mode report.
    mode_unsure: bool,
    /// Whether an `r` or a `gr` has gone out since nvim last reported
    /// normal mode. nvim reports replace mode for one once it reads it, and
    /// that report may arrive after a sequence typed behind it armed.
    replace_owed: bool,
    /// Whether an error from nvim has put the reading of the keys around it
    /// in doubt, and if so whether a key has gone out since the error.
    /// Every key folded while it stands spells a sequence whatever the
    /// keys before it.
    doubt: Option<bool>,
    /// When the first key sent after the error went to nvim, where the
    /// host stamped it.
    doubt_sent: Option<SpecStamp>,
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

/// One text the tracked line held.
#[derive(Debug, Clone)]
struct State {
    typed: String,
    /// What the user's command-line mappings made of `typed` where that
    /// differs from it. Expanded as the key that gave the line this text
    /// is folded, so the `<CR>` reading every state compares text only.
    mapped: Option<String>,
}

/// Whether nvim showing `shown` is showing keys still in flight: a text
/// `states` records, or what the user's mappings made of one.
fn in_flight(states: &[State], shown: &str) -> bool {
    states
        .iter()
        .any(|state| state.typed == shown || state.mapped.as_deref() == Some(shown))
}

/// One key sent to nvim in normal mode.
#[derive(Debug, Clone)]
struct Folded {
    key: String,
    /// Whether nvim read it as the argument of the key before it.
    argument: bool,
    /// Whether a key that leaves normal mode went out ahead of it, with no
    /// mode reported or key answered since.
    mode_unsure: bool,
    /// Whether it went out, or stood in the window, while an error put the
    /// two readings above in doubt.
    doubt: bool,
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
        self.log
            .learn_view(claims.iter().filter_map(|claim| claim.keys.as_ref()));
        self.recent.clear();
        self.sequence.clear();
    }

    /// Learns the key sequences the user's own config maps in normal mode,
    /// and how long nvim waits for the rest of one: `timeoutlen` is `None`
    /// where `'timeout'` is off. Keys a surface holds stay held, and the
    /// next key reads them against the new sequences.
    pub fn learn_user_keys(&mut self, keys: &[String], timeoutlen: Option<Duration>) {
        self.user_keys = keys
            .iter()
            .map(|keys| key_tokens(keys).map(canonical).collect::<Vec<_>>())
            .filter(|keys| !keys.is_empty())
            .collect();
        self.user_longest = self.user_keys.iter().map(Vec::len).max().unwrap_or(0);
        self.log.learn_user(&self.user_keys, timeoutlen);
        self.timeout_off = timeoutlen.is_none();
        self.timeoutlen = timeoutlen;
    }

    /// Learns the user's command-line mappings and abbreviations, which
    /// can make a line `:View` that no typed key spelled. A lhs holding a
    /// key that types no character never matches typed text and is left
    /// out.
    pub fn learn_cmdline_maps(&mut self, maps: &[CmdlineMap]) {
        let text =
            |spelled: &str| -> Option<String> { key_tokens(spelled).map(notation_char).collect() };
        let local = |map: &CmdlineMap| {
            maps.iter()
                .any(|other| other.buffer && other.abbr == map.abbr && other.lhs == map.lhs)
        };
        self.cmdline_maps = maps
            .iter()
            .filter(|map| map.buffer || !local(map))
            .filter_map(|map| {
                let lhs = text(&map.lhs).filter(|lhs| !lhs.is_empty())?;
                // the text ends at the first key in the rhs that types no
                // character, and a `<CR>` there submits the line. A `<C-u>`
                // erases the line up to the cursor, which view models at
                // the end of it
                let rhs = (!map.expr).then(|| {
                    let mut line = Line::typed("");
                    for key in key_tokens(&map.rhs) {
                        if canonical(key) == "<C-u>" {
                            line.text.clear();
                            line.clears = true;
                            continue;
                        }
                        let Some(c) = notation_char(key) else {
                            line.submits = line_key(key) == LineKey::Submit;
                            break;
                        };
                        line.text.push(c);
                    }
                    line
                });
                Some(Expansion {
                    lhs,
                    rhs,
                    abbr: map.abbr,
                    noremap: map.noremap,
                    nowait: map.nowait,
                })
            })
            .collect();
        self.longest_lhs = self
            .cmdline_maps
            .iter()
            .filter(|map| !map.abbr)
            .map(|map| map.lhs.chars().count())
            .max()
            .unwrap_or(0);
        let states = std::mem::take(&mut self.states);
        self.states = states
            .into_iter()
            .map(|state| self.state(state.typed))
            .collect();
    }

    /// `typed` as nvim puts it on the line once its command-line mappings
    /// have run, or `None` where an expansion only nvim can compute stands
    /// in the way. `submitted` says a `<CR>` follows, so a lhs that the
    /// keys so far begin can no longer match, where otherwise nvim waits
    /// for the keys still to come and the line holds none of them yet.
    ///
    /// nvim reads the keys left to right and maps wherever a lhs matches
    /// keys it may remap. A `noremap` rhs is not mapped again, and a rhs
    /// that begins with its own lhs leaves that first character unmapped
    /// and the rest remapped (`:help recursive_mapping`).
    fn expand_mappings(&self, typed: &str, submitted: bool) -> Option<Line> {
        if self.longest_lhs == 0 {
            return Some(Line::typed(typed));
        }
        #[cfg(test)]
        tests::EXPANSIONS.with(|count| count.set(count.get() + 1));
        let mut keys: std::collections::VecDeque<Pending> =
            typed.chars().map(|c| (Some(c), true)).collect();
        let mut text = String::new();
        let mut clears = false;
        let mut depth = 0;
        while let Some(&(key, remap)) = keys.front() {
            let Some(c) = key else {
                return Some(Line {
                    text,
                    submits: true,
                    clears,
                });
            };
            let map = match remap.then(|| self.mapping_at(&keys, submitted)) {
                Some(Match::Waiting) => break,
                Some(Match::Unknowable) => return None,
                Some(Match::Map(map)) => map,
                Some(Match::Unmapped) | None => {
                    text.push(c);
                    keys.pop_front();
                    depth = 0;
                    continue;
                }
            };
            let rhs = map.rhs.as_ref()?;
            depth += 1;
            if depth > MAP_DEPTH {
                return None;
            }
            keys.drain(..map.lhs.chars().count());
            if rhs.clears {
                text.clear();
                clears = true;
            }
            let own_lhs = rhs.text.starts_with(&map.lhs);
            let inserted: Vec<Pending> = rhs
                .text
                .chars()
                .map(Some)
                .chain(rhs.submits.then_some(None))
                .enumerate()
                .map(|(at, key)| (key, !map.noremap && !(at == 0 && own_lhs)))
                .collect();
            for key in inserted.into_iter().rev() {
                keys.push_front(key);
            }
        }
        Some(Line {
            text,
            submits: false,
            clears,
        })
    }

    /// The mapping nvim runs on the keys at the head of `keys`: the longest
    /// lhs they spell, once no longer lhs can still match. A lhs matches
    /// only keys nvim may remap, and no more keys are read than the longest
    /// lhs holds.
    ///
    /// A `<nowait>` lhs that begins a longer one is [`Match::Unknowable`].
    /// nvim runs it as soon as it is spelled when the keys arrive one read
    /// at a time, and runs the longer one when they were already typed.
    /// `nvim_input` queues each key at once, and while nvim is busy between
    /// keys (a `CmdlineChanged` handler) the keys waiting reach its
    /// typeahead in one read.
    fn mapping_at(&self, keys: &std::collections::VecDeque<Pending>, submitted: bool) -> Match<'_> {
        let head = || keys.iter().map_while(|&(key, remap)| key.filter(|_| remap));
        let mappings = || self.cmdline_maps.iter().filter(|map| !map.abbr);
        let mut found = None;
        let mut read = 0;
        for n in 1..=self.longest_lhs {
            if head().nth(n - 1).is_none() {
                break;
            }
            read = n;
            let spells = |map: &Expansion| map.lhs.chars().take(n).eq(head().take(n));
            let longer = mappings().any(|map| map.lhs.chars().nth(n).is_some() && spells(map));
            let mut whole =
                mappings().filter(|map| map.lhs.chars().nth(n).is_none() && spells(map));
            if whole.clone().any(|map| map.nowait) && longer {
                return Match::Unknowable;
            }
            found = whole.next().or(found);
            if !longer {
                return found.map_or(Match::Unmapped, Match::Map);
            }
        }
        if !submitted && read == keys.len() {
            return Match::Waiting;
        }
        found.map_or(Match::Unmapped, Match::Map)
    }

    /// `line` with its command word expanded as nvim expands an
    /// abbreviation, where `end` says which key after the word nvim has
    /// read. An abbreviation's rhs is mapped again unless it is a
    /// `noreabbrev`.
    fn expand_abbreviation(&self, line: Line, end: WordEnd) -> Line {
        let rest = line
            .text
            .trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        let start = line.text.len() - rest.len();
        let word_end = rest.find(|c: char| !is_keyword(c)).unwrap_or(rest.len());
        let at_line_end = word_end == rest.len();
        let triggered = match end {
            WordEnd::Submit => at_line_end,
            WordEnd::Typed => !at_line_end,
            WordEnd::Either => true,
        };
        let word = &rest[..word_end];
        let Some((map, rhs)) = self
            .cmdline_maps
            .iter()
            .filter(|_| triggered)
            .find(|map| map.abbr && map.lhs == word)
            .and_then(|map| Some((map, map.rhs.as_ref()?)))
        else {
            return line;
        };
        let rhs = match self.expand_mappings(&rhs.text, true) {
            Some(mapped) if !map.noremap => Line {
                submits: mapped.submits || rhs.submits,
                clears: mapped.clears || rhs.clears,
                text: mapped.text,
            },
            _ => rhs.clone(),
        };
        // the keys after a `<CR>` the rhs holds reach whatever runs next
        let after = if rhs.submits { "" } else { &rest[word_end..] };
        let before = if rhs.clears { "" } else { &line.text[..start] };
        Line {
            text: format!("{before}{}{after}", rhs.text),
            submits: line.submits || rhs.submits,
            clears: line.clears || rhs.clears,
        }
    }

    /// The line nvim holds for `typed`, its mappings run and then its
    /// abbreviations. `submitted` says the `<CR>` typed after it has gone
    /// out, where otherwise the keys after it are still to come.
    fn expand_typed(&self, typed: &str, submitted: bool) -> Line {
        let line = self
            .expand_mappings(typed, submitted)
            .unwrap_or_else(|| Line::typed(typed));
        let end = if submitted || line.submits {
            WordEnd::Either
        } else {
            WordEnd::Typed
        };
        self.expand_abbreviation(line, end)
    }

    /// `typed` recorded with the text nvim shows for it while keys after
    /// it are still on their way.
    fn state(&self, typed: String) -> State {
        let mapped = (self.longest_lhs > 0)
            .then(|| self.expand_mappings(&typed, false))
            .flatten()
            .map(|line| line.text)
            .filter(|mapped| *mapped != typed);
        State { typed, mapped }
    }

    /// Ends the tracked line when the key just typed into it completed a
    /// mapping or an abbreviation whose rhs submits it, and says whether
    /// that line runs `:View`.
    fn submit_by_mapping(&mut self) -> Option<bool> {
        let Some(Typed::Known(text)) = &self.typed else {
            return None;
        };
        let submitting = self
            .cmdline_maps
            .iter()
            .any(|map| map.rhs.as_ref().is_some_and(|rhs| rhs.submits));
        if !submitting {
            return None;
        }
        let line = self.expand_typed(text, false);
        if !line.submits {
            return None;
        }
        self.end_line(None);
        Some(names_view(&line.text))
    }

    /// Notes that nvim reported `mode`, which answers every key that left
    /// normal mode before it.
    pub fn note_mode_reported(&mut self, mode: &str) {
        self.mode_unsure = false;
        self.replace_owed &= mode != "normal";
        self.log.note_mode_reported(mode);
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

    /// Stamps a line end folded from the key now going to nvim at `now`,
    /// and the first key sent after an error that raised a doubt.
    pub(crate) fn note_key_sent(&mut self, now: SpecStamp) {
        if std::mem::take(&mut self.end_unsent) {
            self.ended_at = Some(now);
        }
        if self.doubt == Some(true) && self.doubt_sent.is_none() {
            self.doubt_sent = Some(now);
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
    /// ran no mapping. Replace mode is the one an `r` or a `gr` sent before
    /// the sequence reports, and ends nothing while one is owed.
    fn ended_by(&self, msg: &Msg) -> bool {
        let Some((armed, _)) = &self.held else {
            return false;
        };
        let leaves = |mode: &str| mode != "normal" && !(mode == "replace" && self.replace_owed);
        match msg {
            Msg::FeatureInvoke { .. } => true,
            Msg::SubmitHoldExpired { generation } => *generation == self.generation,
            Msg::Redraw(events) => {
                *armed == Armed::Sequence
                    && events.iter().any(
                        |event| matches!(event, UiEvent::ModeChange { mode, .. } if leaves(mode)),
                    )
            }
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
    /// The keys of the invocation behind it are dropped, logged or not, so
    /// a later invocation is never logged with them.
    pub fn take_held(&mut self) -> Vec<Msg> {
        let _ = self.log.take_invoked();
        self.held.take().map(|(_, held)| held).unwrap_or_default()
    }

    /// Notes a redraw batch nvim sent because it read a key. It clears the
    /// doubt over the mode whichever key it answered, an earlier one than
    /// the key that left normal mode included. A hold armed in error
    /// because of it ends on the mode report.
    ///
    /// It may end the doubt an error raised, as `typed_ahead::settle_doubt`
    /// states, the batch arriving at `now` with `shortest` the shortest
    /// round trip read.
    pub(crate) fn note_input_answered(&mut self, now: SpecStamp, shortest: Duration) {
        self.mode_unsure = false;
        typed_ahead::settle_doubt(self, now, shortest);
        self.log.note_answered();
    }

    /// Notes an error nvim answered a key with, and raises the doubt over
    /// every key in the window and every key folded until it ends. The
    /// error may answer any key still in flight, and whether a key before
    /// it left normal mode or took the next as its argument is a guess the
    /// error can falsify. The doubt covers every key until nothing is owed,
    /// so no argument is read as owed past the error.
    pub(crate) fn note_refused(&mut self) {
        self.doubt = Some(false);
        self.doubt_sent = None;
        for folded in &mut self.recent {
            folded.doubt = true;
        }
        self.argument_of = None;
    }

    /// Forgets which key nvim reads the next key as the argument of, and
    /// the mapping it waits on, where a click went out after it. A paste
    /// waits for both.
    pub fn forget_argument(&mut self) {
        self.argument_of = None;
        self.recent.clear();
        self.log.forget();
    }

    /// Drops every reading the key log holds, its learned keys included.
    #[cfg(test)]
    pub(crate) fn forget_log(&mut self) {
        self.log = user_run::Matcher::default();
    }

    /// The keys of the view invocation a key last completed, once.
    pub(crate) fn take_invoked(&mut self) -> Option<Vec<String>> {
        self.log.take_invoked()
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

    /// Tracks a `:` line view itself sends the engine, holding `text`, the
    /// way its keys would be tracked if typed. The line opens whatever mode
    /// view last read, since nvim reads it once the command that sent it
    /// has finished.
    pub(crate) fn open_line(&mut self, text: &str) {
        self.set_typed(Some(Typed::Known(String::new())));
        for c in text.chars() {
            if let Some(typed) = self.typed.as_mut() {
                typed.edit(&c.to_string());
            }
            self.note_edited();
        }
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
            Some(Typed::Known(_)) => in_flight(&self.states, &shown),
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
            let state = self.state(text.clone());
            self.states.push(state);
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
    let seen = user_run::Seen::of(model);
    user_run::fold(&mut model.submit_hold.log, seen, notation);
    for (keys, at) in model.submit_hold.log.take_fired() {
        model.key_log.log_user_mapping(&keys, at);
    }
    if typed_ahead::completes_invoke(model, notation) {
        model.submit_hold.set_typed(None);
        return arm(model, Armed::Sequence);
    }
    fold_line(model, notation)
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
        if hold.submit_by_mapping() == Some(true) {
            return arm(model, Armed::Command);
        }
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
    model.engine.literal_pending.is_none()
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
fn submits_view(model: &Model, opened: bool, typed: Option<&Typed>, states: &[State]) -> bool {
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
    let shown_view = |shown: &str| {
        names_view(
            &hold
                .expand_abbreviation(Line::typed(shown), WordEnd::Submit)
                .text,
        )
    };
    match (typed, shown) {
        (Some(Typed::Known(_)), Some(shown)) if opened && !in_flight(states, &shown) => {
            shown_view(&shown)
        }
        (Some(Typed::Known(text)), _) => names_view(&hold.expand_typed(text, true).text),
        (_, shown) => shown.is_some_and(|shown| shown_view(&shown)),
    }
}

/// Whether `msg` ends the standing hold: the command's own notification,
/// the bound it armed, a mode that says a key sequence ran no mapping, or
/// an error nvim reports behind the line that armed it. nvim runs nothing
/// on a line it refuses (a pattern that does not compile, a bad range, an
/// unknown command), so no notification follows the error. Only a
/// submitted line is read for one: a `<Cmd>` mapping leaves the mode
/// where it was, so nothing orders its error.
///
/// With no hold standing this is two `Option` tests. While a `:View`
/// line's hold stands, a redraw batch costs what
/// `refused::reports_error` states.
#[must_use]
pub fn releases(model: &Model, msg: &Msg) -> bool {
    let hold = &model.submit_hold;
    hold.ended_by(msg)
        || matches!(hold.held, Some((Armed::Command, _)))
            && matches!(msg, Msg::Redraw(events) if refused::reports_error(model, events))
}

/// Starts a hold, bounded by the link's own backstop so a command that
/// never reports back releases the keys to wherever focus stands.
fn arm(model: &mut Model, armed: Armed) -> Vec<Effect> {
    let hold = &mut model.submit_hold;
    if armed == Armed::Command {
        // a line typed by hand runs the next invocation, whatever key last
        // completed one nvim never ran
        let _ = hold.log.take_invoked();
    }
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

    thread_local! {
        /// How many times this thread has run the mapping expansion.
        pub(super) static EXPANSIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
        learn_maps(
            model,
            maps.iter()
                .map(|&(lhs, rhs, abbr, noremap, expr)| CmdlineMap {
                    lhs: lhs.to_string(),
                    rhs: rhs.to_string(),
                    abbr,
                    noremap,
                    expr,
                    nowait: false,
                    buffer: false,
                })
                .collect(),
        );
    }

    fn learn_maps(model: &mut Model, cmdline: Vec<CmdlineMap>) {
        let _ = crate::update::update(
            model,
            Msg::UserMappingsRead {
                keys: Vec::new(),
                timeoutlen: None,
                cmdline,
            },
        );
    }

    /// A `cnoremap` row with `nowait` and `buffer` set as given.
    fn flagged(lhs: &str, rhs: &str, nowait: bool, buffer: bool) -> CmdlineMap {
        CmdlineMap {
            lhs: lhs.to_string(),
            rhs: rhs.to_string(),
            abbr: false,
            noremap: true,
            expr: false,
            nowait,
            buffer,
        }
    }

    fn mapped(model: &Model, typed: &str) -> String {
        model.submit_hold.expand_typed(typed, true).text
    }

    /// A rhs ending in `<CR>` submits the line at the key that completes
    /// the lhs: a `:View` there arms the hold and holds the keys behind
    /// it, and any other command leaves no line tracked for a later
    /// `<CR>` to read.
    #[test]
    fn a_mapping_whose_rhs_submits_ends_the_line_at_its_last_key() {
        for abbr in [false, true] {
            let mut model = normal_mode();
            cmdline_maps(&mut model, &[("vv", "View ai open<CR>", abbr, true, false)]);
            let keys: &[&str] = if abbr {
                &[":", "v", "v", "<Space>"]
            } else {
                &[":", "v", "v"]
            };
            let sent = type_keys(&mut model, keys);
            assert!(arms(&sent), "abbr {abbr}: {sent:?}");
            let held = type_keys(&mut model, &["j"]);
            assert!(held.is_empty(), "abbr {abbr}: {held:?}");
            assert!(!model.submit_hold.types_a_line(), "abbr {abbr}");
        }

        let mut model = normal_mode();
        cmdline_maps(&mut model, &[("ww", "w<CR>", false, true, false)]);
        let sent = type_keys(&mut model, &[":", "w", "w"]);
        assert!(!arms(&sent), "{sent:?}");
        assert!(!model.submit_hold.types_a_line());
    }

    /// Mappings apply wherever their lhs is typed, as nvim reads the keys
    /// left to right: after a leading space and after a range.
    #[test]
    fn a_mapping_expands_wherever_its_keys_are_typed() {
        let mut model = normal_mode();
        cmdline_maps(&mut model, &[("vv", "View ai open", false, true, false)]);
        assert_eq!(mapped(&model, " vv"), " View ai open");
        assert_eq!(mapped(&model, "%vv"), "%View ai open");
        assert_eq!(
            mapped(&model, "echo vv|vv"),
            "echo View ai open|View ai open"
        );
        let sent = type_keys(&mut model, &[":", "<Space>", "v", "v", "<CR>"]);
        assert!(arms(&sent), "{sent:?}");
        // `:View` takes no range, so nvim refuses the line
        let mut model = normal_mode();
        cmdline_maps(&mut model, &[("vv", "View ai open", false, true, false)]);
        let sent = type_keys(&mut model, &[":", "%", "v", "v", "<CR>"]);
        assert!(!arms(&sent), "{sent:?}");
    }

    /// nvim maps the keys typed after a `noremap` rhs again.
    #[test]
    fn keys_after_a_noremap_rhs_are_mapped_again() {
        let mut model = normal_mode();
        cmdline_maps(
            &mut model,
            &[
                ("zz", "", false, true, false),
                ("vv", "View ai open", false, true, false),
            ],
        );
        assert_eq!(mapped(&model, "zzvv"), "View ai open");
    }

    /// A rhs that begins with its own lhs leaves that first character
    /// unmapped and maps the rest.
    #[test]
    fn a_rhs_beginning_with_its_lhs_maps_all_but_its_first_character() {
        let mut model = normal_mode();
        cmdline_maps(
            &mut model,
            &[
                ("v", "vw", false, false, false),
                ("w", "iew ai open", false, true, false),
            ],
        );
        assert_eq!(mapped(&model, "v"), "view ai open");
    }

    /// An abbreviation's rhs is mapped again, and a `noreabbrev`'s is not.
    #[test]
    fn an_abbreviation_rhs_is_mapped_unless_noreabbrev() {
        for (noremap, armed) in [(false, true), (true, false)] {
            let mut model = normal_mode();
            cmdline_maps(
                &mut model,
                &[
                    ("vo", "vv", true, noremap, false),
                    ("vv", "View ai open", false, true, false),
                ],
            );
            let sent = type_keys(&mut model, &[":", "v", "o", "<CR>"]);
            assert_eq!(arms(&sent), armed, "noremap {noremap}: {sent:?}");
        }
    }

    /// A `<nowait>` lhs that begins a longer one runs first or loses to it
    /// depending on how many keys nvim reads at once, so the expansion is
    /// left to nvim. Without `<nowait>` the longer one runs.
    #[test]
    fn a_nowait_mapping_beginning_a_longer_one_is_left_to_nvim() {
        for (nowait, expected) in [(true, None), (false, Some("echo"))] {
            let mut model = normal_mode();
            learn_maps(
                &mut model,
                vec![
                    flagged("vv", "View ai open", nowait, false),
                    flagged("vvx", "echo", false, false),
                ],
            );
            let line = model.submit_hold.expand_mappings("vvx", true);
            assert_eq!(
                line.map(|line| line.text).as_deref(),
                expected,
                "nowait {nowait}"
            );
        }
        // with no longer lhs it begins, a `<nowait>` lhs runs as any other
        let mut model = normal_mode();
        learn_maps(&mut model, vec![flagged("vv", "View ai open", true, false)]);
        assert_eq!(mapped(&model, "vvx"), "View ai openx");
    }

    /// `cnoremap vo <C-u>View ai open<CR>`: the `<C-u>` erases the line
    /// before the rest of the rhs is typed, so the line runs `:View`
    /// whatever was typed ahead of `vo`.
    #[test]
    fn a_rhs_clearing_the_line_runs_what_it_types_after() {
        for keys in [&[":", "v", "o"][..], &[":", "e", "<Space>", "v", "o"]] {
            let mut model = normal_mode();
            cmdline_maps(
                &mut model,
                &[("vo", "<C-u>View ai open<CR>", false, true, false)],
            );
            let sent = type_keys(&mut model, keys);
            assert!(arms(&sent), "{keys:?}: {sent:?}");
            let held = type_keys(&mut model, &["j"]);
            assert!(held.is_empty(), "{keys:?}: {held:?}");
        }
    }

    /// `:w|View ai open<CR>` runs `:View` as the second command.
    #[test]
    fn a_view_command_after_a_bar_arms_the_hold() {
        let mut model = normal_mode();
        let mut keys = vec![":", "w", "|"];
        keys.extend(["V", "i", "e", "w", "<Space>", "a", "i", "<CR>"]);
        let sent = type_keys(&mut model, &keys);
        assert!(arms(&sent), "{sent:?}");
        let held = type_keys(&mut model, &["j"]);
        assert!(held.is_empty(), "{held:?}");
    }

    /// A bare `:View` reopens the line holding `View `, and the keys typed
    /// on it are tracked as a typed line's are: the `<CR>` that submits it
    /// holds the keys behind it.
    #[test]
    fn keys_behind_a_reopened_view_line_are_held() {
        // nvim sends the invocation while it still runs the line that
        // named it, so the mode view last read may be the command line's
        for mode in ["normal", "cmdline_normal"] {
            let mut model = normal_mode();
            model.engine.mode.current = mode.to_string();
            keys_behind_a_reopened_line(model);
        }
    }

    fn keys_behind_a_reopened_line(mut model: Model) {
        let reopened = crate::update::update(
            &mut model,
            Msg::FeatureInvoke {
                generation: None,
                feature: String::new(),
                verb: String::new(),
            },
        );
        assert_eq!(inputs(&reopened), vec![":View ".to_string()]);
        assert!(model.submit_hold.types_a_line());
        let sent = type_keys(
            &mut model,
            &["a", "i", "<Space>", "o", "p", "e", "n", "<CR>"],
        );
        assert!(arms(&sent), "{sent:?}");
        let held = type_keys(&mut model, &["j"]);
        assert!(held.is_empty(), "{held:?}");
    }

    /// Ten command-line rows, one of them the `%%` that types the current
    /// file's directory through an expression only nvim can evaluate.
    const TEN_ROWS: [(&str, &str, bool, bool, bool); 10] = [
        ("%%", "<C-r>=expand('%:h').'/'<CR>", false, true, false),
        ("q1", "one", false, true, false),
        ("q2", "two", false, true, false),
        ("q3", "three", false, true, false),
        ("q4", "four", false, true, false),
        ("q5", "five", false, true, false),
        ("q6", "six", false, true, false),
        ("q7", "seven", false, true, false),
        ("q8", "eight", false, true, false),
        ("q9", "nine", false, true, false),
    ];

    /// The keys of the longest line view tracks: `e %%` and then letters,
    /// 255 characters in all.
    fn longest_line() -> Vec<String> {
        let mut keys: Vec<String> = ["e", "<Space>", "%", "%"].map(String::from).into();
        keys.extend((0..TRACKED_MAX - 5).map(|at| ((b'a' + (at % 26) as u8) as char).to_string()));
        keys
    }

    /// `rows` learned, the longest line typed and shown as `shown` makes
    /// of the text typed.
    fn typed_longest(rows: bool, shown: impl Fn(&Model, &str) -> String) -> Model {
        let mut model = normal_mode();
        if rows {
            cmdline_maps(&mut model, &TEN_ROWS);
        }
        let keys = longest_line();
        let typed: Vec<&str> = keys.iter().map(String::as_str).collect();
        let _ = type_keys(&mut model, &[":"]);
        show_line(&mut model, "");
        let _ = type_keys(&mut model, &typed);
        let text = "e %%".to_string() + &keys[4..].concat();
        let shown = shown(&model, &text);
        show_line(&mut model, &shown);
        model
    }

    /// nvim's line for the `%%` config: the expression typed the directory
    /// view cannot compute, so the line matches no text view recorded.
    fn expanded_by_nvim(_: &Model, text: &str) -> String {
        text.replacen("%%", "src/", 1)
    }

    /// The `<CR>` of the longest line view tracks runs no mapping
    /// expansion where the config maps nothing on the command line or
    /// where nvim shows a line no recorded text matches, and one where
    /// nvim shows what the mappings made of the newest text.
    #[test]
    fn a_submitted_line_expands_no_mapping_it_need_not() {
        type Shown = fn(&Model, &str) -> String;
        let newest_mapped: Shown = |model, text| model.submit_hold.expand_typed(text, false).text;
        let cases: [(bool, Shown, usize); 3] = [
            (false, expanded_by_nvim, 0),
            (true, expanded_by_nvim, 0),
            (true, newest_mapped, 1),
        ];
        for (at, (rows, shown, expected)) in cases.into_iter().enumerate() {
            let mut model = typed_longest(rows, shown);
            assert!(model.submit_hold.line_opened(), "case {at}");
            EXPANSIONS.with(|count| count.set(0));
            let sent = type_keys(&mut model, &["<CR>"]);
            assert!(!arms(&sent), "case {at}: {sent:?}");
            let expansions = EXPANSIONS.with(std::cell::Cell::get);
            assert_eq!(expansions, expected, "case {at}");
        }
    }

    /// The median of `runs` timings of `measured`, each taken on a fresh
    /// `prepared` model.
    fn median_of(
        runs: usize,
        prepared: impl Fn() -> Model,
        measured: impl Fn(&mut Model),
    ) -> Duration {
        let mut took: Vec<Duration> = (0..runs)
            .map(|_| {
                let mut model = prepared();
                let started = std::time::Instant::now();
                measured(&mut model);
                started.elapsed()
            })
            .collect();
        took.sort_unstable();
        took[runs / 2]
    }

    /// Under ten command-line rows, the last key of the longest line view
    /// tracks costs at most 8 times the last key of a 64-character line (the
    /// key expands its line once, about 4 times; an expansion per recorded
    /// text is about 16 times), and the `<CR>` of that line, shown as no
    /// recorded text matches, costs at most 15 times the `<CR>` of the same
    /// line with no rows. The side with rows compares each recorded text
    /// once, two string compares for each of up to `TRACKED_MAX` recorded
    /// states, so its ratio grows with the states recorded. The regressions
    /// the bound guards are an expansion per miss, about 40 times, and a
    /// re-read of every edit at the `<CR>`, over a thousand times. Each side
    /// is a median of the same run: the test runs in debug on three CI hosts
    /// beside the binary's other tests, and an absolute taken on one host is
    /// a flake on another.
    #[test]
    fn the_last_key_and_the_cr_of_a_mapped_line_scale_with_the_line() {
        const RUNS: usize = 21;
        const SHORT: usize = 64;
        let keys = longest_line();
        let last_key = |length: usize| {
            let (last, head) = (&keys[length - 1], &keys[..length - 1]);
            let before_last = || {
                let mut model = normal_mode();
                cmdline_maps(&mut model, &TEN_ROWS);
                let typed: Vec<&str> = head.iter().map(String::as_str).collect();
                let _ = type_keys(&mut model, &[":"]);
                show_line(&mut model, "");
                let _ = type_keys(&mut model, &typed);
                model
            };
            median_of(RUNS, before_last, |model| {
                let _ = type_keys(model, &[last]);
            })
        };
        let cr = |rows: bool| {
            median_of(
                RUNS,
                || typed_longest(rows, expanded_by_nvim),
                |model| {
                    let _ = type_keys(model, &["<CR>"]);
                },
            )
        };
        let (key_short, key_long) = (last_key(SHORT), last_key(keys.len()));
        let (cr_bare, cr_rows) = (cr(false), cr(true));
        let ratio = |over: Duration, under: Duration| over.as_secs_f64() / under.as_secs_f64();
        eprintln!(
            "median last key {key_short:?} at {SHORT}, {key_long:?} at {}; \
             median <CR> {cr_bare:?} with no rows, {cr_rows:?} with ten",
            keys.len()
        );
        assert!(
            ratio(key_long, key_short) <= 8.0,
            "the last key of a {}-character line took a median {key_long:?}, \
             against {key_short:?} at {SHORT} characters",
            keys.len()
        );
        assert!(
            ratio(cr_rows, cr_bare) <= 15.0,
            "the <CR> of the longest line under ten rows took a median {cr_rows:?}, \
             against {cr_bare:?} with no rows"
        );
    }

    /// A `:` line view opens itself is read as typed text until it ends:
    /// a claimed sequence its keys spell completes nothing, whatever mode
    /// nvim last reported.
    #[test]
    fn keys_on_a_reopened_line_complete_no_sequence() {
        let mut model = claiming(&[Some("<Space>p")]);
        let _ = crate::update::update(
            &mut model,
            Msg::FeatureInvoke {
                generation: None,
                feature: String::new(),
                verb: String::new(),
            },
        );
        let sent = type_keys(&mut model, &["<Space>", "p"]);
        assert!(!arms(&sent), "{sent:?}");
        assert!(model.submit_hold.types_a_line());
        let sent = type_keys(
            &mut model,
            &[
                "<BS>", "<BS>", "a", "i", "<Space>", "o", "p", "e", "n", "<CR>",
            ],
        );
        assert!(arms(&sent), "{sent:?}");
        let held = type_keys(&mut model, &["j"]);
        assert!(held.is_empty(), "{held:?}");
    }

    /// `:silent View ai open` and `:vert View ai open` run `:View`.
    #[test]
    fn a_view_command_behind_a_modifier_arms_the_hold() {
        for modifier in ["silent", "vert"] {
            let mut model = normal_mode();
            let mut keys = vec![":".to_string()];
            let line = format!("{modifier} View ai open");
            keys.extend(line.chars().map(|c| match c {
                ' ' => "<Space>".to_string(),
                c => c.to_string(),
            }));
            keys.push("<CR>".to_string());
            let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            let sent = type_keys(&mut model, &keys);
            assert!(arms(&sent), "{modifier}: {sent:?}");
            let held = type_keys(&mut model, &["j"]);
            assert!(held.is_empty(), "{modifier}: {held:?}");
        }
    }

    /// A buffer-local row wins over a global one with the same lhs,
    /// whichever order `maplist()` gave them in.
    #[test]
    fn a_buffer_local_mapping_wins_over_a_global_one() {
        let local = flagged("vv", "View ai open", false, true);
        let global = flagged("vv", "vsplit", false, false);
        for rows in [vec![local.clone(), global.clone()], vec![global, local]] {
            let mut model = normal_mode();
            learn_maps(&mut model, rows);
            assert_eq!(mapped(&model, "vv"), "View ai open");
        }
    }

    /// A first show equal to an earlier typed text's expansion is keys
    /// still in flight, so the keys typed after it decide: `:vv` shown as
    /// `View ai open`, then erased and replaced within one round trip.
    #[test]
    fn a_shown_expansion_of_an_earlier_text_is_keys_in_flight() {
        let mut model = normal_mode();
        cmdline_maps(&mut model, &[("vv", "View ai open", false, true, false)]);
        let _ = type_keys(&mut model, &[":", "v", "v"]);
        let _ = type_keys(&mut model, &["<BS>"; 2]);
        let _ = type_keys(&mut model, &["e", "<Space>", "f"]);
        show_line(&mut model, "View ai open");
        assert!(model.submit_hold.line_opened());
        let sent = type_keys(&mut model, &["<CR>"]);
        assert!(!arms(&sent), "{sent:?}");
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
                generation: None,
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

    const REFUSED: [&str; 17] = [
        ":", "f", "i", "l", "t", "e", "r", "<Space>", "/", "<Bslash>", "(", "/", "<Space>", "V",
        "i", "e", "w",
    ];

    /// A model attached with `surfaces` that has submitted
    /// `:filter /\(/ View`, a line nvim refuses with E54, and is holding
    /// the keys typed behind it.
    fn refused_line(surfaces: Vec<crate::native::ext::Ext>, behind: &[&str]) -> Model {
        let mut model = normal_mode();
        model.attach_surfaces(surfaces);
        let _ = type_keys(&mut model, &REFUSED);
        let _ = type_keys(&mut model, &["<CR>"]);
        assert!(model.submit_hold.is_holding());
        let held = type_keys(&mut model, behind);
        assert!(held.is_empty(), "{held:?}");
        model
    }

    fn message(kind: &str, text: &str) -> UiEvent {
        UiEvent::MsgShow {
            kind: kind.to_string(),
            content: vec![(25, text.to_string())],
            replace_last: false,
        }
    }

    /// An error nvim reports for the submitted line releases the keys held
    /// behind it in that update, and the update does nothing else a model
    /// holding no keys would not do.
    #[test]
    fn an_error_on_the_submitted_line_releases_the_keys_at_once() {
        use crate::native::ext::Ext;
        let surfaces = || vec![Ext::LineGrid, Ext::Cmdline, Ext::Messages];
        let batch = || {
            Msg::Redraw(vec![
                UiEvent::CmdlineHide { level: 1 },
                message("emsg", "E54: Unmatched \\("),
                UiEvent::ModeChange {
                    mode: "normal".to_string(),
                    mode_idx: 0,
                },
            ])
        };
        let mut held = refused_line(surfaces(), &["j", "k"]);
        let mut empty = refused_line(surfaces(), &[]);
        let released = crate::update::update(&mut held, batch());
        let alone = crate::update::update(&mut empty, batch());
        assert_eq!(inputs(&released), ["j", "k"], "{released:?}");
        assert!(!held.submit_hold.is_holding());
        let rest: Vec<_> = released
            .iter()
            .filter(|e| !matches!(e, Effect::Rpc(crate::msg::RpcCall::Input { .. })))
            .map(|e| format!("{e:?}"))
            .collect();
        let alone: Vec<_> = alone.iter().map(|e| format!("{e:?}")).collect();
        assert_eq!(rest, alone);
    }

    fn mode(mode: &str) -> UiEvent {
        UiEvent::ModeChange {
            mode: mode.to_string(),
            mode_idx: 0,
        }
    }

    fn shown(firstc: &str, text: &str) -> UiEvent {
        UiEvent::CmdlineShow {
            content: vec![(0, text.to_string())],
            pos: 0,
            firstc: firstc.to_string(),
            prompt: String::new(),
            indent: 0,
            level: 1,
        }
    }

    /// A model attached with `surfaces` that has submitted `ahead` and
    /// then `:View ai open`, and is holding a `j` typed behind it.
    fn typed_ahead(surfaces: Vec<crate::native::ext::Ext>, ahead: &[&str]) -> Model {
        let mut model = normal_mode();
        model.attach_surfaces(surfaces);
        let _ = type_keys(&mut model, ahead);
        let view = [
            ":", "V", "i", "e", "w", " ", "a", "i", " ", "o", "p", "e", "n", "<CR>", "j",
        ];
        let _ = type_keys(&mut model, &view);
        assert!(model.submit_hold.is_holding());
        model
    }

    /// A message that is no error releases nothing, and neither does an
    /// error nvim follows with a mode change into the command line, which
    /// says it is reading the next line and the error was an earlier
    /// line's. The batches are nvim 0.12.4's for keys typed inside one
    /// round trip.
    #[test]
    fn a_message_that_is_no_error_on_the_line_releases_nothing() {
        use crate::native::ext::Ext;
        let surfaces = || vec![Ext::LineGrid, Ext::Cmdline, Ext::Messages];
        let mut model = refused_line(surfaces(), &["j"]);
        let sent = crate::update::update(
            &mut model,
            Msg::Redraw(vec![
                UiEvent::CmdlineHide { level: 1 },
                message("echo", "hello"),
                mode("normal"),
            ]),
        );
        assert!(inputs(&sent).is_empty(), "{sent:?}");
        assert!(model.submit_hold.is_holding());

        let bogus = [":", "b", "o", "g", "u", "s", "<CR>"];
        let search = ["/", "(", "<CR>"];
        let cases = [
            // two flushes drained together, the second sent after the
            // notification the pump has not yet delivered
            (
                "E492 ahead, drained with the View line's end",
                surfaces(),
                &bogus[..],
                vec![
                    UiEvent::CmdlineHide { level: 1 },
                    message("emsg", "E492: Not an editor command: bogus"),
                    shown(":", "View ai open"),
                    mode("cmdline_normal"),
                    UiEvent::CmdlineHide { level: 1 },
                    mode("normal"),
                ],
            ),
            (
                "E492 ahead, messages and the command line",
                surfaces(),
                &bogus[..],
                vec![
                    UiEvent::CmdlineHide { level: 1 },
                    message("emsg", "E492: Not an editor command: bogus"),
                    shown(":", "View ai open"),
                    mode("cmdline_normal"),
                ],
            ),
            (
                "E492 ahead, messages alone",
                vec![Ext::LineGrid, Ext::Messages],
                &bogus[..],
                vec![
                    UiEvent::CmdlineHide { level: 1 },
                    message("emsg", "E492: Not an editor command: bogus"),
                    shown(":", "View ai open"),
                    mode("cmdline_normal"),
                ],
            ),
            (
                "E486 behind a search hide",
                surfaces(),
                &search[..],
                vec![
                    UiEvent::CmdlineHide { level: 1 },
                    message("search_cmd", "/("),
                    mode("normal"),
                    message("emsg", "E486: Pattern not found: ("),
                    shown(":", "View ai open"),
                    mode("cmdline_normal"),
                ],
            ),
        ];
        let mut released = Vec::new();
        for (name, surfaces, ahead, batch) in cases {
            let mut model = typed_ahead(surfaces, ahead);
            let sent = crate::update::update(&mut model, Msg::Redraw(batch));
            if !inputs(&sent).is_empty() || !model.submit_hold.is_holding() {
                released.push(name);
            }
        }
        assert!(released.is_empty(), "released early: {released:?}");
    }

    /// Where nvim draws the whole screen, a diagnostic sign drawn at column
    /// 0 in `ErrorMsg`'s own id, before the valid line's mode change into
    /// the command line, releases nothing.
    #[test]
    fn an_error_sign_on_the_screen_releases_nothing() {
        use crate::native::ext::Ext;
        let mut model = normal_mode();
        model.attach_surfaces(vec![Ext::LineGrid]);
        let _ = crate::update::update(&mut model, error_highlights(None));
        let view = [
            ":", "V", "i", "e", "w", " ", "a", "i", " ", "o", "p", "e", "n", "<CR>", "j",
        ];
        let _ = type_keys(&mut model, &view);
        assert!(model.submit_hold.is_holding());
        let sent = crate::update::update(
            &mut model,
            Msg::Redraw(vec![
                UiEvent::GridLine {
                    grid: 1,
                    row: 7,
                    col_start: 0,
                    cells: vec![cell("E", 25), cell(" ", 25), cell("x", 0)],
                },
                UiEvent::GridLine {
                    grid: 1,
                    row: 9,
                    col_start: 0,
                    cells: vec![cell(":", 1), cell("V", 1)],
                },
                mode("cmdline_normal"),
            ]),
        );
        assert!(inputs(&sent).is_empty(), "{sent:?}");
        assert!(model.submit_hold.is_holding());
    }

    fn cell(text: &str, hl_id: u64) -> crate::events::GridCell {
        crate::events::GridCell {
            text: text.to_string(),
            hl_id,
            repeat: 1,
        }
    }

    fn define(id: u64, fg: Option<u32>, bg: Option<u32>) -> UiEvent {
        UiEvent::HlAttrDefine {
            id,
            fg,
            bg,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        }
    }

    /// The 10-row screen, and the ids and colours nvim 0.12.4 defines for
    /// `ErrorMsg` and `MsgArea`, with `area_bg` as the message area's
    /// background.
    fn error_highlights(area_bg: Option<u32>) -> Msg {
        Msg::Redraw(vec![
            UiEvent::GridResize {
                grid: 1,
                width: 80,
                height: 10,
            },
            define(1, None, None),
            define(25, Some(0xff_c0b9), None),
            define(59, Some(0xaa_aaaa), area_bg),
            UiEvent::HlGroupSet {
                name: "ErrorMsg".to_string(),
                hl_id: 25,
            },
            UiEvent::HlGroupSet {
                name: "MsgArea".to_string(),
                hl_id: if area_bg.is_some() { 59 } else { 1 },
            },
        ])
    }

    /// Where nvim draws its own messages, the error is the message row
    /// opening in `ErrorMsg`, laid over whatever `MsgArea` holds, and
    /// then the mode leaving the command line.
    #[test]
    fn an_error_nvim_draws_itself_releases_the_keys() {
        use crate::native::ext::Ext;
        let line = |hl_id: u64| UiEvent::GridLine {
            grid: 1,
            row: 9,
            col_start: 0,
            cells: vec![cell("E", hl_id), cell("5", hl_id), cell("4", hl_id)],
        };
        // the ids and colours nvim 0.12.4 sends with and without a
        // `MsgArea` background
        for (area_bg, drawn, released) in [
            (None, line(25), true),
            (Some(0x22_2222), line(60), true),
            (None, line(1), false),
        ] {
            let mut model = normal_mode();
            model.attach_surfaces(vec![Ext::LineGrid]);
            let _ = crate::update::update(&mut model, error_highlights(area_bg));
            let _ = type_keys(&mut model, &REFUSED);
            let _ = type_keys(&mut model, &["<CR>", "j"]);
            assert!(model.submit_hold.is_holding());
            let sent = crate::update::update(
                &mut model,
                Msg::Redraw(vec![
                    define(60, Some(0xff_c0b9), area_bg),
                    drawn,
                    mode("normal"),
                ]),
            );
            let expected: &[&str] = if released { &["j"] } else { &[] };
            assert_eq!(inputs(&sent), expected, "MsgArea bg {area_bg:?}");
            assert_eq!(model.submit_hold.is_holding(), !released);
        }
    }

    /// Where the message row already shows an error, nvim starts the next
    /// one at column 1, and the line counts when column 0 is still drawn
    /// as an error: as the grid holds it, or as a line earlier in the
    /// batch draws it. The batches are nvim 0.12.4's for `:bogus<CR>` and
    /// then `:filter /\(/ View`.
    #[test]
    fn an_error_nvim_diffs_against_the_one_shown_releases_the_keys() {
        use crate::native::ext::Ext;
        let e54 = || UiEvent::GridLine {
            grid: 1,
            row: 9,
            col_start: 1,
            cells: vec![cell("5", 25), cell("4", 25), cell(":", 25)],
        };
        let row_opens = |text: &str, hl_id: u64| UiEvent::GridLine {
            grid: 1,
            row: 9,
            col_start: 0,
            cells: vec![cell(text, hl_id)],
        };
        let cases = [
            (
                "E492 on the grid",
                Some(25),
                vec![e54(), mode("normal")],
                true,
            ),
            ("a blank row", Some(1), vec![e54(), mode("normal")], false),
            // the line typed ahead, drained in the same batch
            (
                "E492 earlier in the batch",
                None,
                vec![
                    UiEvent::CmdlineHide { level: 1 },
                    row_opens("E", 25),
                    shown(":", "filter /\\(/ View"),
                    mode("cmdline_normal"),
                    UiEvent::CmdlineHide { level: 1 },
                    e54(),
                    mode("normal"),
                ],
                true,
            ),
            (
                "a row the batch blanked",
                Some(25),
                vec![row_opens(" ", 1), e54(), mode("normal")],
                false,
            ),
            (
                "a grid the batch cleared",
                Some(25),
                vec![UiEvent::GridClear { grid: 1 }, e54(), mode("normal")],
                false,
            ),
        ];
        for (name, on_grid, batch, released) in cases {
            let mut model = normal_mode();
            model.attach_surfaces(vec![Ext::LineGrid, Ext::Cmdline]);
            let _ = crate::update::update(&mut model, error_highlights(None));
            if let Some(hl_id) = on_grid {
                let _ = crate::update::update(&mut model, Msg::Redraw(vec![row_opens("E", hl_id)]));
            }
            let _ = type_keys(&mut model, &REFUSED);
            let _ = type_keys(&mut model, &["<CR>", "j"]);
            assert!(model.submit_hold.is_holding(), "{name}");
            let sent = crate::update::update(&mut model, Msg::Redraw(batch));
            let expected: &[&str] = if released { &["j"] } else { &[] };
            assert_eq!(inputs(&sent), expected, "{name}");
            assert_eq!(model.submit_hold.is_holding(), !released, "{name}");
        }
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
            generation: None,
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
