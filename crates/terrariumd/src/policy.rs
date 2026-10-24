//! The permission rules of PLAN §10 (roles) and §12 (mail scope).
//!
//! Each check returns `Ok(())` or a `forbidden` error whose message names
//! the rule, so an agent learns the boundary. The operator may do anything.
//! The board rules of §13 live in `store::board`, because they need the card.

use terrarium_core::Role;

use crate::actor::{Actor, AgentActor};
use crate::error::{ApiError, ApiResult};
use crate::store::mail::Address;

fn deny(what: &str, need: &str) -> ApiError {
    ApiError::forbidden(format!("{what} needs {need}"))
}

/// Read a project: its peers, board, repos, and targets.
/// Members read their own project; `sysadmin` reads all.
pub fn read_project(actor: &Actor, project: &str) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.project == project || a.is_sysadmin() => Ok(()),
        Actor::Agent(_) => Err(ApiError::forbidden(format!(
            "project `{project}` is not your project; reading it needs sysadmin"
        ))),
    }
}

/// True when the actor administers `project`: the operator, a `sysadmin`, or
/// a `project_admin` of that project.
pub fn is_admin_of(actor: &Actor, project: &str) -> bool {
    match actor {
        Actor::Operator => true,
        Actor::Agent(a) => a.is_admin_of(project),
    }
}

/// A `project_admin` capability of the §10 table: manage agents, set
/// prompts and policies, edit columns and WIP limits, attach repos, read the
/// project inbox.
pub fn admin_project(actor: &Actor, project: &str, what: &str) -> ApiResult<()> {
    if is_admin_of(actor, project) {
        Ok(())
    } else {
        Err(deny(
            what,
            &format!("project_admin of `{project}`, sysadmin, or the operator"),
        ))
    }
}

/// A `sysadmin` capability: create and delete projects, reload definitions.
pub fn sysadmin(actor: &Actor, what: &str) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.is_sysadmin() => Ok(()),
        Actor::Agent(_) => Err(deny(what, "sysadmin or the operator")),
    }
}

/// Read the project inbox and reply from it.
pub fn read_inbox(actor: &Actor, project: &str) -> ApiResult<()> {
    admin_project(actor, project, "reading the project inbox")
}

/// Grant or revoke `role` on the agent `target` of `target_project`.
pub fn change_role(actor: &Actor, target: &str, target_project: &str, role: Role) -> ApiResult<()> {
    if let Actor::Agent(a) = actor
        && a.id == target
    {
        return Err(ApiError::forbidden("an agent cannot change its own roles"));
    }
    match role {
        Role::Member => Ok(()),
        Role::Sysadmin if actor.is_operator() => Ok(()),
        Role::Sysadmin => Err(deny("granting or revoking sysadmin", "the operator")),
        Role::ProjectAdmin => {
            admin_project(actor, target_project, "granting or revoking project_admin")
        }
    }
}

/// May `sender` mail `to`? See PLAN §12.
///
/// - `operator` and `project:<name>` inboxes take mail from any agent.
/// - `mail_to = ["*"]` means every agent in the sender's project; other
///   entries name agents of the project (`name` or `project/name`).
/// - An agent of another project needs an explicit `project/name` entry in
///   `cross_project`, or `project/*` for every agent of that project.
/// - A `sysadmin` may mail any agent.
pub fn mail_to(sender: &AgentActor, to: &Address) -> ApiResult<()> {
    let (id, name, project) = match to {
        Address::Operator | Address::Project(_) => return Ok(()),
        Address::Agent { id, name, project } => (id, name, project),
    };
    if sender.is_sysadmin() || *id == sender.id {
        return Ok(());
    }
    let full = format!("{project}/{name}");
    if *project == sender.project {
        let allowed = sender
            .policy
            .mail_to
            .iter()
            .any(|e| e == "*" || *e == *name || *e == full);
        if allowed {
            return Ok(());
        }
        return Err(ApiError::forbidden(format!(
            "your policy.mail_to does not include `{name}`"
        )));
    }
    let wildcard = format!("{project}/*");
    if sender
        .policy
        .cross_project
        .iter()
        .any(|e| *e == full || *e == wildcard)
    {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "`{full}` is outside project `{}`; add \"{full}\" to policy.cross_project, \
         or write to the project inbox `project:{project}`",
        sender.project
    )))
}

