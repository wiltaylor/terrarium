// The system prompt library of the mock, after the daemon's
// `api/system_prompts.rs` and `system_prompts.rs` (docs/api.md §28): an
// in-memory `system-prompts/` of the config repo. The seed has the packaged
// personas `leader` and `worker` (unedited defaults) and `reviewer`. The
// rights: the operator, a sysadmin, and a project_admin read and add; a
// change or a delete of an entry used outside the caller's project, or of
// `leader` or `worker`, needs sysadmin or the operator. A delete of an entry
// in use is `409 conflict` with `details.users`, unless `force`.
// `available: false` acts as a daemon with no config repo.

import type { Api } from "../client";
import type { AgentEvent, SystemPromptDetail, SystemPromptSummary, SystemPromptUser } from "../types";
import type { MockWorld } from "./data";
import type { ScheduleCaller } from "./schedules";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface SystemPromptMockContext {
  w: MockWorld;
  iso: () => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  emit: (d: { kind: "system_prompt"; actor?: string; summary: string; data: Record<string, unknown> }) => AgentEvent;
  caller?: ScheduleCaller;
  /** False: a daemon with no config repo. Default true. */
  available?: boolean;
}

/** The entries that every daemon has: the personas by role. */
export const BUILTIN_PROMPTS = ["leader", "worker"] as const;
/** The daemon's `SYSTEM_PROMPT_MAX`. */
const MAX_BYTES = 32 * 1024;
const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const SECRET_RE = /(sk-ant-[A-Za-z0-9_-]{8,}|sk-[A-Za-z0-9]{32,}|AKIA[0-9A-Z]{16}|ghp_[A-Za-z0-9]{20,}|-----BEGIN [A-Z ]*PRIVATE KEY-----)/;

interface Stored {
  description: string;
  text: string;
  /** An unedited packaged default. */
  isDefault: boolean;
  updated: string;
}

const utf8 = (s: string) => new TextEncoder().encode(s).length;
const oneLine = (s: string) => s.split(/\s+/).filter(Boolean).join(" ");
/** The file: the description line first when there is one. */
const fileText = (s: Stored) => (s.description ? `<!-- description: ${s.description} -->\n\n` : "") + s.text;

const LEADER = `# How you work

You are \`{{agent.name}}\`, a lead agent in Terrarium. You work for the
operator, the person who runs Terrarium. You plan the work, give it to
workers, check the result, and report.
`;
const WORKER = `# How you work

You are \`{{agent.name}}\`, a worker agent in the Terrarium project
\`{{project.name}}\`. Your lead gives you cards on the board. You do one card
at a time and report on the card.
`;
const REVIEWER = `# How you review

You are \`{{agent.name}}\`, a reviewer. Read each change on a card in
\`review\`. Run the tests on a target. Write what you found on the card, and
move it to \`done\` or back to \`in_progress\`.
`;

function seed(at: string): Map<string, Stored> {
  return new Map<string, Stored>([
    ["leader", { description: "The lead persona: plan the work, give it to workers, check it, and report.", text: LEADER, isDefault: true, updated: at }],
    ["worker", { description: "The worker persona: do one card at a time and report on the card.", text: WORKER, isDefault: true, updated: at }],
    ["reviewer", { description: "Review the cards in review and run the tests.", text: REVIEWER, isDefault: false, updated: at }],
  ]);
}

