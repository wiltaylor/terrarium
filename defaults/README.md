# Terrarium config repo

This directory is the Terrarium config repo (`/config/terrarium` in the
container, `./config/terrarium` next to `compose.yaml`). Edit the files
here to change how the agents of this Terrarium instance work. Put the
directory in git to keep a history, and to share it with other Terrarium
instances (`terrarium admin config sync`).

| Path | Content |
|---|---|
| `prompts/leader.md`, `prompts/worker.md` | The persona prompts. A persona replaces the harness's default system prompt. Leads (`project_admin`, `sysadmin`) get `leader.md`, members get `worker.md`, unless the agent sets `persona`. |
| `prompts/default.md`, `prompts/project-lead.md`, `prompts/system-lead.md` | The Terrarium protocol templates. The daemon appends one after the persona. `prompts/README.md` tells how to write one. |
| `harness/claude/settings.json` | Claude Code settings for every Claude agent. |
| `harness/codex/config.toml` | Codex settings, given as `-c` overrides. |
| `harness/copilot/settings.json` | Copilot CLI settings, merged into the settings copy of each agent. |
| `harness/pi/settings.json` | pi settings, merged into the settings copy of each agent. |
| `export/` | The declarative export of the templates, schedules, and projects (agents, targets, repos), as fleet files. The daemon writes it at each commit. |

A change applies at the next start or restart of an agent. A file that
does not parse stops the start of the agent, with the file and the line in
the error.

## Defaults and upgrades

Terrarium ships a default for each file in `prompts/` and `harness/`. At
each start the daemon writes a missing file. When a new Terrarium version
changes a default, the daemon replaces a file that you did not edit. A
file that you edited stays, and the new default goes next to it as
`<file>.new`. The dashboard (Config page) and `terrarium admin config
defaults` show the `.new` files with a diff: accept the new default, or
keep your file.

## What never goes in this repo

No secret goes in this repo: no API key, login, token, password, private
key, host enrol token, or TLS key. Credentials stay in the state dir. The
`.gitignore` of the repo allows only the known paths and file types. The
daemon scans each commit that it makes for secrets, and the `pre-commit`
hook scans a commit that you make by hand. A hit stops the commit and
names the file, the line, and the rule.
