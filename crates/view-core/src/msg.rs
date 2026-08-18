//! The runtime loop's message and effect vocabulary. `Msg` is what reaches
//! [`crate::update::update`]; `Effect` is everything it can ask the loop to
//! carry out. The loop's exact call site:
//!
//! ```ignore
//! let msg = match msg_rx.recv() {
//!     Ok(Msg::RedrawReady) => Msg::Redraw(pump.take_damage()),
//!     // a stop describing an engine this session has already replaced is
//!     // dropped here, before anything acts on it
//!     Ok(Msg::EngineStopped { generation, .. }) if generation != engine.generation() => continue,
//!     Ok(Msg::EngineStopped { generation, reason }) => {
//!         let exit = engine.wait_exit();
//!         let recoverable = model
//!             .supervision
//!             .note_engine_stop(exit, engine.handle.announced_exit());
//!         model.fatal_reason = reason.clone();
//!         // a death goes to supervision; an exit the user asked for ends the session
//!         if recoverable { Msg::EngineStopped { generation, reason } } else { Msg::EngineDown(exit) }
//!     }
//!     Ok(m) => m,
//!     Err(_) => Msg::EngineDown(ExitInfo { code: None, by_signal: false }),
//! };
//! for eff in update(&mut model, msg) { /* executor, never blocks */ }
//! ```
use std::time::Duration;

use crate::events::UiEvent;
use crate::model::MessageId;
use crate::native::ai_event::{AiCommand, AiEvent};
use crate::native::mappings::{MappingClaim, MappingSpec};
use crate::native::picker::{PickerItem, Source};

