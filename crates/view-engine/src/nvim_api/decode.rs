//! Decoding nvim's replies into the types this crate hands on.
//!
//! Every function here is total over what the wire can carry: a reply shape
//! the pinned engine has never produced degrades to the safe answer its own
//! doc names rather than raising, and the two conventions in play (a corpus
//! that can legitimately be empty degrades to empty; a contract the chunk
//! always satisfies surfaces as `Err`) are stated per decoder.
//!
//! Split out of `nvim_api` rather than living beside the chunks they read:
//! the chunk texts and the handle methods that issue them are one concern,
//! and turning the answers back into `view-core` types is another. Nothing
//! here builds a request or touches the connection.

use rmpv::Value;
use std::path::PathBuf;
use view_core::msg::{CheckTimeOutcome, Msg, OptionValue};
use view_core::native::ai_context::{
    CurrentBufferRead, CursorRead, DiagnosticEntry, DiagnosticSeverity, QuickfixEntry,
    SelectionRead,
};
use view_core::native::ai_event::FsError;

use crate::handle::{saturate_u32, EngineError};
use crate::nvim_api::BufWriteOutcome;
use crate::rpc::RpcError;

/// Maps one [`OptionValue`] onto the msgpack value nvim's option API takes.
///
/// Total by construction, and deliberately so: `OptionValue` is closed over
/// nvim's three option types, so a new variant must break this match rather
/// than fall through to a default that would set an option to something
/// nvim never asked for.
pub(super) fn option_value(value: &OptionValue) -> Value {
    match value {
        OptionValue::Int(n) => Value::from(*n),
        OptionValue::Bool(b) => Value::from(*b),
        OptionValue::Str(s) => Value::from(s.as_str()),
    }
}

/// Renders an `nvim_eval` result as plain text for [`EngineHandle::eval_str`](crate::EngineHandle::eval_str).
///
/// `Value`'s own `Display` impl is unsuitable: `rmpv::Utf8String::fmt`
/// formats through `Debug`, so a vimscript string result like `getline(1)`'s
/// `"hello"` would round-trip as the quoted literal `"\"hello\""` rather
/// than the bare `hello` a text-comparison oracle needs (`s.as_str()`
/// returning `None`, an ill-formed UTF-8 string on the wire, falls back to
/// a lossy conversion rather than silently dropping the reply). `Array`/
/// `Map`/`Binary`/`Ext` results (no probe this crate exposes evaluates to
/// one today) fall through to `Value`'s own `Display` rendering, which is
/// still total -- just not this function's primary concern.
pub(super) fn value_to_string(value: &Value) -> String {
    match value {
        Value::Nil => "nil".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::String(s) => s.as_str().map_or_else(
            || String::from_utf8_lossy(s.as_bytes()).into_owned(),
            str::to_string,
        ),
        Value::Integer(i) => i.to_string(),
        Value::F32(f) => f.to_string(),
        Value::F64(f) => f.to_string(),
        other => other.to_string(),
    }
}

