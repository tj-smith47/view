//! Which of the user's own normal-mode mappings the keys sent to nvim just
//! completed, read off the same window of recent keys the invoking
//! sequences are matched against.
//!
//! Costs nothing per key while the user maps nothing or nvim is in another
//! mode. Otherwise a key costs at most two binary searches over the user's
//! sorted mappings, each comparing at most the longest mapping's length of
//! keys, and allocates only when a mapping fires.

use std::cmp::Ordering;
use std::collections::VecDeque;

use super::Folded;

/// Operators whose next key nvim reads in operator-pending mode, where a
/// normal-mode mapping does not apply.
// ponytail: single-key operators only; `g~`, `gu`, `zf` and a count between
// operator and motion still read as normal mode
const OPERATORS: [&str; 7] = ["d", "c", "y", "<", ">", "=", "!"];

/// Where the latest keys stand against the user's mappings.
#[derive(Debug, Clone, Default)]
pub(crate) struct UserRun {
    /// The most keys any of the user's mappings spells, 0 where none.
    longest: usize,
    /// How many of the latest keys begin, or spell, a mapping.
    run: usize,
    /// The length of a whole mapping inside the run that a longer one
    /// begins with, which nvim runs once the next key parts from the
    /// longer one.
    pending: Option<usize>,
    /// The keys of the mapping the latest key fired, until taken.
    fired: Option<Vec<String>>,
}

impl UserRun {
    /// Sorts and dedups `keys` in place so a run can be searched for, and
    /// starts a fresh run over them.
    pub(crate) fn new(keys: &mut Vec<Vec<String>>) -> Self {
        keys.sort();
        keys.dedup();
        Self {
            longest: keys.iter().map(Vec::len).max().unwrap_or(0),
            ..Self::default()
        }
    }

    /// How many recent keys a run is read from: the longest mapping and the
    /// key before it, which can be an operator. 0 where the user maps
    /// nothing.
    pub(crate) const fn longest(&self) -> usize {
        if self.longest == 0 {
            0
        } else {
            self.longest + 1
        }
    }

    /// Forgets the run, for keys that left normal mode or ran a mapping of
    /// view's own.
    pub(crate) fn reset(&mut self) {
        self.run = 0;
        self.pending = None;
    }

    /// The keys of the mapping the latest key fired, once.
    pub(crate) fn take_fired(&mut self) -> Option<Vec<String>> {
        self.fired.take()
    }

    /// Reads the key just pushed onto `recent` against `keys`, the user's
    /// mappings as [`Self::new`] sorted them.
    pub(crate) fn step(&mut self, recent: &VecDeque<Folded>, keys: &[Vec<String>]) {
        let len = recent.len();
        if self.longest == 0 || len == 0 {
            return;
        }
        let mut run = (self.run + 1).min(len);
        self.pending = self.pending.filter(|pending| *pending < run);
        loop {
            let start = len - run;
            let (whole, longer) = if starts_normal(recent, start) {
                matches(recent, start, keys)
            } else {
                (false, false)
            };
            if whole && !longer {
                self.fire(recent, start, run);
                self.reset();
                return;
            }
            if whole || longer {
                self.run = run;
                if whole {
                    self.pending = Some(run);
                }
                return;
            }
            if let Some(pending) = self.pending.take() {
                self.fire(recent, start, pending);
            }
            if run == 1 {
                self.run = 0;
                return;
            }
            run = 1;
        }
    }

    fn fire(&mut self, recent: &VecDeque<Folded>, start: usize, count: usize) {
        if self.fired.is_none() {
            self.fired = Some(
                recent
                    .range(start..start + count)
                    .map(|folded| folded.key.clone())
                    .collect(),
            );
        }
    }
}

/// Whether nvim reads the key at `start` in normal mode: no argument of the
/// key before it, no mode change in flight, and no operator before it.
fn starts_normal(recent: &VecDeque<Folded>, start: usize) -> bool {
    let Some(first) = recent.get(start) else {
        return false;
    };
    let after_operator = start
        .checked_sub(1)
        .and_then(|before| recent.get(before))
        .is_some_and(|before| !before.argument && OPERATORS.contains(&before.key.as_str()));
    !first.argument && !first.mode_unsure && !after_operator
}

/// Whether the keys of `recent` from `start` spell a whole mapping, and
/// whether a longer mapping begins with them.
fn matches(recent: &VecDeque<Folded>, start: usize, keys: &[Vec<String>]) -> (bool, bool) {
    let run = || recent.range(start..).map(|folded| &folded.key);
    let count = recent.len() - start;
    let at = keys.partition_point(|lhs| lhs.iter().partial_cmp(run()) == Some(Ordering::Less));
    let whole = keys.get(at).is_some_and(|lhs| lhs.iter().eq(run()));
    let begins = |lhs: &Vec<String>| lhs.len() > count && lhs.iter().take(count).eq(run());
    let next = if whole { at + 1 } else { at };
    (whole, keys.get(next).is_some_and(begins))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn folded(key: &str) -> Folded {
        Folded {
            key: key.to_string(),
            argument: false,
            mode_unsure: false,
        }
    }

    fn mappings(spelled: &[&[&str]]) -> Vec<Vec<String>> {
        spelled
            .iter()
            .map(|keys| keys.iter().map(|key| (*key).to_string()).collect())
            .collect()
    }

    /// Types `keys` one at a time, answering what each fired.
    fn typed(keys: &[&str], user: &mut Vec<Vec<String>>) -> Vec<Option<Vec<String>>> {
        let mut run = UserRun::new(user);
        let mut recent = VecDeque::new();
        keys.iter()
            .map(|key| {
                if recent.len() >= run.longest() {
                    recent.pop_front();
                }
                recent.push_back(folded(key));
                run.step(&recent, user);
                run.take_fired()
            })
            .collect()
    }

    fn keys(spelled: &[&str]) -> Option<Vec<String>> {
        Some(spelled.iter().map(|key| (*key).to_string()).collect())
    }

    #[test]
    fn a_mapping_fires_on_its_last_key_and_its_prefix_fires_nothing() {
        let mut user = mappings(&[&[" ", "f", "g"], &["g", "d"]]);
        assert_eq!(
            typed(&["x", " ", "f", "g", "g", "d"], &mut user),
            [
                None,
                None,
                None,
                keys(&[" ", "f", "g"]),
                None,
                keys(&["g", "d"])
            ]
        );
    }

    #[test]
    fn a_whole_mapping_a_longer_one_begins_with_fires_when_the_next_key_parts() {
        let mut user = mappings(&[&[" ", "f"], &[" ", "f", "g"]]);
        assert_eq!(
            typed(&[" ", "f", "x"], &mut user),
            [None, None, keys(&[" ", "f"])]
        );
        assert_eq!(
            typed(&[" ", "f", "g"], &mut user),
            [None, None, keys(&[" ", "f", "g"])]
        );
    }

    #[test]
    fn a_key_after_an_operator_fires_no_normal_mode_mapping() {
        let mut user = mappings(&[&["s"]]);
        assert_eq!(
            typed(&["d", "s", "s"], &mut user),
            [None, None, keys(&["s"])]
        );
    }
}
