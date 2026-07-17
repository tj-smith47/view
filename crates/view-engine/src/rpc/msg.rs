use rmpv::Value;

/// Errors produced when decoding a msgpack `Value` into an `RpcMessage`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RpcError {
    /// The value did not match any valid msgpack-RPC message shape
    /// (wrong type, unknown kind tag, or wrong array arity for its kind).
    #[error("malformed rpc message: {0}")]
    Malformed(String),
}

/// A single msgpack-RPC message exchanged with the embedded nvim process.
///
/// Each variant maps to a tagged msgpack array on the wire: `Request` is
/// `[0, msgid, method, params]`, `Response` is `[1, msgid, error, result]`,
/// and `Notification` is `[2, method, params]`. See `to_value`/`from_value`
/// for the exact encode/decode rules.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RpcMessage {
    /// An outbound call awaiting a `Response` with the same `msgid`.
    Request {
        /// Correlates this request with its eventual `Response`.
        msgid: u32,
        /// The nvim API method name to invoke.
        method: String,
        /// Positional arguments for `method`.
        params: Vec<Value>,
    },
    /// The reply to a `Request` with a matching `msgid`.
    ///
    /// Follows the nvim RPC convention: `error` is non-nil on failure and
    /// `result` is non-nil on success — the two are mutually exclusive,
    /// never both non-nil.
    Response {
        /// The `msgid` of the `Request` this responds to.
        msgid: u32,
        /// Non-nil (`Value::Nil` otherwise) when the call failed.
        error: Value,
        /// Non-nil (`Value::Nil` otherwise) when the call succeeded.
        result: Value,
    },
    /// A fire-and-forget call with no `Response` expected.
    Notification {
        /// The nvim API method name to invoke.
        method: String,
        /// Positional arguments for `method`.
        params: Vec<Value>,
    },
}

impl RpcMessage {
    /// Encodes this message as its msgpack-RPC tagged array (see
    /// [`RpcMessage`] for the per-variant array shape).
    pub fn to_value(&self) -> Value {
        match self {
            Self::Request {
                msgid,
                method,
                params,
            } => Value::Array(vec![
                0.into(),
                (*msgid).into(),
                method.as_str().into(),
                Value::Array(params.clone()),
            ]),
            Self::Response {
                msgid,
                error,
                result,
            } => Value::Array(vec![
                1.into(),
                (*msgid).into(),
                error.clone(),
                result.clone(),
            ]),
            Self::Notification { method, params } => Value::Array(vec![
                2.into(),
                method.as_str().into(),
                Value::Array(params.clone()),
            ]),
        }
    }

    /// Decodes a msgpack-RPC tagged array into an `RpcMessage`.
    ///
    /// Returns `RpcError::Malformed` if `v` is not an array, has an
    /// unrecognized kind tag, or has the wrong arity for its kind.
    pub fn from_value(v: Value) -> Result<Self, RpcError> {
        let Value::Array(items) = v else {
            return Err(RpcError::Malformed("not an array".into()));
        };
        let kind = items
            .first()
            .and_then(Value::as_u64)
            .ok_or_else(|| RpcError::Malformed("missing kind".into()))?;
        let arity = items.len();
        match (kind, arity) {
            (0, 4) => Ok(Self::Request {
                msgid: as_u32(&items[1])?,
                method: as_str(&items[2])?,
                params: as_array(&items[3])?,
            }),
            (1, 4) => Ok(Self::Response {
                msgid: as_u32(&items[1])?,
                error: items[2].clone(),
                result: items[3].clone(),
            }),
            (2, 3) => Ok(Self::Notification {
                method: as_str(&items[1])?,
                params: as_array(&items[2])?,
            }),
            _ => Err(RpcError::Malformed(format!("kind={kind} arity={arity}"))),
        }
    }
}

fn as_u32(v: &Value) -> Result<u32, RpcError> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| RpcError::Malformed("bad msgid".into()))
}

fn as_str(v: &Value) -> Result<String, RpcError> {
    v.as_str()
        .map(str::to_owned)
        .ok_or_else(|| RpcError::Malformed("bad string".into()))
}

fn as_array(v: &Value) -> Result<Vec<Value>, RpcError> {
    match v {
        Value::Array(a) => Ok(a.clone()),
        _ => Err(RpcError::Malformed("bad params".into())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use rmpv::Value;

    #[test]
    fn request_roundtrips() {
        let m = RpcMessage::Request {
            msgid: 7,
            method: "nvim_get_api_info".into(),
            params: vec![],
        };
        assert_eq!(RpcMessage::from_value(m.to_value()).unwrap(), m);
    }

    #[test]
    fn response_roundtrips() {
        let m = RpcMessage::Response {
            msgid: 7,
            error: Value::Nil,
            result: Value::from(42),
        };
        assert_eq!(RpcMessage::from_value(m.to_value()).unwrap(), m);
    }

    #[test]
    fn notification_roundtrips() {
        let m = RpcMessage::Notification {
            method: "redraw".into(),
            params: vec![Value::from("x")],
        };
        assert_eq!(RpcMessage::from_value(m.to_value()).unwrap(), m);
    }

    #[test]
    fn garbage_is_a_typed_error() {
        assert!(matches!(
            RpcMessage::from_value(Value::from("nope")),
            Err(RpcError::Malformed(_))
        ));
    }

    #[test]
    fn request_wire_shape_is_tagged_array() {
        let m = RpcMessage::Request {
            msgid: 7,
            method: "nvim_get_api_info".into(),
            params: vec![],
        };
        let expected = Value::Array(vec![
            Value::from(0),
            Value::from(7),
            Value::from("nvim_get_api_info"),
            Value::Array(vec![]),
        ]);
        assert_eq!(m.to_value(), expected);
    }

    #[test]
    fn response_wire_slots_error_then_result() {
        let v = Value::Array(vec![
            Value::from(1),
            Value::from(9),
            Value::Nil,
            Value::from("ok"),
        ]);
        let m = RpcMessage::from_value(v).unwrap();
        assert_eq!(
            m,
            RpcMessage::Response {
                msgid: 9,
                error: Value::Nil,
                result: Value::from("ok"),
            }
        );
    }

    #[test]
    fn notification_wire_shape_is_tagged_array() {
        let m = RpcMessage::Notification {
            method: "redraw".into(),
            params: vec![Value::from(1)],
        };
        let expected = Value::Array(vec![
            Value::from(2),
            Value::from("redraw"),
            Value::Array(vec![Value::from(1)]),
        ]);
        assert_eq!(m.to_value(), expected);
    }
}
