//! The native feature session: what `view.toml` left switched on, the
//! takeover this session performs for it, and the one notice that says so.
//!
//! `update()` is pure and `view-core` cannot read a config file or write a
//! record, so the two steps a native feature owes a real session -- taking
//! its surfaces over once nvim has finished sourcing the user's config, and
//! introducing itself once -- hang off the two messages that mark those
//! moments rather than off a startup call nothing would sequence. Both
//! arrive through the ordinary dispatch path, so both are covered wherever
//! that path runs: the cutover replay resolves a `VimEnter` staged before the
//! loop started, and the loop itself resolves one that fires after.

use std::path::PathBuf;

use view_core::model::{Look, Model};
use view_core::msg::{Effect, EngineRequest, Msg, RpcCall, TakeoverStep};
use view_core::native::chords::{KeyProfile, ModifierChoice, DESKTOP_CHORD_COUNT};
use view_core::native::registry;
use view_native::config::profile;
#[cfg(test)]
use view_native::config::Source;
use view_native::config::{NativeConfig, Resolved, ResolvedConfig};
use view_native::report::report;
use view_native::supersede::{plan, Supersession};
use view_native::{mappings, toast};

/// Which native step, if any, a message owes beyond `update()`'s own answer
/// to it.
///
/// Read from the message before `update()` consumes it, and applied after,
/// so the takeover follows nvim's `VimEnter` reply rather than racing it and
/// the notice sees the claims `update()` has already recorded.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Nothing native is owed.
    None,
    /// The user's config has been sourced and `mapleader` is theirs: the
    /// moment every takeover this session performs is allowed to happen.
    VimEnter,
    /// The registration answered with what it claimed, which is the last
    /// fact the first-run notice was waiting on.
    Claims,
    /// The terminal answered the kitty keyboard protocol probe after the
    /// takeover already ran with `kitty_kbd` unknown (assumed `false`): the
    /// chords it registered may be spelled with the wrong modifier now that
    /// the true answer is in.
    CapsUpgraded,
    /// `Msg::FeatureInvoke { feature: "keys", .. }` reached `update()`,
    /// which may have moved `model.key_profile_override`.
    ProfileFlip,
    /// The bound on input held because it could begin a key not yet
    /// registered elapsed, for the hold armed with `generation`.
    HoldExpired { generation: u64 },
}

/// How long input that could begin a key the takeover left for the
/// follow-up registration may wait for nvim to run that registration before
/// it goes to nvim unmapped. The registration waits two
/// round trips behind `VimEnter` plus whatever startup work nvim has queued
/// ahead of it, and this leaves that wait a wide margin under a login
/// config while keeping a freeze behind a prompt nvim cannot leave short.
/// An expiry that finds the takeover still unanswered arms the bound again,
/// up to [`CHORD_HOLD_CEILING`]. Once the takeover answers, the bound is
/// armed again from that reply with the takeover reply time added, so an
/// engine across a slow link is given the time the registration still owes.
pub(crate) const CHORD_HOLD_BOUND: std::time::Duration = std::time::Duration::from_millis(300);

/// How long after `VimEnter` input may wait for a takeover that has not
/// answered. A single-grid session sends no sign of a prompt, so this is
/// how long a key typed into a prompt raised ahead of the takeover's reply
/// can be held.
pub(crate) const CHORD_HOLD_CEILING: std::time::Duration = std::time::Duration::from_secs(3);

/// The step `msg` owes, or [`Stage::None`].
pub(crate) fn stage(msg: &Msg) -> Stage {
    match msg {
        Msg::EngineRequest(EngineRequest::VimEnter { .. }) => Stage::VimEnter,
        Msg::MappingsClaimed { .. } => Stage::Claims,
        Msg::CapsUpgraded(_) => Stage::CapsUpgraded,
        Msg::FeatureInvoke { feature, .. } if feature == "keys" => Stage::ProfileFlip,
        Msg::ChordHoldExpired { generation } => Stage::HoldExpired {
            generation: *generation,
        },
        _ => Stage::None,
    }
}

/// The first keys of the keys a takeover left unmapped, which is all a key
/// typed before their registration answers can begin.
#[derive(Debug, Default)]
struct HoldStarts {
    /// The first key of every such key, spelled as [`first_key`] spells it.
    keys: Vec<String>,
    /// The modifier the desktop chords are spelled with, as it stands in
    /// notation (`d` or `m`), under the desktop profile. A chord key is held
    /// on its modifier alone, so a chord the terminal spells with its
    /// modifiers in another order still waits.
    modifier: Option<&'static str>,
}

impl HoldStarts {
    /// Whether `notation` could begin one of the unmapped keys.
    fn starts(&self, notation: &str) -> bool {
        let Some(key) = first_key(notation) else {
            return false;
        };
        if self.keys.contains(&key) {
            return true;
        }
        let modifiers = key
            .strip_prefix('<')
            .and_then(|name| name.strip_suffix('>'))
            .and_then(|name| name.rsplit_once('-'))
            .map_or("", |(modifiers, _)| modifiers);
        self.modifier
            .is_some_and(|wanted| modifiers.split('-').any(|m| m == wanted))
    }
}

/// The first key of `keys` in nvim key notation, one spelling per key: a
/// `<...>` name lowercased, and the name of a printable key written as the
/// character, which is how the terminal reader sends it unmodified.
fn first_key(keys: &str) -> Option<String> {
    let key = view_core::native::keys::key_tokens(keys).next()?;
    Some(
        view_core::native::keys::notation_char(key)
            .map_or_else(|| key.to_ascii_lowercase(), |c| c.to_string()),
    )
}

/// One session's native configuration and the plan it applies.
///
/// Built once, right after attach, so the config is read on the startup
/// thread rather than inside the loop, and the plan every consumer reads --
/// the takeover, the notice, and later doctor -- is the one this session
/// actually applied.
pub(crate) struct NativeSession {
    /// What the user left switched on.
    cfg: NativeConfig,
    /// The option surfaces this session takes over, in registry order.
    plan: Vec<Supersession>,
    /// The config file the answers came from, or `None` for a session
    /// running without one. Keys the first-run record, so a second config
    /// introduces itself on its own terms.
    config_path: Option<PathBuf>,
    /// Where the first-run record lives, or `None` for a machine with no
    /// resolvable state directory. Resolved once here rather than inside the
    /// notice, so no loop pass ever reads the environment.
    record: Option<PathBuf>,
    /// This connection's own channel, which the registered keys notify back
    /// over.
    channel_id: u64,
    /// Whether the takeover has already been emitted. `VimEnter` is a
    /// one-shot autocmd, so this guards a duplicate rather than a repeat:
    /// registering twice would answer the second pass with view's own
    /// mappings and report every key as one the user had.
    handed_over: bool,
    /// Snapshot of `model.ai_enabled` at construction, the same "read once
    /// before the loop, not per pass" rule every other field here follows.
    /// `mappings::register_plan` reads `NativeConfig` alone and has no `ai`
    /// switch to consult (`[ai]` is not a `[native]` key by design), so this
    /// crate -- which already knows `ai` by name through `view_ai::TrustStore`
    /// -- is where the registration plan's `ai` row is dropped when the
    /// feature is off, keeping `view-native` itself unaware of any feature
    /// beyond the generic registry/exemption predicate it already reads.
    ai_enabled: bool,
    /// The look the last takeover was built for: the one this session was
    /// loaded under until a `VimEnter` reads `model.look` again, so an
    /// engine restarted after a `:View ui` flip is held for the look on
    /// screen.
    look: Look,
    /// `[keys] toggle_gaps`/`cycle_surfaces`: the left-hand side to
    /// register `ui gaps`/`ui cycle_surfaces` under, applied to the built
    /// `RegisterMappings` spec in [`Self::build_mapping_call`] the same way
    /// `ai_enabled` is -- `view-native` resolves the override
    /// (`ResolvedConfig::tables.keys`), but the spec it hands back always
    /// carries `default_maps()`'s own compile-time `lhs`, which
    /// [`MappingSpec::lhs`](view_core::native::mappings::MappingSpec)'s
    /// `Cow<'static, str>` lets this override without leaking a `Box`.
    ui_keys_lhs: (String, String),
    /// `[keys] profile`, resolved once at startup: what `Stage::ProfileFlip`
    /// falls back to when `model.key_profile_override` is `None`, i.e. a
    /// flip back to `"auto"`.
    initial_profile: KeyProfile,
    /// This session's current profile: [`Self::initial_profile`] until a
    /// live flip (`:View keys profile ...`) moves it.
    profile: KeyProfile,
    /// The marker that decided [`Self::initial_profile`] under `"auto"`,
    /// for `:View keys profile`'s bare-verb report. Not re-derived after a
    /// live flip: a flip names its profile explicitly, so no marker
    /// decided it, and the report says so the same way
    /// [`view_native::config::ResolvedConfig::rows`] does for an explicit
    /// `view.toml`/environment answer.
    profile_marker: Option<&'static str>,
    /// `[keys] desktop_modifier`, still the raw choice -- the modifier a
    /// takeover actually spells its chords with also needs `model.caps`,
    /// which only exists once the terminal has answered.
    desktop_modifier_choice: ModifierChoice,
    /// `[keys.desktop]`'s resolved rows, in `chords::desktop_chords()`
    /// order, read once at startup the way every other field here is.
    desktop: [Resolved<String>; DESKTOP_CHORD_COUNT],
    /// Set by `take_over` when this session registers any key at all, the
    /// leader chords and the desktop chords, which it leaves out of the
    /// takeover's own `RegisterMappings` call: that call rides the batch
    /// ahead of the reply that frees nvim's blocked `VimEnter` and so counts
    /// toward nvim's own startup clock. Cleared the moment the follow-up
    /// that registers them is sent.
    chords_pending: bool,
    /// The leader nvim reported at `VimEnter`, which `<leader>` in a
    /// registered key stands for.
    leader: String,
    /// What input has to start with to wait for the follow-up registration
    /// ([`Self::holds`]), taken by [`Self::take_over`] from the keys it left
    /// for that registration.
    hold_starts: HoldStarts,
    /// How many `MappingsClaimed` replies are still owed for registrations
    /// sent while input was held, the one carrying the chords among them.
    /// nvim takes `nvim_input` into typeahead the moment it reads it and
    /// runs a registration later, from its main loop, so a chord written
    /// right behind the call that maps it still runs unmapped. Input waits
    /// for the reply instead, which nvim sends only once the registration
    /// has run.
    claims_owed: usize,
    /// The engine-bound effects of input and resizes that arrived while
    /// [`Self::holds_input`], in arrival order. A chord typed during launch
    /// would otherwise reach nvim ahead of its own mapping and run as nvim's
    /// bare keys ([`Self::hold_input`], [`Self::release_input`]).
    held_input: Vec<Effect>,
    /// Set when the hold ends before the registration answered: the bound
    /// elapsed, or nvim raised a message prompt it leaves only on a key.
    /// Input then goes to nvim as typed until a replacement engine's own
    /// takeover.
    hold_lifted: bool,
    /// The generation the newest [`CHORD_HOLD_BOUND`] timer was armed with,
    /// so a timer armed for a replaced engine releases nothing.
    hold_generation: u64,
    /// When the pass that started the hold sent the takeover, until the
    /// takeover's reply measures the takeover reply time from it: the link
    /// both ways plus whatever startup work nvim ran ahead of the reply.
    takeover_sent: Option<std::time::Instant>,
    /// The first-run record keys this session has already shown a notice
    /// for. Every `MappingsClaimed` reruns [`Self::announce`], and the chord
    /// follow-up adds one to every desktop startup, so a session with no
    /// record to consult would otherwise repeat each notice.
    announced: Vec<String>,
    /// The thread that writes the first-run record.
    writer: RecordWriter,
}

/// One write to the first-run record.
enum RecordWrite {
    /// One notice the model raised about the config.
    Key(String),
}

/// The first-run record's writer: one thread that owns every write, spawned
/// at the first write a session makes.
///
/// The record is read and written in full each time, and a write on the
/// dispatch thread would hold the frame behind a disk. One thread keeps the
/// writes in order, so two keys told in one launch never overwrite each
/// other.
///
/// Quitting waits for the writes still queued for at most
/// [`view_proc::writer::QUIT_WAIT`] and then leaves the thread to the
/// process exit. The record is replaced whole by a rename, so a write the
/// exit cuts short, or one a full queue refuses, loses only this launch's
/// additions, which costs the same notice once more next launch.
#[derive(Default)]
struct RecordWriter(
    Option<view_proc::writer::BackgroundWriter<RecordWrite, std::convert::Infallible>>,
);

/// How many record writes may wait for the disk. A session makes a few.
const QUEUED_RECORD_WRITES: usize = 64;

impl RecordWriter {
    fn send(
        &mut self,
        record: &std::path::Path,
        config: Option<&std::path::Path>,
        write: RecordWrite,
    ) {
        if self.0.is_none() {
            let owned_record = record.to_path_buf();
            let owned_config = config.map(std::path::Path::to_path_buf);
            let started = self.start_with(move |write| {
                apply_record_write(&owned_record, owned_config.as_deref(), write);
            });
            if let Err(err) = started {
                // a host out of threads still records, on this thread
                crate::vlog::log_with("native", || {
                    format!("first-run record thread failed: {err}")
                });
                apply_record_write(record, config, write);
                return;
            }
        }
        self.push(write);
    }

    /// Spawns the writer thread, which hands every write to `apply` in the
    /// order it was sent.
    fn start_with(
        &mut self,
        mut apply: impl FnMut(RecordWrite) + Send + 'static,
    ) -> std::io::Result<()> {
        self.0 = Some(view_proc::writer::BackgroundWriter::start(
            "first-run-record",
            QUEUED_RECORD_WRITES,
            move |write| {
                apply(write);
                Ok(())
            },
        )?);
        Ok(())
    }

    fn push(&mut self, write: RecordWrite) {
        let Some(writer) = &mut self.0 else {
            return;
        };
        match writer.try_send(write) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                crate::vlog::log("native", "first-run record queue full, a write dropped");
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                crate::vlog::log("native", "first-run record thread is gone");
            }
        }
    }

    /// Waits up to `wait` for every write handed over so far, and answers
    /// whether they all finished. A writer still busy is left running.
    fn finish_within(&mut self, wait: std::time::Duration) -> bool {
        let Some(writer) = &mut self.0 else {
            return true;
        };
        match writer.finish_within(wait) {
            view_proc::writer::Finished::Busy => {
                crate::vlog::log_with("native", || {
                    format!("first-run record writer still busy after {wait:?}, left to the exit")
                });
                false
            }
            _ => true,
        }
    }
}

