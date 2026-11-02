// The webhook action of the schedule form (docs/api.md §19.5): the fields,
// the action to and from them, and the live preview of the body for a
// sample event. The preview follows the daemon's renderer
// (`terrarium_core::webhook`): the formats json, slack, discord, ntfy, and
// template; `{{path}}` placeholders over `{event, trigger}`, JSON-escaped
// when the content type is JSON; `{{path | json}}` for a JSON literal. The
// daemon has the last word: its `422` shows under the field that
// `details.field` names. The deliveries panel helpers are here too. Pure; no
// Solid imports.

import type { DeliveryStatus, WebhookAction, WebhookDelivery, WebhookFormat, WebhookHeader, WebhookMethod } from "../../api/types";

/** One row of the header editor: a fixed value, or the value of a secret. */
export interface HeaderRow {
  name: string;
  mode: "value" | "secret";
  value: string;
  secret: string;
}

/** The webhook fields of the schedule form. The time limit is the form's `timeout`. */
export interface WebhookFields {
  /** `url`: a URL in the action; `secret`: a secret whose value is the URL (a Slack or Discord URL is a credential). */
  hookUrlMode: "url" | "secret";
  hookUrl: string;
  hookUrlSecret: string;
  hookMethod: WebhookMethod;
  hookFormat: WebhookFormat;
  hookTemplate: string;
  /** Empty: the default (application/json; text/plain for ntfy). */
  hookContentType: string;
  hookHeaders: HeaderRow[];
  /** Empty: no signature. */
  hookSignSecret: string;
  /** Empty: X-Terrarium-Signature. */
  hookSignHeader: string;
}

export type WebhookKey = keyof WebhookFields;

export const DEFAULT_TIMEOUT = 10;
export const MAX_TIMEOUT = 30;
export const SIGNATURE_HEADER = "X-Terrarium-Signature";
export const MAX_BODY = 256 * 1024;

export const WEBHOOK_FORMATS: readonly { id: WebhookFormat; label: string; hint: string }[] = [
  { id: "json", label: "JSON (the event)", hint: '{"event": {...}, "trigger": {...}}' },
  { id: "slack", label: "Slack", hint: '{"text": "<kind>: <summary>"}' },
  { id: "discord", label: "Discord", hint: '{"content": "<kind>: <summary>"}' },
  { id: "ntfy", label: "ntfy", hint: "the summary as text, with Title, Priority, and Tags" },
  { id: "template", label: "Template", hint: "your body, with {{event.*}} placeholders" },
];

/** A body template to start from. */
export const TEMPLATE_STARTER = '{\n  "kind": "{{event.kind}}",\n  "summary": "{{event.summary}}",\n  "project": "{{event.project}}",\n  "status": "{{event.data.to}}",\n  "data": {{event.data | json}}\n}';

/** The placeholders of a body template. */
export const WEBHOOK_PLACEHOLDERS: readonly { text: string; meaning: string }[] = [
  { text: "{{event.kind}}", meaning: "the kind, for example monitor.status" },
  { text: "{{event.summary}}", meaning: "the one-line summary" },
  { text: "{{event.project}}", meaning: "the project" },
  { text: "{{event.at}}", meaning: "the time (RFC 3339)" },
  { text: "{{event.data.to}}", meaning: "monitor.status: the new status (ok, warn, crit, unknown)" },
  { text: "{{event.data.value}}", meaning: "a monitor value" },
  { text: "{{trigger.name}}", meaning: "the name of this trigger" },
  { text: "{{event.data | json}}", meaning: "a value as a JSON literal (outside quotes)" },
];

export const emptyHeaderRow = (): HeaderRow => ({ name: "", mode: "value", value: "", secret: "" });

export const emptyWebhookFields = (): WebhookFields => ({
  hookUrlMode: "url",
  hookUrl: "",
  hookUrlSecret: "",
  hookMethod: "POST",
  hookFormat: "json",
  hookTemplate: "",
  hookContentType: "",
  hookHeaders: [],
  hookSignSecret: "",
  hookSignHeader: "",
});

