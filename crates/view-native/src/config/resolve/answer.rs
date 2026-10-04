//! What a resolved configuration reports: every key's rendered value and
//! the layer that answered it.

use super::*;

impl ResolvedConfig {
    /// Every key, its resolved value rendered for display, and where it
    /// came from, in registry order. The doctor's config section is this
    /// walk; nothing re-derives the list.
    ///
    /// Every registry row whose table is not `[ai]`, minus
    /// `keys.desktop_modifier`, and no others: `[ai]` is parsed and
    /// resolved by the crate that owns it, and a caller that can name both
    /// crates appends its answers to these; `keys.desktop_modifier`'s real
    /// answer needs the terminal's own probe, which this resolver is never
    /// handed, so whoever holds the probe prints that one row itself,
    /// beside this walk.
    ///
    /// Printers outside this crate read
    /// [`ResolvedConfig::held_rows`](crate::config::ResolvedConfig::held_rows),
    /// which shows each key row as the key the run holds.
    #[must_use]
    pub(crate) fn rows(&self) -> Vec<(&'static ConfigKey, String, Source)> {
        keys()
            .iter()
            .filter_map(|key| self.answer(key).map(|(value, source)| (key, value, source)))
            .collect()
    }

    /// Whether `[native] tabline` is still the answer `[ui] panes` derived,
    /// so a `:View ui panes` flip derives it again. False once a flag, an
    /// environment variable or a `view.toml` line spelled the key.
    #[must_use]
    pub fn tabline_follows_look(&self) -> bool {
        registry::features()
            .iter()
            .position(|feature| feature.id == "tabline")
            .and_then(|index| self.native.get(index))
            .is_none_or(|source| *source == Source::Derived)
    }

    /// One line per environment value this session could not read. Empty
    /// whenever every `VIEW_*` in play said something view understood,
    /// which is the ordinary case.
    #[must_use]
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// One key's rendered value and layer, or `None` for a key another
    /// crate answers.
    fn answer(&self, key: &ConfigKey) -> Option<(String, Source)> {
        let ui = UI_KEYS.iter().position(|ui| ui.key == key.key);
        if let Some(&source) = ui.and_then(|at| self.ui_keys.get(at)) {
            if key.table == "keys" {
                return Some((self.tables.keys.lhs(key.key).to_string(), source));
            }
        }
        Some(match (key.table, key.key) {
            ("ui", "tier") => (
                self.ui
                    .tier
                    .value
                    .map_or(AUTO, TierChoice::label)
                    .to_string(),
                self.ui.tier.source,
            ),
            ("ui", "theme") => (
                self.ui.theme.value.clone().unwrap_or_else(|| AUTO.into()),
                self.ui.theme.source,
            ),
            ("ui", "panes") => (
                match self.ui.panes_marker {
                    // the marker is what makes a derived answer readable:
                    // "nvim" alone leaves a user to guess which of their
                    // environment said so
                    Some(marker) => format!("{} ({marker})", panes_label(self.ui.panes.value)),
                    None => panes_label(self.ui.panes.value).to_string(),
                },
                self.ui.panes.source,
            ),
            ("ui", "gaps") => (self.ui.gaps.value.to_string(), self.ui.gaps.source),
            ("ui", "fit_active") => (
                self.ui.fit_active.value.to_string(),
                self.ui.fit_active.source,
            ),
            ("ui", "pill_caps") => (
                self.ui
                    .pill_caps
                    .value
                    .map_or(AUTO, PillCaps::label)
                    .to_string(),
                self.ui.pill_caps.source,
            ),
            ("ui", "tree_icons") => (
                self.ui
                    .tree_icons
                    .value
                    .map_or(AUTO, TreeIcons::label)
                    .to_string(),
                self.ui.tree_icons.source,
            ),
            ("ui", "tile_titles") => (
                render_tile_titles(&self.ui.tile_titles.value),
                self.ui.tile_titles.source,
            ),
            ("ui.tokens", "accent") => (
                self.ui
                    .tokens
                    .value
                    .accent
                    .map_or_else(|| AUTO.to_string(), |rgb| format!("#{rgb:06x}")),
                self.ui.tokens.source,
            ),
            ("engine", "nvim_bin") => (
                self.engine
                    .nvim_bin
                    .value
                    .as_ref()
                    .map_or_else(|| BUNDLED.to_string(), |path| path.display().to_string()),
                self.engine.nvim_bin.source,
            ),
            ("engine", "appname") => (
                // an absent choice still runs the child under *some*
                // profile, and the profile it runs under is the answer a
                // report owes. An empty cell would read as "no appname",
                // which is not a state nvim has.
                self.engine
                    .appname
                    .value
                    .clone()
                    .or_else(|| self.inherited_appname.clone())
                    .unwrap_or_else(|| DEFAULT_APPNAME.to_string()),
                self.engine.appname.source,
            ),
            ("engine", "single_grid") => (
                self.engine.single_grid.value.to_string(),
                self.engine.single_grid.source,
            ),
            (table, "placement" | "anchor" | "size")
                if NativeSurface::ALL
                    .iter()
                    .any(|surface| surface.dotted_table() == table) =>
            {
                let surface = NativeSurface::ALL
                    .into_iter()
                    .find(|surface| surface.dotted_table() == table)?;
                let index = surface.index();
                let layout = self.surfaces[index];
                let source = self.surfaces_source[index];
                match key.key {
                    "placement" => (layout.placement.label().to_string(), source[0]),
                    "anchor" => (layout.anchor.label().to_string(), source[1]),
                    _ => (layout.size.to_string(), source[2]),
                }
            }
            ("native", "tree_width") => {
                (self.tables.native.tree_width.to_string(), self.tree_width)
            }
            ("native", "tabline_shows") => (
                self.tables.native.tabline_shows().label().to_string(),
                self.tabline_shows,
            ),
            ("keys", "profile") => (
                match self.profile_marker {
                    Some(marker) => format!("{} ({marker})", profile_label(self.profile.value)),
                    None => profile_label(self.profile.value).to_string(),
                },
                self.profile.source,
            ),
            // the real answer needs the terminal's own probe, which only
            // the caller holds, and that caller prints it
            // (`crates/view/src/main.rs`'s `caps_notice`)
            ("keys", "desktop_modifier") => return None,
            ("keys.desktop", id) => {
                let index = chords::desktop_chords().iter().position(|c| c.id == id)?;
                let row = &self.desktop[index];
                (row.value.clone(), row.source)
            }
            ("keys", name) => {
                let index = KEY_ACTIONS.iter().position(|(key, _)| *key == name)?;
                (
                    self.tables
                        .keys
                        .bindings()
                        .spellings(KEY_ACTIONS[index].1)
                        .join(", "),
                    self.keys[index],
                )
            }
            ("native", id) => (
                (!self.tables.native.disabled.contains(&id)).to_string(),
                *self
                    .native
                    .get(registry::features().iter().position(|f| f.id == id)?)?,
            ),
            ("supervision", "auto_restart") => (
                self.tables.supervision.auto_restart.to_string(),
                self.supervision,
            ),
            ("dvr", key) => return crate::config::dvr::report(key, &self.tables.dvr, self.dvr),
            _ => return None,
        })
    }
}
