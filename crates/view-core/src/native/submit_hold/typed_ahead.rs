//! Whether a key sent to nvim completes one of view's invoking sequences,
//! which arms the hold on the keys typed ahead of the invocation.
//!
//! The decision reads the hold's own window of recent keys, the user's
//! mapped key sequences as the mapping read wrote them, and nothing the key
//! log keeps. No clock enters it.
//!
//! With no user mappings read, a key costs what it did before they were
//! read. With some, a key that leaves normal mode, or that follows a key
//! taking an argument, compares every suffix start of the window against
//! every user lhs, twice on a key that does both. The window holds W keys,
//! the longest of view's sequences and the user's. One scan costs W first
//! key compares per lhs, and up to W(W+1)/2 key compares for an lhs whose
//! prefix matches the window from every start. At 300 mappings and a
//! five-key window a key costs at most 3,000 first-key compares and 9,000
//! key compares. A longer window costs at most W(W+1) key compares for
//! each lhs.

use std::collections::VecDeque;
use std::time::Duration;

use super::{canonical, notation_char, Folded, SubmitHold, LEAVES_NORMAL, OWES_AFTER};
use crate::model::Model;
use crate::native::speculate::{is_cmdline_mode, SpecStamp, CMDLINE_LITERAL_KEYS};

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
/// Both readings are set aside for a first key that went out unsettled:
/// behind a key that may change what nvim reads next, before [`settle`]
/// reads that key as answered. Every key does, apart from a character typed
/// while view reads insert, replace or a command line, so typed prose never
/// arms. A hold missed sends the query into the buffer as commands. One
/// armed in error ends on a settled mode report out of normal mode, on the
/// next input once the mode last reported is out of normal mode
/// ([`super::released_by_input`]), or on its bound.
pub(super) fn completes_invoke(model: &mut Model, notation: &str) -> bool {
    let mode = model.engine.mode.current.as_str();
    let normal = mode == "normal";
    let text = notation_char(notation).is_some()
        && (matches!(mode, "insert" | "replace") || is_cmdline_mode(mode));
    let hold = &mut model.submit_hold;
    let argument_of = hold.argument_of.take();
    let prior = hold.unsettled;
    if !text {
        hold.unsettled = true;
        hold.sent = None;
    }
    let longest = hold
        .invoke_keys
        .iter()
        .map(|invocation| invocation.keys.len())
        .max()
        .unwrap_or(0);
    // keys typed on a tracked `:` line are its text, whatever mode nvim
    // last reported
    if !normal && !prior || longest == 0 || hold.typed.is_some() {
        hold.recent.clear();
        return false;
    }
    let key = canonical(notation);
    hold.argument_of = owed_after(argument_of, notation);
    // nvim matches a mapping before it reads a key as an argument, so where
    // the keys up to the one owing an argument begin one of the user's
    // mappings, this key may be that mapping's. Where the mapping then fails
    // to match, nvim reads the keys as builtin commands, and this key was
    // the argument after all, so the doubt covers both readings
    let owing_begins_mapping = argument_of.is_some() && in_user_keys(&hold.recent, &hold.user_keys);
    if owing_begins_mapping {
        hold.doubt = true;
    }
    let argument = argument_of.is_some() && !owing_begins_mapping;
    while hold.recent.len() >= longest.max(hold.user_longest) {
        hold.recent.pop_front();
    }
    hold.recent.push_back(Folded {
        key,
        argument,
        unsettled: prior || owing_begins_mapping,
    });
    let recent = &hold.recent;
    let complete = hold.invoke_keys.iter().any(|invocation| {
        let keys = &invocation.keys;
        recent.len().checked_sub(keys.len()).is_some_and(|start| {
            recent
                .get(start)
                .is_some_and(|first| first.unsettled || !first.argument)
                && recent
                    .range(start..)
                    .map(|folded| &folded.key)
                    .eq(keys.iter())
        })
    });
    if complete {
        // the keys inside the sequence were the mapping's
        hold.recent.clear();
        hold.argument_of = None;
    }
    complete
}

/// Settles the hold at `now`, on a batch or a key arriving, once the newest
/// key that unsettled it is at least `floor` old and a batch answering a
/// key has arrived at least `floor` after it. A batch sooner may answer a
/// key sent before that one, and with no round trip read yet any batch
/// may. Where that key went out at least `floor` after the key before it,
/// any batch since answers it. Under a doubt it also waits until nothing
/// is owed: until the newest key neither was read as an argument nor takes
/// one and stays in normal mode, since nvim holds a key that takes an
/// argument until it has it. Without one, view's reading of an argument is
/// nvim's once nvim has answered.
pub(super) fn settle(hold: &mut SubmitHold, now: SpecStamp, floor: Option<Duration>) {
    let (Some(sent), Some(answered), Some(floor)) = (hold.sent, hold.answered, floor) else {
        return;
    };
    if answered < sent
        || now.age_since(sent) < floor
        || (answered.age_since(sent) < floor && !hold.sent_apart)
    {
        return;
    }
    if !(hold.doubt && owed(hold)) {
        hold.unsettled = false;
        hold.doubt = false;
    }
}

/// Whether the newest key nvim reads as an argument, or holds while it
/// waits for one.
fn owed(hold: &SubmitHold) -> bool {
    let owes = |key: &str| CMDLINE_LITERAL_KEYS.contains(&key) && !LEAVES_NORMAL.contains(&key);
    hold.argument_of.is_some()
        || hold
            .recent
            .back()
            .is_some_and(|newest| newest.argument || owes(newest.key.as_str()))
}

/// Whether the wake a `<CR>` armed the slowest recent round trip after it
/// went out may settle the hold. The answer to every key up to the `<CR>`
/// has arrived by then, so the newest batch answers them all. Under a
/// doubt the wake waits until nothing is owed, as [`settle`] does.
pub(super) fn settles_at_wake(hold: &SubmitHold) -> bool {
    !(hold.doubt && owed(hold))
}

/// Whether the latest keys of `recent`, read from a key nvim starts a
/// mapping on, spell one of `user_keys` or begin one.
fn in_user_keys(recent: &VecDeque<Folded>, user_keys: &[Vec<String>]) -> bool {
    (0..recent.len()).any(|start| {
        let run = recent.len() - start;
        recent.get(start).is_some_and(|first| !first.argument)
            && user_keys.iter().any(|lhs| {
                lhs.len() >= run
                    && recent
                        .range(start..)
                        .map(|folded| &folded.key)
                        .eq(lhs.iter().take(run))
            })
    })
}

/// The key nvim reads the key after `notation` as the argument of, where
/// `notation` is typed as the argument of `of` or of nothing. An argument
/// owes nothing more unless it is the second key of a builtin command of
/// three keys.
pub(crate) fn owed_after(of: Option<&str>, notation: &str) -> Option<&'static str> {
    let Some(of) = of else {
        return CMDLINE_LITERAL_KEYS
            .into_iter()
            .find(|literal| *literal == notation);
    };
    let (of, key) = (canonical(of), canonical(notation));
    OWES_AFTER
        .into_iter()
        .find(|(first, second, _)| *first == of && *second == key)
        .map(|(_, _, owing)| owing)
}