export function createSystemPromptMock(c: SystemPromptMockContext): { api: Api["systemPrompts"]; names: () => string[] } {
  const { w, fail } = c;
  const lib = seed(c.iso());
  const need = () => {
    if (c.available === false) fail(501, "not_available", "this daemon has no config repo (TERRARIUM_CONFIG_REPO)");
  };
  const caller = () => c.caller ?? { role: "operator" as const };
  const mayUse = (what: string) => {
    const r = caller().role;
    if (r === "member") fail(403, "forbidden", `${what} needs project_admin, sysadmin, or the operator`);
  };
  /** The agents and templates that select `name` (`leader` and `worker`: also by role). */
  const users = (name: string): SystemPromptUser[] => {
    const out: SystemPromptUser[] = [];
    for (const a of [...w.agents].sort((x, y) => `${x.project}/${x.name}`.localeCompare(`${y.project}/${y.name}`))) {
      const own = a.persona?.trim();
      const lead = a.roles.some((r) => r === "project_admin" || r === "sysadmin");
      const effective = own || (lead ? "leader" : "worker");
      if (effective === name) out.push({ kind: "agent", id: a.id, name: `${a.project}/${a.name}`, project: a.project, ...(own ? {} : { by_role: true }) });
    }
    for (const t of w.templates) {
      const p = typeof t.spec.persona === "string" ? t.spec.persona.trim() : "";
      if (p === name) out.push({ kind: "template", id: t.id, name: t.project ? `${t.project}/${t.name}` : t.name, ...(t.project ? { project: t.project } : {}) });
    }
    return out;
  };
  const mayChange = (name: string, what: string) => {
    mayUse(what);
    const who = caller();
    if (who.role === "operator" || who.role === "sysadmin") return;
    if ((BUILTIN_PROMPTS as readonly string[]).includes(name)) fail(403, "forbidden", `${what} \`${name}\` (the persona by role of every project) needs sysadmin or the operator`);
    const outside = users(name).filter((u) => u.project !== (who as { project: string }).project);
    if (outside.length) fail(403, "forbidden", `${what} \`${name}\` needs sysadmin or the operator: it is used outside project \`${(who as { project: string }).project}\` (${outside.map((u) => u.name).join(", ")})`);
  };
  const summary = (name: string, s: Stored): SystemPromptSummary => ({
    name,
    description: s.description,
    size: utf8(fileText(s)),
    source: s.isDefault ? "default" : "config",
    path: `system-prompts/${name}.md`,
    builtin: (BUILTIN_PROMPTS as readonly string[]).includes(name),
    updated: s.updated,
  });
  const detail = (name: string, s: Stored): SystemPromptDetail => ({ ...summary(name, s), text: s.text, users: users(name) });
  const find = (name: string) => lib.get(name) ?? fail(404, "not_found", `no system prompt \`${name}\` in the library (system-prompts/)`);
  const checked = (name: string, description: string, text: string): Stored => {
    if (name === "none") fail(422, "invalid", "`none` is not a system prompt name: it means the harness's default prompt");
    if (!NAME_RE.test(name)) fail(422, "invalid", `the system prompt name \`${name}\` is not valid: use 1-63 of a-z, 0-9, \`-\`, \`_\`, starting with a letter or a digit`);
    if (description.includes("-->")) fail(422, "invalid", "the description cannot hold `-->`");
    const body = text.replace(/^\s*<!-- description:[\s\S]*?-->\s*/, "");
    if (!body.trim()) fail(422, "invalid", "a system prompt needs `text`");
    const s: Stored = { description: oneLine(description), text: `${body.trimEnd()}\n`, isDefault: false, updated: c.iso() };
    const bytes = utf8(fileText(s));
    if (bytes > MAX_BYTES) fail(422, "invalid", `the system prompt has ${bytes} bytes; the limit is ${MAX_BYTES}`);
    if (SECRET_RE.test(fileText(s))) fail(422, "invalid", `the secret scan refused the system prompt (1 finding(s)): system-prompts/${name}.md:1: a token. A system prompt holds no secret`, { findings: [{ path: `system-prompts/${name}.md`, line: 1, rule: "token" }] });
    return s;
  };
  const changed = (name: string, change: "created" | "updated" | "deleted") => {
    const action = change === "created" ? "create" : change === "updated" ? "update" : "delete";
    c.audit(`system_prompt.${action}`, name, undefined, { name, path: `system-prompts/${name}.md` });
    c.emit({ kind: "system_prompt", actor: "operator", summary: `system prompt ${name} ${change}`, data: { change, name, path: `system-prompts/${name}.md` } });
  };

  const api: Api["systemPrompts"] = {
    list: () =>
      c.reply(() => {
        need();
        mayUse("reading the system prompt library");
        return [...lib].sort(([a], [b]) => a.localeCompare(b)).map(([n, s]) => summary(n, s));
      }),
    get: (name) =>
      c.reply(() => {
        need();
        mayUse("reading the system prompt library");
        return detail(name, find(name));
      }),
    create: (body) =>
      c.reply(() => {
        need();
        mayUse("adding a system prompt");
        const name = (body.name ?? "").trim();
        const exists = lib.has(name);
        if (exists && !body.replace) fail(409, "conflict", `the system prompt \`${name}\` exists already; change it with PUT /api/system-prompts/${name}, or send \`replace: true\``);
        if (exists) mayChange(name, "changing the system prompt");
        const s = checked(name, body.description ?? "", body.text ?? "");
        lib.set(name, s);
        changed(name, exists ? "updated" : "created");
        return detail(name, s);
      }),
    update: (name, body) =>
      c.reply(() => {
        need();
        const old = find(name);
        mayChange(name, "changing the system prompt");
        if (body.text === undefined && body.description === undefined) fail(422, "invalid", "give `text` or `description`");
        const s = checked(name, body.description ?? old.description, body.text ?? old.text);
        lib.set(name, s);
        changed(name, "updated");
        return detail(name, s);
      }),
    remove: (name, opts = {}) =>
      c.reply(() => {
        need();
        mayUse("deleting a system prompt");
        if ((BUILTIN_PROMPTS as readonly string[]).includes(name)) fail(409, "conflict", `\`${name}\` is the persona by role of every project: it cannot be deleted; edit it instead`);
        find(name);
        mayChange(name, "deleting the system prompt");
        const who = users(name);
        if (who.length && !opts.force)
          fail(409, "conflict", `the system prompt \`${name}\` is in use by ${who.length} (${who.map((u) => u.name).join(", ")}): pick another system prompt for them first, or delete with \`force=true\` (their next start then fails)`, { users: who });
        lib.delete(name);
        changed(name, "deleted");
      }),
  };
  return { api, names: () => [...lib.keys()] };
}