/// Decodes [`super::BUF_SET_TEXT_CHUNK`]'s reply. Anything but an explicit
/// `applied = false` reads as applied: the chunk answers that key on every
/// path it takes, so a reply without it can only be a shape this crate has
/// never seen from the pinned engine, and treating an applied write as
/// refused would put an accepted hunk back on screen as undecided.
pub(super) fn decode_buf_set_text_reply(reply: &Value) -> BufWriteOutcome {
    let applied = reply
        .as_map()
        .and_then(|pairs| crate::wire::map_find(pairs, "applied"))
        .and_then(Value::as_bool);
    if applied == Some(false) {
        return BufWriteOutcome::BufferAdvanced;
    }
    let changedtick = reply
        .as_map()
        .and_then(|pairs| crate::wire::map_find(pairs, "changedtick"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    BufWriteOutcome::Applied { changedtick }
}

/// Decodes an [`super::AI_FS_READ_CHUNK`] or [`super::AI_FS_WRITE_CHUNK`] reply into the
/// message the reader thread routes back, correlated on `request_id`.
///
/// Every failure becomes an [`FsError`] rather than a dropped reply: an
/// agent's request is one this client owes an answer to, and a reply lost
/// here leaves the agent blocked forever on a call nothing will ever
/// settle. That is why an `nvim_exec_lua` error degrades to an answered
/// refusal here instead of to the "safe default" every generation-gated
/// reply beside it takes -- there is no later reply to correct a default
/// with.
///
/// A read's `ok = false` is [`FsError::NotFound`]: the chunk answers it for
/// a buffer handle that is no longer valid, which from the agent's side is
/// a path that named nothing readable. A write carries the chunk's own
/// wording instead, because its two refusals are different facts the agent
/// can act on differently -- a moved tick is worth retrying, `E212` is not.
pub(crate) fn decode_ai_fs_reply(
    request_id: u64,
    write: bool,
    error: &Value,
    result: &Value,
) -> Msg {
    if write {
        return Msg::AiFsWriteReply {
            request_id,
            result: decode_ai_fs_write(error, result),
        };
    }
    Msg::AiFsReadReply {
        request_id,
        result: decode_ai_fs_read(error, result),
    }
}

pub(super) fn decode_ai_fs_read(error: &Value, result: &Value) -> Result<String, FsError> {
    if let Some(failure) = remote_failure(error) {
        return Err(failure);
    }
    let pairs = result.as_map().ok_or(FsError::NotFound)?;
    if crate::wire::map_find(pairs, "ok").and_then(Value::as_bool) != Some(true) {
        return Err(FsError::NotFound);
    }
    let lines: Vec<&str> = crate::wire::map_find(pairs, "lines")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let eol = crate::wire::map_find(pairs, "eol")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut content = lines.join("\n");
    // The line list is the same for a file ending `"a\n"` and one ending
    // `"a"`; `eol` is the only thing that tells them apart, and appending
    // unconditionally would hand the agent a byte the file does not hold.
    // An empty window takes no terminator at all -- a lone "\n" would be a
    // line the agent did not ask for.
    if eol && !lines.is_empty() {
        content.push('\n');
    }
    Ok(content)
}

pub(super) fn decode_ai_fs_write(error: &Value, result: &Value) -> Result<(), FsError> {
    if let Some(failure) = remote_failure(error) {
        return Err(failure);
    }
    let pairs = result.as_map().ok_or_else(|| FsError::Other {
        message: "the write produced no answer".to_owned(),
    })?;
    let applied = crate::wire::map_find(pairs, "applied").and_then(Value::as_bool) == Some(true);
    let saved = crate::wire::map_find(pairs, "saved").and_then(Value::as_bool) == Some(true);
    if applied && saved {
        return Ok(());
    }
    Err(FsError::Other {
        message: crate::wire::map_find(pairs, "message")
            .and_then(Value::as_str)
            .unwrap_or("the write could not be carried out")
            .to_owned(),
    })
}

/// The chunk's own thrown error, as the refusal that crosses back, or
/// `None` when nvim answered without one.
pub(super) fn remote_failure(error: &Value) -> Option<FsError> {
    if *error == Value::Nil {
        return None;
    }
    let message = error
        .as_array()
        .and_then(|parts| parts.iter().find_map(Value::as_str))
        .unwrap_or("nvim refused the request")
        .to_owned();
    Some(FsError::Other { message })
}

/// Decodes one entry of [`super::CHECKTIME_CHUNK`]'s own `results` array into the
/// outcome it describes (`docs/checktime-wire-capture.md`'s outcome table).
///
/// `forced` is read from the entry itself rather than from what the caller
/// asked for: the force branch is the only one that reports it, so a
/// completed reload can never decode to the [`CheckTimeOutcome::Conflict`]
/// that would re-raise the prompt the user just answered.
///
/// `gone` is read ahead of `forced` because the chunk answers it from above
/// the force split, for the probe and the forced call alike -- a path that
/// is not a readable file is the same answer whoever asked.
pub(super) fn decode_checktime_entry(entry: &Value) -> CheckTimeOutcome {
    let pairs = entry.as_map().map_or(&[][..], Vec::as_slice);
    let flag =
        |name: &str| crate::wire::map_find(pairs, name).and_then(Value::as_bool) == Some(true);
    if !flag("found") {
        return CheckTimeOutcome::NoBuffer;
    }
    if flag("gone") {
        return CheckTimeOutcome::FileGone {
            modified: flag("modified"),
        };
    }
    if flag("forced") {
        return if flag("ok") {
            CheckTimeOutcome::Reloaded
        } else {
            CheckTimeOutcome::ReloadFailed
        };
    }
    if flag("fired") {
        CheckTimeOutcome::Conflict
    } else {
        CheckTimeOutcome::HandledSilently
    }
}

/// Decodes a [`super::CHECKTIME_CHUNK`] `Response` into `Msg::CheckTimeReply`.
/// `paths` are echoed back from the waiter rather than decoded from the
/// reply (the chunk's own answer is positional -- each path was already
/// resolved to a `bufnr` before nvim ever executed), the same reason
/// `Waiter::Preview` and `Waiter::LoadHidden` carry `path` themselves.
///
/// An entry the reply does not carry at all (a short array, or an `error`
/// reply with no array) degrades to [`CheckTimeOutcome::NoBuffer`] for a
/// probe -- every other async waiter's "safe default over a stuck
/// generation" precedent, since a probe that could not even ask must never
/// read as a genuine conflict -- but to [`CheckTimeOutcome::ReloadFailed`]
/// for a forced call, because there the user asked for something
/// destructive and silence would read as it having happened.
pub(crate) fn decode_checktime_reply(
    call: crate::handle::CheckTimeCall,
    error: &Value,
    result: &Value,
) -> Msg {
    let missing = if call.forced {
        CheckTimeOutcome::ReloadFailed
    } else {
        CheckTimeOutcome::NoBuffer
    };
    let entries = if *error == Value::Nil {
        result
            .as_map()
            .and_then(|pairs| crate::wire::map_find(pairs, "results"))
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
    } else {
        &[][..]
    };
    let results = call
        .paths
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let outcome = entries.get(i).map_or(missing, decode_checktime_entry);
            (path, outcome)
        })
        .collect();
    Msg::CheckTimeReply {
        request_id: call.request_id,
        results,
    }
}