/** The fields of an action, for Edit; `timeout` is the form's time limit. */
export function webhookFieldsFromAction(a: WebhookAction): WebhookFields & { timeout: string } {
  return {
    hookUrlMode: a.url_secret ? "secret" : "url",
    hookUrl: a.url ?? "",
    hookUrlSecret: a.url_secret ?? "",
    hookMethod: a.method ?? "POST",
    hookFormat: a.format ?? "json",
    hookTemplate: a.template ?? "",
    hookContentType: a.content_type ?? "",
    hookHeaders: (a.headers ?? []).map((h) => (h.secret ? { name: h.name, mode: "secret", value: "", secret: h.secret } : { name: h.name, mode: "value", value: h.value ?? "", secret: "" })),
    hookSignSecret: a.signing?.secret ?? "",
    hookSignHeader: a.signing?.header && a.signing.header !== SIGNATURE_HEADER ? a.signing.header : "",
    timeout: a.timeout_secs ? String(a.timeout_secs) : "",
  };
}

/** The daemon's URL check: http or https, a host, no user or password. `null` when it passes. */
export function urlProblem(raw: string): string | null {
  const text = raw.trim();
  if (!text) return "Give the URL.";
  let u: URL;
  try {
    u = new URL(text);
  } catch {
    return "The URL does not parse.";
  }
  if (u.protocol !== "http:" && u.protocol !== "https:") return `The scheme ${u.protocol.replace(":", "")} is not allowed: use http or https.`;
  if (u.username || u.password) return "A URL with a user or a password is not allowed: put the credential in a header from a secret.";
  if (!u.hostname) return "The URL has no host.";
  return null;
}

const HEADER_NAME = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]{1,128}$/;
const RESERVED = ["host", "content-length", "content-type", "transfer-encoding", "connection", "upgrade", "te", "trailer", "keep-alive", "proxy-authorization", "proxy-connection"];

/** The daemon's header-name check. `null` when it passes. */
export function headerNameProblem(name: string): string | null {
  if (!HEADER_NAME.test(name)) return `Bad header name "${name}".`;
  const lower = name.toLowerCase();
  if (RESERVED.includes(lower)) return `The daemon or HTTP sets ${name}${lower === "content-type" ? ": use Content type" : ""}.`;
  if (lower.startsWith("x-terrarium-") && lower !== SIGNATURE_HEADER.toLowerCase()) return `X-Terrarium-* headers are the daemon's.`;
  return null;
}

// ----- the templates -----

type Piece = { text: string } | { path: string; raw: boolean };

function pieces(template: string): Piece[] | string {
  const out: Piece[] = [];
  let rest = template;
  for (;;) {
    const open = rest.indexOf("{{");
    if (open < 0) break;
    if (open > 0) out.push({ text: rest.slice(0, open) });
    const after = rest.slice(open + 2);
    const close = after.indexOf("}}");
    if (close < 0) return "`{{` without `}}`: write `{{event.kind}}`";
    const inner = after.slice(0, close).trim();
    const bar = inner.indexOf("|");
    let path = inner;
    let raw = false;
    if (bar >= 0) {
      const filter = inner.slice(bar + 1).trim();
      if (filter !== "json") return `unknown filter \`${filter}\` in \`{{${inner}}}\`: the only filter is \`json\``;
      path = inner.slice(0, bar).trim();
      raw = true;
    }
    const segs = path.split(".");
    if (!["event", "trigger"].includes(segs[0] ?? "") || segs.some((s) => !s)) return `unknown placeholder \`{{${inner}}}\`: use event.kind, event.summary, event.project, event.at, event.data.<path>, or trigger.name`;
    out.push({ path, raw });
    rest = after.slice(close + 2);
  }
  if (rest) out.push({ text: rest });
  return out;
}

