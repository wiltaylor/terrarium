# config/

This directory holds the harness homes. Compose bind-mounts each one into the
container under `/auth`. Git ignores everything here except this file.

| Host directory     | Container path   | Harness     | Env var in the container |
|--------------------|------------------|-------------|--------------------------|
| `config/claude/`   | `/auth/claude`   | Claude Code | `CLAUDE_CONFIG_DIR`      |
| `config/codex/`    | `/auth/codex`    | Codex       | `CODEX_HOME`             |
| `config/copilot/`  | `/auth/copilot`  | Copilot CLI | `COPILOT_HOME`           |
| `config/pi/`       | `/auth/pi`       | pi          | `PI_CODING_AGENT_DIR`    |

In the container, `~/.claude`, `~/.codex`, `~/.copilot`, and `~/.pi/agent` are
symlinks to the same paths. A harness finds its home through the env var or
through `$HOME`.

## What the home is for

Each home holds the login of one harness, and all agents of that harness
share it. The sandbox (PLAN §11) binds the home read-write into each agent at
the path that the harness expects. Then it binds per-agent files on top of
some paths in the home (the overlay trick, PLAN §8). An agent sees its own
file at a shared path. The file in the home does not change.

The per-agent files are in `/state/agents/<ulid>/` (for Claude Code, in
`/state/agents/<ulid>/claude/`).

| Harness | The home holds | Per-agent files over the home |
|---|---|---|
| Claude Code | the login, the session transcripts, caches | see the table below |
| Codex | the login, the sessions | none yet (the adapter is not built) |
| Copilot CLI | the login, the system-prompt extension | none yet. PLAN §8 plans the agent's `prompt.md` over `SYSTEM.md`. |
| pi | the login, the models | none yet (the adapter is not built) |

### Claude Code: shared and per-agent paths

Every agent has the same cwd (`/home/agent/work`) and, in its PID namespace,
the same pid (2). Claude Code keys some files by cwd or by pid, so those
files must be per-agent. Files that it keys by the session UUID do not
collide. The code and the full reasons are in
`crates/adapters/claude/src/config.rs`.

| Path in `config/claude/` | Shared or per-agent | Why |
|---|---|---|
| `.credentials.json` | shared | The one login. A token refresh by one agent reaches all agents. |
| `.claude.json` | per-agent | Each agent needs its own trust key. Claude Code writes the file on each start. |
| `sessions/` | per-agent | `sessions/<pid>.json` has pid 2 in every sandbox. |
| `history.jsonl` | per-agent | The prompt history. Shared, each agent sees the turns of the other agents and your own. |
| `backups/` | per-agent | Copies of the agent's own `.claude.json`. Claude Code can restore from them. |
| `projects/-home-agent-work/memory/` | per-agent | Claude Code's auto memory for the one shared cwd. Terrarium has its own memory (PLAN §14). |
| `projects/-home-agent-work/<uuid>.jsonl` | shared | One transcript per session. `--resume` and the usage collector read them. An agent can read the transcripts of its peers. |
| `session-env/`, `file-history/`, `todos/`, `security_warnings_state_<uuid>.json` | shared | Keyed by the session UUID. |
| `shell-snapshots/`, `debug/`, `cache/`, `statsig/`, `plugins/`, `paste-cache/` | shared | Unique names, debug output, or caches. They are harmless to share. |

The per-agent `.claude.json` starts as a copy of the shared one. After that,
the shared `.claude.json` does not change.

## Claude Code agents do not read the settings in this home

The adapter starts Claude Code with `--setting-sources ""`. Thus Claude Code
loads no user, project, or local settings, and no `CLAUDE.md` from the home.
These edits do **not** reach agents:

- `config/claude/settings.json` (and `settings.local.json`)
- `config/claude/CLAUDE.md`
- hooks, permissions, and env that you put in those files

Configure the agent behaviour in these places:

| What | Where |
|---|---|
| The system prompt | the prompt templates in `prompts/` (PLAN §8). The agent definition names the template (`prompt = "<name>"`). |
| The model, the roles, the restart policy | the agent definition (`agents/<name>.toml` in the definitions directory) or the API |
| Hooks and the keys that stop the start-up dialogs | the adapter's `--settings` JSON (`settings_json` in `crates/adapters/claude/src/launch.rs`). Each agent has a copy in `/state/agents/<ulid>/claude/settings.json`, for inspection only. |
| Telemetry and update switches | the adapter's env (`quiet_env` in `crates/adapters/claude/src/flags.rs`) |

The settings file in the home still applies when you run `claude` yourself
in the container, for example to log in.

## Edit from the host

The bind mounts let you edit harness files from the host with your normal
editor. For example:

- `config/codex/config.toml`
- `config/copilot/` (the entrypoint installs the system-prompt extension in
  `config/copilot/extensions/`)
- `config/pi/settings.json`, `config/pi/models.json`
- `config/claude/settings.json` (only for your own `claude` runs; see above)

Open one with `just docker::config-edit <harness>`.

## Set up

1. Run `just docker::volumes-init`. It makes these directories, owned by you.
   If Docker makes them first, root owns them and the container cannot write.
2. Make sure that `TERRARIUM_UID` and `TERRARIUM_GID` match the owner of these
   directories. The justfile uses `id -u` and `id -g` by default.

## Log in

All agents share these homes, so you log in once per harness. Use the
dashboard's Subscriptions page (PLAN §16.1), or log in from a shell:

```sh
docker compose exec terrarium claude auth login
docker compose exec terrarium codex login --device-auth
docker compose exec terrarium copilot login --device-code
docker compose exec -it terrarium pi        # then /login in the TUI
```

## Rules

- These directories contain credentials. Do not commit them. Do not share them.
- The sandbox (PLAN §11) mounts these homes read-write into each agent. An agent
  can change the shared files in its harness home, and it can read the shared
  session transcripts of other agents. It cannot reach the per-agent files of
  other agents, their workspaces, or the rest of the host.
- `.terrarium-extension-version` in `config/copilot/` records which image
  installed the Copilot extension. Delete it to force a reinstall on the next
  start.
