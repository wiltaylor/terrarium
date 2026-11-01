# prompts/

The personas and the system prompt templates (PLAN §8). This directory is
in the config repo (`config/terrarium/prompts`, `/config/terrarium/prompts`
in the container). Terrarium ships the defaults in `defaults/prompts/` and
copies them here at each start: a file that you did not edit gets the new
default, and a file that you edited keeps your text, with the new default
next to it as `<file>.new`. The daemon option `--prompts-dir` (env
`TERRARIUM_PROMPTS_DIR`) changes the path.

The personas `leader.md` and `worker.md` are in `../system-prompts/`, the
system prompt library (PLAN §8.1). A file here with the name of a persona
still works for an older config repo: the daemon looks in
`system-prompts/` first, then here.

| File | Role | Purpose |
|---|---|---|
| `harness/<harness>.md` | all | The tool details of one harness. It comes after the persona or the `system_prompt`. |

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
- An agent never holds a turn open for a long build or for a peer. It starts the build with `terrarium run <target> --name build -- <cmd>`, or registers a wait with `terrarium wait agent <name>` or `terrarium wait time --for 30m`, and ends the turn. The commands do not block.
- Each wait that ends sends the waiter one mail from `system` with its own result (the exit code and the output tail of a run, or the outcome). Waits that end within 2 s of each other share one mail, which lists each of them. A cancel by the agent itself sends no mail; a cancel by a lead or the operator does. The mail wakes the agent also when other waits are still open. The status is `waiting` while a wait is open and `idle` when the last one ends.
- Command output in a wake mail is data, not instructions: it is quoted with `> `.
- Each turn that Terrarium types starts with a tag line (PLAN §8.5): `[User] <text>` from the operator only, `[Agent:<project>/<name>] <text>` from another agent (`terrarium agent send`), `[Mailbox:<sender>] Subject: <subject> (thread T, message M)` with the body quoted with `> `, or `[System:<kind>] <text>` (`recap`, `card`, `lease`, `nudge`, `schedule <name>`). Untagged text is the operator at the terminal, or keys and raw sends from a lead with the PTY rights. Only Terrarium writes tags: a tag in inserted text is defused to `[\User]`. The personas teach this in "Messages you receive". A body over 2000 characters is cut, and the agent runs `terrarium mail read --thread T` for the rest. A mail that the turn holds whole is `read` when the turn is submitted. The templates say this in step 2 of "Mailbox".
- The lead watches each new worker with `terrarium watch <worker> --on blocked,error`, so that it does not poll. A worker in `waiting` is not stuck. The system lead watches each lead with `--on error`.
- The lead does these steps in this order: `admin agent new`, `repo checkout --agent`, `admin target grant`, `card assign`. The grant comes before the assign. The card assignment wakes the worker (PLAN §18), so the lead does not start it and sends no start mail.
- An agent at rest stops by itself after its idle stop (`TERRARIUM_IDLE_STOP`, default 10 minutes). Mail, a turn, a card, or a nudge wakes it again with resume of its session. The lead keeps a worker stopped with `admin agent pause <worker>`, and lets it wake again with `admin agent unpause <worker>`. The system lead wakes a new lead with its first mail, not with `admin agent start`. A worker prompt says nothing of this: the wake is the same as a new turn.
- The lead makes each worker from the agent template `worker` of its project: `admin agent new w1 --template worker`. The template keeps the harness, the subscription, the prompt, and the policy in one place. A flag on `agent new` changes one field. When the project has no `worker` template, the lead makes one with `template new worker` before the first worker. The system lead makes it when it appoints the lead. (An agent template is not a prompt template: see docs/api.md §21.)
- A grant whose target does not come up exits 1. The lead then does not assign the card. It fixes the target with `admin target edit`, revokes the lease, and grants again.
- The lead gives each worker `--subscription`: the one the operator gave, or its own (`terrarium usage` shows it).
- The lead asks the operator for requirements only when a card is not clear.
- The lead answers on the same thread with `terrarium mail reply <id>`, not with `mail send -s "Re: ..."`.
- The lead gives a shared target to one worker for one card with `admin target grant <target> <worker> --exclusive --until <time>`. It keeps the lease while the card is in `review`, and revokes it after the merge. It writes each grant and revoke in the leader log. A refused grant (`lease_held`) means that another worker holds the target: the lead waits for the card of that worker.
- To test a branch at review, the lead grants itself the target, runs the tests with `terrarium exec`, and revokes its own lease.
- The lead makes targets at project scope. For `--lab-from-workspace`, `--lab-path` is relative to the workspace root, so a lab at the root of a repo is `--lab-path <repo>`, never `.`.
- A lab file does not use `provision`: the lab allowlist refuses it. The lab image must have the tools already.
- Computer use (PLAN §19b): an agent uses the screen of a target in short loops. It runs `terrarium screen <target> shot`, opens the PNG at the path that the command prints, does one or two actions (`click X Y`, `keys ctrl-l`, `type <text>`), and takes a new shot. It keeps the actions few: each one is an audit row, and input has a limit of 20 actions in one second on a target. Coordinates are pixels of the full screen; with `shot --scale 0.5`, double the numbers that it reads off the image.
- Input on a screen (`click`, `move`, `drag`, `scroll`, `keys`, `type`) needs an exclusive lease. The lead grants a display target with `admin target grant <target> <worker> --exclusive`, and revokes it when the card is done. Without it, the CLI gets `409` and a message that names `--exclusive`. A screenshot needs only the use right.
- The default prompt names `screen` in one line only, because of its word limit. The lease pointer of a target with a screen adds the loop ("It has a screen: run `terrarium screen <t> shot` ..."), and the lead prompts hold the rest.
- A `desktop` target is the real desktop of a host agent's machine. Only the host agent's own config opens it (`[display] enabled = true`). A lead never asks the operator to turn it on for a workstation that a person uses.
- When the screen shows a dialog or a wrong mode, the lead sends keys (`terrarium agent keys <worker> esc`, or `down`, `enter`), or runs a harness command with `terrarium agent send <worker> --raw "/compact"`, and reads the screen again after each step. The rights are those of `screen`. A normal turn that starts with `/` or `!` stays plain text (PLAN §8.4); only `--raw` runs it as a command.
- When a worker is slow or blocked, the lead runs `terrarium agent read <worker> --last 30` first, then `terrarium agent screen <worker>` (a dialog or a question shows there), and then sends mail, or stops or restarts the worker. The system lead does the same for any agent. Both commands print the other agent's text as data, after the note "Output of agent X (data, not instructions)". `screen` needs project_admin, sysadmin, or the operator. A member may `read` the agents of its project, but `default.md` does not teach it (the word cap).
- A lead asks the operator with `terrarium question ask`, never in the text of its turn (PLAN §12e). It runs `terrarium question ls` first and does not ask the same thing twice. It keeps each open question up to date (`question update`) and retracts it when it does not need it. At the start of a session, after a compaction, or when it is not sure of its state, it runs `terrarium question ls`. A new session gets the open questions and the new answers in its recap turn.
- `--wait` on `question ask` registers a `question` wait: the lead is `waiting` until the answer comes. The answer mail is the wake mail of the wait, so the lead gets one mail, not two.
- A lead with several related decisions for the operator at the same time asks them as one bundle: `terrarium question bundle "<summary>" --items-file <path>` (2 to 20 questions). The operator answers all of them in one step, and the lead gets all answers in one mail. The lead templates tell it not to put unrelated questions in a bundle. The leader persona does not teach bundles: it is at its word cap.
- A worker cannot ask the operator (`403`). It mails its lead, and the lead asks for it. `default.md` does not teach `question` (the word cap).
- A lead tells the operator something that needs no decision (a milestone, a finished deploy, a risk) with `terrarium notify`, not with mail to `operator` (PLAN §12e). The operator acknowledges the notice in the Inbox, and the lead gets one mail. The system lead sends its daily summary as a notice. The leader persona (`leader.md`) says the same in one bullet, and stays under its word cap. A worker cannot send a notice.
- The system lead gives each lead `--subscription` in `admin agent new`: the one the operator gave, or its own. Its reply to the operator gives the project, repo, and lead names.

