#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::KeysConfig;
use view_core::native::chords::{desktop_chord, desktop_chords};
use view_core::native::keys::{key_tokens, lhs_keys, Direction};

/// The owned half of an [`Inputs`], for a test to move keys on.
#[derive(Clone)]
struct Fixture {
    cfg: NativeConfig,
    ui_lhs: [String; UI_KEYS.len()],
    desktop: [Resolved<String>; DESKTOP_CHORD_COUNT],
    bindings: KeyBindings,
    profile: KeyProfile,
    modifier: DesktopModifier,
    ai: bool,
    dvr: bool,
    leader: Option<String>,
}

impl Fixture {
    /// Every feature on and every key on its default.
    fn new(profile: KeyProfile, modifier: DesktopModifier) -> Self {
        Self {
            cfg: NativeConfig::all_enabled(),
            ui_lhs: KeysConfig::default().ui_lhs().clone(),
            desktop: std::array::from_fn(|_| Resolved::new(String::new(), Source::Derived)),
            bindings: KeyBindings::default(),
            profile,
            modifier,
            ai: true,
            dvr: true,
            leader: None,
        }
    }

    fn inputs(&self) -> Inputs<'_> {
        Inputs {
            cfg: &self.cfg,
            ui_lhs: &self.ui_lhs,
            desktop: &self.desktop,
            bindings: &self.bindings,
            profile: self.profile,
            modifier: self.modifier,
            ai: self.ai,
            dvr: self.dvr,
            leader: self.leader.as_deref(),
        }
    }

    fn settle(&self) -> Settled {
        settle(&self.inputs())
    }

    fn claimants(&self) -> Vec<Claimant> {
        claimants(&self.inputs())
    }

    /// Moves `row` onto `key`, or false when `key` is no key it can take.
    fn move_row(&mut self, row: (&str, &str), key: &str) -> bool {
        if row.0 == "keys.desktop" {
            let at = desktop_chords().iter().position(|c| c.id == row.1).unwrap();
            self.desktop[at] = Resolved::new(key.into(), Source::File);
            return true;
        }
        if let Some(at) = UI_KEYS.iter().position(|ui| ui.key == row.1) {
            self.ui_lhs[at] = key.into();
            return true;
        }
        let (_, action) = KEY_ACTIONS.iter().find(|(k, _)| *k == row.1).unwrap();
        self.bindings.rebind(*action, &[key.into()])
    }
}

fn desktop_alt() -> Fixture {
    Fixture::new(KeyProfile::Desktop, DesktopModifier::Alt)
}

/// The keys `settled` registers with nvim, and the keys view's own
/// surfaces answer, each canonical and sorted.
fn held(settled: &Settled) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
    let mut specs: Vec<_> = settled
        .specs
        .iter()
        .map(|s| lhs_keys(&s.lhs, None))
        .collect();
    let mut own: Vec<_> = KEY_ACTIONS
        .iter()
        .flat_map(|(_, action)| settled.bindings.spellings(*action))
        .map(|key| lhs_keys(&key, None))
        .collect();
    specs.sort();
    own.sort();
    (specs, own)
}

/// `key` with its modifier letters and its named keys lowercased, which
/// nvim reads as the same key: `<m-cr>` for `<M-CR>`.
fn lowercased(key: &str) -> String {
    key_tokens(key)
        .map(|token| {
            let Some(mut rest) = token.strip_prefix('<').and_then(|t| t.strip_suffix('>')) else {
                return token.to_string();
            };
            let mut out = String::from("<");
            while rest.len() > 2 && rest.as_bytes()[1] == b'-' {
                out.push_str(&rest[..2].to_ascii_lowercase());
                rest = &rest[2..];
            }
            if rest.chars().count() > 1 {
                out.push_str(&rest.to_ascii_lowercase());
            } else {
                out.push_str(rest);
            }
            out.push('>');
            out
        })
        .collect()
}

/// `key` with each `<CR>`, `<BS>` and `<Del>` in it written with nvim's
/// other name for that key: `<M-Enter>` for `<M-CR>`.
fn aliased(key: &str) -> String {
    key.replace("CR>", "Enter>")
        .replace("BS>", "BackSpace>")
        .replace("Del>", "Delete>")
}

