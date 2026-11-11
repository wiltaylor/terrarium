//! Who makes a request. The `resolve_actor` middleware sets an [`Actor`] on
//! every API request; handlers take it as an extractor.
//!
//! - TCP with the bearer token, a session cookie, or a CLI session token:
//!   the operator.
//! - The Unix socket: the operator.
//! - The runner socket of an agent (`agent_rpc`): that agent. The tunnel
//!   puts the [`Actor`] on the request before the router sees it, and the
//!   middleware keeps it. No client can set a request extension.
//! - `X-Terrarium-Agent` on the Unix socket names an agent only when the
//!   daemon runs with `--insecure-agent-header` (tests and development), or
//!   in unit tests. Else the header gets `401 unauthorized`, so a caller does
//!   not think it acts as an agent when it does not. TCP ignores it.

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

    /// `sysadmin` or `architect`: reads every project (TER-47). Only for
    /// the checks of a read; a write never takes it.
    pub fn reads_all(&self) -> bool {
        self.is_sysadmin() || self.roles.contains(&Role::Architect)
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
    /// The label in events and audit rows: `operator`, `agent:<ulid>`, or
    /// `link:<ulid>` for the operator through a link key (the task-local
    /// `crate::links::CALLER` of the request, TER-61).
    pub fn label(&self) -> String {
        match self {
            Self::Operator => crate::links::current_label().unwrap_or_else(|| "operator".into()),
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
    // The agent RPC tunnel set the actor from the runner socket.
    if req.extensions().get::<Actor>().is_some() {
        return next.run(req).await;
    }
    let transport = req.extensions().get::<Transport>().copied();
    let header = req
        .headers()
        .get(AGENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let actor = match (transport, header) {
        (Some(Transport::Unix), Some(_)) if !trust_header(&state) => {
            return ApiError::new(
                Code::Unauthorized,
                "X-Terrarium-Agent is not trusted; an agent calls through its runner socket \
                 (start the daemon with --insecure-agent-header for tests)",
            )
            .into_response();
        }
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

/// Whether `X-Terrarium-Agent` names the caller. Always in unit tests.
fn trust_header(state: &Shared) -> bool {
    cfg!(test) || state.insecure_agent_header
}
