// The agent templates of the mock (docs/api.md §21), after the daemon's
// `api/templates.rs`: CRUD, the scope rules (a project, or global), the
// revision, `resolve` (the project's template first, then the global one;
// `global:` forces the global one), `fromAgent`, and the rights (`403
// forbidden`): everyone reads the global templates; a member reads its
// project's; a project_admin manages its project's; a global template needs
// a sysadmin. `applyTemplate` is the merge of `POST /api/agents` with
// `template`.

import type { Api } from "../client";
import { HARNESSES, RESTART_POLICIES, type Agent, type AgentCreate, type AgentDef, type AgentEvent, type AgentPolicy, type AgentTemplate, type TemplateRef, type TemplateSpec } from "../types";
import type { MockWorld } from "./data";
import type { ScheduleCaller } from "./schedules";

const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const ULID_RE = /^[0-9A-Z]{26}$/;
const SPEC_KEYS = ["harness", "subscription", "model", "prompt", "roles", "policy", "restart", "new_session_on_prompt_change"] as const;

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface TemplateMockContext {
  w: MockWorld;
  iso: () => string;
  ulid: (tag: string) => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: { kind: "template"; actor?: string; project?: string; summary: string; data: Record<string, unknown> }) => AgentEvent;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  agent: (key: string) => Agent;
  caller?: ScheduleCaller;
}

/** `project/name`, or `_/name` for a global template. */
export const templateKey = (t: Pick<AgentTemplate, "name" | "project">) => `${t.project ?? "_"}/${t.name}`;

export const templateRef = (t: AgentTemplate): TemplateRef => ({ id: t.id, name: t.name, project: t.project, revision: t.revision });

/** The daemon's normalized spec: the defaults filled in, the empty optionals left out. Throws a message on a bad spec. */
export function normalizeSpec(raw: unknown): TemplateSpec {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) throw new Error("spec: use a JSON object");
  const o = raw as Record<string, unknown>;
  for (const k of Object.keys(o)) {
    if (k === "name" || k === "project") throw new Error(`spec: \`${k}\` is not part of a template; the agent create gives it`);
    if (!(SPEC_KEYS as readonly string[]).includes(k)) throw new Error(`spec: unknown field \`${k}\``);
  }
  if (!HARNESSES.includes(o.harness as never)) throw new Error("spec: missing or bad `harness`: use claude, codex, copilot, or pi");
  if (o.restart !== undefined && !RESTART_POLICIES.includes(o.restart as never)) throw new Error(`spec: bad \`restart\` \`${String(o.restart)}\``);
  const p = (o.policy ?? {}) as Partial<AgentPolicy>;
  const policy: AgentPolicy = { targets: p.targets ?? [], mail_to: p.mail_to ?? ["*"], ...(p.cross_project?.length ? { cross_project: p.cross_project } : {}) };
  if (policy.mail_to.some((m) => !m.trim())) throw new Error("spec: policy.mail_to has an empty entry");
  for (const x of policy.cross_project ?? []) if (!/^[^/]+\/[^/]+$/.test(x)) throw new Error(`spec: policy.cross_project entry \`${x}\` must be \`project/agent\` or \`project/*\``);
  const roles = ((o.roles as string[] | undefined) ?? []).filter((r) => r !== "member");
  const spec: TemplateSpec = { harness: o.harness as TemplateSpec["harness"] };
  for (const k of ["subscription", "model", "prompt"] as const) if (typeof o[k] === "string" && o[k]) spec[k] = o[k];
  if (roles.length) spec.roles = roles as TemplateSpec["roles"];
  spec.policy = policy;
  spec.restart = (o.restart as TemplateSpec["restart"]) ?? "on-failure";
  if (o.new_session_on_prompt_change === false) spec.new_session_on_prompt_change = false;
  return spec;
}

/**
 * The merge of `POST /api/agents` with `template` (docs/api.md §21): each
 * field of the body wins; `policy` merges per key; `null` clears
 * `subscription`, `model`, or `prompt`.
 */
export function applyTemplate(spec: TemplateSpec, body: AgentCreate): AgentDef {
  const out: Record<string, unknown> = { ...structuredClone(spec) };
  for (const [k, v] of Object.entries(body)) {
    if (k === "template" || v === undefined) continue;
    if (v === null) delete out[k];
    else if (k === "policy" && out.policy) out.policy = { ...(out.policy as AgentPolicy), ...(v as Partial<AgentPolicy>) };
    else out[k] = v;
  }
  return out as unknown as AgentDef;
}

