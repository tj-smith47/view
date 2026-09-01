//! Turning the notifications and replies a connection carries back into
//! the `view-core` types the runtime loop routes.
//!
//! Split out of `handle.rs` rather than living beside the reader thread
//! that calls them: driving the connection is one concern, and decoding
//! what comes back off it is another. Nothing here touches the connection,
//! blocks, or issues a request -- every function is total over what the
//! wire can carry, degrading to the safe answer its own doc names rather
//! than raising, on the same two conventions `nvim_api::decode` states.

use rmpv::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use view_core::msg::{DeleteConfirmOutcome, EngineRequest, Msg, RegisterType, ReplyToken};
use view_core::native::mappings::MappingClaim;
use view_core::native::surfaces::{FloatAnchor, FloatSighting};

use super::{saturate_u32, AttachedBuf};

/// Decodes a `view_invoke` notification's `(feature, verb)` positional
/// params into [`Msg::FeatureInvoke`], or `None` when the notification does
/// not carry that pair.
///
/// The pair is not validated here: nvim is where a user types `:View`
/// followed by any two words, so deciding an unknown pair is not actionable
/// belongs to the one arm that knows what this build can act on, not to the
/// reader thread.
pub(super) fn decode_feature_invoke(params: &[Value]) -> Option<Msg> {
    let [feature, verb, ..] = params else {
        return None;
    };
    Some(Msg::FeatureInvoke {
        feature: feature.as_str()?.to_owned(),
        verb: verb.as_str()?.to_owned(),
    })
}

/// Decodes a `"+p`/`"*p` paste's `(register)` positional param into the
/// message the loop routes to `update()`. `register` must decode to exactly
/// one `char` (`'+'` or `'*'`); anything else falls through to the reader
/// thread's generic "method not supported" response rather than guessing a
/// register, which would silently answer a paste from the wrong clipboard.
pub(super) fn decode_clipboard_get(token: ReplyToken, params: &[Value]) -> Option<Msg> {
    let [register, ..] = params else {
        return None;
    };
    let register = register.as_str()?.chars().next()?;
    Some(Msg::EngineRequest(EngineRequest::ClipboardGet {
        token,
        register,
    }))
}

/// Decodes a `"+yy`/`"*yy` copy's `(register, lines, regtype)` positional
/// params -- `regtype` is the injected `copy` closure's second argument,
/// forwarded by [`REGISTER_CLIPBOARD_CHUNK`] alongside `lines`. `lines`
/// must decode to an array of strings in full -- one undecodable line
/// drops the whole request rather than silently copying a truncated
/// selection. `regtype` must be present and a string, or the request drops
/// the same way, but an unrecognized string (a blockwise regtype this
/// provider does not keep, see [`RegisterType`]) degrades to
/// [`RegisterType::Charwise`] rather than dropping the request: the copy
/// itself is never in question, only which trailing-newline convention
/// its text gets.
pub(super) fn decode_clipboard_set(token: ReplyToken, params: &[Value]) -> Option<Msg> {
    let [register, lines, regtype, ..] = params else {
        return None;
    };
    let register = register.as_str()?.chars().next()?;
    let lines = lines
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()?;
    let regtype = RegisterType::from_nvim(regtype.as_str()?);
    Some(Msg::EngineRequest(EngineRequest::ClipboardSet {
        token,
        register,
        lines,
        regtype,
    }))
}

/// Decodes a `view_bridge` notification's `(event, ...payload)` positional
/// params into the message its consumer reads, or `None` when this build
/// has no consumer for the event. Each event names its own payload shape
/// (see [`crate::nvim_api::REGISTER_BRIDGE_CHUNK`]'s doc): `colorscheme`
/// carries the scheme's name alone, `diagnostics` an `(errors, warnings)`
/// count pair, `git` the branch name alone, `buffer` a `(name, modified)`
/// pair, and `float` the eleven positional fields
/// [`decode_float_observed`] names.
///
/// The bridge deliberately carries more triggers than there are consumers
/// today: the group is registered once, and adding a consumer must not mean
/// re-registering autocommands on a running engine. An event with no
/// consumer costs one dropped notification here.
pub(super) fn decode_bridge_event(params: &[Value]) -> Option<Msg> {
    let [event, first, rest @ ..] = params else {
        return None;
    };
    match event.as_str()? {
        "colorscheme" => Some(Msg::ColorSchemeChanged {
            name: first.as_str().unwrap_or_default().to_owned(),
        }),
        "diagnostics" => {
            let errors = saturate_u32(first.as_u64()?);
            let warnings = saturate_u32(rest.first()?.as_u64()?);
            Some(Msg::DiagnosticsChanged { errors, warnings })
        }
        "git" => Some(Msg::GitBranchChanged {
            branch: first.as_str().unwrap_or_default().to_owned(),
        }),
        "buffer" => {
            let name = first.as_str()?.to_owned();
            let modified = rest.first()?.as_bool()?;
            Some(Msg::BufferChanged { name, modified })
        }
        "float" => decode_float_observed(params),
        _ => None,
    }
}

