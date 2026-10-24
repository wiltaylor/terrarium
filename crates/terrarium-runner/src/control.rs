//! The control protocol between a runner process and the daemon:
//! newline-delimited JSON on `<runtime_dir>/runners/<ulid>.sock`. See
//! `docs/runner.md`, "The control socket".

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use terrarium_core::{AgentId, AgentStatus, Answer, HarnessId, HarnessSessionId};

use crate::error::DeliveryError;
use crate::protocol::{RequestContext, RpcError};
use crate::runner::{ExitStatus, ScreenSnapshot};

/// The version of the control protocol. `hello` reports it.
pub const PROTOCOL_VERSION: u32 = 1;

/// A line from the daemon to the runner process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Call `method`. The runner answers with one `reply` with the same `id`.
    /// A stream method sends `stream` frames and one `end` frame instead.
    Call {
        id: u64,
        method: String,
        #[serde(default)]
        params: Value,
    },
    /// The answer to an `rpc_request`.
    RpcResponse {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
}

/// A line from the runner process to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// The answer to a call. Exactly one of `result` and `error` is set.
    Reply {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
    /// One item of the stream that call `id` opened.
    Stream { id: u64, item: Value },
    /// The stream of call `id` is over. `error` says why, when it did not
    /// end normally.
    End {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
    /// The outcome of the typed turn `delivery` (from a `type_turn` reply).
    Delivery {
        delivery: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<WireDeliveryError>,
    },
    /// An agent RPC request that the runner does not answer itself. The
    /// daemon answers with an `rpc_response` with the same `id`.
    RpcRequest {
        id: u64,
        method: String,
        #[serde(default)]
        params: Value,
        context: RequestContext,
    },
}

impl ServerFrame {
    pub fn reply(id: u64, outcome: Result<Value, RpcError>) -> Self {
        match outcome {
            Ok(v) => Self::Reply {
                id,
                result: Some(v),
                error: None,
            },
            Err(e) => Self::Reply {
                id,
                result: None,
                error: Some(e),
            },
        }
    }
}

/// The params of `hello`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloParams {
    /// A name for the log, for example `terrariumd`.
    #[serde(default)]
    pub client: String,
    /// The protocol version of the client.
    #[serde(default)]
    pub protocol: u32,
}

/// The result of `hello`: what the runner process holds now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// The version of the runner binary.
    pub version: String,
    pub agent: AgentId,
    pub harness: HarnessId,
    pub runner_pid: u32,
    /// The harness process (for a sandbox: `bwrap`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_pid: Option<u32>,
    pub status: AgentStatus,
    /// The harness session from the last `session_started` event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<HarnessSessionId>,
    /// The seq of the last line of `events.jsonl`.
    pub seq: u64,
    /// Set when the harness has exited and the runner is about to exit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitStatus>,
    /// The hook and agent RPC socket, `<run>/agents/<ulid>.sock`.
    pub hook_socket: PathBuf,
    pub events_path: PathBuf,
}

/// The params of `events`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsParams {
    /// Send the records after this seq first. `None`: only new records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

/// The params of `write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteParams {
    /// The bytes, base64.
    pub data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeParams {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeTurnParams {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerParams {
    pub request_id: String,
    pub answer: Answer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerKeysParams {
    pub keys: String,
}

/// The params of `set_stopped_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedByParams {
    pub stopped_by: crate::process::StoppedBy,
}

/// The params of `cancel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelParams {
    /// The call ID of the stream to stop.
    pub call: u64,
}

/// A screen snapshot on the wire. `formatted` is base64.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireSnapshot {
    pub rows: u16,
    pub cols: u16,
    pub lines: Vec<String>,
    pub cursor: (u16, u16),
    pub formatted: String,
}

impl From<&ScreenSnapshot> for WireSnapshot {
    fn from(s: &ScreenSnapshot) -> Self {
        Self {
            rows: s.rows,
            cols: s.cols,
            lines: s.lines.clone(),
            cursor: s.cursor,
            formatted: B64.encode(&s.formatted),
        }
    }
}

impl WireSnapshot {
    pub fn into_snapshot(self) -> Result<ScreenSnapshot, RpcError> {
        Ok(ScreenSnapshot {
            rows: self.rows,
            cols: self.cols,
            lines: self.lines,
            cursor: self.cursor,
            formatted: decode(&self.formatted)?,
        })
    }
}

/// A [`DeliveryError`] on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireDeliveryError {
    /// `no_submit_hook`, `exited`, `write`, or `dropped`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl From<&DeliveryError> for WireDeliveryError {
    fn from(e: &DeliveryError) -> Self {
        let (kind, attempts, message) = match e {
            DeliveryError::NoSubmitHook { attempts } => ("no_submit_hook", Some(*attempts), None),
            DeliveryError::Exited => ("exited", None, None),
            DeliveryError::Write(m) => ("write", None, Some(m.clone())),
            DeliveryError::Dropped => ("dropped", None, None),
        };
        Self {
            kind: kind.into(),
            attempts,
            message,
        }
    }
}

impl From<WireDeliveryError> for DeliveryError {
    fn from(w: WireDeliveryError) -> Self {
        match w.kind.as_str() {
            "no_submit_hook" => Self::NoSubmitHook {
                attempts: w.attempts.unwrap_or(0),
            },
            "exited" => Self::Exited,
            "write" => Self::Write(w.message.unwrap_or_default()),
            _ => Self::Dropped,
        }
    }
}

/// Base64 for the `data` and `output` fields.
pub fn encode(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn decode(text: &str) -> Result<Vec<u8>, RpcError> {
    B64.decode(text)
        .map_err(|e| RpcError::new("invalid_params", format!("bad base64: {e}")))
}

/// Error codes of the control protocol.
pub mod codes {
    pub const HELLO_REQUIRED: &str = "hello_required";
    pub const INVALID_PARAMS: &str = "invalid_params";
    pub const METHOD_NOT_FOUND: &str = "method_not_found";
    pub const EXITED: &str = "exited";
    pub const FAILED: &str = "failed";
    pub const LAGGED: &str = "lagged";
    pub const REPLACED: &str = "replaced";
    pub const CANCELLED: &str = "cancelled";
    /// The agent RPC tunnel: no daemon took the request.
    pub const UNAVAILABLE: &str = "unavailable";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_use_the_documented_shape() {
        let call: ClientFrame = serde_json::from_value(
            json!({"type":"call","id":1,"method":"hello","params":{"client":"t","protocol":1}}),
        )
        .expect("call");
        assert!(matches!(call, ClientFrame::Call { id: 1, ref method, .. } if method == "hello"));
        let d = serde_json::to_value(ServerFrame::Delivery {
            delivery: 3,
            ok: false,
            error: Some((&DeliveryError::NoSubmitHook { attempts: 2 }).into()),
        })
        .expect("delivery");
        assert_eq!(
            d,
            json!({"type":"delivery","delivery":3,"ok":false,
                   "error":{"kind":"no_submit_hook","attempts":2}})
        );
        let end = serde_json::to_value(ServerFrame::End { id: 4, error: None }).expect("end");
        assert_eq!(end, json!({"type":"end","id":4}));
    }

    #[test]
    fn delivery_errors_round_trip() {
        for e in [
            DeliveryError::NoSubmitHook { attempts: 2 },
            DeliveryError::Exited,
            DeliveryError::Write("eio".into()),
            DeliveryError::Dropped,
        ] {
            let w = WireDeliveryError::from(&e);
            assert_eq!(DeliveryError::from(w), e);
        }
    }
}
