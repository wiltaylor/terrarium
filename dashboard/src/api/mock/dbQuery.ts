// The query language of the DB for the mock, after the daemon's
// `store/records/query.rs` (docs/api.md §27). The daemon has the one real
// parser; this copy lets mock mode answer `GET .../db/query` in the same
// shape, with the same error messages and positions (a character offset).
// It runs over the records in memory, not SQL.

import type { DbColumn, DbField, DbKind, DbView } from "../types";

export const DEFAULT_LIMIT = 50;
export const MAX_LIMIT = 500;

export class QueryError extends Error {
  constructor(
    readonly position: number,
    message: string,
  ) {
    super(message);
  }
}

type Op = "=" | "!=" | ">" | ">=" | "<" | "<=" | "~" | ":null" | ":!null";

interface Named {
  name: string;
  pos: number;
}

interface Filter {
  field: Named;
  op: Op;
  value: string;
  valuePos: number;
}

export interface ParsedQuery {
  type?: Named;
  filters: Filter[];
  sort: { field: Named; desc: boolean }[];
  limit?: number;
  fields?: Named[];
  count: boolean;
  group?: Named;
  view?: DbView;
}

interface Term {
  key: string;
  keyPos: number;
  op: Op | ":";
  opPos: number;
  value: string;
  valuePos: number;
  quoted: boolean;
}

const isKeyChar = (c: string) => /^[A-Za-z0-9_-]$/.test(c);
const isSpace = (c: string) => /^\s$/u.test(c);

function terms(q: string): Term[] {
  const s = Array.from(q);
  const out: Term[] = [];
  let i = 0;
  while (i < s.length) {
    if (isSpace(s[i]!)) {
      i++;
      continue;
    }
    const keyPos = i;
    while (i < s.length && isKeyChar(s[i]!)) i++;
    const key = s.slice(keyPos, i).join("");
    if (!key)
      throw new QueryError(i, `expected a field name at \`${s[i]}\`; a term is field=value, field~text, field:null, type:<name>, sort:field`);
    const opPos = i;
    const two = s.slice(i, i + 2).join("");
    let op: Term["op"] | null = null;
    let n = 1;
    if (two === "!=" || two === ">=" || two === "<=") {
      op = two;
      n = 2;
    } else if (["=", ">", "<", "~", ":"].includes(s[i] ?? "")) op = s[i] as Term["op"];
    if (!op) throw new QueryError(opPos, `expected an operator after \`${key}\`: one of = != > >= < <= ~ :`);
    i += n;
    const valuePos = i;
    let value = "";
    let quoted = false;
    while (i < s.length && !isSpace(s[i]!)) {
      if (s[i] === '"') {
        const open = i;
        quoted = true;
        i++;
        for (;;) {
          const c = s[i];
          if (c === undefined) throw new QueryError(open, "this quote has no closing quote");
          if (c === '"') {
            i++;
            break;
          }
          if (c === "\\" && (s[i + 1] === '"' || s[i + 1] === "\\")) {
            value += s[i + 1];
            i += 2;
          } else {
            value += c;
            i++;
          }
        }
      } else {
        value += s[i];
        i++;
      }
    }
    if (!value && !quoted) throw new QueryError(valuePos, `\`${key}${op}\` needs a value (an empty text is "")`);
    out.push({ key, keyPos, op, opPos, value, valuePos, quoted });
  }
  return out;
}

function parts(t: Term): Named[] {
  const out: Named[] = [];
  let off = 0;
  for (const p of t.value.split(",")) {
    const lead = Array.from(p).findIndex((c) => !isSpace(c));
    out.push({ name: p.trim(), pos: t.quoted ? t.valuePos : t.valuePos + off + Math.max(0, lead) });
    off += Array.from(p).length + 1;
  }
  return out;
}

