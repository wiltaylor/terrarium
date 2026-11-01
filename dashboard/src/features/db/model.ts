// The pure logic of the DB tab (docs/api.md §27): the form value of each
// field kind, the patch of an edit, the sort terms of the query box, the
// columns, the error caret, and the schema editor rows. The pages and the
// wiki block use it; the tests cover it.

import type { DbColumn, DbField, DbItemKind, DbKind, DbQueryResult, DbRecord, DbTypePatch } from "../../api/types";

/** The kinds, in the order of the schema editor. */
export const KINDS: DbKind[] = ["string", "text", "number", "integer", "bool", "date", "datetime", "enum", "list", "ref", "json"];
export const ITEM_KINDS: DbItemKind[] = ["string", "integer", "number"];
/** The table shows this many fields at first; the column chooser adds more. */
export const DEFAULT_COLUMNS = 6;

/** The value of one form input: text for most kinds, a list of texts for a list. A bool is `"true"`, `"false"`, or `""` (no value). */
export type FormValue = string | string[];
export type FormState = Record<string, FormValue>;

const pad = (n: number) => String(n).padStart(2, "0");

/** An RFC 3339 time as a `datetime-local` value, in the browser's time zone. */
export function toLocalInput(iso: string): string {
  const d = new Date(iso);
  if (isNaN(d.getTime())) return "";
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** A `datetime-local` value (local time) as RFC 3339 UTC. Null when it does not parse. */
export function fromLocalInput(s: string): string | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})(?::(\d{2}))?$/.exec(s.trim());
  if (!m) return null;
  const d = new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]), Number(m[4]), Number(m[5]), Number(m[6] ?? 0));
  return isNaN(d.getTime()) ? null : d.toISOString();
}

/** The form value of a stored value. */
export function toInput(f: DbField, v: unknown): FormValue {
  if (v === undefined || v === null) return f.kind === "list" ? [] : "";
  switch (f.kind) {
    case "list":
      return Array.isArray(v) ? v.map((x) => String(x)) : [];
    case "bool":
      return v ? "true" : "false";
    case "datetime":
      return typeof v === "string" ? toLocalInput(v) : "";
    case "json":
      return JSON.stringify(v, null, 2);
    default:
      return String(v);
  }
}

/** The empty form of a new record: the defaults of the schema. */
export function newForm(fields: DbField[]): FormState {
  return Object.fromEntries(fields.map((f) => [f.name, toInput(f, f.default)]));
}

/** The form of a record. */
export function recordForm(fields: DbField[], data: Record<string, unknown>): FormState {
  return Object.fromEntries(fields.map((f) => [f.name, toInput(f, data[f.name])]));
}

/** A form value as the value to send. `value: undefined` is no value. */
export function fromInput(f: DbField, input: FormValue | undefined, kind: DbKind | DbItemKind = f.kind): { value?: unknown; error?: string } {
  if (kind === "list") {
    const items = Array.isArray(input) ? input : typeof input === "string" && input.trim() ? input.split(",").map((x) => x.trim()) : [];
    if (!items.length) return {};
    const out: unknown[] = [];
    for (const [i, x] of items.entries()) {
      const r = fromInput(f, x, f.of ?? "string");
      if (r.error) return { error: `item ${i + 1}: ${r.error}` };
      out.push(r.value ?? "");
    }
    return { value: out };
  }
  const s = Array.isArray(input) ? input.join(",") : (input ?? "");
  if (kind === "text" || kind === "json" ? !s.trim() : s.trim() === "") return {};
  switch (kind) {
    case "number":
    case "integer": {
      const n = Number(s.trim());
      if (!isFinite(n)) return { error: "must be a number" };
      if (kind === "integer" && !Number.isInteger(n)) return { error: "must be an integer" };
      return { value: n };
    }
    case "bool":
      return s === "true" ? { value: true } : s === "false" ? { value: false } : { error: "must be true or false" };
    case "datetime": {
      const iso = fromLocalInput(s);
      return iso ? { value: iso } : { error: "must be a date and a time" };
    }
    case "json":
      try {
        return { value: JSON.parse(s) as unknown };
      } catch (e) {
        return { error: `is not JSON: ${(e as Error).message}` };
      }
    case "string":
    case "ref":
    case "enum":
    case "date":
      return { value: s.trim() };
    default:
      return { value: s };
  }
}

/** The data of a new record, and the problems by field. */
export function formData(fields: DbField[], form: FormState): { data: Record<string, unknown>; errors: Record<string, string> } {
  const data: Record<string, unknown> = {};
  const errors: Record<string, string> = {};
  for (const f of fields) {
    const r = fromInput(f, form[f.name]);
    if (r.error) errors[f.name] = r.error;
    else if (r.value !== undefined) data[f.name] = r.value;
    else if (f.required) errors[f.name] = "is required";
  }
  return { data, errors };
}

