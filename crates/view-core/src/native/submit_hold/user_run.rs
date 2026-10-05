//! The key log's reading of the keys sent to nvim: which of the user's own
//! normal-mode mappings they fired and which of view's invocations they
//! completed. It keeps every piece of its state to itself and is fed the
//! keys and events the typed-ahead hold is fed, which reads none of it.
//!
//! Every key sent to nvim costs two scans of the eight recent key round
//! trips, for how long nvim takes to report a mode and how far its clock
//! may read a gap from view's. That is the whole cost while nvim is in
//! another mode, or while neither the user nor view maps anything.
//! Otherwise a key also costs a few binary searches over the user's sorted
//! mappings and view's sorted invoking sequences, each comparing at most
//! the window's length of keys, once per suffix of the window, and a step
//! that returns at once while the user maps nothing. A key allocates its
//! canonical spelling, which the window keeps, and a firing mapping
//! allocates its keys.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use super::{canonical, LEAVES_NORMAL};
use crate::model::Model;

/// The keys that leave normal mode as the argument of the key before
/// them: `gi`, `gv` and `gn` enter insert and visual, and the `g` and `z`
/// operators (nvim's default `gc` and the `zy` yank among them) wait for a
/// motion.
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

/// Whether `key`, typed as the argument of `before` or of nothing, leaves
/// normal mode.
fn leaves_normal(before: Option<&str>, key: &str) -> bool {
    match before {
        None => LEAVES_NORMAL.contains(&key),
        Some(before) => LEAVES_NORMAL_AFTER.contains(&(before, key)),
    }
}

/// One key sent to nvim in normal mode, as the log reads it.
#[derive(Debug, Clone)]
pub(crate) struct Folded {
    key: String,
    /// Whether nvim read it as the argument of the key before it.
    argument: bool,
    /// Whether a key that leaves normal mode went out ahead of it, with no
    /// mode reported since.
    mode_unsure: bool,
    /// Whether one of the user's mappings fired ahead of it with no answer
    /// from nvim since, so its rhs may have left normal mode.
    mapped_unsure: bool,
    /// Whether nvim reads it as part of an operator's motion.
    operand: bool,
}

/// Where the gap before a key falls against nvim's `'timeoutlen'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gap {
    /// Inside it, or no run to time out.
    Within,
    /// Close enough to it that nvim's clock may have read it either way.
    Close,
    /// Past it on either clock.
    Past,
}

/// Every normal-mode key sequence after which nvim reads the next keys as
/// an operator's motion, where a normal-mode mapping does not apply: nvim's
/// own operators and its default `gc` comment operator.
pub(crate) const OPERATORS: [&str; 16] = [
    "d", "y", "c", "<", ">", "!", "=", "g~", "gu", "gU", "g?", "gq", "gw", "g@", "zf", "gc",
];

/// `keys`, one notation each as view's input spells them, made
/// [`canonical`].
pub(crate) fn canonical_typed(keys: &[String]) -> Vec<String> {
    keys.iter().map(|key| canonical(key)).collect()
}

/// Motion keys that read one more key as part of the same motion: a text
/// object, a `g`/`z` motion, a bracket motion, a mark jump or a find.
const MOTION_PREFIXES: [&str; 12] = ["i", "a", "g", "z", "[", "]", "'", "`", "f", "t", "F", "T"];

/// Where nvim stands against the operators, read off the keys alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Operator {
    /// No operator waits.
    #[default]
    Idle,
    /// The first key of a two-key operator went out.
    Prefix(char),
    /// An operator waits for its motion, or for a count before it, and
    /// whether a count has begun, after which `0` is one of its digits.
    Pending(bool),
    /// The motion's first key went out and reads one more.
    MotionArgument,
}

/// Where the latest keys stand against the user's mappings.
#[derive(Debug, Clone, Default)]
pub(crate) struct UserRun {
    /// The most keys any of the user's mappings spells, 0 where none.
    longest: usize,
    /// View's invoking sequences, sorted, which nvim waits on as it waits
    /// on the user's longer mappings.
    view: Vec<Vec<String>>,
    /// The most keys any of `view` spells.
    view_longest: usize,
    /// How long nvim waits for the rest of a mapping, `None` where
    /// `'timeout'` is off and it waits for the next key however long.
    wait: Option<Duration>,
    /// When the latest key read in normal mode was pressed.
    last: Option<SystemTime>,
    /// Where in the window the keys after the latest fired mapping begin,
    /// until taken.
    fired_end: Option<usize>,
    /// How many of the latest keys begin, or spell, a mapping.
    run: usize,
    /// The length of a whole mapping inside the run that a longer one
    /// begins with, which nvim runs once the next key parts from the
    /// longer one, and when its last key was pressed.
    pending: Option<(usize, SystemTime)>,
    /// The keys of each mapping the latest key fired and when its last key
    /// was pressed, until taken.
    fired: Vec<(Vec<String>, SystemTime)>,
    /// The keys of view's invocation the latest key completed, until taken.
    invoked: Option<Vec<String>>,
    operator: Operator,
}

