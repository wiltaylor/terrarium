// The DB of the mock answers as the daemon does (docs/api.md §27,
// crates/terrariumd/src/store/records): the shapes, the error codes and
// details, the positions of query errors, and the query evaluation.
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import type { AgentEvent, DbField } from "../types";
import { parseQuery, QueryError } from "./dbQuery";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const P = "terrarium";
const f = (name: string, kind: DbField["kind"], extra: Partial<DbField> = {}): DbField => ({ name, kind, required: false, ...extra });

/** The error of a call that must fail. */
async function failure(p: Promise<unknown>): Promise<{ status: number; code: string; message: string; details: Record<string, unknown> }> {
  try {
    await p;
  } catch (e) {
    return e as never;
  }
  throw new Error("the call did not fail");
}

async function withBugs() {
  const api = mock();
  await api.db.createType(P, {
    name: "svc",
    fields: [f("name", "string", { required: true, unique: true }), f("port", "integer")],
  });
  for (const [name, port] of [
    ["auth", 1],
    ["billing", 2],
    ["search", 3],
  ] as const)
    await api.db.createRecord(P, "svc", { name, port });
  await api.db.createType(P, {
    name: "issue",
    fields: [
      f("title", "string", { required: true }),
      f("severity", "enum", { required: true, values: ["low", "medium", "high", "critical"] }),
      f("status", "enum", { values: ["open", "fixed"], default: "open" }),
      f("service", "ref", { ref: "svc" }),
      f("reported", "date"),
      f("points", "number"),
      f("tags", "list", { of: "string" }),
    ],
  });
  const rows: [string, string, string, string, number, string[]][] = [
    ["Login fails", "critical", "auth", "2026-10-01", 5, ["ui", "auth"]],
    ["Slow invoices", "medium", "billing", "2026-10-03", 3, ["perf"]],
    ["Typo on page", "low", "auth", "2026-10-05", 1, ["ui"]],
    ["Index lost", "high", "search", "2026-10-07", 8, []],
  ];
  for (const [title, severity, service, reported, points, tags] of rows) await api.db.createRecord(P, "issue", { title, severity, service, reported, points, tags });
  return api;
}

const titles = (r: { records: { data: Record<string, unknown> }[] }) => r.records.map((x) => x.data.title);