impl Drop for RecordWriter {
    fn drop(&mut self) {
        self.finish_within(view_proc::writer::QUIT_WAIT);
    }
}

fn apply_record_write(
    record: &std::path::Path,
    config: Option<&std::path::Path>,
    write: RecordWrite,
) {
    let result = match write {
        RecordWrite::Key(key) => toast::record_key(config, &key, record),
    };
    if let Err(err) = result {
        crate::vlog::log_with("native", || format!("first-run record failed: {err}"));
    }
}

impl NativeSession {
    /// Folds `resolved` -- the one read of `config_path` this session
    /// performs, made in `main.rs` before the attach -- into `model` and
    /// into the plan this session applies.
    ///
    /// The value arrives already resolved rather than being read here
    /// because the `ext_*` set `nvim_ui_attach` requests follows the same
    /// `[native]` answers, and that decision is made before there is a
    /// channel for anything to notify back over. Reading the file a second
    /// time here would let one session hold two answers: a file edited or
    /// made unreadable in the window between the two reads would leave the
    /// attach and the takeover disagreeing about which surfaces view owns.
    ///
    /// Returns whatever effects the notices it raises owe the engine,
    /// rather than pushing them and discarding the return the way a bare
    /// `push_native` call would: they are discovered before `runtime::run`'s
    /// loop exists to run an effect through, so the caller (`main.rs`) is
    /// the one that knows whether that is "immediately, through the
    /// pre-cutover executor" or, for an even earlier failure, "once an
    /// executor exists at all" -- this method has no opinion on which and
    /// must not silently drop the effect deciding it does not apply yet.
    ///
    /// `record` is the first-run record
    /// ([`view_native::paths::first_run_record`]), or
    /// `None` for a machine with no state directory. The told keys under
    /// `config_path` are read from it here, once, and seeded onto `model`.
    pub(crate) fn load(
        resolved: ResolvedConfig,
        config_path: Option<PathBuf>,
        record: Option<PathBuf>,
        channel_id: u64,
        model: &mut Model,
    ) -> (Self, Vec<Effect>) {
        let mut effects = Vec::new();
        let initial_profile = resolved.profile.value;
        let profile_marker = resolved.profile_marker;
        let desktop_modifier_choice = resolved.desktop_modifier.value;
        let desktop = resolved.desktop.clone();
        let resolved = resolved.tables;
        let cfg = resolved.native;
        model.statusline_enabled = cfg.enabled("statusline");
        model.palette_enabled = cfg.enabled("palette");
        // one source for the tree's share: the caller has already put the
        // resolved `[ui.surfaces]` placements on the model, and that table's
        // `size` is where `[native] tree_width` has been folded in under its
        // older name. Reading the older key back here put the two in
        // disagreement, so an overlay tree ignored `size` outright
        model.tree_width_pct = model
            .surfaces
            .layout(view_core::native::geometry::NativeSurface::Tree)
            .size;
        // a width that could not be read is the one `[native]` mistake that
        // does not fail the table (see `resolve_tree_width`), so this is the
        // only place it can be said out loud
        if let Some(notice) = cfg.tree_width_notice() {
            model.dirty = true;
            effects.extend(model.engine.record_native_notice(notice.to_string(), false));
        }
        model.key_bindings = resolved.keys.bindings().clone();
        // an entry that named no key leaves its own action on the defaults
        // rather than failing the table (see `resolve_key_bindings`), so this
        // is the only place it can be said out loud
        for notice in resolved.keys.notices() {
            model.dirty = true;
            effects.extend(
                model
                    .engine
                    .record_native_notice((*notice).to_string(), false),
            );
        }
        model.supervision.auto_restart = resolved.supervision.auto_restart;
        // `ui_attach` already ran, at the raw terminal height, before this
        // config was even read (see `main.rs`'s call ordering), so nvim's
        // live grid still claims every row `statusline_rows()` now needs to
        // reserve. Without this, `view_surface::render` places the
        // statusline at `offset + grid_h` using nvim's still-full grid
        // height and paints it one row below the terminal entirely, same
        // shape as `update()`'s resize when the chrome row count moves.
        if model.statusline_rows() > 0 {
            let (grid_width, grid_height) = model.grid_target();
            effects.push(Effect::Rpc(RpcCall::TryResize {
                width: grid_width,
                height: grid_height,
            }));
        }
        // the model's own look, which `main.rs` set from the same resolved
        // `[ui]` table before this ran: two readings of one answer would
        // let the takeover hold `laststatus` for a look the frame is not
        // drawing
        let look = model.look;
        let plan = plan(&cfg, registry::features(), look);
        let ui_keys_lhs = (
            resolved.keys.gaps_lhs().to_string(),
            resolved.keys.cycle_lhs().to_string(),
        );
        // read once here, before the loop, so a channel report looks the
        // key up in memory and the record is touched only to add one
        if let Some(record) = &record {
            match toast::announced_keys(config_path.as_deref(), record) {
                Ok(keys) => model.seed_announced(keys),
                Err(err) => {
                    crate::vlog::log_with("native", || format!("first-run record failed: {err}"));
                }
            }
        }
        let session = Self {
            cfg,
            plan,
            config_path,
            record,
            channel_id,
            handed_over: false,
            ui_keys_lhs,
            ai_enabled: model.ai_enabled,
            look,
            initial_profile,
            profile: initial_profile,
            profile_marker,
            desktop_modifier_choice,
            desktop,
            chords_pending: false,
            leader: view_core::msg::DEFAULT_MAPLEADER.to_string(),
            hold_starts: HoldStarts::default(),
            claims_owed: 0,
            held_input: Vec::new(),
            hold_lifted: false,
            hold_generation: 0,
            takeover_sent: None,
            announced: Vec::new(),
            writer: RecordWriter::default(),
        };
        (session, effects)
    }

    /// Points this session at a replacement engine.
    ///
    /// A restarted engine is a different connection: it answers on its own
    /// channel, and it carries none of the mappings or option takeovers the
    /// one it replaced was given. So both facts this session holds about
    /// the connection are reset -- the channel the registered keys notify
    /// back over, and whether the takeover has been performed -- and the
    /// fresh engine's own `VimEnter` performs it again. The config, the
    /// plan and the first-run record are untouched: they are facts about
    /// the session, which is the thing that survived.
    pub(crate) fn rebind(&mut self, channel_id: u64) {
        self.channel_id = channel_id;
        self.handed_over = false;
        // input held for the connection that died was addressed to it, and
        // the replacement's own takeover decides afresh whether to hold
        self.chords_pending = false;
        self.claims_owed = 0;
        self.held_input.clear();
        self.hold_lifted = false;
        self.takeover_sent = None;
    }

    /// Whether the hold is in force: from the takeover that left the keys
    /// out until nvim has answered the registration that carries them, or
    /// until the hold is lifted ([`CHORD_HOLD_BOUND`],
    /// [`CHORD_HOLD_CEILING`], [`Self::note_redraw`]). What it holds is
    /// [`Self::holds`]'s answer.
    pub(crate) fn holds_input(&self) -> bool {
        !self.hold_lifted && (self.chords_pending || self.claims_owed > 0)
    }

    /// Whether `effect`, which input produced, waits for the registration:
    /// keys that could begin a key not yet mapped, since nvim would run them
    /// as its own, and anything behind held input, so input reaches nvim in
    /// the order it was typed. Every other key, a mouse event, a paste and
    /// a resize go to nvim at once.
    pub(crate) fn holds(&self, effect: &Effect) -> bool {
        if !self.holds_input() || !matches!(effect, Effect::Rpc(_)) {
            return false;
        }
        if !self.held_input.is_empty() {
            return true;
        }
        matches!(effect, Effect::Rpc(RpcCall::Input { notation }) if self.hold_starts.starts(notation))
    }

    /// Records the leader nvim reported at `VimEnter`, ahead of the
    /// takeover that reads it.
    pub(crate) fn note_vim_enter(&mut self, msg: &Msg) {
        if let Msg::EngineRequest(EngineRequest::VimEnter { leader, .. }) = msg {
            self.leader.clone_from(leader);
        }
    }

    /// The first keys of `specs`, `<leader>` read as [`Self::leader`], and
    /// under the desktop profile the modifier its chords are spelled with.
    fn starts_of(
        &self,
        specs: &[view_core::native::mappings::MappingSpec],
        model: &Model,
    ) -> HoldStarts {
        let leader = first_key(&self.leader);
        let keys = specs
            .iter()
            .filter_map(|spec| {
                let lhs = spec.lhs.as_ref();
                let leads = lhs
                    .get(..8)
                    .is_some_and(|head| head.eq_ignore_ascii_case("<leader>"));
                if leads {
                    leader.clone()
                } else {
                    first_key(lhs)
                }
            })
            .collect();
        let modifier =
            (self.profile == KeyProfile::Desktop).then(|| {
                match profile::modifier_for(self.desktop_modifier_choice, model.caps.kitty_kbd).0 {
                    view_core::native::chords::DesktopModifier::Super => "d",
                    _ => "m",
                }
            });
        HoldStarts { keys, modifier }
    }

    /// Ends the hold with the registration still unanswered, so the held
    /// input goes to nvim at the end of this pass and later input is not
    /// held. A chord among it runs as nvim's own keys, which is how every
    /// key reached nvim before the hold existed.
    fn lift_hold(&mut self, why: &str) {
        if self.holds_input() {
            self.hold_lifted = true;
            crate::vlog::log_with("native", || {
                format!("input hold lifted: {why} claims_owed={}", self.claims_owed)
            });
        }
    }

    /// Lifts the hold when `events` show nvim raising a hit-enter or more
    /// prompt: nvim runs no registration until a key dismisses it, and the
    /// key that would is one this hold keeps. A scrolled message area is
    /// the one sign of the prompt a UI that leaves messages to nvim is
    /// sent under multigrid. A single-grid session sends none: its hold
    /// ends at [`CHORD_HOLD_CEILING`] for a prompt raised ahead of the
    /// takeover's reply, and at the bound armed from that reply once it
    /// has come back.
    pub(crate) fn note_redraw(&mut self, events: &[view_core::events::UiEvent]) {
        if !self.holds_input() {
            return;
        }
        let prompt = events.iter().any(|event| {
            matches!(
                event,
                view_core::events::UiEvent::MsgSetPos { scrolled: true, .. }
            )
        });
        if prompt {
            self.lift_hold("nvim raised a message prompt");
        }
    }

    /// Queues `effect`, which input produced while [`Self::holds_input`],
    /// behind the chord registration's reply.
    pub(crate) fn hold_input(&mut self, effect: Effect) {
        self.held_input.push(effect);
    }

    /// The held input, in arrival order, once nothing holds it any more;
    /// empty while [`Self::holds_input`].
    pub(crate) fn release_input(&mut self) -> Vec<Effect> {
        if self.holds_input() {
            return Vec::new();
        }
        std::mem::take(&mut self.held_input)
    }

    /// Drops the held input: a pass that lost the connection or ended the
    /// session leaves nobody to write it to.
    pub(crate) fn drop_held_input(&mut self) {
        self.held_input.clear();
    }

    /// Carries out `stage` against `model`, returning whatever it owes the
    /// engine.
    #[must_use]
    pub(crate) fn follow_up(&mut self, model: &mut Model, stage: Stage) -> Vec<Effect> {
        self.follow_up_at(model, stage, std::time::Instant::now)
    }

    /// [`Self::follow_up`] reading the time from `clock`, which only the
    /// passes that arm or end a hold call, so a key never pays for it.
    fn follow_up_at(
        &mut self,
        model: &mut Model,
        stage: Stage,
        clock: impl Fn() -> std::time::Instant,
    ) -> Vec<Effect> {
        let holding = self.holds_input();
        let mut effects = self.follow_up_stage(model, stage);
        if holding || self.holds_input() {
            self.claims_owed += effects.iter().filter(|e| answers_with_claims(e)).count();
        }
        let takeover_answered = matches!(stage, Stage::Claims)
            .then(|| self.takeover_sent.take())
            .flatten();
        let expired = matches!(
            stage,
            Stage::HoldExpired { generation } if generation == self.hold_generation
        );
        // last, so the takeover batch still leads the pass that starts a
        // hold (`runtime::dispatch` splits the attach off at the first
        // effect that is not one)
        let bound = if !holding && self.holds_input() {
            self.takeover_sent = Some(clock());
            Some(CHORD_HOLD_BOUND)
        } else if !self.holds_input() {
            None
        } else if let Some(sent) = takeover_answered {
            Some(CHORD_HOLD_BOUND + clock().saturating_duration_since(sent))
        } else if expired {
            self.bound_after_expiry(clock())
        } else {
            None
        };
        if let Some(after) = bound {
            self.hold_generation += 1;
            effects.push(Effect::ScheduleChordHold {
                after,
                generation: self.hold_generation,
            });
        }
        effects
    }

    fn follow_up_stage(&mut self, model: &mut Model, stage: Stage) -> Vec<Effect> {
        match stage {
            Stage::None => Vec::new(),
            Stage::VimEnter => {
                crate::vlog::log("startup", "vim_enter received");
                let effects = self.take_over(model);
                crate::vlog::log_with("startup", || {
                    let batched: usize = effects
                        .iter()
                        .filter_map(|effect| match effect {
                            Effect::Rpc(RpcCall::Takeover { steps }) => Some(steps.len()),
                            _ => None,
                        })
                        .sum();
                    format!("takeover sent messages={} steps={batched}", effects.len())
                });
                effects
            }
            Stage::Claims => {
                self.claims_owed = self.claims_owed.saturating_sub(1);
                crate::vlog::log("startup", "takeover answered");
                crate::vlog::log_takeover("answered");
                // the answer to the last registration this session owes, so
                // no later claims answer adds to the launch box
                let settled = !self.chords_pending && self.claims_owed == 0;
                let mut effects = self.announce(model);
                effects.extend(self.follow_up_chords(model));
                if settled {
                    crate::vlog::log("startup", "claims settled");
                }
                effects
            }
            Stage::CapsUpgraded | Stage::ProfileFlip => self.reissue_mappings(model, stage),
            Stage::HoldExpired { .. } => Vec::new(),
        }
    }

