// Tool proxies in the mock: the seed, grants within the scope, and the
// refusals as the daemon gives them.
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });

describe("tools in the mock", () => {
  it("lists the seed with grants, and grants and revokes within the scope", async () => {
    const api = mock();
    const list = await api.tools.list();
    expect(list.map((t) => t.name)).toEqual(["tracker-read", "pg-reset"]);
    expect(list[1]).toMatchObject({ scope: "project", project: "webshop", source: "api" });
    await expect(api.tools.grant("pg-reset", "project:terrarium")).rejects.toMatchObject({ status: 422 });
    await expect(api.tools.grant("pg-reset", "project:webshop")).rejects.toMatchObject({ status: 409 });
    const g = await api.tools.grant("tracker-read", "project:webshop");
    expect(g).toMatchObject({ grantee: "project:webshop", grantee_label: "project:webshop" });
    expect((await api.tools.grants("tracker-read")).map((x) => x.grantee)).toContain("project:webshop");
    await api.tools.revoke("tracker-read", "project:webshop");
    await expect(api.tools.revoke("tracker-read", "project:webshop")).rejects.toMatchObject({ status: 404 });
    await expect(api.tools.grant("tracker-read", "bogus")).rejects.toMatchObject({ status: 422 });
  });

  it("deletes an API tool and refuses a file tool", async () => {
    const api = mock();
    await expect(api.tools.remove("tracker-read")).rejects.toMatchObject({ status: 409 });
    await api.tools.remove("pg-reset");
    expect((await api.tools.list()).map((t) => t.name)).toEqual(["tracker-read"]);
    await expect(api.tools.get("pg-reset")).rejects.toMatchObject({ status: 404 });
  });
});
