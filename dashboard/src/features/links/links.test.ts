import { describe, expect, it } from "vitest";
import { createMockApi } from "../../api/mock";
import { linkChip } from "./LinksPage";

const mock = () => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, autoLoginMs: 0 });

describe("links (docs/api.md §36)", () => {
  it("makes a link with a one-time code, rotates it, and revokes it", async () => {
    const api = mock();
    const made = await api.links.create("office", { readOnly: true });
    expect(made.code).toMatch(/^tlp_/);
    expect(made.link).toMatchObject({ label: "office", read_only: true, status: "pending" });
    expect(await api.links.list()).toHaveLength(1);
    const again = await api.links.rotate(made.link.id);
    expect(again.code).not.toBe(made.code);
    await api.links.revoke(made.link.id);
    expect(await api.links.list()).toHaveLength(0);
    expect((await api.links.list({ revoked: true }))[0].status).toBe("revoked");
    await expect(api.links.revoke(made.link.id)).rejects.toThrow();
  });

  it("colours the status", () => {
    expect(linkChip({ status: "active" })).toBe("ok");
    expect(linkChip({ status: "pending" })).toBe("warn");
    expect(linkChip({ status: "revoked" })).toBe("bad");
  });
});