const same = (a: unknown, b: unknown) => JSON.stringify(a ?? null) === JSON.stringify(b ?? null);

/**
 * The patch of an edit: only the changed fields; `null` for a field that
 * had a value and has none now. A datetime that the form shows to the
 * minute stays when its minute did not change.
 */
export function patchData(fields: DbField[], before: Record<string, unknown>, form: FormState): { data: Record<string, unknown>; errors: Record<string, string> } {
  const { data: next, errors } = formData(fields, form);
  const data: Record<string, unknown> = {};
  for (const f of fields) {
    if (errors[f.name]) continue;
    const old = before[f.name];
    const now = next[f.name];
    if (f.kind === "datetime" && typeof old === "string" && toInput(f, old) === form[f.name]) continue;
    if (now === undefined) {
      if (old !== undefined && old !== null) data[f.name] = null;
    } else if (!same(old, now)) data[f.name] = now;
  }
  return { data, errors };
}

// ---- the query box ----

/** The terms of a query, as written (a quoted part keeps its quotes). */
export function splitTerms(q: string): string[] {
  const out: string[] = [];
  let cur = "";
  let quoted = false;
  for (let i = 0; i < q.length; i++) {
    const c = q[i]!;
    if (quoted && c === "\\" && i + 1 < q.length) {
      cur += c + q[i + 1];
      i++;
      continue;
    }
    if (c === '"') quoted = !quoted;
    if (!quoted && /\s/.test(c)) {
      if (cur) out.push(cur);
      cur = "";
    } else cur += c;
  }
  if (cur) out.push(cur);
  return out;
}

/** The sort of `field` in the query: `asc`, `desc`, or null. The first `sort:` term counts. */
export function sortOf(q: string, field: string): "asc" | "desc" | null {
  for (const t of splitTerms(q)) {
    if (!t.startsWith("sort:")) continue;
    const first = t.slice(5).split(",")[0] ?? "";
    if (first === field || first === `+${field}`) return "asc";
    if (first === `-${field}`) return "desc";
    return null;
  }
  return null;
}

/** Click a header: ascending, then descending, then no sort. The other `sort:` terms go. */
export function toggleSort(q: string, field: string): string {
  const now = sortOf(q, field);
  const rest = splitTerms(q).filter((t) => !t.startsWith("sort:"));
  if (now === "asc") rest.push(`sort:-${field}`);
  else if (now === null) rest.push(`sort:${field}`);
  return rest.join(" ");
}