#[test]
fn no_two_defaults_share_a_key_in_one_scope() {
    for profile in [KeyProfile::Desktop, KeyProfile::Editor] {
        for modifier in [DesktopModifier::Alt, DesktopModifier::Super] {
            let fixture = Fixture::new(profile, modifier);
            let claimants = fixture.claimants();
            for (i, a) in claimants.iter().enumerate() {
                for b in claimants.iter().skip(i + 1) {
                    let shared: Vec<_> = a
                        .canonical
                        .iter()
                        .filter(|k| b.canonical.contains(k))
                        .collect();
                    assert!(
                        a.scope & b.scope == 0 || shared.is_empty(),
                        "{profile:?}/{modifier:?}: `{}` and `{}` share {shared:?}",
                        a.name,
                        b.name
                    );
                }
            }
            let settled = fixture.settle();
            assert!(settled.notices.is_empty(), "{:?}", settled.notices);
            assert!(settled.put_back.is_empty(), "{:?}", settled.put_back);
        }
    }
}

/// Every config row moved onto every default key of every other claimant,
/// in its own spelling, with its letters lowercased and with nvim's other
/// name for each `<CR>`, `<BS>` and `<Del>`: a key another
/// feature holds in the same scope puts the row back with one notice, and
/// a key held only in another scope is the row's to take.
#[test]
fn every_view_key_moved_onto_another_keeps_its_default_and_says_so() {
    let base = desktop_alt();
    let baseline = base.settle();
    assert!(baseline.notices.is_empty(), "{:?}", baseline.notices);
    let defaults = held(&baseline);
    let claimants = base.claimants();
    let (mut pairs, mut variants, mut aliases, mut apart) = (0, 0, 0, 0);
    for (m, moved) in claimants.iter().enumerate() {
        let Some(row) = moved.row else { continue };
        let own: Vec<_> = moved.defaults.iter().map(|d| lhs_keys(d, None)).collect();
        for (h, holder) in claimants.iter().enumerate().filter(|(h, _)| *h != m) {
            for key in &holder.defaults {
                let canonical = lhs_keys(key, None);
                if own.contains(&canonical) {
                    continue;
                }
                let sharing: Vec<_> = claimants
                    .iter()
                    .enumerate()
                    .filter(|(i, c)| {
                        *i != m && c.scope & moved.scope != 0 && c.canonical.contains(&canonical)
                    })
                    .map(|(i, _)| i)
                    .collect();
                let mut spellings = vec![key.clone(), lowercased(key), aliased(key)];
                spellings.sort();
                spellings.dedup();
                for spelled in spellings {
                    let mut case = base.clone();
                    if !case.move_row(row, &spelled) {
                        continue;
                    }
                    let name = format!("{row:?} onto `{}`'s {spelled}", holder.name);
                    let settled = case.settle();
                    if sharing.is_empty() {
                        assert!(settled.notices.is_empty(), "{name}: {:?}", settled.notices);
                        assert!(settled.put_back.is_empty(), "{name}");
                        apart += 1;
                        continue;
                    }
                    assert_eq!(
                        held(&settled),
                        defaults,
                        "{name}: every key back on its default"
                    );
                    let back = PutBack {
                        table: row.0,
                        key: row.1,
                        value: moved.defaults.join(", "),
                    };
                    assert_eq!(settled.put_back, [back], "{name}");
                    assert_eq!(settled.notices.len(), 1, "{name}: {:?}", settled.notices);
                    let notice = &settled.notices[0];
                    assert!(
                        notice.contains(&format!("= \"{spelled}\"")),
                        "{name}: {notice}"
                    );
                    let stays = format!("`{}` stays", moved.name);
                    assert!(notice.contains(&stays), "{name}: {notice}");
                    if sharing == [h] {
                        let named = format!("`{}` already", holder.name);
                        assert!(notice.contains(&named), "{name}: {notice}");
                    }
                    pairs += 1;
                    if spelled != *key {
                        variants += 1;
                    }
                    if spelled == aliased(key) && spelled != *key {
                        aliases += 1;
                    }
                }
            }
        }
    }
    assert!(pairs > 40 * claimants.len(), "{pairs} pairs reached");
    assert!(
        variants > 20 * claimants.len(),
        "{variants} variants reached"
    );
    assert!(apart > 100, "{apart} cross-scope moves reached");
    assert!(aliases > 0, "{aliases} other names reached");
}

