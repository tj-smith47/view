//! The JSON-RPC frame and its newline-delimited stdio framing.
//!
//! The protocol's stdio transport is pinned in `docs/acp-v1-wire-capture.md`:
//! messages are JSON-RPC 2.0, UTF-8, delimited by `\n`, and MUST NOT contain
//! an embedded newline. Nothing here knows what a method means -- this
//! module carries frames, and the session layer above it is the only place
//! that names a method or a payload field.
//!
//! Hand-rolled rather than taken from the published protocol crate. That
//! crate is Apache-2.0 and actively maintained, so the licence and
//! maintenance tests both pass; what fails is the runtime. It is built on
//! `async-io`/`async-process`/`blocking` and carries tokio only as a
//! dev-dependency, so adopting it would put a second reactor and its
//! blocking pool into a process whose design pins exactly one async runtime
//! -- against a wire shape already captured, for a codec this size.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader, Lines};

/// The only `jsonrpc` value the protocol defines.
pub const JSONRPC_VERSION: &str = "2.0";

/// Anything that can go wrong carrying a frame.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// The child's stdin or stdout failed.
    #[error("agent stdio failed: {0}")]
    Io(#[from] std::io::Error),
    /// A line arrived that is not JSON, or is JSON of the wrong shape.
    #[error("agent sent a line that is not a JSON-RPC message: {0}")]
    Decode(#[from] serde_json::Error),
    /// A well-formed JSON object that is none of request, notification, or
    /// response. Distinguished from [`Self::Decode`] because the two call
    /// for different answers: bad JSON means the stream is unusable, while
    /// a well-formed frame of an unknown shape is one message to skip.
    #[error(
        "agent sent a JSON-RPC frame that is neither a request, a notification, nor a response"
    )]
    UnknownFrame,
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// The reserved code for a request whose answer was cancelled, pinned in
/// `docs/acp-v1-wire-capture.md`.
pub const REQUEST_CANCELLED: i64 = -32800;

/// The reserved code for a method the receiver does not implement.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// The reserved code for a request the receiver understood and could not
/// carry out.
pub const INTERNAL_ERROR: i64 = -32603;

/// One frame, in the union shape JSON-RPC 2.0 actually puts on the wire.
///
/// A single struct with optional members rather than an untagged enum:
/// untagged decoding picks a variant by trying each in turn and reports a
/// useless error when every one fails, so a frame that is *almost* a
/// response would be indistinguishable from a frame that is almost a
/// request. Here the frame always decodes, and [`Self::classify`] is the one
/// place that decides which of the three it is -- and says which member was
/// missing when it is none of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcMessage {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcMessage {
    /// A request: answered, so it carries an `id`.
    #[must_use]
    pub fn request(id: u64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(id),
            method: Some(method.to_string()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    /// A notification: one-way, so it carries no `id` and no answer is ever
    /// sent for it.
    #[must_use]
    pub fn notification(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            method: Some(method.to_string()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    /// A successful answer to the request numbered `id`.
    #[must_use]
    pub fn response(id: u64, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(id),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    /// A failed answer to the request numbered `id`.
    #[must_use]
    pub fn error_response(id: u64, code: i64, message: &str) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(id),
            method: None,
            params: None,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.to_string(),
                data: None,
            }),
        }
    }

    /// Which of the three JSON-RPC message kinds this frame is.
    ///
    /// # Errors
    ///
    /// [`WireError::UnknownFrame`] when the frame is none of them.
    pub fn classify(self) -> Result<Incoming, WireError> {
        let params = self.params.unwrap_or(Value::Null);
        match (self.id, self.method) {
            (Some(id), Some(method)) => Ok(Incoming::Request { id, method, params }),
            (None, Some(method)) => Ok(Incoming::Notification { method, params }),
            (Some(id), None) => {
                // an error member wins over a result member when a
                // non-conforming agent sends both: the failure is the load-
                // bearing half, and treating such a frame as a success would
                // hand the session a result it has no reason to trust
                let outcome = match self.error {
                    Some(error) => Err(error),
                    None => Ok(self.result.unwrap_or(Value::Null)),
                };
                Ok(Incoming::Response { id, outcome })
            }
            (None, None) => Err(WireError::UnknownFrame),
        }
    }
}

/// A frame, sorted into the kind the session layer dispatches on.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// The agent is asking something of this client and expects an answer
    /// addressed to `id`.
    Request {
        id: u64,
        method: String,
        params: Value,
    },
    /// The agent is reporting something; no answer is expected or legal.
    Notification { method: String, params: Value },
    /// The agent's answer to a request this client sent.
    Response {
        id: u64,
        outcome: Result<Value, JsonRpcError>,
    },
}

