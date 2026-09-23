//! Live-nvim proof of `REGISTER_MAPPINGS_CHUNK`'s restore-then-set contract
//! across two runs: a second `register_mappings` call is what a
//! `Stage::CapsUpgraded`/`Stage::ProfileFlip` reissue sends
//! (`crates/view/src/native.rs`'s `reissue_mappings`), and only a live nvim
//! can say what `maparg` reads after it.
//!
//! `nvim_api/mappings.rs`'s own unit tests pin the chunk's Lua source and
//! the wire shape of its arguments; neither says what nvim's keymap table
//! actually holds once the chunk has run twice.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::borrow::Cow;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use view_core::msg::Msg;
use view_core::native::mappings::{MappingSpec, Rhs};
use view_engine::process::{Engine, EngineConfig};

const TICK: Duration = Duration::from_millis(500);

/// The next `Msg::MappingsClaimed` on `rx`, every other message discarded.
fn next_claims(rx: &mpsc::Receiver<Msg>) -> Vec<view_core::native::mappings::MappingClaim> {
    loop {
        match rx.recv_timeout(view_test_support::host_deadline(TICK)) {
            Ok(Msg::MappingsClaimed { claimed, .. }) => return claimed,
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                panic!("no Msg::MappingsClaimed arrived within the deadline")
            }
        }
    }
}

fn chord(lhs: &'static str) -> MappingSpec {
    MappingSpec {
        feature: "window",
        lhs: Cow::Borrowed(lhs),
        verb: "focus_left",
        rhs: Rhs::Keys("<C-w>h"),
    }
}

/// The pump and cutover are returned alongside the engine so the caller
/// keeps them alive for the test's whole body: dropped here, the pump would
/// stop routing replies before the test ever calls `register_mappings`.
fn spawn_attached() -> (
    Engine,
    u64,
    mpsc::Receiver<Msg>,
    view_engine::damage::DamagePump,
    view_engine::damage::SinkCutover,
) {
    let mut engine = Engine::spawn(EngineConfig::isolated()).unwrap();
    let channel = engine.api_info.channel_id;
    let (tx, rx) = mpsc::sync_channel(256);
    let (pump, cutover) = engine.start_pump(tx);
    engine
        .handle
        .ui_attach(80, 24, view_engine::UI_EXT_OPTIONS)
        .unwrap();
    engine.handle.register_bridge(channel).unwrap();
    (engine, channel, rx, pump, cutover)
}

/// A second registration that reissues the same chord must not report it as
/// taken from a user: the previous run's own claim is not a user mapping,
/// so the reissue's own claim for the same key must answer
/// `had_user_mapping: false`.
#[test]
fn a_reissue_claims_no_key_from_itself() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    let specs = [chord("<D-Left>")];

    engine.handle.register_mappings(&specs, channel).unwrap();
    let first = next_claims(&rx);
    let first_claim = first
        .iter()
        .find(|c| c.lhs == "<D-Left>")
        .expect("the first run must claim <D-Left>");
    assert!(
        !first_claim.had_user_mapping,
        "nothing was mapped there before the first run: {first:?}"
    );

    engine.handle.register_mappings(&specs, channel).unwrap();
    let second = next_claims(&rx);
    let second_claim = second
        .iter()
        .find(|c| c.lhs == "<D-Left>")
        .expect("the reissue must claim <D-Left>");
    assert!(
        !second_claim.had_user_mapping,
        "a reissue over its own prior registration is not a user mapping taken: {second:?}"
    );
}

/// A registration that lands on a user's own mapping, then a reissue that
/// drops the chord (a flip to the editor profile), must give the user's
/// mapping back -- `maparg` after the second run reads what the user
/// wrote.
#[test]
fn a_flip_gives_back_the_user_mapping_it_took() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();
    let user_rhs = ":echo 'mine'<CR>";
    engine
        .handle
        .request(
            "nvim_set_keymap",
            vec![
                rmpv::Value::from("n"),
                rmpv::Value::from("<D-Left>"),
                rmpv::Value::from(user_rhs),
                rmpv::Value::Map(vec![(
                    rmpv::Value::from("noremap"),
                    rmpv::Value::from(true),
                )]),
            ],
        )
        .expect("planting the user's own mapping");

    let specs = [chord("<D-Left>")];
    engine.handle.register_mappings(&specs, channel).unwrap();
    let claimed = next_claims(&rx);
    let claim = claimed
        .iter()
        .find(|c| c.lhs == "<D-Left>")
        .expect("the first run must claim <D-Left>");
    assert!(
        claim.had_user_mapping,
        "the user's own mapping was there before the first run: {claimed:?}"
    );

    // the flip to the editor profile: no chords in the reissue's specs
    engine.handle.register_mappings(&[], channel).unwrap();
    let _ = next_claims(&rx);

    let read_back = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from("return vim.fn.maparg(..., 'n')"),
                rmpv::Value::Array(vec![rmpv::Value::from("<D-Left>")]),
            ],
        )
        .expect("reading maparg back after the flip");
    assert_eq!(
        read_back.as_str(),
        Some(user_rhs),
        "the flip must give the user's own mapping back, read: {read_back:?}"
    );
}