/// Decodes the float watcher's `('float', win, buf, row, col, width,
/// height, zindex, filetype, name, anchor)` params into
/// [`Msg::FloatObserved`], or `None` for a shape the chunk does not
/// produce.
///
/// Read off `params` whole rather than off the `(first, rest)` split its
/// caller already made: eleven positional fields destructured in one
/// pattern is the shape a reviewer can check against the `rpcnotify` call
/// in [`crate::nvim_api::REGISTER_BRIDGE_CHUNK`] argument for argument.
///
/// `row`/`col` arrive through [`wire_i64`] because they are `Float` in
/// nvim's own window-config API and may be negative (a float placed partly
/// off-grid); `width`/`height` are counts and saturate. `zindex` saturates
/// into `u16`, which is wider than the 1001 the highest observed float
/// carries. A missing `anchor` is impossible from this chunk (it defaults
/// the field in Lua) and still degrades to nvim's own `NW` default rather
/// than dropping the sighting.
fn decode_float_observed(params: &[Value]) -> Option<Msg> {
    let [_, win, buf, row, col, width, height, zindex, filetype, name, anchor] = params else {
        return None;
    };
    Some(Msg::FloatObserved(FloatSighting {
        win: win.as_u64()?,
        buf: buf.as_u64()?,
        row: wire_i64(row)?,
        col: wire_i64(col)?,
        width: saturate_u16(wire_i64(width)?),
        height: saturate_u16(wire_i64(height)?),
        anchor: FloatAnchor::from_wire(anchor.as_str().unwrap_or_default()),
        zindex: saturate_u16(wire_i64(zindex)?),
        filetype: filetype.as_str().unwrap_or_default().to_owned(),
        name: name.as_str().unwrap_or_default().to_owned(),
    }))
}

/// A wire number as `i64`, whether it arrived as an integer or a float.
///
/// nvim's msgpack encoder emits an integral Lua number as an Integer, so
/// the second arm answers only a value that really did carry a fraction --
/// which `nvim_win_get_config`'s `Float`-typed `row`/`col` legitimately
/// can. Truncating toward zero there rather than refusing: half a cell of
/// offset is not a reason to drop a sighting, and the `as` conversion
/// saturates at the bounds rather than wrapping.
fn wire_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|number| number as i64))
}

/// Saturates a wire `i64` into the `u16` a cell count or a `zindex` is,
/// clamping at both ends rather than wrapping: a negative width is not a
/// huge one.
fn saturate_u16(value: i64) -> u16 {
    u16::try_from(value).unwrap_or(if value < 0 { 0 } else { u16::MAX })
}

/// Decodes one `nvim_buf_lines_event` notification's `(buf, changedtick,
/// firstline, lastline, linedata, more)` positional params (see
/// `docs/nvim-buf-attach-wire-capture.md` capture #2) into
/// `Msg::BufTextChanged`, or `None` when `buf` names a buffer this
/// connection has no attach entry recorded for -- a stray event racing a
/// detach (`buf_detach` already removed the entry before nvim's own
/// confirmation arrives) rather than a wire-shape decode failure, so it is
/// dropped the same way every other stale-generation reply in this crate
/// is, not treated as malformed.
///
/// A decode failure on a buffer that DOES have an entry (an unexpected
/// shape -- e.g. `lastline` arriving as `-1`, which `.as_u64()` refuses --
/// not merely an absent one) still returns `None` for this event, but marks
/// the entry desynced first: this connection cannot reconstruct what that
/// event said, and the next event it does manage to decode for this same
/// buffer must own up to the gap rather than fold in atop state that skipped
/// an edit. See `AttachedBuf::desynced`'s own doc for the full contract this
/// implements, and the reader thread's own notification-routing match for
/// the other place (a full sink's dropped `try_send`) that also marks it.
///
/// `more` (the trailing element) is read positionally but never carried
/// onto `Msg::BufTextChanged`: every capture in the wire-capture doc
/// produced `more: false` for a single `nvim_buf_set_lines` call, and this
/// crate has no batching state to fold a `true` into yet -- a future
/// consumer that needs it can add the field without changing this
/// function's decode shape.
pub(super) fn decode_buf_lines_event(
    params: &[Value],
    attached: &Mutex<HashMap<u64, AttachedBuf>>,
) -> Option<Msg> {
    let [buf, changedtick, firstline, lastline, linedata, ..] = params else {
        return None;
    };
    let buf = crate::ui_events::decode_ext_handle(buf)?;
    let body = (|| -> Option<(u64, u64, Vec<String>, u64)> {
        Some((
            firstline.as_u64()?,
            lastline.as_u64()?,
            linedata
                .as_array()?
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<Vec<String>>>()?,
            changedtick.as_u64()?,
        ))
    })();
    let mut guard = attached.lock().ok()?;
    let entry = guard.get_mut(&buf)?;
    match body {
        Some((firstline, lastline, linedata, changedtick)) => {
            let desynced = std::mem::replace(&mut entry.desynced, false);
            Some(Msg::BufTextChanged {
                buf: view_core::msg::BufferHandle(buf),
                generation: entry.generation,
                firstline,
                lastline,
                linedata,
                changedtick,
                desynced,
            })
        }
        None => {
            entry.desynced = true;
            None
        }
    }
}