/** The seed: a global `worker` and a terrarium `reviewer`. */
export function seedTemplates(now: number): AgentTemplate[] {
  const ago = (min: number) => new Date(now - min * 60_000).toISOString();
  return [
    { id: "01JBTPL000000000000000000001", name: "worker", project: null, description: "A board worker: takes cards from ready, mails its lead.", spec: { harness: "claude", prompt: "worker", policy: { targets: [], mail_to: ["*"] }, restart: "on-failure" }, revision: 2, created_by: "operator", source: "api", created_at: ago(60 * 24 * 3), updated_at: ago(60 * 24) },
    { id: "01JBTPL000000000000000000002", name: "reviewer", project: "terrarium", description: "Reviews versions; codex with a model of its own.", spec: { harness: "codex", model: "gpt-5.5", prompt: "worker", policy: { targets: ["build-box"], mail_to: ["lead"] }, restart: "on-failure" }, revision: 1, created_by: "operator", source: "api", created_at: ago(60 * 24 * 2), updated_at: ago(60 * 24 * 2) },
  ];
}

/** The template that `key` names for an agent of `project` (the resolution of the agent create). */
export function resolveTemplate(templates: AgentTemplate[], key: string, project: string, fail: Fail): AgentTemplate {
  const missing = (): never => fail(404, "not_found", `no template \`${key}\` for project \`${project}\``);
  let hit: AgentTemplate | undefined;
  if (ULID_RE.test(key)) hit = templates.find((t) => t.id === key);
  else if (key.startsWith("global:")) hit = templates.find((t) => t.project === null && t.name === key.slice(7));
  else if (key.includes("/")) {
    const [p, n] = key.split("/", 2);
    hit = templates.find((t) => t.name === n && (t.project ?? "_") === p);
  } else hit = templates.find((t) => t.project === project && t.name === key) ?? templates.find((t) => t.project === null && t.name === key);
  if (!hit) return missing();
  if (hit.project !== null && hit.project !== project) fail(422, "invalid", `template \`${templateKey(hit)}\` belongs to project \`${hit.project}\`; an agent of \`${project}\` uses its project's templates and the global ones`, { field: "template" });
  return hit;
}

