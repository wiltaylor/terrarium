//! The permission rules of PLAN §10 (roles) and §12 (mail scope).
//!
//! Each check returns `Ok(())` or a `forbidden` error whose message names
//! the rule, so an agent learns the boundary. The operator may do anything.
//! The board rules of §13 live in `store::board`, because they need the card.

use terrarium_core::Role;
use terrarium_core::config::Policy;

use crate::actor::{Actor, AgentActor};
use crate::error::{ApiError, ApiResult};
use crate::store::leases::Standing;
use crate::store::mail::Address;
use crate::store::schedules::Reach;

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

/// Store, list, or delete git credentials (`/api/credentials`). The API
/// never returns a secret.
pub fn manage_credentials(actor: &Actor) -> ApiResult<()> {
    sysadmin(actor, "managing credentials")
}

/// Ask for a git credential through the credential helper
/// (`POST /api/repos/credential`). Agents only: the answer goes to git in
/// the agent's own sandbox, for a repo of the agent's own project.
pub fn credential_helper(actor: &Actor) -> ApiResult<&AgentActor> {
    match actor {
        Actor::Agent(a) => Ok(a),
        Actor::Operator => Err(ApiError::forbidden(
            "the credential helper answers agents only; the operator has the credential store",
        )),
    }
}

/// Clone or update a repo of `repo_project` into the workspace of the agent
/// `agent_id` of `agent_project`. An agent may check out into its own
/// workspace; into another agent's workspace needs `project_admin`. The
/// repo and the agent are in the same project.
pub fn checkout_into(
    actor: &Actor,
    repo_project: &str,
    agent_id: &str,
    agent_project: &str,
) -> ApiResult<()> {
    if repo_project != agent_project {
        return Err(ApiError::forbidden(format!(
            "the repo is in project `{repo_project}`; the agent is in `{agent_project}`"
        )));
    }
    read_project(actor, repo_project)?;
    match actor {
        Actor::Agent(a) if a.id == agent_id => Ok(()),
        _ => admin_project(
            actor,
            agent_project,
            "checking out into another agent's workspace",
        ),
    }
}

/// Attach a repo whose URL is a local path. The sandbox binds a local repo
/// under `TERRARIUM_REPOS_DIR` read-write into each agent of each project
/// that has it, so a `project_admin` could reach the repo of another
/// project. Only a `sysadmin` or the operator attaches one. `roles` are the
/// roles of the calling agent; the operator and the system pass no check.
pub fn attach_local_repo(roles: &[Role]) -> ApiResult<()> {
    if roles.contains(&Role::Sysadmin) {
        Ok(())
    } else {
        Err(deny(
            "attaching a repo with a local path URL",
            "sysadmin or the operator",
        ))
    }
}

/// Read the subscription list with quota numbers (`terrarium usage
/// subscriptions`, PLAN §16): the operator, a `sysadmin`, or a
/// `project_admin`.
pub fn read_subscriptions(actor: &Actor) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.is_sysadmin() || a.roles.contains(&Role::ProjectAdmin) => Ok(()),
        Actor::Agent(_) => Err(deny(
            "reading all subscriptions",
            "project_admin, sysadmin, or the operator",
        )),
    }
}

/// Read the model list of a subscription: the operator, a `sysadmin`, or
/// a `project_admin` of a project with an agent on it (`projects`).
pub fn read_subscription_models(actor: &Actor, projects: &[String]) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.is_sysadmin() || projects.iter().any(|p| a.is_admin_of(p)) => Ok(()),
        Actor::Agent(_) => Err(deny(
            "reading the models of a subscription",
            "project_admin of a project that uses it, sysadmin, or the operator",
        )),
    }
}

/// Define subscriptions, run a probe, verify a login: `sysadmin` or the
/// operator.
pub fn manage_subscriptions(actor: &Actor, what: &str) -> ApiResult<()> {
    sysadmin(actor, what)
}