describe("mock DB: types and records", () => {
  it("seeds project terrarium with types and records", async () => {
    const api = mock();
    const types = await api.db.types(P);
    expect(types.map((t) => t.name)).toEqual(["benchmark", "bug", "decision", "service"]);
    expect(types.find((t) => t.name === "bug")!.records).toBeGreaterThan(20);
    const r = await api.db.query(P, "type:bug sort:-severity limit:3 fields:title,service");
    expect(r.records).toHaveLength(3);
    expect(Object.keys(r.records[0]!.data).sort()).toEqual(["service", "title"]);
    expect(Object.values(r.labels).length).toBeGreaterThan(0);
  });

  it("checks records: defaults, refs by name, unique, per-field problems", async () => {
    const api = await withBugs();
    const r = await api.db.createRecord(P, "issue", { title: "X", severity: "low", service: "billing" });
    expect(r.data.status).toBe("open");
    const billing = (await api.db.query(P, "type:svc name=billing")).records[0]!.id;
    expect(r.data.service).toBe(billing);
    expect(r).toMatchObject({ type: "issue", revision: 1, created_by: "operator" });
    let e = await failure(api.db.createRecord(P, "issue", { title: "X", severity: "low", service: "payments" }));
    expect(e).toMatchObject({ status: 422, code: "invalid" });
    expect(e.details.fields).toEqual({ service: "no `svc` record has name `payments`" });
    e = await failure(api.db.createRecord(P, "svc", { name: "auth" }));
    expect((e.details.fields as Record<string, string>).name).toMatch(/^must be unique; record /);
    e = await failure(api.db.createRecord(P, "issue", { severity: "urgent", colour: 1 }));
    expect(e.details.fields).toMatchObject({ title: "is required", severity: "must be one of: low, medium, high, critical", colour: expect.stringMatching(/^is not a field/) });
    expect(e.message).toMatch(/^the `issue` record is not valid: /);
  });

  it("merges a patch, refuses a stale revision with the current record", async () => {
    const api = await withBugs();
    const r = await api.db.createRecord(P, "issue", { title: "X", severity: "low", points: 1 });
    const r2 = await api.db.updateRecord(P, r.id, { points: null, title: "Y" }, 1);
    expect(r2.revision).toBe(2);
    expect(r2.data).not.toHaveProperty("points");
    const e = await failure(api.db.updateRecord(P, r.id, { title: "Z" }, 1));
    expect(e).toMatchObject({ status: 409, code: "conflict" });
    expect(e.details).toMatchObject({ current: 2, revision: 1, record: { id: r.id, revision: 2 } });
    const e2 = await failure(api.db.updateRecord(P, r.id, { title: null }));
    expect(e2.details.fields).toEqual({ title: "is required" });
  });

  it("keeps a referenced record and a target type", async () => {
    const api = await withBugs();
    const auth = (await api.db.query(P, "type:svc name=auth")).records[0]!.id;
    const e = await failure(api.db.removeRecord(P, auth));
    expect(e).toMatchObject({ status: 409, code: "conflict" });
    expect(e.details).toMatchObject({ total: 2, referrers: [{ type: "issue", field: "service" }, { type: "issue", field: "service" }] });
    const e2 = await failure(api.db.removeType(P, "svc", "records"));
    expect(e2.details).toEqual({ referrers: ["issue.service"] });
    const e3 = await failure(api.db.removeType(P, "issue", "true"));
    expect(e3).toMatchObject({ status: 409, code: "confirm_required", details: { records: 4 } });
    await api.db.removeType(P, "issue", "records");
    expect((await api.db.types(P)).some((t) => t.name === "issue")).toBe(false);
  });

  it("changes a schema: required with a default, kind refusal, removal with confirm, rename", async () => {
    const api = await withBugs();
    let t = await api.db.type(P, "issue");
    const fields = t.fields.map((x) => ({ ...x }));
    let e = await failure(api.db.updateType(P, "issue", { revision: t.revision, fields: [...fields, f("team", "string", { required: true })] }));
    expect(e.message).toBe("the new required field `team` needs a default for the 4 records of `issue`");
    t = await api.db.updateType(P, "issue", { revision: t.revision, fields: [...fields, f("team", "string", { required: true, default: "core" })] });
    expect((await api.db.query(P, "type:issue team=core")).count).toBe(4);
    e = await failure(api.db.updateType(P, "issue", { revision: 1, description: "x" }));
    expect(e).toMatchObject({ status: 409, code: "conflict", details: { current: t.revision, revision: 1 } });
    const asInt = t.fields.map((x) => (x.name === "title" ? { ...x, kind: "integer" as const } : x));
    e = await failure(api.db.updateType(P, "issue", { revision: t.revision, fields: asInt }));
    expect(e).toMatchObject({ status: 422, code: "invalid", details: { failing: 4 } });
    expect((e.details.records as { field: string; message: string }[])[0]).toMatchObject({ field: "title", message: "must be an integer" });
    const without = t.fields.filter((x) => x.name !== "points");
    e = await failure(api.db.updateType(P, "issue", { revision: t.revision, fields: without }));
    expect(e).toMatchObject({ status: 409, code: "confirm_required", details: { removed: ["points"] } });
    t = await api.db.updateType(P, "issue", { revision: t.revision, fields: without, confirm: true });
    await api.db.updateType(P, "issue", { revision: t.revision, rename: [{ from: "reported", to: "found_on" }] });
    expect(titles(await api.db.query(P, "type:issue found_on>=2026-10-05 sort:found_on"))).toEqual(["Typo on page", "Index lost"]);
    const e4 = await failure(api.db.createType(P, { name: "x", fields: [f("id", "string"), f("e", "enum")] }));
    expect(e4.details.problems).toEqual(["field `id`: the name is reserved (reserved: id, type, created, updated, created_by, updated_by, revision, sort, limit, fields, count, group, view)", "field `e`: an enum field needs `values`"]);
  });

  it("emits db.type and db.record events, and audits deletes", async () => {
    const api = mock();
    const seen: AgentEvent[] = [];
    const s = api.events.stream({ since: api.world.seq });
    s.subscribe((e) => seen.push(e));
    await new Promise((r) => setTimeout(r, 5));
    await api.db.createType(P, { name: "note", fields: [f("text", "string")] });
    const r = await api.db.createRecord(P, "note", { text: "hi" });
    await api.db.removeRecord(P, r.id);
    await new Promise((res) => setTimeout(res, 5));
    expect(seen.map((e) => [e.kind, e.data.change])).toEqual([
      ["db.type", "created"],
      ["db.record", "created"],
      ["db.record", "deleted"],
    ]);
    expect(seen[1]).toMatchObject({ project: P, data: { project: P, type: "note", id: r.id, revision: 1 } });
    expect(api.world.audit.at(-1)).toMatchObject({ action: "db.record.delete", target: `${P}/note/${r.id}` });
    s.close();
  });
});

