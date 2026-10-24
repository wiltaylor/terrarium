//! The runner socket protocol: newline-delimited JSON on a Unix socket. See
//! `docs/runner.md`.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use terrarium_core::{AgentId, AgentStatus, HarnessId};

/// One line on the runner socket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    /// A hook call from `terrarium hook <event>`. Fire and forget: the runner
    /// sends no answer.
    Hook {
        hook: String,
        #[serde(default)]
        payload: Value,
    },
    /// A call from the agent CLI. The runner answers with a `response` that
    /// carries the same `id`.
    Request {
        id: Value,
        method: String,
        #[serde(default)]
        params: Value,
    },
    /// The answer to a `request`. Exactly one of `result` and `error` is set.
    Response {
        id: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<RpcError>,
    },
}

impl Message {
    pub fn response(id: Value, outcome: Result<Value, RpcError>) -> Self {
        match outcome {
            Ok(result) => Self::Response {
                id,
                result: Some(result),
                error: None,
            },
            Err(error) => Self::Response {
                id,
                result: None,
                error: Some(error),
            },
        }
    }
}

/// An error answer on the runner socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct RpcError {
    /// A stable `snake_case` code, for example `method_not_found`.
    pub code: String,
    pub message: String,
}

impl RpcError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new("method_not_found", format!("unknown method `{method}`"))
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new("parse_error", message)
    }
}

/// Who sent a request. The runner knows the caller by the socket that the
/// request arrived on: each agent has its own socket. A runner process sends
/// it to the daemon with each `rpc_request` frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestContext {
    pub agent: AgentId,
    pub name: String,
    pub project: String,
    /// The roles above `member`, for `whoami`.
    #[serde(default)]
    pub roles: Vec<String>,
    pub harness: HarnessId,
    pub status: AgentStatus,
}

/// A boxed, sendable future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Answers agent CLI requests that the runner does not answer itself. The
/// runner answers `ping` and `whoami`. The daemon implements this trait for
/// mail, the board, and the other agent commands.
pub trait RequestHandler: Send + Sync {
    fn handle<'a>(
        &'a self,
        ctx: &'a RequestContext,
        method: &'a str,
        params: Value,
    ) -> BoxFuture<'a, Result<Value, RpcError>>;
}

/// A handler that knows no methods.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHandler;

impl RequestHandler for NoHandler {
    fn handle<'a>(
        &'a self,
        _ctx: &'a RequestContext,
        method: &'a str,
        _params: Value,
    ) -> BoxFuture<'a, Result<Value, RpcError>> {
        let err = RpcError::method_not_found(method);
        Box::pin(async move { Err(err) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_use_the_documented_shape() {
        let hook: Message =
            serde_json::from_str(r#"{"type":"hook","hook":"Stop","payload":{"a":1}}"#)
                .expect("hook");
        assert_eq!(
            hook,
            Message::Hook {
                hook: "Stop".into(),
                payload: json!({"a": 1})
            }
        );
        let req: Message =
            serde_json::from_str(r#"{"type":"request","id":7,"method":"ping"}"#).expect("req");
        assert!(matches!(req, Message::Request { ref method, .. } if method == "ping"));
        let ok = serde_json::to_value(Message::response(json!(7), Ok(json!("pong")))).expect("ok");
        assert_eq!(ok, json!({"type":"response","id":7,"result":"pong"}));
        let err = serde_json::to_value(Message::response(
            json!(8),
            Err(RpcError::method_not_found("x")),
        ))
        .expect("err");
        assert_eq!(err["error"]["code"], "method_not_found");
        assert!(err.get("result").is_none());
    }
}