/// Every two config rows in one scope moved onto one key: a key nobody
/// holds goes to the first, and a key a third feature holds by default
/// stays with it whatever the order, both notices naming it.
#[test]
fn two_rows_moved_onto_one_key_leave_it_to_its_holder_or_the_first() {
    let base = desktop_alt();
    let claimants = base.claimants();
    let free = "<M-F9>";
    assert!(claimants
        .iter()
        .all(|c| !c.canonical.contains(&lhs_keys(free, None))));
    let (mut pairs, mut three_way) = (0, 0);
    for (a, first) in claimants.iter().enumerate() {
        let Some(first_row) = first.row else { continue };
        for (b, second) in claimants.iter().enumerate().skip(a + 1) {
            let Some(second_row) = second.row else {
                continue;
            };
            let scope = first.scope & second.scope;
            if scope == 0 {
                continue;
            }
            let mut case = base.clone();
            if !case.move_row(first_row, free) || !case.move_row(second_row, free) {
                continue;
            }
            let name = format!("{first_row:?} and {second_row:?} onto {free}");
            let settled = case.settle();
            let rows: Vec<_> = settled.put_back.iter().map(|p| (p.table, p.key)).collect();
            assert_eq!(rows, [second_row], "{name}");
            assert_eq!(settled.notices.len(), 1, "{name}: {:?}", settled.notices);
            let named = format!("`{}` already", first.name);
            assert!(settled.notices[0].contains(&named), "{name}");
            pairs += 1;

            let taken: Vec<_> = first.canonical.iter().chain(&second.canonical).collect();
            let holder = claimants.iter().enumerate().rev().find(|(h, c)| {
                *h != a
                    && *h != b
                    && c.scope & scope != 0
                    && c.canonical.first().is_some_and(|k| !taken.contains(&k))
            });
            let Some((_, holder)) = holder else { continue };
            let key = &holder.defaults[0];
            let mut case = base.clone();
            if !case.move_row(first_row, key) || !case.move_row(second_row, key) {
                continue;
            }
            let name = format!(
                "{first_row:?} and {second_row:?} onto `{}`'s {key}",
                holder.name
            );
            let settled = case.settle();
            assert_eq!(settled.put_back.len(), 2, "{name}: {:?}", settled.put_back);
            assert_eq!(settled.notices.len(), 2, "{name}: {:?}", settled.notices);
            let named = format!("`{}` already", holder.name);
            for notice in &settled.notices {
                assert!(notice.contains(&named), "{name}: {notice}");
            }
            three_way += 1;
        }
    }
    assert!(pairs > 500, "{pairs} pairs reached");
    assert!(three_way > 500, "{three_way} three-way cases reached");
}

#[test]
fn the_sidebar_keys_are_held_exactly_while_a_surface_answers_them() {
    for (tree, notifications, ai, held) in [
        (true, false, false, true),
        (false, true, false, true),
        (false, false, true, true),
        (false, false, false, false),
    ] {
        let mut case = Fixture::new(KeyProfile::Editor, DesktopModifier::Alt);
        let toml = format!("[native]\ntree = {tree}\nnotifications = {notifications}\n");
        case.cfg = NativeConfig::from_toml_str(&toml).unwrap();
        case.ai = ai;
        assert!(case.move_row(("keys", "resize_mode"), "<S-Right>"));
        let settled = case.settle();
        let name = format!("tree {tree}, notifications {notifications}, ai {ai}");
        assert_eq!(
            !settled.notices.is_empty(),
            held,
            "{name}: {:?}",
            settled.notices
        );
    }
}