// ---- memory graph, wiki, artifacts, versions (PLAN §14, §15) ----
//
// Every member reads and writes its own project's memories, pages,
// artifacts, and versions. The checks below are the exceptions.

/// Edit the title, body, or sources of a memory: its author, or an admin of
/// the project. Anyone may add tags and links.
pub fn edit_memory(actor: &Actor, project: &str, author: &str, id: &str) -> ApiResult<()> {
    if actor.address() == author || is_admin_of(actor, project) {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "{id} is not your memory; only its author, a project admin, or the operator edits it \
         (you may add tags and links, or write a correction and supersede it)"
    )))
}

/// Delete a memory. Agents supersede; admins and the operator delete.
pub fn delete_memory(actor: &Actor, project: &str) -> ApiResult<()> {
    if is_admin_of(actor, project) {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "deleting a memory needs project_admin of `{project}`, sysadmin, or the operator; \
         agents supersede instead (POST /api/memories/{{id}}/supersede)"
    )))
}

/// Remove a memory link: its author or an admin.
pub fn unlink_memory(actor: &Actor, project: &str, author: &str) -> ApiResult<()> {
    if actor.address() == author || is_admin_of(actor, project) {
        return Ok(());
    }
    Err(ApiError::forbidden(
        "only the author of a link, a project admin, or the operator removes it",
    ))
}

/// Move, delete, or undelete a wiki page.
pub fn admin_wiki(actor: &Actor, project: &str, what: &str) -> ApiResult<()> {
    admin_project(actor, project, what)
}

/// Review a wiki page: the operator, an admin, or the agent named as the
/// page's `reviewer`.
pub fn review_page(actor: &Actor, project: &str, reviewer: Option<&str>) -> ApiResult<()> {
    if is_admin_of(actor, project) {
        return Ok(());
    }
    if let (Actor::Agent(a), Some(r)) = (actor, reviewer)
        && (r == a.name || r == a.id || r == format!("{}/{}", a.project, a.name))
    {
        return Ok(());
    }
    Err(deny(
        "reviewing a page",
        &format!("the page's reviewer, project_admin of `{project}`, sysadmin, or the operator"),
    ))
}

/// Delete an artifact: its author, or an admin.
pub fn delete_artifact(actor: &Actor, project: &str, author: &str) -> ApiResult<()> {
    if actor.address() == author || is_admin_of(actor, project) {
        return Ok(());
    }
    Err(ApiError::forbidden(
        "only the author of an artifact, a project admin, or the operator deletes it",
    ))
}

/// Prune artifacts by the retention rule.
pub fn prune_artifacts(actor: &Actor) -> ApiResult<()> {
    sysadmin(actor, "pruning artifacts")
}

/// Edit or submit a version: its creator, or an admin.
pub fn edit_version(actor: &Actor, project: &str, creator: &str, name: &str) -> ApiResult<()> {
    if actor.address() == creator || is_admin_of(actor, project) {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "version `{name}` is not yours; only its creator, a project admin, or the operator changes it"
    )))
}

/// Approve or reject a version: the user decides, so only the operator.
pub fn decide_version(actor: &Actor) -> ApiResult<()> {
    if actor.is_operator() {
        Ok(())
    } else {
        Err(deny("approving or rejecting a version", "the operator"))
    }
}