/// Every input `update()` can react to.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Msg {
    /// Already-encoded nvim notation from the terminal input reader.
    Key(Key),
    /// One compacted damage batch, drained by the loop.
    Redraw(Vec<UiEvent>),
    /// Pump token: damage staged; the loop MUST drain it into `Redraw`
    /// before `update()` sees it (raw = silent no-op).
    RedrawReady,
    /// Reader token: engine stream ended; the loop resolves `ExitInfo` and,
    /// with it, whether this stop is a death to recover from or the
    /// session's own ending
    /// ([`SupervisionState::note_engine_stop`](crate::native::supervision::SupervisionState::note_engine_stop)).
    /// A death reaches `update()` as this same message and is acted on by
    /// the liveness fold a later pass takes; an exit is resolved into
    /// [`EngineDown`](Self::EngineDown) before `update()` sees anything.
    /// Carries the reader thread's own reason when it stopped reading for
    /// a cause other than the engine process simply exiting (e.g. an
    /// engine-initiated request it could not route because the runtime
    /// channel was gone), or `None` for an ordinary stream-ended exit. The
    /// reader thread never writes this to stderr itself: it runs headless
    /// behind the terminal's raw-mode alternate screen, so a direct write
    /// there would be invisible until an unrelated redraw scrolled past it
    /// or corrupt the screen outright. The caller stashes it (see the
    /// module doc's call site) and reports it only once the terminal is
    /// restored.
    EngineStopped {
        /// Which connection's reader announced this. One loop channel serves
        /// every engine a session opens, and the reader of an engine being
        /// replaced posts its stop after the replacement is already live, so
        /// a stop is only ever about the connection that carried it. The
        /// loop drops any whose generation is not the one it is running (see
        /// the module doc's call site).
        generation: u64,
        /// The reader thread's own reason, when it stopped reading for a
        /// cause other than the engine process simply exiting.
        reason: Option<String>,
    },
    /// Loop plumbing: startup's pre-attach key-buffering loop consumes this
    /// the moment the background attach thread finishes `nvim_ui_attach`,
    /// unblocking its `recv()` without a poll or a timer. Never reachable
    /// once the steady-state loop in `runtime::run` begins (the sender side
    /// only ever fires once, before that loop starts), but `update()` still
    /// carries a no-op arm for it, mirroring `RedrawReady`/`EngineStopped`'s
    /// contract.
    EngineReady,
    /// A connection is attached, its pump is running, and view may ask it
    /// things: the moment the cutover hands the session over to a new engine,
    /// whether that is the first one or a replacement.
    ///
    /// Distinct from [`Msg::EngineReady`], which fires on the attach call
    /// returning and is consumed before the loop exists. This is folded, so
    /// the effects it owes are dispatched like any other.
    ///
    /// Carries no generation tag, and needs none: one place constructs it,
    /// synchronously, and hands it straight to the dispatcher on the same
    /// thread. It never crosses the message channel, so the race every
    /// engine-originated reply is tagged against -- a replacement engine
    /// writing into the sink its predecessor was given -- cannot reach it.
    EngineAttached,
    EngineDown(ExitInfo),
    EngineRequest(EngineRequest),
    Resized {
        width: u16,
        height: u16,
    },
    /// A terminal bracketed-paste payload, decoded by the input reader from
    /// crossterm's `Event::Paste`.
    Paste(String),
    /// A terminal mouse event, decoded by the input reader from crossterm's
    /// `Event::Mouse` into nvim's button/action/modifier vocabulary.
    Mouse(MouseInput),
    /// The async reply to an `nvim_get_hl(0, {name = "Normal"})` probe
    /// issued by `Effect::Rpc(RpcCall::GetDefaultHl)` on every
    /// `DefaultColorsSet` (see that effect's doc comment for why the probe
    /// exists). `generation` must match `HlTable::probe_generation` at the
    /// moment `update()` applies this or it is dropped as stale -- a reply
    /// for a superseded `DefaultColorsSet` (a colorscheme change that landed
    /// while an earlier probe was still in flight) must never clobber a
    /// newer one. `fg`/`bg` are `None` exactly when the probe reply's wire
    /// map had no `fg`/`bg` key at all, i.e. genuinely unset -- decoded by
    /// `view-engine`, never re-derived from the wire-ambiguous
    /// `default_colors_set` values already in `HlTable::default_fg`/
    /// `default_bg`.
    HlProbeReply {
        generation: u64,
        fg: Option<u32>,
        bg: Option<u32>,
    },
    /// The async acknowledgement of one read-side liveness probe. Carries
    /// no value: that the engine answered at all is the whole signal, and
    /// `generation` names which probe it answered, so a reply arriving out
    /// of order can be folded in without moving the liveness verdict
    /// backwards -- the same stale-reply guard [`Msg::HlProbeReply`]
    /// documents, applied to a watch rather than to a colour.
    ///
    /// Recorded by the runtime loop's own liveness watch as it dispatches
    /// this, not by `update()`: the verdict is a reading of a connection,
    /// which this crate holds no state about.
    HeartbeatReply {
        generation: u64,
    },
    /// The runtime loop's folded reading of both sides of the engine
    /// connection, and how long that reading has held.
    ///
    /// `wedge` is `None` for a healthy connection, which is also how a wedge
    /// is retracted: the banner and the modal both come down on the first
    /// observation that no longer sees one. Carried as a message rather than
    /// applied by the loop directly so that the escalation rule -- banner
    /// now, modal after [`ENGINE_BUSY_MODAL_THRESHOLD`] -- lives in
    /// `update()` with every other state transition, and is provable with no
    /// engine attached.
    ///
    /// `observed_for` is measured by the loop, which owns the clock this
    /// crate does not have. It is the age of the current wedge, restarting
    /// whenever the verdict changes, and is `Duration::ZERO` on the
    /// observation that clears one.
    ///
    /// [`ENGINE_BUSY_MODAL_THRESHOLD`]: crate::native::supervision::ENGINE_BUSY_MODAL_THRESHOLD
    EngineLiveness {
        wedge: Option<crate::native::supervision::WedgeKind>,
        observed_for: Duration,
    },
    /// A user reached a native feature, either through one of view's
    /// registered default keys or through the `:View` command. `feature` is
    /// a [`registry`](crate::native::registry) id and `verb` the entry point
    /// named by the [`MappingSpec`] that registered the key.
    ///
    /// Unvalidated on arrival: the pair crosses back from nvim, where a user
    /// can type any words after `:View`, so the arm that handles this is
    /// what decides an unknown pair is not actionable.
    FeatureInvoke {
        feature: String,
        verb: String,
    },
    /// The async answer to one [`RpcCall::ProbeSwapRecovery`]: what this
    /// connection's engine replayed out of a swap file while it was
    /// starting, whether it wrote its own report about doing so, and the
    /// error it raised if the recovery could not be performed at all.
    ///
    /// `count` is how many buffers came back holding work the file on disk
    /// does not have -- the fact a user is owed a line about. `reported` is
    /// the wider one: nvim writes its multi-line recovery report whenever it
    /// replays a swap, so a recovery that restored nothing changed still
    /// leaves a report over the buffer that only a redraw takes down.
    /// `failure` is the third case and the one neither of the others can
    /// express: the recovery was asked for and did not happen, leaving an
    /// error on screen saying why. `empty` says whether that left the user
    /// looking at an empty buffer, and it is read out of the buffer rather
    /// than assumed from the error, because a failed recovery is not one
    /// shape -- some leave the file on disk in place.
    ///
    /// All of them are empty for the ordinary session, which is the common
    /// case and not an error: a session that met no swap file has nothing to
    /// announce and nothing on its screen to clear. The arm that folds this
    /// is what decides that, so the reading still arrives.
    ///
    /// `generation` must match the model's own outstanding swap probe, the
    /// same stale-reply guard [`Msg::HlProbeReply`] documents: a restart
    /// hands the replacement engine the sink the dead one wrote into.
    SwapRecovered {
        generation: u64,
        count: u64,
        reported: bool,
        failure: Option<String>,
        empty: bool,
    },
    /// The complete answer to one [`RpcCall::RegisterMappings`]: every
    /// default key the session registered, and whether it landed over a
    /// mapping the user's config had already made.
    ///
    /// One message rather than one per key. "What did view claim?" is a
    /// single fact the user is told once, and reassembling it view-side from
    /// N replies interleaved with startup traffic would make the report a
    /// function of arrival order.
    MappingsClaimed {
        claimed: Vec<MappingClaim>,
    },
    /// nvim applied a new colorscheme: the `ColorScheme` autocmd registered
    /// by [`RpcCall::RegisterBridge`] fired, carrying the scheme's name (or
    /// an empty string when nvim reported none).
    ///
    /// Carries no colors. The colors themselves arrive as ordinary
    /// `default_colors_set`/`hl_attr_define`/`hl_group_set` redraw events
    /// and are already in [`HlTable`](crate::hl::HlTable) by the time the
    /// next frame derives a
    /// [`Theme`](crate::theme::Theme) from it. What this message adds is
    /// the *fact* that a switch happened, which the redraw stream does not
    /// state anywhere: a highlight batch looks identical whether a plugin
    /// redefined one group or the user changed their whole colorscheme, so
    /// a consumer that must act once per switch (persisting the cold-start
    /// theme cache) has nothing else to hang off.
    ///
    /// # Ordering: neither this message nor its colors is guaranteed first
    ///
    /// nvim fires the autocmd while applying the scheme and flushes the UI
    /// batch afterwards, but that write order is not the order a consumer
    /// observes. Redraw damage reaches the runtime loop coalesced behind a
    /// wakeup token travelling the same channel, so highlights folded into a
    /// token that was already queued are delivered ahead of an announcement
    /// still waiting its turn. Both orders occur in ordinary sessions.
    ///
    /// A consumer that must act on the switch's colors therefore cannot read
    /// the highlight table on receipt and stop: reading here may see the OLD
    /// theme, and waiting for the next batch may wait forever because the
    /// new one already went by. Act on both edges instead -- on this message
    /// and on the following highlight-bearing batch -- and deduplicate on
    /// the state already acted upon.
    ColorSchemeChanged {
        name: String,
    },
    /// The `DiagnosticChanged` autocmd registered by [`RpcCall::RegisterBridge`]
    /// fired; `errors`/`warnings` are `vim.diagnostic.count(0)`'s totals for
    /// severities 1 and 2, computed inside the Lua callback itself (a
    /// synchronous, non-blocking nvim API call -- see
    /// `docs/statusline-wire-capture.md`'s "no worker crate" conclusion) so
    /// no separate RPC round trip is needed to learn the count that fired
    /// the autocmd in the first place.
    DiagnosticsChanged {
        errors: u32,
        warnings: u32,
    },
    /// The bridge's git-branch trigger group (`BufEnter`, `DirChanged`,
    /// `FocusGained`) fired and the Lua callback's `vim.system()` git lookup
    /// (async, off nvim's main loop) resolved. Empty `branch` means either
    /// no repo or the lookup failed -- the statusline segment hides itself
    /// on empty, the same convention `SegmentUpdate`'s other segments use.
    GitBranchChanged {
        branch: String,
    },
    /// The bridge's new `buffer` trigger group (`BufEnter`, `BufFilePost`,
    /// `BufWritePost`, `BufModifiedSet`) fired, carrying the current
    /// buffer's tail name and modified flag for the statusline's file
    /// segment. Deliberately not `TextChanged`/`TextChangedI`: those fire
    /// once per keystroke in insert mode, and `BufModifiedSet` alone already
    /// covers every actual modified-flag transition.
    BufferChanged {
        name: String,
        modified: bool,
    },
    /// A `Route::Transient` toast's idle timeout elapsed with no other input
    /// to have dismissed it another way. `id` names the exact
    /// [`MessageEntry`](crate::model::MessageEntry) `toast::route` scheduled
    /// this for, since by the time the timer fires `Messages::push` may have
    /// appended or replaced other entries at arbitrary positions in the log.
    /// A no-op if that entry is already gone (cleared, replaced, or
    /// dismissed by a keypress in the meantime) -- expiry always races
    /// those, and losing the race is exactly "already handled", not an
    /// error.
    ToastExpired {
        id: MessageId,
    },
    /// The matcher worker's answer to one `Effect::PickerQuery`, streamed:
    /// the worker sends this as many times as its nucleo tick loop produces
    /// a new ranked prefix for a still-running Files scan, not once at the
    /// end. `generation` must match `PickerState::generation` at the moment
    /// `update()` applies this or it is dropped as stale, the same contract
    /// [`Msg::HlProbeReply`] documents for `HlTable::probe_generation` -- a
    /// reply for a query a keystroke has since superseded must never
    /// clobber a newer one.
    PickerResults {
        generation: u64,
        items: Vec<PickerItem>,
    },
    /// The decoded answer to one `RpcCall::ListBuffers`, resolving
    /// `Source::Buffers`'s corpus; see
    /// `docs/picker-buffer-list-wire-capture.md`. `names` is each listed
    /// buffer's path, empty string standing for nvim's own `[No Name]`
    /// scratch buffer. Generation-gated on the same terms as
    /// `PickerResults`.
    PickerBufferList {
        generation: u64,
        names: Vec<String>,
    },
    /// The decoded answer to one `RpcCall::PreviewBuffer`, resolving the
    /// preview pane's text for the picker's selected candidate; see
    /// `docs/picker-preview-wire-capture.md`. `path` echoes back the path
    /// the request was issued for (the selection may have moved on by the
    /// time this lands, and the applier needs to know which candidate this
    /// answers, not only which generation). `loaded` is `false` exactly
    /// when nvim has no buffer open for `path` -- `lines` is empty in that
    /// case, and the applier's next step is `Effect::PickerPreviewFallback`,
    /// never this reply's own empty `lines` misread as "an empty file".
    /// Generation-gated on the same terms as `PickerResults`.
    PickerPreviewReply {
        generation: u64,
        path: String,
        loaded: bool,
        lines: Vec<String>,
    },
    /// The disk-fallback read `Effect::PickerPreviewFallback` requested,
    /// for a candidate `PickerPreviewReply` reported `loaded: false` for.
    /// `lines` is `None` for a path that does not exist or could not be
    /// read as UTF-8 -- the preview pane shows nothing rather than a
    /// misleading placeholder for either. Generation-gated on the same
    /// terms as `PickerResults`.
    PickerPreviewFile {
        generation: u64,
        lines: Option<Vec<String>>,
    },
    /// The `ignore`-walked filesystem scan's answer for one
    /// `Effect::TreeScan`, tagged `generation`
    /// (`TreeState::generation` at the moment `update()` emitted the
    /// request). Generation-gated on the same terms as `PickerResults`:
    /// `TreeState::apply_scan` drops a reply for any generation but its
    /// own.
    TreeScanResult {
        generation: u64,
        entries: Vec<crate::native::tree::TreeEntry>,
    },
    /// `git status --porcelain=v2`'s answer for one `Effect::TreeGitScan`,
    /// tagged `generation` (`TreeState::git_generation` at the moment
    /// `update()` emitted the request). An empty `status` is not an
    /// error -- it is what a clean tree, or a tree with `git` absent from
    /// `PATH`, both report, and `TreeState::apply_git` renders either
    /// undecorated. Generation-gated on the same terms as `PickerResults`.
    ///
    /// `timed_out` tells apart the two ways `status` can be empty: `false`
    /// covers the ordinary cases above, `true` means the subprocess itself
    /// was killed for outliving its own bounded wait (a wedged `git`, not a
    /// clean or git-less tree) -- `update()` surfaces that case as a notice
    /// rather than rendering it identically to "nothing to decorate".
    TreeGitResult {
        generation: u64,
        status: Vec<crate::native::tree::GitEntry>,
        timed_out: bool,
    },
    /// The decoded answer to one `RpcCall::RenameFile`, tagged `generation`
    /// (`TreeState::generation` at the moment `update()` emitted the
    /// rename, reused rather than a fresh generation since the reply's
    /// only job is triggering the rescan that follows a successful
    /// rename -- see `RpcCall::RenameFile`'s doc). `ok` is `false` when the
    /// destination already existed or the rename otherwise failed; either
    /// way the buffer this rename targeted, if any, is left exactly where
    /// it was, so a failed rename never orphans it.
    TreeRenameReply {
        generation: u64,
        ok: bool,
    },
    /// The decoded answer to one `RpcCall::TreeCreatePrompt`: the filename
    /// text nvim's blocked `vim.fn.input()` collected, or `None` when the
    /// user cancelled (`<Esc>`) or submitted an empty answer -- the two are
    /// indistinguishable at the wire (both return `""`, verified live), and
    /// conveniently both mean "nothing to create" here. `generation` is
    /// `TreeState::generation` at the moment `update()` issued the prompt;
    /// `update()` drops a reply whose generation no longer matches the
    /// tree's current one (the tree was closed and reopened, or rescanned,
    /// while the prompt was still open), unlike `TreeRenameReply`'s reused
    /// generation, which names the rename call itself rather than a tree
    /// state to compare against.
    TreeCreatePromptReply {
        generation: u64,
        name: Option<String>,
    },
    /// The decoded answer to one `RpcCall::TreeRenamePrompt`, on the same
    /// "`None` means cancelled or empty" terms as
    /// [`Msg::TreeCreatePromptReply`]. `old_path` echoes back the entry the
    /// prompt was opened for (the reply itself carries no path), so a
    /// generation match still names the right file to rename even though
    /// nothing else about the tree's selection can move while the prompt
    /// holds focus.
    TreeRenamePromptReply {
        generation: u64,
        old_path: String,
        name: Option<String>,
    },
    /// The decoded answer to one `RpcCall::TreeDeleteConfirm`. `path` echoes
    /// back the entry the prompt was opened for, on the same terms
    /// `TreeRenamePromptReply::old_path` does.
    TreeDeleteConfirmReply {
        generation: u64,
        path: String,
        outcome: DeleteConfirmOutcome,
    },
    /// `Effect::TreeCreateFile`'s answer: `ok` is `false` when the
    /// destination already existed or the create otherwise failed. Either
    /// way nothing on disk changed that was not already there -- see
    /// `Effect::TreeCreateFile`'s doc on why `create_new` rather than a
    /// truncating write.
    TreeCreateFileResult {
        generation: u64,
        ok: bool,
    },
    /// Symmetric to [`Msg::TreeCreateFileResult`], for
    /// `Effect::TreeDeleteFile`.
    TreeDeleteFileResult {
        generation: u64,
        ok: bool,
    },
    /// Everything the agent side reports, in the closed vocabulary of
    /// [`AiEvent`]. One arm rather than one per agent event, because the
    /// crate that speaks the agent protocol is the only thing that ever
    /// constructs these and it has no business naming the rest of this
    /// enum -- the same boundary [`Effect::Rpc`] draws in the other
    /// direction.
    Ai(AiEvent),
    /// The trust store's own answer to one [`Effect::AiTrustSet`], carried
    /// back by the bin's executor -- the only code with a `view-ai`
    /// dependency -- so `update()` can fold the durable fact into
    /// `Model::ai_trusted` without ever calling into the store itself.
    /// `trusted` is `false` both for a user who declined and for a
    /// persistence failure after an affirmative answer: the store write is
    /// what durably grants trust, so a write that failed must not leave the
    /// in-memory model believing it succeeded anyway (see
    /// [`Effect::AiTrustSet`]'s own doc).
    AiTrustResolved {
        trusted: bool,
    },
}

