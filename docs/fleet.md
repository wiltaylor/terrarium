# Fleet files

A fleet file declares a full Terrarium set-up in one file: projects, their
repos, agents, and targets, global targets, and subscriptions. A vmlab lab
file declares a full lab in the same way. PLAN §20 (Phase 6) asks for it.

The example is [`examples/fleet/terrarium.fleet.toml`](../examples/fleet/terrarium.fleet.toml).

## 1. The file

A fleet file is TOML. It uses the definition structs of PLAN §9, §16, and
§19 as nested tables, so a fleet has the same fields as the files under
`projects/`, `agents/`, `targets/`, and `subscriptions/`.

```toml
version = 1                      # the schema version; 1 is the only one
name = "terrarium"               # the fleet name; the default is the file stem
description = "One line"

[defaults]                       # for each agent that does not set the field
harness = "claude"
subscription = "anthropic-max"
model = "anthropic/claude-opus-5-5"
prompt = "default"
restart = "on-failure"
new_session_on_prompt_change = true
start = false
# [defaults.policy] is also possible; an agent with its own [policy] replaces it.

[[subscriptions]]                # optional: a SubscriptionDef (PLAN §16)
name = "anthropic-max"
provider = "anthropic"
harnesses = ["claude", "pi"]

[[targets]]                      # optional: a global TargetDef (PLAN §19), no `project`
name = "shared-lab"
kind = "ssh"
address = "lab@lab1"

[[projects]]                     # a ProjectDef (PLAN §9)
name = "terrarium"
description = "The Terrarium tool itself"
prefix = "TER"

[[projects.repos]]
name = "terrarium"
url = "git@github.com:wiltaylor/terrarium.git"
default_branch = "main"

[projects.checkout]
on_agent_create = ["terrarium"]

[[projects.agents]]              # an AgentDef without `project`, plus `start`
name = "lead"
prompt = "project-lead"
roles = ["project_admin"]
start = true                     # start on `fleet apply --start`

[projects.agents.policy]
targets = ["buildbox", "shared-lab"]

[[projects.targets]]             # a TargetDef without `project`
name = "buildbox"
kind = "ssh"
address = "build@buildbox.lan"
```

Rules:

- Each project, agent, target, and subscription needs `name`. The name
  rule is the daemon rule: 1 to 63 of `a-z`, `0-9`, `-`, `_`.
- An agent takes its project from the enclosing `[[projects]]`. A project
  target does the same. A `project` key in either is an error when it names
  a different project.
- A global target under `[[targets]]` has no `project`.
- An unknown key is an error, so a typo does not go by silently.
- An agent needs a `harness`, from itself or from `[defaults]`.
- Target names are one namespace across projects. Agent names are unique in
  a project.
- `checkout.on_agent_create` names repos of the same project.
- `policy.targets` names global targets and targets of the agent's own
  project.
- The fleet names `validate`, `plan`, `apply`, and `export` are reserved.

A subscription or a target that an agent names, and that the file does not
define, must exist in the daemon. Prompt templates must exist in the
prompts directory (`TERRARIUM_PROMPTS_DIR`, default `/prompts`). The
template `default` is built in.

## 2. Ownership and the "file is truth" rule

A fleet owns each object that it makes. It also takes over ("adopts") an
API object with the same name that no fleet owns. The daemon records the
owner and the state of each object after the last apply.

| The object is | `plan` shows |
|---|---|
| Absent | `create` |
| Owned by this fleet, equal to the file | `unchanged` |
| Owned by this fleet, different from the file | `update` |
| Changed through the API after the last apply | `update`, with `drift` and the changed fields |
| Deleted through the API after the last apply | `create`, with `drift: (deleted)` |
| An API object that no fleet owns | `update` or `unchanged`, with `adopt` |
| Declared in a definition file (`source = file`) | `conflict`: the definition file wins |
| Owned by another fleet | `conflict` |
| Owned by this fleet, no longer in the file | `delete` (agents, targets) or `release` (projects, subscriptions) |

A fleet object shows `source = fleet:<name>` in the API. The API can
change it, because it is not file-managed. The next `plan` shows the
change as drift, and the next `apply` writes the file values again. The
file is the truth, as for `source = file` objects. `POST /api/reload`
deletes and releases `source = file` objects only, so it keeps the fleet
objects.

`--prune` deletes the agents and targets that the fleet owns and the file
no longer declares. It releases the projects and subscriptions: they stay
as API objects, with their board, cards, wiki, and quota samples. This is
the same as a definition file that is removed. Without `--prune`, `apply`
keeps them and reports them as `skipped`.

An apply is one transaction. When one object fails, nothing changes.

## 3. Commands

```
terrarium fleet validate <file|-> [--offline]   check the file, list its objects; exit 2 on errors
terrarium fleet plan <file|-> [--prune]         show create, update, delete, unchanged per object
terrarium fleet apply <file|-> [--prune] [--confirm] [--start]
terrarium fleet ls                              the applied fleets
terrarium fleet show <name>                     the fleet, its last applied file, and its plan now
terrarium fleet export <project> [--name n] [-o file]
                                                write a fleet file from a project as it is now
```

