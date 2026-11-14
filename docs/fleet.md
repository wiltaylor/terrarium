# Fleet files

A fleet file declares a full Terrarium set-up in one file: projects, their
repos, agents, targets, and schedules, global targets, and subscriptions. A vmlab lab
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
# system_prompt = "..."          # the base system prompt (PLAN §8); see "Agent templates"
restart = "on-failure"
new_session_on_prompt_change = true
start = false
# [defaults.policy] is also possible; an agent with its own [policy] replaces it.

[[templates]]                    # optional: a global agent template (api.md §21)
name = "worker"
harness = "claude"
prompt = "default"

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

[projects.delivery]              # optional: the job queue of the project (TER-87)
target = "buildbox"
recipe = "just ci::validate"
slots = 2                        # optional, default 2
next_repos = ["terrarium"]       # optional: the repos with a `next` branch
work_dir = "/tmp/terrarium-jobs" # optional
timeout_secs = 3600              # optional

[[projects.columns]]             # optional: a column of a `board_flow = "columns"` board (TER-1305)
name = "merge_queue"
role = "merge_queue"             # role and wip_limit: for a column that the board lacks
checks = { require_comment_on_enter = false, require_dependencies_met = false, terminal_for_members = false, clear_eta_on_enter = true }
queue = { release = "batch", batch_size = 4, max_wait_seconds = 1800, max_batch_cards = 8 }
rules = [
    { name = "two", trigger = { kind = "count_reached", count = 2 }, actions = [{ verb = "mail", args = { to = ["operator"], subject = "two in the queue" } }] },
]

[[projects.agents]]              # an AgentDef without `project`, plus `start`
name = "lead"
prompt = "project-lead"
roles = ["project_admin"]
idle_stop = "30m"                # optional: `off`, or the rest before the idle stop (PLAN §18)
compact_on_idle = true           # optional: compact the session before the idle stop
compact_max_age = "24h"          # optional: `off`, or compact an older session at its next rest
card_reminder = false            # optional: no reminder of the cards in progress when idle
start = true                     # start on `fleet apply --start`; work also wakes it

[projects.agents.policy]
targets = ["buildbox", "shared-lab"]
thread_open = true               # optional: the right thread:open (api.md §6.13)
calendar_write = true            # optional: write the project calendar

[projects.agents.policy.spawn]   # optional: the spawn rights (PLAN §18)
templates = ["worker"]
max_concurrent = 2
max_lifetime = "4h"
bound = true                     # optional: bind a spawn to a card (TER-74)
max_bound_lifetime = "3d"

[[projects.targets]]             # a TargetDef without `project`
name = "buildbox"
kind = "ssh"
address = "build@buildbox.lan"

[[projects.targets]]
name = "box"
kind = "host"
host = "lab1"
[projects.targets.limits]        # optional, host and docker: api.md §17.2, "Resource limits"
cpu = 2
memory = "4G"

[[projects.schedules]]           # a ScheduleDef (api.md §19), no `project`
name = "standup"
catch_up = "once"                # optional: `skip` (the default) or `once`
enabled = true                   # optional
spec = { kind = "cron", expr = "0 9 * * MON-FRI", tz = "Europe/London" }
action = { type = "mail", to = "lead", subject = "standup", body = "Post your status." }

[[projects.schedules]]
name = "tests"
[projects.schedules.spec]
kind = "interval"
every = "30m"
[projects.schedules.action]
type = "exec"
target = "buildbox"
cmd = ["make", "test"]
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
- `delivery` is the config of `PUT /api/projects/{name}/delivery`, with
  the same checks at the apply: `target` is a target of the project (one
  of the file is fine), or a global target only for a sysadmin or the
  operator. `next_repos` names repos of the project, and `slots` is 1 or
  more. `fleet export` writes it when the project has a config. A plan
  shows a change as the `delivery` field of the project. Without the
  table the apply leaves the config as it is. A file that drops a table
  that the fleet applied before clears the config (no queue).
