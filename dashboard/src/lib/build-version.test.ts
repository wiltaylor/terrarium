import { describe, expect, it } from "vitest";
import { staleHosts, staleTitle, versionParts } from "./build-version";
import { createMockApi } from "../api/mock";
import { MOCK_DAEMON_VERSION } from "../api/mock/data";

describe("build version (docs/api.md §4)", () => {
  it("splits the full version as the terrarium-version crate does", () => {
    expect(versionParts("0.1.0+6e31754")).toEqual({ version: "0.1.0", ref: "6e31754", dirty: false });
    expect(versionParts("0.1.0+6e31754-dirty")).toEqual({ version: "0.1.0", ref: "6e31754", dirty: true });
    expect(versionParts("0.2.0-alpha+6e31754")).toEqual({ version: "0.2.0-alpha", ref: "6e31754", dirty: false });
    expect(versionParts("0.1.0+unknown")).toEqual({ version: "0.1.0", ref: null, dirty: false });
    expect(versionParts("0.1.0")).toEqual({ version: "0.1.0", ref: null, dirty: false });
    expect(versionParts(null)).toEqual({ version: "", ref: null, dirty: false });
  });

  it("lists the stale host agents and explains the mark", () => {
    expect(staleHosts([{ name: "a", stale: true }, { name: "b", stale: false }, { name: "c" }])).toEqual(["a"]);
    expect(staleHosts(undefined)).toEqual([]);
    expect(staleTitle("0.0.9", "0.1.0+6e31754")).toContain("runs 0.0.9, the daemon 0.1.0+6e31754");
    expect(staleTitle(null, null)).toContain("an unknown version");
  });

  it("the mock serves the daemon build and marks the older host agents", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const v = await api.systemVersion();
    expect(v).toMatchObject({ version: MOCK_DAEMON_VERSION, cargo_version: "0.1.0", git_ref: "6e31754", dirty: false });
    expect(v.build_time).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/);
    expect((await api.health()).version).toBe(MOCK_DAEMON_VERSION);
    const hosts = await api.hosts.list();
    expect(staleHosts(hosts).sort()).toEqual(["nas", "pi-cluster"]);
    for (const h of hosts) {
      const p = versionParts(h.version);
      expect(h.stale).toBe(p.ref !== versionParts(MOCK_DAEMON_VERSION).ref);
    }
  });
});
