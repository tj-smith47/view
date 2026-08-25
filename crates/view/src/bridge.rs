//! Persists the cold-start theme cache when the user changes colorscheme
//! mid-session, so the next launch's first paint already wears the scheme
//! they are actually using rather than the one they had when view last
//! exited normally.
//!
//! Nothing about a colorscheme switch is visible in the redraw stream on its
//! own -- a highlight batch looks the same whether a plugin redefined one
//! group or the whole scheme changed -- so the fact arrives out of band, as
//! `Msg::ColorSchemeChanged` from the `view_bridge` autocmd group.
//!
//! The colors it announces are not attached to it, and neither one reliably
//! precedes the other. nvim writes the notification before it flushes the
//! redraw that carries the new highlights, but redraw damage reaches the
//! runtime loop coalesced behind a wakeup token, and a token already queued
//! when those highlights are folded delivers them ahead of the announcement
//! still waiting its turn in the same channel. Both orders are ordinary, and
//! a cache write that assumed either one would silently stop happening on
//! the other. Hence a state machine that only ever writes a highlight state
//! it has actually seen, on both edges, and disarms on the first
//! highlight-bearing batch after the announcement -- so the write path is
//! reachable for one batch per switch and never for the steady state.
//!
//! The announcement is not the only way in, because it is not reliable
//! enough to be. It is a best-effort notification, and its position in the
//! stream relative to the highlights it announces is not fixed. The case
//! that made this matter is a session whose colorscheme is chosen by the
//! user's own config: the announcement arrives with an *older* probe
//! generation already confirmed, the one armed batch that follows it is
//! still carrying the previous scheme's colors, and disarming there leaves
//! the cache holding a theme the user never saw for the rest of the
//! session. So a probe reply is a write edge on its own, announced or not.
//! It answers a `default_colors_set` -- a scheme change, never steady-state
//! traffic -- which is what keeps that unconditional edge off the paint
//! path, and it is the last edge any scheme that moves the default
//! background produces, so the theme it writes is the applied one.
//!
//! One window is inverted rather than closed by that, and it is worth
//! knowing before reading the crash case as fully covered. A session whose
//! config selects a scheme confirms nvim's *default* palette first, and
//! that probe reply is a write edge like any other -- so for the few
//! hundred milliseconds until the scheme's own colors settle, the cache
//! holds the default theme over the good one the previous session left.
//! A crash inside that window costs the user their cached theme for one
//! launch. Nothing here can avoid it: at that point no view code knows a
//! scheme is coming. It is strictly shorter than the window it replaced,
//! where the wrong theme stood for the whole session and only the exit
//! path repaired it.

use std::path::{Path, PathBuf};

use view_core::events::UiEvent;
use view_core::model::Model;
use view_core::msg::{Effect, Msg};
use view_core::theme::Theme;

/// What `msg` means to the theme cache, read from the message before
/// `update()` consumes it and applied after, so the write sees the highlight
/// state the message produced rather than the state that preceded it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    /// Nothing the cache reacts to.
    None,
    /// The user changed colorscheme. Says nothing about whether the
    /// colors it announces have been applied yet.
    Switched,
    /// Highlight state actually moved: a batch redefining highlights, or a
    /// probe answering for one. Either can be the one that completes a
    /// pending switch.
    Applied,
}

/// Whether `event` can change what `Theme::from_hl` derives.
fn redefines_highlights(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::HlAttrDefine { .. }
            | UiEvent::HlGroupSet { .. }
            | UiEvent::DefaultColorsSet { .. }
    )
}

/// Whether an announced switch still owes the cache a write.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// No switch is outstanding.
    Idle,
    /// A switch was announced and has not yet been met by a highlight batch
    /// whose colors were readable.
    Open,
}

/// One session's theme-cache writer.
pub(crate) struct ThemeBridge {
    /// The cache file this session writes, or `None` for a session with no
    /// config path or no resolvable state directory. Resolved once here
    /// rather than per write, so no loop pass reads the environment.
    target: Option<PathBuf>,
    /// Whether an announced switch is still outstanding.
    pending: Pending,
    /// The theme last written to `target`. A switch that resolves to colors
    /// already on disk is not a write, which is what keeps the two edges
    /// below from costing two writes whenever they see the same state.
    written: Option<Theme>,
}

