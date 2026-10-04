#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::config::KeysConfig;
use view_core::native::chords::desktop_chords;
use view_core::native::keys::key_tokens;

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
        .map(|s| canonical_keys(&s.lhs))
        .collect();
    let mut own: Vec<_> = KEY_ACTIONS
        .iter()
        .flat_map(|(_, action)| settled.bindings.spellings(*action))
        .map(|key| canonical_keys(&key))
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
/// in its own spelling and with its letters lowercased: a key another
/// feature holds in the same scope puts the row back with one notice, and
/// a key held only in another scope is the row's to take.
#[test]
fn every_view_key_moved_onto_another_keeps_its_default_and_says_so() {
    let base = desktop_alt();
    let baseline = base.settle();
    assert!(baseline.notices.is_empty(), "{:?}", baseline.notices);
    let defaults = held(&baseline);
    let claimants = base.claimants();
    let (mut pairs, mut variants, mut apart) = (0, 0, 0);
    for (m, moved) in claimants.iter().enumerate() {
        let Some(row) = moved.row else { continue };
        let own: Vec<_> = moved.defaults.iter().map(|d| canonical_keys(d)).collect();
        for (h, holder) in claimants.iter().enumerate().filter(|(h, _)| *h != m) {
            for key in &holder.defaults {
                let canonical = canonical_keys(key);
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
                let mut spellings = vec![key.clone(), lowercased(key)];
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
        .all(|c| !c.canonical.contains(&canonical_keys(free))));
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
    const ON: &str = "[dvr]\nenabled = true\n";
    let defaults = resolved(ON);
    let printed = |config: &ResolvedConfig, row: (&str, &str)| {
        config
            .held_rows(DesktopModifier::Alt, true)
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
            .filter(|k| !moved.canonical.contains(&same(k)))
            .find_map(|key| {
                let toml = format!("{ON}[{}]\n{} = {key:?}\n", row.0, row.1);
                let config = resolved(&toml);
                let put_back = config.held_keys(DesktopModifier::Alt, true).put_back;
                put_back
                    .iter()
                    .any(|back| (back.table, back.key) == row)
                    .then_some((key.clone(), config))
            });
        let Some((key, config)) = held else {
            panic!("[{}] {} takes no key another feature holds", row.0, row.1);
        };
        let notices = config.held_rows(DesktopModifier::Alt, true).1;
        assert_eq!(
            printed(&config, row),
            Some((moved.defaults.join(", "), Source::Derived)),
            "[{}] {} = {key:?} prints the default the run holds",
            row.0,
            row.1
        );
        // a chord's unmoved row prints the `super` spelling, the one this
        // resolver knows with no terminal probe
        if row.0 == "keys" {
            assert_eq!(
                printed(&defaults, row).map(|(value, _)| value),
                Some(moved.defaults.join(", "))
            );
        }
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
    let (rows, notices) = config.held_rows(DesktopModifier::Alt, true);
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
            scope,
            spec: None,
            action: None,
        }
        .canonicalized()
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