/** The problem of a template, or `null`. */
export const templateProblem = (template: string): string | null => {
  const p = pieces(template);
  return typeof p === "string" ? p : null;
};

/** The value at a dotted path; a number segment indexes an array. */
export function lookup(doc: unknown, path: string): unknown {
  let at: unknown = doc;
  for (const seg of path.split(".")) {
    if (Array.isArray(at)) at = at[Number(seg)];
    else if (at && typeof at === "object") at = (at as Record<string, unknown>)[seg];
    else return undefined;
    if (at === undefined) return undefined;
  }
  return at;
}

/** Text escaped for the inside of a JSON string. */
export const jsonEscape = (s: string): string => JSON.stringify(s).slice(1, -1);

/** Fill a template from `{event, trigger}`, as the daemon does. */
export function renderTemplate(template: string, doc: unknown, jsonMode: boolean): string {
  const p = pieces(template);
  if (typeof p === "string") return template;
  return p
    .map((x) => {
      if ("text" in x) return x.text;
      const v = lookup(doc, x.path);
      if (x.raw) return JSON.stringify(v ?? null);
      const text = v === undefined || v === null ? "" : typeof v === "string" ? v : JSON.stringify(v);
      return jsonMode ? jsonEscape(text) : text;
    })
    .join("");
}

export const isJsonType = (ct: string): boolean => {
  const base = (ct.split(";")[0] ?? "").trim().toLowerCase();
  return base === "application/json" || base.endsWith("+json");
};

// ----- the sample event and the preview -----

/** The event of a delivery. */
export interface EventDoc {
  seq: number;
  id: string;
  kind: string;
  at: string;
  project: string | null;
  agent: string | null;
  actor: string | null;
  summary: string;
  data: Record<string, unknown>;
}

/** A sample event of `kind` (the first kind of the trigger), as the daemon's test send makes it. */
export function sampleEvent(kindGlob: string, project: string | null, at: string): EventDoc {
  let kind = kindGlob.trim() || "monitor.status";
  if (/[*?]/.test(kind)) {
    const k = kind.replace(/\?/g, "x");
    kind = k === "*" || k === "monitor.*" ? "monitor.status" : k.replace(/\*/g, "sample");
  }
  const p = project ?? "demo";
  let summary: string;
  let data: Record<string, unknown>;
  if (kind === "monitor.status") {
    summary = "monitor disk-usage: ok → crit 93 %";
    data = { project: p, monitor: "disk-usage", monitor_id: "01SAMPLE", from: "ok", to: "crit", value: 93, unit: "%", message: "/ is 93% full", error: null };
  } else if (kind === "monitor.result") {
    summary = "monitor disk-usage: crit 93 %";
    data = { project: p, monitor: "disk-usage", monitor_id: "01SAMPLE", status: "crit", value: 93, unit: "%", message: "/ is 93% full", error: null, duration_ms: 120 };
  } else if (kind.startsWith("ci.")) {
    summary = `${kind} on main`;
    data = { repo: "wil/terrarium", branch: "main", run_id: 42, run_url: "https://ci.example/run/42", conclusion: "failure" };
  } else {
    summary = `a sample ${kind} event`;
    data = { sample: true };
  }
  return { seq: 0, id: "01SAMPLE0000000000000000000", kind, at, project, agent: null, actor: "system", summary, data };
}

export const documentOf = (e: EventDoc, trigger: string, project: string | null) => ({ event: e, trigger: { name: trigger, project } });

/** `<kind>: <summary>`, with `[project]` first. */
export function summaryLine(e: EventDoc): string {
  const what = e.summary.trim() ? `${e.kind}: ${e.summary.trim()}` : e.kind;
  return e.project ? `[${e.project}] ${what}` : what;
}

const statusOf = (e: EventDoc): string => String((e.data.to as string | undefined) ?? (e.data.status as string | undefined) ?? "");