/// Decodes [`super::CURRENT_BUFFER_TEXT_CHUNK`]'s `{path, text}` reply, live-
/// verified against a real `nvim --clean --headless` (see
/// `docs/ai-context-reads-wire-capture.md`). Unlike `decode_preview_reply`'s
/// "absent or malformed degrades to a safe default" convention, a malformed
/// reply here surfaces as `Err` rather than an empty `CurrentBufferRead`:
/// the chunk's own two keys are unconditional (nvim always has a current
/// buffer, even an unnamed scratch one), so a shape missing either is a
/// contract violation this crate has never actually seen from the pinned
/// engine, not an expected "nothing to read" case.
pub(super) fn decode_current_buffer_text_reply(
    result: &Value,
) -> Result<CurrentBufferRead, EngineError> {
    let malformed = || {
        EngineError::Rpc(RpcError::Malformed(format!(
            "current-buffer-text reply: {result}"
        )))
    };
    let pairs = result.as_map().ok_or_else(malformed)?;
    let path = crate::wire::map_find(pairs, "path")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    let text = crate::wire::map_find(pairs, "text")
        .and_then(Value::as_str)
        .ok_or_else(malformed)?;
    Ok(CurrentBufferRead::new(PathBuf::from(path), text.to_owned()))
}

/// Decodes [`super::CURSOR_CONTEXT_CHUNK`]'s `{line, col, selection_*}` reply,
/// live-verified against a real `nvim --clean --headless` (see
/// `docs/ai-context-reads-wire-capture.md`). `line`/`col` are unconditional
/// (nvim always has a cursor) and a shape missing either is malformed, the
/// same contract-violation reasoning
/// [`decode_current_buffer_text_reply`] documents. `col` crosses the wire
/// 0-indexed (`nvim_win_get_cursor`'s own convention); this decoder adds 1
/// so [`CursorRead::col`] carries the single 1-indexed convention every
/// `EngineReadSnapshot` position field shares (see that type's own doc).
/// `line` needs no such adjustment: `nvim_win_get_cursor`'s row is already
/// 1-indexed on the wire. The three `selection_*` keys are read together or
/// not at all: the chunk only ever writes all three or none, so a reply
/// carrying just one or two is treated as no active selection rather than a
/// partial one built from whichever keys happened to be present.
/// `selection_start`/`selection_end` need no adjustment either: they are
/// buffer line numbers, already 1-indexed the same way `line` is.
pub(super) fn decode_cursor_context_reply(
    result: &Value,
) -> Result<(CursorRead, Option<SelectionRead>), EngineError> {
    let malformed = || {
        EngineError::Rpc(RpcError::Malformed(format!(
            "cursor-context reply: {result}"
        )))
    };
    let pairs = result.as_map().ok_or_else(malformed)?;
    let line = crate::wire::map_find(pairs, "line")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    let col = crate::wire::map_find(pairs, "col")
        .and_then(Value::as_u64)
        .ok_or_else(malformed)?;
    let cursor = CursorRead::new(saturate_u32(line), saturate_u32(col).saturating_add(1));
    let selection = match (
        crate::wire::map_find(pairs, "selection_text").and_then(Value::as_str),
        crate::wire::map_find(pairs, "selection_start").and_then(Value::as_u64),
        crate::wire::map_find(pairs, "selection_end").and_then(Value::as_u64),
    ) {
        (Some(text), Some(start), Some(end)) => Some(SelectionRead::new(
            text.to_owned(),
            (saturate_u32(start), saturate_u32(end)),
        )),
        _ => None,
    };
    Ok((cursor, selection))
}