/** Parse a query (`query::parse`). */
export function parseQuery(q: string): ParsedQuery {
  const out: ParsedQuery = { filters: [], sort: [], count: false };
  for (const t of terms(q)) {
    const colon = t.op === ":";
    const dir = (name: string) => colon && t.key === name;
    if (t.key === "type") {
      if (!colon) throw new QueryError(t.opPos, "write the type as type:<name>");
      if (out.type) throw new QueryError(t.keyPos, "a query has one type:");
      out.type = { name: t.value, pos: t.valuePos };
    } else if (dir("sort")) {
      for (const p of parts(t)) {
        const sign = p.name[0] === "-" || p.name[0] === "+";
        const name = sign ? p.name.slice(1) : p.name;
        if (!name) throw new QueryError(p.pos, "sort: needs a field name, as sort:field or sort:-field");
        out.sort.push({ field: { name, pos: p.pos + (sign ? 1 : 0) }, desc: p.name[0] === "-" });
      }
    } else if (dir("limit")) {
      const n = /^\d+$/.test(t.value) ? Number(t.value) : NaN;
      if (!(n >= 1 && n <= MAX_LIMIT)) throw new QueryError(t.valuePos, `limit: takes a number from 1 to ${MAX_LIMIT}`);
      out.limit = n;
    } else if (dir("fields")) {
      const p = parts(t);
      const e = p.find((x) => !x.name);
      if (e) throw new QueryError(e.pos, "fields: takes field names separated by commas");
      out.fields = p;
    } else if (dir("count")) {
      if (t.value !== "true" && t.value !== "false") throw new QueryError(t.valuePos, "count: takes true or false");
      out.count = t.value === "true";
    } else if (dir("group")) {
      out.group = { name: t.value, pos: t.valuePos };
    } else if (dir("view")) {
      if (!["table", "list", "count", "group"].includes(t.value)) throw new QueryError(t.valuePos, "view: takes table, list, count, or group");
      out.view = t.value as DbView;
    } else if (colon) {
      if (t.quoted || (t.value !== "null" && t.value !== "!null"))
        throw new QueryError(t.opPos, `\`${t.key}:\` takes null or !null; to compare, write ${t.key}=value`);
      out.filters.push({ field: { name: t.key, pos: t.keyPos }, op: t.value === "null" ? ":null" : ":!null", value: "", valuePos: t.valuePos });
    } else {
      out.filters.push({ field: { name: t.key, pos: t.keyPos }, op: t.op as Op, value: t.value, valuePos: t.valuePos });
    }
  }
  return out;
}

// ---- evaluation ----

export interface TypeInfo {
  id: string;
  name: string;
  fields: DbField[];
}

/** The rows the evaluator reads: the stored records of one type. */
export interface Row {
  id: string;
  data: Record<string, unknown>;
  revision: number;
  created_by: string;
  updated_by: string;
  created_at: string;
  updated_at: string;
}

const META: Record<string, { kind: DbKind; key: keyof Row }> = {
  id: { kind: "string", key: "id" },
  created: { kind: "datetime", key: "created_at" },
  updated: { kind: "datetime", key: "updated_at" },
  created_by: { kind: "string", key: "created_by" },
  updated_by: { kind: "string", key: "updated_by" },
  revision: { kind: "integer", key: "revision" },
};

type Col = { meta: true; name: string; kind: DbKind; key: keyof Row } | { meta: false; field: DbField };

const colName = (c: Col) => (c.meta ? c.name : c.field.name);
const colKind = (c: Col) => (c.meta ? c.kind : c.field.kind);
const colValue = (c: Col, r: Row): unknown => (c.meta ? r[c.key] : r.data[c.field.name]);

function resolve(t: TypeInfo, n: Named): Col {
  const m = META[n.name];
  if (m) return { meta: true, name: n.name, ...m };
  const f = t.fields.find((x) => x.name === n.name);
  if (f) return { meta: false, field: f };
  throw new QueryError(
    n.pos,
    `type \`${t.name}\` has no field \`${n.name}\`; its fields: ${t.fields.map((x) => x.name).join(", ")} (and id, created, updated, created_by, updated_by, revision)`,
  );
}

export const parseDate = (s: string): string | null => {
  const v = s.trim();
  if (!/^\d{4}-\d{2}-\d{2}$/.test(v)) return null;
  const d = new Date(`${v}T00:00:00Z`);
  return !isNaN(d.getTime()) && d.toISOString().slice(0, 10) === v ? v : null;
};

const RFC3339 = /^\d{4}-\d{2}-\d{2}[Tt ]\d{2}:\d{2}:\d{2}(\.\d+)?([Zz]|[+-]\d{2}:\d{2})$/;
export const parseDatetime = (s: string): string | null => {
  const v = s.trim();
  if (!RFC3339.test(v)) return null;
  const d = new Date(v);
  return isNaN(d.getTime()) ? null : d.toISOString();
};

function relative(v: string): Date | null {
  if (v === "now" || v === "today") return new Date();
  const m = /^-(\d+)([dhmw])$/.exec(v);
  if (!m) return null;
  const ms = { d: 86400_000, h: 3600_000, m: 60_000, w: 7 * 86400_000 }[m[2] as "d"];
  return new Date(Date.now() - Number(m[1]) * ms);
}