impl UserRun {
    /// Sorts and dedups `keys` in place so a run can be searched for, and
    /// learns how long nvim waits for the rest of one, `None` where it
    /// waits for good. The run in progress is read on against the new
    /// keys, since a read lands between any two keys, and is forgotten
    /// where the new keys leave nothing to read it against.
    pub(crate) fn learn_user(&mut self, keys: &mut Vec<Vec<String>>, wait: Option<Duration>) {
        keys.sort();
        keys.dedup();
        self.longest = keys.iter().map(Vec::len).max().unwrap_or(0);
        self.wait = wait;
        // a run no read key can extend any more, or one longer than the
        // window keeps, points past the keys it was read from
        if self.longest == 0 || self.run > self.window() {
            self.reset();
        }
    }

    /// Learns view's invoking sequences, each spelled as `keytrans()`
    /// writes it.
    pub(crate) fn learn_view<'a>(&mut self, keys: impl Iterator<Item = &'a String>) {
        self.view = keys
            .map(|spelled| crate::native::keys::canonical_keys(spelled))
            .filter(|keys| !keys.is_empty())
            .collect();
        self.view.sort();
        self.view_longest = self.view.iter().map(Vec::len).max().unwrap_or(0);
        self.reset();
    }

    /// How many recent keys the window keeps: the most any mapping, the
    /// user's or view's, spells. 0 where neither maps anything.
    pub(crate) const fn window(&self) -> usize {
        if self.longest > self.view_longest {
            self.longest
        } else {
            self.view_longest
        }
    }

    /// Forgets the run, for keys that left normal mode or ran a mapping.
    pub(crate) fn reset(&mut self) {
        self.run = 0;
        self.pending = None;
        self.operator = Operator::Idle;
    }

    /// Forgets the run for a click, which nvim reads as the end of whatever
    /// mapping it waits on. Answers whether a whole mapping was pending,
    /// which the click ran.
    pub(crate) fn forget(&mut self) -> bool {
        let pending = self.pending.is_some();
        self.reset();
        pending
    }

    /// Where in `recent` the keys after the latest fired mapping begin,
    /// once.
    pub(crate) fn take_fired_end(&mut self) -> Option<usize> {
        self.fired_end.take()
    }

    /// Reads the gap between the latest key and one pressed at `now`
    /// against nvim's `'timeoutlen'`, answering where it falls. Past it,
    /// nvim gave up on the run: a whole mapping pending inside it ran then,
    /// so it fires stamped with its own last key's time, and the rest of
    /// the run went to nvim as typed keys and is matched no further.
    ///
    /// nvim reads the gap on its own clock, which may differ from view's by
    /// up to `margin`. A gap that close to `'timeoutlen'` ends the run with
    /// no row, and a whole mapping pending in it is one nvim may have run
    /// unseen.
    pub(crate) fn time_out(
        &mut self,
        recent: &mut VecDeque<Folded>,
        now: SystemTime,
        margin: Duration,
    ) -> Gap {
        let last = self.last.replace(now);
        let gap = last.and_then(|last| now.duration_since(last).ok());
        let Some((wait, gap)) = self.wait.zip(gap).filter(|_| self.run > 0) else {
            return Gap::Within;
        };
        if gap.saturating_add(margin) <= wait {
            return Gap::Within;
        }
        let start = recent.len().saturating_sub(self.run);
        let past = gap > wait.saturating_add(margin);
        match self.pending.take() {
            Some((pending, at)) if past => {
                self.fire(recent, start, pending, at);
                self.refold(recent, start + pending);
            }
            // the keys after the pending mapping are read for a mode they
            // left, as on the path where it fires
            Some((pending, _)) => self.fired_end = Some(start + pending),
            None => {}
        }
        self.run = 0;
        if past {
            Gap::Past
        } else {
            Gap::Close
        }
    }

    /// How long before `now` the latest key was pressed.
    pub(crate) fn since_last(&self, now: SystemTime) -> Option<Duration> {
        self.last.and_then(|last| now.duration_since(last).ok())
    }

    /// Reads the keys of `recent` from `from` against the operators again
    /// from no operator, for keys a fired mapping left behind it, whose
    /// operator state was read with the mapping's own keys in it.
    fn refold(&mut self, recent: &mut VecDeque<Folded>, from: usize) {
        self.operator = Operator::Idle;
        for folded in recent.range_mut(from..) {
            folded.operand = self.operand(&folded.key, folded.argument);
        }
    }

    /// Where the latest keys of `recent` complete one of view's invoking
    /// sequences, typed as a command and spelling none of `user`, the
    /// user's own mappings: a buffer's own mapping on the same keys is the
    /// one nvim runs.
    pub(crate) fn completed(
        &self,
        recent: &VecDeque<Folded>,
        user: &[Vec<String>],
    ) -> Option<usize> {
        (0..recent.len()).find(|&start| {
            recent.get(start).is_some_and(|first| !first.argument)
                && whole(recent, start, &self.view)
                && !whole(recent, start, user)
        })
    }

    /// Forgets the run behind `keys`, which completed one of view's
    /// invocations, and keeps them for the row that invocation logs.
    pub(crate) fn invoked(&mut self, keys: Vec<String>) {
        self.reset();
        self.invoked = Some(keys);
    }

    /// The keys of the view invocation a key last completed, once.
    pub(crate) fn take_invoked(&mut self) -> Option<Vec<String>> {
        self.invoked.take()
    }

    /// Each mapping the latest key fired and when its last key was
    /// pressed, once.
    pub(crate) fn take_fired(&mut self) -> Vec<(Vec<String>, SystemTime)> {
        std::mem::take(&mut self.fired)
    }

    /// Reads `key`, an `argument` of the key before it or not, against the
    /// operators, answering whether nvim reads it as part of an operator's
    /// motion.
    pub(crate) fn operand(&mut self, key: &str, argument: bool) -> bool {
        let (operand, next) = match self.operator {
            Operator::Idle if argument => (false, Operator::Idle),
            Operator::Idle if OPERATORS.contains(&key) => (false, Operator::Pending(false)),
            Operator::Idle => match key {
                "g" => (false, Operator::Prefix('g')),
                "z" => (false, Operator::Prefix('z')),
                _ => (false, Operator::Idle),
            },
            Operator::Prefix(first) => {
                let pair = OPERATORS
                    .iter()
                    .any(|operator| operator.strip_prefix(first) == Some(key));
                let next = if pair {
                    Operator::Pending(false)
                } else {
                    Operator::Idle
                };
                (false, next)
            }
            Operator::Pending(counting) => {
                // `0` is the start-of-line motion until a count has begun
                let digit = key.len() == 1
                    && key.bytes().all(|b| b.is_ascii_digit())
                    && (counting || key != "0");
                let next = if digit {
                    Operator::Pending(true)
                } else if matches!(key, "v" | "V" | "<C-v>") {
                    Operator::Pending(counting)
                } else if MOTION_PREFIXES.contains(&key) {
                    Operator::MotionArgument
                } else {
                    Operator::Idle
                };
                (true, next)
            }
            Operator::MotionArgument => (true, Operator::Idle),
        };
        self.operator = next;
        operand
    }

    /// Reads the key just pushed onto `recent`, pressed at `now`, against
    /// `keys`, the user's mappings as [`Self::learn_user`] sorted them.
    ///
    /// A run that parts from every mapping with a whole one pending inside
    /// it fires that mapping, and the keys after it are matched no
    /// further: its rhs may have left normal mode. A run with none pending
    /// is read again from each of its later suffixes, longest first, since
    /// nvim runs its first key unmapped and matches the rest from the
    /// start.
    pub(crate) fn step(
        &mut self,
        recent: &mut VecDeque<Folded>,
        keys: &[Vec<String>],
        now: SystemTime,
    ) {
        let len = recent.len();
        if self.longest == 0 || len == 0 {
            return;
        }
        let run = (self.run + 1).min(len);
        self.pending = self.pending.filter(|(pending, _)| *pending < run);
        if self.extend(recent, keys, run, now) {
            return;
        }
        self.run = 0;
        if let Some((pending, at)) = self.pending.take() {
            let start = len - run;
            self.fire(recent, start, pending, at);
            // nvim ran the mapping, so it reads the key after it fresh
            if let Some(after) = recent.get_mut(start + pending) {
                after.argument = false;
            }
            self.refold(recent, start + pending);
            return;
        }
        for suffix in (1..run).rev() {
            if self.extend(recent, keys, suffix, now) {
                return;
            }
        }
    }

    /// Reads the last `run` keys of `recent` as one run, answering whether
    /// they spell or begin a mapping.
    fn extend(
        &mut self,
        recent: &VecDeque<Folded>,
        keys: &[Vec<String>],
        run: usize,
        now: SystemTime,
    ) -> bool {
        let start = recent.len() - run;
        if !starts_normal(recent, start) {
            return false;
        }
        let whole = whole(recent, start, keys);
        let longer = begins_longer(recent, start, keys) || begins_longer(recent, start, &self.view);
        if whole && !longer {
            self.fire(recent, start, run, now);
            self.reset();
            return true;
        }
        if whole || longer {
            self.run = run;
            if whole {
                self.pending = Some((run, now));
            }
            return true;
        }
        false
    }

    fn fire(&mut self, recent: &VecDeque<Folded>, start: usize, count: usize, at: SystemTime) {
        let keys = recent
            .range(start..start + count)
            .map(|folded| folded.key.clone())
            .collect();
        self.fired.push((keys, at));
        self.fired_end = Some(start + count);
    }
}

