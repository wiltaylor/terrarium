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

The API can change a fleet object, because its row keeps `source = api`.
The next `plan` shows the change as drift, and the next `apply` writes the
file values again. The file is the truth, as for `source = file` objects.

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
To use `fleets/`, add a bind mount for `./fleets:/etc/terrarium/fleets`.

## 5. Policy

PLAN §10 applies to each object of the plan:

| Object | Needs |
|---|---|
| A new project | sysadmin or the operator |
| A change to a project | project_admin of the project |
| An agent (create, update, delete, start) | project_admin of its project |
| A role on an agent | the rule of `role grant`: sysadmin needs the operator |
| A project target | project_admin of the project |
| A global target | sysadmin |
| A subscription | sysadmin |

So a project_admin may apply a fleet that touches only its own project. A
member may not plan or apply. Anyone may validate. With `--prune`, the
removals count too. An agent cannot delete itself.

`GET /api/fleet` shows every fleet to the operator and to sysadmins. An
agent sees the fleets that own objects in its own project.

## 6. API

All routes are under `/api` and take the usual auth. The bodies are JSON.

### `POST /api/fleet/validate`

Body: `{"content": "<toml>", "path"?: "file name"}`. `path` gives the stem
for a fleet without `name`. Answer `200`:

```json
{
  "fleet": "terrarium",
  "valid": true,
  "errors": [],
  "warnings": [{"level": "warning", "at": "prompts", "message": "..."}],
  "objects": [{"kind": "agent", "key": "terrarium/lead", "project": "terrarium"}],
  "references": {"subscriptions": {}, "targets": {}, "prompts": {"project-lead": ["terrarium/lead"]}}
}
```

An issue has `level` (`error` or `warning`), `at` (the place in the file,
for example `projects.terrarium.agents.lead`), and `message`. A file that
does not parse is a `200` with `valid: false`.

### `POST /api/fleet/plan`

Body: `{"content", "path"?, "prune"?: false}`. Answer `200`, a plan:

```json
{
  "fleet": "terrarium",
  "valid": true,
  "errors": [],
  "warnings": [],
  "summary": {"create": 0, "update": 1, "unchanged": 4, "delete": 1, "release": 0, "conflict": 0, "drift": 1},
  "entries": [
    {"kind": "agent", "key": "terrarium/lead", "project": "terrarium", "action": "update",
     "drift": ["prompt"],
     "changes": [{"field": "prompt", "from": "default", "to": "project-lead"}]},
    {"kind": "agent", "key": "terrarium/old", "project": "terrarium", "action": "delete", "live": true}
  ]
}
```

- `action`: `create`, `update`, `unchanged`, `delete`, `release`, or
  `conflict`.
- `drift`: the fields that changed outside the fleet since the last apply,
  or `["(deleted)"]`.
- `adopt`: `true` when the apply takes over an API object.
- `reason`: why an entry is a conflict, or a note.
- `live`: an agent with a live session.

A file with errors is a `200` with `valid: false` and no entries. The plan
needs the same rights as the apply (`403 forbidden`).

### `POST /api/fleet/apply`

Body: `{"content", "path"?, "prune"?: false, "confirm"?: false, "start"?:
false}`. Answer `200`: the plan, with `result` (`done` or `skipped`) on each
entry, and:

- `prune`: the flag.
- `checkout`: the clone results for new agents (as `POST /api/agents`).
- `started`: the agents that `start` started.
- `failures`: starts and clones that failed. The apply itself stays.

Errors: `422 invalid` (the file has errors; `details.errors`), `409
conflict` (`details` is the plan), `409 confirm_required` (`details.agents`
names the live agents), `403 forbidden`.

### `GET /api/fleet`

The applied fleets, by name:

```json
[{"name": "terrarium", "description": "...", "origin": "api", "applied_at": "...",
  "applied_by": "operator", "objects": 5, "projects": ["terrarium"]}]
```

`origin` is `api` or `file:<path in the config dir>`.

### `GET /api/fleet/{name}`

The row of `GET /api/fleet`, with `content` (the last applied file) and
`plan` (the plan of that content now, so drift shows). `404` when the fleet
does not exist or the caller may not see it.

### `GET /api/fleet/export?project=P[&name=N]`

Answer `200`: `{"fleet": "N", "project": "P", "content": "<toml>"}`. `name`
defaults to the project name. Needs read access to the project.

### `POST /api/reload`

The `errors` list of the sync report also has the errors of the fleet files
in the config dir, as `<path>: <message>`.

## 7. Storage

The daemon keeps two tables, made with `CREATE TABLE IF NOT EXISTS` when a
fleet call runs (not by a migration in `db.rs`):

- `fleets`: name, description, origin, the last applied content, and when
  and by whom.
- `fleet_objects`: kind, key, owning fleet, project, and the state of the
  object after the last apply (JSON). An object has one owner at most.

The object rows keep `source = api`. The `source` column of `projects`,
`agents`, `targets`, and `subscriptions` has a `CHECK (source IN ('file',
'api'))`, so a `fleet:<name>` value needs a migration that rebuilds those
tables. Until then, the fleet API reports the owner, and
`crate::fleet::source_of` gives `fleet:<name>` for an object.