function literal(kind: DbKind, c: Col, f: Filter): string | number {
  const v = f.value;
  const name = colName(c);
  const bad = (what: string): never => {
    throw new QueryError(f.valuePos, `\`${name}\` is ${what}; \`${v}\` is not`);
  };
  switch (kind) {
    case "string":
    case "text":
    case "ref":
      return v;
    case "integer":
    case "number": {
      const n = Number(v);
      return v.trim() !== "" && isFinite(n) ? n : bad("a number");
    }
    case "bool":
      return v === "true" ? 1 : v === "false" ? 0 : bad("true or false");
    case "date": {
      const d = parseDate(v) ?? relative(v)?.toISOString().slice(0, 10);
      return d ?? bad("a date (YYYY-MM-DD, today, or -7d)");
    }
    case "datetime": {
      const d = parseDatetime(v) ?? (parseDate(v) ? `${v}T00:00:00.000Z` : null) ?? relative(v)?.toISOString();
      return d ?? bad("a time (RFC 3339, YYYY-MM-DD, now, or -2h)");
    }
    case "enum": {
      const values = c.meta ? [] : (c.field.values ?? []);
      if (!values.includes(v)) throw new QueryError(f.valuePos, `\`${v}\` is not a value of \`${name}\`; its values: ${values.join(", ")}`);
      return v;
    }
    default:
      return bad("not a value to compare");
  }
}

/** A value as the evaluator compares it: a bool as 1/0 (as SQLite `json_extract` gives it). */
const norm = (v: unknown): string | number | null => (v === undefined || v === null ? null : typeof v === "boolean" ? (v ? 1 : 0) : (v as string | number));

function cmp(a: string | number, b: string | number): number {
  if (typeof a === "number" && typeof b === "number") return a - b;
  return String(a) < String(b) ? -1 : String(a) > String(b) ? 1 : 0;
}

function test(op: Op, a: string | number, b: string | number): boolean {
  const c = cmp(a, b);
  switch (op) {
    case "=":
      return c === 0;
    case "!=":
      return c !== 0;
    case ">":
      return c > 0;
    case ">=":
      return c >= 0;
    case "<":
      return c < 0;
    case "<=":
      return c <= 0;
    default:
      return false;
  }
}

const isUlid = (s: string) => /^[0-9A-Z]{26}$/.test(s);

export interface Lookup {
  /** Another type of the project by name. */
  type(name: string): TypeInfo | undefined;
  /** The rows of a type. */
  rows(typeId: string): Row[];
}

type Pred = (r: Row) => boolean;

function condition(t: TypeInfo, f: Filter, lookup: Lookup): Pred {
  const c = resolve(t, f.field);
  const name = colName(c);
  const kind = colKind(c);
  const refuse = (why: string): never => {
    throw new QueryError(f.field.pos, `\`${name}${f.op}\`: ${why}`);
  };
  if (f.op === ":null" || f.op === ":!null") {
    if (c.meta) refuse("a record always has this property");
    const want = f.op === ":null";
    return (r) => (norm(colValue(c, r)) === null) === want;
  }
  if (!c.meta && c.field.kind === "list") {
    const item: DbKind = c.field.of === "integer" ? "integer" : c.field.of === "number" ? "number" : "string";
    if (!["=", "~", "!="].includes(f.op)) refuse("a list takes = or ~ (has) and != (has not)");
    const v = literal(item, c, f);
    const has = (r: Row) => {
      const list = r.data[c.field.name];
      if (!Array.isArray(list)) return false;
      return list.some((x) => (item === "string" ? String(x).toLowerCase() === String(v).toLowerCase() : x === v));
    };
    return f.op === "!=" ? (r) => !has(r) : has;
  }
  if (f.op === "~") {
    if (kind !== "string" && kind !== "text") refuse("`~` (contains) is for string, text, and list fields");
    const needle = f.value.toLowerCase();
    return (r) => {
      const v = colValue(c, r);
      return typeof v === "string" && v.toLowerCase().includes(needle);
    };
  }
  if (!c.meta && c.field.kind === "json") refuse("a json field takes only :null and :!null");
  if (!c.meta && c.field.kind === "ref") {
    if (f.op !== "=" && f.op !== "!=") refuse("a ref field takes = and !=");
    let ids: Set<string>;
    if (isUlid(f.value)) ids = new Set([f.value]);
    else {
      const targetName = c.field.ref ?? "";
      const target = lookup.type(targetName);
      if (!target) throw new QueryError(f.field.pos, `the target type \`${targetName}\` of \`${name}\` is gone`);
      const label = target.fields.find((x) => x.kind === "string");
      if (!label) throw new QueryError(f.valuePos, `type \`${targetName}\` has no string field to find \`${f.value}\` by; give a record ID`);
      ids = new Set(lookup.rows(target.id).filter((x) => x.data[label.name] === f.value).map((x) => x.id));
    }
    return f.op === "=" ? (r) => ids.has(String(r.data[c.field.name] ?? "")) : (r) => r.data[c.field.name] === undefined || !ids.has(String(r.data[c.field.name]));
  }
  if (kind === "bool" && f.op !== "=" && f.op !== "!=") refuse("a bool field takes = and !=");
  if (c.meta && ["id", "created_by", "updated_by"].includes(c.name) && f.op !== "=" && f.op !== "!=") refuse("this property takes = and !=");
  const value = literal(kind, c, f);
  const order = kind === "enum" && !c.meta ? c.field.values ?? [] : null;
  return (r) => {
    const v = norm(colValue(c, r));
    if (v === null) return f.op === "!=";
    if (order && f.op !== "=" && f.op !== "!=") return test(f.op, order.indexOf(String(v)), order.indexOf(String(value)));
    return test(f.op, v, value);
  };
}