/** Quote a value for the query when it needs it. */
export function quoteValue(s: string): string {
  return /^[^\s"\\]+$/.test(s) ? s : `"${s.replace(/[\\"]/g, (c) => `\\${c}`)}"`;
}

/** The query with `type:` first: a query box always names its type. */
export function withType(q: string, type: string): string {
  const terms = splitTerms(q).filter((t) => !t.startsWith("type:"));
  return [`type:${type}`, ...terms].join(" ");
}

/** The query split at an error position, to mark the character (a space at the end). */
export function errorAt(q: string, position: number): { before: string; at: string; after: string } {
  const chars = Array.from(q);
  const p = Math.max(0, Math.min(position, chars.length));
  return { before: chars.slice(0, p).join(""), at: chars[p] ?? " ", after: chars.slice(p + 1).join("") };
}

/** The error of a bad query: the position and message from `details`, or null. */
export function queryErrorOf(e: unknown): { position: number; message: string } | null {
  const d = (e as { details?: { position?: unknown; message?: unknown } } | null)?.details;
  if (d && typeof d.position === "number" && typeof d.message === "string") return { position: d.position, message: d.message };
  return null;
}

// ---- columns and cells ----

/** The first columns of a type. */
export const defaultColumns = (fields: DbField[], n = DEFAULT_COLUMNS): string[] => fields.slice(0, n).map((f) => f.name);

/** The value of a column of a record: a property for a meta column. */
export function columnValue(col: DbColumn, r: DbRecord): unknown {
  if (!col.meta) return r.data[col.name];
  switch (col.name) {
    case "id":
      return r.id;
    case "created":
      return r.created_at;
    case "updated":
      return r.updated_at;
    case "created_by":
      return r.created_by_name || r.created_by;
    case "updated_by":
      return r.updated_by_name || r.updated_by;
    case "revision":
      return r.revision;
    default:
      return undefined;
  }
}

/** A value as one line of text for a cell. A ref shows its label; a long text its first line. */
export function cellText(col: DbColumn, v: unknown, labels: Record<string, string> = {}): string {
  if (v === undefined || v === null || v === "") return "";
  if (col.kind === "ref" && typeof v === "string") return labels[v] ?? v;
  if (Array.isArray(v)) return v.map((x) => (typeof x === "string" ? x : JSON.stringify(x))).join(", ");
  if (col.kind === "datetime" && typeof v === "string") return /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}/.test(v) ? `${v.slice(0, 10)} ${v.slice(11, 16)} UTC` : v;
  if (col.kind === "bool") return v ? "yes" : "no";
  if (typeof v === "object") return JSON.stringify(v);
  const s = String(v);
  const first = s.split("\n")[0]!;
  return first.length < s.length ? `${first} …` : s;
}

/** The DB path of a type, with an optional query string. */
export function typeHref(project: string, type: string, q?: { q?: string; id?: string }): string {
  const base = `/p/${encodeURIComponent(project)}/knowledge/db/${encodeURIComponent(type)}`;
  const params = new URLSearchParams();
  if (q?.q) params.set("q", q.q);
  if (q?.id) params.set("id", q.id);
  const s = params.toString();
  return s ? `${base}?${s}` : base;
}

/** A group value as text: a ref shows its label. */
export function groupText(r: DbQueryResult, v: unknown): string {
  if (v === null || v === undefined) return "(none)";
  if (typeof v === "string") return r.labels[v] ?? v;
  return String(v);
}

// ---- the schema editor ----

/** One row of the schema editor. `orig` is the name in the saved schema (none for a new field). */
export interface FieldDraft {
  key: number;
  orig?: string;
  name: string;
  kind: DbKind;
  required: boolean;
  unique: boolean;
  /** The default as text: a list is `a,b`, json is JSON. */
  default: string;
  description: string;
  /** The enum values as `a,b,c`. */
  values: string;
  of: DbItemKind;
  ref: string;
}

let nextKey = 1;

export function draftOf(f: DbField): FieldDraft {
  return {
    key: nextKey++,
    orig: f.name,
    name: f.name,
    kind: f.kind,
    required: f.required,
    unique: !!f.unique,
    default: f.default === undefined || f.default === null ? "" : f.kind === "json" ? JSON.stringify(f.default) : Array.isArray(f.default) ? f.default.join(",") : String(f.default),
    description: f.description ?? "",
    values: (f.values ?? []).join(","),
    of: f.of ?? "string",
    ref: f.ref ?? "",
  };
}

export const emptyDraft = (): FieldDraft => ({ key: nextKey++, name: "", kind: "string", required: false, unique: false, default: "", description: "", values: "", of: "string", ref: "" });

/** A row as a field. `error` names the first problem of the row that the form can find. */
export function fieldOf(d: FieldDraft): { field?: DbField; error?: string } {
  const f: DbField = { name: d.name.trim(), kind: d.kind, required: d.required };
  if (d.description.trim()) f.description = d.description.trim();
  if (d.kind === "enum") f.values = d.values.split(",").map((v) => v.trim()).filter(Boolean);
  if (d.kind === "list") f.of = d.of;
  if (d.kind === "ref") f.ref = d.ref;
  if (d.unique) f.unique = true;
  if (d.default.trim() && d.kind !== "ref") {
    const raw = d.kind === "datetime" ? d.default.trim() : d.default;
    const r = d.kind === "datetime" ? { value: raw } : fromInput(f, raw);
    if (r.error) return { error: `field \`${f.name}\`: the default ${r.error}` };
    f.default = r.value;
  }
  return { field: f };
}

/** The patch of the schema editor: the renames, the new field list, and the removed fields. */
export function schemaPatch(revision: number, before: DbField[], drafts: FieldDraft[], description?: string): { patch?: DbTypePatch; removed: string[]; error?: string } {
  const fields: DbField[] = [];
  for (const d of drafts) {
    const r = fieldOf(d);
    if (r.error) return { removed: [], error: r.error };
    fields.push(r.field!);
  }
  const rename = drafts.filter((d) => d.orig && d.orig !== d.name.trim()).map((d) => ({ from: d.orig!, to: d.name.trim() }));
  const kept = new Set(drafts.map((d) => d.orig).filter(Boolean));
  const removed = before.map((f) => f.name).filter((n) => !kept.has(n));
  const patch: DbTypePatch = { revision, fields };
  if (rename.length) patch.rename = rename;
  if (description !== undefined) patch.description = description;
  if (removed.length) patch.confirm = true;
  return { patch, removed };
}
