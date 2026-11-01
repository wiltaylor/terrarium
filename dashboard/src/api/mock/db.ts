// The project DB of the mock, after the daemon's `store/records`
// (docs/api.md §27, PLAN §15.4): record types with a schema, records, and
// queries. The checks are those of `schema.rs` (field list, values) and
// `mod.rs` (refs, unique, revisions, referrers, schema changes), with the
// same error codes and `details`. The query language is ./dbQuery. The
// seed gives project `terrarium` the types service, bug, decision, and
// benchmark.

import type { Api } from "../client";
import type { AgentEvent, DbField, DbQueryResult, DbRecord, DbType, DbTypePatch, NewDbType } from "../types";
import { colName, columnOf, compileQuery, groupRows, parseDate, parseDatetime, parseQuery, QueryError, type Lookup, type Row, type TypeInfo } from "./dbQuery";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface DbMockContext {
  iso: () => string;
  ulid: (tag: string) => string;
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: { kind: AgentEvent["kind"]; actor: string; project: string; summary: string; data: Record<string, unknown> }) => unknown;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  /** Throws `404` for an unknown project. */
  project: (name: string) => unknown;
  /** The time of the seed records. */
  now: number;
}

export const MAX_FIELDS = 100;
export const MAX_STRING_CHARS = 1000;
export const MAX_TEXT_BYTES = 64 * 1024;
export const MAX_RECORD_BYTES = 64 * 1024;
const MAX_LIST_ITEMS = 1000;
export const RESERVED = ["id", "type", "created", "updated", "created_by", "updated_by", "revision", "sort", "limit", "fields", "count", "group", "view"];
const UNIQUE_KINDS = ["string", "number", "integer", "date", "datetime", "enum", "ref"];
export const TYPE_NAME_RE = /^[a-z][a-z0-9_-]{0,62}$/;
export const FIELD_NAME_RE = /^[a-z][a-z0-9_]{0,62}$/;

const bytes = (s: string) => new TextEncoder().encode(s).length;

/** Check one value against its field; the stored form (`schema::check_value`). */
export function checkValue(f: DbField, v: unknown, kind = f.kind): { ok: unknown } | { err: string } {
  const want = (what: string) => ({ err: `must be ${what}` });
  switch (kind) {
    case "string":
      if (typeof v !== "string") return want("a string");
      if (Array.from(v).length > MAX_STRING_CHARS) return { err: `has more than ${MAX_STRING_CHARS} characters (use a text field for long text)` };
      if (v.includes("\n")) return { err: "is one line (use a text field for more lines)" };
      return { ok: v };
    case "text":
      if (typeof v !== "string") return want("a string");
      return bytes(v) > MAX_TEXT_BYTES ? { err: "has more than 64 KiB" } : { ok: v };
    case "number":
      return typeof v === "number" && isFinite(v) ? { ok: v } : want("a number");
    case "integer":
      return typeof v === "number" && Number.isInteger(v) ? { ok: v } : want("an integer");
    case "bool":
      return typeof v === "boolean" ? { ok: v } : want("true or false");
    case "date": {
      const d = typeof v === "string" ? parseDate(v) : null;
      return d ? { ok: d } : want("a date YYYY-MM-DD");
    }
    case "datetime": {
      const d = typeof v === "string" ? parseDatetime(v) : null;
      return d ? { ok: d } : want("an RFC 3339 time, for example 2026-10-01T09:30:00Z");
    }
    case "enum": {
      const values = f.values ?? [];
      return typeof v === "string" && values.includes(v) ? { ok: v } : { err: `must be one of: ${values.join(", ")}` };
    }
    case "list": {
      if (!Array.isArray(v)) return want("a list");
      if (v.length > MAX_LIST_ITEMS) return { err: `has more than ${MAX_LIST_ITEMS} items` };
      const out: unknown[] = [];
      for (let i = 0; i < v.length; i++) {
        const r = checkValue(f, v[i], f.of ?? "string");
        if ("err" in r) return { err: `item ${i + 1}: ${r.err}` };
        out.push(r.ok);
      }
      return { ok: out };
    }
    case "ref":
      return typeof v === "string" && v.trim() && v.length <= 200 ? { ok: v.trim() } : want("the ID of a record (or the name of one)");
    case "json":
      return bytes(JSON.stringify(v)) > MAX_TEXT_BYTES ? { err: "has more than 64 KiB" } : { ok: v };
  }
}