/// Decodes a mapping registration's reply: an array of `{feature, lhs,
/// had_user_mapping}` rows, one per key the chunk registered, in
/// registration order.
///
/// A row missing `feature` or `lhs` is dropped rather than reported as a
/// claim naming nothing, and a missing `had_user_mapping` reads as `false`:
/// the flag is what promotes a claim to news, so an undecodable one must not
/// invent an announcement about a user's key.
pub(super) fn decode_mapping_claims(result: &Value) -> Vec<MappingClaim> {
    let Some(rows) = result.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let pairs = row.as_map()?;
            Some(MappingClaim {
                feature: crate::wire::map_find(pairs, "feature")?
                    .as_str()?
                    .to_owned(),
                lhs: crate::wire::map_find(pairs, "lhs")?.as_str()?.to_owned(),
                had_user_mapping: crate::wire::map_find(pairs, "had_user_mapping")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Decodes a buffer-list reply into each listed buffer's `name`, dropping
/// `bufnr`/`modified` (the picker's `Source::Buffers` corpus is a plain path
/// list; nothing here orders or annotates by either field -- see
/// `docs/picker-buffer-list-wire-capture.md`). A row missing `name` is
/// dropped: an unnamed scratch buffer still round-trips, since nvim replies
/// `name = ""` for one rather than omitting the key (capture #1), so a
/// missing key here can only mean a row shape this crate has never actually
/// seen from the pinned engine.
pub(super) fn decode_buffer_list_reply(result: &Value) -> Vec<String> {
    let Some(rows) = result.as_array() else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let pairs = row.as_map()?;
            Some(crate::wire::map_find(pairs, "name")?.as_str()?.to_owned())
        })
        .collect()
}

/// What one connection answered about the recovery it performed while
/// starting, as decoded from [`SWAP_RECOVERY_PROBE`].
///
/// [`SWAP_RECOVERY_PROBE`]: crate::process::SWAP_RECOVERY_PROBE
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct SwapRecoveryReading {
    /// Buffers that came back holding work the file on disk does not have.
    pub(super) count: u64,
    /// Whether the engine wrote a recovery report on screen at all.
    pub(super) reported: bool,
    /// The engine's own error text when the recovery it was asked for could
    /// not be performed, `None` when it went through.
    pub(super) failure: Option<String>,
    /// Whether the buffer this connection came up holding is empty -- read,
    /// not inferred from the error, because a failed recovery is not one
    /// shape.
    pub(super) empty: bool,
}

/// Decodes [`SWAP_RECOVERY_PROBE`]'s four-element answer.
///
/// A shape this crate has never seen from the pinned engine degrades to the
/// default -- the same "absent or malformed is exactly as informative as an
/// explicit nothing" precedent [`decode_hl_probe_reply`] follows, and the
/// conservative direction for all four: no notice claiming a recovery that
/// was not read, no redraw over a report that may not be there, no failure
/// attributed to an engine that did not report one, and no claim about a
/// buffer nobody looked in.
///
/// [`SWAP_RECOVERY_PROBE`]: crate::process::SWAP_RECOVERY_PROBE
pub(super) fn decode_swap_recovery_reply(result: &Value) -> SwapRecoveryReading {
    let Some(fields) = result.as_array() else {
        return SwapRecoveryReading::default();
    };
    SwapRecoveryReading {
        count: fields.first().and_then(Value::as_u64).unwrap_or(0),
        // vimscript has no boolean type: `||` and a comparison both answer
        // with the numbers 0 and 1, which is what arrives here
        reported: fields.get(1).and_then(Value::as_u64).unwrap_or(0) != 0,
        failure: fields
            .get(2)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned),
        empty: fields.get(3).and_then(Value::as_u64).unwrap_or(0) != 0,
    }
}