/// Logins, logouts, and API keys (PLAN §10: credentials are operator-only;
/// PLAN §16.1: the login terminal is never for agents).
pub fn operator_only(actor: &Actor, what: &str) -> ApiResult<()> {
    if actor.is_operator() {
        Ok(())
    } else {
        Err(deny(what, "the operator"))
    }
}

/// Use a target: exec, shell, copy, and its lifecycle verbs (PLAN §9,
/// §19). The operator may use every target. An agent may use a target that
/// its `policy.targets` lists, or that it holds an active lease on
/// (docs/api.md §17.7), when the target is global or in the agent's own
/// project. The role does not widen this: a sysadmin agent also needs the
/// name in its policy or a lease. `lease` is the agent's lease standing on
/// the target.
pub fn use_target(
    actor: &Actor,
    target: &str,
    target_project: Option<&str>,
    lease: Standing,
) -> ApiResult<()> {
    let Actor::Agent(a) = actor else {
        return Ok(());
    };
    if let Some(p) = target_project
        && p != a.project
    {
        return Err(ApiError::forbidden(format!(
            "target `{target}` belongs to project `{p}`; an agent uses global targets and those of its own project"
        )));
    }
    if a.policy.targets.iter().any(|t| t == target) {
        return Ok(());
    }
    match lease {
        Standing::Active => Ok(()),
        Standing::Ended => Err(ApiError::forbidden(format!(
            "your lease on {target} ended; ask your lead to grant it again"
        ))),
        Standing::None => Err(ApiError::forbidden(format!(
            "target `{target}` is not in your policy.targets, and you hold no lease on it; \
             ask a project admin to add it or to grant a lease"
        ))),
    }
}

/// Grant, extend, or revoke a lease on a target (docs/api.md §17.7). The
/// operator and a sysadmin may lease any target to any agent. A
/// project_admin may lease a target of its own project to an agent of its
/// own project; a global target needs a sysadmin, as a global target in a
/// policy does (F13).
pub fn lease_target(
    actor: &Actor,
    target: &str,
    target_project: Option<&str>,
    agent_project: &str,
) -> ApiResult<()> {
    let Actor::Agent(a) = actor else {
        return Ok(());
    };
    if a.is_sysadmin() {
        return Ok(());
    }
    let Some(p) = target_project else {
        return Err(deny(
            &format!("leasing the global target `{target}`"),
            "sysadmin or the operator",
        ));
    };
    if !a.is_admin_of(p) {
        return Err(deny(
            &format!("leasing target `{target}`"),
            &format!("project_admin of `{p}`, sysadmin, or the operator"),
        ));
    }
    if agent_project != p {
        return Err(deny(
            &format!("leasing target `{target}` to an agent of project `{agent_project}`"),
            "sysadmin or the operator",
        ));
    }
    Ok(())
}

/// The entries that a policy write adds to the policy of an agent of
/// `project` and that need a `sysadmin` or the operator
/// (`docs/security-review.md` F13): a target that is not a target of
/// `project` (a global target, or a name that no target has yet), a
/// `cross_project` entry, and a `mail_to` entry that names another
/// project. `old` is the policy now (`None` for a new agent); an entry
/// that it has already stays. `project_target` says whether a name is a
/// target of `project`.
pub fn policy_grants(
    project: &str,
    old: Option<&Policy>,
    new: &Policy,
    project_target: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    let had =
        |list: fn(&Policy) -> &Vec<String>, e: &String| old.is_some_and(|o| list(o).contains(e));
    let mut out = Vec::new();
    for t in &new.targets {
        if !had(|p| &p.targets, t) && !project_target(t) {
            out.push(format!(
                "target `{t}` (not a target of project `{project}`)"
            ));
        }
    }
    for c in &new.cross_project {
        if !had(|p| &p.cross_project, c) {
            out.push(format!("cross_project `{c}`"));
        }
    }
    for m in &new.mail_to {
        if let Some((p, _)) = m.split_once('/')
            && p != project
            && !had(|p| &p.mail_to, m)
        {
            out.push(format!("mail_to `{m}` (outside project `{project}`)"));
        }
    }
    out
}

