//! Wire-shape conversions for `session/request_permission`, split out of
//! `driver.rs` so the request/reply value shapes live beside the pinned
//! strings that define them (`docs/acp-v1-wire-capture.md`'s
//! `PermissionOption` and `RequestPermissionOutcome` sections) instead of
//! mixed into the driver's own dispatch and correlation code.
//!
//! The round trip itself -- holding the JSON-RPC reply open until
//! `AiCommand::AnswerPermission` arrives, and the single-outstanding-request
//! degrade for a second request while one is still open -- lives in
//! `driver.rs` (`open_permissions`) and `view-core`'s
//! `update::ai::on_ai_event` (`AiPanelState::pending_permission`'s single
//! slot). Nothing here holds any of that state; these are pure value
//! mappings only.

use serde_json::{json, Value};
use view_core::native::ai_event::{PermissionOption, PermissionOptionKind, PermissionOutcome};

/// Decodes one wire `PermissionOption` object into the closed vocabulary, or
/// `None` for a malformed entry -- a missing required field, or a `kind`
/// string outside the four pinned in `docs/acp-v1-wire-capture.md`
/// (`allow_once`, `allow_always`, `reject_once`, `reject_always`). An option
/// this client cannot represent is left out of the presented list rather
/// than guessed at: offering a kind the agent never sent would let the user
/// pick something with no real backing `optionId`.
pub(crate) fn permission_option(raw: &Value) -> Option<PermissionOption> {
    let kind = match raw.get("kind").and_then(Value::as_str)? {
        "allow_once" => PermissionOptionKind::AllowOnce,
        "allow_always" => PermissionOptionKind::AllowAlways,
        "reject_once" => PermissionOptionKind::RejectOnce,
        "reject_always" => PermissionOptionKind::RejectAlways,
        _ => return None,
    };
    Some(PermissionOption {
        option_id: raw.get("optionId").and_then(Value::as_str)?.to_string(),
        name: raw.get("name").and_then(Value::as_str)?.to_string(),
        kind,
    })
}

/// Encodes the user's answer into the wire's `RequestPermissionOutcome`
/// shape, per `docs/acp-v1-wire-capture.md`'s pinned two variants: `{"outcome":
/// "cancelled"}` with no other fields, or `{"outcome": "selected", "optionId":
/// ...}`. `Cancelled` is also what answers a second, overlapping
/// `session/request_permission` (see this module's own doc) -- the schema
/// defines no third variant, so there is no separate "declined due to
/// overlap" shape to encode.
pub(crate) fn permission_outcome(outcome: &PermissionOutcome) -> Value {
    match outcome {
        PermissionOutcome::Cancelled => json!({ "outcome": "cancelled" }),
        PermissionOutcome::Selected { option_id } => {
            json!({ "outcome": "selected", "optionId": option_id })
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn every_pinned_kind_string_decodes() {
        for (wire, expected) in [
            ("allow_once", PermissionOptionKind::AllowOnce),
            ("allow_always", PermissionOptionKind::AllowAlways),
            ("reject_once", PermissionOptionKind::RejectOnce),
            ("reject_always", PermissionOptionKind::RejectAlways),
        ] {
            let option = permission_option(&json!({
                "optionId": "opt-1",
                "name": "Allow once",
                "kind": wire,
            }))
            .unwrap_or_else(|| panic!("{wire} must decode"));
            assert_eq!(option.kind, expected);
            assert_eq!(option.option_id, "opt-1");
            assert_eq!(option.name, "Allow once");
        }
    }

    #[test]
    fn an_unpinned_kind_string_decodes_to_none() {
        assert!(permission_option(&json!({
            "optionId": "opt-1",
            "name": "Allow once",
            "kind": "allow-once",
        }))
        .is_none());
    }

    #[test]
    fn a_missing_required_field_decodes_to_none() {
        assert!(
            permission_option(&json!({ "name": "Allow once", "kind": "allow_once" })).is_none()
        );
        assert!(permission_option(&json!({ "optionId": "opt-1", "kind": "allow_once" })).is_none());
        assert!(permission_option(&json!({ "optionId": "opt-1", "name": "Allow once" })).is_none());
    }

    #[test]
    fn cancelled_encodes_with_no_other_fields() {
        assert_eq!(
            permission_outcome(&PermissionOutcome::Cancelled),
            json!({ "outcome": "cancelled" })
        );
    }

    #[test]
    fn selected_encodes_with_the_chosen_option_id() {
        assert_eq!(
            permission_outcome(&PermissionOutcome::Selected {
                option_id: "opt-1".to_string(),
            }),
            json!({ "outcome": "selected", "optionId": "opt-1" })
        );
    }
}
