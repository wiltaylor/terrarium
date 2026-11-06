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
        Actor::Agent(a) if a.project == project || a.reads_all() => Ok(()),
        Actor::Agent(_) => Err(ApiError::forbidden(format!(
            "project `{project}` is not your project; reading it needs sysadmin or architect"
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

/// The project gate of a write (TER-47): the own project, a sysadmin, or
/// the operator. It is what [`read_project`] was before the architect: an
/// architect reads every project but writes only in its own, as a member.
pub fn write_project(actor: &Actor, project: &str) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.project == project || a.is_sysadmin() => Ok(()),
        Actor::Agent(_) => Err(ApiError::forbidden(format!(
            "project `{project}` is not your project; writing in it needs sysadmin"
        ))),
    }
}

/// A read of the whole system that a `sysadmin` and an `architect` may do
/// (TER-47): the global event sources, the users of a skill, the state of
/// the self-update.
pub fn read_system(actor: &Actor, what: &str) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.reads_all() => Ok(()),
        Actor::Agent(_) => Err(deny(what, "sysadmin, architect, or the operator")),
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

/// How far an actor may write a calendar (docs/api.md §33.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarWrite {
    /// Add, change, and remove any event: the operator, a sysadmin, and a
    /// project_admin (a lead) of the project.
    Any,
    /// Add events, and change and remove the own ones: an agent of the
    /// project with the calendar grant (`policy.calendar_write`).
    Own,
}

/// Read a calendar: a project calendar is a project read; the global
/// calendar is for every agent and the operator.
pub fn read_calendar(actor: &Actor, project: Option<&str>) -> ApiResult<()> {
    match project {
        Some(p) => read_project(actor, p),
        None => Ok(()),
    }
}

/// Write a calendar (`project` `None`: the global one). A global event
/// needs a sysadmin or the operator. A project event needs a lead of the
/// project (project_admin), a sysadmin, the operator, or an agent of the
/// project with the calendar grant that a lead gives.
pub fn write_calendar(actor: &Actor, project: Option<&str>) -> ApiResult<CalendarWrite> {
    let Some(p) = project else {
        sysadmin(actor, "writing the global calendar")?;
        return Ok(CalendarWrite::Any);
    };
    if is_admin_of(actor, p) {
        return Ok(CalendarWrite::Any);
    }
    match actor {
        Actor::Agent(a) if a.project == p && a.policy.calendar_write => Ok(CalendarWrite::Own),
        Actor::Agent(a) if a.project == p => Err(ApiError::forbidden(format!(
            "writing the calendar of `{p}` needs the calendar grant (`terrarium calendar grant \
             {}`) from a lead (project_admin), or project_admin; ask your lead to add the \
             event for you",
            a.name
        ))),
        _ => Err(deny(
            &format!("writing the calendar of `{p}`"),
            &format!("project_admin of `{p}`, sysadmin, or the operator"),
        )),
    }
}

/// Change or remove one event of `project` that `created_by` (an actor
/// label) made: [`write_calendar`], and with the grant only, the own
/// events.
pub fn change_event(actor: &Actor, project: Option<&str>, created_by: &str) -> ApiResult<()> {
    match write_calendar(actor, project)? {
        CalendarWrite::Any => Ok(()),
        CalendarWrite::Own if actor.label() == created_by => Ok(()),
        CalendarWrite::Own => Err(ApiError::forbidden(
            "with the calendar grant you change and remove only the events that you made; \
             ask your lead to change this one",
        )),
    }
}

/// Store, list, show, or delete the secrets of a scope (PLAN §16b.5):
/// `global` needs sysadmin or the operator; a project needs project_admin
/// of that project, sysadmin, or the operator. The API never returns a
/// secret field (a reveal is [`reveal_secret`]).
pub fn manage_secrets(actor: &Actor, scope: &str) -> ApiResult<()> {
    if scope == crate::secrets::GLOBAL {
        sysadmin(actor, "managing global secrets")
    } else {
        admin_project(actor, scope, "managing the secrets of a project")
    }
}

/// Show the value of a secret (`POST /api/secrets/{scope}/{name}/reveal`):
/// the operator only, with the operator password (PLAN §16b, S4).
pub fn reveal_secret(actor: &Actor) -> ApiResult<()> {
    operator_only(actor, "revealing a secret")
}

/// Grant a secret, revoke a grant, or list the grants of a secret
/// (PLAN §16b.3): the same rule as [`manage_secrets`]. The store checks
/// that a project secret goes only to its own project.
pub fn grant_secret(actor: &Actor, scope: &str) -> ApiResult<()> {
    if scope == crate::secrets::GLOBAL {
        sysadmin(actor, "granting a global secret")
    } else {
        admin_project(actor, scope, "granting a secret of a project")
    }
}

