//! Which of the user's own normal-mode mappings the keys sent to nvim just
//! completed, read off the same window of recent keys the invoking
//! sequences are matched against.
//!
//! Costs nothing per key while the user maps nothing or nvim is in another
//! mode. Otherwise a key costs a few binary searches over the user's sorted
//! mappings and view's sorted invoking sequences, each comparing at most
//! the window's length of keys, once per suffix of the window. A key
//! allocates its canonical spelling, which the window keeps, and a firing
//! mapping allocates its keys.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

use super::{canonical, key_tokens, Folded, LEAVES_NORMAL, LEAVES_NORMAL_AFTER};
use crate::model::Model;

/// Every normal-mode key sequence after which nvim reads the next keys as
/// an operator's motion, where a normal-mode mapping does not apply: nvim's
/// own operators and its default `gc` comment operator.
pub(crate) const OPERATORS: [&str; 16] = [
    "d", "y", "c", "<", ">", "!", "=", "g~", "gu", "gU", "g?", "gq", "gw", "g@", "zf", "gc",
];

/// The keys `spelled` names, one [`canonical`] key each.
pub(crate) fn canonical_keys(spelled: &str) -> Vec<String> {
    key_tokens(spelled).map(canonical).collect()
}

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
    /// keys, since a read lands between any two keys.
    pub(crate) fn learn_user(&mut self, keys: &mut Vec<Vec<String>>, wait: Option<Duration>) {
        keys.sort();
        keys.dedup();
        self.longest = keys.iter().map(Vec::len).max().unwrap_or(0);
        self.wait = wait;
    }

    /// Learns view's invoking sequences, each spelled as `keytrans()`
    /// writes it.
    pub(crate) fn learn_view<'a>(&mut self, keys: impl Iterator<Item = &'a String>) {
        self.view = keys
            .map(|spelled| canonical_keys(spelled))
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

    /// Forgets the run for input that is no key, a click or a paste, which
    /// nvim reads as the end of whatever mapping it waits on. Answers
    /// whether a whole mapping was pending, which that input ran.
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
    /// against nvim's `'timeoutlen'`, answering whether nvim gave up on
    /// the run in it. A whole mapping pending inside it ran then, so it
    /// fires stamped with its own last key's time. The rest of the run went
    /// to nvim as typed keys and is matched no further.
    pub(crate) fn time_out(&mut self, recent: &mut VecDeque<Folded>, now: SystemTime) -> bool {
        let last = self.last.replace(now);
        let expired = self.run > 0
            && self
                .wait
                .zip(last)
                .is_some_and(|(wait, last)| now.duration_since(last).is_ok_and(|gap| gap > wait));
        if !expired {
            return false;
        }
        let start = recent.len().saturating_sub(self.run);
        if let Some((pending, at)) = self.pending.take() {
            self.fire(recent, start, pending, at);
            self.refold(recent, start + pending);
        }
        self.run = 0;
        true
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

/// Folds one key going to nvim into the window of recent keys, answering
/// whether it completes one of view's invoking sequences typed in normal
/// mode, which arms the hold. See [`super::fold_engine_key`].
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
/// key that leaves normal mode, or behind a fired user mapping, with no
/// answer from nvim since, arms nothing. Its keys are kept for the row the
/// invocation logs should nvim run it.
pub(super) fn completes_invoke(model: &mut Model, notation: &str) -> bool {
    let normal = model.engine.mode.current == "normal";
    let now = model.key_log().now();
    let round_trip = crate::native::speculate::cmdline_backstop(model);
    let hold = &mut model.submit_hold;
    // a rhs that left normal mode is reported inside the round trip bound,
    // and one that draws nothing sends no batch to clear the doubt
    let gap = hold.user_run.since_last(now);
    let quiet =
        |ran_after: Duration| gap.is_some_and(|gap| gap.saturating_sub(ran_after) > round_trip);
    if quiet(Duration::ZERO) {
        hold.mapped_unsure = false;
    }
    let argument_of = hold.argument_of.take();
    let window = hold.user_run.window();
    // keys typed on a tracked `:` line are its text, whatever mode nvim
    // last reported: a line view sends itself opens with no `:` folded
    // here to mark the mode unsure
    if !normal || window == 0 || hold.typed.is_some() {
        hold.recent.clear();
        hold.user_run.reset();
        return false;
    }
    if hold.user_run.time_out(&mut hold.recent, now) {
        hold.note_fired();
        // nvim ran the held mapping `'timeoutlen'` after its last key
        if quiet(hold.user_run.wait.unwrap_or_default()) {
            hold.mapped_unsure = false;
        }
        // nvim has read every key before the gap as typed
        hold.recent.clear();
    }
    let key = canonical(notation);
    hold.argument_of = (argument_of.is_none()
        && crate::native::speculate::CMDLINE_LITERAL_KEYS.contains(&notation))
    .then(|| key.clone());
    let mode_unsure = hold.mode_unsure || hold.mapped_unsure;
    hold.mode_unsure |= leaves_normal(argument_of.as_deref(), &key);
    while hold.recent.len() >= window {
        hold.recent.pop_front();
    }
    let operand = hold.user_run.operand(&key, argument_of.is_some());
    hold.recent.push_back(Folded {
        key,
        argument: argument_of.is_some(),
        mode_unsure,
        operand,
    });
    let Some(start) = hold.user_run.completed(&hold.recent, &hold.user_keys) else {
        hold.user_run.step(&mut hold.recent, &hold.user_keys, now);
        hold.note_fired();
        return false;
    };
    let unsure = hold
        .recent
        .get(start)
        .is_some_and(|first| first.mode_unsure);
    let invocation = hold.recent.range(start..).map(|f| f.key.clone()).collect();
    hold.recent.clear();
    hold.user_run.invoked(invocation);
    if unsure {
        return false;
    }
    // the keys inside the sequence were the mapping's, and left no mode
    // behind them
    hold.argument_of = None;
    hold.mode_unsure = false;
    true
}

/// Whether `key`, typed as the argument of `before` or of nothing, leaves
/// normal mode.
fn leaves_normal(before: Option<&str>, key: &str) -> bool {
    match before {
        None => LEAVES_NORMAL.contains(&key),
        Some(before) => LEAVES_NORMAL_AFTER.contains(&(before, key)),
    }
}

impl super::SubmitHold {
    /// Reads the user mapping a key just fired: its own keys left no mode
    /// behind them, the keys after it left one where any of them leaves
    /// normal mode, and its rhs may have left one, so nothing is matched
    /// until nvim answers or a key comes a round trip later with no mode
    /// reported.
    fn note_fired(&mut self) {
        let Some(end) = self.user_run.take_fired_end() else {
            return;
        };
        let mut before: Option<&str> = None;
        let mut leaves = false;
        for folded in self.recent.range(end..) {
            let of = before.filter(|_| folded.argument);
            leaves |= leaves_normal(of, &folded.key);
            before = Some(&folded.key);
        }
        self.mode_unsure = leaves;
        self.mapped_unsure = true;
    }
}

/// Whether nvim reads the key at `start` in normal mode: no argument of the
/// key before it, no mode change in flight, and no operator waiting.
fn starts_normal(recent: &VecDeque<Folded>, start: usize) -> bool {
    recent
        .get(start)
        .is_some_and(|first| !first.argument && !first.mode_unsure && !first.operand)
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
        let mut run = UserRun::default();
        run.learn_user(user, wait);
        let mut recent = VecDeque::new();
        let mut fired = Vec::new();
        for (key, secs) in typed {
            if run.time_out(&mut recent, at(*secs)) {
                recent.clear();
            }
            recent.push_back(Folded {
                key: (*key).to_string(),
                argument: false,
                mode_unsure: false,
                operand: false,
            });
            run.step(&mut recent, user, at(*secs));
            fired.extend(run.take_fired());
        }
        fired
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
}