/** The ntfy priority: a monitor by its status (crit 5, warn 4, ok 2), a failure 4, else 3. */
export function ntfyPriority(e: EventDoc): number {
  const s = statusOf(e);
  if (e.kind.startsWith("monitor.")) return s === "crit" ? 5 : s === "warn" || s === "unknown" ? 4 : s === "ok" ? 2 : 3;
  if (e.kind.endsWith(".failed") || s === "failed" || s === "giving_up") return 4;
  return 3;
}

export function ntfyTags(e: EventDoc): string {
  const s = statusOf(e);
  const tag = s === "crit" ? "rotating_light" : s === "warn" || s === "unknown" ? "warning" : s === "ok" ? "white_check_mark" : s === "failed" || s === "giving_up" || e.kind.endsWith(".failed") ? "x" : "bell";
  return `${tag},${e.kind.replace(/,/g, "_")}`;
}

/** The `Content-Type` of an action. */
export const contentTypeOf = (a: Pick<WebhookAction, "content_type" | "format">): string => (a.content_type?.trim() ? a.content_type.trim() : a.format === "ntfy" ? "text/plain; charset=utf-8" : "application/json");

export interface Preview {
  method: WebhookMethod;
  url: string;
  headers: [string, string][];
  body: string;
  /** A JSON content type with a body that is not JSON. */
  problem: string | null;
}

/** The request of an action for an event: the body as the daemon renders it, the headers with `[secret:<name>]` for a secret value. */
export function previewRequest(a: WebhookAction, e: EventDoc, trigger: string, project: string | null): Preview {
  const doc = documentOf(e, trigger, project);
  const line = summaryLine(e);
  const ct = contentTypeOf(a);
  let body: string;
  const extra: [string, string][] = [];
  switch (a.format ?? "json") {
    case "json":
      body = JSON.stringify(doc);
      break;
    case "slack":
      body = JSON.stringify({ text: line });
      break;
    case "discord":
      body = JSON.stringify({ content: [...line].slice(0, 2000).join("") });
      break;
    case "ntfy":
      body = e.summary.trim() || e.kind;
      extra.push(["Title", e.project ? `${e.project}: ${e.kind}` : e.kind], ["Priority", String(ntfyPriority(e))], ["Tags", ntfyTags(e)]);
      break;
    case "template":
      body = renderTemplate(a.template ?? "", doc, isJsonType(ct));
      break;
  }
  const headers: [string, string][] = [["Content-Type", ct], ...extra];
  for (const h of a.headers ?? []) headers.push([h.name, h.secret ? `[secret:${h.secret}]` : (h.value ?? "")]);
  headers.push(["X-Terrarium-Delivery", "<delivery id>"], ["X-Terrarium-Event", e.kind], ["X-Terrarium-Timestamp", "<unix seconds>"]);
  if (a.signing?.secret) headers.push([a.signing.header || SIGNATURE_HEADER, `sha256=<HMAC of the body with ${a.signing.secret}>`]);
  let problem: string | null = null;
  if (isJsonType(ct)) {
    try {
      JSON.parse(body);
    } catch (err) {
      problem = `The content type is JSON, but the body is not valid JSON (${err instanceof Error ? err.message : String(err)}). Quote text placeholders ("{{event.summary}}") or use | json.`;
    }
  }
  if (new TextEncoder().encode(body).length > MAX_BODY) problem = `The body is over ${MAX_BODY} bytes.`;
  const url = a.url_secret ? `(secret ${a.url_secret})` : (a.url ?? "");
  return { method: a.method ?? "POST", url, headers, body, problem };
}

/** Pretty JSON when the body is JSON; else the body as it is. */
export function prettyBody(body: string): string {
  try {
    return JSON.stringify(JSON.parse(body), null, 2);
  } catch {
    return body;
  }
}

// ----- the action -----

type Built<T> = { value: T } | { error: string; field: WebhookKey | "timeout" };