/// Read the own granted secrets (`/api/agent/secrets`): agents only.
pub fn agent_secrets(actor: &Actor) -> ApiResult<&AgentActor> {
    match actor {
        Actor::Agent(a) => Ok(a),
        Actor::Operator => Err(ApiError::forbidden(
            "/api/agent/secrets answers agents only; the operator uses /api/secrets",
        )),
    }
}

/// Ask for a git credential through the credential helper
/// (`POST /api/repos/credential`). Agents only: the answer goes to git in
/// the agent's own sandbox, for a repo of the agent's own project.
pub fn credential_helper(actor: &Actor) -> ApiResult<&AgentActor> {
    match actor {
        Actor::Agent(a) => Ok(a),
        Actor::Operator => Err(ApiError::forbidden(
            "the credential helper answers agents only; the operator has the secret store",
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
    write_project(actor, repo_project)?;
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
        Actor::Agent(a) if a.reads_all() || a.roles.contains(&Role::ProjectAdmin) => Ok(()),
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
        Actor::Agent(a) if a.reads_all() || projects.iter().any(|p| a.is_admin_of(p)) => Ok(()),
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

/// Write the `spawn` block of an agent's policy (PLAN §18, "Ephemeral
/// agents"): the right to spawn ephemeral agents. The caller checked
/// `project_admin` of the agent's project. `target` is the agent (`None`
/// for a new agent), `target_roles` its roles after the write, `old` its
/// policy now. No change of the block: no check.
///
/// - The block must be valid ([`terrarium_core::config::SpawnPolicy::check`]).
/// - The operator and a sysadmin may write any block.
/// - A project_admin may write the block of a member (an agent with no
///   role above `member`) of its project, with templates of the project
///   or global ones (the entries name nothing else). It may not write its
///   own block or the block of an admin, and it may not set
///   `may_spawn_children`.
pub fn set_spawn_policy(
    actor: &Actor,
    target: Option<&str>,
    target_roles: &[Role],
    old: Option<&Policy>,
    new: &Policy,
) -> ApiResult<()> {
    let before = old.and_then(|o| o.spawn.as_ref());
    if before == new.spawn.as_ref() {
        return Ok(());
    }
    if let Some(sp) = &new.spawn {
        sp.check().map_err(ApiError::invalid)?;
    }
    let Actor::Agent(a) = actor else {
        return Ok(());
    };
    if a.is_sysadmin() {
        return Ok(());
    }
    if target.is_some_and(|t| t == a.id) {
        return Err(deny(
            "changing your own spawn rights",
            "sysadmin or the operator",
        ));
    }
    if target_roles
        .iter()
        .any(|r| matches!(r, Role::ProjectAdmin | Role::Sysadmin))
    {
        return Err(deny(
            "setting the spawn rights of a project_admin or sysadmin",
            "sysadmin or the operator",
        ));
    }
    if new.spawn.as_ref().is_some_and(|s| s.may_spawn_children)
        && !before.is_some_and(|b| b.may_spawn_children)
    {
        return Err(deny(
            "setting spawn.may_spawn_children",
            "sysadmin or the operator",
        ));
    }
    Ok(())
}

/// Write `policy.thread_open` of an agent: the right `thread:open` to open
/// Inbox threads (docs/api.md §6.13, TER-93). The rules are those of the
/// spawn rights ([`set_spawn_policy`]): the operator and a sysadmin set it
/// on any agent; a project_admin (the caller checked it) sets it on a
/// member of its project, not on itself and not on an admin. No change: no
/// check.
pub fn set_thread_open(
    actor: &Actor,
    target: Option<&str>,
    target_roles: &[Role],
    old: Option<&Policy>,
    new: &Policy,
) -> ApiResult<()> {
    if old.is_some_and(|o| o.thread_open) == new.thread_open {
        return Ok(());
    }
    let Actor::Agent(a) = actor else {
        return Ok(());
    };
    if a.is_sysadmin() {
        return Ok(());
    }
    if target.is_some_and(|t| t == a.id) {
        return Err(deny(
            "changing your own right thread:open",
            "sysadmin or the operator",
        ));
    }
    if target_roles
        .iter()
        .any(|r| matches!(r, Role::ProjectAdmin | Role::Sysadmin))
    {
        return Err(deny(
            "setting thread:open on a project_admin or sysadmin",
            "sysadmin or the operator",
        ));
    }
    Ok(())
}

/// Open an Inbox thread in `scope` (`global` or a project; docs/api.md
/// §6.13). The operator opens any thread. An agent needs the right
/// `thread:open` (`policy.thread_open`), and opens a thread of its own
/// project or a `global` one; another project needs sysadmin, as each
/// write does (TER-47: an architect writes only in its own project). The
/// template check (A1) is [`thread_rights`].
pub fn open_thread(actor: &Actor, scope: &str) -> ApiResult<()> {
    let Actor::Agent(a) = actor else {
        return Ok(());
    };
    if !a.policy.thread_open {
        return Err(ApiError::forbidden(
            "opening a thread needs the right thread:open (policy.thread_open); a lead grants \
             it with `terrarium agent set <agent> --thread-open on`",
        ));
    }
    if scope == crate::store::threads::GLOBAL {
        return Ok(());
    }
    write_project(actor, scope)
        .map_err(|e| ApiError::forbidden(format!("opening a `{scope}` thread: {}", e.message)))
}

/// The rights of the attached agent of a thread: from its template, or of
/// the agent that the opener names.
#[derive(Debug, Clone, Copy)]
pub struct ThreadAgentRights<'a> {
    /// The project that the agent is (or will be) in.
    pub project: &'a str,
    pub roles: &'a [Role],
    pub policy: &'a Policy,
}

/// The rights of `r` that the opening agent `a` does not hold (A1 of the
/// plan `inbox-threads`, the rule of TER-65 for spawned agents): an
/// agent-opened thread may only use a template whose rights do not exceed
/// the opener's. Empty: the open may go on. A sysadmin holds every right.
///
/// - A role: `project_admin` of `r.project` needs the opener to be its
///   project_admin; `architect` needs the opener to hold `architect`;
///   `sysadmin` needs a sysadmin.
/// - A target: in the opener's `policy.targets`, or a target of
///   `r.project` when the opener administers that project.
/// - A `cross_project` entry: in the opener's `cross_project`. A `mail_to`
///   entry of another project than `r.project`: in the opener's `mail_to`,
///   covered by its `cross_project`, or of the opener's own project.
/// - A `spawn` block: the opener administers `r.project` and `r` is a
///   member there with no `may_spawn_children` (what a lead may grant), or
///   the opener's own spawn block has each template entry (or `*`), and
///   `may_spawn_children` when `r` has it.
/// - `calendar_write`: the opener has it, or administers `r.project`.
pub fn thread_rights(
    a: &AgentActor,
    r: ThreadAgentRights<'_>,
    project_target: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    if a.is_sysadmin() {
        return Vec::new();
    }
    let admin = a.is_admin_of(r.project);
    let mut out = Vec::new();
    for role in r.roles {
        let held = match role {
            Role::Member => true,
            Role::ProjectAdmin => admin,
            Role::Architect => a.roles.contains(&Role::Architect),
            Role::Sysadmin => false,
        };
        if !held {
            let role = match role {
                Role::ProjectAdmin => format!("role project_admin of `{}`", r.project),
                Role::Architect => "role architect".to_owned(),
                _ => "role sysadmin".to_owned(),
            };
            out.push(role);
        }
    }
    let mine = &a.policy;
    for t in &r.policy.targets {
        if !mine.targets.contains(t) && !(admin && project_target(t)) {
            out.push(format!("target `{t}`"));
        }
    }
    for c in &r.policy.cross_project {
        if !mine.cross_project.contains(c) {
            out.push(format!("cross_project `{c}`"));
        }
    }
    for m in &r.policy.mail_to {
        let Some((p, _)) = m.split_once('/') else {
            continue;
        };
        let covered = p == r.project
            || p == a.project
            || mine.mail_to.contains(m)
            || mine.cross_project.contains(m)
            || mine.cross_project.contains(&format!("{p}/*"));
        if !covered {
            out.push(format!("mail_to `{m}`"));
        }
    }
    if let Some(sp) = &r.policy.spawn {
        let member = !r
            .roles
            .iter()
            .any(|x| matches!(x, Role::ProjectAdmin | Role::Sysadmin));
        let lead_grant = admin && member && !sp.may_spawn_children;
        let own = mine.spawn.as_ref().is_some_and(|o| {
            sp.templates
                .iter()
                .all(|e| o.templates.iter().any(|m| m == e || m == "*"))
                && (!sp.may_spawn_children || o.may_spawn_children)
        });
        if !lead_grant && !own {
            out.push("spawn rights".to_owned());
        }
    }
    if r.policy.calendar_write && !mine.calendar_write && !admin {
        out.push("calendar_write".to_owned());
    }
    out
}

/// Spawn an ephemeral agent for `owner` of `project` (PLAN §18,
/// "Ephemeral agents"): the owner itself, a `project_admin` of the
/// project, a sysadmin, or the operator. The owner's `spawn` policy
/// then limits the spawn ([`crate::spawn`]).
pub fn spawn_for(actor: &Actor, owner: &str, project: &str) -> ApiResult<()> {
    if let Actor::Agent(a) = actor
        && a.id == owner
    {
        return Ok(());
    }
    admin_project(
        actor,
        project,
        "spawning an ephemeral agent for another agent",
    )
}

/// Cancel an ephemeral agent: its owner, a `project_admin` of its
/// project, a sysadmin, or the operator.
pub fn cancel_spawn(actor: &Actor, owner: &str, project: &str) -> ApiResult<()> {
    if let Actor::Agent(a) = actor
        && a.id == owner
    {
        return Ok(());
    }
    admin_project(actor, project, "cancelling an ephemeral agent")
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

/// View a project inbox (list and show, never mark read or reply): its
/// admin, or a sysadmin or architect (TER-47).
pub fn view_inbox(actor: &Actor, project: &str) -> ApiResult<()> {
    match actor {
        Actor::Agent(a) if a.reads_all() => Ok(()),
        _ => read_inbox(actor, project),
    }
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
        Role::Sysadmin | Role::Architect if actor.is_operator() => Ok(()),
        Role::Sysadmin => Err(deny("granting or revoking sysadmin", "the operator")),
        Role::Architect => Err(deny("granting or revoking architect", "the operator")),
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

// ---- waits and watches (PLAN §12d, docs/api.md §6.7) ----

/// Create a wait or a watch for `owner` (an agent ULID): the agent itself
/// or the operator.
pub fn own_waits(actor: &Actor, owner: &str, what: &str) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.id == owner => Ok(()),
        Actor::Agent(_) => Err(ApiError::forbidden(format!(
            "{what}: an agent manages only its own waits and watches"
        ))),
    }
}

/// See or cancel the waits and watches of `owner`, an agent of `project`:
/// the agent itself, a project_admin of the project, a sysadmin, or the
/// operator.
pub fn manage_waits(actor: &Actor, owner: &str, project: &str, what: &str) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.id == owner || a.is_admin_of(project) => Ok(()),
        Actor::Agent(_) => Err(deny(
            what,
            &format!("the agent itself, project_admin of `{project}`, or the operator"),
        )),
    }
}

// ---- questions to the operator (PLAN §12e) ----

/// Ask, update, or retract a question of `owner` (an agent ULID of
/// `project`): only the agent itself, and only a leader (project_admin of
/// its project, or sysadmin). The operator retracts with
/// [`retract_question`].
pub fn ask_question(actor: &Actor, owner: &str, project: &str, what: &str) -> ApiResult<()> {
    match actor {
        Actor::Agent(a) if a.id == owner && a.is_admin_of(project) => Ok(()),
        Actor::Agent(a) if a.id == owner => Err(ApiError::forbidden(format!(
            "{what} needs a leader (project_admin or sysadmin); workers send questions to \
             their lead: terrarium mail send <lead> -s \"question: ...\" -"
        ))),
        Actor::Agent(_) => Err(ApiError::forbidden(format!(
            "{what}: an agent asks and changes only its own questions"
        ))),
        Actor::Operator => Err(ApiError::forbidden(format!(
            "{what}: the operator answers or retracts questions; an agent asks them"
        ))),
    }
}

/// Ask a question in an Inbox thread (TER-91): the agent itself, also when
/// it is not a leader. The store checks that the thread is its own.
pub fn ask_in_thread(actor: &Actor, owner: &str, what: &str) -> ApiResult<()> {
    match actor {
        Actor::Agent(a) if a.id == owner => Ok(()),
        Actor::Agent(_) => Err(ApiError::forbidden(format!(
            "{what}: an agent asks only its own questions"
        ))),
        Actor::Operator => Err(ApiError::forbidden(format!(
            "{what}: the operator posts to a thread (POST /api/threads/{{id}}/messages); an \
             agent asks in it"
        ))),
    }
}

/// Link a question of `owner` (an agent of `project`) to a card, or
/// unlink it: the operator or a sysadmin at any time (`Ok(true)`), the
/// asking leader while the question is open (`Ok(false)`).
pub fn link_question(actor: &Actor, owner: &str, project: &str) -> ApiResult<bool> {
    match actor {
        Actor::Operator => Ok(true),
        Actor::Agent(a) if a.id != owner && a.is_sysadmin() => Ok(true),
        _ => ask_question(actor, owner, project, "linking a question to a card").map(|()| false),
    }
}

/// Read the questions of `owner`, an agent of `project`: the agent
/// itself, a project_admin of the project, a sysadmin, or the operator.
pub fn read_questions(actor: &Actor, owner: &str, project: &str) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.id == owner || a.is_admin_of(project) || a.reads_all() => Ok(()),
        Actor::Agent(_) => Err(deny(
            "reading questions",
            &format!("the agent itself, project_admin of `{project}`, sysadmin, or the operator"),
        )),
    }
}