/// Decodes [`super::DIAGNOSTIC_ENTRIES_CHUNK`]'s reply, live-verified against a
/// real `nvim --clean --headless` (see
/// `docs/ai-context-reads-wire-capture.md`). A non-array `result` (a shape
/// this crate has never actually seen from the pinned engine, since the
/// chunk always returns a table) degrades to an empty list rather than an
/// `Err`, matching `decode_buffer_list_reply`'s convention for a corpus that
/// legitimately can be empty (no diagnostics currently posted) -- a row
/// missing any of its four fields is dropped rather than failing the whole
/// read. `line`/`col` cross the wire 0-indexed (`vim.diagnostic.get`'s own
/// convention); both get +1 here so [`DiagnosticEntry::line`]/`::col` carry
/// the same single 1-indexed convention [`decode_cursor_context_reply`]
/// normalizes onto (see `EngineReadSnapshot`'s own doc) -- `getqflist`'s
/// entries need no such adjustment, already 1-indexed on the wire (see
/// [`decode_quickfix_entries_reply`]). `severity` is
/// `vim.diagnostic.severity`'s own closed 1-4 range (`:help
/// diagnostic-severity`); an out-of-range value this crate has never seen
/// from the pinned engine drops the row rather than guessing a severity
/// nvim never reported.
pub(super) fn decode_diagnostic_entries_reply(
    result: &Value,
) -> Result<Vec<DiagnosticEntry>, EngineError> {
    let Some(rows) = result.as_array() else {
        return Ok(Vec::new());
    };
    let entries = rows
        .iter()
        .filter_map(|row| {
            let pairs = row.as_map()?;
            let line =
                saturate_u32(crate::wire::map_find(pairs, "line")?.as_u64()?).saturating_add(1);
            let col =
                saturate_u32(crate::wire::map_find(pairs, "col")?.as_u64()?).saturating_add(1);
            let severity = match crate::wire::map_find(pairs, "severity")?.as_u64()? {
                1 => DiagnosticSeverity::Error,
                2 => DiagnosticSeverity::Warning,
                3 => DiagnosticSeverity::Info,
                4 => DiagnosticSeverity::Hint,
                _ => return None,
            };
            let message = crate::wire::map_find(pairs, "message")?
                .as_str()?
                .to_owned();
            Some(DiagnosticEntry::new(line, col, severity, message))
        })
        .collect();
    Ok(entries)
}

