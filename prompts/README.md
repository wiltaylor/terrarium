# prompts/

System prompt templates (PLAN §8). Compose mounts this directory read-only
at `/prompts`. The daemon option `--prompts-dir` (env
`TERRARIUM_PROMPTS_DIR`) changes the path.

| Template | Role | Purpose |
|---|---|---|
| `default.md` | `member` | A worker: the CLI, and the mailbox, board, memory, and wiki protocols. |
| `project-lead.md` | `project_admin` | The default content, plus the lead duties of PLAN §10: triage the inbox, keep `ready` stocked, run workers. |
| `system-lead.md` | `sysadmin` | The default content, plus the system duties: make projects, appoint leads, watch hosts, agents, and usage. |

## Protocol rules

These rules come from the live leader and worker runs of 2026-10-25,
including the mode 1 run. Keep them when you edit a template.

- An agent edits files and runs `git` (commit, push) in its sandbox. It runs builds and tests on a target with `terrarium exec <target> -- sh -c 'cd /work/<project>/<agent>/<repo> && <command>'`. It does not run `git` on the target. A worker with no target moves the card to `blocked` with "need a target".
- On a vmlab target, an agent takes a snapshot before a risky change with `terrarium target snapshot <target> <name>`, and rolls back with `terrarium target restore <target> <name>`. `terrarium target snapshots <target>` lists them.
- A worker does not mail the lead when it runs `card move <id> review`. The move tells the lead. A worker mails the lead only when it is blocked or has a question.
- A worker writes the branch, the test result, and what to look at in the `card move ... -m` comment.
- An agent does not run `terrarium mail wait` in a turn, because it holds the turn open. It ends the turn, and the daemon wakes it when mail or a card change arrives.
- The lead does these steps in this order: `admin agent new`, `repo checkout --agent`, `admin agent start`, `admin target grant`, `card assign`. The grant comes before the assign. The card pointer starts the worker, so the lead sends no start mail.
- The lead makes each worker from the agent template `worker` of its project: `admin agent new w1 --template worker`. The template keeps the harness, the subscription, the prompt, and the policy in one place. A flag on `agent new` changes one field. When the project has no `worker` template, the lead makes one with `template new worker` before the first worker. The system lead makes it when it appoints the lead. (An agent template is not a prompt template: see docs/api.md §21.)
- A grant whose target does not come up exits 1. The lead then does not assign the card. It fixes the target with `admin target edit`, revokes the lease, and grants again.
- The lead gives each worker `--subscription`: the one the operator gave, or its own (`terrarium usage` shows it).
- The lead asks the operator for requirements only when a card is not clear.
- The lead answers on the same thread with `terrarium mail reply <id>`, not with `mail send -s "Re: ..."`.
- The lead gives a shared target to one worker for one card with `admin target grant <target> <worker> --exclusive --until <time>`. It keeps the lease while the card is in `review`, and revokes it after the merge. It writes each grant and revoke in the leader log. A refused grant (`lease_held`) means that another worker holds the target: the lead waits for the card of that worker.
- To test a branch at review, the lead grants itself the target, runs the tests with `terrarium exec`, and revokes its own lease.
- The lead makes targets at project scope. For `--lab-from-workspace`, `--lab-path` is relative to the workspace root, so a lab at the root of a repo is `--lab-path <repo>`, never `.`.
- A lab file does not use `provision`: the lab allowlist refuses it. The lab image must have the tools already.
- The system lead gives each lead `--subscription` in `admin agent new`: the one the operator gave, or its own. Its reply to the operator gives the project, repo, and lead names.

## How the daemon selects a template

An agent definition names its template with `prompt`:

```toml
# agents/terrarium-lead.toml
name = "lead"
project = "terrarium"
harness = "claude"
prompt = "project-lead"
roles = ["project_admin"]
```

The CLI does the same with `terrarium agent new <name> --prompt
project-lead`, or `terrarium agent set <agent> --prompt project-lead`.

Before each start, the daemon reads `<prompts dir>/<prompt>.md`, fills in
the variables, and writes the result to `/state/agents/<ulid>/harness/prompt.md`.
The adapter gives that file to the harness. A change to a template applies
at the next start or restart of the agent.

- An agent with no `prompt` gets the built-in template
  (`DEFAULT_TEMPLATE` in `crates/terrariumd/src/prompt.rs`). It is a copy of
  `default.md`, so it works when the prompts directory is not mounted. It
  does not read `default.md` at run time. To use an edited `default.md`, set
  `prompt = "default"`.
- A `prompt` name that has no file stops the start with an error. The daemon
  does not fall back to the default.
- A name with `/` or `..` is not valid.

## Variables

Write a variable as `{{name}}`. Spaces inside the braces are allowed. An
unknown variable stays in the text as it is, so check the spelling.

| Variable | Value |
|---|---|
| `{{agent.name}}` | The agent name. |
| `{{agent.id}}` | The agent ULID. |
| `{{agent.harness}}` | `claude`, `codex`, `copilot`, or `pi`. |
| `{{project.name}}` | The project name. |
| `{{peers}}` | One Markdown list line for each other agent of the project, sorted by name: name, harness, and roles. |
| `{{targets}}` | One list line for each allowed target, or `- none`. |
| `{{roles}}` | The roles, for example `member` or `project_admin`. |

The rendered prompt holds only stable facts. It has no peer status and no
times. The daemon keeps a hash of the prompt. A harness that keeps the prompt
of a session (Codex) gets a new session when the hash changes, so a live value
in the prompt would stop each resume. The templates tell the agent to run
`terrarium agents ls` for the live status of the peers.

## How to add a template

1. Copy `default.md` to `prompts/<name>.md`. The renderer has no include
   feature, so a template holds all of its text.
2. Add the duties of the role in a new section at the end. Name only
   commands that are in `docs/cli.md`.
3. Use only the variables in the table above.
4. Keep the template short. The model reads the full prompt on each turn.
   Keep a worker under about 900 words and a lead under about 1300 words.
5. Set `prompt = "<name>"` in the agent definition, then restart the agent.

When you change the protocols in `default.md`, make the same change in each
lead template. Copy the new `default.md` into `DEFAULT_TEMPLATE` too: the
test `default_template_matches_prompts_default_md` fails until the two are
the same.