    /// What the bound's expiry at `now` arms next. A takeover still
    /// unanswered is a slow engine or a slow link, so the hold waits on
    /// under a new bound until [`CHORD_HOLD_CEILING`] after `VimEnter`. An
    /// answered one has had its registration's time, and the hold lifts.
    fn bound_after_expiry(&mut self, now: std::time::Instant) -> Option<std::time::Duration> {
        let left = self
            .takeover_sent
            .map(|sent| CHORD_HOLD_CEILING.saturating_sub(now.saturating_duration_since(sent)))
            .filter(|left| !left.is_zero());
        if left.is_none() {
            self.lift_hold("bound elapsed");
        }
        left.map(|left| left.min(CHORD_HOLD_BOUND))
    }

    /// The leader and desktop chords `Self::take_over` left out of its own
    /// `RegisterMappings` call, sent once the takeover's registration has
    /// replied. That reply is written after the one that freed nvim's
    /// blocked `vim.rpcrequest`, so this call normally runs after nvim's
    /// startup clock stops. The `VimEnter` hook still waits in `vim.wait`
    /// until the attach is applied, and a follow-up that reaches nvim before
    /// the loop turn applying the attach runs inside that wait, on the
    /// startup clock. Sends the whole live set, the same call
    /// [`Self::reissue_mappings`] sends.
    ///
    /// A no-op once `Self::chords_pending` is false: nothing to send for a
    /// session with every key off (`Self::take_over` never sets it), and
    /// nothing left to send once this has already fired once, or a
    /// [`Stage::CapsUpgraded`]/[`Stage::ProfileFlip`] reissue beat it to it.
    fn follow_up_chords(&mut self, model: &mut Model) -> Vec<Effect> {
        if !std::mem::take(&mut self.chords_pending) {
            return Vec::new();
        }
        let (mapping_call, mut effects) = self.build_mapping_call(model);
        effects.insert(0, Effect::Rpc(mapping_call));
        effects
    }

    /// Rebuilds and resends this session's default-map registration outside
    /// the one-shot takeover, for the two moments the plan `take_over` sent
    /// can go stale after `VimEnter`: the kitty keyboard protocol probe
    /// answering late (`Stage::CapsUpgraded`), and a live `:View keys
    /// profile` flip (`Stage::ProfileFlip`, which also moves
    /// [`Self::profile`] first). Nothing to redo before the takeover has
    /// run once, since no registration exists yet for either to correct.
    ///
    /// The reissue is a second `RegisterMappings` on its own, outside any
    /// `Takeover` batch: `REGISTER_MAPPINGS_CHUNK` (`view-engine`'s
    /// `nvim_api/mappings.rs`) restores whatever it unmapped the call before,
    /// so resending it is the whole of "give the old plan back, then apply
    /// the new one."
    fn reissue_mappings(&mut self, model: &mut Model, stage: Stage) -> Vec<Effect> {
        if stage == Stage::ProfileFlip && model.key_profile_report_requested {
            model.key_profile_report_requested = false;
            return self.report_profile(model);
        }
        if !self.handed_over {
            return Vec::new();
        }
        if stage == Stage::ProfileFlip {
            let flipped = model.key_profile_override.unwrap_or(self.initial_profile);
            if flipped == self.profile {
                // a `:View keys ...` invoke with no profile in it (a typo, or
                // the same profile named twice) reaches this stage the same
                // way a real flip does; nothing changed, so nothing reissues.
                return Vec::new();
            }
            self.profile = flipped;
        }
        // this call already carries the live set whole, chords included, so
        // the deferred first-startup follow-up (`Self::follow_up_chords`)
        // owes nothing more if it has not fired yet
        self.chords_pending = false;
        let (mapping_call, mut effects) = self.build_mapping_call(model);
        effects.insert(0, Effect::Rpc(mapping_call));
        effects
    }

    /// `:View keys profile` with no argument: reports the live profile and
    /// modifier and changes neither, on the same wording
    /// [`view_native::config::ResolvedConfig::rows`] renders for the
    /// `keys.profile` config row. The marker is shown only while the live
    /// profile is still the one `"auto"` derived at startup -- a flip
    /// named its profile explicitly, so no marker decided it.
    fn report_profile(&self, model: &mut Model) -> Vec<Effect> {
        let marker = (self.profile == self.initial_profile)
            .then_some(self.profile_marker)
            .flatten();
        let (_, modifier_row, _) =
            profile::modifier_for(self.desktop_modifier_choice, model.caps.kitty_kbd);
        let text = format!(
            "view: keys.profile = {}; keys.desktop_modifier = {modifier_row}",
            view_native::config::profile_report_value(self.profile, marker)
        );
        model.engine.record_native_notice(text, false)
    }

    /// Builds this session's default-map registration: the plan's own
    /// enabled features, minus `ai` when the feature is off, the
    /// `[keys] toggle_gaps`/`cycle_surfaces` overrides, and the desktop
    /// chords this session's live profile and modifier put in play.
    ///
    /// Shared by [`Self::follow_up_chords`] and
    /// [`Self::reissue_mappings`], so a chord respelled once the terminal's
    /// kitty keyboard protocol probe answers, or a profile flipped
    /// mid-session, both travel through the one place that turns `self`'s
    /// resolved answers into a spec list. The second element is the notice
    /// [`profile::modifier_for`] owes when `[keys] desktop_modifier =
    /// "super"` is unreachable this run, empty otherwise.
    fn build_mapping_call(&self, model: &mut Model) -> (RpcCall, Vec<Effect>) {
        let (specs, super_notice) = self.live_specs(model);
        let notice_effects = match super_notice {
            Some(text) => model.engine.record_native_notice(text.to_string(), false),
            None => Vec::new(),
        };
        let mapping_call = RpcCall::RegisterMappings {
            specs,
            channel_id: self.channel_id,
        };
        (mapping_call, notice_effects)
    }

    /// Every key this session registers, in the order
    /// [`Self::build_mapping_call`] sends them, and the notice
    /// [`profile::modifier_for`] owes when a desktop chord among them falls
    /// back from an unreachable `super`. Reads `self` and `model` and changes
    /// neither, so [`Self::take_over`] asks it what the follow-up will map
    /// without raising that notice early.
    fn live_specs(
        &self,
        model: &Model,
    ) -> (
        Vec<view_core::native::mappings::MappingSpec>,
        Option<&'static str>,
    ) {
        let mut mapping_call = mappings::register_plan(&self.cfg, self.channel_id);
        // `[keys] toggle_gaps`/`cycle_surfaces`: `view-native` already
        // validated the override (`resolve_ui_lhs`). `MappingSpec::lhs` is
        // `Cow<'static, str>`, so the session-resolved value replaces the
        // compile-time default in place, with no `Box::leak` of the kind
        // `view-native`'s own `keys.rs` config registry still pays for a
        // resolved value with nowhere `'static` to live.
        //
        // Applied to `register_plan`'s own specs before the desktop chords
        // join the list: a chord's `(feature, verb)` names the same `ui`
        // `gaps`/`cycle_surfaces` pair its default-map twin does, spelled
        // under the OS chord it always keeps, so running this loop after
        // the chords were appended rewrote the chord's own `lhs` to the
        // default map's -- two specs claiming the one `lhs` in the same
        // registration, which left `REGISTER_MAPPINGS_CHUNK`'s pre-set
        // `maparg` snapshot for the second of them holding the first's own
        // fresh mapping where it should hold nothing, and a later reissue
        // read that snapshot back as a user mapping view had taken.
        if let RpcCall::RegisterMappings { specs, .. } = &mut mapping_call {
            let (gaps_lhs, cycle_lhs) = &self.ui_keys_lhs;
            for spec in specs.iter_mut() {
                if spec.feature == "ui"
                    && spec.verb == "gaps"
                    && gaps_lhs.as_str() != spec.lhs.as_ref()
                {
                    spec.lhs = std::borrow::Cow::Owned(gaps_lhs.clone());
                }
                if spec.feature == "ui"
                    && spec.verb == "cycle_surfaces"
                    && cycle_lhs.as_str() != spec.lhs.as_ref()
                {
                    spec.lhs = std::borrow::Cow::Owned(cycle_lhs.clone());
                }
            }
            // `[keys] resize_mode` names every key the mode answers to,
            // none included, so the one default spec becomes one per key
            let resize_keys = model
                .key_bindings
                .spellings(view_core::native::keys::Action::ResizeMode);
            let mut rebound = Vec::with_capacity(specs.len() + resize_keys.len());
            for spec in specs.drain(..) {
                if spec.feature == "window" && spec.verb == "resize_mode" {
                    rebound.extend(resize_keys.iter().map(|lhs| {
                        let mut spec = spec.clone();
                        spec.lhs = std::borrow::Cow::Owned(lhs.clone());
                        spec
                    }));
                } else {
                    rebound.push(spec);
                }
            }
            *specs = rebound;
        }
        let (modifier, _, super_notice) =
            profile::modifier_for(self.desktop_modifier_choice, model.caps.kitty_kbd);
        let chords = profile::chord_plan(&self.desktop, self.profile, modifier, &self.cfg);
        // Only owed when this call actually registers a desktop chord
        // under the fallback: a flip to `editor` (no chords at all) or a
        // reissue that keeps carrying the same fallback would otherwise
        // repeat the same notice on every one of them.
        let super_notice = super_notice.filter(|_| !chords.is_empty());
        let RpcCall::RegisterMappings { mut specs, .. } = mapping_call else {
            return (Vec::new(), None);
        };
        specs.extend(chords);
        // `NativeConfig::enabled("ai")` is unconditionally `true` -- `[ai]`
        // has no `[native]` switch by design, so `register_plan` alone would
        // always register the key. `model.ai_enabled` is the bit `[native]`
        // structurally cannot carry for this one feature, so it is applied
        // here, once, after the desktop chords have joined the list too.
        // `view-native` has no other reason to know the feature's name.
        if !self.ai_enabled {
            specs.retain(|spec| spec.feature != "ai");
        }
        (specs, super_notice)
    }

    /// Every takeover this session performs, then the registration of the
    /// `:View` command. The keys follow from `Stage::Claims`
    /// ([`Self::follow_up_chords`]).
    ///
    /// Options first: they are what nvim stops drawing, and issuing them
    /// ahead of a registration that answers asynchronously means the session
    /// is never briefly holding a key for a surface it has not taken yet.
    ///
    /// The clipboard provider registers unconditionally, unlike every option
    /// and mapping above: `"+yy`/`"+p` are core editing infrastructure, not a
    /// feature a config can decline the way it declines the picker or
    /// statusline, so this push does not read `self.cfg` or `self.plan` at
    /// all.
    ///
    /// The attach that externalizes the surfaces closes the sequence.
    /// `cmdheight` is the message area's alone and rides in its feature's
    /// plan entry: nvim zeroes the row for any UI attached with
    /// `ext_messages`, command line or not.
    fn take_over(&mut self, model: &mut Model) -> Vec<Effect> {
        if self.handed_over {
            return Vec::new();
        }
        self.handed_over = true;
        // a restarted engine is taken over for the look on screen, which a
        // `:View ui` flip may have moved since load
        self.look = model.look;
        self.plan = plan(&self.cfg, registry::features(), self.look);
        let mut effects: Vec<RpcCall> = Vec::new();
        // a plan entry with no call is a surface the attach already took
        // (`Supersession::rpc`); it is in the plan to be reported, not to be
        // performed
        effects.extend(self.plan.iter().filter_map(|entry| entry.rpc.clone()));
        // every key, leader chords and desktop chords alike, rides behind
        // the reply that frees `VimEnter` (`Self::follow_up_chords`, fired
        // from `Stage::Claims`): this batch is in force before nvim's own
        // startup clock stops, and each key costs a `maparg` snapshot and a
        // `nvim_set_keymap` there. The empty registration still carries the
        // `:View` command and the reply that fires `Stage::Claims`.
        let (specs, _) = self.live_specs(model);
        self.chords_pending = !specs.is_empty();
        self.hold_starts = self.starts_of(&specs, model);
        effects.push(RpcCall::RegisterMappings {
            specs: Vec::new(),
            channel_id: self.channel_id,
        });
        effects.push(RpcCall::RegisterClipboard {
            channel_id: self.channel_id,
        });
        crate::vlog::log_with("native", || {
            let (taken, look_held): (Vec<&Supersession>, Vec<&Supersession>) =
                self.plan.iter().partition(|e| e.announced);
            let taken: Vec<&str> = taken.iter().map(|e| e.feature).collect();
            let look_held: Vec<&str> = look_held.iter().map(|e| e.feature).collect();
            format!(
                "takeover options={taken:?} look_held={look_held:?} channel={}",
                self.channel_id
            )
        });
        // after every call above: nvim applies one connection's traffic in
        // the order it arrives, so the frame the attach produces is drawn
        // with this takeover already in force -- a `cmdheight` or a
        // `laststatus` landing after it would cost a second frame showing
        // the surface view had just taken. The batch goes out before the
        // reply that frees nvim, so it is in force when nvim reads that
        // reply, and the attach goes out behind it ([`Model::takes_attach`],
        // and `view::runtime`'s `dispatch` for the split).
        let mut effects = batched(effects);
        effects.extend(model.takes_attach().map(Effect::Rpc));
        // and last of all: `nvim_ui_set_option` is about a UI on this
        // channel and is refused where there is none. The claim is worth
        // nothing earlier anyway -- nvim's own tty defaults have finished
        // looking for a terminal by `VimEnter`, so claiming here buys
        // `ui_send` delivery without the startup query and keystroke-eating
        // wait that finding it earlier would have cost
        effects.push(Effect::Rpc(RpcCall::ClaimStdoutTty));
        effects
    }

    /// Records `key` as told under this session's config, for something the
    /// launch box named as it was raised ([`Effect::RecordAnnounced`]).
    ///
    /// A record that cannot be written is logged, which costs the same
    /// notice once more next launch.
    ///
    /// Latency consequence: the dispatch thread hands the key to the
    /// record's writer thread and touches no file. The first record write a
    /// session makes spawns that thread.
    pub(crate) fn record_announced(&mut self, key: &str) {
        self.write_record(RecordWrite::Key(key.to_string()));
    }

