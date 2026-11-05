// Tool proxies in the mock (docs/api.md §17.11, the daemon's api/tools.rs):
// the definitions with their grants, and grant and revoke. The operator
// sees every tool with `grants`; the run path is not mocked. The seed has
// the global file tool `tracker-read` granted to `terrarium/lead`, and the
// API project tool `pg-reset` of `webshop` granted to the project.

import type { Api } from "../client";
import type { Agent, Tool, ToolGrant } from "../types";
import type { MockWorld } from "./data";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface ToolMockContext {
  w: MockWorld & { tools?: Tool[] };
  iso: () => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
}

function seed(w: MockWorld, at: string): Tool[] {
  const lead = w.agents.find((a) => a.project === "terrarium" && a.name === "lead");
  const grant = (tool: string, grantee: string, label: string): ToolGrant => ({ tool, grantee, grantee_label: label, created_by: "operator", created_at: at });
  return [
    {
      name: "tracker-read",
      description: "Read tickets of the internal tracker",
      target: "pi-runner",
      command: ["/home/pi/bin/tracker-read"],
      args: { allow: ["show", "search"], deny_flags: ["--config"] },
      env_secrets: { TRACKER_TOKEN: "tracker-token" },
      timeout: "120s",
      scope: "global",
      source: "file",
      created_at: at,
      updated_at: at,
      grants: lead ? [grant("tracker-read", `agent:${lead.id}`, "agent:terrarium/lead")] : [],
    },
    {
      name: "pg-reset",
      description: "Reset the test database",
      target: "pg-test",
      project: "webshop",
      command: ["/usr/local/bin/pg-reset"],
      stdin: false,
      scope: "project",
      source: "api",
      created_at: at,
      updated_at: at,
      grants: [grant("pg-reset", "project:webshop", "project:webshop")],
    },
  ];
}

/** The tools that `agent` may run: its own grants and its project's. */
export function grantedTo(tools: Tool[], agent: Pick<Agent, "id" | "project">): Tool[] {
  return tools.filter(
    (t) => (!t.project || t.project === agent.project) && (t.grants ?? []).some((g) => g.grantee === `agent:${agent.id}` || g.grantee === `project:${agent.project}`),
  );
}

export function createToolMock(c: ToolMockContext) {
  const { w, fail, reply } = c;
  const all = () => (w.tools ??= seed(w, c.iso()));
  const find = (name: string) => all().find((t) => t.name === name) ?? fail(404, "not_found", `no tool \`${name}\``);
  /** `agent:<project>/<name>`, `agent:<id>`, or `project:<name>`: the stored key and the label. */
  const grantee = (s: string): { key: string; label: string; project: string } => {
    if (s.startsWith("agent:")) {
      const k = s.slice(6);
      const a = k.includes("/") ? w.agents.find((x) => `${x.project}/${x.name}` === k) : w.agents.find((x) => x.id === k);
      if (!a) fail(404, "not_found", `no agent \`${k}\``);
      return { key: `agent:${a!.id}`, label: `agent:${a!.project}/${a!.name}`, project: a!.project };
    }
    if (s.startsWith("project:") && s.length > 8) {
      const p = s.slice(8);
      if (!w.projects.some((x) => x.name === p)) fail(404, "not_found", `no project \`${p}\``);
      return { key: s, label: s, project: p };
    }
    return fail(422, "invalid", `grantee \`${s}\` is not \`agent:<project>/<name>\`, \`agent:<id>\`, or \`project:<name>\``);
  };
  const api: Api["tools"] = {
    list: () => reply(() => all().map((t) => ({ ...t, grants: [...(t.grants ?? [])] }))),
    get: (name) => reply(() => ({ ...find(name) })),
    remove: (name) =>
      reply(() => {
        const t = find(name);
        if (t.source === "file") fail(409, "file_managed", `tool \`${name}\` is declared in tools/${name}.toml; edit the file, then POST /api/reload`);
        w.tools = all().filter((x) => x.name !== name);
        c.audit("tool.delete", name, t.project, {});
      }),
    grants: (name) => reply(() => [...(find(name).grants ?? [])]),
    grant: (name, who) =>
      reply(() => {
        const t = find(name);
        const g = grantee(who);
        if (t.project && g.project !== t.project) fail(422, "invalid", `tool \`${name}\` belongs to project \`${t.project}\`: it goes only to that project, not to ${g.label}`);
        if ((t.grants ?? []).some((x) => x.grantee === g.key)) fail(409, "conflict", `${g.label} has tool \`${name}\` already`);
        const row: ToolGrant = { tool: name, grantee: g.key, grantee_label: g.label, created_by: "operator", created_at: c.iso() };
        t.grants = [...(t.grants ?? []), row].sort((a, b) => a.grantee.localeCompare(b.grantee));
        c.audit("tool.grant", name, t.project ?? g.project, { grantee: g.key, grantee_label: g.label });
        return row;
      }),
    revoke: (name, who) =>
      reply(() => {
        const t = find(name);
        const g = grantee(who);
        if (!(t.grants ?? []).some((x) => x.grantee === g.key)) fail(404, "not_found", `${g.label} has no grant of tool \`${name}\``);
        t.grants = (t.grants ?? []).filter((x) => x.grantee !== g.key);
        c.audit("tool.revoke", name, t.project ?? g.project, { grantee: g.key, grantee_label: g.label });
      }),
  };
  return { api, all };
}