/// The key log's own reading of the keys sent to nvim: its window of recent
/// keys and the doubts it reads them by.
#[derive(Debug, Clone, Default)]
pub(crate) struct Matcher {
    run: UserRun,
    /// The user's normal-mode mappings, sorted for [`UserRun`] to search.
    user: Vec<Vec<String>>,
    /// The latest keys sent to nvim in normal mode, as many as
    /// [`UserRun::window`].
    recent: VecDeque<Folded>,
    /// The key whose argument nvim reads the next normal-mode key as.
    argument_of: Option<&'static str>,
    /// Whether a key that leaves normal mode has gone out since nvim last
    /// reported a mode, or a round trip passed with no report.
    mode_unsure: bool,
    /// Whether one of the user's mappings has fired since nvim last
    /// answered a key, so its rhs may have left normal mode where nvim
    /// reports nothing for a mode it did not change.
    mapped_unsure: bool,
}

impl Matcher {
    /// Learns view's invoking sequences, each spelled as `keytrans()`
    /// writes it, and forgets the keys read against the old ones.
    pub(crate) fn learn_view<'a>(&mut self, keys: impl Iterator<Item = &'a String>) {
        self.run.learn_view(keys);
        self.recent.clear();
    }

    /// Learns the user's normal-mode mappings, one [`canonical`] key per
    /// entry, and how long nvim waits for the rest of one, `None` where it
    /// waits for good.
    pub(crate) fn learn_user(&mut self, keys: &[Vec<String>], wait: Option<Duration>) {
        self.user = keys.to_vec();
        self.run.learn_user(&mut self.user, wait);
    }

    /// Forgets the keys nvim was reading a mapping or an argument from, for
    /// a click, which ends both. A whole mapping pending then is one the
    /// click ran.
    pub(crate) fn forget(&mut self) {
        self.argument_of = None;
        self.recent.clear();
        self.mapped_unsure |= self.run.forget();
    }

    /// Notes a mode nvim reported. Keys a fired mapping left unsure were
    /// typed in that mode, so a mode other than normal drops the
    /// invocation they completed.
    pub(crate) fn note_mode_reported(&mut self, mode: &str) {
        self.mode_unsure = false;
        self.mapped_unsure = false;
        if mode != "normal" {
            let _ = self.run.take_invoked();
        }
    }

    /// Notes a redraw batch nvim sent because it read a key: a mapping that
    /// fired before it has run, and a mode its rhs entered rides the same
    /// batch.
    pub(crate) fn note_answered(&mut self) {
        self.mapped_unsure = false;
    }

    /// The keys of the view invocation a key last completed, once.
    pub(crate) fn take_invoked(&mut self) -> Option<Vec<String>> {
        self.run.take_invoked()
    }

    /// Each mapping the latest key fired and when its last key was
    /// pressed, once.
    pub(crate) fn take_fired(&mut self) -> Vec<(Vec<String>, SystemTime)> {
        self.run.take_fired()
    }

    /// Reads `notation`, pressed at `now`, read in normal mode where
    /// `normal`. nvim reports a mode it entered inside `round_trip`, and
    /// its clock may read a gap up to `margin` away from view's.
    ///
    /// Inside a sequence nvim matches the mapping before it reads a key as
    /// an argument, so the `a` of `\ai` still completes it. The first key
    /// is the exception: typed as the argument of `f`, `r` or `"`, nvim
    /// reads it literally and starts no mapping. A key behind one that
    /// leaves normal mode, or behind a fired mapping nvim has not answered,
    /// starts no row of the user's until nvim answers or a round trip
    /// passes. The keys of view's invocation are kept either way, for the
    /// row it logs should nvim run it.
    fn read(
        &mut self,
        notation: &str,
        normal: bool,
        now: SystemTime,
        round_trip: Duration,
        margin: Duration,
    ) {
        // a mode nvim entered is reported inside the round trip bound, and
        // a key it swallowed, or a rhs that draws nothing, sends no batch
        // to clear the doubt
        let gap = self.run.since_last(now);
        let quiet =
            |ran_after: Duration| gap.is_some_and(|gap| gap.saturating_sub(ran_after) > round_trip);
        if quiet(Duration::ZERO) {
            self.mapped_unsure = false;
            self.mode_unsure = false;
        }
        let argument_of = self.argument_of.take();
        let window = self.run.window();
        if !normal || window == 0 {
            self.recent.clear();
            self.run.reset();
            return;
        }
        let ended = self.run.time_out(&mut self.recent, now, margin);
        if ended != Gap::Within {
            self.note_fired();
            // nvim ran the held mapping `'timeoutlen'` after its last key
            if quiet(self.run.wait.unwrap_or_default()) {
                self.mapped_unsure = false;
                self.mode_unsure = false;
            }
        }
        // past the gap nvim has read every key before it as typed; close to
        // it, those keys may still complete one of view's sequences
        if ended == Gap::Past {
            self.recent.clear();
        }
        let key = canonical(notation);
        self.argument_of = super::owed_after(argument_of, notation);
        let mode_unsure = self.mode_unsure;
        let mapped_unsure = self.mapped_unsure;
        self.mode_unsure |= leaves_normal(argument_of, &key);
        while self.recent.len() >= window {
            self.recent.pop_front();
        }
        let operand = self.run.operand(&key, argument_of.is_some());
        self.recent.push_back(Folded {
            key,
            argument: argument_of.is_some(),
            mode_unsure,
            mapped_unsure,
            operand,
        });
        if let Some(start) = self.run.completed(&self.recent, &self.user) {
            let unsure = self
                .recent
                .get(start)
                .is_some_and(|first| first.mode_unsure);
            let invocation = run_from(&self.recent, start).cloned().collect();
            self.recent.clear();
            self.run.invoked(invocation);
            // the keys inside the sequence were the mapping's, and left no
            // mode behind them
            if !unsure {
                self.argument_of = None;
                self.mode_unsure = false;
            }
        } else {
            self.run.step(&mut self.recent, &self.user, now);
            self.note_fired();
        }
        // a mapping that ran makes the key after it a command again
        if !self.run.fired.is_empty() {
            self.argument_of = None;
        }
    }

    /// Reads the user mapping a key just fired: its own keys left no mode
    /// behind them, the keys after it left one where any of them leaves
    /// normal mode, and its rhs may have left one, so nothing is matched
    /// until nvim answers or a key comes a round trip later with no mode
    /// reported.
    fn note_fired(&mut self) {
        let Some(end) = self.run.take_fired_end() else {
            return;
        };
        let mut before: Option<&str> = None;
        let mut leaves = false;
        for folded in self.recent.range(end.min(self.recent.len())..) {
            let of = before.filter(|_| folded.argument);
            leaves |= leaves_normal(of, &folded.key);
            before = Some(&folded.key);
        }
        self.mode_unsure = leaves;
        self.mapped_unsure = true;
    }
}