/// The three outcomes [`Msg::TreeDeleteConfirmReply`] can carry, closed by
/// construction so a caller cannot observe "confirmed" and "a loaded buffer
/// blocked this" at once: the engine-side chunk checks `bufloaded` before it
/// ever offers `vim.fn.confirm`, so those two states are mutually exclusive
/// on the wire itself, not just by convention here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteConfirmOutcome {
    /// The user answered "Yes" (`vim.fn.confirm`'s first choice): the
    /// delete may proceed.
    Confirmed,
    /// `<Esc>`, "No", or an error reply -- all degrade to the same "safe
    /// default over an ambiguous or lost reply" precedent every other async
    /// reply in this crate follows.
    Declined,
    /// `path` names a file with a loaded buffer still open on it: the
    /// engine refused to even offer the confirm prompt, since deleting a
    /// path nvim still owns a buffer for would silently orphan it.
    BufferOpen,
}

/// One decoded mouse event in nvim `nvim_input_mouse` vocabulary: `button`
/// is one of `"left"`/`"right"`/`"middle"`/`"wheel"`/`"move"`; `action` is
/// `"press"`/`"drag"`/`"release"` for ordinary buttons, `"up"`/`"down"`/
/// `"left"`/`"right"` for the wheel, and ignored for `"move"`; `modifier` is
/// a string of single-char modifier prefixes (`"C-"`, `"S-"`, `"M-"`, in
/// that order, matching `view_tui::keys::encode_key`'s convention).
/// `row`/`col` are the raw terminal cell position the input reader
/// observed, zero-based; `update()` is what maps them into engine grid
/// coordinates, since only it has the chrome-reservation state to do so.
#[derive(Debug, Clone)]
pub struct MouseInput {
    pub button: String,
    pub action: String,
    pub modifier: String,
    pub row: u16,
    pub col: u16,
}

