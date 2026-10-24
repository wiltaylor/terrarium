# prompts/

System prompt templates (PLAN §8). Compose mounts this directory read-only
at `/prompts`. The daemon option `--prompts-dir` (env
`TERRARIUM_PROMPTS_DIR`) changes the path.

| Template | Role | Purpose |
|---|---|---|
| `default.md` | `member` | A worker: the CLI, and the mailbox, board, memory, and wiki protocols. |
| `project-lead.md` | `project_admin` | The default content, plus the lead duties of PLAN §10: triage the inbox, keep `ready` stocked, run workers. |
| `system-lead.md` | `sysadmin` | The default content, plus the system duties: make projects, appoint leads, watch hosts, agents, and usage. |

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
the variables, and writes the result to `/state/agents/<ulid>/prompt.md`.
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
| `{{peers}}` | One Markdown list line for each other agent of the project: name, harness, status at start, and roles. |
| `{{targets}}` | One list line for each allowed target, or `- none`. |
| `{{roles}}` | The roles, for example `member` or `project_admin`. |

The peer status is the status at the start of the agent. The templates tell
the agent to run `terrarium agents ls` for the current list.

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
