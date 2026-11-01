# Terrarium config repo

This directory is the Terrarium config repo (`/config/terrarium` in the
container, `./config/terrarium` next to `compose.yaml`). Edit the files
here to change how the agents of this Terrarium instance work. Put the
directory in git to keep a history, and to share it with other Terrarium
instances (`terrarium admin config sync`).

| Path | Content |
|---|---|
| `system-prompts/<name>.md` | The system prompt library. An entry replaces the harness's default system prompt. Leads (`project_admin`, `sysadmin`) get `leader.md`, members get `worker.md`, unless the agent or its template selects another entry (`persona = "<name>"`). A first line `<!-- description: ... -->` gives the short text of the selector. The API, the CLI (`terrarium system-prompt`), and the dashboard (Settings, System prompts, and "Save as new system prompt" under a custom text) write these files; you can edit them by hand, and an agent gets the change at its next start. An older repo had the personas in `prompts/`: the first start with these defaults moves them here. |
| `prompts/default.md`, `prompts/project-lead.md`, `prompts/system-lead.md` | The Terrarium protocol templates. The daemon appends one after the persona. `prompts/README.md` tells how to write one. |
| `harness/claude/settings.json` | Claude Code settings for every Claude agent. |
| `harness/codex/config.toml` | Codex settings, given as `-c` overrides. |
| `harness/copilot/settings.json` | Copilot CLI settings, merged into the settings copy of each agent. |
| `harness/pi/settings.json` | pi settings, merged into the settings copy of each agent. |
| `skills/<name>/` | The skill library: one folder for each skill, with `SKILL.md` (front matter `name` and `description`, then the instructions) and optional `scripts/`, `references/`, `assets/`. An agent gets only the skills that its `skills` list names. Terrarium ships one skill, `terrarium-db` (the project DB); add it to the `skills` list of an agent or a template (`terrarium agent set <agent> --skill terrarium-db`; the flag replaces the list, so name the other skills too). A packaged skill that you delete stays deleted. `terrarium skill validate` checks them. |
| `templates/<name>.wcl`, `templates/<project>/<name>.wcl` | The agent templates: one WCL file each, for a global template or a template of `<project>`. The file name is the template name. The API, the CLI (`terrarium template`), and the dashboard write these files; you can edit them by hand, and the daemon reads them again within 10 seconds. Terrarium ships eight global templates. A default template that you delete stays deleted. |
| `export/` | The declarative export of the schedules and projects (agents, targets, repos), as fleet files. The daemon writes it at each commit. The templates are not in it: their files are. |

A change applies at the next start or restart of an agent. A file that
does not parse stops the start of the agent, with the file and the line in
the error.

## Defaults and upgrades

Terrarium ships a default for each file in `prompts/`, `harness/`, and
`system-prompts/` (`leader.md`, `worker.md`),
the default templates in `templates/`, and the packaged skills in
`skills/`. At each start the daemon writes
a missing file (not a template or a packaged skill that you deleted). When a new Terrarium version
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