/// Answer a question of `owner`: the operator, or a sysadmin agent for
/// the question of another agent. Never the asking agent.
pub fn answer_question(actor: &Actor, owner: &str) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        Actor::Agent(a) if a.id == owner => Err(ApiError::forbidden(
            "an agent cannot answer its own question; the operator answers it",
        )),
        Actor::Agent(a) if a.is_sysadmin() => Ok(()),
        Actor::Agent(_) => Err(deny("answering a question", "sysadmin or the operator")),
    }
}

/// Retract a question of `owner`: the operator, or the asking agent when
/// it is a leader.
pub fn retract_question(actor: &Actor, owner: &str, project: &str) -> ApiResult<()> {
    match actor {
        Actor::Operator => Ok(()),
        _ => ask_question(actor, owner, project, "retracting a question"),
    }
}

/// May the agent `waiter` wait on or watch the agent `name` of `project`?
/// An agent of its own project, yes. Another project needs a
/// `cross_project` entry (`project/name` or `project/*`), as mail does. A
/// sysadmin reaches every agent.
pub fn reach_agent(waiter: &AgentActor, name: &str, project: &str) -> ApiResult<()> {
    if waiter.reads_all() || waiter.project == project {
        return Ok(());
    }
    let full = format!("{project}/{name}");
    let wildcard = format!("{project}/*");
    if waiter
        .policy
        .cross_project
        .iter()
        .any(|e| *e == full || *e == wildcard)
    {
        return Ok(());
    }
    Err(ApiError::forbidden(format!(
        "`{full}` is outside project `{}`; waiting on or watching it needs \"{full}\" in \
         policy.cross_project",
        waiter.project
    )))
}