/// What the key log reads off the editor and the hold for one key, copied
/// out before the key folds.
#[derive(Debug, Clone, Copy)]
pub(super) struct Seen {
    normal: bool,
    now: SystemTime,
    round_trip: Duration,
    margin: Duration,
}

impl Seen {
    /// What `model` shows the key log for the key about to go to nvim.
    pub(super) fn of(model: &Model) -> Self {
        Self {
            // keys typed on a tracked `:` line are its text, whatever mode
            // nvim last reported: a line view sends itself opens with no
            // `:` folded here to mark the mode unsure
            normal: model.engine.mode.current == "normal" && model.submit_hold.typed.is_none(),
            now: model.key_log().now(),
            round_trip: crate::native::speculate::cmdline_backstop(model),
            margin: round_trip_spread(&model.engine.key_round_trips),
        }
    }
}

/// Folds one key going to nvim into the key log's reading of the keys.
/// The hold reaches it only as `seen`, so nothing here writes the hold.
/// See [`super::fold_engine_key`].
pub(super) fn fold(log: &mut Matcher, seen: Seen, notation: &str) {
    log.read(
        notation,
        seen.normal,
        seen.now,
        seen.round_trip,
        seen.margin,
    );
}

/// Whether nvim reads the key at `start` in normal mode: no argument of the
/// key before it, no mode change in flight, and no operator waiting.
fn starts_normal(recent: &VecDeque<Folded>, start: usize) -> bool {
    recent.get(start).is_some_and(|first| {
        !first.argument && !first.mode_unsure && !first.mapped_unsure && !first.operand
    })
}