/// Decodes a picker-preview reply's `loaded`/`lines` keys, live-verified
/// against a real `nvim --clean --headless` (see
/// `docs/picker-preview-wire-capture.md`): `loaded: false` carries no
/// `lines` key at all, so `lines` is read only once `loaded` is confirmed
/// `true`, and a non-map/malformed `result` this crate has not actually seen
/// from the pinned engine degrades to `(false, [])` -- the same "absent or
/// malformed is exactly as informative as an explicit false" precedent
/// `decode_hl_probe_reply` follows.
pub(super) fn decode_preview_reply(result: &Value) -> (bool, Vec<String>) {
    let Some(pairs) = result.as_map() else {
        return (false, Vec::new());
    };
    let loaded = crate::wire::map_find(pairs, "loaded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !loaded {
        return (false, Vec::new());
    }
    let lines = crate::wire::map_find(pairs, "lines")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    (true, lines)
}

/// Decodes a rename reply's `ok` key, live-verified against a real `nvim
/// --clean --headless` (see `docs/tree-rename-wire-capture.md`): the chunk
/// returns `{ ok = true }` on a successful `vim.fn.rename` plus buffer
/// retarget, and `{ ok = false }` when it refused to overwrite an existing
/// destination. A non-map/malformed `result` this crate has not actually
/// seen from the pinned engine degrades to `false` -- the same "absent or
/// malformed is exactly as informative as an explicit false" precedent
/// `decode_hl_probe_reply` follows, and the safe reading here besides: a
/// rename this decoder cannot confirm succeeded must not trigger the rescan
/// that assumes it did.
pub(super) fn decode_rename_reply(result: &Value) -> bool {
    result
        .as_map()
        .and_then(|pairs| crate::wire::map_find(pairs, "ok"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Decodes a tree create/rename prompt's reply, live-verified against a real
/// `nvim --clean --headless` (see `docs/tree-input-prompt-wire-capture.md`):
/// `vim.fn.input` replies with a bare string, not a map -- an empty string
/// both on an outright `<Esc>` cancel and on the user submitting a blank
/// answer, neither of which names a file to create or rename to, so both
/// degrade to `None` the same way a non-string `result` this crate has not
/// actually seen from the pinned engine does.
pub(super) fn decode_prompt_reply(result: &Value) -> Option<String> {
    result.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Decodes a tree delete confirmation's reply, live-verified against a real
/// `nvim --clean --headless` (see `docs/tree-input-prompt-wire-capture.md`):
/// `TREE_DELETE_CONFIRM_CHUNK` returns `{ buffer_open = true }` when it
/// refused to even offer the prompt, or otherwise `{ choice = N }`, `N`
/// being `vim.fn.confirm`'s own 1-based button index per `:help confirm()`
/// -- `1` for the first (`&Yes`), `2` for the second (`&No`), and `0` when
/// the dialog was force-closed (e.g. `<C-c>`) without a choice. Only an
/// explicit `choice = 1` confirms; every other shape, including a
/// non-table `result` this crate has not actually seen from the pinned
/// engine, degrades to `Declined` -- the same "absent or malformed is
/// exactly as informative as an explicit false" precedent `decode_rename_reply`
/// follows, and the safe reading here besides: a delete this decoder cannot
/// confirm the user chose must not happen.
pub(super) fn decode_delete_confirm_reply(result: &Value) -> DeleteConfirmOutcome {
    let Some(map) = result.as_map() else {
        return DeleteConfirmOutcome::Declined;
    };
    if crate::wire::map_find(map, "buffer_open").and_then(Value::as_bool) == Some(true) {
        return DeleteConfirmOutcome::BufferOpen;
    }
    if crate::wire::map_find(map, "choice").and_then(Value::as_i64) == Some(1) {
        return DeleteConfirmOutcome::Confirmed;
    }
    DeleteConfirmOutcome::Declined
}

/// Decodes an `nvim_get_hl(0, {name = "Normal"})` reply's `fg`/`bg` map
/// keys, live-verified against a real `nvim --embed`: a transparent
/// `Normal` (`hi Normal guifg=#f8f8f2`, no `guibg`) replies `{fg =
/// 16316658}` with no `bg` key at all; an explicit background (`hi Normal
/// guibg=#282a36`) replies `{fg = 16316658, bg = 2632246}` with both
/// present. A key's absence, not a sentinel value, is what disambiguates
/// "unset" from "genuinely zero" -- the exact ambiguity `default_colors_set`
/// alone cannot resolve (see [`view_core::msg::RpcCall::GetDefaultHl`]).
/// `result` shapes this crate has not seen from a real `nvim_get_hl`
/// (non-map, or present keys of an unexpected wire type) degrade to `None`
/// for that channel rather than erroring: a malformed reply is exactly as
/// informative as an absent key for this probe's purposes.
pub(super) fn decode_hl_probe_reply(result: &Value) -> (Option<u32>, Option<u32>) {
    let Some(map) = result.as_map() else {
        return (None, None);
    };
    let get = |key: &str| {
        crate::wire::map_find(map, key)
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
    };
    (get("fg"), get("bg"))
}