impl ThemeBridge {
    /// The writer for `config_path`'s cache slot.
    pub(crate) fn new(config_path: Option<&Path>) -> Self {
        Self {
            target: config_path.and_then(crate::theme_cache::cache_target),
            pending: Pending::Idle,
            written: None,
        }
    }

    /// What `msg` means to this bridge, read before `update()` consumes the
    /// message and applied after, so the write sees the highlight state the
    /// message produced rather than the state that preceded it.
    ///
    /// A redraw batch completes a switch only when it redefines highlights.
    /// The vast majority do not -- cursor motion, grid lines, mode changes,
    /// tabline updates -- and treating those as [`Trigger::Applied`] would
    /// close an announced switch on whichever frame happened to arrive next
    /// rather than on the one carrying its colors.
    ///
    /// With no switch outstanding, no batch can complete one, so the steady
    /// state settles this on the message's own discriminant and never walks
    /// a frame's events. That ordering is the cost bound: every frame view
    /// paints passes through here, and a scan proportional to batch size on
    /// each of them would be paint-path work bought for a state the session
    /// spends almost none of its life in.
    pub(crate) fn classify(&self, msg: &Msg) -> Trigger {
        match msg {
            Msg::ColorSchemeChanged { .. } => Trigger::Switched,
            // above the outstanding-switch gate, unlike every other edge: a
            // probe reply answers a `default_colors_set`, so it is the one
            // message that cannot arrive without the colors having moved,
            // and the cache has to reach it whether or not the announcement
            // that moved them was seen (see the module docs)
            Msg::HlProbeReply { .. } => Trigger::Applied,
            _ if self.pending != Pending::Open => Trigger::None,
            Msg::Redraw(events) if events.iter().any(redefines_highlights) => Trigger::Applied,
            _ => Trigger::None,
        }
    }

    /// Advances the outstanding switch by `trigger`, writing the cache
    /// whenever the highlight state it can see is settled and differs from
    /// what is already on disk.
    ///
    /// Both edges attempt a write because either can be the one carrying the
    /// new colors (see the module docs). The announcement's own attempt
    /// persists whatever is applied at that moment: in the order where the
    /// colors arrived first that is already the new theme, and in the order
    /// where they have not arrived yet it re-persists the theme the cache
    /// was holding anyway, which costs a write and cannot regress the file's
    /// contents.
    ///
    /// Only the highlight-bearing edge closes the switch, and it closes on
    /// the first one it sees rather than on the first one that wrote. That
    /// bound is what keeps the write path off the steady state: an
    /// announcement leaves the bridge armed for exactly one highlight batch,
    /// so a config that redefines a chrome group on every window or mode
    /// change cannot turn each of those into a synchronous cache write.
    ///
    /// A probe reply writes with no switch outstanding at all, which is what
    /// keeps an early close from losing a scheme: whatever the armed batch
    /// was carrying, the reply settling the new default colors arrives after
    /// it and persists them. `classify` is what keeps that free in the
    /// steady state -- an unannounced *redraw* never reaches
    /// [`Trigger::Applied`] at all -- so the gate below is on the write
    /// being worth doing, never on a switch being outstanding.
    ///
    /// Returns whatever effect a failed write owes the engine (a native
    /// notice reporting the failure), the same "return it, never push it
    /// directly" contract [`crate::native::NativeSession::load`] follows --
    /// this runs on the runtime loop's own dispatch seam, where the caller
    /// (`runtime::dispatch`) already has the executor these effects need to
    /// run through.
    pub(crate) fn follow_up(&mut self, model: &mut Model, trigger: Trigger) -> Vec<Effect> {
        if self.target.is_none() {
            return Vec::new();
        }
        match trigger {
            Trigger::None => Vec::new(),
            // an announcement arriving mid-switch does not stack: what gets
            // persisted is the scheme that ends up applied, and a user
            // cycling schemes quickly must not leave the cache holding one
            // they passed through
            Trigger::Switched => {
                self.pending = Pending::Open;
                let (_, effects) = self.persist(model);
                effects
            }
            Trigger::Applied => {
                let (settled, effects) = self.persist(model);
                if settled == Settled::Yes {
                    self.pending = Pending::Idle;
                }
                effects
            }
        }
    }