/// Release an approved version.
pub fn release_version(actor: &Actor, project: &str) -> ApiResult<()> {
    admin_project(actor, project, "releasing a version")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use terrarium_core::config::Policy;

    fn agent(project: &str, roles: &[Role], policy: Policy) -> AgentActor {
        AgentActor {
            id: format!("{project}-id"),
            name: "me".into(),
            project: project.into(),
            roles: roles.to_vec(),
            policy,
        }
    }

    fn peer(project: &str, name: &str) -> Address {
        Address::Agent {
            id: format!("{project}-{name}"),
            name: name.into(),
            project: project.into(),
        }
    }

    #[test]
    fn wildcard_means_same_project() {
        let a = agent("p", &[], Policy::default());
        assert!(mail_to(&a, &peer("p", "x")).is_ok());
        assert!(mail_to(&a, &peer("q", "x")).is_err());
        assert!(mail_to(&a, &Address::Project("q".into())).is_ok());
        assert!(mail_to(&a, &Address::Operator).is_ok());
    }

    #[test]
    fn named_mail_to_and_cross_project() {
        let policy = Policy {
            targets: vec![],
            mail_to: vec!["lead".into()],
            cross_project: vec!["q/triage".into()],
        };
        let a = agent("p", &[], policy);
        assert!(mail_to(&a, &peer("p", "lead")).is_ok());
        assert!(mail_to(&a, &peer("p", "other")).is_err());
        assert!(mail_to(&a, &peer("q", "triage")).is_ok());
        let err = mail_to(&a, &peer("q", "other")).unwrap_err();
        assert!(err.message.contains("cross_project"), "{}", err.message);
    }

    #[test]
    fn cross_project_wildcard_covers_the_project() {
        let policy = Policy {
            targets: vec![],
            mail_to: vec!["*".into()],
            cross_project: vec!["q/*".into()],
        };
        let a = agent("p", &[], policy);
        assert!(mail_to(&a, &peer("q", "x")).is_ok());
        assert!(mail_to(&a, &peer("q", "y")).is_ok());
        assert!(mail_to(&a, &peer("r", "x")).is_err());
    }

    #[test]
    fn sysadmin_mails_anyone() {
        let a = agent("p", &[Role::Sysadmin], Policy::default());
        assert!(mail_to(&a, &peer("q", "x")).is_ok());
    }

    #[test]
    fn roles_table() {
        let member = Actor::Agent(agent("p", &[], Policy::default()));
        let admin = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        let sys = Actor::Agent(agent("p", &[Role::Sysadmin], Policy::default()));
        assert!(admin_project(&member, "p", "x").is_err());
        assert!(admin_project(&admin, "p", "x").is_ok());
        assert!(admin_project(&admin, "q", "x").is_err());
        assert!(admin_project(&sys, "q", "x").is_ok());
        assert!(sysadmin(&admin, "x").is_err());
        assert!(sysadmin(&sys, "x").is_ok());
        assert!(read_project(&member, "p").is_ok());
        assert!(read_project(&member, "q").is_err());
        assert!(read_inbox(&member, "p").is_err());
        assert!(read_inbox(&admin, "p").is_ok());
        // Grants.
        assert!(change_role(&admin, "other", "p", Role::ProjectAdmin).is_ok());
        assert!(change_role(&admin, "other", "q", Role::ProjectAdmin).is_err());
        assert!(change_role(&sys, "other", "q", Role::Sysadmin).is_err());
        assert!(change_role(&Actor::Operator, "other", "q", Role::Sysadmin).is_ok());
        assert!(change_role(&admin, "p-id", "p", Role::ProjectAdmin).is_err());
    }

    #[test]
    fn knowledge_rules() {
        let member = Actor::Agent(agent("p", &[], Policy::default()));
        let admin = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        // Authors edit their own memories; admins edit any.
        assert!(edit_memory(&member, "p", "p-id", "MEM-1").is_ok());
        assert!(edit_memory(&member, "p", "someone", "MEM-1").is_err());
        assert!(edit_memory(&admin, "p", "someone", "MEM-1").is_ok());
        // Agents supersede; admins and the operator delete.
        assert!(delete_memory(&member, "p").is_err());
        assert!(delete_memory(&admin, "p").is_ok());
        assert!(delete_memory(&Actor::Operator, "p").is_ok());
        assert!(admin_wiki(&member, "p", "x").is_err());
        assert!(review_page(&member, "p", Some("me")).is_ok());
        assert!(review_page(&member, "p", Some("other")).is_err());
        assert!(decide_version(&admin).is_err());
        assert!(decide_version(&Actor::Operator).is_ok());
        assert!(release_version(&member, "p").is_err());
        assert!(release_version(&admin, "p").is_ok());
        assert!(prune_artifacts(&admin).is_err());
    }
}