/** The `Api["templates"]` of the mock. */
export function createTemplateMock(c: TemplateMockContext): Api["templates"] {
  const { w, fail } = c;
  const caller = () => c.caller ?? { role: "operator" as const };
  const callerProject = () => {
    const who = caller();
    return "project" in who ? who.project : undefined;
  };
  const find = (key: string): AgentTemplate => {
    let hit: AgentTemplate | undefined;
    if (ULID_RE.test(key)) hit = w.templates.find((t) => t.id === key);
    else if (key.startsWith("global:")) hit = w.templates.find((t) => t.project === null && t.name === key.slice(7));
    else if (key.includes("/")) {
      const [p, n] = key.split("/", 2);
      hit = w.templates.find((t) => t.name === n && (t.project ?? "_") === p);
    } else {
      const mine = callerProject();
      hit = (mine ? w.templates.find((t) => t.project === mine && t.name === key) : undefined) ?? w.templates.find((t) => t.project === null && t.name === key);
    }
    return hit ?? fail(404, "not_found", `no template \`${key}\``);
  };
  const mayRead = (project: string | null) => {
    const who = caller();
    if (project === null || who.role === "operator" || who.role === "sysadmin") return;
    if (who.project !== project) fail(403, "forbidden", `project \`${project}\` is not your project; reading it needs sysadmin`);
  };
  const readable = (t: AgentTemplate) => {
    try {
      mayRead(t.project);
      return true;
    } catch {
      return false;
    }
  };
  const mayManage = (project: string | null) => {
    const who = caller();
    if (who.role === "operator" || who.role === "sysadmin") return;
    if (project === null) fail(403, "forbidden", "managing a global template needs sysadmin or the operator");
    if (who.role === "member" || who.project !== project) fail(403, "forbidden", `managing the templates of \`${project}\` needs project_admin of \`${project}\`, sysadmin, or the operator`);
  };
  const checkName = (name: unknown, project: string | null, self?: string) => {
    if (typeof name !== "string" || !NAME_RE.test(name)) fail(422, "invalid", `bad template name \`${String(name)}\`: use 1-63 of a-z, 0-9, \`-\`, \`_\`, starting with a letter or digit`, { field: "name" });
    if (w.templates.some((t) => t.name === name && t.project === project && t.id !== self)) fail(409, "conflict", `template \`${String(name)}\` exists in this scope`);
  };
  const spec = (raw: unknown): TemplateSpec => {
    try {
      return normalizeSpec(raw);
    } catch (e) {
      return fail(422, "invalid", (e as Error).message, { field: "spec" });
    }
  };
  const project = (p: string | null) => {
    if (p !== null && !w.projects.some((x) => x.name === p)) fail(404, "not_found", `no project \`${p}\``);
    return p;
  };
  const event = (t: AgentTemplate, change: string) =>
    c.emit({ kind: "template", actor: "operator", ...(t.project ? { project: t.project } : {}), summary: `template ${templateKey(t)} ${change}`, data: { change, template: t.id, name: t.name, key: templateKey(t), revision: t.revision } });
  const make = (name: string, scope: string | null, description: string | undefined, s: TemplateSpec): AgentTemplate => {
    mayManage(scope);
    checkName(name, scope);
    const t: AgentTemplate = { id: c.ulid("tpl"), name, project: scope, description: description ?? "", spec: s, revision: 1, created_by: "operator", source: "api", created_at: c.iso(), updated_at: c.iso() };
    w.templates.push(t);
    c.audit("template.create", t.id, t.project ?? undefined, { name: t.name });
    event(t, "created");
    return structuredClone(t);
  };

  return {
    list: (q = {}) =>
      c.reply(() => {
        if (q.project) {
          project(q.project);
          mayRead(q.project);
        }
        return w.templates
          .filter((t) => readable(t) && (q.scope === "global" ? t.project === null : !q.project || t.project === null || t.project === q.project))
          .slice()
          .sort((a, b) => (a.project ?? "").localeCompare(b.project ?? "") || a.name.localeCompare(b.name))
          .map((t) => structuredClone(t));
      }),
    get: (key) =>
      c.reply(() => {
        const t = find(key);
        mayRead(t.project);
        return structuredClone(t);
      }),
    create: (body) =>
      c.reply(() => {
        const scope = body.project === undefined ? (callerProject() ?? null) : body.project || null;
        return make(body.name, project(scope), body.description, spec(body.spec));
      }),
    update: (key, patch) =>
      c.reply(() => {
        const t = find(key);
        mayRead(t.project);
        mayManage(t.project);
        if (t.source === "file") fail(409, "file_managed", `template \`${templateKey(t)}\` is declared in projects/${t.project}.toml; edit the file, then POST /api/reload`);
        if (patch.name !== undefined) checkName(patch.name, t.project, t.id);
        const next = { name: patch.name ?? t.name, description: patch.description ?? t.description, spec: patch.spec !== undefined ? spec(patch.spec) : t.spec };
        if (JSON.stringify(next) === JSON.stringify({ name: t.name, description: t.description, spec: t.spec })) return structuredClone(t);
        Object.assign(t, next, { revision: t.revision + 1, updated_at: c.iso() });
        c.audit("template.update", t.id, t.project ?? undefined, { fields: Object.keys(patch), revision: t.revision });
        event(t, "updated");
        return structuredClone(t);
      }),
    remove: (key) =>
      c.reply(() => {
        const t = find(key);
        mayRead(t.project);
        mayManage(t.project);
        if (t.source === "file") fail(409, "file_managed", `template \`${templateKey(t)}\` is declared in projects/${t.project}.toml`);
        w.templates.splice(w.templates.indexOf(t), 1);
        c.audit("template.delete", t.id, t.project ?? undefined, { name: t.name });
        event(t, "deleted");
      }),
    resolve: (key, p) =>
      c.reply(() => {
        project(p);
        mayRead(p);
        const t = resolveTemplate(w.templates, key, p, fail);
        return { template: templateRef(t), agent: { project: p, ...structuredClone(t.spec) } };
      }),
    fromAgent: (agentKey, body) =>
      c.reply(() => {
        const a = c.agent(agentKey);
        mayRead(a.project);
        const scope = body.project === undefined ? a.project : body.project || null;
        const s = spec({
          harness: a.harness,
          ...(a.subscription ? { subscription: a.subscription } : {}),
          ...(a.model ? { model: a.model } : {}),
          ...(a.prompt ? { prompt: a.prompt } : {}),
          roles: a.roles,
          policy: a.policy,
          restart: a.restart,
        });
        return make(body.name, project(scope), body.description, s);
      }),
  };
}