/// A key event already encoded to nvim `nvim_input` notation.
#[derive(Debug, Clone)]
pub struct Key {
    pub notation: String,
}

/// How the engine process exited.
///
/// Producers compute `code` before this reaches `update()`: unix signal
/// death is `code = Some(128 + signal)`. `update()` maps `code: None`
/// (status unreadable) to exit 1. `RedrawReady`/`EngineStopped` are loop
/// plumbing: the loop resolves them before `update()`; `update()` returns
/// no effects for them (totality).
#[derive(Debug, Clone, Copy)]
pub struct ExitInfo {
    pub code: Option<i32>,
    /// Whether `code` came from a signal death rather than a normal exit
    /// status. The process exit code the bin crate reports is computed from
    /// `code` alone, but this is what separates an engine that died from one
    /// that was told to stop: see
    /// [`SupervisionState::note_engine_stop`](crate::native::supervision::SupervisionState::note_engine_stop),
    /// which recovers only from the former.
    pub by_signal: bool,
}

/// Engine-initiated requests, decoded to a closed vocabulary in
/// `view-engine`; unknown methods never reach core (the reader auto-errors
/// them, as built).
///
/// The engine BLOCKS awaiting the reply, so every arm MUST produce exactly
/// one `Effect::Reply`; the reply routes through the writer thread's
/// channel and never blocks the loop. Future request kinds (modal prompts,
/// a clipboard provider) extend this enum; the one-reply-per-arm dispatch
/// seam is the fixed contract any new variant must honor.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum EngineRequest {
    VimEnter {
        token: ReplyToken,
    },
    /// `"+p`/`"*p`: the injected `g:clipboard.paste` closure blocks nvim on
    /// this `rpcrequest`, so the loop must delegate rather than answer
    /// inline -- see [`Effect::ClipboardRead`]. `register` is `'+'` or
    /// `'*'`; view wires both to the same backend (see
    /// `Effect::ClipboardRead`'s doc for why).
    ClipboardGet {
        token: ReplyToken,
        register: char,
    },
    /// `"+yy`/`"*yy`: the injected `g:clipboard.copy` closure blocks nvim on
    /// this `rpcrequest` the same way `ClipboardGet` does, so a copy and a
    /// paste that race each other serialize through the same one-token,
    /// one-reply contract instead of one silently overtaking the other.
    /// `regtype` is nvim's own copy of the register type for these `lines`,
    /// forwarded unchanged from the `copy` closure's second argument -- see
    /// [`RegisterType`] for why the system-clipboard backend needs it at
    /// all.
    ClipboardSet {
        token: ReplyToken,
        register: char,
        lines: Vec<String>,
        regtype: RegisterType,
    },
}

/// Identifies the pending msgpack-RPC request a reply must answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyToken {
    pub msgid: u64,
}

/// nvim's register type for a `g:clipboard` copy/paste, per `:help
/// setreg()`. Only the two shapes this provider's system-clipboard backend
/// distinguishes: `Charwise` for `"v"`, `Linewise` for `"V"`. A blockwise
/// regtype (`"\x16{width}"`, `:help blockwise-visual`) decodes to
/// `Charwise` -- the system clipboard is plain text with no column-block
/// concept to keep one for, and falling back loses only the block shape,
/// never the copied text.
///
/// This is the fix for the cross-process yank/paste divergence a
/// same-session round trip through view alone could not catch: nvim's own
/// shell-based clipboard providers signal linewise by writing a trailing
/// `\n` onto the plain text they hand the system clipboard tool (and read
/// that same trailing `\n` back to reconstruct it), which is the exact
/// signal a `char`-only register model has no field to carry. Threading
/// this enum end to end -- into [`EngineRequest::ClipboardSet`],
/// [`Effect::ClipboardWrite`], [`Effect::Osc52Copy`], and back out as half
/// of the `[lines, regtype]` pair [`ReplyValue::ClipboardLines`] answers a
/// paste with -- is what lets `view-native::clipboard`'s text/line
/// conversion apply that same trailing-newline convention instead of
/// discarding the one signal nvim itself relies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterType {
    Charwise,
    Linewise,
}

impl RegisterType {
    /// Decodes nvim's own regtype string (`"v"`, `"V"`, or a blockwise
    /// `"\x16{width}"`) into the two shapes this provider keeps. Any string
    /// not starting with `'V'` decodes to `Charwise`, matching nvim's own
    /// default when a `g:clipboard.paste` closure omits a regtype
    /// entirely (see [`RegisterType`]'s doc).
    #[must_use]
    pub fn from_nvim(regtype: &str) -> Self {
        match regtype.chars().next() {
            Some('V') => Self::Linewise,
            _ => Self::Charwise,
        }
    }

