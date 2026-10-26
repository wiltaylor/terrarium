import { describe, expect, it } from "vitest";
import type { EventSource, EventSourceWithSecret } from "../../api/types";
import { GLOBAL } from "./form";
import { emptySourceForm, sortSources, sourceBody, sourceFieldOf, upsertSource, withoutSecret } from "./sources";

const src = (name: string, project: string | null, extra: Partial<EventSource> = {}): EventSource => ({
  id: `${project ?? "_"}/${name}`,
  name,
  project,
  kind: "github",
  enabled: true,
  created_by: "operator",
  last_seen: null,
  last_error: null,
  received: 0,
  created_at: "2026-10-26T00:00:00Z",
  updated_at: "2026-10-26T00:00:00Z",
  hook_path: `/hooks/${project ?? "_"}/${name}`,
  hook_url: null,
  setup: [],
  ...extra,
});

describe("the create form of an event source", () => {
  it("defaults the scope to the selected project, never to global", () => {
    expect(emptySourceForm("terrarium")).toEqual({ name: "", scope: "terrarium", kind: "github" });
    expect(emptySourceForm(null).scope).toBe("");
  });

  it("sends name, kind, and the project (null for global)", () => {
    expect(sourceBody({ name: " gh ", scope: "terrarium", kind: "forgejo" })).toEqual({ body: { name: "gh", kind: "forgejo", project: "terrarium" } });
    expect(sourceBody({ name: "nightly", scope: GLOBAL, kind: "generic" })).toEqual({ body: { name: "nightly", kind: "generic", project: null } });
  });

  it("needs a valid name and a scope", () => {
    expect(sourceBody({ name: "", scope: "p", kind: "github" })).toMatchObject({ field: "name" });
    expect(sourceBody({ name: "Bad Name", scope: "p", kind: "github" })).toMatchObject({ field: "name" });
    expect(sourceBody({ name: "gh", scope: "", kind: "github" })).toMatchObject({ field: "scope" });
  });

  it("shows the daemon's field: project is the scope", () => {
    expect(sourceFieldOf("name")).toBe("name");
    expect(sourceFieldOf("project")).toBe("scope");
    expect(sourceFieldOf("other")).toBeUndefined();
  });
});

describe("the list never keeps the secret", () => {
  it("drops the secret of a create or a rotate answer", () => {
    const answer: EventSourceWithSecret = { ...src("gh", "terrarium"), secret: "ab".repeat(32) };
    expect(withoutSecret(answer)).not.toHaveProperty("secret");
    const list = upsertSource([src("gh", "terrarium", { received: 3 })], answer);
    expect(list).toHaveLength(1);
    expect(list[0]).not.toHaveProperty("secret");
    expect(list[0]!.received).toBe(0);
  });

  it("sorts the global sources first, then by project and name", () => {
    expect(sortSources([src("b", "web"), src("z", null), src("a", "web"), src("x", "app")]).map((s) => `${s.project ?? "_"}/${s.name}`)).toEqual(["_/z", "app/x", "web/a", "web/b"]);
  });
});