/// Decodes [`super::QUICKFIX_ENTRIES_CHUNK`]'s reply, live-verified against a real
/// `nvim --clean --headless` (see `docs/ai-context-reads-wire-capture.md`),
/// on the same "non-array degrades to empty, a malformed row is dropped"
/// terms as [`decode_diagnostic_entries_reply`] -- an empty quickfix list is
/// the ordinary case, not an error. `line`/`col` need no index adjustment
/// here, unlike that decoder's: `getqflist()` is already 1-indexed on the
/// wire, the same convention every `EngineReadSnapshot` position field
/// shares.
pub(super) fn decode_quickfix_entries_reply(
    result: &Value,
) -> Result<Vec<QuickfixEntry>, EngineError> {
    let Some(rows) = result.as_array() else {
        return Ok(Vec::new());
    };
    let entries = rows
        .iter()
        .filter_map(|row| {
            let pairs = row.as_map()?;
            let path = crate::wire::map_find(pairs, "path")?.as_str()?.to_owned();
            let line = saturate_u32(crate::wire::map_find(pairs, "line")?.as_u64()?);
            let col = saturate_u32(crate::wire::map_find(pairs, "col")?.as_u64()?);
            let text = crate::wire::map_find(pairs, "text")?.as_str()?.to_owned();
            Some(QuickfixEntry::new(PathBuf::from(path), line, col, text))
        })
        .collect();
    Ok(entries)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Every outcome the wire can describe, decoded from the exact reply
    /// entries `docs/checktime-wire-capture.md` captured. The forced pair is
    /// the load-bearing half: a force branch that reported `fired` instead
    /// of `forced` would decode to `Conflict` and re-raise the prompt the
    /// user just answered. The `gone` pair carries no `forced` at all, since
    /// the stat that answers it sits above the force split -- reading it
    /// after `forced` would decode a probe's own `gone` as
    /// `HandledSilently` and swallow the notice.
    #[test]
    fn each_captured_reply_entry_decodes_to_its_outcome() {
        let entry = |pairs: &[(&str, bool)]| {
            Value::Map(
                pairs
                    .iter()
                    .map(|(k, v)| (Value::from(*k), Value::from(*v)))
                    .collect(),
            )
        };
        let cases = [
            (vec![("found", false)], CheckTimeOutcome::NoBuffer),
            (
                vec![("found", true), ("fired", false)],
                CheckTimeOutcome::HandledSilently,
            ),
            (
                vec![("found", true), ("fired", true)],
                CheckTimeOutcome::Conflict,
            ),
            (
                vec![("found", true), ("forced", true), ("ok", true)],
                CheckTimeOutcome::Reloaded,
            ),
            (
                vec![("found", true), ("forced", true), ("ok", false)],
                CheckTimeOutcome::ReloadFailed,
            ),
            (
                vec![("found", true), ("gone", true), ("modified", true)],
                CheckTimeOutcome::FileGone { modified: true },
            ),
            (
                vec![("found", true), ("gone", true), ("modified", false)],
                CheckTimeOutcome::FileGone { modified: false },
            ),
        ];
        for (pairs, expected) in cases {
            assert_eq!(
                decode_checktime_entry(&entry(&pairs)),
                expected,
                "{pairs:?}"
            );
        }
    }

    /// A reply that never arrived degrades in the direction that cannot
    /// mislead: silence for a probe, "the reload failed" for the forced
    /// call, because there the user already asked for something destructive
    /// and hearing nothing reads as it having happened.
    #[test]
    fn an_error_reply_degrades_by_which_call_it_answers() {
        let call = |forced| crate::handle::CheckTimeCall {
            request_id: 1,
            paths: vec![std::path::PathBuf::from("a.rs")],
            forced,
        };
        let outcome = |forced| {
            let Msg::CheckTimeReply { results, .. } =
                decode_checktime_reply(call(forced), &Value::from("nvim refused"), &Value::Nil)
            else {
                return None;
            };
            results.first().map(|(_, outcome)| *outcome)
        };
        assert_eq!(outcome(false), Some(CheckTimeOutcome::NoBuffer));
        assert_eq!(outcome(true), Some(CheckTimeOutcome::ReloadFailed));
    }

    #[test]
    fn every_option_value_kind_maps_to_its_own_msgpack_type() {
        assert_eq!(option_value(&OptionValue::Int(3)), Value::from(3));
        assert_eq!(option_value(&OptionValue::Bool(true)), Value::from(true));
        assert_eq!(
            option_value(&OptionValue::Str("%f".to_string())),
            Value::from("%f")
        );
    }
}
