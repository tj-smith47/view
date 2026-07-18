//! Small decoding helpers shared across the wire-format decoders
//! (`process`'s `api_info` handshake, `ui_events`'s `redraw` payloads):
//! both walk msgpack maps keyed by string field name, and neither owns the
//! concept exclusively enough to host it as a method on the other.

use rmpv::Value;

/// Finds `key` in a decoded msgpack map's pairs, by string-equality on the
/// key `Value`. `O(n)` linear scan: these maps are wire-sized (single-digit
/// to low-double-digit field counts per nvim event), so a `HashMap` build
/// per call would cost more than it saves.
pub(crate) fn map_find<'a>(pairs: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    pairs
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}
