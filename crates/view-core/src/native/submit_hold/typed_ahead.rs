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

use super::{canonical, Folded, LEAVES_NORMAL, LEAVES_NORMAL_AFTER};
use crate::model::Model;

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
pub(super) fn completes_invoke(model: &mut Model, notation: &str) -> bool {
    let normal = model.engine.mode.current == "normal";
    let hold = &mut model.submit_hold;
    let argument_of = hold.argument_of.take();
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
        hold.object_next = false;
        return false;
    }
    let key = canonical(notation);
    hold.argument_of = argument_of
        .is_none()
        .then(|| {
            crate::native::speculate::CMDLINE_LITERAL_KEYS
                .into_iter()
                .find(|literal| *literal == notation)
        })
        .flatten();
    hold.argument_object = hold.object_next && matches!(hold.argument_of, Some("i" | "a"));
    let mode_unsure = hold.mode_unsure;
    let leaves = leaves_normal(argument_of, &key);
    // nvim matches a mapping before it reads a key as an argument, so the
    // key after one of the user's may start a sequence
    let argument = argument_of.is_some() && !in_user_keys(&hold.recent, &hold.user_keys);
    let follows = object_follows(argument_of, &key);
    // a count typed behind an operator leaves it waiting for its motion
    let digit = |key: &str| key.len() == 1 && key.bytes().all(|b| b.is_ascii_digit());
    let count = digit(&key) && (key != "0" || hold.recent.back().is_some_and(|f| digit(&f.key)));
    while hold.recent.len() >= longest.max(hold.user_longest) {
        hold.recent.pop_front();
    }
    hold.recent.push_back(Folded {
        key,
        argument,
        mode_unsure,
    });
    let left = leaves && !in_user_keys(&hold.recent, &hold.user_keys);
    hold.mode_unsure |= left;
    hold.object_next = if left {
        follows
    } else {
        count && hold.object_next
    };
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
        hold.object_next = false;
    }
    complete
}

/// Whether an `i` or `a` typed after `key`, a key that leaves normal mode
/// as the argument of `before` or of nothing, names a text object: `key`
/// is an operator or a visual-mode key, and opens no insert, replace or
/// command-line mode.
fn object_follows(before: Option<&str>, key: &str) -> bool {
    match before {
        None => !matches!(
            key,
            ":" | "/"
                | "?"
                | "o"
                | "O"
                | "a"
                | "A"
                | "i"
                | "I"
                | "s"
                | "S"
                | "C"
                | "R"
                | "<Insert>"
        ),
        Some(before) => !matches!((before, key), ("g", "i" | "I" | "R" | "Q")),
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

/// Whether `key`, typed as the argument of `before` or of nothing, leaves
/// normal mode.
pub(super) fn leaves_normal(before: Option<&str>, key: &str) -> bool {
    match before {
        None => LEAVES_NORMAL.contains(&key),
        Some(before) => LEAVES_NORMAL_AFTER.contains(&(before, key)),
    }
}
