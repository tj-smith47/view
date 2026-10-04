//! Whether a key sent to nvim completes one of view's invoking sequences,
//! which arms the hold on the keys typed ahead of the invocation.
//!
//! The decision reads the hold's own window of recent keys and nothing the
//! key log keeps. No clock and none of the user's mappings enter it.

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
/// key that leaves normal mode, with no mode reported since, completes
/// nothing.
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
    let mode_unsure = hold.mode_unsure;
    hold.mode_unsure |= leaves_normal(argument_of, &key);
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

/// Whether `key`, typed as the argument of `before` or of nothing, leaves
/// normal mode.
pub(super) fn leaves_normal(before: Option<&str>, key: &str) -> bool {
    match before {
        None => LEAVES_NORMAL.contains(&key),
        Some(before) => LEAVES_NORMAL_AFTER.contains(&(before, key)),
    }
}
