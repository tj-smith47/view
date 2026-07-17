use rmpv::Value;

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("malformed rpc message: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RpcMessage {
    Request {
        msgid: u32,
        method: String,
        params: Vec<Value>,
    },
    Response {
        msgid: u32,
        error: Value,
        result: Value,
    },
    Notification {
        method: String,
        params: Vec<Value>,
    },
}

impl RpcMessage {
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
}
