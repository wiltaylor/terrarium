import { describe, expect, it } from "vitest";
import { hostLimitsChip, limitsChip, limitsInfo, limitsKindError, limitsSummary, limitsTitle, parseBytes, resolveLimits, showBytes } from "./limits";

describe("resource limits (docs/api.md §17.2)", () => {
  it("resolves the values as terrarium_core::limits does", () => {
    expect(resolveLimits({ cpu: 2, memory: "4G", pids: 512, io_weight: 200 })).toEqual({ limits: { cpuMillis: 2000, memoryBytes: 4 * 2 ** 30, pids: 512, ioWeight: 200 } });
    expect(resolveLimits({ cpu: "0.5" })).toEqual({ limits: { cpuMillis: 500 } });
    expect(resolveLimits({ cpu: 0.005 })).toHaveProperty("error");
    expect(resolveLimits({ cpu: 0 })).toHaveProperty("error");
    expect(resolveLimits({ memory: "4X" })).toHaveProperty("error");
    expect(resolveLimits({ memory: "1M" })).toEqual({ error: 'bad `limits.memory` "1M": give at least 16M' });
    expect(resolveLimits({ swap: "1G" })).toEqual({ error: "`limits.swap` needs `limits.memory`: swap comes on top of it" });
    expect(resolveLimits({ pids: 1.5 })).toHaveProperty("error");
    expect(resolveLimits({ io_weight: 10001 })).toHaveProperty("error");
  });

  it("parses sizes in powers of 1024", () => {
    expect(parseBytes("512M")).toBe(512 * 2 ** 20);
    expect(parseBytes("4GiB")).toBe(4 * 2 ** 30);
    expect(parseBytes("1.5g")).toBe(1536 * 2 ** 20);
    expect(parseBytes("1024")).toBe(1024);
    expect(parseBytes("-1G")).toBeNull();
    expect(showBytes(1536 * 2 ** 20)).toBe("1536M");
  });

  it("makes the chip text and the tooltip", () => {
    expect(limitsSummary({ cpuMillis: 2000, memoryBytes: 4 * 2 ** 30 })).toBe("2 CPU · 4G");
    expect(limitsSummary({ cpuMillis: 500, memoryBytes: 2 ** 30, swapBytes: 2 ** 30, pids: 64, ioWeight: 50 })).toBe("0.5 CPU · 1G (+1G swap) · 64 pids · io 50");
    expect(limitsInfo("docker", { cpu: 1 })).toEqual({ summary: "1 CPU", per: "container" });
    expect(limitsChip({ kind: "host", limits: { memory: "2G" } })).toBe("2G");
    expect(limitsChip({ kind: "host", limits: { memory: "2G" }, limits_info: { summary: "from daemon", per: "holder" } })).toBe("from daemon");
    expect(limitsChip({ kind: "host" })).toBeNull();
    expect(limitsTitle({ kind: "host", limits_info: { summary: "x", per: "holder", problem: "this host agent cannot apply resource limits: old" } })).toContain("cannot apply");
  });

  it("allows limits on host targets and docker targets with an image only", () => {
    expect(limitsKindError("host", false)).toBeNull();
    expect(limitsKindError("docker", true)).toBeNull();
    expect(limitsKindError("docker", false)).toContain("needs `image`");
    for (const k of ["vmlab", "ssh", "vnc", "rdp", "desktop"] as const) expect(limitsKindError(k, true)).not.toBeNull();
  });

  it("names what the host agent can do, and nothing for an older one", () => {
    expect(hostLimitsChip({ capabilities: {} })).toBeNull();
    expect(hostLimitsChip({ capabilities: { limits: { systemd: true, controllers: ["cpu", "memory"], docker: true } } })).toEqual({
      label: "limits",
      ok: true,
      title: "systemd-run --user works; the user manager has the controllers: cpu, memory",
    });
    const off = hostLimitsChip({ capabilities: { limits: { systemd: false, reason: "no user manager", docker: false } } });
    expect(off).toMatchObject({ label: "no limits", ok: false });
    expect(off?.title).toContain("no user manager");
  });
});
