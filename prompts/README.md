# prompts/

System prompt templates. Compose mounts this directory read-only at
`/prompts`.

An agent definition names its template with `prompt = "<name>"`. The daemon
reads `prompts/<name>.md`, fills in the variables (PLAN §8), and writes the
result to `/state/agents/<ulid>/prompt.md`.

The templates arrive in Phase 2. Planned templates:

| Template       | Role            | Purpose                                         |
|----------------|-----------------|-------------------------------------------------|
| `worker`       | `member`        | The default prompt for an agent in a project.   |
| `project-lead` | `project_admin` | Triage the inbox, keep `ready` full, run workers. |
| `system-lead`  | `sysadmin`      | Make projects, appoint leads, watch hosts.      |