/// Every chord's `with_super` spelling beside its `with_alt` spelling, both
/// registered in the one session: `no_two_chords_share_a_spelling_under_
/// either_modifier` (`chords.rs`) pins this over the table alone, and this
/// is nvim's own answer once the 92 rows are real keymaps. Each of the 92
/// must claim (nothing was mapped there before), `maparg` must answer
/// non-empty for each, and `keytrans` must read 92 distinct forms -- proof
/// that no two of the table's spellings collapse to the same key once nvim
/// normalizes them.
#[test]
fn every_chord_spelling_registers_as_its_own_key() {
    let (engine, channel, rx, _pump, _cutover) = spawn_attached();

    // built with `DesktopChord::lhs`, the same call `view-native`'s
    // `chord_plan` makes to respell a derived row under a settled modifier
    // -- view-engine cannot depend on view-native (dependency direction),
    // so this reads the production spelling function directly, past the
    // compiled-in `with_super`/`with_alt` fields.
    use view_core::native::chords::DesktopModifier;
    let mut specs = Vec::new();
    for chord in view_core::native::chords::desktop_chords() {
        specs.push(MappingSpec {
            feature: chord.feature,
            lhs: Cow::Borrowed(chord.lhs(DesktopModifier::Super)),
            verb: chord.verb,
            rhs: chord.rhs,
        });
        specs.push(MappingSpec {
            feature: chord.feature,
            lhs: Cow::Borrowed(chord.lhs(DesktopModifier::Alt)),
            verb: chord.verb,
            rhs: chord.rhs,
        });
    }
    let want = specs.len();
    assert_eq!(
        want,
        2 * view_core::native::chords::DESKTOP_CHORD_COUNT,
        "one super spelling and one alt spelling per chord"
    );

    engine.handle.register_mappings(&specs, channel).unwrap();
    let claimed = next_claims(&rx);
    let claimed_lhs: std::collections::BTreeSet<&str> =
        claimed.iter().map(|c| c.lhs.as_str()).collect();
    assert_eq!(
        claimed_lhs.len(),
        want,
        "every one of the {want} spellings must claim its own key: {claimed:?}"
    );
    for claim in &claimed {
        assert!(
            !claim.had_user_mapping,
            "a fresh session had nothing mapped under {}: {claimed:?}",
            claim.lhs
        );
    }

    let lhs_array = rmpv::Value::Array(
        specs
            .iter()
            .map(|s| rmpv::Value::from(s.lhs.as_ref()))
            .collect(),
    );
    let report = engine
        .handle
        .request(
            "nvim_exec_lua",
            vec![
                rmpv::Value::from(
                    "local lhs = ...\n\
                     local hits, forms = {}, {}\n\
                     for i, l in ipairs(lhs) do\n\
                       hits[i] = vim.fn.maparg(l, 'n') ~= ''\n\
                       local raw =\n\
                         vim.api.nvim_replace_termcodes(l, true, true, true)\n\
                       forms[i] = vim.fn.keytrans(raw)\n\
                     end\n\
                     return { hits, forms }",
                ),
                rmpv::Value::Array(vec![lhs_array]),
            ],
        )
        .expect("reading maparg and keytrans for all 92 spellings");
    let pair = report.as_array().expect("the chunk returns [hits, forms]");
    let hits = pair[0].as_array().expect("hits crosses as an array");
    let forms = pair[1].as_array().expect("forms crosses as an array");
    assert_eq!(hits.len(), want);
    assert!(
        hits.iter().all(|h| h.as_bool() == Some(true)),
        "every one of the {want} spellings must answer maparg: {hits:?}"
    );
    let distinct: std::collections::BTreeSet<&str> =
        forms.iter().filter_map(rmpv::Value::as_str).collect();
    assert_eq!(
        distinct.len(),
        want,
        "the {want} spellings must read {want} distinct keytrans forms, read {forms:?}"
    );
}