/// The read half: one frame per line.
#[derive(Debug)]
pub struct JsonRpcReader<R> {
    lines: Lines<BufReader<R>>,
}

impl<R: AsyncRead + Unpin> JsonRpcReader<R> {
    /// The next frame, or `None` once the stream has ended.
    ///
    /// # Errors
    ///
    /// [`WireError::Io`] if the stream fails, [`WireError::Decode`] if a
    /// line is not a JSON object.
    pub async fn next_message(&mut self) -> Result<Option<JsonRpcMessage>, WireError> {
        loop {
            let Some(line) = self.lines.next_line().await? else {
                return Ok(None);
            };
            // a blank line is skipped rather than reported: it carries no
            // frame, so surfacing it as a decode failure would turn a
            // trailing newline -- which costs nothing to ignore -- into a
            // dead session
            if line.trim().is_empty() {
                continue;
            }
            return Ok(Some(serde_json::from_str(&line)?));
        }
    }
}

/// The write half: one line per frame, flushed immediately.
#[derive(Debug)]
pub struct JsonRpcWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> JsonRpcWriter<W> {
    /// Writes one frame and flushes it.
    ///
    /// # Errors
    ///
    /// [`WireError::Io`] if the write fails, [`WireError::Decode`] if the
    /// frame cannot be serialized.
    pub async fn write_message(&mut self, message: &JsonRpcMessage) -> Result<(), WireError> {
        // compact serialization is what keeps the transport's "no embedded
        // newline" rule true by construction: a pretty-printed frame would
        // break framing outright, and a newline inside a string field is
        // escaped by JSON itself rather than emitted raw
        let mut line = serde_json::to_string(message)?;
        line.push('\n');
        self.inner.write_all(line.as_bytes()).await?;
        // flushed per frame, not per batch: an unflushed request is one the
        // agent never sees, and every request here is one the session is
        // about to wait on
        self.inner.flush().await?;
        Ok(())
    }
}

/// The two halves of one child's stdio, constructed together and used
/// apart.
#[derive(Debug)]
pub struct JsonRpcCodec<R, W> {
    reader: JsonRpcReader<R>,
    writer: JsonRpcWriter<W>,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> JsonRpcCodec<R, W> {
    /// Wraps the child's stdout (frames in) and stdin (frames out).
    #[must_use]
    pub fn new(stdout: R, stdin: W) -> Self {
        Self {
            reader: JsonRpcReader {
                lines: BufReader::new(stdout).lines(),
            },
            writer: JsonRpcWriter { inner: stdin },
        }
    }

    /// Splits into halves that can be awaited independently.
    ///
    /// The session loop reads and writes concurrently, and a single value
    /// owning both would force each write to wait for whatever read is
    /// outstanding -- with a child that only speaks when spoken to, that is
    /// a deadlock rather than a slowdown.
    #[must_use]
    pub fn split(self) -> (JsonRpcReader<R>, JsonRpcWriter<W>) {
        (self.reader, self.writer)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    /// Verbatim from `docs/acp-v1-wire-capture.md`'s worked `initialize`
    /// pair.
    const INITIALIZE_REQUEST: &str = r#"{
      "jsonrpc": "2.0",
      "id": 0,
      "method": "initialize",
      "params": {
        "protocolVersion": 1,
        "clientCapabilities": {
          "fs": {
            "readTextFile": true,
            "writeTextFile": true
          },
          "terminal": true
        },
        "clientInfo": {
          "name": "my-client",
          "title": "My Client",
          "version": "1.0.0"
        }
      }
    }"#;

    const INITIALIZE_RESPONSE: &str = r#"{
      "jsonrpc": "2.0",
      "id": 0,
      "result": {
        "protocolVersion": 1,
        "agentCapabilities": {
          "loadSession": true,
          "promptCapabilities": {
            "image": true,
            "audio": true,
            "embeddedContext": true
          },
          "mcpCapabilities": {
            "http": true,
            "sse": true
          }
        },
        "agentInfo": {
          "name": "my-agent",
          "title": "My Agent",
          "version": "1.0.0"
        },
        "authMethods": []
      }
    }"#;

    /// serde_json's object map is ordered, so re-serializing a parsed
    /// `Value` puts every member of every object in one fixed order: two
    /// byte-identical canonical forms mean the two documents carry exactly
    /// the same members and the same values, with nothing dropped and
    /// nothing invented. Member order itself is not part of what JSON means,
    /// which is why it is normalized away rather than asserted on.
    fn canonical(json: &str) -> String {
        let value: Value = serde_json::from_str(json).unwrap();
        serde_json::to_string(&value).unwrap()
    }