`terrarium admin fleet ...` is the same command, for agents with a role.
The file content goes to the daemon in the request body, so the commands
work in agent mode too.

`plan` and `apply` print one line for each object:

```
fleet terrarium: 1 to create, 1 to update, 1 to delete, 3 unchanged, 1 drifted

  = subscription anthropic-max
  = project      terrarium
  = target       buildbox
  + agent        terrarium/worker-3
  ~ agent        terrarium/lead      drift: prompt
      prompt: "default" -> "project-lead"
  - agent        terrarium/worker-2  kept: needs --prune
1 object(s) are no longer in the file; add --prune to remove them
```

The marks are `+` create, `~` update, `-` delete or release, `=`
unchanged, and `!` conflict.

- `validate --offline` runs the checks that need no daemon. It does not
  check references or prompt templates.
- `apply` refuses a file with errors (exit 1, code `invalid`) and a plan
  with a conflict (code `conflict`).
- `apply --prune` needs `--confirm` when it deletes an agent with a live
  session. The daemon stops the agent, then deletes it.
- `apply --start` starts each agent with `start = true` that does not run.
- `apply` clones `checkout.on_agent_create` into the workspace of each new
  agent, as `agent new` does.
- `export` captures a project that was made by hand. The fleet declares the
  project, its repos, its agents, and its project targets. Subscriptions
  and global targets stay references. `start` is `false` for each agent.
  An apply of the export adopts the project with no changes.

## 4. Fleet files in the config dir

The daemon applies each fleet file in the config dir at start and on
`POST /api/reload`:

- `<config dir>/*.fleet.toml`
- `<config dir>/fleets/*.toml`

This auto-apply does not prune, does not start agents, and does not clone
repos. A file with errors, or with a conflict, goes into the `errors` list
of the reload report, and the daemon applies the other files. Two files
with the same fleet name are an error for the second file. The sync of the
definition directories runs first, so a definition file wins over a fleet
file.

The Docker image mounts each definition directory on its own (PLAN §23.4).
`compose.yaml` mounts `./fleets` at `/etc/terrarium/fleets`, and `just docker::volumes-init` creates the directory.

## 5. Policy

PLAN §10 applies to each object of the plan:

| Object | Needs |
|---|---|
| A new project | sysadmin or the operator |
| A change to a project | project_admin of the project |
| An agent (create, update, delete, start) | project_admin of its project |
| A role on an agent | the rule of `role grant`: sysadmin needs the operator |
| An agent's `policy`: a new global target, a new `cross_project` entry, or a new `mail_to` entry in another project | sysadmin or the operator |
| A project target | project_admin of the project |
| A global target | sysadmin |
| A subscription | sysadmin |

So a project_admin may apply a fleet that touches only its own project. A
member may not plan or apply (403). Anyone may validate. An agent cannot
delete itself with `--prune`.

When you plan or apply a fleet whose name is in the store already, these
rules apply too:

- You need the rights over every object that the stored fleet owns, with
  or without `--prune`. An object that your file drops counts too, because
  a later prune of the stored file deletes it.
- A file that declares no objects can replace a stored fleet only if you
  are a sysadmin or the operator.
- If the stored fleet comes from the config dir (`origin = file:<path>`),
  `apply` through the API needs `--confirm`. Else it stops with
  `confirm_required`. The next reload applies the file in the config dir
  again, so change that file instead. `plan` needs no `--confirm`.

`GET /api/fleet` shows every fleet to the operator and to sysadmins. An
agent sees the fleets that own objects in its own project.

## 6. API

The HTTP contract is [api.md §18 Fleet](api.md#18-fleet): the request and
response shapes, the `source = fleet:<name>` value, and the error codes.
The routes are:

- `POST /api/fleet/validate`
- `POST /api/fleet/plan`
- `POST /api/fleet/apply`
- `GET /api/fleet`
- `GET /api/fleet/{name}`
- `GET /api/fleet/export?project=P[&name=N]`

The `errors` list of `POST /api/reload` also has the errors of the fleet
files in the config dir.

## 7. Storage

Migration 9 in `crates/terrariumd/src/db.rs` makes two tables:

- `fleets`: name, description, origin, the last applied content, and when
  and by whom.
- `fleet_objects`: kind, key, owning fleet, project, and the state of the
  object after the last apply (JSON). An object has one owner at most.

The `source` column of `projects`, `agents`, `targets`, and
`subscriptions` accepts `file`, `api`, and `fleet:<name>`. An apply writes
`fleet:<name>` on each object that it makes or adopts, and a release sets
`api` again. A `file` row stays `file`. Migration 9 also writes
`fleet:<name>` on the rows that `fleet_objects` already listed.