/// How far apart the recent key round trips lie, the jitter between when
/// view stamped a key and when nvim read it.
fn round_trip_spread(trips: &[Option<Duration>]) -> Duration {
    let trips = trips.iter().flatten();
    trips
        .clone()
        .max()
        .zip(trips.min())
        .map_or(Duration::ZERO, |(max, min)| max.saturating_sub(*min))
}

/// The keys of `recent` from `start`.
fn run_from(recent: &VecDeque<Folded>, start: usize) -> impl Iterator<Item = &String> + Clone {
    recent.range(start..).map(|folded| &folded.key)
}

/// Where the run from `start` sorts among `keys`.
fn sorted_at(recent: &VecDeque<Folded>, start: usize, keys: &[Vec<String>]) -> usize {
    let run = run_from(recent, start);
    keys.partition_point(|lhs| lhs.iter().partial_cmp(run.clone()) == Some(Ordering::Less))
}

/// Whether the keys of `recent` from `start` spell one of `keys` whole.
fn whole(recent: &VecDeque<Folded>, start: usize, keys: &[Vec<String>]) -> bool {
    let at = sorted_at(recent, start, keys);
    keys.get(at)
        .is_some_and(|lhs| lhs.iter().eq(run_from(recent, start)))
}

/// Whether one of `keys` is longer than the keys of `recent` from `start`
/// and begins with them.
fn begins_longer(recent: &VecDeque<Folded>, start: usize, keys: &[Vec<String>]) -> bool {
    let count = recent.len() - start;
    let at = sorted_at(recent, start, keys);
    let begins =
        |lhs: &Vec<String>| lhs.len() > count && lhs.iter().take(count).eq(run_from(recent, start));
    keys.get(at).is_some_and(begins) || keys.get(at + 1).is_some_and(begins)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const AT: SystemTime = SystemTime::UNIX_EPOCH;

    fn mappings(spelled: &[&[&str]]) -> Vec<Vec<String>> {
        spelled
            .iter()
            .map(|keys| keys.iter().map(|key| (*key).to_string()).collect())
            .collect()
    }

    /// Types `keys` one at a time over `view`'s invoking sequences, the
    /// key after `g` or `z` read as its argument the way the fold reads it,
    /// and a run that spells one of `view` whole taken as view's invocation,
    /// answering what each fired.
    fn typed_over(
        keys: &[&str],
        user: &mut Vec<Vec<String>>,
        view: &[&str],
    ) -> Vec<Vec<Vec<String>>> {
        let mut run = UserRun::default();
        let view: Vec<String> = view.iter().map(|keys| (*keys).to_string()).collect();
        run.learn_view(view.iter());
        run.learn_user(user, None);
        let window = run.window().max(4);
        let mut recent: VecDeque<Folded> = VecDeque::new();
        let mut fired_last = false;
        keys.iter()
            .map(|key| {
                if recent.len() >= window {
                    recent.pop_front();
                }
                let argument = !fired_last
                    && recent.back().is_some_and(|before| {
                        !before.argument && matches!(before.key.as_str(), "g" | "z")
                    });
                let operand = run.operand(key, argument);
                recent.push_back(Folded {
                    key: (*key).to_string(),
                    argument,
                    mode_unsure: false,
                    mapped_unsure: false,
                    operand,
                });
                match run.completed(&recent, user) {
                    Some(start) => run.invoked(run_from(&recent, start).cloned().collect()),
                    None => run.step(&mut recent, user, AT),
                }
                let fired: Vec<_> = run.take_fired().into_iter().map(|(keys, _)| keys).collect();
                fired_last = !fired.is_empty();
                fired
            })
            .collect()
    }

    fn typed(keys: &[&str], user: &mut Vec<Vec<String>>) -> Vec<Vec<Vec<String>>> {
        typed_over(keys, user, &[])
    }

    fn keys(spelled: &[&str]) -> Vec<Vec<String>> {
        vec![spelled.iter().map(|key| (*key).to_string()).collect()]
    }

    const NONE: Vec<Vec<String>> = Vec::new();

    #[test]
    fn a_mapping_fires_on_its_last_key_and_its_prefix_fires_nothing() {
        let mut user = mappings(&[&[" ", "f", "g"], &["g", "d"]]);
        assert_eq!(
            typed(&["x", " ", "f", "g", "g", "d"], &mut user),
            [
                NONE,
                NONE,
                NONE,
                keys(&[" ", "f", "g"]),
                NONE,
                keys(&["g", "d"])
            ]
        );
    }

    #[test]
    fn a_whole_mapping_a_longer_one_begins_with_fires_when_the_next_key_parts() {
        let mut user = mappings(&[&[" ", "f"], &[" ", "f", "g"]]);
        assert_eq!(
            typed(&[" ", "f", "x"], &mut user),
            [NONE, NONE, keys(&[" ", "f"])]
        );
        assert_eq!(
            typed(&[" ", "f", "g"], &mut user),
            [NONE, NONE, keys(&[" ", "f", "g"])]
        );
    }

    /// nvim waits on a user mapping that one of view's own keys extends,
    /// so the user's mapping runs only once the next key parts from view's,
    /// and not at all when it completes view's.
    #[test]
    fn a_users_mapping_a_view_key_extends_waits_for_the_next_key() {
        let mut user = mappings(&[&[" ", "w"]]);
        assert_eq!(
            typed_over(&[" ", "w", "z"], &mut user, &["<Space>wz"]),
            [NONE, NONE, NONE],
            "the keys completed view's <Space>wz"
        );
        assert_eq!(
            typed_over(&[" ", "w", "x"], &mut user, &["<Space>wz"]),
            [NONE, NONE, keys(&[" ", "w"])]
        );
    }

    /// The held row is stamped with the moment its own last key went out,
    /// whenever the key that settles it arrives.
    #[test]
    fn a_held_mapping_is_stamped_with_its_last_keys_time() {
        let mut user = mappings(&[&[" ", "f"], &[" ", "f", "g"]]);
        let fired = timed(&mut user, None, &[(" ", 1), ("f", 2), ("x", 9)]);
        assert_eq!(fired, [(keys(&[" ", "f"]).remove(0), at(2))]);
    }

    fn at(secs: u64) -> SystemTime {
        AT + Duration::from_secs(secs)
    }

    /// Types `(key, secs)` over `user` with `'timeoutlen'` at `wait`, the
    /// window cleared at a gap nvim gave up on the way the fold clears it,
    /// answering every mapping fired and its stamp.
    fn timed(
        user: &mut Vec<Vec<String>>,
        wait: Option<Duration>,
        typed: &[(&str, u64)],
    ) -> Vec<(Vec<String>, SystemTime)> {
        let typed: Vec<_> = typed
            .iter()
            .map(|(key, secs)| (*key, secs * 1000))
            .collect();
        timed_ms(user, wait, Duration::ZERO, &typed)
    }

    /// [`timed`] with `(key, ms)` stamps, nvim's clock up to `margin` away
    /// from view's.
    fn timed_ms(
        user: &mut Vec<Vec<String>>,
        wait: Option<Duration>,
        margin: Duration,
        typed: &[(&str, u64)],
    ) -> Vec<(Vec<String>, SystemTime)> {
        let mut run = UserRun::default();
        run.learn_user(user, wait);
        let mut recent = VecDeque::new();
        let mut fired = Vec::new();
        for (key, ms) in typed {
            let now = AT + Duration::from_millis(*ms);
            if run.time_out(&mut recent, now, margin) == Gap::Past {
                recent.clear();
            }
            recent.push_back(folded(key));
            run.step(&mut recent, user, now);
            fired.extend(run.take_fired());
        }
        fired
    }

    fn folded(key: &str) -> Folded {
        Folded {
            key: key.to_string(),
            argument: false,
            mode_unsure: false,
            mapped_unsure: false,
            operand: false,
        }
    }

    /// A gap within the round trips' jitter of `'timeoutlen'` may have
    /// read on the other side of it on nvim's clock, so it completes no
    /// mapping and fires none held at it: no row where nvim may have run
    /// another.
    #[test]
    fn a_gap_this_close_to_timeoutlen_logs_nothing() {
        let wait = Some(Duration::from_millis(1000));
        let margin = Duration::from_millis(5);
        for late in [997, 1003] {
            let typed = [(" ", 0), ("f", 10), ("g", 10 + late)];
            let mut user = mappings(&[&[" ", "f", "g"]]);
            assert_eq!(timed_ms(&mut user, wait, margin, &typed), [], "{late}");
            let mut user = mappings(&[&[" ", "f"], &[" ", "f", "g"]]);
            assert_eq!(timed_ms(&mut user, wait, margin, &typed), [], "{late}");
        }
        let typed = [(" ", 0), ("f", 10), ("g", 1100)];
        let mut user = mappings(&[&[" ", "f"], &[" ", "f", "g"]]);
        assert_eq!(
            timed_ms(&mut user, wait, margin, &typed),
            [(keys(&[" ", "f"]).remove(0), AT + Duration::from_millis(10))],
            "a gap past the jitter"
        );
    }

    /// The spread reads the round trips taken and nothing for fewer than
    /// two of them.
    #[test]
    fn the_spread_reads_only_the_trips_taken() {
        let ms = Duration::from_millis;
        assert_eq!(round_trip_spread(&[None, None]), Duration::ZERO);
        assert_eq!(round_trip_spread(&[Some(ms(7)), None]), Duration::ZERO);
        assert_eq!(
            round_trip_spread(&[Some(ms(40)), None, Some(ms(4))]),
            ms(36)
        );
    }

    /// A read that takes every mapping away mid-run forgets the run, so a
    /// gap after it reads no keys the window no longer holds.
    #[test]
    fn a_read_with_no_mappings_forgets_the_run() {
        let mut run = UserRun::default();
        run.learn_view([String::from("q")].iter());
        let wait = Some(Duration::from_secs(1));
        let mut user = mappings(&[&[" ", "f"], &[" ", "f", "g"]]);
        run.learn_user(&mut user, wait);
        let mut recent = VecDeque::new();
        for key in [" ", "f"] {
            let gap = run.time_out(&mut recent, at(0), Duration::ZERO);
            assert_eq!(gap, Gap::Within);
            recent.push_back(folded(key));
            run.step(&mut recent, &user, at(0));
        }
        run.learn_user(&mut Vec::new(), wait);
        let user = Vec::new();
        let gap = run.time_out(&mut recent, at(0), Duration::ZERO);
        assert_eq!(gap, Gap::Within);
        while recent.len() >= run.window() {
            recent.pop_front();
        }
        recent.push_back(folded("x"));
        run.step(&mut recent, &user, at(0));
        let gap = run.time_out(&mut recent, at(9), Duration::ZERO);
        assert_eq!(gap, Gap::Within);
        assert!(run.take_fired().is_empty());
    }

    /// A gap past `'timeoutlen'` is where nvim gave up on the run: no
    /// mapping completes across it, and a whole one held at it fires with
    /// its own stamp. With `'timeout'` off the run waits for good.
    #[test]
    fn a_gap_past_timeoutlen_ends_the_run() {
        let wait = Some(Duration::from_secs(1));
        let typed = [(" ", 1), ("f", 2), ("g", 9)];
        let mut user = mappings(&[&[" ", "f", "g"]]);
        assert_eq!(timed(&mut user, wait, &typed), []);
        let mut user = mappings(&[&[" ", "f"], &[" ", "f", "g"]]);
        assert_eq!(
            timed(&mut user, wait, &typed),
            [(keys(&[" ", "f"]).remove(0), at(2))]
        );
        let mut user = mappings(&[&[" ", "f", "g"]]);
        assert_eq!(
            timed(&mut user, None, &typed),
            [(keys(&[" ", "f", "g"]).remove(0), at(9))]
        );
        assert_eq!(
            timed(&mut user, wait, &[(" ", 1), ("f", 2), ("g", 3)]),
            [(keys(&[" ", "f", "g"]).remove(0), at(3))],
            "a gap inside 'timeoutlen'"
        );
    }

    /// After a run parts with nothing pending, nvim matches again from the
    /// run's second key, so a mapping that starts inside the parted run
    /// still fires. A pending mapping the next key parts from fires alone:
    /// its rhs may have left normal mode before nvim reads that key.
    #[test]
    fn a_parted_run_is_matched_again_from_each_later_key() {
        let mut user = mappings(&[&[" ", "x", "y"], &["x", "z"]]);
        assert_eq!(
            typed(&[" ", "x", "z"], &mut user),
            [NONE, NONE, keys(&["x", "z"])]
        );
        let mut user = mappings(&[&[" ", "f"], &[" ", "f", "g"], &["s"]]);
        assert_eq!(
            typed(&[" ", "f", "s"], &mut user),
            [NONE, NONE, keys(&[" ", "f"])],
            "the pending mapping runs, and the key that parted it is unsure"
        );
    }

    /// An operator key inside a pending mapping's lhs was the mapping's, so
    /// the keys after the mapping are read from no operator.
    #[test]
    fn keys_after_a_pending_mapping_wait_on_no_operator_in_it() {
        let mut user = mappings(&[&[" ", "d"], &[" ", "d", "x"], &["s"]]);
        assert_eq!(
            typed(&[" ", "d", "2", "s"], &mut user),
            [NONE, NONE, keys(&[" ", "d"]), keys(&["s"])],
            "2 is a count before s, no operator's"
        );
        assert_eq!(
            typed(&[" ", "d", "d", "s"], &mut user),
            [NONE, NONE, keys(&[" ", "d"]), NONE],
            "the d after the mapping is an operator, and s its motion"
        );
    }

    /// `0` after an operator is the start-of-line motion, and a digit only
    /// once a count has begun.
    #[test]
    fn a_zero_after_an_operator_is_its_motion() {
        let mut user = mappings(&[&["s"]]);
        assert_eq!(
            typed(&["d", "0", "s"], &mut user),
            [NONE, NONE, keys(&["s"])]
        );
        assert_eq!(
            typed(&["d", "2", "0", "s", "s"], &mut user),
            [NONE, NONE, NONE, NONE, keys(&["s"])]
        );
    }

    /// A buffer's own mapping on view's keys is the one nvim runs, so it
    /// completes no invocation.
    #[test]
    fn a_users_mapping_on_view_keys_completes_no_invocation() {
        let mut user = mappings(&[&[" ", "e"]]);
        assert_eq!(
            typed_over(&[" ", "e"], &mut user, &["<Space>e"]),
            [NONE, keys(&[" ", "e"])]
        );
    }

    /// Every operator nvim reads in normal mode, a count between it and
    /// its motion, and a motion that reads a second key: the key after
    /// each is the operator's, so it fires no normal-mode mapping. Once the
    /// motion is done, the next key is a normal-mode command again.
    #[test]
    fn a_key_an_operator_reads_fires_no_normal_mode_mapping() {
        for operator in OPERATORS {
            let spelled: Vec<String> = operator.chars().map(String::from).collect();
            for between in [&[][..], &["2"], &["i"], &["2", "a"]] {
                let mut sequence: Vec<&str> = spelled.iter().map(String::as_str).collect();
                sequence.extend_from_slice(between);
                sequence.push("s");
                let mut user = mappings(&[&["s"]]);
                let fired = typed(&sequence, &mut user);
                assert!(
                    fired.iter().all(Vec::is_empty),
                    "{sequence:?} fired {fired:?}"
                );
                sequence.push("s");
                let fired = typed(&sequence, &mut user);
                assert_eq!(
                    fired.last(),
                    Some(&keys(&["s"])),
                    "{sequence:?}: the motion is done, so the next `s` is a command"
                );
            }
        }
    }

    /// The user's `gr` beside a longer `grn`: `<Space>` parts from `grn`,
    /// so nvim runs `gr` and reads `<Space>` as a command of its own. The
    /// log names `gr` and the `<Space>ff` invocation after it.
    #[test]
    fn a_key_after_a_mapping_its_own_key_ran_is_no_argument() {
        let mut log = Matcher::default();
        let view = ["<Space>ff".to_string()];
        log.learn_view(view.iter());
        log.learn_user(&mappings(&[&["g", "r"], &["g", "r", "n"]]), None);
        for (key, at) in ["g", "r", " ", "f", "f"].into_iter().zip(0..) {
            let now = AT + Duration::from_millis(at * 10);
            log.read(key, true, now, Duration::from_millis(5), Duration::ZERO);
        }
        let fired: Vec<_> = log.take_fired().into_iter().map(|(keys, _)| keys).collect();
        assert_eq!(fired, keys(&["g", "r"]));
        let invoked = ["<Space>", "f", "f"].map(canonical);
        assert_eq!(log.take_invoked().as_deref(), Some(&invoked[..]));
    }
}