    /// nvim's own wire spelling for this shape, used when answering a
    /// paste with the `[lines, regtype]` pair form (see
    /// `docs/clipboard-provider-wire-capture.md`).
    #[must_use]
    pub fn as_nvim_str(self) -> &'static str {
        match self {
            Self::Charwise => "v",
            Self::Linewise => "V",
        }
    }
}

/// The value an `Effect::Reply` sends back to the engine.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum ReplyValue {
    Nil,
    /// The system clipboard's current lines and register type, answering
    /// [`EngineRequest::ClipboardGet`] with the `[lines, regtype]` pair
    /// form `g:clipboard.paste` accepts -- the shape
    /// `docs/clipboard-provider-wire-capture.md` verified against the
    /// pinned engine and the one that round-trips linewise/charwise
    /// fidelity through `"+yy`/`"+p` (see [`RegisterType`]'s doc for why a
    /// bare list, which nvim would default to charwise, is not enough).
    ClipboardLines {
        lines: Vec<String>,
        regtype: RegisterType,
    },
}

/// The largest base64-encoded [`Effect::Osc52Copy`] payload the runtime
/// loop will write to the terminal. A yank has no upper bound of its own
/// (`"+yG` on a large file yanks the whole buffer), while OSC 52 is
/// synchronous on the loop thread and unbounded terminal emulators exist
/// that buffer the entire escape before acting on it -- so an oversized
/// copy is skipped rather than base64'd and written, trading a silent
/// remote-clipboard miss (the local system clipboard write from the
/// companion `ClipboardWrite` effect still succeeds) for a bounded worst
/// case on the paint thread. 100 KiB of encoded bytes is roughly 75 KiB of
/// yanked text, generous for the terminal-copy use case OSC 52 exists for
/// and small enough that the write stays well under a frame budget on any
/// terminal that does act on it synchronously.
pub const OSC52_MAX_PAYLOAD_BYTES: usize = 100 * 1024;