/// Write `new` into the policy of an agent of `project` whose policy is
/// `old` (PLAN §10). The caller checked `project_admin` of `project`. A
/// global target, a `cross_project` entry, or a `mail_to` entry outside the
/// project needs a `sysadmin` or the operator ([`policy_grants`]).
pub fn set_policy(
    actor: &Actor,
    project: &str,
    old: Option<&Policy>,
    new: &Policy,
    project_target: &dyn Fn(&str) -> bool,
) -> ApiResult<()> {
    let grants = policy_grants(project, old, new, project_target);
    grant_policy(actor, &grants)
}

/// The check of [`set_policy`] on the list of [`policy_grants`].
pub fn grant_policy(actor: &Actor, grants: &[String]) -> ApiResult<()> {
    if grants.is_empty() {
        return Ok(());
    }
    sysadmin(
        actor,
        &format!("adding {} to an agent's policy", grants.join(", ")),
    )
}

/// Define, change, or delete a target: a global target needs sysadmin; a
/// project target needs project_admin of that project (PLAN §10).
pub fn define_target(actor: &Actor, project: Option<&str>) -> ApiResult<()> {
    match project {
        None => sysadmin(actor, "defining a global target"),
        Some(p) => admin_project(actor, p, "defining a project target"),
    }
}

