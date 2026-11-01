import { describe, expect, it } from "vitest";
import type { DbColumn, DbField, DbRecord } from "../../api/types";
import {
  cellText,
  columnValue,
  defaultColumns,
  draftOf,
  emptyDraft,
  errorAt,
  formData,
  fromInput,
  fromLocalInput,
  newForm,
  patchData,
  queryErrorOf,
  quoteValue,
  recordForm,
  schemaPatch,
  sortOf,
  splitTerms,
  toggleSort,
  toInput,
  toLocalInput,
  typeHref,
  withType,
} from "./model";

const f = (name: string, kind: DbField["kind"], extra: Partial<DbField> = {}): DbField => ({ name, kind, required: false, ...extra });

const FIELDS: DbField[] = [
  f("title", "string", { required: true }),
  f("notes", "text"),
  f("points", "integer"),
  f("score", "number"),
  f("open", "bool"),
  f("reported", "date"),
  f("seen_at", "datetime"),
  f("severity", "enum", { values: ["low", "high"], default: "low" }),
  f("tags", "list", { of: "string" }),
  f("sizes", "list", { of: "integer" }),
  f("service", "ref", { ref: "service" }),
  f("extra", "json"),
];

describe("form values for each kind", () => {
  it("makes the form of a record and gives the same data back", () => {
    const data = {
      title: "Login fails",
      notes: "# Steps\n\n1. x",
      points: 3,
      score: 2.5,
      open: true,
      reported: "2026-10-01",
      seen_at: "2026-10-01T09:30:00.000Z",
      severity: "high",
      tags: ["ui", "auth"],
      sizes: [1, 2],
      service: "01JBDBR0000000000000000001",
      extra: { a: [1, null] },
    };
    const form = recordForm(FIELDS, data);
    expect(form.open).toBe("true");
    expect(form.tags).toEqual(["ui", "auth"]);
    expect(form.sizes).toEqual(["1", "2"]);
    expect(form.seen_at).toBe(toLocalInput("2026-10-01T09:30:00.000Z"));
    const back = formData(FIELDS, form);
    expect(back.errors).toEqual({});
    expect(back.data).toEqual(data);
  });

  it("converts and checks each input", () => {
    expect(fromInput(f("p", "integer"), "2.5")).toEqual({ error: "must be an integer" });
    expect(fromInput(f("p", "number"), "x")).toEqual({ error: "must be a number" });
    expect(fromInput(f("p", "number"), " ")).toEqual({});
    expect(fromInput(f("b", "bool"), "false")).toEqual({ value: false });
    expect(fromInput(f("b", "bool"), "")).toEqual({});
    expect(fromInput(f("j", "json"), "{")).toMatchObject({ error: expect.stringMatching(/^is not JSON/) });
    expect(fromInput(f("j", "json"), "null")).toEqual({ value: null });
    expect(fromInput(f("l", "list", { of: "number" }), ["1.5", "x"])).toEqual({ error: "item 2: must be a number" });
    expect(fromInput(f("l", "list", { of: "string" }), "a, b")).toEqual({ value: ["a", "b"] });
    expect(fromInput(f("d", "datetime"), "nonsense")).toEqual({ error: "must be a date and a time" });
    expect(fromInput(f("s", "string"), "  padded  ")).toEqual({ value: "padded" });
    expect(toInput(f("j", "json"), { a: 1 })).toBe('{\n  "a": 1\n}');
  });

  it("converts datetime-local in local time to UTC and back", () => {
    const iso = fromLocalInput("2026-10-01T09:30")!;
    expect(iso).toMatch(/Z$/);
    expect(toLocalInput(iso)).toBe("2026-10-01T09:30");
    expect(fromLocalInput("2026-10-01")).toBeNull();
  });

  it("fills a new form with the defaults and marks missing required fields", () => {
    const form = newForm(FIELDS);
    expect(form.severity).toBe("low");
    expect(form.tags).toEqual([]);
    expect(formData(FIELDS, form).errors).toEqual({ title: "is required" });
  });

  it("patches only the changed fields, and null for a cleared one", () => {
    const before = { title: "A", points: 3, tags: ["x"], seen_at: "2026-10-01T09:30:12.345Z" };
    const form = recordForm(FIELDS, before);
    form.points = "";
    form.tags = ["x", "y"];
    const p = patchData(FIELDS, before, form);
    expect(p.errors).toEqual({});
    // The datetime shows to the minute: unchanged, it stays out of the patch.
    expect(p.data).toEqual({ points: null, tags: ["x", "y"] });
    expect(patchData(FIELDS, before, recordForm(FIELDS, before)).data).toEqual({});
  });
});