export interface Plan {
  columns: Col[];
  limit: number;
  count: boolean;
  group?: Col;
  view: DbView;
  matches: Pred;
  compare: (a: Row, b: Row) => number;
}

const sortValue = (c: Col, r: Row): string | number | null => {
  const v = norm(colValue(c, r));
  if (v === null) return null;
  if (!c.meta && c.field.kind === "enum") return (c.field.values ?? []).indexOf(String(v));
  return v;
};

/** Compile a parsed query for the type `t` (`query::compile`). */
export function compileQuery(t: TypeInfo, q: ParsedQuery, lookup: Lookup): Plan {
  const preds = q.filters.map((f) => condition(t, f, lookup));
  const keys = q.sort.map((k) => {
    const c = resolve(t, k.field);
    const kind = colKind(c);
    if (kind === "list" || kind === "json" || kind === "text") throw new QueryError(k.field.pos, `cannot sort by \`${colName(c)}\`, a ${kind} field`);
    return { c, desc: k.desc };
  });
  const columns = q.fields ? q.fields.map((n) => resolve(t, n)) : t.fields.map((f): Col => ({ meta: false, field: f }));
  let group: Col | undefined;
  if (q.group) {
    group = resolve(t, q.group);
    const kind = colKind(group);
    if (kind === "json" || kind === "text") throw new QueryError(q.group.pos, `cannot group by \`${colName(group)}\`, a ${kind} field`);
  }
  const view: DbView = q.view ?? (group ? "group" : q.count ? "count" : "table");
  if (view === "group" && !group) throw new QueryError(0, "view:group needs group:<field>");
  const compare = (a: Row, b: Row): number => {
    for (const { c, desc } of keys) {
      const x = sortValue(c, a);
      const y = sortValue(c, b);
      // A record with no value goes last in both directions.
      if (x === null || y === null) {
        if (x === y) continue;
        return x === null ? 1 : -1;
      }
      const d = cmp(x, y);
      if (d) return desc ? -d : d;
    }
    return keys.length ? cmp(a.id, b.id) : cmp(b.id, a.id);
  };
  return {
    columns,
    limit: q.limit ?? DEFAULT_LIMIT,
    count: q.count || view === "count",
    group,
    view,
    matches: (r) => preds.every((p) => p(r)),
    compare,
  };
}

export const columnOf = (c: Col): DbColumn => (c.meta ? { name: c.name, kind: c.kind, required: false, meta: true } : { ...c.field });

/** The groups of `rows` by the column `g`: an enum in its declared order, a bool true first, else the largest count first. */
export function groupRows(g: Col, rows: Row[], limit: number): { value: unknown; count: number }[] {
  const counts = new Map<string, { value: unknown; count: number }>();
  const add = (v: unknown) => {
    const k = JSON.stringify(v ?? null);
    const e = counts.get(k) ?? { value: v ?? null, count: 0 };
    e.count++;
    counts.set(k, e);
  };
  for (const r of rows) {
    const v = colValue(g, r);
    if (!g.meta && g.field.kind === "list") (Array.isArray(v) ? v : []).forEach(add);
    else add(v);
  }
  const list = [...counts.values()];
  if (!g.meta && g.field.kind === "enum") {
    const order = g.field.values ?? [];
    const idx = (v: unknown) => (v === null ? Infinity : order.indexOf(String(v)));
    list.sort((a, b) => idx(a.value) - idx(b.value));
  } else if (!g.meta && g.field.kind === "bool") list.sort((a, b) => Number(b.value) - Number(a.value));
  else list.sort((a, b) => b.count - a.count || cmp(String(a.value ?? ""), String(b.value ?? "")));
  return list.slice(0, limit);
}

export { colName };
export type { Col };
