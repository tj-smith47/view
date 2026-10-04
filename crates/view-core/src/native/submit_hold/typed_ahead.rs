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

use super::{canonical, Folded, SubmitHold, LEAVES_NORMAL, LEAVES_NORMAL_AFTER, OWES_AFTER};
use crate::model::Model;
use crate::native::speculate::{SpecStamp, CMDLINE_LITERAL_KEYS};

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
/// key that leaves normal mode, with no mode reported or key answered
/// since, completes nothing. A key inside one of the user's own mapped
/// sequences, the `a` of `<leader>a`, is that mapping's: it leaves no mode
/// by itself, and the key after it may start a sequence. A rhs that does
/// leave normal mode is reported, and the hold it lets arm ends on that
/// report.
///
/// Both readings are set aside for a first key folded under an error's
/// doubt. A hold missed sends the query into the buffer as commands, and
/// one armed in error ends on a mode report out of normal mode or on its
/// bound.
pub(super) fn completes_invoke(model: &mut Model, notation: &str) -> bool {
    let normal = model.engine.mode.current == "normal";
    let hold = &mut model.submit_hold;
    let argument_of = hold.argument_of.take();
    if let Some(sent) = &mut hold.doubt {
        *sent = true;
    }
    let longest = hold
        .invoke_keys
        .iter()
        .map(|invocation| invocation.keys.len())
        .max()
        .unwrap_or(0);
    // keys typed on a tracked `:` line are its text, whatever mode nvim
    // last reported: a line view sends itself opens with no `:` folded
    // here to mark the mode unsure
    if !normal || longest == 0 || hold.typed.is_some() {
        hold.recent.clear();
        return false;
    }
    let key = canonical(notation);
    hold.argument_of = owed_after(argument_of, notation);
    hold.replace_owed |= key == "r" && argument_of.is_none_or(|of| of == "g");
    let mode_unsure = hold.mode_unsure;
    let leaves = leaves_normal(argument_of, &key);
    // nvim matches a mapping before it reads a key as an argument, so where
    // the keys up to the one owing an argument begin one of the user's
    // mappings, this key may be that mapping's. Where the mapping then fails
    // to match, nvim reads the keys as builtin commands, and this key was
    // the argument after all, so the doubt covers both readings
    let owing_begins_mapping = argument_of.is_some() && in_user_keys(&hold.recent, &hold.user_keys);
    if owing_begins_mapping && hold.doubt.is_none() {
        hold.doubt = Some(true);
        hold.doubt_sent = None;
    }
    let argument = argument_of.is_some() && !owing_begins_mapping;
    while hold.recent.len() >= longest.max(hold.user_longest) {
        hold.recent.pop_front();
    }
    hold.recent.push_back(Folded {
        key,
        argument,
        mode_unsure,
        doubt: hold.doubt.is_some(),
    });
    hold.mode_unsure |= leaves && !in_user_keys(&hold.recent, &hold.user_keys);
    let recent = &hold.recent;
    let complete = hold.invoke_keys.iter().any(|invocation| {
        let keys = &invocation.keys;
        recent.len().checked_sub(keys.len()).is_some_and(|start| {
            recent
                .get(start)
                .is_some_and(|first| first.doubt || (!first.argument && !first.mode_unsure))
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

/// Ends a doubt on an answer to a key sent after the error or the mapping
/// key that raised it, once nothing is owed. A mode report counts as an
/// answer. One that answers a key sent before the error leaves the doubt
/// standing, because the error erased what that key owes.
///
/// The answering batch, arriving at `now`, has to be one that can answer a
/// key typed since the doubt was raised. One sooner than `shortest`, the
/// shortest round trip read, after the first such key answers a key sent
/// before it. Nothing is owed once the newest key neither was read as
/// an argument nor takes one and stays in normal mode: nvim and view then
/// agree, whichever way either read the keys before it. A `"` or an `f`
/// read as no argument may still owe one, since nvim holds a key that
/// takes an argument until it has it.
pub(super) fn settle_doubt(hold: &mut SubmitHold, now: SpecStamp, shortest: Duration) {
    let answers_sent = hold
        .doubt_sent
        .is_none_or(|sent| now.age_since(sent) >= shortest);
    let owes = |key: &str| CMDLINE_LITERAL_KEYS.contains(&key) && !LEAVES_NORMAL.contains(&key);
    if hold.doubt == Some(true)
        && answers_sent
        && hold.argument_of.is_none()
        && !hold
            .recent
            .back()
            .is_some_and(|newest| newest.argument || owes(newest.key.as_str()))
    {
        hold.doubt = None;
    }
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

/// Whether `key`, typed as the argument of `before` or of nothing, leaves
/// normal mode.
pub(super) fn leaves_normal(before: Option<&str>, key: &str) -> bool {
    match before {
        None => LEAVES_NORMAL.contains(&key),
        Some(before) => LEAVES_NORMAL_AFTER.contains(&(before, key)),
    }
}
