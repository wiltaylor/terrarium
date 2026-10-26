// The event sources of the mock (docs/api.md §20), after the daemon's
// `api/event_sources.rs`: CRUD, rotate, the hook URL and the setup steps of
// each sender, and the rights (`403 forbidden`): the operator and a sysadmin
// everything; a project_admin its project's; a member reads its project's.
// The secret is only in the answers of a create and a rotate; the world keeps
// it apart from the row, as the daemon keeps only what it needs.

import type { Api } from "../client";
import type { AgentEvent, EventSource, EventSourceKind, EventSourceWithSecret } from "../types";
import type { MockWorld } from "./data";
import type { ScheduleCaller } from "./schedules";

/** A source as the store keeps it: the row, with no hook fields and no secret. */
export type EventSourceRow = Omit<EventSource, "hook_path" | "hook_url" | "hook_note" | "setup">;

const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,62}$/;
const KINDS: readonly EventSourceKind[] = ["github", "forgejo", "generic"];
/** The mock's ingest listener: no public URL, so the daemon makes it from the address. */
const INGEST = "http://localhost:8083";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface EventSourceMockContext {
  w: MockWorld;
  iso: () => string;
  ulid: (tag: string) => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: { kind: "event_source"; actor?: string; project?: string; summary: string; data: Record<string, unknown> }) => AgentEvent;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  caller?: ScheduleCaller;
  /** 32 random bytes in hex. */
  secret?: () => string;
}

/** The daemon's `setup_steps`. */
export function setupSteps(kind: EventSourceKind, url: string): string[] {
  switch (kind) {
    case "github":
      return [
        "GitHub: the repository (or organisation) > Settings > Webhooks > Add webhook.",
        `Payload URL: ${url}`,
        "Content type: application/json.",
        "Secret: the secret of this source (shown at create and at rotate only).",
        'Events: "Let me select individual events": Workflow runs, Workflow jobs, Pushes, Pull requests.',
        "GitHub sends a ping at once; the source shows it in last_seen.",
      ];
    case "forgejo":
      return [
        "Forgejo: the repository > Settings > Webhooks > Add webhook > Forgejo.",
        `Target URL: ${url}`,
        "HTTP method: POST. POST content type: application/json.",
        "Secret: the secret of this source (shown at create and at rotate only).",
        "Trigger on: custom events: Push, Pull request, and the workflow run and workflow job events (Forgejo 17), or the action run events (Forgejo 12 to 16). Forgejo sends no commit status webhook.",
        'Use "Test delivery" to check it; the source shows it in last_seen.',
      ];
    case "generic":
      return [
        `POST ${url} with content type application/json.`,
        'Body: {"kind": "ci.run.failed", "summary": "...", "data": {...}}; the kind starts with ci., git., pr., ext., or agent.',
        "Auth: `Authorization: Bearer <secret>`, or `X-Terrarium-Signature: sha256=<hex>` (HMAC-SHA256 of the body with the secret).",
        "Optional: `X-Terrarium-Delivery: <id>`; a repeated ID is dropped.",
      ];
  }
}

/** A source as the API shows it. */
export function sourceView(r: EventSourceRow): EventSource {
  const hook_path = `/hooks/${r.project ?? "_"}/${r.name}`;
  const hook_url = `${INGEST}${hook_path}`;
  return { ...structuredClone(r), hook_path, hook_url, hook_note: "made from TERRARIUM_INGEST_ADDR; set TERRARIUM_INGEST_PUBLIC_URL to the URL that the forge reaches", setup: setupSteps(r.kind, hook_url) };
}