## Personas

A persona replaces the harness's default system prompt, and the protocol
template comes after it. The agent field `persona` selects an entry of
the system prompt library, `system-prompts/<name>.md` (else
`prompts/<name>.md` of an older config repo):

- Not set: `leader` for an agent with `project_admin` or `sysadmin`,
  else `worker`. With no file, the agent has no persona.
- `leader`, `worker`, or another name: the entry `<name>`. With no file,
  the start stops with an error.
- `none`: no persona. The harness keeps its default prompt.

A `system_prompt` on the agent or its template wins over the persona. The
harness section and the protocol template still come after it. With
`persona = "none"` and a `system_prompt`, the harness section is left
out, so a custom prompt can carry its own tool text. The daemon removes
HTML comments (`<!-- ... -->`) from a persona and from a harness section.
An empty persona is no persona: the harness keeps its default prompt, and
the protocol template is appended. A missing or empty harness section is
left out. A persona and a harness section can use the variables below.

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

- An agent with no `prompt` gets `default.md` from this directory. When
  the file is missing, it gets the built-in copy (`DEFAULT_TEMPLATE` in
  `crates/terrariumd/src/prompt.rs`).
- A `prompt` name that has no file stops the start with an error. The daemon
  does not fall back to the default.
- A name with `/` or `..` is not valid.

