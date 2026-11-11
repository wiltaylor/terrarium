//! Read-only, cross-project Home lists, with the same scope as `/cards`.
use super::{ApiQuery, Shared};
use crate::{
    actor::Actor,
    error::ApiResult,
    policy,
    store::{
        Page,
        home::{self, HomePage},
    },
};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use serde::Deserialize;

pub fn routes() -> Router<Shared> {
    Router::new().route("/home/{kind}", get(list))
}

#[derive(Default, Deserialize)]
struct Query {
    project: Option<String>,
    column: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

async fn list(
    State(state): State<Shared>,
    actor: Actor,
    Path(kind): Path<String>,
    ApiQuery(query): ApiQuery<Query>,
) -> ApiResult<Json<HomePage>> {
    let project = scope(&actor, query.project)?;
    Ok(Json(
        state
            .db
            .read(move |c| {
                home::list(
                    c,
                    &kind,
                    query.column.as_deref().unwrap_or("in_progress"),
                    project.as_deref(),
                    &Page {
                        limit: query.limit,
                        cursor: query.cursor,
                    },
                )
            })
            .await?,
    ))
}

fn scope(actor: &Actor, requested: Option<String>) -> ApiResult<Option<String>> {
    if let Some(project) = &requested {
        policy::read_project(actor, project)?;
        return Ok(requested);
    }
    Ok(match actor {
        Actor::Agent(a) if !a.reads_all() => Some(a.project.clone()),
        _ => None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::actor::AgentActor;
    use terrarium_core::Role;

    #[test]
    fn home_scope_matches_cross_project_card_rights() {
        assert_eq!(scope(&Actor::Operator, None).unwrap(), None);
        for role in [
            None,
            Some(Role::ProjectAdmin),
            Some(Role::Architect),
            Some(Role::Sysadmin),
        ] {
            let reads_all = matches!(role, Some(Role::Architect | Role::Sysadmin));
            let actor = Actor::Agent(AgentActor {
                id: "id".into(),
                name: "member".into(),
                project: "alpha".into(),
                roles: role.into_iter().collect(),
                policy: Default::default(),
            });
            assert_eq!(
                scope(&actor, None).unwrap(),
                if reads_all {
                    None
                } else {
                    Some("alpha".into())
                }
            );
            assert_eq!(
                scope(&actor, Some("alpha".into())).unwrap(),
                Some("alpha".into())
            );
            assert_eq!(scope(&actor, Some("beta".into())).is_ok(), reads_all);
        }
    }
}