/** The problems of a field list (`schema::check_fields`). */
export function checkFields(fields: DbField[]): string[] {
  const out: string[] = [];
  if (!fields.length) out.push("a type needs at least one field");
  if (fields.length > MAX_FIELDS) out.push(`a type has at most ${MAX_FIELDS} fields, not ${fields.length}`);
  const seen = new Set<string>();
  for (const f of fields) {
    const bad = (m: string) => out.push(`field \`${f.name}\`: ${m}`);
    if (!FIELD_NAME_RE.test(f.name)) bad("a field name is 1-63 of a-z, 0-9, `_`, starting with a letter");
    else if (RESERVED.includes(f.name)) bad(`the name is reserved (reserved: ${RESERVED.join(", ")})`);
    if (seen.has(f.name)) bad("two fields have this name");
    seen.add(f.name);
    if (f.kind === "enum") {
      const v = f.values;
      if (!v) bad("an enum field needs `values`");
      else {
        if (!v.length || v.length > 200) bad("an enum has 1 to 200 values");
        const s = new Set<string>();
        for (const x of v) {
          if (!x.trim() || x.length > 100 || x.trim() !== x) bad(`the enum value \`${x}\` is not valid: 1-100 characters, no space at the ends`);
          if (s.has(x)) bad(`the enum value \`${x}\` is there twice`);
          s.add(x);
        }
      }
    } else if (f.values) bad("only an enum field has `values`");
    if (f.kind === "list" && !f.of) bad("a list field needs `of` (string, integer, or number)");
    if (f.kind !== "list" && f.of) bad("only a list field has `of`");
    if (f.kind === "ref" && !f.ref) bad("a ref field needs `ref`, the name of the target type");
    else if (f.kind === "ref" && !TYPE_NAME_RE.test(f.ref!)) bad(`\`${f.ref}\` is not a valid type name`);
    if (f.kind !== "ref" && f.ref) bad("only a ref field has `ref`");
    if (f.unique && !UNIQUE_KINDS.includes(f.kind)) bad(`a ${f.kind} field cannot be unique`);
    if (f.default !== undefined && f.default !== null) {
      if (f.kind === "ref") bad("a ref field has no default");
      else {
        const r = checkValue(f, f.default);
        if ("err" in r) bad(`the default is not valid: ${r.err}`);
        else if (f.unique) bad("a unique field has no default");
      }
    }
  }
  return out;
}

const blank = (v: unknown) => typeof v === "string" && !v.trim();

/** Check `data` against `fields` (`schema::check_data`). Problems by field; `*` is the whole record. */
export function checkData(fields: DbField[], data: Record<string, unknown>, defaults: boolean): { ok: Record<string, unknown> } | { problems: Record<string, string> } {
  const problems: Record<string, string> = {};
  const out: Record<string, unknown> = {};
  for (const k of Object.keys(data)) if (!fields.some((f) => f.name === k)) problems[k] = `is not a field of this type (fields: ${fields.map((f) => f.name).join(", ")})`;
  for (const f of fields) {
    const given = data[f.name] ?? undefined;
    const value = given !== undefined && given !== null ? given : defaults && f.default !== undefined && f.default !== null ? f.default : undefined;
    if (value === undefined) {
      if (f.required) problems[f.name] = "is required";
    } else if (f.required && blank(value)) problems[f.name] = "is required";
    else {
      const r = checkValue(f, value);
      if ("err" in r) problems[f.name] = r.err;
      else out[f.name] = r.ok;
    }
  }
  if (!Object.keys(problems).length) {
    const size = bytes(JSON.stringify(out));
    if (size > MAX_RECORD_BYTES) problems["*"] = `the record has ${size} bytes; the limit is 64 KiB`;
  }
  return Object.keys(problems).length ? { problems } : { ok: out };
}

const problemText = (p: Record<string, string>) =>
  Object.entries(p)
    .map(([k, m]) => (k === "*" ? m : `${k}: ${m}`))
    .join("; ");

interface StoredType {
  id: string;
  project: string;
  name: string;
  description: string;
  fields: DbField[];
  revision: number;
  created_by: string;
  created_at: string;
  updated_at: string;
}

interface StoredRecord extends Row {
  project: string;
  type: string;
}