/// Register or revoke host agents: the operator only. Each registration
/// makes a host agent token, and PLAN §10 keeps "manage credentials and
/// host agent tokens" for the operator. A sysadmin agent may list hosts.
pub fn manage_hosts(actor: &Actor, what: &str) -> ApiResult<()> {
    operator_only(actor, what)
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
        Address::System => return Err(ApiError::invalid("`system` only sends mail")),
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

// ---- schedules (docs/api.md §19) ----

/// Read the schedules of `project`, or the global schedules (`None`):
/// members read their own project's; a global schedule needs sysadmin.
pub fn read_schedules(actor: &Actor, project: Option<&str>) -> ApiResult<()> {
    match project {
        Some(p) => read_project(actor, p),
        None => sysadmin(actor, "reading the global schedules"),
    }
}

/// The first half of [`manage_schedule`], without the action: the
/// operator and a sysadmin anywhere, a project_admin in its project.
pub fn schedule_scope(actor: &Actor, project: Option<&str>) -> ApiResult<()> {
    let Actor::Agent(a) = actor else {
        return Ok(());
    };
    if a.is_sysadmin() {
        return Ok(());
    }
    let Some(p) = project else {
        return Err(deny(
            "managing a global schedule",
            "sysadmin or the operator",
        ));
    };
    if !a.is_admin_of(p) {
        return Err(deny(
            &format!("managing the schedules of project `{p}`"),
            &format!("project_admin of `{p}`, sysadmin, or the operator"),
        ));
    }
    Ok(())
}

/// Create, change, delete, or run a schedule of `project` (`None`:
/// global) whose action touches `reach`. The daemon checks this for the
/// caller of each write, and again for the schedule's `run_as` at each
/// run.
///
/// - The operator and a sysadmin: any schedule.
/// - A global schedule: sysadmin.
/// - A project_admin of the project: the action stays inside it. Mail to
///   an agent of the project, `project:<project>`, or `operator`; a turn
///   or an agent verb on an agent of the project; an exec on a global
///   target or one of the project (the executor checks the policy and the
///   leases at run time); a probe of a subscription that an agent of the
///   project uses; a card on the project's board.
/// - A member: none (members read only).
pub fn manage_schedule(actor: &Actor, project: Option<&str>, reach: &Reach) -> ApiResult<()> {
    schedule_scope(actor, project)?;
    let (Actor::Agent(a), Some(p)) = (actor, project) else {
        return Ok(());
    };
    if a.is_sysadmin() {
        return Ok(());
    }
    let outside = |what: String| {
        Err(ApiError::forbidden(format!(
            "a schedule of project `{p}` stays inside it: {what}; that needs sysadmin or the operator"
        )))
    };
    match reach {
        Reach::Mail(Address::Operator) => Ok(()),
        Reach::Mail(Address::Project(q)) if q == p => Ok(()),
        Reach::Mail(Address::Agent { project: q, .. }) if q == p => Ok(()),
        Reach::Mail(to) => outside(format!("mail to `{}` leaves the project", to.display())),
        Reach::Agent { project: q, .. } if q == p => Ok(()),
        Reach::Agent {
            name, project: q, ..
        } => outside(format!("agent `{q}/{name}` is in another project")),
        Reach::Target { project: None, .. } => Ok(()),
        Reach::Target {
            project: Some(q), ..
        } if q == p => Ok(()),
        Reach::Target {
            name,
            project: Some(q),
        } => outside(format!("target `{name}` belongs to project `{q}`")),
        Reach::Subscription { projects, .. } if projects.iter().any(|q| q == p) => Ok(()),
        Reach::Subscription { name, .. } => outside(format!(
            "no agent of the project uses subscription `{name}`"
        )),
        Reach::Card { project: q } if q == p => Ok(()),
        Reach::Card { project: q } => outside(format!("the board of `{q}` is another project's")),
    }
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

    /// F13: a project_admin sets project targets and mail_to in its
    /// project; a global target and cross_project need sysadmin.
    #[test]
    fn policy_writes_by_role() {
        let member = Actor::Agent(agent("p", &[], Policy::default()));
        let admin = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        let sys = Actor::Agent(agent("q", &[Role::Sysadmin], Policy::default()));
        let local = |n: &str| n == "p-box";
        let policy = |targets: &[&str], mail: &[&str], cross: &[&str]| Policy {
            targets: targets.iter().map(|s| s.to_string()).collect(),
            mail_to: mail.iter().map(|s| s.to_string()).collect(),
            cross_project: cross.iter().map(|s| s.to_string()).collect(),
        };
        let in_project = policy(&["p-box"], &["*", "lead", "p/dev"], &[]);
        assert!(set_policy(&admin, "p", None, &in_project, &local).is_ok());
        assert!(policy_grants("p", None, &in_project, &local).is_empty());
        for bad in [
            policy(&["buildbox"], &["*"], &[]),
            policy(&[], &["*"], &["ops/*"]),
            policy(&[], &["ops/dev"], &[]),
        ] {
            let err = set_policy(&admin, "p", None, &bad, &local).unwrap_err();
            assert!(err.message.contains("sysadmin"), "{}", err.message);
            assert!(set_policy(&sys, "p", None, &bad, &local).is_ok());
            assert!(set_policy(&Actor::Operator, "p", None, &bad, &local).is_ok());
            // An entry that the policy has already may stay.
            assert!(set_policy(&admin, "p", Some(&bad), &bad, &local).is_ok());
            let mut more = bad.clone();
            more.mail_to.push("lead".into());
            assert!(set_policy(&admin, "p", Some(&bad), &more, &local).is_ok());
        }
        // The grants list names each entry; a member has no admin right, but
        // the caller checks that first.
        let g = policy_grants("p", None, &policy(&["buildbox"], &[], &["ops/x"]), &local);
        assert_eq!(g.len(), 2, "{g:?}");
        assert!(admin_project(&member, "p", "x").is_err());
    }

    /// Leases: the policy or an active lease lets an agent use a target;
    /// an ended lease names itself. A project_admin leases its project's
    /// targets to its project's agents; a global target needs sysadmin.
    #[test]
    fn leases_widen_use_and_follow_the_roles() {
        let worker = Actor::Agent(agent("p", &[], Policy::default()));
        assert!(use_target(&worker, "vm", Some("p"), Standing::Active).is_ok());
        assert!(use_target(&worker, "vm", None, Standing::Active).is_ok());
        let e = use_target(&worker, "vm", Some("p"), Standing::Ended).unwrap_err();
        assert!(
            e.message.contains("your lease on vm ended"),
            "{}",
            e.message
        );
        let e = use_target(&worker, "vm", Some("p"), Standing::None).unwrap_err();
        assert!(e.message.contains("policy.targets"), "{}", e.message);
        // A lease does not cross projects.
        assert!(use_target(&worker, "vm", Some("q"), Standing::Active).is_err());
        let admin = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        let sys = Actor::Agent(agent("q", &[Role::Sysadmin], Policy::default()));
        assert!(lease_target(&admin, "vm", Some("p"), "p").is_ok());
        let e = lease_target(&admin, "g", None, "p").unwrap_err();
        assert!(e.message.contains("sysadmin"), "{}", e.message);
        assert!(lease_target(&admin, "vm", Some("p"), "q").is_err());
        assert!(lease_target(&admin, "vm", Some("q"), "q").is_err());
        assert!(lease_target(&worker, "vm", Some("p"), "p").is_err());
        assert!(lease_target(&sys, "g", None, "p").is_ok());
        assert!(lease_target(&Actor::Operator, "g", None, "p").is_ok());
    }

    /// Schedules: the operator and sysadmins anywhere; a project_admin
    /// inside its project; a member reads only.
    #[test]
    fn schedule_rights_per_role_and_action() {
        let member = Actor::Agent(agent("p", &[], Policy::default()));
        let admin = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        let sys = Actor::Agent(agent("q", &[Role::Sysadmin], Policy::default()));
        let mine = Reach::Agent {
            id: "p-w".into(),
            name: "w".into(),
            project: "p".into(),
        };
        let theirs = Reach::Agent {
            id: "q-w".into(),
            name: "w".into(),
            project: "q".into(),
        };
        let reaches_in = [
            Reach::Mail(Address::Operator),
            Reach::Mail(Address::Project("p".into())),
            Reach::Mail(peer("p", "w")),
            mine.clone(),
            Reach::Target {
                name: "g".into(),
                project: None,
            },
            Reach::Target {
                name: "p-box".into(),
                project: Some("p".into()),
            },
            Reach::Subscription {
                name: "s".into(),
                projects: vec!["o".into(), "p".into()],
            },
        ];
        let reaches_out = [
            Reach::Mail(peer("q", "w")),
            Reach::Mail(Address::Project("q".into())),
            theirs.clone(),
            Reach::Target {
                name: "q-box".into(),
                project: Some("q".into()),
            },
            Reach::Subscription {
                name: "s".into(),
                projects: vec!["q".into()],
            },
        ];
        for r in &reaches_in {
            assert!(manage_schedule(&admin, Some("p"), r).is_ok(), "{r:?}");
            assert!(manage_schedule(&member, Some("p"), r).is_err(), "{r:?}");
            // Another project's admin rights do not count.
            assert!(manage_schedule(&admin, Some("q"), r).is_err(), "{r:?}");
        }
        for r in &reaches_out {
            let e = manage_schedule(&admin, Some("p"), r).unwrap_err();
            assert_eq!(e.code, crate::error::Code::Forbidden, "{r:?}");
            assert!(e.message.contains("stays inside"), "{}", e.message);
            assert!(manage_schedule(&sys, Some("p"), r).is_ok(), "{r:?}");
            assert!(manage_schedule(&Actor::Operator, Some("p"), r).is_ok());
        }
        // Global schedules: sysadmin or the operator.
        assert!(manage_schedule(&admin, None, &mine).is_err());
        assert!(manage_schedule(&sys, None, &theirs).is_ok());
        assert!(manage_schedule(&Actor::Operator, None, &theirs).is_ok());
        // Reads: members read their project; the global list needs sysadmin.
        assert!(read_schedules(&member, Some("p")).is_ok());
        assert!(read_schedules(&member, Some("q")).is_err());
        assert!(read_schedules(&admin, None).is_err());
        assert!(read_schedules(&sys, None).is_ok());
    }

    #[test]
    fn host_tokens_are_for_the_operator() {
        let sys = Actor::Agent(agent("p", &[Role::Sysadmin], Policy::default()));
        assert!(manage_hosts(&Actor::Operator, "x").is_ok());
        let err = manage_hosts(&sys, "registering a host agent").unwrap_err();
        assert!(err.message.contains("the operator"), "{}", err.message);
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