describe("mock DB: queries", () => {
  it("orders enums, compares, finds text and list items, and resolves refs", async () => {
    const api = await withBugs();
    const q = (s: string) => api.db.query(P, s);
    expect(titles(await q("type:issue sort:-severity"))).toEqual(["Login fails", "Index lost", "Slow invoices", "Typo on page"]);
    expect(titles(await q("type:issue severity>=high sort:severity"))).toEqual(["Index lost", "Login fails"]);
    expect(titles(await q("type:issue title~LOGIN"))).toEqual(["Login fails"]);
    expect((await q("type:issue tags~UI")).count).toBe(2);
    expect((await q("type:issue tags!=ui")).count).toBe(2);
    expect((await q("type:issue reported<2026-10-04")).count).toBe(2);
    expect((await q("type:issue points>=3 points<8")).count).toBe(2);
    expect((await q("type:issue service=auth")).count).toBe(2);
    expect((await q('type:issue title="Slow invoices"')).count).toBe(1);
    await api.db.createRecord(P, "issue", { title: "Lonely", severity: "low" });
    expect((await q("type:issue service:null")).count).toBe(1);
    expect((await q("type:issue service!=auth")).count).toBe(3);
    expect((await q("type:issue service:!null")).count).toBe(4);
  });

  it("pages, counts, groups, and gives labels", async () => {
    const api = await withBugs();
    const r = await api.db.query(P, "type:issue limit:2 sort:title");
    expect(r).toMatchObject({ count: 4, next_cursor: "2", view: "table" });
    const r2 = await api.db.query(P, "type:issue limit:2 sort:title", { cursor: "2" });
    expect(titles(r2)).toEqual(["Slow invoices", "Typo on page"]);
    expect(r2.next_cursor).toBeUndefined();
    expect(await api.db.query(P, "type:issue count:true")).toMatchObject({ count: 4, view: "count", records: [] });
    const g = await api.db.query(P, "type:issue group:severity");
    expect(g.groups).toEqual([
      { value: "low", count: 1 },
      { value: "medium", count: 1 },
      { value: "high", count: 1 },
      { value: "critical", count: 1 },
    ]);
    const gs = await api.db.query(P, "type:issue group:service");
    expect(gs.groups![0]!.count).toBe(2);
    expect(gs.labels[gs.groups![0]!.value as string]).toBe("auth");
    expect((await api.db.query(P, "type:issue group:tags")).groups![0]).toEqual({ value: "ui", count: 2 });
    expect((await api.db.query(P, "severity=low", { type: "issue" })).count).toBe(1);
    const t = await api.db.query(P, "type:issue fields:title,created");
    expect(t.fields.map((c) => [c.name, !!c.meta])).toEqual([
      ["title", false],
      ["created", true],
    ]);
  });

  it("answers a bad query with 400 and the position", async () => {
    const api = await withBugs();
    const cases: [string, number, string][] = [
      ["type:issue sevrity=low", 11, "type `issue` has no field `sevrity`"],
      ["type:issue title", 16, "expected an operator after `title`"],
      ['type:issue title="open', 17, "this quote has no closing quote"],
      ["type:nope", 5, "project `terrarium` has no DB type `nope`"],
      ["title=x", 0, "the query needs type:<name>"],
      ["type:issue points=many", 18, "`points` is a number; `many` is not"],
      ["type:issue limit:501", 17, "limit: takes a number from 1 to 500"],
      ["type:issue sort:tags", 16, "cannot sort by `tags`, a list field"],
    ];
    for (const [q, position, message] of cases) {
      const e = await failure(api.db.query(P, q));
      expect(e, q).toMatchObject({ status: 400, code: "bad_request" });
      expect(e.details.position, q).toBe(position);
      expect(String(e.details.message), q).toContain(message);
      expect(e.message).toBe(`bad query: position ${position}: ${String(e.details.message)}`);
    }
    // A hostile value is a value.
    expect((await api.db.query(P, `type:issue title="x' OR '1'='1"`)).count).toBe(0);
  });

  it("parses the same terms as query.rs", () => {
    const q = parseQuery('type:bug title~"login page" sort:-severity,title limit:10 fields:a,b count:false view:list x:null y:!null');
    expect(q.type).toEqual({ name: "bug", pos: 5 });
    expect(q.filters.map((x) => [x.field.name, x.op, x.value])).toEqual([
      ["title", "~", "login page"],
      ["x", ":null", ""],
      ["y", ":!null", ""],
    ]);
    expect(q.sort.map((s) => [s.field.name, s.desc, s.field.pos])).toEqual([
      ["severity", true, 34],
      ["title", false, 43],
    ]);
    expect(q).toMatchObject({ limit: 10, view: "list", count: false });
    expect(() => parseQuery('title:"null"')).toThrow(QueryError);
  });
});
