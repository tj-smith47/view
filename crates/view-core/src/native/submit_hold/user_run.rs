//! Which of the user's own normal-mode mappings the keys sent to nvim just
//! completed, read off the same window of recent keys the invoking
//! sequences are matched against.
//!
//! Costs nothing per key while the user maps nothing or nvim is in another
//! mode. Otherwise a key costs a few binary searches over the user's sorted
//! mappings and view's sorted invoking sequences, each comparing at most
//! the window's length of keys, once per suffix of the run a parted match
//! leaves, and allocates only when a mapping fires.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::time::SystemTime;

use super::{canonical, key_tokens, Folded};

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
    /// An operator waits for its motion, or for a count before it.
    Pending,
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
    /// starts a fresh run over them, keeping view's sequences.
    pub(crate) fn learn_user(&mut self, keys: &mut Vec<Vec<String>>) {
        keys.sort();
        keys.dedup();
        self.longest = keys.iter().map(Vec::len).max().unwrap_or(0);
        self.reset();
    }

    /// Learns view's invoking sequences, each spelled as `keytrans()`
    /// writes it.
    pub(crate) fn learn_view<'a>(&mut self, keys: impl Iterator<Item = &'a String>) {
        self.view = keys
            .map(|spelled| canonical_keys(spelled))
            .filter(|keys| !keys.is_empty())
            .collect();
        self.view.sort();
        self.reset();
    }

    /// How many recent keys a run is read from. 0 where the user maps
    /// nothing.
    pub(crate) const fn longest(&self) -> usize {
        self.longest
    }

    /// Forgets the run, for keys that left normal mode or ran a mapping.
    pub(crate) fn reset(&mut self) {
        self.run = 0;
        self.pending = None;
        self.operator = Operator::Idle;
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
            Operator::Idle if OPERATORS.contains(&key) => (false, Operator::Pending),
            Operator::Idle => match key {
                "g" => (false, Operator::Prefix('g')),
                "z" => (false, Operator::Prefix('z')),
                _ => (false, Operator::Idle),
            },
            Operator::Prefix(first) => {
                let pair = format!("{first}{key}");
                let next = if OPERATORS.contains(&pair.as_str()) {
                    Operator::Pending
                } else {
                    Operator::Idle
                };
                (false, next)
            }
            Operator::Pending => {
                let counted = key.len() == 1 && key.bytes().all(|b| b.is_ascii_digit());
                let next = if counted || matches!(key, "v" | "V" | "<C-v>") {
                    Operator::Pending
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
    /// A run that parts from every mapping is read again from each of its
    /// later suffixes, longest first, after the whole mapping pending
    /// inside it fires: nvim runs that mapping, or the run's first key
    /// unmapped, and matches what is left from the start.
    pub(crate) fn step(
        &mut self,
        recent: &VecDeque<Folded>,
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
        let rest = match self.pending.take() {
            Some((pending, at)) => {
                self.fire(recent, len - run, pending, at);
                run - pending
            }
            None => run - 1,
        };
        self.run = 0;
        for suffix in (1..=rest).rev() {
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
        run.learn_user(user);
        let window = run.longest().max(4);
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
                let completes = (0..recent.len()).find(|start| whole(&recent, *start, &run.view));
                match completes {
                    Some(start) => run.invoked(run_from(&recent, start).cloned().collect()),
                    None => run.step(&recent, user, AT),
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
        let mut run = UserRun::default();
        run.learn_user(&mut user);
        let mut recent = VecDeque::new();
        let at = |secs| AT + std::time::Duration::from_secs(secs);
        for (key, secs) in [(" ", 1), ("f", 2), ("x", 9)] {
            recent.push_back(Folded {
                key: key.to_string(),
                argument: false,
                mode_unsure: false,
                operand: false,
            });
            run.step(&recent, &user, at(secs));
        }
        assert_eq!(run.take_fired(), [(keys(&[" ", "f"]).remove(0), at(2))]);
    }

    /// After a run parts, nvim matches again from the run's second key, so
    /// a mapping that starts inside the parted run still fires.
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
            [NONE, NONE, [keys(&[" ", "f"]), keys(&["s"])].concat()],
            "the pending mapping runs, then the key that parted it"
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