    /// Writes `model`'s current theme when the applied colors are
    /// unambiguous and not already on disk, reporting whether they were
    /// readable at all.
    ///
    /// The probe gate is what makes a persisted theme the applied one rather
    /// than a blend. A colorscheme that changes the default background emits
    /// `default_colors_set`, whose background is wire-ambiguous at zero (see
    /// `RpcCall::GetDefaultHl`), and opens a fresh probe generation to settle
    /// it; a write issued before that reply lands would pair the new
    /// foreground with the previous background and persist a theme that never
    /// existed. A switch that never opens a new generation -- a scheme
    /// redefining only named groups -- passes this test immediately, since
    /// the standing reply is still current.
    ///
    /// Colors that were readable but identical to what is on disk still
    /// report [`Settled::Yes`]: a scheme re-selected while already cached has
    /// finished just as completely as one that changed something, and
    /// reporting otherwise would leave the switch armed over a session that
    /// has nothing further to say.
    ///
    /// `store_to_path` runs synchronously on the runtime loop thread here,
    /// not off-loaded: it is bounded to one small TOML write per switch (see
    /// this type's own doc comment for why the write path never reaches the
    /// steady state), so its cost is one file write on an already-rare,
    /// user-initiated event rather than per-frame paint-path work. A write
    /// failure never reaches stderr directly -- the terminal may be
    /// mid-frame when this runs -- it is turned into a native notice through
    /// `model.engine.record_native_notice` instead, returned for the caller
    /// to run through the loop's own effect executor.
    fn persist(&mut self, model: &mut Model) -> (Settled, Vec<Effect>) {
        let hl = model.engine.hl();
        if hl
            .confirmed()
            .is_none_or(|probe| probe.generation != hl.probe_generation())
        {
            return (Settled::No, Vec::new());
        }
        let theme = Theme::from_hl(hl);
        if self.written == Some(theme) {
            return (Settled::Yes, Vec::new());
        }
        let Some(path) = &self.target else {
            return (Settled::Yes, Vec::new());
        };
        crate::vlog::log_with("theme", || {
            format!("caching applied colorscheme to {}", path.display())
        });
        let notice = crate::theme_cache::store_to_path(theme, path);
        self.written = Some(theme);
        let effects = match notice {
            Some(text) => model.engine.record_native_notice(text, false),
            None => Vec::new(),
        };
        (Settled::Yes, effects)
    }
}