    #[tokio::test]
    async fn the_captured_initialize_pair_round_trips_byte_for_byte() {
        let mut written = Vec::new();
        {
            let mut writer = JsonRpcWriter {
                inner: &mut written,
            };
            for captured in [INITIALIZE_REQUEST, INITIALIZE_RESPONSE] {
                let frame: JsonRpcMessage = serde_json::from_str(captured).unwrap();
                writer.write_message(&frame).await.unwrap();
            }
        }

        let mut reader = JsonRpcReader {
            lines: BufReader::new(written.as_slice()).lines(),
        };
        let mut round_tripped = Vec::new();
        while let Some(frame) = reader.next_message().await.unwrap() {
            round_tripped.push(canonical(&serde_json::to_string(&frame).unwrap()));
        }

        assert_eq!(
            round_tripped,
            vec![
                canonical(INITIALIZE_REQUEST),
                canonical(INITIALIZE_RESPONSE)
            ]
        );
    }

    #[tokio::test]
    async fn frames_are_delimited_by_newlines_and_carry_none_of_their_own() {
        let mut written = Vec::new();
        {
            let mut writer = JsonRpcWriter {
                inner: &mut written,
            };
            writer
                .write_message(&JsonRpcMessage::notification(
                    "session/update",
                    serde_json::json!({ "text": "one\ntwo\nthree" }),
                ))
                .await
                .unwrap();
            writer
                .write_message(&JsonRpcMessage::request(
                    7,
                    "initialize",
                    serde_json::json!({ "protocolVersion": 1 }),
                ))
                .await
                .unwrap();
        }

        let text = String::from_utf8(written).unwrap();
        assert_eq!(text.matches('\n').count(), 2, "two frames, two delimiters");
        assert!(text.ends_with('\n'));

        let mut reader = JsonRpcReader {
            lines: BufReader::new(text.as_bytes()).lines(),
        };
        let first = reader.next_message().await.unwrap().unwrap();
        let Incoming::Notification { method, params } = first.classify().unwrap() else {
            panic!("first frame is a notification")
        };
        assert_eq!(method, "session/update");
        assert_eq!(params["text"], "one\ntwo\nthree");
        let second = reader.next_message().await.unwrap().unwrap();
        assert!(matches!(
            second.classify().unwrap(),
            Incoming::Request { id: 7, .. }
        ));
        assert!(reader.next_message().await.unwrap().is_none());
    }

    #[test]
    fn classify_sorts_the_three_message_kinds_and_rejects_the_fourth() {
        let request = JsonRpcMessage::request(1, "session/new", Value::Null);
        assert!(matches!(
            request.classify().unwrap(),
            Incoming::Request { id: 1, .. }
        ));

        let notification = JsonRpcMessage::notification("session/update", Value::Null);
        assert!(matches!(
            notification.classify().unwrap(),
            Incoming::Notification { .. }
        ));

        let ok = JsonRpcMessage::response(2, serde_json::json!({ "sessionId": "s" }));
        let Incoming::Response { id: 2, outcome } = ok.classify().unwrap() else {
            panic!("a frame with an id and a result is a response")
        };
        assert_eq!(outcome.unwrap()["sessionId"], "s");

        let failed = JsonRpcMessage::error_response(3, REQUEST_CANCELLED, "Cancelled");
        let Incoming::Response { id: 3, outcome } = failed.classify().unwrap() else {
            panic!("a frame with an id and an error is a response")
        };
        assert_eq!(outcome.unwrap_err().code, REQUEST_CANCELLED);

        let orphan = JsonRpcMessage {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            method: None,
            params: None,
            result: Some(Value::Null),
            error: None,
        };
        assert!(matches!(orphan.classify(), Err(WireError::UnknownFrame)));
    }

    #[tokio::test]
    async fn a_blank_line_is_skipped_and_a_broken_line_is_reported() {
        let stream = "\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\nnot json\n";
        let mut reader = JsonRpcReader {
            lines: BufReader::new(stream.as_bytes()).lines(),
        };
        assert!(matches!(
            reader
                .next_message()
                .await
                .unwrap()
                .unwrap()
                .classify()
                .unwrap(),
            Incoming::Response { id: 1, .. }
        ));
        assert!(matches!(
            reader.next_message().await,
            Err(WireError::Decode(_))
        ));
    }
}
