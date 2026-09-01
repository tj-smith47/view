//! The cross-crate pin for the augroup names a takeover creates inside nvim.
//!
//! Two crates spell the same string and neither can read the other's: the Lua
//! that runs `nvim_create_augroup` lives in `view-engine`, the takeover table
//! whose uniqueness rule is keyed on the resulting name lives in
//! `view-native`, and the dependency direction forbids an edge between them.
//! A prefix that drifted on one side would leave the collision check
//! comparing names nvim never creates -- two rows could then claim one guard
//! and the check would wave them through, with nothing failing until a user
//! found a surface view believed it owned.
//!
//! This file is where the two meet. `view-harness` already depends on both
//! as dev-dependencies, so the pin costs no new edge, and both sides expose
//! what it reads behind their `test-support` features rather than in any
//! shipped build.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use view_engine::nvim_api::{NOTIFY_HOLD_CHUNK, OPTION_HOLD_CHUNK};
use view_native::supersede::takeover_augroups;

/// The augroup expression `HOLD_OPTION_CHUNK` builds, with the option name
/// concatenated onto it at runtime.
const OPTION_GROUP_EXPR: &str = "'view-hold-' .. name";

/// The augroup literal `HOLD_NOTIFY_CHUNK` passes to `nvim_create_augroup`.
const NOTIFY_GROUP_LITERAL: &str = "'view-hold-notify'";

/// The literal both sides share, read out of the engine's own chunk rather
/// than written down here, so this file cannot become a third spelling.
fn prefix_from_the_option_chunk() -> String {
    let expr = OPTION_GROUP_EXPR
        .strip_suffix(" .. name")
        .expect("the option chunk's group expression concatenates the option name");
    expr.trim_matches('\'').to_string()
}

#[test]
fn the_chunks_still_build_their_groups_the_way_this_pin_reads_them() {
    // the pin below is only as good as its two anchors: a chunk that stopped
    // spelling its augroup this way would leave the assertions passing
    // against expressions nvim no longer runs
    assert!(
        OPTION_HOLD_CHUNK.contains(OPTION_GROUP_EXPR),
        "HOLD_OPTION_CHUNK no longer builds its augroup as {OPTION_GROUP_EXPR}"
    );
    assert!(
        NOTIFY_HOLD_CHUNK.contains(NOTIFY_GROUP_LITERAL),
        "HOLD_NOTIFY_CHUNK no longer names its augroup {NOTIFY_GROUP_LITERAL}"
    );
}

#[test]
fn every_takeover_augroup_is_the_one_its_chunk_builds() {
    let prefix = prefix_from_the_option_chunk();
    let notify = NOTIFY_GROUP_LITERAL.trim_matches('\'');
    let augroups = takeover_augroups();
    assert!(
        !augroups.is_empty(),
        "the takeover table is empty -- this walk enforces nothing"
    );
    for augroup in &augroups {
        assert!(
            augroup.starts_with(&prefix),
            "{augroup} is not a name either hold chunk creates: the option chunk builds \
             {prefix}<option> and the notify chunk creates {notify}"
        );
    }
    assert!(
        augroups.iter().any(|group| group == notify),
        "no takeover row claims {notify}, which HOLD_NOTIFY_CHUNK creates unconditionally: \
         got {augroups:?}"
    );
}
