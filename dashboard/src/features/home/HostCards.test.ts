// @vitest-environment jsdom
/// <reference types="node" />
// The host cards (#4): each card takes the full row, a long mount path (a
// WSL2 host) ends in an ellipsis with the full path in the tooltip, and the
// CSS keeps the card from growing past its row. And the offline reason of a
// host (#3) on the card and on the Hosts page.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import type { Host, HostMetricsSample } from "../../api/types";

const LONG = "/usr/lib/linux-tools/6.18.54.1-microsoft-standard-WSL2";
const sample: HostMetricsSample = {
  at: new Date().toISOString(),
  interval_secs: 10,
  cpu: { busy_pct: 68, cores: 22, load1: 25.2 },
  filesystems: [
    { mount: "/", fstype: "ext4", device: "/dev/sdd", total_bytes: 1007, used_bytes: 151, avail_bytes: 856, used_pct: 15 },
    { mount: LONG, fstype: "overlay", device: "none", total_bytes: 96, used_bytes: 96, avail_bytes: 0, used_pct: 100 },
  ],
};
const wsl: Host = { name: "wsl", status: "online", capabilities: { metrics: true }, metrics: { ...sample, stale: false } };
const gone: Host = { name: "gone", status: "offline", offline_reason: "never connected: the host agent has not reached the daemon", last_seen: null };

const list = vi.fn(async () => [wsl, gone]);
const metrics = vi.fn(async () => ({ host: "wsl", interval_secs: 10, minutes: 15, stale: false, samples: [sample] }));
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({ api: { hosts: { list, metrics, revoke: vi.fn() }, targets: { list: vi.fn(async () => []) } } })); });
vi.mock("@solidjs/router", () => ({ A: (p: { children?: unknown }) => p.children }));

const { HostCards } = await import("./HostCards");
const HostsPage = (await import("../hosts/HostsPage")).default;

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
});

const mount = (fn: () => unknown) => {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(fn as () => Element, el);
  return el;
};

/** The body of the first rule whose selector list is exactly `selector`. */
function rule(css: string, selector: string): string {
  const at = css.indexOf(`\n${selector} {`);
  expect(at, selector).toBeGreaterThanOrEqual(0);
  return css.slice(at, css.indexOf("}", at));
}

describe("host cards", () => {
  it("shows a long mount path in one cell that ends in an ellipsis, with the full path in the tooltip", async () => {
    const el = mount(() => createComponent(HostCards, { now: Date.now() }));
    await waitFor(() => expect(el.querySelector(".fs-list")).not.toBeNull());
    const meters = [...el.querySelectorAll<HTMLElement>(".fs-list .meter")];
    expect(meters).toHaveLength(2);
    const long = meters.find((m) => m.textContent?.includes(LONG))!;
    expect(long.title).toBe(`${LONG}\nnone (overlay)`);
    const mountEl = long.querySelector(".fs-mount")!;
    expect(mountEl.textContent).toBe(LONG);
    expect(mountEl.classList.contains("ellipsis")).toBe(true);
    expect(long.textContent).toContain("almost full");
  });

  it("shows why an offline host is offline", async () => {
    const el = mount(() => createComponent(HostCards, { now: Date.now() }));
    await waitFor(() => expect(el.querySelector(".offline-reason")).not.toBeNull());
    expect(el.querySelector(".offline-reason")!.textContent).toBe(gone.offline_reason);
  });

  it("keeps each card on a row of its own and inside it (the CSS)", () => {
    // Vitest runs in dashboard/; jsdom gives import.meta.url no file scheme.
    const css = readFileSync(resolve(process.cwd(), "src/styles/global.css"), "utf8");
    // A grid track sized `auto` grows to the min-content of a nowrap path: the card overflowed.
    expect(rule(css, ".host-strip")).toContain("grid-template-columns: minmax(0, 1fr);");
    expect(rule(css, ".host-card")).toContain("grid-template-columns: minmax(0, 1fr);");
    expect(rule(css, ".host-fs .meter")).toContain("grid-template-columns: minmax(0, 1fr);");
    expect(rule(css, ".host-fs .meter-label > .fs-mount")).toContain("min-width: 0;");
    // At phone width the lists go to one column.
    expect(rule(css, ".fs-list")).toContain("minmax(min(100%, 440px), 1fr)");
    expect(rule(css, ".metric-grid")).toContain("minmax(min(100%, 150px), 1fr)");
  });
});

describe("Hosts page", () => {
  it("shows the offline reason under the status of an offline host only", async () => {
    const el = mount(() => createComponent(HostsPage, {}));
    await waitFor(() => expect(el.querySelector('[aria-label="Why gone is offline"]')).not.toBeNull());
    expect(el.querySelector('[aria-label="Why gone is offline"]')!.textContent).toBe(gone.offline_reason);
    expect(el.querySelector('[aria-label="Why wsl is offline"]')).toBeNull();
  });
});
