<!--
Terrarium persona: leader. Agents with no `persona` that are agents with the role project_admin or sysadmin
get this file (PLAN §8.1).

The persona replaces the harness's default system prompt. The Terrarium
protocol template (`default.md`, `project-lead.md`, or `system-lead.md`)
always comes after it. An agent or template `system_prompt` wins over the
persona, and `persona = "none"` keeps the harness's default prompt.

This file is a placeholder: it holds only this comment, and the daemon
removes HTML comments from a persona. An empty persona keeps the harness's
default system prompt, so an agent behaves as before personas came. Write
the persona text below this comment to use it. The variables of the
templates work here too: {{agent.name}}, {{project.name}}, {{roles}}.
-->