    /// Hands `write` to the record's writer thread.
    fn write_record(&mut self, write: RecordWrite) {
        let Some(record) = &self.record else {
            crate::vlog::log(
                "native",
                "no state directory: a told notice cannot be recorded",
            );
            return;
        };
        self.writer.send(record, self.config_path.as_deref(), write);
    }

    /// Adds whatever this session took over for the first time under this
    /// config to the launch's one notice.
    ///
    /// Options and keys come through one report, so the wording, the off
    /// switch and the record entry are the same mechanism for both. A record
    /// that cannot be written is logged and the notice shown anyway: the
    /// worst that costs is repeating it next launch, and a user who is
    /// never told what took their key is worse.
    ///
    /// Records exactly what the model's box named as it was raised
    /// ([`Effect::RecordAnnounced`]), which is beside a taken key or a held
    /// channel. The effects this returns go to the executor, which has no
    /// reach to the record, so those are recorded here.
    ///
    /// Latency consequence: the record write goes to the writer thread
    /// ([`Self::record_announced`]), so the dispatch thread builds the
    /// report and touches no file. A registration reply that claims nothing
    /// past `Self::announced` returns before the report reaches the model.
    fn announce(&mut self, model: &mut Model) -> Vec<Effect> {
        let mut handovers = report(&self.plan, model.claimed_keys(), registry::features());
        handovers.retain(|h| !self.announced.contains(&h.record_key()));
        if handovers.is_empty() {
            return Vec::new();
        }
        self.announced.extend(
            handovers
                .iter()
                .map(view_native::report::Handover::record_key),
        );
        let taken = handovers
            .iter()
            .map(|h| (h.record_key(), h.taken()))
            .collect();
        let mut effects = view_core::update::tell_taken_over(model, taken);
        effects.retain(|effect| match effect {
            Effect::RecordAnnounced { key } => {
                self.record_announced(key);
                false
            }
            _ => true,
        });
        effects
    }

    /// Waits up to [`view_proc::writer::QUIT_WAIT`] for every record write this session
    /// has handed over. Called on quit after the terminal is restored,
    /// since `std::process::exit` runs no destructor.
    pub(crate) fn finish_record(&mut self) {
        self.writer.finish_within(view_proc::writer::QUIT_WAIT);
    }
}

/// Whether nvim answers `effect` with a `Msg::MappingsClaimed`: a takeover
/// batch and a lone registration each answer with exactly one.
fn answers_with_claims(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::Rpc(RpcCall::Takeover { .. } | RpcCall::RegisterMappings { .. })
    )
}

/// Folds `calls` into as few round trips as the vocabulary allows, keeping
/// the order they were built in.
///
/// A call [`TakeoverStep::from_call`] has no step for cannot ride the batch,
/// so it closes whatever has accumulated and travels on its own: the engine
/// applies one connection's traffic in arrival order, and a call hoisted past
/// its neighbours would land against a session they had not configured yet.
fn batched(calls: Vec<RpcCall>) -> Vec<Effect> {
    let mut effects = Vec::new();
    let mut steps = Vec::new();
    let flush = |steps: &mut Vec<TakeoverStep>, effects: &mut Vec<Effect>| {
        if !steps.is_empty() {
            effects.push(Effect::Rpc(RpcCall::Takeover {
                steps: std::mem::take(steps),
            }));
        }
    };
    for call in calls {
        match TakeoverStep::from_call(&call) {
            Some(step) => steps.push(step),
            None => {
                flush(&mut steps, &mut effects);
                effects.push(Effect::Rpc(call));
            }
        }
    }
    flush(&mut steps, &mut effects);
    effects
}

#[cfg(test)]
/// [`NativeSession::ui_keys_lhs`]'s own default, for every test constructor
/// below that has no override of its own to resolve.
fn default_ui_keys_lhs() -> (String, String) {
    let lhs_for = |verb: &str| {
        view_core::native::mappings::default_maps()
            .iter()
            .find(|spec| spec.feature == "ui" && spec.verb == verb)
            .map_or_else(String::new, |spec| spec.lhs.to_string())
    };
    (lhs_for("gaps"), lhs_for("cycle_surfaces"))
}

#[cfg(test)]
/// Every desktop chord left to derive from [`chords::DesktopChord::lhs`]
/// under whatever modifier a takeover settles on, the same starting point
/// [`view_native::config::resolve`] gives a config that names no
/// `[keys.desktop]` overrides.
fn default_desktop() -> [Resolved<String>; DESKTOP_CHORD_COUNT] {
    std::array::from_fn(|_| Resolved::new(String::new(), Source::Derived))
}

#[cfg(test)]
impl NativeSession {
    /// A session that hands nothing over, for the tests whose subject is the
    /// dispatch path itself rather than what a native feature does on it.
    pub(crate) fn inert() -> Self {
        Self {
            cfg: NativeConfig::all_enabled(),
            plan: Vec::new(),
            config_path: None,
            record: None,
            channel_id: 0,
            handed_over: true,
            ai_enabled: true,
            look: Look::default(),
            ui_keys_lhs: default_ui_keys_lhs(),
            initial_profile: KeyProfile::Editor,
            profile: KeyProfile::Editor,
            desktop_modifier_choice: ModifierChoice::Auto,
            desktop: default_desktop(),
            profile_marker: None,
            chords_pending: false,
            leader: view_core::msg::DEFAULT_MAPLEADER.to_string(),
            hold_starts: HoldStarts::default(),
            claims_owed: 0,
            held_input: Vec::new(),
            hold_lifted: false,
            hold_generation: 0,
            takeover_sent: None,
            announced: Vec::new(),
            writer: RecordWriter::default(),
        }
    }

    /// A session with every feature on, reading no config file, recording
    /// its notices at `record`, notifying back over `channel_id`.
    pub(crate) fn all_enabled(channel_id: u64, record: Option<PathBuf>) -> Self {
        Self {
            cfg: NativeConfig::all_enabled(),
            plan: plan(
                &NativeConfig::all_enabled(),
                registry::features(),
                Look::default(),
            ),
            config_path: None,
            record,
            channel_id,
            handed_over: false,
            ai_enabled: true,
            look: Look::default(),
            ui_keys_lhs: default_ui_keys_lhs(),
            initial_profile: KeyProfile::Editor,
            profile: KeyProfile::Editor,
            desktop_modifier_choice: ModifierChoice::Auto,
            desktop: default_desktop(),
            profile_marker: None,
            chords_pending: false,
            leader: view_core::msg::DEFAULT_MAPLEADER.to_string(),
            hold_starts: HoldStarts::default(),
            claims_owed: 0,
            held_input: Vec::new(),
            hold_lifted: false,
            hold_generation: 0,
            takeover_sent: None,
            announced: Vec::new(),
            writer: RecordWriter::default(),
        }
    }