#[test]
fn swapped_keys_a_disabled_feature_s_key_and_a_chain_settle_as_written() {
    let at = |key: &str| UI_KEYS.iter().position(|ui| ui.key == key).unwrap();
    let defaults = KeysConfig::default().ui_lhs().clone();
    let (gaps, scrub) = (
        defaults[at("toggle_gaps")].clone(),
        defaults[at("dvr_scrub")].clone(),
    );
    let lhs = |settled: &Settled, verb: &str| {
        settled
            .specs
            .iter()
            .filter(|s| s.verb == verb && s.feature != "window")
            .map(|s| s.lhs.to_string())
            .collect::<Vec<_>>()
    };

    let mut case = desktop_alt();
    assert!(case.move_row(("keys", "toggle_gaps"), &scrub));
    assert!(case.move_row(("keys", "dvr_scrub"), &gaps));
    let settled = case.settle();
    assert!(settled.notices.is_empty(), "{:?}", settled.notices);
    assert!(lhs(&settled, "scrub").contains(&gaps));

    let mut case = desktop_alt();
    case.cfg = NativeConfig::from_toml_str("[native]\npicker = false\n").unwrap();
    assert!(case.move_row(("keys", "dvr_scrub"), "<leader>ff"));
    let settled = case.settle();
    assert!(settled.notices.is_empty(), "{:?}", settled.notices);
    assert_eq!(lhs(&settled, "scrub"), ["<leader>ff"]);

    // a default put back is the key another row was moved onto
    let mut case = desktop_alt();
    assert!(case.move_row(("keys", "toggle_gaps"), &scrub));
    assert!(case.move_row(("keys", "dvr_scrub"), "<leader>ff"));
    let settled = case.settle();
    assert_eq!(settled.notices.len(), 2, "{:?}", settled.notices);
    assert_eq!(lhs(&settled, "scrub"), [scrub]);
    assert!(lhs(&settled, "gaps").contains(&gaps));

    // a buffer key and a key the mode answers everywhere share the buffer
    let mut case = desktop_alt();
    assert!(case.move_row(("keys", "key_log"), "<c-g>"));
    assert!(case.move_row(("keys", "resize_mode"), "<C-g>"));
    let settled = case.settle();
    assert_eq!(settled.notices.len(), 1, "{:?}", settled.notices);
}

/// `toml` resolved with no flag and no environment variable.
fn resolved(toml: &str) -> ResolvedConfig {
    let file = crate::config::ViewConfig::from_toml_str(toml).unwrap();
    crate::config::resolve_with(&file, &crate::config::Overrides::default(), &|_| None)
}

#[test]
fn every_row_a_run_puts_back_prints_its_default_with_the_notice_under_it() {
    // `auto` derives the editor profile on macOS and Windows, which
    // registers no desktop chord to collide with.
    const ON: &str = "dvr.enabled = true\nkeys.profile = \"desktop\"\n";
    let defaults = resolved(ON);
    let printed = |config: &ResolvedConfig, row: (&str, &str)| {
        config
            .held_rows(DesktopModifier::Alt, true, None)
            .0
            .into_iter()
            .find(|(key, _, _)| (key.table, key.key) == row)
            .map(|(_, value, source)| (value, source))
    };
    let all = desktop_alt().claimants();
    let mut walked = 0;
    let rows: Vec<_> = all.iter().filter_map(|c| c.row).collect();
    for moved in all.iter().filter(|c| c.row.is_some()) {
        let Some(row) = moved.row else { continue };
        // the first holder's key the row's own value accepts
        let held = all
            .iter()
            .filter(|h| h.name != moved.name && shares_scope(h, moved))
            .filter_map(|h| h.keys.first())
            .filter(|k| !moved.canonical.contains(&lhs_keys(k, None)))
            .find_map(|key| {
                let toml = format!("{ON}{}.{} = {key:?}\n", row.0, row.1);
                let config = resolved(&toml);
                let put_back = config.held_keys(DesktopModifier::Alt, true, None).put_back;
                put_back
                    .iter()
                    .any(|back| (back.table, back.key) == row)
                    .then_some((key.clone(), config))
            });
        let Some((key, config)) = held else {
            panic!("[{}] {} takes no key another feature holds", row.0, row.1);
        };
        let notices = config.held_rows(DesktopModifier::Alt, true, None).1;
        assert_eq!(
            printed(&config, row),
            Some((moved.defaults.join(", "), Source::Derived)),
            "[{}] {} = {key:?} prints the default the run holds",
            row.0,
            row.1
        );
        assert_eq!(
            printed(&defaults, row).map(|(value, _)| value),
            Some(moved.defaults.join(", ")),
            "[{}] {} unmoved",
            row.0,
            row.1
        );
        let entry = format!("[{}] {} = {key:?}", row.0, row.1);
        assert!(
            notices.iter().any(|n| n.contains(&entry)),
            "{entry}: {notices:?}"
        );
        walked += 1;
    }
    assert_eq!(walked, rows.len());
    assert!(walked > 20, "{walked} rows walked");
}