const nameOf = (addr: string) => addr;

export function createDbMock(ctx: DbMockContext) {
  const { reply, fail, iso, ulid } = ctx;
  const types: StoredType[] = [];
  const records: StoredRecord[] = [];
  // The seed writes no events and no audit rows.
  let seeding = false;
  const audit: DbMockContext["audit"] = (...a) => (seeding ? undefined : ctx.audit(...a));
  const emit: DbMockContext["emit"] = (d) => (seeding ? undefined : ctx.emit(d));

  const typeByName = (project: string, name: string) => types.find((t) => t.project === project && t.name === name);
  const needType = (project: string, name: string) => typeByName(project, name) ?? fail(404, "not_found", `project \`${project}\` has no DB type \`${name}\``);
  const rowsOf = (typeId: string) => records.filter((r) => r.type === typeId);
  const typeView = (t: StoredType): DbType => ({ ...structuredClone(t), records: rowsOf(t.id).length, created_by_name: nameOf(t.created_by) });
  const recordView = (r: StoredRecord): DbRecord => {
    const t = types.find((x) => x.id === r.type)!;
    return { id: r.id, project: r.project, type: t.name, data: structuredClone(r.data), revision: r.revision, created_by: r.created_by, created_by_name: nameOf(r.created_by), updated_by: r.updated_by, updated_by_name: nameOf(r.updated_by), created_at: r.created_at, updated_at: r.updated_at };
  };
  const schemaInvalid = (problems: string[]): never => fail(422, "invalid", `the schema is not valid: ${problems.join("; ")}`, { problems });
  const labelField = (t: { fields: DbField[] }) => t.fields.find((f) => f.kind === "string");
  const isUlid = (s: string) => /^[0-9A-Z]{26}$/.test(s);

  const emitType = (t: StoredType, change: string, extra: Record<string, unknown> = {}) =>
    emit({ kind: "db.type", actor: "operator", project: t.project, summary: `DB type ${t.name} ${change} (rev ${t.revision})`, data: { change, project: t.project, type: t.name, id: t.id, revision: t.revision, ...extra } });
  const emitRecord = (project: string, type: string, id: string, revision: number, change: string) =>
    emit({ kind: "db.record", actor: "operator", project, summary: `DB ${type} record ${id} ${change}`, data: { change, project, type, id, revision } });

  /** Refs: an ID of the target type, or the label of one record; unique fields (`check_record`). */
  const checkRecord = (project: string, t: { id: string; fields: DbField[] }, data: Record<string, unknown>, defaults: boolean, id: string, unique = true) => {
    const r = checkData(t.fields, data, defaults);
    if ("problems" in r) return r;
    const out = r.ok;
    const p: Record<string, string> = {};
    for (const f of t.fields.filter((x) => x.kind === "ref")) {
      const v = out[f.name];
      if (typeof v !== "string") continue;
      const target = typeByName(project, f.ref ?? "");
      if (!target) {
        p[f.name] = `the target type \`${f.ref}\` is gone`;
        continue;
      }
      if (isUlid(v)) {
        if (!rowsOf(target.id).some((x) => x.id === v)) p[f.name] = `no \`${target.name}\` record has the ID ${v}`;
        continue;
      }
      const label = labelField(target);
      if (!label) {
        p[f.name] = `give the ID of a \`${target.name}\` record (the type has no string field to find \`${v}\` by)`;
        continue;
      }
      const hits = rowsOf(target.id).filter((x) => x.data[label.name] === v);
      if (hits.length === 1) out[f.name] = hits[0]!.id;
      else if (!hits.length) p[f.name] = `no \`${target.name}\` record has ${label.name} \`${v}\``;
      else p[f.name] = `more than one \`${target.name}\` record has ${label.name} \`${v}\`; give the record ID`;
    }
    if (unique)
      for (const f of t.fields.filter((x) => x.unique)) {
        const v = out[f.name];
        if (v === undefined) continue;
        const o = rowsOf(t.id).find((x) => x.id !== id && x.data[f.name] === v);
        if (o) p[f.name] = `must be unique; record ${o.id} has this value`;
      }
    return Object.keys(p).length ? { problems: p } : { ok: out };
  };

  const checkTargets = (project: string, self: string, fields: DbField[]) => {
    const problems = fields.filter((f) => f.kind === "ref" && f.ref !== self && !typeByName(project, f.ref ?? "")).map((f) => `field \`${f.name}\`: project \`${project}\` has no type \`${f.ref}\``);
    if (problems.length) schemaInvalid(problems);
  };

  const createType = (project: string, n: NewDbType): StoredType => {
    ctx.project(project);
    if (!TYPE_NAME_RE.test(n.name)) fail(422, "invalid", `bad type name \`${n.name}\`: use 1-63 of a-z, 0-9, \`_\`, \`-\`, starting with a letter`);
    const problems = checkFields(n.fields ?? []);
    if (problems.length) schemaInvalid(problems);
    if (typeByName(project, n.name)) fail(409, "conflict", `project \`${project}\` has a DB type \`${n.name}\` already`);
    checkTargets(project, n.name, n.fields);
    const at = iso();
    const t: StoredType = { id: ulid("dbt"), project, name: n.name, description: n.description ?? "", fields: structuredClone(n.fields), revision: 1, created_by: "operator", created_at: at, updated_at: at };
    types.push(t);
    audit("db.type.create", `${project}/${t.name}`, project, { type_id: t.id, fields: t.fields.map((f) => f.name) });
    emitType(t, "created");
    return t;
  };

  const createRecord = (project: string, type: string, data: Record<string, unknown>, at = iso()): StoredRecord => {
    ctx.project(project);
    const t = needType(project, type);
    const id = ulid("dbr");
    const r = checkRecord(project, t, data ?? {}, true, id);
    if ("problems" in r) fail(422, "invalid", `the \`${t.name}\` record is not valid: ${problemText(r.problems)}`, { fields: r.problems });
    const rec: StoredRecord = { id, project, type: t.id, data: (r as { ok: Record<string, unknown> }).ok, revision: 1, created_by: "operator", updated_by: "operator", created_at: at, updated_at: at };
    records.push(rec);
    emitRecord(project, t.name, id, 1, "created");
    return rec;
  };

  const findRecord = (project: string, id: string) => {
    ctx.project(project);
    const r = records.find((x) => x.id === id && x.project === project);
    return r ?? fail(404, "not_found", `project \`${project}\` has no DB record \`${id}\``);
  };

  const updateType = (project: string, name: string, p: DbTypePatch): StoredType => {
    ctx.project(project);
    const t = needType(project, name);
    if (p.revision !== t.revision)
      fail(409, "conflict", `type \`${name}\` is at revision ${t.revision}, not your revision ${p.revision}; load it again and redo the change`, { current: t.revision, revision: p.revision });
    const old = structuredClone(t.fields);
    const renamed: [string, string][] = [];
    for (const r of p.rename ?? []) {
      if (!FIELD_NAME_RE.test(r.to)) fail(422, "invalid", `cannot rename \`${r.from}\` to \`${r.to}\`: a field name is 1-63 of a-z, 0-9, \`_\`, starting with a letter`);
      if (old.some((f) => f.name === r.to)) fail(422, "invalid", `cannot rename \`${r.from}\` to \`${r.to}\`: the type has a field \`${r.to}\``);
      const f = old.find((x) => x.name === r.from) ?? fail(422, "invalid", `cannot rename \`${r.from}\`: type \`${name}\` has no such field`);
      f.name = r.to;
      renamed.push([r.from, r.to]);
    }
    const next = structuredClone(p.fields ?? old);
    const problems = checkFields(next);
    if (problems.length) schemaInvalid(problems);
    checkTargets(project, name, next);
    const removed = old.filter((f) => !next.some((n) => n.name === f.name)).map((f) => f.name);
    if (removed.length && !p.confirm)
      fail(409, "confirm_required", `the change removes ${removed.map((r) => `\`${r}\``).join(", ")} and their data from every record; send confirm: true`, { removed });
    const same = (a: DbField, b: DbField) => JSON.stringify(a) === JSON.stringify(b);
    const added = next.filter((f) => !old.some((o) => o.name === f.name));
    const changed = next.filter((f) => old.some((o) => o.name === f.name && !same(o, f)));
    const rows = rowsOf(t.id).sort((a, b) => (a.id < b.id ? -1 : 1));
    for (const f of added)
      if (f.required && (f.default === undefined || f.default === null) && rows.length)
        fail(422, "invalid", `the new required field \`${f.name}\` needs a default for the ${rows.length} records of \`${name}\``);
    const failures: { id: string; field: string; message: string }[] = [];
    let failing = 0;
    const writes: [StoredRecord, Record<string, unknown>][] = [];
    const seen = new Map<string, string>();
    for (const rec of rows) {
      const data = structuredClone(rec.data);
      for (const [from, to] of renamed)
        if (from in data) {
          data[to] = data[from];
          delete data[from];
        }
      for (const r of removed) delete data[r];
      for (const f of [...added, ...changed.filter((x) => x.required)]) if (f.default !== undefined && f.default !== null && (data[f.name] === undefined || data[f.name] === null)) data[f.name] = structuredClone(f.default);
      const r = checkRecord(project, { id: t.id, fields: next }, data, false, rec.id, false);
      let probs: Record<string, string> = {};
      if ("problems" in r) probs = r.problems;
      else {
        for (const f of next.filter((x) => x.unique)) {
          const v = r.ok[f.name];
          if (v === undefined) continue;
          const key = `${f.name}\u0000${JSON.stringify(v)}`;
          const o = seen.get(key);
          if (o) probs[f.name] = `must be unique; record ${o} has this value`;
          else seen.set(key, rec.id);
        }
        if (!Object.keys(probs).length && JSON.stringify(r.ok) !== JSON.stringify(rec.data)) writes.push([rec, r.ok]);
      }
      const keys = Object.keys(probs).sort();
      if (keys.length) {
        failing++;
        if (failures.length < 20) failures.push({ id: rec.id, field: keys[0]!, message: probs[keys[0]!]! });
      }
    }
    if (failing) {
      const f = failures[0]!;
      fail(422, "invalid", `${failing} records of \`${name}\` do not fit the new schema (for example ${f.id}: ${f.field}: ${f.message}); change or delete them first, or change the schema`, { records: failures, failing });
    }
    const at = iso();
    for (const [rec, data] of writes) {
      rec.data = data;
      rec.revision++;
      rec.updated_by = "operator";
      rec.updated_at = at;
    }
    const descChanged = p.description !== undefined && p.description !== t.description;
    if (p.description !== undefined) t.description = p.description;
    t.fields = next;
    t.revision++;
    t.updated_at = at;
    audit("db.type.update", `${project}/${name}`, project, {
      type_id: t.id,
      revision: t.revision,
      added: added.map((f) => f.name),
      removed,
      renamed: renamed.map(([from, to]) => ({ from, to })),
      changed: changed.map((f) => f.name),
      description: descChanged,
      records_changed: writes.length,
    });
    emitType(t, "updated", { records_changed: writes.length });
    return t;
  };

  const removeType = (project: string, name: string, confirm?: string) => {
    ctx.project(project);
    const t = needType(project, name);
    const referrers = types.filter((o) => o.project === project && o.name !== name).flatMap((o) => o.fields.filter((f) => f.kind === "ref" && f.ref === name).map((f) => `${o.name}.${f.name}`));
    if (referrers.length) fail(409, "conflict", `type \`${name}\` is the target of ${referrers.join(", ")}; remove those fields first`, { referrers });
    const n = rowsOf(t.id).length;
    if (n === 0 && confirm !== "true" && confirm !== "records") fail(409, "confirm_required", `deleting type \`${name}\` needs ?confirm=true`);
    if (n > 0 && confirm !== "records")
      fail(409, "confirm_required", `type \`${name}\` has ${n} records; ?confirm=records deletes them with the type (project_admin, sysadmin, or the operator)`, { records: n });
    for (let i = records.length - 1; i >= 0; i--) if (records[i]!.type === t.id) records.splice(i, 1);
    types.splice(types.indexOf(t), 1);
    audit("db.type.delete", `${project}/${name}`, project, { type_id: t.id, revision: t.revision, records: n });
    emitType(t, "deleted", { records: n });
  };

  const updateRecord = (project: string, id: string, patch: Record<string, unknown>, revision?: number): StoredRecord => {
    const rec = findRecord(project, id);
    const t = types.find((x) => x.id === rec.type)!;
    if (revision !== undefined && revision !== rec.revision)
      fail(409, "conflict", `record ${id} is at revision ${rec.revision}, not your revision ${revision}; someone changed it — load it again`, { current: rec.revision, revision, record: recordView(rec) });
    const data = structuredClone(rec.data);
    for (const [k, v] of Object.entries(patch ?? {})) {
      if (v === null) delete data[k];
      else data[k] = v;
    }
    const r = checkRecord(project, t, data, false, id);
    if ("problems" in r) fail(422, "invalid", `the \`${t.name}\` record is not valid: ${problemText(r.problems)}`, { fields: r.problems });
    const ok = (r as { ok: Record<string, unknown> }).ok;
    if (JSON.stringify(ok) === JSON.stringify(rec.data)) return rec;
    rec.data = ok;
    rec.revision++;
    rec.updated_by = "operator";
    rec.updated_at = iso();
    emitRecord(project, t.name, id, rec.revision, "updated");
    return rec;
  };

  const removeRecord = (project: string, id: string) => {
    const rec = findRecord(project, id);
    const t = types.find((x) => x.id === rec.type)!;
    const referrers: { id: string; type: string; field: string }[] = [];
    let total = 0;
    for (const o of types.filter((x) => x.project === project))
      for (const f of o.fields.filter((x) => x.kind === "ref" && x.ref === t.name))
        for (const r of rowsOf(o.id).sort((a, b) => (a.id < b.id ? -1 : 1)))
          if (r.id !== id && r.data[f.name] === id) {
            total++;
            if (referrers.length < 20) referrers.push({ id: r.id, type: o.name, field: f.name });
          }
    if (total) fail(409, "conflict", `${total} records refer to record ${id}; change or delete them first`, { referrers, total });
    records.splice(records.indexOf(rec), 1);
    audit("db.record.delete", `${project}/${t.name}/${id}`, project, { type: t.name, id, revision: rec.revision });
    emitRecord(project, t.name, id, rec.revision, "deleted");
  };

  const queryError = (e: QueryError): never => fail(400, "bad_request", `bad query: position ${e.position}: ${e.message}`, { position: e.position, message: e.message });

  const runQuery = (project: string, q: string, scope?: string, cursor?: string): DbQueryResult => {
    ctx.project(project);
    try {
      const parsed = parseQuery(q);
      let typeName = parsed.type;
      if (typeName && scope && typeName.name !== scope) throw new QueryError(typeName.pos, `the query names type \`${typeName.name}\`, the request \`${scope}\``);
      if (!typeName && scope) typeName = { name: scope, pos: 0 };
      if (!typeName) throw new QueryError(0, "the query needs type:<name>");
      const t = typeByName(project, typeName.name);
      if (!t) {
        const names = types.filter((x) => x.project === project).map((x) => x.name).sort();
        throw new QueryError(typeName.pos, `project \`${project}\` has no DB type \`${typeName.name}\`; its types: ${names.length ? names.join(", ") : "none"}`);
      }
      const lookup: Lookup = { type: (n) => typeByName(project, n), rows: (id) => rowsOf(id) };
      const plan = compileQuery(t as TypeInfo, parsed, lookup);
      const offset = cursor ? Number(cursor) : 0;
      if (!Number.isInteger(offset) || offset < 0) fail(400, "bad_request", `bad cursor \`${cursor}\``);
      const hits = rowsOf(t.id).filter(plan.matches);
      const out: DbQueryResult = { type: t.name, query: q, view: plan.view, fields: plan.columns.map(columnOf), records: [], count: hits.length, labels: {} };
      const refIds = new Map<string, Set<string>>();
      const addRef = (target: string, id: unknown) => {
        if (typeof id === "string") (refIds.get(target) ?? refIds.set(target, new Set()).get(target)!).add(id);
      };
      if (plan.group) {
        const g = plan.group;
        out.group = colName(g);
        out.groups = groupRows(g, hits, plan.limit);
        if (!g.meta && g.field.kind === "ref") out.groups.forEach((x) => addRef(g.field.ref!, x.value));
      } else if (!plan.count) {
        const sorted = [...hits].sort(plan.compare);
        const page = sorted.slice(offset, offset + plan.limit);
        if (sorted.length > offset + plan.limit) out.next_cursor = String(offset + plan.limit);
        const keep = plan.columns.filter((c) => !c.meta).map((c) => (c.meta ? "" : c.field));
        out.records = page.map((r) => {
          const v = recordView(r);
          v.data = Object.fromEntries(Object.entries(v.data).filter(([k]) => keep.some((f) => f && f.name === k)));
          for (const f of keep) if (f && f.kind === "ref") addRef(f.ref!, v.data[f.name]);
          return v;
        });
      }
      for (const [target, ids] of refIds) {
        const tt = typeByName(project, target);
        const label = tt && labelField(tt);
        if (!tt || !label) continue;
        for (const id of ids) {
          const r = rowsOf(tt.id).find((x) => x.id === id);
          const l = r?.data[label.name];
          if (typeof l === "string") out.labels[id] = l;
        }
      }
      return out;
    } catch (e) {
      if (e instanceof QueryError) return queryError(e);
      throw e;
    }
  };

  const api: Api["db"] = {
    types: (project) =>
      reply(() => {
        ctx.project(project);
        return types.filter((t) => t.project === project).sort((a, b) => (a.name < b.name ? -1 : 1)).map(typeView);
      }),
    type: (project, name) =>
      reply(() => {
        ctx.project(project);
        return typeView(needType(project, name));
      }),
    createType: (project, body) => reply(() => typeView(createType(project, body))),
    updateType: (project, name, body) => reply(() => typeView(updateType(project, name, body))),
    removeType: (project, name, confirm) => reply(() => removeType(project, name, confirm)),
    createRecord: (project, type, data) => reply(() => recordView(createRecord(project, type, data))),
    record: (project, id) => reply(() => recordView(findRecord(project, id))),
    updateRecord: (project, id, data, revision) => reply(() => recordView(updateRecord(project, id, data, revision))),
    removeRecord: (project, id) => reply(() => removeRecord(project, id)),
    query: (project, q, o = {}) => reply(() => runQuery(project, q, o.type, o.cursor)),
  };

  const seed = () => {
    seeding = true;
    try {
      seedDb(ctx.now, createType, createRecord);
    } finally {
      seeding = false;
    }
  };
  return { api, seed };
}