    /// [`Self::all_enabled`] under the desktop profile, every chord on its
    /// derived key.
    pub(crate) fn desktop(channel_id: u64, record: Option<PathBuf>) -> Self {
        Self {
            profile: KeyProfile::Desktop,
            initial_profile: KeyProfile::Desktop,
            ..Self::all_enabled(channel_id, record)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use view_core::msg::OptionValue;
    use view_core::msg::ReplyToken;
    use view_core::native::ext::Ext;
    use view_core::native::mappings::MappingClaim;

    /// `effects` with the takeover expanded back into one effect per call
    /// it batches, so an assertion about what a takeover performs reads the
    /// same whether or not those calls travelled as one message. The batch
    /// itself is pinned by
    /// `the_takeover_travels_as_one_round_trip_ahead_of_the_attach`.
    fn unbatched(effects: Vec<Effect>) -> Vec<Effect> {
        effects
            .into_iter()
            .flat_map(|effect| match effect {
                Effect::Rpc(RpcCall::Takeover { steps }) => steps
                    .into_iter()
                    .map(|step| Effect::Rpc(step.into_call()))
                    .collect(),
                other => vec![other],
            })
            .collect()
    }

    fn model() -> Model {
        Model::with_term_size(80, 24)
    }

    /// A mapping's first key reads the way the terminal reader sends it,
    /// through the tokenizer and character table every surface shares.
    #[test]
    fn a_mappings_first_key_is_spelled_as_the_reader_sends_it() {
        for (lhs, key) in [
            ("<space>ff", Some(" ")),
            ("<Space>", Some(" ")),
            ("<LT>x", Some("<")),
            ("<Bar>", Some("|")),
            ("<C->>x", Some("<c->>")),
            ("<C-W>v", Some("<c-w>")),
            ("gx", Some("g")),
            ("", None),
        ] {
            assert_eq!(first_key(lhs).as_deref(), key, "{lhs}");
        }
    }

    /// Every spec a session registers at startup: the takeover's own
    /// registration and the follow-up `Stage::Claims` sends, in that order.
    fn startup_specs(
        session: &mut NativeSession,
        m: &mut Model,
    ) -> Vec<view_core::native::mappings::MappingSpec> {
        [Stage::VimEnter, Stage::Claims]
            .into_iter()
            .flat_map(|stage| unbatched(session.follow_up(m, stage)))
            .filter_map(|e| match e {
                Effect::Rpc(RpcCall::RegisterMappings { specs, .. }) => Some(specs),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// The takeover's effects with the hold's timer left out, which is a
    /// local timer and nothing the takeover sends.
    fn sent(effects: Vec<Effect>) -> Vec<Effect> {
        effects
            .into_iter()
            .filter(|e| !matches!(e, Effect::ScheduleChordHold { .. }))
            .collect()
    }

    fn input(notation: &str) -> Effect {
        Effect::Rpc(RpcCall::Input {
            notation: notation.to_string(),
        })
    }

    fn leader_vim_enter(leader: &str) -> Msg {
        Msg::EngineRequest(EngineRequest::VimEnter {
            token: ReplyToken { msgid: 1 },
            leader: leader.to_string(),
        })
    }

    /// `load` over a config file read the way `main.rs` reads it, so these
    /// tests keep asserting from a path on disk rather than from a value
    /// they built by hand -- the parse is half of what they cover.
    fn load_from(
        config_path: Option<PathBuf>,
        channel_id: u64,
        model: &mut Model,
    ) -> (NativeSession, Vec<Effect>) {
        let file = view_native::config::ViewConfig::load(config_path.as_deref()).unwrap();
        let resolved =
            view_native::config::resolve(&file, &view_native::config::Overrides::default());
        // no record: a test reading or writing the user's own would pass
        // or fail on what that machine's launches have told
        NativeSession::load(resolved, config_path, None, channel_id, model)
    }

    /// A scratch record path for one test, named for it so two tests never
    /// read each other's record. The returned guard must outlive every use
    /// of the path: dropping it removes the directory the path points
    /// into.
    fn scratch(name: &str) -> (view_test_support::ScratchDir, PathBuf) {
        let dir = view_test_support::ScratchDir::new(&format!("native-{name}")).unwrap();
        let record = dir.join("first-run.toml");
        (dir, record)
    }

    /// Everything the message surface is currently showing, as one string.
    fn shown(model: &Model) -> String {
        model
            .engine
            .messages
            .entries
            .iter()
            .flat_map(|e| e.content().iter().map(|(_, t)| t.as_str()))
            .collect()
    }

    #[test]
    fn vim_enter_is_the_stage_that_hands_the_surfaces_over() {
        assert!(
            stage(&Msg::EngineRequest(EngineRequest::VimEnter {
                token: ReplyToken { msgid: 1 },
                leader: view_core::msg::DEFAULT_MAPLEADER.to_string(),
            })) == Stage::VimEnter
        );
        assert!(
            stage(&Msg::MappingsClaimed {
                claimed: Vec::new(),
                colon_mapped: false,
                generation: 0,
            }) == Stage::Claims
        );
        assert!(stage(&Msg::RedrawReady) == Stage::None);
    }

    /// `VimEnter` blocks nvim's own startup until view answers it, so what
    /// the takeover costs the user is the round trips it spends there, not
    /// the calls it performs. Every call the batch can carry rides one
    /// request; the attach and the tty claim cannot ride it (neither has a
    /// lua entry point) and follow it as their own notifications, behind
    /// the answer `view::runtime`'s `dispatch` writes between the two
    /// halves -- which
    /// `the_vim_enter_answer_is_written_between_the_takeover_and_the_attach`
    /// pins.
    #[test]
    fn the_takeover_travels_as_one_round_trip_ahead_of_the_attach() {
        let mut session = NativeSession::all_enabled(7, None);
        let mut m = model();
        let effects = sent(session.follow_up(&mut m, Stage::VimEnter));
        let batches: Vec<&Vec<TakeoverStep>> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Rpc(RpcCall::Takeover { steps }) => Some(steps),
                _ => None,
            })
            .collect();
        assert_eq!(
            batches.len(),
            1,
            "one request, not one per call: {effects:?}"
        );
        assert!(
            matches!(effects.first(), Some(Effect::Rpc(RpcCall::Takeover { .. }))),
            "the takeover leads, so it is in force before the answer frees \
             nvim and before the attach draws its first frame: {effects:?}"
        );
        let loose: Vec<&Effect> = effects
            .iter()
            .filter(|e| !matches!(e, Effect::Rpc(RpcCall::Takeover { .. })))
            .collect();
        assert!(
            loose.iter().all(|e| matches!(
                e,
                Effect::Rpc(RpcCall::UiAttach { .. } | RpcCall::ClaimStdoutTty)
            )),
            "only the two calls with no lua behind them travel alone: {loose:?}"
        );
        assert_eq!(
            unbatched(effects.clone()).len(),
            batches[0].len() + loose.len(),
            "the batch performs every call it swallowed: {effects:?}"
        );
    }

    #[test]
    fn the_takeover_holds_every_planned_surface_and_registers_the_keys_once() {
        let mut session = NativeSession::all_enabled(7, None);
        let mut m = model();
        let effects = unbatched(session.follow_up(&mut m, Stage::VimEnter));
        // the plan's own calls, compared as a list rather than counted: a
        // count matches whenever a hold of the wrong surface replaces the
        // right one, and the plan carries two kinds of hold now
        let planned: Vec<RpcCall> = plan(
            &NativeConfig::all_enabled(),
            registry::features(),
            Look::default(),
        )
        .iter()
        .filter_map(|entry| entry.rpc.clone())
        .collect();
        let holds: Vec<RpcCall> = effects
            .iter()
            .filter_map(|e| match e {
                Effect::Rpc(
                    call @ (RpcCall::HoldOption { .. }
                    | RpcCall::HoldWindowOption { .. }
                    | RpcCall::HoldNotify),
                ) => Some(call.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            holds, planned,
            "every planned surface must be held: {effects:?}"
        );
        assert!(
            !holds.is_empty(),
            "the shipped plan holds at least one surface; a takeover of none means the plan never reached the seam"
        );
        let registrations: Vec<&Effect> = effects
            .iter()
            .filter(|e| matches!(e, Effect::Rpc(RpcCall::RegisterMappings { .. })))
            .collect();
        assert_eq!(
            registrations.len(),
            1,
            "the keys register in exactly one chunk: {effects:?}"
        );
        match registrations[0] {
            Effect::Rpc(RpcCall::RegisterMappings { channel_id, .. }) => {
                assert_eq!(*channel_id, 7);
            }
            other => unreachable!("{other:?}"),
        }
        let clipboard_registrations = effects
            .iter()
            .filter(|e| matches!(e, Effect::Rpc(RpcCall::RegisterClipboard { .. })))
            .count();
        assert_eq!(
            clipboard_registrations, 1,
            "the clipboard provider registers exactly once, unconditionally: {effects:?}"
        );
        assert!(
            session.follow_up(&mut m, Stage::VimEnter).is_empty(),
            "a second VimEnter must register nothing: the second pass would read view's own keys back as the user's"
        );
    }

    /// The UI goes on last, behind every takeover call and behind the
    /// answer `dispatch` writes between them, so the one frame the attach
    /// produces is drawn with the surfaces already taken -- and the option
    /// that only exists once a UI does goes on behind the attach itself,
    /// where nvim will accept it.
    #[test]
    fn the_attach_closes_the_takeover_and_the_stdout_claim_closes_the_attach() {
        let mut session = NativeSession::all_enabled(7, None);
        let mut m = model();
        let effects = sent(unbatched(session.follow_up(&mut m, Stage::VimEnter)));
        let tail: Vec<&Effect> = effects.iter().rev().take(2).collect();
        assert!(
            matches!(tail[0], Effect::Rpc(RpcCall::ClaimStdoutTty)),
            "the stdout claim must be the last call of the takeover: {effects:?}"
        );
        assert!(
            matches!(tail[1], Effect::Rpc(RpcCall::UiAttach { .. })),
            "the attach must be the call the takeover itself closes with: {effects:?}"
        );
        assert!(
            session.follow_up(&mut m, Stage::VimEnter).is_empty(),
            "one connection is attached exactly once"
        );
    }

    /// A session whose UI went on before its config was sourced -- the one
    /// spawn that keeps `--embed`'s barrier, because nvim reads its piped
    /// stdin during startup (`EngineConfig::attaches_late`) -- still owes
    /// the claim, and must not attach a second time.
    #[test]
    fn a_session_already_attached_claims_stdout_without_attaching_again() {
        let mut session = NativeSession::all_enabled(7, None);
        let mut m = model();
        let _ = m.takes_attach();
        let effects = sent(unbatched(session.follow_up(&mut m, Stage::VimEnter)));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Rpc(RpcCall::UiAttach { .. }))),
            "a second attach on one connection: {effects:?}"
        );
        assert!(
            matches!(effects.last(), Some(Effect::Rpc(RpcCall::ClaimStdoutTty))),
            "the stdout claim is owed either way: {effects:?}"
        );
    }

    /// Whether the takeover zeroes `cmdheight`: nvim does so itself for
    /// any UI attached with `ext_messages`, command line or not, and a
    /// session that handed messages back keeps the row they draw on. Walks
    /// every combination of the switches that decide an attach, so a hold
    /// keyed on any other switch fails by name.
    #[test]
    fn cmdheight_is_zeroed_exactly_where_view_draws_the_messages() {
        let ids = view_core::native::ext::switches();
        for on in view_core::native::ext::switch_sets() {
            let toml: String = std::iter::once("[native]\n".to_string())
                .chain(ids.iter().map(|id| format!("{id} = {}\n", on.contains(id))))
                .collect();
            let file = view_native::config::ViewConfig::from_toml_str(&toml).expect("valid toml");
            let resolved = view_native::config::resolve_with(
                &file,
                &view_native::config::Overrides::default(),
                &|_| None,
            );
            let surfaces = view_native::config::ext_surfaces(&resolved);
            let zeroed = surfaces.contains(&Ext::Messages);
            let mut session = NativeSession::all_enabled(7, None);
            session.cfg = resolved.tables.native;
            let mut m = model();
            m.attach_surfaces(surfaces.clone());
            let effects = unbatched(session.follow_up(&mut m, Stage::VimEnter));
            let sets_cmdheight = effects.iter().any(|e| {
                matches!(
                    e,
                    Effect::Rpc(RpcCall::HoldOption { name, value })
                        if name == "cmdheight" && *value == OptionValue::Int(0)
                )
            });
            assert_eq!(
                sets_cmdheight,
                zeroed,
                "attached {surfaces:?} but cmdheight=0 was {}",
                if sets_cmdheight { "sent" } else { "withheld" }
            );
        }
    }

    #[test]
    fn a_disabled_ai_feature_registers_no_ai_key() {
        let mut session = NativeSession {
            cfg: NativeConfig::all_enabled(),
            plan: plan(
                &NativeConfig::all_enabled(),
                registry::features(),
                Look::default(),
            ),
            config_path: None,
            record: None,
            channel_id: 13,
            handed_over: false,
            ai_enabled: false,
            look: Look::default(),
            ui_keys_lhs: default_ui_keys_lhs(),
            initial_profile: KeyProfile::Editor,
            profile: KeyProfile::Editor,
            desktop_modifier_choice: ModifierChoice::Auto,
            desktop: default_desktop(),
            profile_marker: None,
            chords_pending: false,
            leader: view_core::msg::DEFAULT_MAPLEADER.to_string(),
            hold_starts: HoldStarts::default(),
            claims_owed: 0,
            held_input: Vec::new(),
            hold_lifted: false,
            hold_generation: 0,
            takeover_sent: None,
            announced: Vec::new(),
            writer: RecordWriter::default(),
        };
        let mut m = model();
        let specs = startup_specs(&mut session, &mut m);
        assert!(
            specs.iter().all(|s| s.feature != "ai"),
            "ai must contribute no key while disabled: {specs:?}"
        );
        assert!(
            specs.iter().any(|s| s.feature == "picker"),
            "every other feature's keys must still register: {specs:?}"
        );
    }

    #[test]
    fn an_enabled_ai_feature_still_registers_its_key() {
        let mut session = NativeSession::all_enabled(14, None);
        let mut m = model();
        let specs = startup_specs(&mut session, &mut m);
        assert!(
            specs.iter().any(|s| s.feature == "ai"),
            "the default (enabled) session must still register the ai key: {specs:?}"
        );
    }

    #[test]
    fn load_snapshots_ai_enabled_from_the_model_rather_than_a_hardcoded_default() {
        let mut m = model();
        m.ai_enabled = false;
        let (mut session, _effects) = load_from(None, 21, &mut m);
        let specs = startup_specs(&mut session, &mut m);
        assert!(!specs.is_empty(), "the other features' keys still register");
        assert!(
            specs.iter().all(|s| s.feature != "ai"),
            "load() must carry model.ai_enabled into the session, not a hardcoded true: {specs:?}"
        );
    }

    #[test]
    fn the_takeover_registers_the_clipboard_provider_even_with_every_plan_entry_empty() {
        let mut session = NativeSession {
            cfg: NativeConfig::all_enabled(),
            plan: Vec::new(),
            config_path: None,
            record: None,
            channel_id: 9,
            handed_over: false,
            ai_enabled: true,
            look: Look::default(),
            ui_keys_lhs: default_ui_keys_lhs(),
            initial_profile: KeyProfile::Editor,
            profile: KeyProfile::Editor,
            desktop_modifier_choice: ModifierChoice::Auto,
            desktop: default_desktop(),
            profile_marker: None,
            chords_pending: false,
            leader: view_core::msg::DEFAULT_MAPLEADER.to_string(),
            hold_starts: HoldStarts::default(),
            claims_owed: 0,
            held_input: Vec::new(),
            hold_lifted: false,
            hold_generation: 0,
            takeover_sent: None,
            announced: Vec::new(),
            writer: RecordWriter::default(),
        };
        let mut m = model();
        let effects = unbatched(session.follow_up(&mut m, Stage::VimEnter));
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::Rpc(RpcCall::RegisterClipboard { channel_id: 9 })
            )),
            "clipboard registration is not registry-gated: it must survive an empty plan, got {effects:?}"
        );
    }

    /// A replacement engine has none of the registrations the one it
    /// replaced was given, and answers on a channel of its own. A session
    /// that kept either fact would leave a recovered editor with view's keys
    /// unbound and its notifications addressed to a channel that is gone.
    #[test]
    fn a_rebound_session_hands_over_again_and_to_the_new_channel() {
        let mut session = NativeSession::all_enabled(7, None);
        let mut m = model();
        let first = unbatched(session.follow_up(&mut m, Stage::VimEnter));
        assert!(
            !first.is_empty(),
            "the first takeover registered nothing at all"
        );
        assert!(
            session.follow_up(&mut m, Stage::VimEnter).is_empty(),
            "one engine is handed over to exactly once"
        );

        session.rebind(21);
        let again = unbatched(session.follow_up(&mut m, Stage::VimEnter));
        assert!(
            again.iter().any(|e| matches!(
                e,
                Effect::Rpc(RpcCall::RegisterClipboard { channel_id: 21 })
            )),
            "the replacement engine was never handed the registrations the \
             dead one had, or was handed them on the dead one's channel: {again:?}"
        );
        assert!(
            again.iter().any(|e| matches!(
                e,
                Effect::Rpc(RpcCall::RegisterMappings { channel_id: 21, .. })
            )),
            "view's own keys are unbound in the recovered session: {again:?}"
        );
    }

    #[test]
    fn a_claimed_key_is_announced_with_the_switch_that_returns_it() {
        let (_dir, record) = scratch("claimed");
        let claimed = vec![MappingClaim::new("picker", "<leader>ff", true)];

        let mut session = NativeSession::all_enabled(7, Some(record.clone()));
        let mut m = model();
        m.record_claimed_keys(claimed.clone());
        let effects = session.follow_up(&mut m, Stage::Claims);
        assert!(
            !effects.is_empty()
                && effects
                    .iter()
                    .all(|e| matches!(e, Effect::ScheduleToastExpiry { .. })),
            "every notice this stage pushes must talk to the user through the same \
             choke point every other locally-synthesized notice uses (never straight \
             to nvim), got {effects:?}"
        );
        let first = shown(&m);
        assert!(
            first.contains("<leader>ff") && first.contains("native.picker = false"),
            "the notice must name the key and the switch that returns it, got {first:?}"
        );
        assert!(
            first.contains("native.statusline = false"),
            "the option this session held must announce itself through the same notice, got {first:?}"
        );
        assert!(m.dirty);
        session.finish_record();

        let mut next = NativeSession::all_enabled(7, Some(record.clone()));
        let mut later = model();
        later.seed_announced(toast::announced_keys(next.config_path.as_deref(), &record).unwrap());
        later.record_claimed_keys(claimed);
        let _ = next.follow_up(&mut later, Stage::Claims);
        assert_eq!(
            shown(&later),
            "",
            "a surface introduces itself once per config, not every launch"
        );
    }

    /// One launch under `record`, seeded from it the way `load` seeds: the
    /// claims answer names `claimed` as keys the config had mapped, then
    /// each of `held` is reported held by the config, its record effects
    /// handed to the session the way `runtime::dispatch` hands them.
    /// Answers what the box says and what the record holds afterwards.
    fn launch(
        record: &std::path::Path,
        claimed: &[MappingClaim],
        held: &[&str],
    ) -> (String, Vec<String>) {
        let mut session = NativeSession::all_enabled(7, Some(record.to_path_buf()));
        let mut m = model();
        m.attach_surfaces(view_core::native::ext::ALL.to_vec());
        m.statusline_enabled = true;
        m.seed_announced(toast::announced_keys(None, record).unwrap());
        m.record_claimed_keys(claimed.to_vec());
        let _ = session.follow_up(&mut m, Stage::Claims);
        for channel in held {
            let effects = view_core::update::update(
                &mut m,
                Msg::ChannelHeld {
                    channel: (*channel).to_string(),
                    holder: "%!v:lua.a()".to_string(),
                },
            );
            for effect in effects {
                if let Effect::RecordAnnounced { key } = effect {
                    session.record_announced(&key);
                }
            }
        }
        session.finish_record();
        (shown(&m), toast::announced_keys(None, record).unwrap())
    }

    /// The record keys of every feature an all-enabled launch draws.
    fn drawing_keys() -> Vec<String> {
        let session = NativeSession::all_enabled(7, None);
        report(&session.plan, &[], registry::features())
            .iter()
            .map(view_native::report::Handover::record_key)
            .collect()
    }

    /// `keys` sorted the way the record stores them.
    fn sorted(keys: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut keys: Vec<String> = keys.into_iter().collect();
        keys.sort();
        keys
    }

    fn picker_key() -> Vec<MappingClaim> {
        vec![MappingClaim::new("picker", "<leader>ff", true)]
    }

    const PICKER_KEY: &str = "picker:key:<leader>ff";

    /// A launch with nothing held and no key taken raises no box and leaves
    /// the record as it was.
    #[test]
    fn a_launch_that_took_nothing_tells_and_records_nothing() {
        let (_dir, record) = scratch("took-nothing");
        let (shown, recorded) = launch(&record, &[], &[]);
        assert_eq!(shown, "");
        assert_eq!(recorded, Vec::<String>::new());
    }

    /// A held channel raises the box with the held line and the line naming
    /// the features view draws, and the record holds both.
    #[test]
    fn a_held_channel_records_what_the_box_names() {
        let (_dir, record) = scratch("held");
        let (shown, recorded) = launch(&record, &[], &["statusline"]);
        assert!(
            shown.starts_with("view: your config also draws the status line (statusline)"),
            "{shown:?}"
        );
        assert!(
            shown.contains("Now drawing ") && shown.contains("the tab line"),
            "{shown:?}"
        );
        assert_eq!(
            recorded,
            sorted(drawing_keys().into_iter().chain(["held:statusline".into()]))
        );
    }

    /// A taken key raises the box with the drawing line and the key line,
    /// and the record holds both. The same launch again raises none and
    /// leaves the record as it was.
    #[test]
    fn a_taken_key_records_what_the_box_names_once() {
        let (_dir, record) = scratch("key");
        let (shown, recorded) = launch(&record, &picker_key(), &[]);
        assert!(shown.starts_with("view: now drawing "), "{shown:?}");
        assert!(shown.contains("Now mapping <leader>ff"), "{shown:?}");
        let both = sorted(drawing_keys().into_iter().chain([PICKER_KEY.into()]));
        assert_eq!(recorded, both);

        let (shown, recorded) = launch(&record, &picker_key(), &[]);
        assert_eq!(shown, "");
        assert_eq!(recorded, both);
    }

    /// A launch that told nothing leaves the features unrecorded, so the
    /// later launch whose config holds a channel names them beside it and
    /// records them.
    #[test]
    fn a_launch_that_told_nothing_leaves_the_features_for_the_conflict() {
        let (_dir, record) = scratch("told-nothing");
        let (shown, recorded) = launch(&record, &[], &[]);
        assert_eq!((shown.as_str(), recorded.len()), ("", 0));

        let (shown, recorded) = launch(&record, &[], &["statusline"]);
        assert!(
            shown.starts_with("view: your config also draws "),
            "{shown:?}"
        );
        assert!(
            shown.contains("Now drawing ") && shown.contains("the tab line"),
            "{shown:?}"
        );
        assert_eq!(
            recorded,
            sorted(drawing_keys().into_iter().chain(["held:statusline".into()]))
        );
    }

    /// A key told at an earlier launch and mapped again tells nothing, so
    /// the features it would once have carried stay unrecorded and a later
    /// held channel names them.
    #[test]
    fn a_key_told_before_records_no_feature_the_box_did_not_name() {
        let (_dir, record) = scratch("key-told-before");
        toast::record_key(None, PICKER_KEY, &record).unwrap();
        let (shown, recorded) = launch(&record, &picker_key(), &[]);
        assert_eq!(shown, "");
        assert_eq!(recorded, [PICKER_KEY]);

        let (shown, recorded) = launch(&record, &picker_key(), &["statusline"]);
        assert!(
            shown.starts_with("view: your config also draws "),
            "{shown:?}"
        );
        assert!(
            shown.contains("Now drawing ") && shown.contains("the tab line"),
            "{shown:?}"
        );
        assert_eq!(
            recorded,
            sorted(
                drawing_keys()
                    .into_iter()
                    .chain(["held:statusline".into(), PICKER_KEY.into()])
            )
        );
    }

    /// `ui_attach` already ran, at the raw terminal height, before `load`
    /// ever reads a config (see `main.rs`'s call ordering): nvim's live
    /// grid still claims the row the default-on statusline now needs.
    /// Without a resize here, `view_surface::render` would place the
    /// statusline at `offset + grid_h` using nvim's still-full grid height
    /// and paint it one row below the terminal entirely.
    #[test]
    fn load_reserves_the_statusline_row_with_a_resize_when_nothing_disables_it() {
        let mut m = model();
        let (_session, effects) = load_from(None, 7, &mut m);
        assert!(m.statusline_enabled, "an absent config is every feature on");
        assert!(
            effects.iter().any(|e| matches!(
                e,
                Effect::Rpc(RpcCall::TryResize {
                    width: 80,
                    height: 23
                })
            )),
            "load must reserve the statusline's row the moment it turns the \
             feature on, got {effects:?}"
        );
    }

    /// The opposite of the row-reservation test above: a config that turns
    /// the statusline off must never touch nvim's grid, or a shrunk grid
    /// with no statusline painted over it would leave a permanently blank
    /// row a user never asked to give up.
    #[test]
    fn load_skips_the_resize_when_the_config_turns_the_statusline_off() {
        let dir = view_test_support::ScratchDir::new("native-statusline-resize").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(&path, "[native]\nstatusline = false\n")
            .expect("a temp config must be writable");

        let mut m = model();
        let (_session, effects) = load_from(Some(path), 7, &mut m);

        assert!(
            !m.statusline_enabled,
            "the config explicitly disabled the feature"
        );
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Rpc(RpcCall::TryResize { .. }))),
            "a disabled statusline reserves no row and must not resize nvim's \
             already-correct grid, got {effects:?}"
        );
    }

    /// The palette's off switch, mirroring the statusline pair above minus
    /// the resize concern: the palette floats over the grid rather than
    /// reserving a row from it, so turning it off has nothing to undo on
    /// nvim's side -- only `Model::palette_enabled` itself changes.
    #[test]
    fn load_turns_the_palette_on_by_default_and_off_when_configured() {
        let mut on = model();
        let _ = load_from(None, 7, &mut on);
        assert!(on.palette_enabled, "an absent config is every feature on");

        let dir = view_test_support::ScratchDir::new("native-palette-toggle").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(
            &path,
            "[native]
palette = false
",
        )
        .expect("a temp config must be writable");

        let mut off = model();
        let _ = load_from(Some(path), 7, &mut off);

        assert!(
            !off.palette_enabled,
            "the config explicitly disabled the feature"
        );
    }
    /// The `[supervision]` table's one switch reaching the model, over the
    /// same single load every other table's answers cross.
    #[test]
    fn load_recovers_automatically_by_default_and_stops_when_configured() {
        let mut on = model();
        let _ = load_from(None, 7, &mut on);
        assert!(
            on.supervision.auto_restart,
            "an absent config must keep automatic recovery on"
        );

        let dir = view_test_support::ScratchDir::new("native-supervision-toggle").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(
            &path,
            "[supervision]
auto_restart = false
",
        )
        .expect("a temp config must be writable");

        let mut off = model();
        let _ = load_from(Some(path), 7, &mut off);

        assert!(
            !off.supervision.auto_restart,
            "the config explicitly turned automatic recovery off"
        );
        assert!(
            off.palette_enabled,
            "a supervision-only config must leave every native feature at its default"
        );
    }

    /// The `[keys]` table crossing the same load: the resolved bindings
    /// reach the model, and an entry naming no key leaves its own action
    /// alone while telling the user.
    #[test]
    fn load_carries_the_key_bindings_and_reports_one_it_could_not_read() {
        use view_core::native::keys::{Action, Direction, Resolved};

        let mut default = model();
        let _ = load_from(None, 7, &mut default);
        assert_eq!(
            default.key_bindings.resolve(Some("<C-w>"), ">"),
            Some(Resolved::Act(Action::Resize(Direction::Wider))),
            "an absent config is the shipped chord"
        );
        assert_eq!(
            default.key_bindings.resolve(None, "<M-CR>"),
            Some(Resolved::Act(Action::ComposerNewline)),
            "and the composer's shipped line break"
        );

        let dir = view_test_support::ScratchDir::new("native-keys").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(
            &path,
            "[keys]
sidebar_wider = \"<M-.>\"
sidebar_narrower = 30
composer_newline = \"<A-x>\"
",
        )
        .expect("a temp config must be writable");

        let mut configured = model();
        let (_session, effects) = load_from(Some(path), 7, &mut configured);

        assert_eq!(
            configured.key_bindings.resolve(None, "<M-.>"),
            Some(Resolved::Act(Action::Resize(Direction::Wider))),
            "the readable action was rebound"
        );
        assert_eq!(
            configured.key_bindings.resolve(None, "<S-Left>"),
            Some(Resolved::Act(Action::Resize(Direction::Narrower))),
            "the unreadable one kept its defaults"
        );
        assert!(
            !effects.is_empty(),
            "the notice is raised through an effect"
        );
        let raised = format!("{:?}", configured.engine.messages.entries);
        assert!(
            raised.contains("sidebar_narrower"),
            "and the user is told which entry was dropped: {raised}"
        );
        assert!(
            !raised.contains("sidebar_wider"),
            "while the entry that read fine is not complained about: {raised}"
        );
        assert_eq!(
            configured.key_bindings.resolve(None, "<M-x>"),
            Some(Resolved::Act(Action::ComposerNewline)),
            "and Alt reaches the same binding however the config spells it"
        );
    }

    /// `toggle_gaps`/`cycle_surfaces` are real nvim mappings, and the
    /// two-key ceiling is a [`KeyBindings`] chord's alone, so a `[keys]`
    /// override of either may be any length: this rebinds one to a bare
    /// function key and the other to a two-character chord and checks both
    /// land on the spec `take_over` actually registers, the way
    /// `mappings_live.rs` checks a registered `lhs` against a real nvim
    /// beside the plan.
    #[test]
    fn load_carries_a_ui_key_rebind_of_either_shape_into_the_registered_mapping() {
        let dir = view_test_support::ScratchDir::new("native-ui-keys").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(
            &path,
            "[keys]
toggle_gaps = \"<F2>\"
cycle_surfaces = \"gz\"
",
        )
        .expect("a temp config must be writable");

        let mut m = model();
        let (mut session, _) = load_from(Some(path), 7, &mut m);
        let specs = startup_specs(&mut session, &mut m);

        let lhs_for = |verb: &str| {
            specs
                .iter()
                .find(|spec| spec.feature == "ui" && spec.verb == verb)
                .map(|spec| spec.lhs.as_ref())
        };
        assert_eq!(
            lhs_for("gaps"),
            Some("<F2>"),
            "the single-key rebind must reach the registered spec: {specs:?}"
        );
        assert_eq!(
            lhs_for("cycle_surfaces"),
            Some("gz"),
            "the two-key rebind must reach the registered spec too: {specs:?}"
        );
    }

    #[test]
    fn a_resize_mode_rebind_registers_every_key_it_names_and_the_default_none() {
        let dir = view_test_support::ScratchDir::new("native-resize-keys").unwrap();
        let path = dir.join("view.toml");
        std::fs::write(&path, "[keys]\nresize_mode = [\"<C-w>R\", \"<F3>\"]\n")
            .expect("a temp config must be writable");
        let mut m = model();
        let (mut session, _) = load_from(Some(path), 7, &mut m);
        let specs = startup_specs(&mut session, &mut m);
        let resize: Vec<&str> = specs
            .iter()
            .filter(|spec| spec.feature == "window" && spec.verb == "resize_mode")
            .map(|spec| spec.lhs.as_ref())
            .collect();
        assert_eq!(resize, ["<C-w>R", "<F3>"], "{specs:?}");
    }

    /// `main` puts the resolved `[ui.surfaces]` placements on the model
    /// before `load` runs, and `load` read `[native] tree_width` back over
    /// the tree's share, so a file naming only `[ui.surfaces.tree] size`
    /// opened the float at the default width.
    #[test]
    fn the_overlay_tree_takes_its_configured_size() {
        let mut m = model();
        m.surfaces.set_layout(
            view_core::native::geometry::NativeSurface::Tree,
            view_core::native::geometry::SurfaceLayout::new(
                view_core::native::geometry::SurfacePlacement::Overlay,
                view_core::native::geometry::Anchor::Left,
                22,
            ),
        );
        let _ = load_from(None, 1, &mut m);
        assert_eq!(
            m.tree_width_pct, 22,
            "the surfaces table's size was read back over by [native] tree_width"
        );
    }

    /// The `VimEnter` takeover registers no key at all, under either
    /// profile: each key costs nvim's own blocked startup clock a `maparg`
    /// snapshot and a `nvim_set_keymap` (`view-bench`'s `startup`
    /// scenario's `server_delta_ms`), so `Self::take_over` leaves the leader
    /// chords and the desktop chords for the follow-up `Stage::Claims`
    /// fires once that clock has already been freed. A key in the
    /// `VimEnter` batch is a regression back onto nvim's own startup path.
    #[test]
    fn the_vim_enter_takeover_registers_no_key_and_claims_sends_the_live_set() {
        for desktop in [false, true] {
            let mut session = if desktop {
                NativeSession::desktop(7, None)
            } else {
                NativeSession::all_enabled(7, None)
            };
            let mut m = model();
            let specs_of = |effects: &[Effect]| -> Vec<view_core::native::mappings::MappingSpec> {
                effects
                    .iter()
                    .find_map(|e| match e {
                        Effect::Rpc(RpcCall::RegisterMappings { specs, .. }) => Some(specs.clone()),
                        _ => None,
                    })
                    .expect("a RegisterMappings call must ride this stage")
            };
            let vim_enter = unbatched(session.follow_up(&mut m, Stage::VimEnter));
            let vim_enter_specs = specs_of(&vim_enter);
            assert!(
                vim_enter_specs.is_empty(),
                "desktop={desktop}: the VimEnter batch must carry no key: {vim_enter_specs:?}"
            );
            let claims = unbatched(session.follow_up(&mut m, Stage::Claims));
            let claims_specs = specs_of(&claims);
            let RpcCall::RegisterMappings { specs: whole, .. } =
                session.build_mapping_call(&mut m).0
            else {
                panic!("build_mapping_call must build a RegisterMappings call");
            };
            assert_eq!(
                claims_specs, whole,
                "desktop={desktop}: Stage::Claims must send the whole live set"
            );
            for default in view_core::native::mappings::default_maps() {
                assert!(
                    claims_specs.iter().any(|s| s.lhs == default.lhs),
                    "desktop={desktop}: {} must reach nvim behind the reply",
                    default.lhs
                );
            }
            let (modifier, _, _) = profile::modifier_for(session.desktop_modifier_choice, false);
            let chords =
                profile::chord_plan(&session.desktop, session.profile, modifier, &session.cfg);
            assert_eq!(
                desktop,
                !chords.is_empty(),
                "only the desktop profile has chords"
            );
            for chord in &chords {
                assert!(
                    claims_specs.iter().any(|s| s.lhs == chord.lhs),
                    "the desktop chord {} must reach nvim behind the reply",
                    chord.lhs
                );
            }
            let again = unbatched(session.follow_up(&mut m, Stage::Claims));
            assert!(
                !again
                    .iter()
                    .any(|e| matches!(e, Effect::Rpc(RpcCall::RegisterMappings { .. }))),
                "desktop={desktop}: a second Stage::Claims must resend nothing: {again:?}"
            );
        }
    }

    /// A kitty probe answering between `VimEnter` and the takeover's claims
    /// reissues the whole set, chords included, so the follow-up the claims
    /// would fire owes nothing more.
    #[test]
    fn a_reissue_ahead_of_the_claims_leaves_the_chord_follow_up_nothing_to_send() {
        let mut session = NativeSession::desktop(7, None);
        let mut m = model();
        let _ = session.follow_up(&mut m, Stage::VimEnter);
        assert!(session.holds_input());
        let reissue = session.follow_up(&mut m, Stage::CapsUpgraded);
        assert!(
            reissue
                .iter()
                .any(|e| matches!(e, Effect::Rpc(RpcCall::RegisterMappings { .. }))),
            "the reissue must resend the mappings: {reissue:?}"
        );
        let claims = session.follow_up(&mut m, Stage::Claims);
        assert!(
            !claims
                .iter()
                .any(|e| matches!(e, Effect::Rpc(RpcCall::RegisterMappings { .. }))),
            "Stage::Claims must not resend what the reissue already sent: {claims:?}"
        );
        assert!(
            session.holds_input(),
            "the takeover's reply came back, the reissue's has not"
        );
        let _ = session.follow_up(&mut m, Stage::Claims);
        assert!(
            !session.holds_input(),
            "the reissue that carried the chords has answered"
        );
    }

    /// An engine restarted after a `:View ui panes` flip is taken over for
    /// the look on screen, where the plan built at load would hold
    /// `laststatus` for the startup look.
    #[test]
    fn a_restarted_engine_is_taken_over_for_the_look_on_screen() {
        use view_core::model::Panes;

        let mut session = NativeSession::all_enabled(7, None);
        let mut m = model();
        assert_eq!(m.look.panes, Panes::Nvim);
        let _ = session.follow_up(&mut m, Stage::VimEnter);
        m.look = Look::new(Panes::Tiles, true);
        session.rebind(8);
        let effects = unbatched(session.follow_up(&mut m, Stage::VimEnter));
        let held = effects.iter().find_map(|e| match e {
            Effect::Rpc(RpcCall::HoldOption { name, value }) if name == "laststatus" => {
                Some(value.clone())
            }
            _ => None,
        });
        assert_eq!(
            held,
            Some(OptionValue::Int(2)),
            "the restart held laststatus for the startup look: {effects:?}"
        );
    }

    /// Input waits for nvim's answer to the registration that carries the
    /// chords, and sending that registration is not enough: nvim puts
    /// `nvim_input` into typeahead as it reads it and runs the registration
    /// later, so a chord written right behind it runs unmapped.
    #[test]
    fn input_is_held_until_the_chord_registration_has_answered() {
        // a leader chord under the editor profile waits for its mapping the
        // way a desktop chord does, since both register behind the reply
        let chord = view_core::native::chords::desktop_chords()[0]
            .lhs(profile::modifier_for(ModifierChoice::Auto, model().caps.kitty_kbd).0);
        for (mut session, typed) in [
            (NativeSession::desktop(7, None), chord),
            (NativeSession::desktop(7, None), " ff"),
            (NativeSession::all_enabled(7, None), " ff"),
        ] {
            let mut m = model();
            session.note_vim_enter(&leader_vim_enter(" "));
            let _ = session.follow_up(&mut m, Stage::VimEnter);
            assert!(
                session.holds_input(),
                "{typed:?}: the takeover starts the hold"
            );
            for passes in [
                input("j"),
                input("hello"),
                Effect::Rpc(RpcCall::Paste {
                    text: " ff".to_string(),
                }),
                Effect::Rpc(RpcCall::TryResize {
                    width: 100,
                    height: 30,
                }),
                Effect::Rpc(RpcCall::InputMouse {
                    button: "left".to_string(),
                    action: "press".to_string(),
                    modifier: String::new(),
                    grid: view_core::grid::registry::GridId(1),
                    row: 5,
                    col: 10,
                }),
            ] {
                assert!(!session.holds(&passes), "{typed:?}: held {passes:?}");
            }
            assert!(session.holds(&input(typed)), "{typed:?} went out unmapped");
            session.hold_input(input(typed));
            assert!(
                session.holds(&input("j")),
                "{typed:?}: a key typed behind held input overtook it"
            );
            let follow_up = session.follow_up(&mut m, Stage::Claims);
            assert!(
                follow_up
                    .iter()
                    .any(|e| matches!(e, Effect::Rpc(RpcCall::RegisterMappings { .. }))),
                "the takeover's claims send the keys: {follow_up:?}"
            );
            assert!(
                session.release_input().is_empty(),
                "{typed:?}: the key registration was only sent, and nvim has not run it"
            );
            let _ = session.follow_up(&mut m, Stage::Claims);
            let released = session.release_input();
            assert!(
                matches!(
                    released.as_slice(),
                    [Effect::Rpc(RpcCall::Input { notation })] if notation == typed
                ),
                "the key registration answered, so the held input goes out: {released:?}"
            );
            assert!(!session.holds_input());
            assert!(!session.holds(&input(typed)), "the hold has ended");
        }
    }

    /// The leader nvim reports at `VimEnter` decides which key waits: under
    /// a `,` leader a `,` is held and a space goes to nvim at once.
    #[test]
    fn the_configs_leader_decides_which_key_waits() {
        for (leader, held, passes) in [(",", ",", " "), (" ", " ", ","), ("\\", "\\", " ")] {
            let mut session = NativeSession::all_enabled(7, None);
            session.note_vim_enter(&leader_vim_enter(leader));
            let _ = session.follow_up(&mut model(), Stage::VimEnter);
            assert!(session.holds(&input(held)), "{leader:?}: {held:?} passed");
            assert!(
                !session.holds(&input(passes)),
                "{leader:?}: {passes:?} held"
            );
        }
    }

    /// Delivers each bound's expiry on time to a session whose takeover
    /// went out at `sent` and never answers, until the hold lifts. Returns
    /// how long after `sent` the expiry that lifted it arrived, and the
    /// generation it carried.
    fn expire_until_lifted(
        session: &mut NativeSession,
        m: &mut Model,
        sent: std::time::Instant,
        first: &[Effect],
    ) -> (std::time::Duration, u64) {
        let mut due = first.to_vec();
        let mut offset = std::time::Duration::ZERO;
        loop {
            let Some(&Effect::ScheduleChordHold { after, generation }) = due.last() else {
                panic!("a hold still in force arms its next bound: {due:?}");
            };
            assert!(after <= CHORD_HOLD_BOUND, "armed {after:?}");
            offset += after;
            assert!(
                offset <= CHORD_HOLD_CEILING,
                "the hold outlived the ceiling: {offset:?}"
            );
            due = session.follow_up_at(m, Stage::HoldExpired { generation }, || sent + offset);
            if !session.holds_input() {
                return (offset, generation);
            }
        }
    }

    /// The pass that starts the hold arms its bound last, behind the
    /// takeover's own calls. While the takeover stays unanswered each
    /// expiry arms the bound again, and the one that reaches
    /// `CHORD_HOLD_CEILING` after `VimEnter` releases the held input. An
    /// expiry armed for an earlier hold releases nothing.
    #[test]
    fn the_ceiling_releases_input_held_for_an_unanswered_takeover() {
        let mut session = NativeSession::desktop(7, None);
        let mut m = model();
        let sent = std::time::Instant::now();
        let take_over = session.follow_up_at(&mut m, Stage::VimEnter, || sent);
        let Some(&Effect::ScheduleChordHold { after, generation }) = take_over.last() else {
            panic!("the pass that starts the hold arms its bound last: {take_over:?}");
        };
        assert_eq!(after, CHORD_HOLD_BOUND);
        session.hold_input(Effect::Rpc(RpcCall::Input {
            notation: "<CR>".to_string(),
        }));
        let _ = session.follow_up_at(
            &mut m,
            Stage::HoldExpired {
                generation: generation - 1,
            },
            || sent + CHORD_HOLD_CEILING,
        );
        assert!(
            session.release_input().is_empty(),
            "a bound armed for another hold released this one"
        );
        let (lifted_at, _) = expire_until_lifted(&mut session, &mut m, sent, &take_over);
        assert_eq!(
            lifted_at, CHORD_HOLD_CEILING,
            "the unanswered takeover's hold lifted at the wrong time"
        );
        let released = session.release_input();
        assert!(
            matches!(
                released.as_slice(),
                [Effect::Rpc(RpcCall::Input { notation })] if notation == "<CR>"
            ),
            "the bound elapsed, so the held input goes out: {released:?}"
        );
        assert!(!session.holds_input());
        let chords = session.follow_up(&mut m, Stage::Claims);
        assert!(
            chords
                .iter()
                .any(|e| matches!(e, Effect::Rpc(RpcCall::RegisterMappings { .. }))),
            "a lifted hold still sends the chords once the takeover answers: {chords:?}"
        );
        assert!(
            !session.holds_input(),
            "the chord registration sent after the hold was lifted holds nothing"
        );
    }

    /// A pass that neither arms nor ends a hold reads no clock, so a key or
    /// a redraw dispatched during a hold costs no more than one outside it.
    #[test]
    fn a_pass_that_moves_no_bound_reads_no_clock() {
        let mut session = NativeSession::desktop(7, None);
        let mut m = model();
        let _ = session.follow_up(&mut m, Stage::VimEnter);
        assert!(session.holds_input());
        let _ = session.follow_up_at(&mut m, Stage::None, || {
            panic!("a pass that moves no bound read the clock")
        });
    }

    /// A replacement engine started after a lifted hold holds input for its
    /// own chords again, under a bound of its own.
    #[test]
    fn a_restart_after_a_lifted_hold_holds_input_again() {
        let mut session = NativeSession::desktop(7, None);
        let mut m = model();
        let sent = std::time::Instant::now();
        let take_over = session.follow_up_at(&mut m, Stage::VimEnter, || sent);
        let (_, generation) = expire_until_lifted(&mut session, &mut m, sent, &take_over);
        assert!(!session.holds_input());
        session.rebind(8);
        let take_over = session.follow_up(&mut m, Stage::VimEnter);
        assert!(
            session.holds_input(),
            "the replacement's chords are unmapped, so input typed during its launch waits"
        );
        assert!(
            matches!(
                take_over.last(),
                Some(Effect::ScheduleChordHold { after, generation: next })
                    if *after == CHORD_HOLD_BOUND && *next == generation + 1
            ),
            "the replacement's hold arms a bound under the next generation: {take_over:?}"
        );
    }

    /// Runs a desktop launch whose takeover answers `reply_time` after the
    /// pass that sent it, delivering every bound's expiry on time up to that
    /// reply. Returns the bound the reply arms and the generation it carries,
    /// with the session and the instant the reply arrived.
    fn rearmed_after(
        reply_time: std::time::Duration,
    ) -> (
        std::time::Duration,
        u64,
        NativeSession,
        Model,
        std::time::Instant,
    ) {
        let mut session = NativeSession::desktop(7, None);
        let mut m = model();
        let sent = std::time::Instant::now();
        let mut due = session.follow_up_at(&mut m, Stage::VimEnter, || sent);
        let mut offset = std::time::Duration::ZERO;
        while let Some(&Effect::ScheduleChordHold { after, generation }) = due.last() {
            if offset + after > reply_time {
                break;
            }
            offset += after;
            due = session.follow_up_at(&mut m, Stage::HoldExpired { generation }, || sent + offset);
            assert!(
                session.holds_input(),
                "the expiry at {offset:?} lifted the hold with the takeover unanswered"
            );
        }
        let answered = session.follow_up_at(&mut m, Stage::Claims, || sent + reply_time);
        assert!(
            session.holds_input(),
            "the chord registration is still owed"
        );
        let Some(&Effect::ScheduleChordHold { after, generation }) = answered.last() else {
            panic!("the takeover's reply arms the bound again: {answered:?}");
        };
        (after, generation, session, m, sent + reply_time)
    }

    /// A takeover that answers after the first bound expired still has its
    /// hold: the expiry arms the bound again, the reply arms it once more
    /// with the takeover reply time added, and only that last generation
    /// lifts it. A takeover that answers at once leaves the bound where it
    /// was.
    #[test]
    fn a_slow_takeover_extends_the_bound_by_its_reply_time() {
        let slow = std::time::Duration::from_millis(400);
        let (after, generation, mut session, mut m, answered) = rearmed_after(slow);
        assert_eq!(
            after,
            CHORD_HOLD_BOUND + slow,
            "a 400 ms takeover reply owes the registration its time past the reply"
        );
        assert_eq!(
            generation, 3,
            "the VimEnter bound and its expiry's re-arm are both superseded"
        );
        let _ = session.follow_up_at(
            &mut m,
            Stage::HoldExpired {
                generation: generation - 1,
            },
            || answered + CHORD_HOLD_BOUND,
        );
        assert!(
            session.holds_input(),
            "the bound armed before the reply lifted the hold"
        );
        let _ = session.follow_up_at(&mut m, Stage::HoldExpired { generation }, || {
            answered + after
        });
        assert!(
            !session.holds_input(),
            "the bound the reply armed did not lift the hold"
        );

        let fast = std::time::Duration::from_micros(500);
        let (after, generation, ..) = rearmed_after(fast);
        assert_eq!(generation, 2, "the bound armed at VimEnter is superseded");
        let from_vim_enter = fast + after;
        assert!(
            from_vim_enter <= CHORD_HOLD_BOUND + std::time::Duration::from_millis(1),
            "an immediate reply moved the bound to {from_vim_enter:?} after VimEnter"
        );
    }

    /// The bound sits at least twice above the slowest reply the chord
    /// registration was measured at after `VimEnter`, the worst launch under
    /// a login config on dev-linux, whose draws the commit that set the bound
    /// records. A bound under it releases a chord typed during an ordinary
    /// launch unmapped.
    #[test]
    fn the_bound_outlasts_the_slowest_measured_registration_reply() {
        let slowest_reply = std::time::Duration::from_millis(125);
        assert!(
            CHORD_HOLD_BOUND >= slowest_reply * 2,
            "{CHORD_HOLD_BOUND:?} leaves the {slowest_reply:?} reply no margin"
        );
    }

    /// A scrolled message area while input is held is nvim at a hit-enter
    /// or more prompt, which it leaves only on a key, so the hold ends at
    /// once. An unscrolled one is an ordinary message and changes nothing.
    #[test]
    fn a_message_prompt_releases_input_held_for_the_chords() {
        let msg_set_pos = |scrolled| view_core::events::UiEvent::MsgSetPos {
            grid: 3,
            row: 20,
            scrolled,
            sep_char: " ".to_string(),
            zindex: 200,
            compindex: 1,
        };
        let mut session = NativeSession::desktop(7, None);
        let mut m = model();
        let _ = session.follow_up(&mut m, Stage::VimEnter);
        session.hold_input(Effect::Rpc(RpcCall::Input {
            notation: "<CR>".to_string(),
        }));
        session.note_redraw(&[msg_set_pos(false)]);
        assert!(
            session.holds_input(),
            "a message that fits raised no prompt"
        );
        session.note_redraw(&[msg_set_pos(true)]);
        assert_eq!(
            session.release_input().len(),
            1,
            "the key that dismisses the prompt must reach nvim"
        );
    }

    /// A desktop profile whose every chord row resolves to no key still
    /// defers its leader chords, so the claims send them and input is held
    /// until they answer.
    #[test]
    fn a_desktop_profile_with_every_chord_off_still_defers_its_leader_chords() {
        let mut session = NativeSession {
            desktop: std::array::from_fn(|_| Resolved::new(String::new(), Source::File)),
            ..NativeSession::desktop(7, None)
        };
        let mut m = model();
        let _ = session.follow_up(&mut m, Stage::VimEnter);
        assert!(session.holds_input());
        let claims = unbatched(session.follow_up(&mut m, Stage::Claims));
        let specs = claims
            .iter()
            .find_map(|e| match e {
                Effect::Rpc(RpcCall::RegisterMappings { specs, .. }) => Some(specs),
                _ => None,
            })
            .expect("the leader chords follow up");
        let defaults = view_core::native::mappings::default_maps();
        assert!(
            specs.iter().all(|s| defaults.contains(s)),
            "no desktop chord is on, so only the default maps register: {specs:?}"
        );
    }

    /// The chord follow-up answers with a second `MappingsClaimed`. A session
    /// with no first-run record to consult still shows each handover once.
    #[test]
    fn a_second_claims_reply_repeats_no_handover_notice() {
        let mut session = NativeSession::desktop(7, None);
        let mut m = model();
        m.record_claimed_keys(vec![MappingClaim::new("picker", "<leader>ff", true)]);
        let _ = session.follow_up(&mut m, Stage::VimEnter);
        let _ = session.follow_up(&mut m, Stage::Claims);
        let first = shown(&m);
        assert!(first.contains("<leader>ff"), "{first:?}");
        let _ = session.follow_up(&mut m, Stage::Claims);
        assert_eq!(
            shown(&m),
            first,
            "the second claims reply repeated a notice"
        );
    }

    /// Input held for a connection that died goes with it: the replacement's
    /// own takeover decides afresh whether to hold.
    #[test]
    fn a_rebind_drops_input_held_for_the_dead_connection() {
        let mut session = NativeSession::desktop(7, None);
        let mut m = model();
        let _ = session.follow_up(&mut m, Stage::VimEnter);
        session.hold_input(Effect::Rpc(RpcCall::Input {
            notation: "x".to_string(),
        }));
        assert!(
            session.release_input().is_empty(),
            "held until the chords go"
        );
        session.rebind(8);
        assert!(!session.holds_input());
        assert!(session.release_input().is_empty());
    }

    /// A session that starts under the editor profile and reads no desktop
    /// chords registers only `default_maps()`. A live `:View keys profile
    /// desktop` flip must reissue the whole current set, with the desktop
    /// chords folded in beside it.
    #[test]
    fn a_profile_flip_reissues_every_default_map() {
        let mut session = NativeSession {
            handed_over: true,
            profile: KeyProfile::Editor,
            initial_profile: KeyProfile::Editor,
            desktop_modifier_choice: ModifierChoice::Auto,
            desktop: default_desktop(),
            ..NativeSession::all_enabled(7, None)
        };
        let mut m = model();
        m.key_profile_override = Some(KeyProfile::Desktop);
        let effects = unbatched(session.follow_up(&mut m, Stage::ProfileFlip));
        let specs = effects
            .iter()
            .find_map(|e| match e {
                Effect::Rpc(RpcCall::RegisterMappings { specs, .. }) => Some(specs),
                _ => None,
            })
            .expect("a profile flip must reissue a RegisterMappings call");
        for spec in view_core::native::mappings::default_maps() {
            assert!(
                specs.iter().any(|s| s.lhs.as_ref() == spec.lhs.as_ref()),
                "the reissue dropped a default map key: {spec:?} missing from {specs:?}"
            );
        }
        assert!(
            specs
                .iter()
                .any(|s| s.feature == "window" && s.verb == "focus_left"),
            "the reissue must fold the desktop chords in once the flip lands \
             on Desktop: {specs:?}"
        );
        assert_eq!(session.profile, KeyProfile::Desktop);
    }

    /// A `:View keys ...` invoke that never touched `key_profile_override`
    /// (a typo, or the current profile named again) still reaches
    /// `Stage::ProfileFlip` -- `feature == "keys"` alone decides the stage,
    /// before `keys_invoke` has parsed the verb -- but must reissue nothing:
    /// the plan is already live under the profile that would have resulted.
    #[test]
    fn a_no_op_flip_reissues_nothing() {
        let mut session = NativeSession {
            handed_over: true,
            profile: KeyProfile::Desktop,
            initial_profile: KeyProfile::Desktop,
            desktop_modifier_choice: ModifierChoice::Auto,
            desktop: default_desktop(),
            ..NativeSession::all_enabled(7, None)
        };
        let mut m = model();
        m.key_profile_override = None;
        let effects = unbatched(session.follow_up(&mut m, Stage::ProfileFlip));
        assert!(
            effects.is_empty(),
            "a flip that settles on the profile already live must reissue nothing: {effects:?}"
        );
        assert_eq!(session.profile, KeyProfile::Desktop);
    }

    /// The kitty keyboard protocol probe answers after the takeover already
    /// spelled the desktop chords under the Alt fallback. `Stage::CapsUpgraded`
    /// must reissue the same chords respelled with the terminal's real
    /// answer, without moving `profile` -- only the modifier changed.
    #[test]
    fn a_late_caps_upgrade_reissues_the_plan() {
        let mut session = NativeSession {
            handed_over: true,
            profile: KeyProfile::Desktop,
            initial_profile: KeyProfile::Desktop,
            desktop_modifier_choice: ModifierChoice::Auto,
            desktop: default_desktop(),
            ..NativeSession::all_enabled(7, None)
        };
        let mut m = model();
        m.caps.kitty_kbd = true;
        let effects = unbatched(session.follow_up(&mut m, Stage::CapsUpgraded));
        let specs = effects
            .iter()
            .find_map(|e| match e {
                Effect::Rpc(RpcCall::RegisterMappings { specs, .. }) => Some(specs),
                _ => None,
            })
            .expect("a caps upgrade must reissue a RegisterMappings call");
        let focus_left = specs
            .iter()
            .find(|s| s.feature == "window" && s.verb == "focus_left")
            .expect("the desktop chord table must still carry focus_left");
        assert_eq!(
            focus_left.lhs.as_ref(),
            "<D-Left>",
            "kitty_kbd=true must respell the chord with the super modifier \
             now that the terminal has actually answered: {specs:?}"
        );
        assert_eq!(
            session.profile,
            KeyProfile::Desktop,
            "a caps upgrade never moves the profile, only the modifier a chord is spelled with"
        );
    }

    /// The desktop chord for `ui gaps` shares its `(feature, verb)` with the
    /// default map's own `ui gaps` row, so a `[keys] toggle_gaps` override
    /// that matched on that pair alone once rewrote the chord's own `lhs`
    /// to the default map's, leaving two specs claiming the same key in one
    /// `RegisterMappings` call -- and the second one's pre-set snapshot
    /// then read as the first one's own fresh mapping where it should hold
    /// nothing, so a later reissue reported it as a user mapping view had
    /// taken.
    #[test]
    fn the_gaps_chord_keeps_its_own_spelling_under_the_toggle_gaps_override() {
        let session = NativeSession {
            handed_over: true,
            profile: KeyProfile::Desktop,
            initial_profile: KeyProfile::Desktop,
            desktop_modifier_choice: ModifierChoice::Auto,
            desktop: default_desktop(),
            ..NativeSession::all_enabled(7, None)
        };
        let mut m = model();
        let (call, _) = session.build_mapping_call(&mut m);
        let specs = match call {
            RpcCall::RegisterMappings { specs, .. } => specs,
            other => panic!("build_mapping_call built {other:?}"),
        };
        let gaps: Vec<_> = specs
            .iter()
            .filter(|s| s.feature == "ui" && s.verb == "gaps")
            .collect();
        assert_eq!(
            gaps.len(),
            2,
            "the default map's own gaps row and the desktop chord's must both register: {specs:?}"
        );
        let lhss: std::collections::BTreeSet<&str> = gaps.iter().map(|s| s.lhs.as_ref()).collect();
        assert_eq!(
            lhss.len(),
            2,
            "the chord and the default map must not collapse onto the same lhs: {specs:?}"
        );
    }

    /// `[keys] desktop_modifier = "super"` with no kitty keyboard protocol
    /// falls back to Alt, and [`profile::modifier_for`] owes a notice
    /// saying so -- `build_mapping_call` must actually record it. Under the
    /// desktop profile, which is the one that actually registers a chord
    /// under the fallback (see
    /// [`a_super_choice_under_the_editor_profile_notices_nothing`]).
    #[test]
    fn a_super_choice_with_no_protocol_notices_the_fallback() {
        let session = NativeSession {
            desktop_modifier_choice: ModifierChoice::Super,
            profile: KeyProfile::Desktop,
            ..NativeSession::all_enabled(7, None)
        };
        let mut m = model();
        m.caps.kitty_kbd = false;
        let _ = session.build_mapping_call(&mut m);
        let raised = format!("{:?}", m.engine.messages.entries);
        assert!(
            raised.contains("desktop_modifier = super needs the kitty keyboard protocol"),
            "a Super choice with no protocol must notice the alt fallback: {raised}"
        );
    }

    /// A session under the editor profile registers no desktop chord at
    /// all, so a `Super` choice with no protocol has nothing to fall back
    /// for -- the notice must not fire on a call that carries no chord,
    /// which is what every `Stage::CapsUpgraded`/`Stage::ProfileFlip`
    /// reissue after the first would otherwise repeat it on.
    #[test]
    fn a_super_choice_under_the_editor_profile_notices_nothing() {
        let session = NativeSession {
            desktop_modifier_choice: ModifierChoice::Super,
            profile: KeyProfile::Editor,
            ..NativeSession::all_enabled(7, None)
        };
        let mut m = model();
        m.caps.kitty_kbd = false;
        let _ = session.build_mapping_call(&mut m);
        assert!(
            m.engine.messages.entries.is_empty(),
            "a call that registers no desktop chord must notice nothing: {:?}",
            m.engine.messages.entries
        );
    }

    /// The mirror of [`a_super_choice_with_no_protocol_notices_the_fallback`]:
    /// once the protocol answers, `Super` is reachable and
    /// [`profile::modifier_for`] owes no notice.
    #[test]
    fn a_super_choice_with_the_protocol_notices_nothing() {
        let session = NativeSession {
            desktop_modifier_choice: ModifierChoice::Super,
            ..NativeSession::all_enabled(7, None)
        };
        let mut m = model();
        m.caps.kitty_kbd = true;
        let _ = session.build_mapping_call(&mut m);
        assert!(
            m.engine.messages.entries.is_empty(),
            "a Super choice with the protocol answered must notice nothing: {:?}",
            m.engine.messages.entries
        );
    }

    /// A bare `:View keys profile` sets [`Model::key_profile_report_requested`]
    /// (`view-core`'s `keys_invoke`); `Stage::ProfileFlip` with that flag set
    /// must report the live profile and modifier and reissue no
    /// registration.
    #[test]
    fn a_profile_flip_with_the_report_flag_reports_instead_of_reissuing() {
        let mut session = NativeSession {
            handed_over: true,
            profile: KeyProfile::Desktop,
            initial_profile: KeyProfile::Desktop,
            desktop_modifier_choice: ModifierChoice::Auto,
            desktop: default_desktop(),
            ..NativeSession::all_enabled(7, None)
        };
        let mut m = model();
        m.key_profile_override = None;
        m.key_profile_report_requested = true;
        let effects = unbatched(session.follow_up(&mut m, Stage::ProfileFlip));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Rpc(RpcCall::RegisterMappings { .. }))),
            "the report flag must take the report path, not a reissue: {effects:?}"
        );
        let raised = format!("{:?}", m.engine.messages.entries);
        assert!(
            raised.contains("keys.profile") && raised.contains("keys.desktop_modifier"),
            "the report path must record the profile and modifier: {raised}"
        );
        assert!(
            !m.key_profile_report_requested,
            "the flag must be cleared once the report is recorded"
        );
    }

    /// A record write stalled on a disk that never answers holds quitting
    /// for the bound and no longer.
    #[test]
    fn quitting_waits_no_longer_than_the_bound_for_a_stalled_record_write() {
        let (release, stalled) = std::sync::mpsc::channel::<()>();
        let mut writer = RecordWriter::default();
        writer
            .start_with(move |_| {
                let _ = stalled.recv();
            })
            .expect("the writer thread must spawn");
        writer.push(RecordWrite::Key("held:vim.notify".to_string()));
        let started = std::time::Instant::now();
        let finished = writer.finish_within(view_proc::writer::QUIT_WAIT);
        let waited = started.elapsed();
        // a timeout is the wait having run its whole bound
        assert!(!finished, "a stalled write must be reported unfinished");
        let budget = view_test_support::HostBudget::new(
            view_proc::writer::QUIT_WAIT,
            std::time::Duration::from_millis(250),
        );
        assert!(
            waited < budget.total(),
            "quitting waited {waited:?} against {budget}"
        );
        drop(release);
    }

    /// Writes that finish end the wait as soon as they do.
    #[test]
    fn quitting_returns_once_the_record_writes_finish() {
        let mut writer = RecordWriter::default();
        writer
            .start_with(|_| {})
            .expect("the writer thread must spawn");
        writer.push(RecordWrite::Key("held:vim.notify".to_string()));
        assert!(writer.finish_within(view_proc::writer::QUIT_WAIT));
    }
}