#[test]
fn dvr_scrub_on_the_gaps_key_prints_its_own_default() {
    let config = resolved("[dvr]\nenabled = true\n[keys]\ndvr_scrub = \"<leader>ug\"\n");
    let (rows, notices) = config.held_rows(DesktopModifier::Alt, true, None);
    let row = rows
        .iter()
        .find(|(key, _, _)| (key.table, key.key) == ("keys", "dvr_scrub"))
        .unwrap();
    assert_eq!((row.1.as_str(), row.2), ("<leader>fv", Source::Derived));
    assert_eq!(notices.len(), 1, "{notices:?}");
}

#[test]
fn two_defaults_on_one_key_leave_it_to_the_first_and_say_so() {
    let surface = |name: &str, keys: &[&str], scope| {
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_string()).collect();
        Claimant {
            row: None,
            name: name.to_string(),
            keys: keys.clone(),
            canonical: Vec::new(),
            defaults: keys,
            default_canonical: Vec::new(),
            scope,
            spec: None,
            action: None,
        }
        .canonicalized(None)
    };
    let mut claimants = [
        surface("first", &["<C-x>"], SURFACES),
        surface("buffer", &["<c-x>"], BUFFER),
        surface("second", &["<c-x>", "<C-y>"], SURFACES),
    ];
    let (notices, put_back) = settle_claimants(&mut claimants);
    assert_eq!(
        notices,
        ["view: `first` and `second` both hold <c-x> by default. `first` keeps it this run"]
    );
    assert!(put_back.is_empty());
    assert_eq!(claimants[1].keys, ["<c-x>"], "another scope keeps its key");
    assert_eq!(claimants[2].keys, ["<C-y>"]);
}

/// A row that keeps one of its own defaults beside a free key holds that
/// default against a row moved onto it, and keeps the key it added.
#[test]
fn a_row_on_one_of_its_own_defaults_keeps_it_against_a_row_moved_onto_it() {
    let config = resolved(
        "[keys]\nsidebar_wider = \"<C-w><lt>\"\nsidebar_narrower = [\"<C-w><lt>\", \"<M-.>\"]\n",
    );
    let settled = config.held_keys(DesktopModifier::Alt, true, None);
    let rows: Vec<_> = settled.put_back.iter().map(|p| (p.table, p.key)).collect();
    assert_eq!(rows, [("keys", "sidebar_wider")], "{:?}", settled.notices);
    assert_eq!(settled.notices.len(), 1, "{:?}", settled.notices);
    assert!(
        settled.notices[0].contains("`sidebar narrower` already"),
        "{}",
        settled.notices[0]
    );
    assert_eq!(
        settled
            .bindings
            .spellings(Action::Resize(Direction::Narrower)),
        ["<C-w><lt>", "<M-.>"]
    );
}