describe("the query box", () => {
  it("splits terms and keeps quoted spaces", () => {
    expect(splitTerms(`type:bug title~"login page" sort:-a  x="a \\" b"`)).toEqual(["type:bug", 'title~"login page"', "sort:-a", 'x="a \\" b"']);
  });

  it("cycles a header: ascending, descending, none; one sort term", () => {
    let q = "type:bug status=open sort:title";
    expect(sortOf(q, "title")).toBe("asc");
    q = toggleSort(q, "severity");
    expect(q).toBe("type:bug status=open sort:severity");
    q = toggleSort(q, "severity");
    expect(q).toBe("type:bug status=open sort:-severity");
    expect(sortOf(q, "severity")).toBe("desc");
    q = toggleSort(q, "severity");
    expect(q).toBe("type:bug status=open");
    expect(sortOf(q, "severity")).toBeNull();
  });

  it("quotes values and keeps the type first", () => {
    expect(quoteValue("auth")).toBe("auth");
    expect(quoteValue('login "page"')).toBe('"login \\"page\\""');
    expect(withType("status=open type:other", "bug")).toBe("type:bug status=open");
  });

  it("marks the character of an error", () => {
    expect(errorAt("type:bug sevrity=x", 9)).toEqual({ before: "type:bug ", at: "s", after: "evrity=x" });
    expect(errorAt("title", 5)).toEqual({ before: "title", at: " ", after: "" });
    expect(queryErrorOf({ details: { position: 3, message: "m" } })).toEqual({ position: 3, message: "m" });
    expect(queryErrorOf(new Error("x"))).toBeNull();
  });
});

describe("columns and cells", () => {
  const rec: DbRecord = {
    id: "R1",
    project: "p",
    type: "bug",
    data: { title: "T\nsecond line", tags: ["a", "b"], open: false, service: "S1", seen_at: "2026-10-01T09:30:00.000Z" },
    revision: 2,
    created_by: "operator",
    created_by_name: "operator",
    updated_by: "01A",
    updated_by_name: "proj/w1",
    created_at: "2026-10-01T00:00:00.000Z",
    updated_at: "2026-10-02T00:00:00.000Z",
  };
  const col = (name: string, kind: DbColumn["kind"], meta = false): DbColumn => ({ name, kind, required: false, ...(meta ? { meta } : {}) });

  it("takes the first six fields", () => {
    expect(defaultColumns(FIELDS)).toEqual(["title", "notes", "points", "score", "open", "reported"]);
  });

  it("reads meta columns from the record", () => {
    expect(columnValue(col("created", "datetime", true), rec)).toBe(rec.created_at);
    expect(columnValue(col("updated_by", "string", true), rec)).toBe("proj/w1");
    expect(columnValue(col("revision", "integer", true), rec)).toBe(2);
  });

  it("writes cells as one line of text", () => {
    expect(cellText(col("title", "string"), rec.data.title)).toBe("T …");
    expect(cellText(col("tags", "list"), rec.data.tags)).toBe("a, b");
    expect(cellText(col("open", "bool"), false)).toBe("no");
    expect(cellText(col("service", "ref"), "S1", { S1: "auth" })).toBe("auth");
    expect(cellText(col("service", "ref"), "S2", {})).toBe("S2");
    expect(cellText(col("seen_at", "datetime"), rec.data.seen_at)).toBe("2026-10-01 09:30 UTC");
    expect(cellText(col("x", "string"), undefined)).toBe("");
  });

  it("links a type with a query or a record", () => {
    expect(typeHref("my p", "bug", { q: "type:bug a=b c", id: "R1" })).toBe("/p/my%20p/knowledge/db/bug?q=type%3Abug+a%3Db+c&id=R1");
    expect(typeHref("p", "bug")).toBe("/p/p/knowledge/db/bug");
  });
});

describe("the schema editor", () => {
  const before: DbField[] = [f("title", "string", { required: true }), f("owner", "string"), f("tags", "list", { of: "string", default: ["a"] })];

  it("sends renames, the new list, and confirm for a removal", () => {
    const rows = before.map(draftOf);
    rows[0]!.name = "headline";
    rows.splice(1, 1);
    rows.push({ ...emptyDraft(), name: "level", kind: "enum", values: "low, high", default: "low", required: true });
    const r = schemaPatch(4, before, rows, "Bugs");
    expect(r.removed).toEqual(["owner"]);
    expect(r.patch).toEqual({
      revision: 4,
      description: "Bugs",
      confirm: true,
      rename: [{ from: "title", to: "headline" }],
      fields: [f("headline", "string", { required: true }), f("tags", "list", { of: "string", default: ["a"] }), f("level", "enum", { required: true, values: ["low", "high"], default: "low" })],
    });
  });

  it("refuses a default that does not fit the kind", () => {
    const rows = [{ ...emptyDraft(), name: "n", kind: "integer" as const, default: "x" }];
    expect(schemaPatch(1, [], rows).error).toBe("field `n`: the default must be a number");
  });
});