- `board_flow` is `code`, `rules`, or `columns`. `fleet export` writes it
  when it is not `code`. A plan compares it whenever the file gives it and
  shows a switch as the `board_flow` field of the project. The apply writes
  it only when it differs, as `PATCH /api/projects/{name}` does (audit
  `project.update`). A board on column rules is not taken off them: a file
  that gives it `code` or `rules` fails the apply with
  `projects.<p>.board_flow`. A board on column rules (api.md §19.13, §19.14)
  exports as `board_flow = "columns"` and one `[[projects.columns]]` for
  each column, in board order: its `name`, `title` (only a title that is
  not the default), `role`, and `wip_limit`, and
  the `checks`, `queue`, and `rules` of `GET
  /api/projects/{name}/board/columns/{column}/options`, without the rule
  IDs (TER-1305). `[[projects.columns]]` needs `board_flow = "columns"`,
  and `board_flow = "columns"` needs `[[projects.columns]]`.
  The apply switches the project to column rules first. Then it adds each
  named column that the board lacks, with its title, role, and WIP limit, after
  the nearest column before it in the file that the board has (one board
  change, as `PATCH /api/projects/{name}/board`). A role that another
  column of the board holds is an error: the apply does not move a role.
  A column that the board has keeps its place, title, role, and WIP limit. Then
  it writes each named column as the options `PUT` does, with its checks:
  a rule matches the column's rule of the same name and keeps its ID, and
  an absent option keeps what the column has. A column that the file does
  not name stays as it is. An unchanged file writes nothing. A plan shows
  a change as the `board_flow` and `columns` fields of the project. The
  import writes no `board_conversions` row, so `POST .../board/convert`
  `revert` answers `404` for it. A file does not take a board off column
  rules. An export of a value that TOML cannot hold (a `null` in a
  list of a rule) fails and names the rule.
- `policy.targets` names global targets and targets of the agent's own
  project.
- `policy` takes `targets`, `mail_to`, `cross_project`, `calendar_write`,
  `thread_open`, and the sub-table `spawn` (`templates`, `max_concurrent`,
  `max_per_day`, `max_lifetime`, `may_spawn_children`, `bound`,
  `max_bound_lifetime`). `fleet export` writes each of them, so an export
  plans with no change. A plan names a policy change by key, for example
  `policy.spawn`.
- A schedule takes its project from the enclosing `[[projects]]`. Its
  key is `project/name`, and its name is unique in the project. Its
  `spec` must be valid (a five-field cron expression, an interval of 60 s
  or more, or an `at` in the future); `plan` reports an error at
  `projects.<p>.schedules.<name>`. An agent or target that its `action`
  names must exist after the apply.
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
| Owned by this fleet, no longer in the file | `delete` (agents, targets, schedules) or `release` (projects, subscriptions) |

A fleet object shows `source = fleet:<name>` in the API. The API can
change it, because it is not file-managed. The next `plan` shows the
change as drift, and the next `apply` writes the file values again. The
file is the truth, as for `source = file` objects. `POST /api/reload`
deletes and releases `source = file` objects only, so it keeps the fleet
objects.

`--prune` deletes the agents, targets, and schedules that the fleet owns
and the file no longer declares. It releases the projects and subscriptions: they stay
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
- `apply --start` starts each agent with `start = true` that does not run
  and is not paused. It is not needed for work: mail, a turn, or a card
  wakes an agent (PLAN §18).
- `apply` clones `checkout.on_agent_create` into the workspace of each new
  agent, as `agent new` does.
- `export` captures a project that was made by hand. The fleet declares the
  project, its repos, its agents, its project targets, and its schedules
  (not those of `projects/<name>.toml`). Subscriptions
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
| A change to a project's `board_flow = "columns"` or `[[projects.columns]]` | project_admin of the project or the operator (as `PUT /api/projects/{name}/board/columns/{column}/options`) |
| A change to a project's `delivery` | project_admin of the project, and for a global `target` sysadmin or the operator (as `PUT /api/projects/{name}/delivery`) |
| An agent (create, update, delete, start) | project_admin of its project |
| A role on an agent | the rule of `role grant`: sysadmin needs the operator |
| An agent's `policy`: a new global target, a new `cross_project` entry, or a new `mail_to` entry in another project | sysadmin or the operator |
| An agent's `policy.spawn` or `policy.thread_open` | the rule of `PATCH /api/agents/{id}`: a project_admin changes them on a member of its project, not on itself or an admin, and does not set `may_spawn_children` or `bound`; else sysadmin or the operator |
| A project target | project_admin of the project |
| A global target | sysadmin |
| A subscription | sysadmin |
| A schedule | project_admin of its project, and an action inside the project (api.md §19.3) |
| A project agent template | project_admin of its project |
| A global agent template | sysadmin |

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

