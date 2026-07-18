//! The runtime loop's message and effect vocabulary. `Msg` is what reaches
//! [`crate::update::update`]; `Effect` is everything it can ask the loop to
//! carry out. The loop's exact call site:
//!
//! ```ignore
//! let msg = match msg_rx.recv() {
//!     Ok(Msg::RedrawReady) => Msg::Redraw(pump.take_damage()),
//!     Ok(Msg::EngineStopped) => Msg::EngineDown(engine.wait_exit()),
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
    EngineStopped,
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

/// A closed vocabulary of RPC calls instead of `(method, Vec<Value>)`: core
/// stays rmpv-free and an unencodable call is unrepresentable. Runner-up
/// (stringly method + opaque params) rejected: re-opens the door to core
/// building wire values.
#[non_exhaustive]
#[derive(Debug, Clone)]
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
}
