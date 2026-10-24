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

## Why bind mounts

You can edit harness settings from the host with your normal editor. For
example:

- `config/claude/settings.json`
- `config/codex/config.toml`
- `config/copilot/` (the entrypoint installs the system-prompt extension in
  `config/copilot/extensions/`)
- `config/pi/settings.json`, `config/pi/models.json`

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
  can change its harness session files. It cannot reach other agents' files.
- `.terrarium-extension-version` in `config/copilot/` records which image
  installed the Copilot extension. Delete it to force a reinstall on the next
  start.