### The base system prompt

An agent, a template, or `[defaults]` can set `system_prompt`, the agent's
base system prompt (PLAN §8, [api.md §6.1](api.md#61-crud)). When it is
set, it replaces the harness's default system prompt, and Terrarium's
protocol prompt (`prompt`) comes after it. Use a multi-line string:

```toml
[[projects.agents]]
name = "reviewer"
system_prompt = """
You review code for {{project.name}}. You do not write features.
"""
```

`system_prompt_file = "<path>"` reads the text from a file, relative to
the fleet file. The CLI (`fleet validate`, `plan`, `apply`) and the config
dir read the file. The API refuses `system_prompt_file`, because it does
not know the path. An agent cannot set both keys. Export writes
`system_prompt`.

An agent or a template can set `persona` (PLAN §8.1): `leader`, `worker`,
`none`, or a prompt file name of the config repo. Not set: by role. The
persona replaces the harness's default prompt when the agent has no
`system_prompt`. `[defaults]` has no `persona`.

```toml
[[projects.agents]]
name = "w1"
persona = "none"        # the harness's own default prompt
```

An agent or a template can set `skills` (PLAN §8.6): names of the skill
library in the config repo, or `*` for every skill. Not set: no explicit
skills. A name that is not in the library is an error of the plan. Each
agent also gets the default-on skills of the library (the packaged
`terrarium-*` skills), less the names in `skills_off`; `default_skills =
false` turns off all of them. `*` in `skills_off` is an error.

```toml
[[projects.agents]]
name = "w2"
skills = ["fern-check"]
skills_off = ["terrarium-spawn"]
```

The config repo (PLAN §8.3) keeps an export of each project as a fleet
file in `export/projects/<project>.fleet.toml`. A pull from another
instance shows its plan, and `terrarium admin config pull --apply` applies
it as `fleet apply` does (no prune, no start).

### Agent templates

`[[templates]]` declares a global agent template, and
`[[projects.templates]]` a template of the project. The spec keys
(`harness`, `subscription`, `model`, `prompt`, `system_prompt`, `persona`, `roles`,
`policy`, `restart`, `idle_stop`, `compact_on_idle`, `compact_max_age`, `card_reminder`, `skills`,
`skills_off`, `default_skills`, `new_session_on_prompt_change`) sit next to `name` and
`description`. An agent entry with `template = "worker"` gets the values
in this order: `[defaults]`, then the template, then the keys of the entry.
`policy` merges key by key. `worker` is the project's template, else the
global one; `global:worker` is the global one. The template comes from the
file first, then from the daemon. `fleet validate --offline` cannot read
the daemon: an agent of a template that the file does not declare gets a
warning and is left out of the check.

The fleet owns its templates (kind `template`, key `project/name` or
`_/name`). An apply writes them before the agents, and records the
template on each agent. A change of a template in the file changes the
agents of the fleet at the next apply, because the file is the truth.
`--prune` deletes a template that the file no longer declares. Export
writes the project's templates, and `template = "..."` with only the keys
that differ for each agent that came from a template that still exists.

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

A fleet schedule runs as the actor of the apply: `operator` for a fleet
file in the config dir. A changed `action` makes the actor of the apply
its `run_as` again (api.md §19.1).

The `source` column of `projects`, `agents`, `targets`, `schedules`,
`agent_templates`, and `subscriptions` accepts `file`, `api`, and
`fleet:<name>`. An apply writes
`fleet:<name>` on each object that it makes or adopts, and a release sets
`api` again. A `file` row stays `file`. Migration 9 also writes
`fleet:<name>` on the rows that `fleet_objects` already listed.