/** The webhook action of the fields, or the reason it is not complete. */
export function buildWebhookAction(f: WebhookFields & { timeout: string }): Built<WebhookAction> {
  const a: WebhookAction = { type: "webhook" };
  if (f.hookUrlMode === "secret") {
    if (!f.hookUrlSecret.trim()) return { error: "Pick the secret that holds the URL.", field: "hookUrlSecret" };
    a.url_secret = f.hookUrlSecret.trim();
  } else {
    const p = urlProblem(f.hookUrl);
    if (p) return { error: p, field: "hookUrl" };
    a.url = f.hookUrl.trim();
  }
  if (f.hookMethod !== "POST") a.method = f.hookMethod;
  a.format = f.hookFormat;
  if (f.hookFormat === "template") {
    if (!f.hookTemplate.trim()) return { error: "Write the body template.", field: "hookTemplate" };
    const p = templateProblem(f.hookTemplate);
    if (p) return { error: `The template: ${p}.`, field: "hookTemplate" };
    a.template = f.hookTemplate;
  }
  if (f.hookContentType.trim()) a.content_type = f.hookContentType.trim();
  const headers: WebhookHeader[] = [];
  for (const [i, h] of f.hookHeaders.entries()) {
    if (!h.name.trim() && !h.value.trim() && !h.secret.trim()) continue;
    const p = headerNameProblem(h.name.trim());
    if (p) return { error: `Header ${i + 1}: ${p}`, field: "hookHeaders" };
    if (h.mode === "secret") {
      if (!h.secret.trim()) return { error: `Header ${i + 1}: pick the secret.`, field: "hookHeaders" };
      headers.push({ name: h.name.trim(), secret: h.secret.trim() });
    } else {
      if ([...h.value].some((ch) => ch.charCodeAt(0) < 0x20 || ch.charCodeAt(0) === 0x7f)) return { error: `Header ${i + 1}: a value has no control characters.`, field: "hookHeaders" };
      headers.push({ name: h.name.trim(), value: h.value.trim() });
    }
  }
  if (headers.length) a.headers = headers;
  if (f.hookSignSecret.trim()) {
    const header = f.hookSignHeader.trim();
    if (header) {
      const p = headerNameProblem(header);
      if (p) return { error: p, field: "hookSignHeader" };
    }
    a.signing = { secret: f.hookSignSecret.trim(), ...(header ? { header } : {}) };
  }
  if (f.timeout.trim()) {
    const n = Number(f.timeout.trim().replace(/s$/, ""));
    if (!Number.isInteger(n) || n < 1 || n > MAX_TIMEOUT) return { error: `The timeout is a whole number of seconds, 1 to ${MAX_TIMEOUT}.`, field: "timeout" };
    a.timeout_secs = n;
  }
  return { value: a };
}

// ----- the deliveries -----

export const DELIVERY_CHIP: Record<DeliveryStatus, string> = {
  pending: "info",
  ok: "ok",
  failed: "bad",
  giving_up: "bad",
  dropped: "warn",
};

/** The status in words: "giving up", "retry 2 at 10:31". */
export function deliveryStatusWords(d: Pick<WebhookDelivery, "status" | "attempt" | "next_attempt_at">): string {
  if (d.status === "giving_up") return "giving up";
  if (d.status === "pending" && d.attempt > 0 && d.next_attempt_at) return `retry ${d.attempt + 1} at ${new Date(d.next_attempt_at).toLocaleTimeString()}`;
  if (d.status === "pending") return "queued";
  return d.status;
}

/** May the delivery be sent again? Not a rate-limit note, not one that waits. */
export const canRedeliver = (d: Pick<WebhookDelivery, "status">): boolean => d.status !== "dropped" && d.status !== "pending";

/** Put a delivery in a newest-first list: replace the one with its ID, or add it first. */
export function upsertDelivery(list: WebhookDelivery[], d: WebhookDelivery): WebhookDelivery[] {
  const i = list.findIndex((x) => x.id === d.id);
  if (i >= 0) return list.map((x) => (x.id === d.id ? d : x));
  return [d, ...list];
}
