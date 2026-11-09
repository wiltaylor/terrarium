//! The API error type and the JSON error shape.
//!
//! Every error answer has the body `{"error": {"code", "message", "details"?}}`.
//! `docs/api.md` lists the codes.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;

/// The stable error codes of the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// The request is malformed: bad JSON, a bad query, a bad ID.
    BadRequest,
    /// No bearer token, a wrong token, or an unknown agent header.
    Unauthorized,
    /// The actor may not do this. The message says which rule refused it.
    Forbidden,
    /// A link key called a route of the link deny list (`docs/api.md`
    /// §35), or a read-only link tried to write.
    LinkForbidden,
    /// The object does not exist, or the actor may not see it.
    NotFound,
    /// The route exists, but not with this method.
    MethodNotAllowed,
    /// The object exists already, or its state does not allow the change.
    Conflict,
    /// Host admission is temporarily busy with lifecycle or limit changes.
    ResourceBusy,
    /// The object is declared in a file. Edit the file and reload.
    FileManaged,
    /// A destructive action needs `?confirm=true`.
    ConfirmRequired,
    /// The values are well-formed but break a rule.
    Invalid,
    /// The target column is at its WIP limit.
    WipLimit,
    /// The card has dependencies that are not done.
    DependenciesUnmet,
    /// Moving a card to `blocked` needs a comment.
    CommentRequired,
    /// The agent is not running.
    NotRunning,
    /// The agent is running already.
    AlreadyRunning,
    /// A wiki edit names a base revision that is not the current one.
    StaleBase,
    /// The upload is over the per-file limit or the project quota.
    TooLarge,
    /// The body is not of a media type that the endpoint takes (an
    /// attachment that is not a PNG, JPEG, GIF, or WebP image).
    UnsupportedMediaType,
    /// The caller sent too many requests of this kind; try again later.
    RateLimited,
    /// Another agent holds a lease on the target that this lease conflicts
    /// with (an exclusive lease, or any lease for an exclusive request).
    LeaseHeld,
    /// The endpoint exists, but this phase does not implement the request.
    NotAvailable,
    /// The daemon cannot do the request on this machine now (the repo
    /// browser has no `bwrap` for an agent tree).
    Unavailable,
    /// A bug or an I/O failure in the daemon.
    Internal,
}

impl Code {
    pub fn status(self) -> StatusCode {
        match self {
            Self::BadRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::LinkForbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            Self::Conflict
            | Self::ResourceBusy
            | Self::FileManaged
            | Self::ConfirmRequired
            | Self::WipLimit
            | Self::DependenciesUnmet
            | Self::NotRunning
            | Self::AlreadyRunning
            | Self::StaleBase
            | Self::LeaseHeld => StatusCode::CONFLICT,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::NotAvailable => StatusCode::NOT_IMPLEMENTED,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Invalid | Self::CommentRequired => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// An error that becomes an HTTP answer.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub code: Code,
    pub message: String,
    pub details: Option<Value>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(Code::BadRequest, m)
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(Code::Forbidden, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(Code::NotFound, m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(Code::Conflict, m)
    }
    pub fn resource_busy(m: impl Into<String>) -> Self {
        Self::new(Code::ResourceBusy, m)
    }
    pub fn invalid(m: impl Into<String>) -> Self {
        Self::new(Code::Invalid, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(Code::Internal, m)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        tracing::error!("database: {e}");
        Self::internal(format!("database error: {e}"))
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        tracing::error!("json: {e}");
        Self::internal(format!("json error: {e}"))
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("{e:#}");
        Self::internal(format!("{e:#}"))
    }
}

#[derive(Serialize)]
struct Body<'a> {
    error: Inner<'a>,
}

#[derive(Serialize)]
struct Inner<'a> {
    code: Code,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a Value>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Body {
            error: Inner {
                code: self.code,
                message: &self.message,
                details: self.details.as_ref(),
            },
        };
        (self.code.status(), Json(body)).into_response()
    }
}