/// Whether the highlight state a write attempt looked at was readable, which
/// is the only thing that keeps an announced switch outstanding.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Settled {
    /// A probe is still outstanding, so the colors cannot be read without
    /// blending the new scheme's foreground into the old background.
    No,
    /// The colors were readable, whether or not they differed from disk.
    Yes,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use view_core::theme::{ChromeGroup, ResolvedStyle};
    use view_core::update::update;

    /// A bridge writing to `path`, bypassing the environment lookup
    /// [`ThemeBridge::new`] performs so a test never depends on process-wide
    /// state another test may be mutating.
    fn bridge_writing_to(path: &Path) -> ThemeBridge {
        ThemeBridge {
            target: Some(path.to_path_buf()),
            pending: Pending::Idle,
            written: None,
        }
    }

    /// A scratch cache file for one test, named for it so two tests never
    /// read each other's cache.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("view-bridge-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory must be creatable");
        dir.join("theme.toml")
    }

    /// A model whose highlight table has already settled on one probe, the
    /// ordinary steady state a colorscheme switch interrupts.
    fn settled_model() -> Model {
        let mut m = Model::with_term_size(80, 24);
        let _ = update(
            &mut m,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0x00ff_ffff),
                bg: Some(0x0000_0000),
                sp: None,
            }]),
        );
        let generation = m.engine.hl().probe_generation();
        let _ = update(
            &mut m,
            Msg::HlProbeReply {
                generation,
                fg: Some(0x00ff_ffff),
                bg: Some(0x0000_0000),
            },
        );
        m
    }

    /// The batch nvim sends to define one named group, so the theme the
    /// bridge derives is one the derivation path produced.
    fn define_group_batch(group: ChromeGroup, hl_id: u64, fg: u32) -> Msg {
        Msg::Redraw(vec![
            UiEvent::HlAttrDefine {
                id: hl_id,
                fg: Some(fg),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
            UiEvent::HlGroupSet {
                name: group.hl_name().to_string(),
                hl_id,
            },
        ])
    }

    /// Applies one named group to `model`, leaving the trigger to the
    /// caller.
    fn define_group(model: &mut Model, group: ChromeGroup, hl_id: u64, fg: u32) {
        let _ = update(model, define_group_batch(group, hl_id, fg));
    }

    /// Each highlight-bearing batch shape, wrapped in an ordinary frame's
    /// traffic so a classifier that only looked at the first event would be
    /// caught.
    fn hl_bearing_batches() -> Vec<Msg> {
        [
            UiEvent::HlGroupSet {
                name: "TabLine".into(),
                hl_id: 4,
            },
            UiEvent::DefaultColorsSet {
                fg: Some(1),
                bg: Some(2),
                sp: None,
            },
            UiEvent::HlAttrDefine {
                id: 4,
                fg: Some(1),
                bg: None,
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
        ]
        .into_iter()
        .map(|event| Msg::Redraw(vec![UiEvent::GridClear { grid: 1 }, event]))
        .collect()
    }

    /// An ordinary frame: the traffic a session is made almost entirely of.
    fn ordinary_frame() -> Msg {
        Msg::Redraw(vec![
            UiEvent::GridCursorGoto {
                grid: 1,
                row: 3,
                col: 7,
            },
            UiEvent::ModeChange {
                mode: "insert".into(),
                mode_idx: 1,
            },
        ])
    }

    /// Dispatches `msg` the way `runtime::dispatch` does -- classified
    /// before `update()` consumes it, followed up after -- so a test
    /// exercises the ordering the loop really has rather than a trigger the
    /// test chose itself.
    fn dispatch(bridge: &mut ThemeBridge, model: &mut Model, msg: Msg) {
        let trigger = bridge.classify(&msg);
        let _ = update(model, msg);
        let _ = bridge.follow_up(model, trigger);
    }

    #[test]
    fn only_a_switch_or_a_probe_reply_opens_a_write_on_an_idle_bridge() {
        let bridge = bridge_writing_to(Path::new("/nonexistent/theme.toml"));
        assert!(
            bridge.classify(&Msg::ColorSchemeChanged {
                name: "dracula".into()
            }) == Trigger::Switched
        );
        assert!(bridge.classify(&Msg::RedrawReady) == Trigger::None);
        assert!(
            bridge.classify(&Msg::HlProbeReply {
                generation: 1,
                fg: None,
                bg: None
            }) == Trigger::Applied,
            "a probe reply answers a default-colors change, so it is a write edge with or without an announcement"
        );
    }

    /// The classification the write path's whole bound rests on: with a
    /// switch outstanding, only a batch that can move the derived theme
    /// completes it. Treating an ordinary frame as applied would close the
    /// switch before its colors arrived.
    #[test]
    fn only_a_batch_that_redefines_highlights_completes_an_outstanding_switch() {
        let mut bridge = bridge_writing_to(Path::new("/nonexistent/theme.toml"));
        bridge.pending = Pending::Open;

        assert!(bridge.classify(&ordinary_frame()) == Trigger::None);
        assert!(bridge.classify(&Msg::Redraw(Vec::new())) == Trigger::None);
        assert!(
            bridge.classify(&Msg::HlProbeReply {
                generation: 1,
                fg: None,
                bg: None
            }) == Trigger::Applied
        );
        for batch in hl_bearing_batches() {
            assert!(
                bridge.classify(&batch) == Trigger::Applied,
                "a batch redefining highlights must complete the switch: {batch:?}"
            );
        }
    }

    /// Highlight batches arrive throughout a session with no colorscheme
    /// switch behind them -- any `:hi`, any plugin restyling a group -- and
    /// none of them is the cache's business. This is also the behavioural
    /// half of the cost bound: the answer is settled before the events are
    /// reachable, so the steady state never walks a batch.
    #[test]
    fn an_idle_bridge_reacts_to_no_redraw_a_switch_did_not_announce() {
        let bridge = bridge_writing_to(Path::new("/nonexistent/theme.toml"));
        assert!(bridge.pending == Pending::Idle);
        for batch in hl_bearing_batches() {
            assert!(
                bridge.classify(&batch) == Trigger::None,
                "an idle bridge reacted to a batch no switch announced: {batch:?}"
            );
        }
    }

    /// The announcement can precede the colors it announces, and a cache
    /// written on the announcement alone would then hold the theme the user
    /// just left. The batch that follows has to supersede it.
    #[test]
    fn the_colors_that_follow_an_announcement_supersede_what_it_cached() {
        let path = scratch("after-colors");
        let mut bridge = bridge_writing_to(&path);
        let mut model = settled_model();

        let _ = bridge.follow_up(&mut model, Trigger::Switched);

        define_group(&mut model, ChromeGroup::StatusLine, 42, 0x0011_2233);
        let _ = bridge.follow_up(&mut model, Trigger::Applied);

        let (cached, _) = crate::theme_cache::load_from_path(&path);
        let cached = cached.expect("the applied switch must have been cached");
        assert_eq!(
            cached.chrome(ChromeGroup::StatusLine),
            ResolvedStyle {
                fg: Some(0x0011_2233),
                // the group defined no background of its own, so the
                // derivation falls back to the session's default
                bg: Some(0x0000_0000),
                bold: false,
                italic: false,
                underline: false,
                reverse: false,
            },
            "the cached theme must be the one the redraw applied"
        );
    }

    /// The other arrival order, and the one a live session hits whenever a
    /// wakeup token is already queued when the new highlights are folded:
    /// the colors are applied before the announcement is seen at all. A
    /// bridge that only ever wrote on a batch *following* an announcement
    /// would never write again for the rest of that session.
    #[test]
    fn colors_applied_before_the_announcement_are_still_cached() {
        let path = scratch("colors-first");
        let mut bridge = bridge_writing_to(&path);
        let mut model = settled_model();

        dispatch(
            &mut bridge,
            &mut model,
            define_group_batch(ChromeGroup::TabLineFill, 7, 0x00ff_00ff),
        );
        assert!(
            !path.exists(),
            "nothing had announced a switch yet, so nothing may be persisted"
        );

        let _ = bridge.follow_up(&mut model, Trigger::Switched);

        let (cached, _) = crate::theme_cache::load_from_path(&path);
        let cached =
            cached.expect("an announcement whose colors already landed must still cache them");
        assert_eq!(
            cached.chrome(ChromeGroup::TabLineFill).fg,
            Some(0x00ff_00ff)
        );
    }

    /// A frame that redefined no highlight must not close the switch: the
    /// batch carrying the new scheme may still be behind it. The frame is
    /// routed through `classify` here exactly as the runtime loop routes
    /// it, because classification is the thing that decides this.
    #[test]
    fn a_batch_that_redefines_nothing_leaves_the_switch_outstanding() {
        let path = scratch("unchanged-batch");
        let mut bridge = bridge_writing_to(&path);
        let mut model = settled_model();

        let _ = bridge.follow_up(&mut model, Trigger::Switched);

        let frame = ordinary_frame();
        let carried = bridge.classify(&frame);
        let _ = update(&mut model, frame);
        let _ = bridge.follow_up(&mut model, carried);
        assert!(
            bridge.pending == Pending::Open,
            "a frame carrying no highlights closed the switch before its colors arrived"
        );

        define_group(&mut model, ChromeGroup::MsgArea, 11, 0x0000_beef);
        let _ = bridge.follow_up(&mut model, Trigger::Applied);
        let (cached, _) = crate::theme_cache::load_from_path(&path);
        let cached = cached.expect("the batch that did move the colors must have been cached");
        assert_eq!(cached.chrome(ChromeGroup::MsgArea).fg, Some(0x0000_beef));
        assert!(bridge.pending == Pending::Idle);
    }

    /// The write path is reachable for one batch per switch, not for the
    /// rest of the session. A config that redefines a chrome group per
    /// window or per mode keeps highlight batches coming indefinitely, and
    /// a bridge left armed would put a synchronous file write on every one
    /// of them, inside the runtime loop.
    #[test]
    fn a_closed_switch_ignores_every_later_theme_change() {
        let path = scratch("stays-closed");
        let mut bridge = bridge_writing_to(&path);
        let mut model = settled_model();

        let _ = bridge.follow_up(&mut model, Trigger::Switched);
        define_group(&mut model, ChromeGroup::StatusLine, 3, 0x0011_2233);
        let _ = bridge.follow_up(&mut model, Trigger::Applied);
        assert!(
            bridge.pending == Pending::Idle,
            "the switch's own highlight batch must have closed it"
        );

        for id in 20u32..25 {
            dispatch(
                &mut bridge,
                &mut model,
                define_group_batch(ChromeGroup::StatusLine, u64::from(id), 0x00aa_bb00 + id),
            );
        }

        let (cached, _) = crate::theme_cache::load_from_path(&path);
        let cached = cached.expect("the announced switch must have been cached");
        assert_eq!(
            cached.chrome(ChromeGroup::StatusLine).fg,
            Some(0x0011_2233),
            "a theme change no colorscheme switch announced was written to the cache"
        );
    }

    /// A scheme that moves the default background leaves the background
    /// wire-ambiguous until the probe answers. A theme persisted in between
    /// pairs the new foreground with the old background.
    #[test]
    fn a_switch_awaiting_its_probe_is_not_cached_until_the_probe_answers() {
        let path = scratch("awaiting-probe");
        let mut bridge = bridge_writing_to(&path);
        let mut model = settled_model();

        let _ = bridge.follow_up(&mut model, Trigger::Switched);
        let _ = update(
            &mut model,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0x00ab_cdef),
                bg: Some(0),
                sp: None,
            }]),
        );
        let _ = bridge.follow_up(&mut model, Trigger::Applied);
        let (still_ambiguous_cache, _) = crate::theme_cache::load_from_path(&path);
        assert!(
            still_ambiguous_cache.is_none_or(|cached| cached.fg != Some(0x00ab_cdef)),
            "the background is still ambiguous: this write would persist a theme that never existed"
        );

        let generation = model.engine.hl().probe_generation();
        let _ = update(
            &mut model,
            Msg::HlProbeReply {
                generation,
                fg: Some(0x00ab_cdef),
                bg: Some(0x0044_5566),
            },
        );
        let _ = bridge.follow_up(&mut model, Trigger::Applied);

        let (cached, _) = crate::theme_cache::load_from_path(&path);
        let cached = cached.expect("the answered probe must have released the write");
        assert_eq!(cached.bg, Some(0x0044_5566));
        assert_eq!(cached.fg, Some(0x00ab_cdef));
    }

    /// Every frame applies highlight state; a redraw no switch announced
    /// may not write. A bridge writing on ordinary redraw traffic would put
    /// a file write on the paint path. Routed through `classify`, because
    /// that is the gate: `follow_up` is handed whatever the loop's own
    /// classification produced, never a trigger a caller picked.
    #[test]
    fn ordinary_redraw_traffic_never_writes_the_cache() {
        let path = scratch("no-switch");
        let mut bridge = bridge_writing_to(&path);
        let mut model = settled_model();
        for id in 1..8 {
            dispatch(
                &mut bridge,
                &mut model,
                define_group_batch(ChromeGroup::TabLine, id, 0x0000_1111),
            );
        }
        assert!(
            !path.exists(),
            "nothing announced a switch, so nothing may be persisted"
        );
    }

    /// The first run every user has: a config that selects a colorscheme at
    /// startup, so the derived theme exists long before any exit path could
    /// persist it. The probe that settles the scheme's default colors is
    /// what caches it, with nothing announced -- a crash before exit must
    /// not cost the user their theme, and the next launch's first paint has
    /// to wear the scheme they actually use.
    #[test]
    fn a_first_run_probe_reply_caches_the_theme_with_no_switch_announced() {
        let path = scratch("first-run-probe");
        let mut bridge = bridge_writing_to(&path);
        let mut model = Model::with_term_size(80, 24);

        dispatch(
            &mut bridge,
            &mut model,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0x00f8_f8f2),
                bg: Some(0x0028_2a36),
                sp: None,
            }]),
        );
        assert!(
            !path.exists(),
            "the background is still wire-ambiguous, so nothing may be persisted yet"
        );

        let generation = model.engine.hl().probe_generation();
        dispatch(
            &mut bridge,
            &mut model,
            Msg::HlProbeReply {
                generation,
                fg: Some(0x00f8_f8f2),
                bg: Some(0x0028_2a36),
            },
        );

        let (cached, _) = crate::theme_cache::load_from_path(&path);
        let cached = cached.expect("the confirmed probe must have cached the derived theme");
        assert_eq!(cached.bg, Some(0x0028_2a36));
        assert_eq!(cached.fg, Some(0x00f8_f8f2));
    }

    /// The write bound, now that the announcement is no longer the only
    /// door in. Every `default_colors_set` opens a probe, and every probe
    /// reply is a write edge -- a plugin re-asserting the same palette, a
    /// scheme reloaded, a background re-set on focus all repeat one -- so
    /// the only thing between that stream and a synchronous TOML write per
    /// message on the loop thread is the theme this bridge last wrote.
    #[test]
    fn a_probe_reply_that_settles_the_theme_already_written_writes_nothing() {
        let path = scratch("unchanged-probe");
        let mut bridge = bridge_writing_to(&path);
        let mut model = Model::with_term_size(80, 24);

        dispatch(
            &mut bridge,
            &mut model,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0x00f8_f8f2),
                bg: Some(0x0028_2a36),
                sp: None,
            }]),
        );
        let generation = model.engine.hl().probe_generation();
        let reply = Msg::HlProbeReply {
            generation,
            fg: Some(0x00f8_f8f2),
            bg: Some(0x0028_2a36),
        };
        dispatch(&mut bridge, &mut model, reply.clone());
        assert!(path.exists(), "the confirmed probe must have written once");

        // removed rather than compared: a second write of identical content
        // leaves a byte-identical file, so the file's absence is the only
        // thing that can tell "wrote again" from "did not"
        std::fs::remove_file(&path).expect("the written cache must be removable");
        dispatch(&mut bridge, &mut model, reply);

        assert!(
            !path.exists(),
            "a probe reply carrying the theme already written must not reach the disk"
        );
    }

    /// The arrival order that left a session's cache holding a theme its
    /// user never saw: the announcement lands while the *previous* scheme's
    /// probe is already confirmed, so it writes the previous theme and the
    /// one batch it arms is still carrying those same colors -- closing the
    /// switch before the new scheme's highlights have arrived at all. What
    /// makes the cache right anyway is the probe the new scheme's own
    /// default-colors change opens, which writes with no switch
    /// outstanding.
    #[test]
    fn a_switch_that_closed_early_is_still_corrected_by_the_probe_that_settles_it() {
        let path = scratch("closed-early");
        let mut bridge = bridge_writing_to(&path);
        let mut model = settled_model();

        dispatch(
            &mut bridge,
            &mut model,
            Msg::ColorSchemeChanged {
                name: "view-dracula".into(),
            },
        );
        dispatch(
            &mut bridge,
            &mut model,
            Msg::Redraw(vec![UiEvent::HlGroupSet {
                name: "TabLine".into(),
                hl_id: 4,
            }]),
        );
        assert!(
            bridge.pending == Pending::Idle,
            "the arrival order this test exists for is the one that closes the switch early"
        );

        dispatch(
            &mut bridge,
            &mut model,
            Msg::Redraw(vec![UiEvent::DefaultColorsSet {
                fg: Some(0x00f8_f8f2),
                bg: Some(0x0028_2a36),
                sp: None,
            }]),
        );
        let generation = model.engine.hl().probe_generation();
        dispatch(
            &mut bridge,
            &mut model,
            Msg::HlProbeReply {
                generation,
                fg: Some(0x00f8_f8f2),
                bg: Some(0x0028_2a36),
            },
        );

        let (cached, _) = crate::theme_cache::load_from_path(&path);
        let cached = cached.expect("the settled scheme must reach the cache");
        assert_eq!(
            cached.bg,
            Some(0x0028_2a36),
            "the cache kept the theme the switch was leaving, not the one it arrived at"
        );
        assert_eq!(cached.fg, Some(0x00f8_f8f2));
    }

    /// A user cycling schemes announces the next one before the previous
    /// one's colors land. What must survive is the scheme they stop on.
    #[test]
    fn a_second_switch_supersedes_one_whose_colors_never_landed() {
        let path = scratch("superseded");
        let mut bridge = bridge_writing_to(&path);
        let mut model = settled_model();

        let _ = bridge.follow_up(&mut model, Trigger::Switched);
        let _ = bridge.follow_up(&mut model, Trigger::Switched);

        define_group(&mut model, ChromeGroup::Pmenu, 9, 0x00fa_ce00);
        let _ = bridge.follow_up(&mut model, Trigger::Applied);
        let (cached, _) = crate::theme_cache::load_from_path(&path);
        let cached = cached.expect("the settled scheme must be cached");
        assert_eq!(cached.chrome(ChromeGroup::Pmenu).fg, Some(0x00fa_ce00));
    }

    /// A session started without a config file has no cache slot to write.
    #[test]
    fn a_session_with_no_config_path_writes_nothing_and_stays_idle() {
        let mut bridge = ThemeBridge::new(None);
        let mut model = settled_model();
        let _ = bridge.follow_up(&mut model, Trigger::Switched);
        let _ = bridge.follow_up(&mut model, Trigger::Applied);
        assert!(bridge.target.is_none());
        assert!(bridge.pending == Pending::Idle);
    }

    /// A mid-session write failure must reach the user through the model's
    /// own native-notice channel, never a direct stderr write -- this runs
    /// on the runtime loop while the terminal is raw-mode/alternate-screen
    /// owned, where a bare stderr write is invisible at best. The scratch
    /// "path" is planted as a directory rather than a file, so
    /// `store_to_path`'s directory-creation step succeeds (the parent
    /// already exists) but its own `fs::write` fails on the write step
    /// this test exercises.
    #[test]
    fn a_write_failure_surfaces_as_a_native_notice_not_stderr() {
        let path = scratch("write-failure");
        std::fs::create_dir_all(&path).expect("the planted directory must be creatable");
        let mut bridge = bridge_writing_to(&path);
        let mut model = settled_model();

        let effects = bridge.follow_up(&mut model, Trigger::Switched);

        assert!(
            !effects.is_empty(),
            "a failed write must return the notice's toast-expiry effect for the caller to run"
        );
        assert_eq!(
            model.engine.messages.entries.len(),
            1,
            "a failed write must record exactly one native notice"
        );
        let entry = &model.engine.messages.entries[0];
        assert_eq!(entry.kind, "native");
        assert!(
            entry
                .content
                .iter()
                .any(|(_, text)| text.contains("failed to write theme cache")),
            "the notice must name the failure, got {:?}",
            entry.content
        );
    }
}