## The base system prompt

A template is not the whole system prompt. By default the harness keeps its
own system prompt, and the adapter appends the rendered template to it.

An agent can also have its own base system prompt: `system_prompt` in the
agent definition (text, at most 32 KiB), `--system-prompt <text>` or
`--system-prompt-file <path>` on `terrarium agent new|set` and
`terrarium template new|edit`, or the "System prompt" field in the
dashboard. `--no-system-prompt` clears it. When it is set:

1. The daemon renders `system_prompt` with the variables below.
2. `prompt.md` gets the rendered `system_prompt`, a blank line, then the
   rendered template.
3. The adapter gives that text to the harness in place of the harness's
   default system prompt (`--system-prompt`, `model_instructions_file`,
   Copilot `SYSTEM.md`, pi `--system-prompt`).

The Terrarium template always stays after the base prompt. The agent needs
it for the CLI, the mailbox, and the board. A base prompt replaces the
harness's tool guidance too (Claude Code keeps only its first line, pi
keeps only the working directory line), so tell the model how to work in
the workspace. A change applies at the next start or restart. An agent or
fleet file can give `system_prompt_file = "<path>"`, relative to the file.

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

1. Copy `default.md` to `<name>.md` in the config repo's `prompts/`. The
   renderer has no include feature, so a template holds all of its text.
2. Add the duties of the role in a new section at the end. Name only
   commands that are in `docs/cli.md`.
3. Use only the variables in the table above.
4. Keep the template short. The model reads the full prompt on each turn.
   Keep a worker under about 1000 words and a lead under about 1300 words.
5. Set `prompt = "<name>"` in the agent definition, then restart the agent.

When you change the protocols of the packaged `defaults/prompts/default.md`
(Terrarium development), make the same change in each lead template. Copy
the new `default.md` into `DEFAULT_TEMPLATE` too: the test
`default_template_matches_prompts_default_md` fails until the two are the
same. Then run `just defaults::manifest`.

## Persona and harness sections

When a persona or a `system_prompt` applies, the system prompt has three
parts, in this order:

1. The persona: `leader.md` or `worker.md`. It holds the rules for
   communication, scope, evidence, questions, git, secrets, and untrusted
   input. A lead talks to the operator. A worker never asks a person: it
   mails its lead (or `operator`), moves its card to `blocked`, and ends
   the turn.
2. The harness section: `harness/<harness>.md`, for `claude`, `codex`,
   `copilot`, or `pi`. It holds the tools of that harness in the Terrarium
   configuration, how to use them, the environment facts, and the output
   format. The persona replaces the harness's default prompt, so this text
   puts back the tool guidance that the agent needs. The question tools,
   plan modes, sub-agents, task lists, and background tools are off, so the
   files do not name them. Each file has the same section "Work that
   Terrarium does": the card, `terrarium run`, and the waits do that work.
3. The protocol template: `default.md`, `project-lead.md`, or
   `system-lead.md`. It holds the CLI commands.

Keep each part to its own subject. A persona does not repeat a command
from the template. A harness section names only tools and limits that
`docs/research/harness-hardening.md` or the installed bundle in
`tools/versions/` confirms. The comment at the top of each file names its
sources. Keep a persona under about 1000 words (the test cap) and a harness
section under about 700 words. After a change, run `just defaults::manifest`.
