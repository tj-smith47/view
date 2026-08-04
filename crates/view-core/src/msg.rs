//! The runtime loop's message and effect vocabulary. `Msg` is what reaches
//! [`crate::update::update`]; `Effect` is everything it can ask the loop to
//! carry out. The loop's exact call site:
//!
//! ```ignore
//! let msg = match msg_rx.recv() {
//!     Ok(Msg::RedrawReady) => Msg::Redraw(pump.take_damage()),
//!     Ok(Msg::EngineStopped(reason)) => {
//!         model.fatal_reason = reason;
//!         Msg::EngineDown(engine.wait_exit())
//!     }
//!     Ok(m) => m,
//!     Err(_) => Msg::EngineDown(ExitInfo { code: None, by_signal: false }),
//! };
//! for eff in update(&mut model, msg) { /* executor, never blocks */ }
//! ```
use crate::events::UiEvent;

/// Every input `update()` can react to.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Msg {
    /// Already-encoded nvim notation from the input thread.
    Key(Key),
    /// One compacted damage batch, drained by the loop.
    Redraw(Vec<UiEvent>),
    /// Pump token: damage staged; the loop MUST drain it into `Redraw`
    /// before `update()` sees it (raw = silent no-op).
    RedrawReady,
    /// Reader token: engine stream ended; the loop resolves `ExitInfo`.
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
    EngineStopped(Option<String>),
    /// Loop plumbing: startup's pre-attach key-buffering loop consumes this
    /// the moment the background attach thread finishes `nvim_ui_attach`,
    /// unblocking its `recv()` without a poll or a timer. Never reachable
    /// once the steady-state loop in `runtime::run` begins (the sender side
    /// only ever fires once, before that loop starts), but `update()` still
    /// carries a no-op arm for it, mirroring `RedrawReady`/`EngineStopped`'s
    /// contract.
    EngineReady,
    EngineDown(ExitInfo),
    EngineRequest(EngineRequest),
    Resized {
        width: u16,
        height: u16,
    },
    /// A terminal bracketed-paste payload, decoded by the input thread from
    /// crossterm's `Event::Paste`.
    Paste(String),
    /// A terminal mouse event, decoded by the input thread from crossterm's
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
}

/// One decoded mouse event in nvim `nvim_input_mouse` vocabulary: `button`
/// is one of `"left"`/`"right"`/`"middle"`/`"wheel"`/`"move"`; `action` is
/// `"press"`/`"drag"`/`"release"` for ordinary buttons, `"up"`/`"down"`/
/// `"left"`/`"right"` for the wheel, and ignored for `"move"`; `modifier` is
/// a string of single-char modifier prefixes (`"C-"`, `"S-"`, `"M-"`, in
/// that order, matching `view_tui::keys::encode_key`'s convention).
/// `row`/`col` are the raw terminal cell position the input thread
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
#[derive(Debug, Clone)]
pub struct ExitInfo {
    pub code: Option<i32>,
    /// Whether `code` came from a signal death rather than a normal exit
    /// status. Decoded for wire completeness; the process exit code the
    /// bin crate reports is computed from `code` alone, so this does not
    /// currently change any observable behavior.
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
    VimEnter { token: ReplyToken },
}

/// Identifies the pending msgpack-RPC request a reply must answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyToken {
    pub msgid: u64,
}

/// The value an `Effect::Reply` sends back to the engine.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum ReplyValue {
    Nil,
}

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
    Quit {
        exit_code: i32,
    },
}

/// The value side of [`RpcCall::SetOption`], spelled in Rust's own scalar
/// types so `view-core` stays free of `rmpv`: the crate that speaks the
/// wire maps each variant onto a msgpack value.
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
}
