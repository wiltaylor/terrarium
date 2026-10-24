//! Who makes a request. The `resolve_actor` middleware sets an [`Actor`] on
//! every API request; handlers take it as an extractor.
//!
//! - TCP with the bearer token: the operator.
//! - The Unix socket: the operator, unless the request carries
//!   `X-Terrarium-Agent`. Then it is that agent. The header is for tests and
//!   for the runner socket of a later phase; TCP ignores it.

use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use terrarium_core::Role;
use terrarium_core::config::Policy;

use crate::api::Shared;
use crate::error::{ApiError, Code};

/// The header that names the calling agent on the Unix socket.
pub const AGENT_HEADER: &str = "x-terrarium-agent";

/// The listener a request came in on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Unix,
    Tcp,
}

/// An agent that makes a request.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentActor {
    pub id: String,
    pub name: String,
    pub project: String,
    /// The roles above `member`.
    pub roles: Vec<Role>,
    pub policy: Policy,
}

impl AgentActor {
    pub fn is_sysadmin(&self) -> bool {
        self.roles.contains(&Role::Sysadmin)
    }

    /// `project_admin` of `project`, or `sysadmin`.
    pub fn is_admin_of(&self, project: &str) -> bool {
        self.is_sysadmin() || (self.roles.contains(&Role::ProjectAdmin) && self.project == project)
    }
}

/// The caller of an API request.
#[derive(Debug, Clone, PartialEq)]
pub enum Actor {
    Operator,
    Agent(AgentActor),
}

impl Actor {
    /// The label in events and audit rows: `operator` or `agent:<ulid>`.
    pub fn label(&self) -> String {
        match self {
            Self::Operator => "operator".into(),
            Self::Agent(a) => format!("agent:{}", a.id),
        }
    }

    /// The address in mail, card authors, and assignees: `operator` or the
    /// agent ULID.
    pub fn address(&self) -> String {
        match self {
            Self::Operator => "operator".into(),
            Self::Agent(a) => a.id.clone(),
        }
    }

    pub fn agent(&self) -> Option<&AgentActor> {
        match self {
            Self::Operator => None,
            Self::Agent(a) => Some(a),
        }
    }

    pub fn is_operator(&self) -> bool {
        matches!(self, Self::Operator)
    }

    /// The project of an agent actor.
    pub fn project(&self) -> Option<&str> {
        self.agent().map(|a| a.project.as_str())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Actor {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Actor>()
            .cloned()
            .ok_or_else(|| ApiError::internal("no actor on the request"))
    }
}

/// Set the [`Actor`] of an API request.
pub async fn resolve_actor(State(state): State<Shared>, mut req: Request, next: Next) -> Response {
    let transport = req.extensions().get::<Transport>().copied();
    let header = req
        .headers()
        .get(AGENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let actor = match (transport, header) {
        (Some(Transport::Unix), Some(key)) => {
            let found = state
                .db
                .read(move |c| crate::store::agents::actor_for(c, &key))
                .await;
            match found {
                Ok(Some(agent)) => Actor::Agent(agent),
                Ok(None) => {
                    return ApiError::new(Code::Unauthorized, "unknown agent in X-Terrarium-Agent")
                        .into_response();
                }
                Err(e) => return e.into_response(),
            }
        }
        _ => Actor::Operator,
    };
    req.extensions_mut().insert(actor);
    next.run(req).await
}