const randomSecret = () => {
  const b = new Uint8Array(32);
  globalThis.crypto.getRandomValues(b);
  return [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
};

/** The seed: a GitHub source of terrarium that has delivered, and a disabled global generic one with an error. */
export function seedEventSources(now: number): EventSourceRow[] {
  const ago = (min: number) => new Date(now - min * 60_000).toISOString();
  return [
    { id: "01JBSRC000000000000000000001", name: "gh", project: "terrarium", kind: "github", enabled: true, created_by: "operator", last_seen: ago(118), last_error: null, received: 12, created_at: ago(60 * 24 * 2), updated_at: ago(60 * 24 * 2) },
    { id: "01JBSRC000000000000000000002", name: "nightly", project: null, kind: "generic", enabled: false, created_by: "operator", last_seen: null, last_error: `${ago(300)} bad or missing signature`, received: 0, created_at: ago(60 * 24), updated_at: ago(60) },
  ];
}

/** The `Api["eventSources"]` of the mock. */
export function createEventSourceMock(c: EventSourceMockContext): Api["eventSources"] {
  const { w, fail } = c;
  const caller = () => c.caller ?? { role: "operator" as const };
  const secret = c.secret ?? randomSecret;
  const find = (key: string): EventSourceRow => {
    const [p, n] = key.includes("/") ? key.split("/", 2) : [undefined, key];
    const hit = w.eventSources.find((s) => s.id === key || (n !== undefined && s.name === n && (p === undefined || (s.project ?? "_") === p)));
    return hit ?? fail(404, "not_found", `no event source \`${key}\``);
  };
  const mayRead = (project: string | null) => {
    const who = caller();
    if (who.role === "operator" || who.role === "sysadmin") return;
    if (project === null) fail(403, "forbidden", "reading the global event sources needs sysadmin or the operator");
    if (who.project !== project) fail(403, "forbidden", `reading project \`${project}\` needs a member of it`);
  };
  const readable = (s: EventSourceRow) => {
    try {
      mayRead(s.project);
      return true;
    } catch {
      return false;
    }
  };
  const mayManage = (project: string | null) => {
    const who = caller();
    if (who.role === "operator" || who.role === "sysadmin") return;
    if (project === null) fail(403, "forbidden", "managing a global event source needs sysadmin or the operator");
    if (who.role === "member" || who.project !== project) fail(403, "forbidden", `managing the event sources of \`${project}\` needs project_admin of \`${project}\`, sysadmin, or the operator`);
  };
  const checkName = (name: unknown, project: string | null, self?: string) => {
    if (typeof name !== "string" || !NAME_RE.test(name)) fail(422, "invalid", `name: \`${String(name)}\` is not a valid name: lowercase letters, digits, - and _`, { field: "name" });
    if (w.eventSources.some((s) => s.name === name && s.project === project && s.id !== self)) fail(409, "conflict", `event source \`${String(name)}\` exists in ${project ? `project \`${project}\`` : "the global scope"}`);
  };
  const key = (s: EventSourceRow) => `${s.project ?? "_"}/${s.name}`;
  const event = (s: EventSourceRow, change: string) =>
    c.emit({ kind: "event_source", actor: "operator", ...(s.project ? { project: s.project } : {}), summary: `event source ${key(s)}: ${change}`, data: { change, source: s.id, name: s.name, key: key(s), kind: s.kind, enabled: s.enabled } });
  const withSecret = (s: EventSourceRow): EventSourceWithSecret => {
    const value = secret();
    w.eventSourceSecrets[s.id] = value;
    return { ...sourceView(s), secret: value };
  };

  return {
    list: (q = {}) =>
      c.reply(() => {
        if (q.project) {
          if (!w.projects.some((p) => p.name === q.project)) fail(404, "not_found", `no project \`${q.project}\``);
          mayRead(q.project);
        }
        return w.eventSources
          .filter((s) => readable(s) && (!q.project || s.project === null || s.project === q.project))
          .slice()
          .sort((a, b) => (a.project ?? "").localeCompare(b.project ?? "") || a.name.localeCompare(b.name))
          .map(sourceView);
      }),
    get: (id) =>
      c.reply(() => {
        const s = find(id);
        mayRead(s.project);
        return sourceView(s);
      }),
    create: (body) =>
      c.reply(() => {
        const project = body.project ? body.project : null;
        if (project && !w.projects.some((p) => p.name === project)) fail(404, "not_found", `no project \`${project}\``);
        mayManage(project);
        checkName(body.name, project);
        if (!KINDS.includes(body.kind)) fail(422, "invalid", `kind: \`${String(body.kind)}\` is not github, forgejo, or generic`, { field: "kind" });
        const s: EventSourceRow = { id: c.ulid("src"), name: body.name, project, kind: body.kind, enabled: body.enabled ?? true, created_by: "operator", last_seen: null, last_error: null, received: 0, created_at: c.iso(), updated_at: c.iso() };
        w.eventSources.push(s);
        c.audit("event_source.create", s.name, s.project ?? undefined, { id: s.id, kind: s.kind });
        event(s, "created");
        return withSecret(s);
      }),
    update: (id, patch) =>
      c.reply(() => {
        const s = find(id);
        mayRead(s.project);
        mayManage(s.project);
        if (patch.name !== undefined) checkName(patch.name, s.project, s.id);
        const toggled = patch.enabled !== undefined && patch.enabled !== s.enabled;
        Object.assign(s, { name: patch.name ?? s.name, enabled: patch.enabled ?? s.enabled, updated_at: c.iso() });
        c.audit("event_source.update", s.name, s.project ?? undefined, { id: s.id, fields: Object.keys(patch) });
        event(s, toggled ? (s.enabled ? "enabled" : "disabled") : "updated");
        return sourceView(s);
      }),
    rotate: (id) =>
      c.reply(() => {
        const s = find(id);
        mayRead(s.project);
        mayManage(s.project);
        s.updated_at = c.iso();
        c.audit("event_source.rotate", s.name, s.project ?? undefined, { id: s.id });
        event(s, "rotated");
        return withSecret(s);
      }),
    remove: (id) =>
      c.reply(() => {
        const s = find(id);
        mayRead(s.project);
        mayManage(s.project);
        w.eventSources.splice(w.eventSources.indexOf(s), 1);
        delete w.eventSourceSecrets[s.id];
        c.audit("event_source.delete", s.name, s.project ?? undefined, { id: s.id });
        event(s, "deleted");
      }),
  };
}