// ---- seed ----

/** A small deterministic random source, so the seed is the same each load. */
function rng(seed: number) {
  let s = seed >>> 0;
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0;
    return s / 2 ** 32;
  };
}

const f = (name: string, kind: DbField["kind"], extra: Partial<DbField> = {}): DbField => ({ name, kind, required: false, ...extra });

/** The seed DB of project `terrarium`: services, bugs, decisions, and benchmarks. */
function seedDb(now: number, createType: (p: string, t: NewDbType) => unknown, createRecord: (p: string, t: string, d: Record<string, unknown>, at?: string) => StoredRecord) {
  const P = "terrarium";
  const rand = rng(42);
  const pick = <T>(xs: readonly T[]): T => xs[Math.floor(rand() * xs.length)]!;
  const day = 86400_000;
  const at = (msAgo: number) => new Date(now - msAgo).toISOString();
  const date = (daysAgo: number) => new Date(now - daysAgo * day).toISOString().slice(0, 10);

  createType(P, {
    name: "service",
    description: "The services of the platform, and who owns them.",
    fields: [
      f("name", "string", { required: true, unique: true }),
      f("owner", "string"),
      f("language", "enum", { values: ["rust", "go", "typescript", "python"] }),
      f("port", "integer"),
      f("healthy", "bool", { default: true }),
      f("url", "string"),
      f("tags", "list", { of: "string" }),
    ],
  });
  const services: [string, string, string, number][] = [
    ["auth", "dana", "rust", 8001],
    ["billing", "lee", "go", 8002],
    ["search", "sam", "rust", 8003],
    ["gateway", "dana", "go", 8000],
    ["notifications", "kim", "typescript", 8004],
    ["reports", "lee", "python", 8005],
    ["storage", "sam", "rust", 8006],
    ["scheduler", "kim", "go", 8007],
    ["web", "ari", "typescript", 3000],
    ["ml-ranker", "ari", "python", 8008],
  ];
  services.forEach(([name, owner, language, port], i) =>
    createRecord(P, "service", { name, owner, language, port, healthy: i !== 5, url: `https://${name}.internal.example`, tags: i % 3 ? ["core"] : ["core", "public"] }, at((40 - i) * day)),
  );

  createType(P, {
    name: "bug",
    description: "Bugs that the agents found or fixed.",
    fields: [
      f("title", "string", { required: true }),
      f("severity", "enum", { required: true, values: ["low", "medium", "high", "critical"] }),
      f("status", "enum", { required: true, values: ["open", "in_progress", "fixed", "wontfix"], default: "open" }),
      f("component", "string"),
      f("service", "ref", { ref: "service" }),
      f("reported", "date", { required: true }),
      f("fixed", "date"),
      f("assignee", "string"),
      f("notes", "text", { description: "Markdown: steps, logs, and the fix." }),
      f("tags", "list", { of: "string" }),
    ],
  });
  const subjects = ["Login form", "Token refresh", "Invoice PDF", "Search index", "Rate limiter", "Webhook retry", "Email digest", "CSV export", "Cache warmup", "Upload", "Session cookie", "Dark mode"];
  const problems = ["fails on retry", "leaks memory", "times out", "returns 500", "shows stale data", "drops the last item", "is slow under load", "ignores the locale"];
  const components = ["api", "ui", "worker", "db", "infra"];
  const people = ["dana", "lee", "sam", "kim", "ari"];
  for (let i = 0; i < 36; i++) {
    const reported = Math.floor(rand() * 45) + 1;
    const status = pick(["open", "open", "in_progress", "fixed", "fixed", "wontfix"] as const);
    const data: Record<string, unknown> = {
      title: `${pick(subjects)} ${pick(problems)}`,
      severity: pick(["low", "medium", "medium", "high", "critical"] as const),
      status,
      component: pick(components),
      service: pick(services)[0],
      reported: date(reported),
      assignee: pick(people),
      tags: rand() < 0.4 ? ["regression"] : rand() < 0.5 ? ["customer"] : [],
    };
    if (status === "fixed") data.fixed = date(Math.max(0, reported - Math.floor(rand() * 5) - 1));
    if (i % 4 === 0) data.notes = `## Steps\n\n1. Open the ${String(data.component)} page.\n2. Wait 30 s.\n\n\`\`\`\nERROR ${String(data.title).toLowerCase()}\n\`\`\`\n`;
    createRecord(P, "bug", data, at(reported * day));
  }

  createType(P, {
    name: "decision",
    description: "Architecture decisions.",
    fields: [
      f("title", "string", { required: true }),
      f("status", "enum", { required: true, values: ["proposed", "accepted", "superseded", "rejected"] }),
      f("date", "date", { required: true }),
      f("context", "text"),
      f("decision", "text", { required: true }),
      f("consequences", "text"),
    ],
  });
  const decisions: [string, string][] = [
    ["Use SQLite for the daemon store", "accepted"],
    ["One PTY per agent", "accepted"],
    ["Keep the dashboard in SolidJS", "accepted"],
    ["Move the queue to Postgres", "proposed"],
    ["Use gRPC between services", "rejected"],
    ["Store artifacts by SHA-256", "accepted"],
    ["Poll the harness for status", "superseded"],
  ];
  decisions.forEach(([title, status], i) =>
    createRecord(P, "decision", { title, status, date: date(60 - i * 7), context: `We need a choice for **${title.toLowerCase()}**.`, decision: `${title}.`, consequences: "- Simple to run.\n- One more thing to watch." }, at((60 - i * 7) * day)),
  );

  createType(P, {
    name: "benchmark",
    description: "Benchmark results of the services.",
    fields: [
      f("name", "string", { required: true }),
      f("service", "ref", { ref: "service" }),
      f("metric", "enum", { required: true, values: ["latency_ms", "throughput_rps", "memory_mb"] }),
      f("value", "number", { required: true }),
      f("run_at", "datetime", { required: true }),
      f("commit", "string"),
      f("notes", "text"),
    ],
  });
  const base: Record<string, number> = { latency_ms: 40, throughput_rps: 1200, memory_mb: 180 };
  for (let d = 14; d >= 0; d -= 1)
    for (const svc of ["auth", "search", "gateway"])
      for (const metric of ["latency_ms", "throughput_rps"] as const) {
        const ms = d * day + 3600_000 * (2 + (svc.length % 5));
        createRecord(P, "benchmark", { name: `${svc}-${metric}`, service: svc, metric, value: Math.round(base[metric]! * (0.8 + rand() * 0.4) * 10) / 10, run_at: at(ms), commit: Math.floor(rand() * 0xfffffff).toString(16).padStart(7, "0") }, at(ms));
      }
}