/// A row written with the key `<leader>` stands for collides with a
/// default written with `<leader>`, once the leader is known.
#[test]
fn a_key_spelled_with_the_leader_s_own_key_is_the_leader_key() {
    let mut case = desktop_alt();
    assert!(case.move_row(("keys", "toggle_gaps"), "<Space>fv"));
    let settled = case.settle();
    assert!(settled.notices.is_empty(), "{:?}", settled.notices);
    case.leader = Some(" ".into());
    let settled = case.settle();
    let rows: Vec<_> = settled.put_back.iter().map(|p| (p.table, p.key)).collect();
    assert_eq!(rows, [("keys", "toggle_gaps")], "{:?}", settled.notices);
    assert_eq!(settled.notices.len(), 1, "{:?}", settled.notices);
    assert!(
        settled.notices[0].contains("`dvr scrub` already"),
        "{}",
        settled.notices[0]
    );
}

/// Two spellings nvim stores as one key collide, and two it stores as two
/// keys do not.
#[test]
fn a_key_nvim_stores_once_collides_and_two_it_stores_apart_do_not() {
    for (modifier, key, collides) in [
        (DesktopModifier::Alt, "<M-Enter>", true),
        (DesktopModifier::Super, "<D-Enter>", true),
        (DesktopModifier::Super, "<C-D-a>", false),
        (DesktopModifier::Alt, "<C-M-S-a>", false),
    ] {
        let mut case = Fixture::new(KeyProfile::Desktop, modifier);
        assert!(case.move_row(("keys", "toggle_gaps"), key));
        let settled = case.settle();
        assert_eq!(
            settled.notices.len(),
            usize::from(collides),
            "{modifier:?} {key}: {:?}",
            settled.notices
        );
    }
    let mut case = Fixture::new(KeyProfile::Desktop, DesktopModifier::Super);
    assert!(case.move_row(("keys", "key_log"), "<S-D-BackSpace>"));
    assert_eq!(case.settle().notices.len(), 1);
}

/// Every listing spells each chord no layer moved with the modifier the
/// run holds it under, a row put back onto a chord's key beside it.
#[test]
fn a_listing_spells_every_unmoved_chord_with_the_modifier_the_run_holds() {
    for modifier in [DesktopModifier::Alt, DesktopModifier::Super] {
        let chord = desktop_chord("focus_right").unwrap().lhs(modifier);
        let config = resolved(&format!(
            "[keys]\nprofile = \"desktop\"\ntoggle_gaps = {chord:?}\n"
        ));
        let (rows, notices) = config.held_rows(modifier, true, None);
        assert_eq!(notices.len(), 1, "{modifier:?}: {notices:?}");
        let mut chords = 0;
        for (key, value, source) in rows
            .iter()
            .filter(|(key, _, _)| key.table == "keys.desktop")
        {
            let want = desktop_chord(key.key).unwrap().lhs(modifier);
            assert_eq!(
                (value.as_str(), *source),
                (want, Source::Derived),
                "{modifier:?}: keys.desktop.{}",
                key.key
            );
            chords += 1;
        }
        assert_eq!(chords, DESKTOP_CHORD_COUNT);
    }
}

/// A `[keys]` value the resolver refused prints as the default the run
/// holds in its place, for every `[keys]` key.
#[test]
fn a_keys_value_the_resolver_refused_prints_as_the_default_it_is() {
    let defaults = resolved("");
    let row = |config: &ResolvedConfig, key: &str| {
        config
            .held_rows(DesktopModifier::Alt, true, None)
            .0
            .into_iter()
            .find(|(row, _, _)| (row.table, row.key) == ("keys", key))
            .map(|(_, value, source)| (value, source))
    };
    let keys: Vec<&str> = KEY_ACTIONS
        .iter()
        .map(|(key, _)| *key)
        .chain(UI_KEYS.iter().map(|ui| ui.key))
        .collect();
    for key in &keys {
        let config = resolved(&format!("[keys]\n{key} = 42\n"));
        let (value, _) = row(&defaults, key).unwrap();
        assert_eq!(row(&config, key), Some((value, Source::Derived)), "{key}");
    }
    let config = resolved("[keys]\nresize_mode = \"<leader>ff\"\n");
    assert_eq!(
        row(&config, "resize_mode").map(|(_, source)| source),
        Some(Source::Derived)
    );
    assert_eq!(keys.len(), KEY_ACTIONS.len() + UI_KEYS.len());
}
