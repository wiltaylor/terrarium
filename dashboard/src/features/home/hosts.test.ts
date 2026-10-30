import { describe, expect, it } from "vitest";
import type { Host, HostFilesystem, HostMetricsSample } from "../../api/types";
import { fullestFirst, hostState, nearest, pct, rate, scaled, series, size, sortHosts, spaceLevel, timePath, METRICS } from "./hosts";

const sample = (at: number, cpu?: number): HostMetricsSample => ({
  at: new Date(at).toISOString(),
  interval_secs: 10,
  ...(cpu === undefined ? {} : { cpu: { busy_pct: cpu, cores: 4, load1: 0.5 } }),
  net: { rx_bytes_per_sec: 2048, tx_bytes_per_sec: 0 },
  filesystems: [],
});

const fsys = (mount: string, used_pct: number): HostFilesystem => ({ mount, fstype: "ext4", device: `/dev/${mount}`, total_bytes: 100, used_bytes: used_pct, avail_bytes: 100 - used_pct, used_pct });

describe("host cards", () => {
  it("names the state of a host", () => {
    const h = (o: Partial<Host>): Pick<Host, "status" | "capabilities" | "metrics"> => ({ status: "online", capabilities: { metrics: true }, metrics: { ...sample(0, 1), stale: false }, ...o });
    expect(hostState(h({}))).toBe("live");
    expect(hostState(h({ metrics: { ...sample(0, 1), stale: true } }))).toBe("stale");
    expect(hostState(h({ status: "offline" }))).toBe("offline");
    expect(hostState(h({ metrics: null }))).toBe("waiting");
    // An older host agent: no `metrics` capability.
    expect(hostState(h({ capabilities: {}, metrics: null }))).toBe("unsupported");
  });

  it("puts online hosts first, then by name", () => {
    const hosts = [{ name: "b", status: "offline" }, { name: "c", status: "online" }, { name: "a", status: "online" }] as const;
    expect(sortHosts([...hosts]).map((h) => h.name)).toEqual(["a", "c", "b"]);
  });

  it("formats sizes, rates, and shares", () => {
    expect(size(512)).toBe("512 B");
    expect(size(1536)).toBe("1.5 KB");
    expect(size(1.25 * 1024 ** 2)).toBe("1.3 MB");
    expect(size(1862 * 1024 ** 3)).toBe("1.8 TB");
    expect(size(250 * 1024 ** 3)).toBe("250 GB");
    expect(rate(1.2 * 1024 ** 2)).toBe("1.2 MB/s");
    expect(rate(0)).toBe("0 B/s");
    expect(pct(4.26)).toBe("4.3%");
    expect(pct(42.6)).toBe("43%");
    expect(pct(0)).toBe("0%");
    expect(pct(140)).toBe("100%");
  });

  it("warns at 80% and 90% disk space", () => {
    expect(spaceLevel(79.9)).toBe("ok");
    expect(spaceLevel(80)).toBe("warn");
    expect(spaceLevel(90)).toBe("full");
  });

  it("shows the fullest filesystems first, 3 at most", () => {
    const r = fullestFirst([fsys("a", 10), fsys("b", 95), fsys("c", 50), fsys("d", 81)]);
    expect(r.shown.map((f) => f.mount)).toEqual(["b", "d", "c"]);
    expect(r.hidden).toBe(1);
    expect(r.all).toHaveLength(4);
    expect(fullestFirst([fsys("a", 1)]).hidden).toBe(0);
  });

  it("takes one metric over the window, oldest first", () => {
    const s = [sample(30_000, 3), sample(10_000, 1), sample(20_000), sample(0, 9)];
    expect(series(s, METRICS.cpu, 5_000)).toEqual([
      { at: 10_000, value: 1 },
      { at: 30_000, value: 3 },
    ]);
    expect(series(s, METRICS.rx, 0)).toHaveLength(4);
    expect(series(s, METRICS.read, 0)).toEqual([]);
  });

  it("scales shares to 100 and rates to their peak with a floor", () => {
    const pts = [{ at: 0, value: 25 }, { at: 1, value: 50 }];
    expect(scaled(pts, 100)).toEqual([25, 50]);
    expect(scaled(pts)).toEqual([(25 / 1024) * 100, (50 / 1024) * 100]);
    expect(scaled([{ at: 0, value: 2048 }, { at: 1, value: 4096 }])).toEqual([50, 100]);
  });

  it("finds the nearest point in time", () => {
    const pts = [{ at: 0, value: 1 }, { at: 10, value: 2 }, { at: 20, value: 3 }];
    expect(nearest(pts, 14)).toBe(1);
    expect(nearest(pts, 16)).toBe(2);
    expect(nearest([], 5)).toBe(-1);
  });

  it("breaks the line where samples are missing", () => {
    const pts = [{ at: 0, value: 0 }, { at: 10_000, value: 0 }, { at: 100_000, value: 0 }];
    const d = timePath(pts, [0, 100, 50], 0, 100_000, 100, 10);
    expect(d.match(/M/g)).toHaveLength(2);
    // The lone last point draws as a dot.
    expect(d.endsWith("l0.1,0")).toBe(true);
    expect(d.startsWith("M0.0,9.0 L10.0,1.0")).toBe(true);
  });
});