/// Everything `update()` can ask the loop's executor to carry out. The
/// executor never blocks; every effect crosses a channel or is fired and
/// forgotten.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Effect {
    Rpc(RpcCall),
    Reply {
        token: ReplyToken,
        value: ReplyValue,
    },
    /// Hands a paste read to the clipboard worker off the loop thread; the
    /// worker, not this arm, owns answering `token` (see
    /// [`EngineRequest::ClipboardGet`]). Never a cached read (Option A,
    /// rejected): the system clipboard can change between a cache refresh
    /// and the paste that reads it, and a cache would then hand back text
    /// the user did not most recently copy -- a silent data-correctness
    /// bug. `register` is `'+'` or `'*'`, forwarded unchanged; both map to
    /// the one system clipboard arboard exposes; there is no cross-platform
    /// primary-selection equivalent to give `'*'` a distinct backend.
    ClipboardRead {
        token: ReplyToken,
        register: char,
    },
    /// Hands a yank write to the clipboard worker the same way
    /// `ClipboardRead` hands off a paste; the worker owns answering `token`
    /// (see [`EngineRequest::ClipboardSet`]). Companion to
    /// [`Osc52Copy`](Self::Osc52Copy), which `update()` emits alongside
    /// this on every copy: the local write and the terminal escape are two
    /// effects from one arm, not a branch on whether a local display is
    /// present, so `"+yy` behaves identically over SSH. `regtype` is
    /// forwarded from `EngineRequest::ClipboardSet` unchanged; see
    /// [`RegisterType`] for why the local write needs it.
    ClipboardWrite {
        token: ReplyToken,
        register: char,
        lines: Vec<String>,
        regtype: RegisterType,
    },
    /// Writes an OSC 52 clipboard-set escape sequence to the real terminal.
    /// Routed to `view-tui`, never issued by the clipboard worker itself:
    /// only `view-tui` touches the terminal, and the worker thread writing
    /// raw bytes to stdout could interleave with the paint loop's own
    /// buffered frame flush. Carries no `ReplyToken`: unlike
    /// `ClipboardRead`/`ClipboardWrite`, nothing on the wire is blocked on
    /// this, so a terminal that ignores or strips OSC 52 costs nothing
    /// beyond the escape sequence itself -- which is also why the runtime
    /// loop's own drain site treats both a transient stdout error and a
    /// payload over [`OSC52_MAX_PAYLOAD_BYTES`] as skip-and-log rather than
    /// a session-ending failure: this effect is fire-and-forget end to end,
    /// never just at the wire-reply layer. `regtype` is the same one
    /// `ClipboardWrite` carries for this copy: the remote clipboard this
    /// escape targets must see the identical linewise/charwise text shape
    /// the local system clipboard just got, not a charwise-only rendering
    /// of the same `lines` (see [`RegisterType`]).
    Osc52Copy {
        register: char,
        lines: Vec<String>,
        regtype: RegisterType,
    },
    Quit {
        exit_code: i32,
    },
    /// Tears the current engine down and brings a fresh one up, keeping the
    /// session and its terminal.
    ///
    /// Carried by the loop rather than the executor: the executor holds only
    /// a clone of the engine's RPC handle, while the `Engine` value whose
    /// lifetime this effect changes belongs to the loop. Recovery is nvim's
    /// own -- the fresh engine inherits whatever its swap file persisted, and
    /// view replays nothing, since it never held authoritative buffer text
    /// to replay.
    RestartEngine,
    /// Arms the idle-expiry timer for one `Route::Transient` toast: after
    /// `after` elapses with no other effect having cancelled it, the timer
    /// worker sends `Msg::ToastExpired { id }` back into the loop. One-shot
    /// per toast (a background thread that owns exactly one send), not a
    /// persistent multi-deadline scheduler. Chosen over evaluating expiry at paint
    /// time (shape B in the design note) because the runtime loop has no
    /// free-running clock: nothing else causes a repaint on an idle editor,
    /// so a paint-time check would never re-run and the toast would never
    /// expire.
    ScheduleToastExpiry {
        id: MessageId,
        after: Duration,
    },
    /// Hands a picker query to the matcher worker off the loop thread; the
    /// worker, not this arm, owns streaming back `Msg::PickerResults`.
    /// Issued whenever a picker overlay opens (empty `needle`, seeding the
    /// worker's corpus for `source`) and on every query edit thereafter.
    /// `generation` is `PickerState::generation` at the moment `update()`
    /// emitted this, so a reply the worker is still computing when a later
    /// keystroke supersedes it can be told apart from a current one.
    ///
    /// `resolved` carries a corpus `view-native` cannot gather itself:
    /// `Source::Buffers`'s listed-buffer names, decoded from
    /// `Msg::PickerBufferList` (the RPC reply only `view-engine` can issue,
    /// per the crate boundary -- see `update::update`'s `PickerBufferList`
    /// arm). `Some` replaces the worker's cached corpus for `source` before
    /// matching; `None` reuses whatever corpus is already cached for it,
    /// which is every query after the first for `Source::Files` (the worker
    /// walks the tree itself, once, on the first query) and every query
    /// after the seeding one for `Source::Buffers`. `Source::LiveGrep` never
    /// takes this path: the query text is the search pattern itself, not a
    /// fuzzy filter over a static corpus, so the worker re-walks and
    /// re-searches on every distinct query rather than caching a corpus to
    /// filter.
    PickerQuery {
        generation: u64,
        needle: String,
        source: Source,
        resolved: Option<Vec<PickerItem>>,
    },
    /// Tells the matcher worker to drop its live `Session` (see
    /// `view_native::picker::matcher::Session`), the moment a picker overlay
    /// closes rather than only when a later, differently-sourced query
    /// happens to replace it. A `Files` scan is a background thread that
    /// keeps walking a possibly huge tree until told otherwise; without this
    /// effect, closing a picker (the dominant way a user abandons a scan --
    /// `<Esc>` on a topmost `Picker` overlay, see `update::update`'s
    /// `Msg::Key` arm) would leave that thread running unobserved, since
    /// `PickerQuery`'s own session-replacement path only fires on the next
    /// `MatchRequest` for a different source, which may never arrive.
    /// Carries no fields: the worker keeps at most one session alive at a
    /// time, so "close it" needs no source or generation to disambiguate.
    PickerClose,
    /// Reads `path` from disk off the paint loop, for a preview candidate
    /// `Msg::PickerPreviewReply` reported `loaded: false` for -- nvim has no
    /// buffer open for it, so there is no RPC content to read instead. The
    /// read itself is plain `std::fs` I/O in `view-native`
    /// (`view_native::picker::preview::read_file`), never RPC: only
    /// `view-engine` speaks RPC, and this path exists precisely because RPC
    /// already answered "nothing to read here." `generation` is
    /// `PickerState::generation` at the moment `update()` emitted this, the
    /// same contract every other picker generation carries.
    PickerPreviewFallback {
        generation: u64,
        path: String,
    },
    /// Hands an `ignore`-walked filesystem scan of `root` to a worker off
    /// the loop thread; the worker, not this arm, owns streaming back
    /// `Msg::TreeScanResult`. Issued once when a tree overlay opens and
    /// again whenever `TreeState::request_rescan` allocates a fresh
    /// generation (today, only after a successful rename -- see
    /// `RpcCall::RenameFile`'s doc on why that reply cannot rely on the
    /// autocmd bridge's ordinary write callbacks to trigger it).
    TreeScan {
        generation: u64,
        root: std::path::PathBuf,
    },
    /// Hands a `git status --porcelain=v2` refresh of `root` to a worker off
    /// the loop thread; the worker, not this arm, owns answering
    /// `Msg::TreeGitResult`. Issued when a tree overlay opens and again on
    /// every autocmd bridge write/focus callback while it stays open --
    /// never on a timer, so a tree left open and idle issues no background
    /// git traffic at all.
    TreeGitScan {
        generation: u64,
        root: std::path::PathBuf,
    },
    /// Tells the tree's scan worker to cancel any filesystem walk still in
    /// flight, the moment the sidebar closes -- see `Effect::PickerClose`'s
    /// doc for the identical problem this solves on the other overlay:
    /// `tree::fs::scan` walks the whole tree in one blocking call with no
    /// generation check of its own until it returns, so a huge tree closed
    /// mid-scan would otherwise keep a thread walking it, unobserved, for as
    /// long as the walk takes. Carries no fields, like `PickerClose`: the
    /// executor keeps at most one tree scan's cancel flag alive at a time.
    TreeClose,
    /// Creates a new, empty file at `path` on disk, refusing to overwrite an
    /// existing one -- see the executor's own doc for why `create_new`
    /// rather than a plain truncating write. A genuine filesystem effect,
    /// never RPC: an as-yet-nonexistent path names no buffer for nvim to
    /// own, so there is nothing for the engine to be authoritative over
    /// until the file is opened afterward (an ordinary
    /// `RpcCall::OpenFile`). `generation` is `TreeState::generation` at the
    /// moment `update()` emitted this, carried through to the reply
    /// (`Msg::TreeCreateFileResult`) on the same terms `RpcCall::RenameFile`
    /// carries it to `Msg::TreeRenameReply`: not compared against the
    /// tree's own counters on arrival, since the reply's only job is
    /// triggering the rescan that follows a successful create.
    TreeCreateFile {
        path: std::path::PathBuf,
        generation: u64,
    },
    /// Deletes the file at `path` from disk. A genuine filesystem effect
    /// like `TreeCreateFile`, and, symmetrically, never issued for a path
    /// with a loaded buffer still open on it: `update()` only ever emits
    /// this from a `DeleteConfirmOutcome::Confirmed` reply, and the engine
    /// side of that reply (`TREE_DELETE_CONFIRM_CHUNK`) already refused to
    /// offer the confirm prompt at all when `bufloaded` found one -- the
    /// same buffer-identity boundary `RpcCall::RenameFile` draws, enforced
    /// here rather than merely documented. `generation` carries through to
    /// `Msg::TreeDeleteFileResult` on the same terms `TreeCreateFile`'s
    /// does.
    TreeDeleteFile {
        path: std::path::PathBuf,
        generation: u64,
    },
    /// Hands one [`AiCommand`] to the agent session off the loop thread.
    /// Non-blocking like every other effect: the session queues the command
    /// onto its own channel and returns, so a busy or wedged agent can
    /// never stall the executor -- the same contract "the paint loop never
    /// awaits RPC" states for [`Rpc`](Self::Rpc), extended to agent traffic.
    Ai(AiCommand),
    /// Persists the user's answer to the per-project AI trust gate
    /// (`update/mod.rs`'s `Msg::FeatureInvoke` arm) through
    /// `view_ai::TrustStore::set_trusted`, the one crossing point plain data
    /// takes instead of a call: `view-core` cannot depend on `view-ai` (see
    /// `scripts/audit-deps.sh`), so the write itself happens in the bin's
    /// executor, which answers with [`Msg::AiTrustResolved`] once the write
    /// (or its failure) is known. Never issued for a prompt whose answer
    /// routes to the engine instead -- see `PromptState`'s own `Origin`
    /// distinction (`native/prompt.rs`).
    AiTrustSet {
        project_root: std::path::PathBuf,
        trusted: bool,
    },
}