// ---- agent templates (docs/api.md §21) ----

/// Read the templates of `project`, or the global templates (`None`):
/// members read their own project's; every caller reads the global ones,
/// because every caller that may create an agent may use them.
pub fn read_templates(actor: &Actor, project: Option<&str>) -> ApiResult<()> {
    match project {
        Some(p) => read_project(actor, p),
        None => Ok(()),
    }
}

/// Create, change, or delete a template of `project` (`None`: global):
/// the operator and a sysadmin any; a project_admin those of its
/// project. A template grants nothing: an agent create from it checks
/// the rights on the values it copies.
pub fn manage_templates(actor: &Actor, project: Option<&str>, what: &str) -> ApiResult<()> {
    match project {
        Some(p) => admin_project(actor, p, &format!("managing the templates of `{p}` {what}")),
        None => sysadmin(actor, &format!("managing a global template {what}")),
    }
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
    inside_project(p, reach)
}

/// The action of a schedule of project `p` stays inside it: the rule of
/// [`manage_schedule`] for a project_admin, and for a project-owned
/// schedule (`crate::owner::schedule_reach`, which also needs a grant for
/// a global target).
pub fn inside_project(p: &str, reach: &Reach) -> ApiResult<()> {
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
        // The URL is the project_admin's to choose (the SSRF rules of
        // `crate::webhooks::net` apply to every caller); the secrets of
        // its headers are checked on their own (`webhooks::check_secrets`).
        Reach::Webhook { .. } => Ok(()),
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

/// Read the DB of a project (PLAN §15.4): its types, records, and
/// queries. The same rule as the other project data: the members of the
/// project (its agents too), a sysadmin, and the operator.
pub fn read_db(actor: &Actor, project: &str) -> ApiResult<()> {
    read_project(actor, project)
}

/// Write the DB of a project: create and change types and records, and
/// delete a type with no records or a record. Every member writes, so an
/// agent can make its own types.
pub fn write_db(actor: &Actor, project: &str) -> ApiResult<()> {
    write_project(actor, project).map_err(|_| {
        ApiError::forbidden(format!(
            "project `{project}` is not your project; writing its DB needs sysadmin"
        ))
    })
}

/// Delete a DB type with its `records` records: project_admin of the
/// project, sysadmin, or the operator.
pub fn purge_db_type(actor: &Actor, project: &str, records: i64) -> ApiResult<()> {
    admin_project(
        actor,
        project,
        &format!("deleting a DB type with its {records} records"),
    )
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

/// Upload a test run: every member of the project (its agents too).
pub fn write_test_runs(actor: &Actor, project: &str) -> ApiResult<()> {
    write_project(actor, project).map_err(|_| {
        ApiError::forbidden(format!(
            "project `{project}` is not your project; uploading its test runs needs sysadmin"
        ))
    })
}

/// Delete a test run: its creator, or an admin.
pub fn delete_test_run(actor: &Actor, project: &str, creator: &str) -> ApiResult<()> {
    if actor.address() == creator || is_admin_of(actor, project) {
        return Ok(());
    }
    Err(ApiError::forbidden(
        "only the creator of a test run, a project admin, or the operator deletes it",
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
    fn secrets_follow_their_scope() {
        let member = Actor::Agent(agent("p", &[], Policy::default()));
        let admin = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        let sys = Actor::Agent(agent("q", &[Role::Sysadmin], Policy::default()));
        let op = Actor::Operator;
        // Global: sysadmin and the operator.
        assert!(manage_secrets(&member, "global").is_err());
        assert!(manage_secrets(&admin, "global").is_err());
        assert!(manage_secrets(&sys, "global").is_ok());
        assert!(manage_secrets(&op, "global").is_ok());
        // A project: its project_admin, sysadmin, and the operator.
        assert!(manage_secrets(&member, "p").is_err());
        assert!(manage_secrets(&admin, "p").is_ok());
        assert!(manage_secrets(&admin, "other").is_err());
        assert!(manage_secrets(&sys, "p").is_ok());
        assert!(manage_secrets(&op, "p").is_ok());
        // A reveal: the operator only.
        assert!(reveal_secret(&sys).is_err());
        assert!(reveal_secret(&admin).is_err());
        assert!(reveal_secret(&op).is_ok());
        // Grants: the same matrix.
        for (actor, scope, ok) in [
            (&member, "global", false),
            (&admin, "global", false),
            (&sys, "global", true),
            (&op, "global", true),
            (&member, "p", false),
            (&admin, "p", true),
            (&admin, "other", false),
            (&sys, "p", true),
            (&op, "p", true),
        ] {
            assert_eq!(grant_secret(actor, scope).is_ok(), ok, "{actor:?} {scope}");
        }
        // The own grants: agents only.
        assert!(agent_secrets(&member).is_ok());
        assert!(agent_secrets(&op).is_err());
    }

    #[test]
    fn waits_reach_the_own_project_and_cross_project_entries() {
        let a = agent("p", &[], Policy::default());
        assert!(reach_agent(&a, "w1", "p").is_ok());
        let e = reach_agent(&a, "w1", "q").unwrap_err();
        assert!(e.message.contains("cross_project"), "{}", e.message);
        let pol = Policy {
            cross_project: vec!["q/w1".into(), "r/*".into()],
            ..Policy::default()
        };
        let b = agent("p", &[], pol);
        assert!(reach_agent(&b, "w1", "q").is_ok());
        assert!(reach_agent(&b, "w2", "q").is_err());
        assert!(reach_agent(&b, "any", "r").is_ok());
        let sys = agent("p", &[Role::Sysadmin], Policy::default());
        assert!(reach_agent(&sys, "x", "z").is_ok());
        // Own waits: the agent itself or the operator; a project_admin
        // sees and cancels those of its project.
        let me = Actor::Agent(a.clone());
        assert!(own_waits(&me, "p-id", "x").is_ok());
        assert!(own_waits(&me, "other", "x").is_err());
        assert!(own_waits(&Actor::Operator, "other", "x").is_ok());
        let lead = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        assert!(manage_waits(&lead, "w-id", "p", "x").is_ok());
        assert!(manage_waits(&lead, "w-id", "q", "x").is_err());
        assert!(manage_waits(&me, "w-id", "p", "x").is_err());
    }

    #[test]
    fn question_rights_per_role() {
        let member = Actor::Agent(agent("p", &[], Policy::default()));
        let lead = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        let other_lead = Actor::Agent(agent("q", &[Role::ProjectAdmin], Policy::default()));
        let sys = Actor::Agent(agent("s", &[Role::Sysadmin], Policy::default()));
        // Ask: a leader for itself only.
        assert!(ask_question(&lead, "p-id", "p", "x").is_ok());
        let e = ask_question(&member, "p-id", "p", "x").unwrap_err();
        assert!(
            e.message.contains("terrarium mail send <lead>"),
            "{}",
            e.message
        );
        assert!(ask_question(&lead, "w-id", "p", "x").is_err());
        assert!(ask_question(&Actor::Operator, "p-id", "p", "x").is_err());
        assert!(ask_question(&sys, "s-id", "s", "x").is_ok());
        // Read: the owner, the project's admins, sysadmin, the operator.
        assert!(read_questions(&member, "p-id", "p").is_ok());
        assert!(read_questions(&member, "w-id", "p").is_err());
        assert!(read_questions(&lead, "w-id", "p").is_ok());
        assert!(read_questions(&other_lead, "p-id", "p").is_err());
        assert!(read_questions(&sys, "p-id", "p").is_ok());
        assert!(read_questions(&Actor::Operator, "p-id", "p").is_ok());
        // Answer: the operator; a sysadmin for others, not for itself.
        assert!(answer_question(&Actor::Operator, "p-id").is_ok());
        assert!(answer_question(&sys, "p-id").is_ok());
        assert!(answer_question(&sys, "s-id").is_err());
        assert!(answer_question(&lead, "w-id").is_err());
        assert!(answer_question(&lead, "p-id").is_err());
        // Retract: the operator, or the leader itself.
        assert!(retract_question(&Actor::Operator, "p-id", "p").is_ok());
        assert!(retract_question(&lead, "p-id", "p").is_ok());
        assert!(retract_question(&sys, "p-id", "p").is_err());
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
            spawn: None,
            calendar_write: false,
            thread_open: false,
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
            spawn: None,
            calendar_write: false,
            thread_open: false,
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
            spawn: None,
            calendar_write: false,
            thread_open: false,
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

    #[test]
    fn spawn_policy_rules() {
        use terrarium_core::config::SpawnPolicy;
        let with = |sp: Option<SpawnPolicy>| Policy {
            spawn: sp.map(Box::new),
            ..Policy::default()
        };
        let some = with(Some(SpawnPolicy {
            templates: vec!["project:*".into()],
            ..SpawnPolicy::default()
        }));
        let children = with(Some(SpawnPolicy {
            templates: vec!["*".into()],
            may_spawn_children: true,
            ..SpawnPolicy::default()
        }));
        let none = Policy::default();
        let lead = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        let sys = Actor::Agent(agent("q", &[Role::Sysadmin], Policy::default()));
        // No change: no check, also for a member.
        let member = Actor::Agent(agent("p", &[], Policy::default()));
        assert!(set_spawn_policy(&member, Some("x"), &[], Some(&some), &some).is_ok());
        // A lead on a member: yes; on itself, on an admin, children: no.
        assert!(set_spawn_policy(&lead, Some("w"), &[], Some(&none), &some).is_ok());
        assert!(set_spawn_policy(&lead, None, &[], None, &some).is_ok());
        assert!(
            set_spawn_policy(
                &lead,
                Some("p-id"),
                &[Role::ProjectAdmin],
                Some(&none),
                &some
            )
            .is_err()
        );
        assert!(
            set_spawn_policy(&lead, Some("l2"), &[Role::ProjectAdmin], Some(&none), &some).is_err()
        );
        assert!(set_spawn_policy(&lead, Some("w"), &[], Some(&none), &children).is_err());
        // A lead may remove the block of a member.
        assert!(set_spawn_policy(&lead, Some("w"), &[], Some(&some), &none).is_ok());
        // Sysadmin and the operator: anything valid.
        assert!(
            set_spawn_policy(
                &sys,
                Some("l2"),
                &[Role::ProjectAdmin],
                Some(&none),
                &children
            )
            .is_ok()
        );
        assert!(set_spawn_policy(&Actor::Operator, Some("w"), &[], None, &children).is_ok());
        let bad = with(Some(SpawnPolicy {
            max_concurrent: 0,
            ..SpawnPolicy::default()
        }));
        let e = set_spawn_policy(&Actor::Operator, Some("w"), &[], None, &bad).unwrap_err();
        assert_eq!(e.code, crate::error::Code::Invalid);
        // Spawn and cancel: the owner itself, else an admin.
        assert!(spawn_for(&member, "p-id", "p").is_ok());
        assert!(spawn_for(&member, "other", "p").is_err());
        assert!(spawn_for(&lead, "other", "p").is_ok());
        assert!(cancel_spawn(&member, "p-id", "p").is_ok());
        assert!(cancel_spawn(&member, "other", "p").is_err());
        assert!(cancel_spawn(&Actor::Operator, "other", "p").is_ok());
    }

    /// TER-93: the grant of thread:open follows the spawn rights.
    #[test]
    fn thread_open_is_granted_like_the_spawn_rights() {
        let lead = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        let sys = Actor::Agent(agent("q", &[Role::Sysadmin], Policy::default()));
        let off = Policy::default();
        let on = Policy {
            thread_open: true,
            ..Policy::default()
        };
        assert!(set_thread_open(&lead, Some("w"), &[], Some(&off), &on).is_ok());
        assert!(set_thread_open(&lead, None, &[], None, &on).is_ok());
        assert!(set_thread_open(&lead, Some("w"), &[], Some(&on), &off).is_ok());
        // Not on itself, not on an admin; a sysadmin and the operator may.
        assert!(set_thread_open(&lead, Some("p-id"), &[], Some(&off), &on).is_err());
        let admin = [Role::ProjectAdmin];
        assert!(set_thread_open(&lead, Some("l2"), &admin, Some(&off), &on).is_err());
        assert!(set_thread_open(&sys, Some("l2"), &admin, Some(&off), &on).is_ok());
        assert!(set_thread_open(&Actor::Operator, Some("l2"), &admin, None, &on).is_ok());
        // No change: no check.
        assert!(set_thread_open(&lead, Some("p-id"), &[], Some(&on), &on).is_ok());
    }

    /// TER-93: who opens a thread, and in which scope.
    #[test]
    fn open_thread_needs_the_right_and_the_own_project() {
        let on = Policy {
            thread_open: true,
            ..Policy::default()
        };
        let none = Actor::Agent(agent("p", &[Role::ProjectAdmin], Policy::default()));
        let e = open_thread(&none, "p").unwrap_err();
        assert!(e.message.contains("thread:open"), "{}", e.message);
        let w = Actor::Agent(agent("p", &[], on.clone()));
        assert!(open_thread(&w, "p").is_ok());
        assert!(open_thread(&w, "global").is_ok());
        assert!(open_thread(&w, "q").is_err());
        // An architect writes only in its own project (TER-47).
        let arch = Actor::Agent(agent("meta", &[Role::Architect], on.clone()));
        assert!(open_thread(&arch, "meta").is_ok());
        assert!(open_thread(&arch, "p").is_err());
        let sys = Actor::Agent(agent("meta", &[Role::Sysadmin], on));
        assert!(open_thread(&sys, "p").is_ok());
        assert!(open_thread(&Actor::Operator, "p").is_ok());
    }

    /// TER-93, A1: the template of an agent-opened thread may not exceed
    /// the opener's rights.
    #[test]
    fn thread_rights_lists_what_the_opener_does_not_hold() {
        let local = |n: &str| n == "p-box";
        let w = agent(
            "p",
            &[],
            Policy {
                targets: vec!["w-box".into()],
                cross_project: vec!["q/*".into()],
                ..Policy::default()
            },
        );
        let lead = agent("p", &[Role::ProjectAdmin], Policy::default());
        let arch = agent("meta", &[Role::Architect], Policy::default());
        let sys = agent("ops", &[Role::Sysadmin], Policy::default());
        let check = |a: &AgentActor, project: &str, roles: &[Role], pol: &Policy| {
            thread_rights(
                a,
                ThreadAgentRights {
                    project,
                    roles,
                    policy: pol,
                },
                &local,
            )
        };
        let plain = Policy::default();
        assert!(check(&w, "p", &[], &plain).is_empty());
        assert!(check(&w, "meta", &[Role::Member], &plain).is_empty());
        // Roles.
        let pa = [Role::ProjectAdmin];
        assert_eq!(
            check(&w, "p", &pa, &plain),
            vec!["role project_admin of `p`"]
        );
        assert!(check(&lead, "p", &pa, &plain).is_empty());
        // A lead of `p` opening a global thread: project_admin of `meta`.
        assert_eq!(
            check(&lead, "meta", &pa, &plain),
            vec!["role project_admin of `meta`"]
        );
        let ar = [Role::Architect];
        assert_eq!(check(&lead, "meta", &ar, &plain), vec!["role architect"]);
        assert!(check(&arch, "meta", &ar, &plain).is_empty());
        assert_eq!(
            check(&arch, "meta", &[Role::Sysadmin], &plain),
            vec!["role sysadmin"]
        );
        assert!(check(&sys, "meta", &[Role::Sysadmin], &plain).is_empty());
        // Targets: held, or a project target for its admin.
        let boxes = |t: &[&str]| Policy {
            targets: t.iter().map(|s| s.to_string()).collect(),
            ..Policy::default()
        };
        assert!(check(&w, "p", &[], &boxes(&["w-box"])).is_empty());
        assert_eq!(
            check(&w, "p", &[], &boxes(&["p-box", "big"])),
            vec!["target `p-box`", "target `big`"]
        );
        assert!(check(&lead, "p", &[], &boxes(&["p-box"])).is_empty());
        assert_eq!(
            check(&lead, "p", &[], &boxes(&["big"])),
            vec!["target `big`"]
        );
        // cross_project and mail_to outside the project.
        let reach = Policy {
            cross_project: vec!["q/*".into(), "r/x".into()],
            mail_to: vec!["*".into(), "p/dev".into(), "q/dev".into(), "s/dev".into()],
            ..Policy::default()
        };
        assert_eq!(
            check(&w, "p", &[], &reach),
            vec!["cross_project `r/x`", "mail_to `s/dev`"]
        );
        // Spawn rights: what a lead may grant, or held by the opener.
        let spawner = |t: &[&str], children: bool| Policy {
            spawn: Some(Box::new(terrarium_core::config::SpawnPolicy {
                templates: t.iter().map(|s| s.to_string()).collect(),
                may_spawn_children: children,
                ..Default::default()
            })),
            ..Policy::default()
        };
        assert!(check(&lead, "p", &[], &spawner(&["worker"], false)).is_empty());
        assert_eq!(
            check(&lead, "p", &[], &spawner(&["worker"], true)),
            vec!["spawn rights"]
        );
        assert_eq!(
            check(&w, "p", &[], &spawner(&["worker"], false)),
            vec!["spawn rights"]
        );
        let ws = agent("p", &[], spawner(&["worker", "helper"], false));
        assert!(check(&ws, "p", &[], &spawner(&["worker"], false)).is_empty());
        assert_eq!(
            check(&ws, "p", &[], &spawner(&["other"], false)),
            vec!["spawn rights"]
        );
        // calendar_write: held, or the admin of the project.
        let cal = Policy {
            calendar_write: true,
            ..Policy::default()
        };
        assert_eq!(check(&w, "p", &[], &cal), vec!["calendar_write"]);
        assert!(check(&lead, "p", &[], &cal).is_empty());
        // A sysadmin holds every right.
        assert!(check(&sys, "p", &pa, &reach).is_empty());
    }
}