/// The value side of [`RpcCall::SetOption`] and [`RpcCall::HoldOption`],
/// spelled in Rust's own scalar types so `view-core` stays free of `rmpv`:
/// the crate that speaks the wire maps each variant onto a msgpack value.
///
/// Deliberately NOT `#[non_exhaustive]`, unlike its neighbours here. These
/// three are nvim's whole option value domain (`:help option-types`:
/// number, boolean, string), so the enum is closed by the API it models
/// rather than by this crate's current needs, and every mapping of it can
/// be total. A fourth variant would be a genuine change of what an nvim
/// option is, and the compile error it would cause at each mapping site is
/// the desired outcome: an unmapped option value silently degrading to a
/// no-op would leave a feature believing it had taken over a surface it
/// never touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionValue {
    /// A number option, e.g. `laststatus`.
    Int(i64),
    /// A boolean option, e.g. `ruler`.
    Bool(bool),
    /// A string option, e.g. `statusline`.
    Str(String),
}

/// A closed vocabulary of RPC calls instead of `(method, Vec<Value>)`: core
/// stays rmpv-free and an unencodable call is unrepresentable. Runner-up
/// (stringly method + opaque params) rejected: re-opens the door to core
/// building wire values.
// PartialEq/Eq (which `Msg` and `Effect` do not carry): a call is a value a
// caller assembles ahead of emitting it, so the assembled call has to be
// comparable to the exact call that was meant, field for field, without a
// hand-written `matches!` arm per variant that silently stops checking a
// field the day one is added.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcCall {
    Input {
        notation: String,
    },
    TryResize {
        width: u16,
        height: u16,
    },
    Paste {
        text: String,
    },
    /// Forwards one mouse event via `nvim_input_mouse`. `grid` is not a
    /// field here: single-grid semantics hardcode it to `0` at the call
    /// site in `view-engine`'s `input_mouse`, letting nvim itself resolve
    /// window positioning rather than this frontend tracking multigrid
    /// window layout.
    InputMouse {
        button: String,
        action: String,
        modifier: String,
        row: u16,
        col: u16,
    },
    /// Sets one nvim option to `value` via `nvim_set_option_value`, the
    /// non-interactive channel every option change view makes on the user's
    /// behalf rides.
    ///
    /// Never `Input`: that variant shares one stream with startup's
    /// buffered-key replay and with live typeahead, so an ex-command
    /// interleaved into it lands wherever the session's mode happens to be
    /// at that instant. A user left in insert mode by replayed keys would
    /// get `:set laststatus=0` typed into their buffer rather than applied
    /// to their session. An API call carries no mode dependency at all.
    ///
    /// Fire-and-forget like every other `RpcCall`, and reversible by
    /// construction: nothing here writes to the user's config, so the
    /// option returns to whatever that config set the moment the feature
    /// that asked for it is turned off and the session is restarted.
    SetOption {
        name: String,
        value: OptionValue,
    },
    /// Sets one nvim option to `value` and keeps it there for the rest of
    /// the session: the durable form of [`SetOption`](Self::SetOption), for
    /// a surface view has taken over from a plugin that is still running.
    ///
    /// A one-shot write is not enough for a takeover. A plugin that owns a
    /// surface re-asserts its own option on its own events -- lualine
    /// re-runs `setup()` on `ColorScheme` and on `OptionSet background`,
    /// and that `setup()` sets `laststatus` back (observed live against the
    /// compat harness's heavy fixture: `laststatus` returned to `2` on the
    /// first `:colorscheme` after a plain set to `0`). Nothing about that
    /// fails loudly: view would go on drawing a status line it no longer
    /// owned, over the one nvim had resumed drawing.
    ///
    /// Reversible on exactly the same terms as every other call here: the
    /// hold is session state, never a config edit, so it is gone the moment
    /// the session ends and it is never issued at all for a feature the
    /// user has turned off.
    HoldOption {
        name: String,
        value: OptionValue,
    },
    /// Issues an async `nvim_get_hl(0, {name = "Normal"})` probe, tagged
    /// with `generation` (`HlTable::probe_generation` at the moment
    /// `update()` emitted this, from its `DefaultColorsSet` arm). Resolves
    /// the wire ambiguity `default_colors_set` alone cannot: nvim sends
    /// `rgb_bg = 0` both when `Normal` has no background at all and when a
    /// colorscheme genuinely sets `guibg = #000000`, and a probe reply's
    /// `fg`/`bg` map key presence disambiguates the two. Fire-and-forget
    /// like every other `RpcCall`: the reply crosses back as
    /// `Msg::HlProbeReply` through the same dispatch seam other
    /// engine-originated traffic uses, never by blocking the caller that
    /// emitted this effect.
    GetDefaultHl {
        generation: u64,
    },
    /// Reads what this engine recovered on the user's behalf while starting,
    /// and answers with the reading as [`Msg::SwapRecovered`], tagged with
    /// `generation`.
    ///
    /// Issued off nvim's own `VimEnter` rather than off the attach that
    /// preceded it: nvim opens the files it was given after config sourcing,
    /// so the reading is not final until `VimEnter` fires, and one taken at
    /// attach time races the recovery it is asking about.
    ///
    /// Fire-and-forget like every other `RpcCall`: the reading crosses back
    /// through the same dispatch seam other engine-originated traffic uses,
    /// never by blocking the caller that emitted this effect.
    ProbeSwapRecovery {
        generation: u64,
    },
    /// Asks nvim to clear and redraw its screen, which is also how it is
    /// told that the messages it has shown are over.
    ///
    /// The retraction is the point, not the repaint. With `ext_messages`
    /// attached nvim puts no message in the grid, so anything it reported --
    /// a swap-recovery report a user never asked for, most of all -- is
    /// view's own overlay until nvim retracts it with a `msg_clear`. Which
    /// ex command actually produces one is `view-engine`'s to know and its
    /// `redraw_live.rs` to pin; the two candidates do not behave alike.
    ///
    /// Not a substitute for damage-driven painting, and never issued per
    /// frame: a full repaint discards every reuse the grid's own damage
    /// tracking earns, so this belongs only where view knows the screen is
    /// showing something the user did not ask for.
    Redraw,
    /// Registers `specs` as real nvim mappings and the `:View` command, in
    /// one chunk, and answers with every claim as [`Msg::MappingsClaimed`].
    /// `channel_id` is view's own RPC channel: the registered right-hand
    /// side notifies back over it, so a key press reaches `update()` the
    /// same way any other engine-originated traffic does.
    ///
    /// Issued after `VimEnter` so `<leader>` expands to the user's
    /// `mapleader` rather than nvim's default, and carrying only the specs
    /// of features the session actually enabled: a disabled feature
    /// contributes nothing here, which is what leaves the user's own mapping
    /// on that key untouched.
    ///
    /// The `:View` command registers regardless of any enabled bit, so
    /// turning the default keys off never removes the way in.
    RegisterMappings {
        specs: Vec<MappingSpec>,
        channel_id: u64,
    },
    /// Registers the one autocmd group through which nvim tells view about
    /// state no UI event reports: a colorscheme switch, a diagnostic
    /// update, and the buffer/directory changes a branch indicator follows.
    /// Every callback notifies back over `channel_id`, view's own RPC
    /// channel.
    ///
    /// One group with several triggers rather than one registration per
    /// consumer: the registration is what a restarted engine loses, and
    /// three registrations are three chances for one of them to be missed
    /// and for its consumer to go quietly stale while the other two keep
    /// working. Cheap to keep whole -- the group clears itself on
    /// re-registration, so issuing it again is idempotent.
    ///
    /// Must be issued BEFORE `nvim_ui_attach` returns. nvim cannot begin
    /// sourcing the user's config until attach completes, and a config that
    /// sets a colorscheme fires `ColorScheme` while sourcing; a
    /// registration made afterwards misses that first switch entirely.
    RegisterBridge {
        channel_id: u64,
    },
    /// Injects view's `g:clipboard` provider, conditionally: the chunk (see
    /// `view-engine`'s `REGISTER_CLIPBOARD_CHUNK`) checks `vim.g.clipboard`
    /// and only installs view's dict when the user's config left it unset,
    /// so a user's own clipboard tool always wins. Issued after config
    /// sourcing has already run, which is exactly
    /// when the precedence check needs to happen -- unlike `RegisterBridge`,
    /// this cannot run ahead of `ui_attach`, because the fact it depends on
    /// (whether the user set `g:clipboard`) does not exist yet at that
    /// point. `channel_id` is view's own RPC channel, the same one every
    /// other registration here uses to target callbacks back at this
    /// connection.
    RegisterClipboard {
        channel_id: u64,
    },
    /// Enumerates listed, loaded buffers for `Source::Buffers`, tagged
    /// `generation` (`PickerState::generation` at the moment `update()`
    /// emitted this). Async like `GetDefaultHl`: the reply decodes on the
    /// reader thread and routes back as `Msg::PickerBufferList`. See
    /// `docs/picker-buffer-list-wire-capture.md` for the exact
    /// `nvim_exec_lua` chunk and its `buflisted`-filtered, error-degrades-
    /// to-empty contract.
    ListBuffers {
        generation: u64,
    },
    /// Looks up `path`'s content through any loaded nvim buffer, for the
    /// picker preview pane, tagged `generation` (`PickerState::generation`
    /// at the moment `update()` emitted this). Async like `ListBuffers`:
    /// the reply decodes on the reader thread and routes back as
    /// `Msg::PickerPreviewReply`. See `docs/picker-preview-wire-capture.md`
    /// for the exact `nvim_exec_lua` chunk and its `loaded`-flagged,
    /// error-degrades-to-`loaded: false` contract -- nvim owns all buffer
    /// text, so a modified-but-unsaved buffer must answer with its
    /// modified content, never the still-unmodified file on disk.
    PreviewBuffer {
        path: String,
        generation: u64,
    },
    /// Opens `path` as nvim would for `:edit`: an existing buffer for it is
    /// reused rather than duplicated, and a path with no buffer yet gets
    /// one, either way leaving nvim as the sole owner of the resulting
    /// buffer's identity and text. Fire-and-forget: the tree overlay closes
    /// on the same keypress that issues this, so nothing here needs a
    /// reply to act on.
    OpenFile {
        path: String,
    },
    /// Renames the file at `old_path` to `new_path` and, when a buffer is
    /// open for `old_path`, retargets that buffer onto the new path in the
    /// same call rather than leaving it pointing at a path that no longer
    /// exists. Tagged `generation` (`TreeState::generation` at the moment
    /// `update()` emitted this), answered by `Msg::TreeRenameReply`.
    ///
    /// Renaming a file with an open, modified buffer is exactly the case
    /// that must not orphan it: a plain `std::fs::rename` off the loop
    /// (rejected) moves the file nvim's in-memory buffer still names the
    /// old, now-nonexistent path under, so the next `:w` from that buffer
    /// would recreate the file at the old path instead of saving to the
    /// new one, silently splitting the file in two. See
    /// `docs/tree-rename-wire-capture.md` for the live capture proving
    /// `nvim_buf_set_name` retargets the buffer while preserving its
    /// modified flag and unsaved content verbatim, and for why the rename
    /// chunk itself refuses to overwrite an existing destination rather
    /// than silently replacing it (`vim.fn.rename`'s own behavior,
    /// confirmed live, if left unguarded).
    RenameFile {
        old_path: String,
        new_path: String,
        generation: u64,
    },
    /// Asks nvim for a new file's name: a blocked `vim.fn.input()`, primed
    /// with a `kind = "confirm"` `nvim_echo` so it arrives on the wire as
    /// the same `msg_show`/`cmdline_show` pair every other confirm-class
    /// prompt does (see `docs/tree-input-prompt-wire-capture.md`) and is
    /// picked up by the existing `PromptState` overlay machinery without any
    /// change to how a prompt is opened or painted -- only its answer routes
    /// somewhere new (`Msg::TreeCreatePromptReply`, via
    /// [`Waiter::CreatePrompt`](crate) in `view-engine`, rather than back
    /// into the engine as a keystroke). Async by construction, like
    /// `RenameFile`: issuing this never blocks the paint loop, however long
    /// the user takes to answer.
    TreeCreatePrompt {
        generation: u64,
    },
    /// Asks nvim for a rename target, on the same wire shape and async
    /// terms as [`RpcCall::TreeCreatePrompt`], pre-filled with
    /// `current_name` (`vim.fn.input`'s own `default` field) so renaming is
    /// an edit of the existing name rather than typing it from scratch.
    /// `old_path` is not sent over the wire at all -- it names nothing
    /// `vim.fn.input` needs -- but is carried on this call anyway so the
    /// executor has it to echo back once the reply arrives
    /// (`Msg::TreeRenamePromptReply::old_path`): the tree's selection cannot
    /// move while this prompt holds focus, but the reply itself carries only
    /// the typed name, with no path of its own for `update()` to resolve the
    /// rename against. Answered by `Msg::TreeRenamePromptReply`.
    TreeRenamePrompt {
        generation: u64,
        old_path: String,
        current_name: String,
    },
    /// Asks nvim to confirm deleting `path` via `vim.fn.confirm(prompt,
    /// "&Yes\n&No")`, reusing the already-proven `Answer::Choices` parsing
    /// path `PromptState` handles for `:confirm()` and the swapfile
    /// ATTENTION dialog -- no new prompt-state code needed for this one, only
    /// the async plumbing to route its choice back as
    /// `Msg::TreeDeleteConfirmReply`. Async on the same terms as
    /// `TreeCreatePrompt`.
    ///
    /// Before it ever offers that prompt, the engine-side chunk checks
    /// whether `path` (canonicalized, the same symlink-safe comparison
    /// `RENAME_CHUNK` uses) names a loaded buffer, and refuses outright
    /// (`DeleteConfirmOutcome::BufferOpen`) rather than asking a question
    /// whose "Yes" would delete a file nvim still owns a buffer for: buffer
    /// state lives in the engine, not here, so the check belongs where the
    /// state does.
    TreeDeleteConfirm {
        generation: u64,
        path: String,
    },
}
