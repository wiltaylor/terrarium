import { describe, expect, it } from "vitest";
import type { HarnessReport, HarnessVersion, Host } from "../../api/types";
import { createMockApi } from "../../api/mock";
import { harnessUpdateKind } from "../../lib/harness-update";
import { harnessStatus, HARNESS_ROLLOUT, offPin, outOfRange, policyNote, publishedDay, updateCommand, updateHosts, updateNames, withUpdate } from "./harnesses";

const row = (over: Partial<HarnessVersion>): HarnessVersion => ({
  harness: "claude",
  package: "@anthropic-ai/claude-code",
  pinned: "2.1.310",
  tested: ">=2.1.310, <2.2.0",
  installed: "2.1.310",
  in_range: true,
  checked_at: "2026-10-25T10:00:00Z",
  ...over,
});

const report = (...harnesses: HarnessVersion[]): HarnessReport => ({ policy: "warn", harnesses });

describe("harness versions table", () => {
  it("names the status from the range and the npm update", () => {
    const latest = { version: "2.1.310", published_at: "2026-10-20T10:00:00Z" };
    expect(harnessStatus(row({ latest, update: "none" }))).toMatchObject({ status: "latest", cls: "ok" });
    expect(harnessStatus(row({ latest: { ...latest, version: "2.1.320" }, update: "minor" }))).toMatchObject({ status: "update available", cls: "info" });
    const major = harnessStatus(row({ latest: { ...latest, version: "3.0.0" }, update: "major" }));
    expect(major).toMatchObject({ status: "update available (major)", cls: "warn" });
    expect(major.title).toContain("major bump");
    // Outside the tested range wins over the npm status.
    const out = harnessStatus(row({ installed: "2.3.0", in_range: false, latest, update: "none" }));
    expect(out).toMatchObject({ status: "unsupported", cls: "bad" });
    expect(out.title).toContain("just harness::update claude");
    // npm did not answer: the range alone.
    const noNpm = harnessStatus(row({ latest: null, update: null, latest_error: "cannot reach npm: timeout" }));
    expect(noNpm).toMatchObject({ status: "in range", cls: "ok" });
    expect(noNpm.title).toContain("cannot reach npm");
    // Without `latest=true` (an older daemon): the range alone.
    expect(harnessStatus(row({})).status).toBe("in range");
    expect(harnessStatus(row({ installed: null, in_range: null, error: "cannot run claude" }))).toEqual({ status: "unknown", cls: "warn", title: "cannot run claude" });
  });

  it("gives the command to run on the Docker host", () => {
    const none = row({ update: "none" });
    expect(updateCommand(report(none))).toBeNull();
    expect(updateCommand(undefined)).toBeNull();
    expect(updateCommand(report(none, row({ harness: "copilot", update: "minor" })))).toBe("just harness::update copilot && just harness::rollout");
    expect(updateCommand(report(row({ harness: "codex", update: "major" })))).toBe("just harness::update codex --allow-major && just harness::rollout");
    const two = report(row({ harness: "codex", update: "major" }), row({ harness: "pi", update: "minor" }), row({ update: null }));
    expect(updateCommand(two)).toBe("just harness::latest");
    expect(withUpdate(two).map((h) => h.harness)).toEqual(["codex", "pi"]);
    expect(updateNames(two)).toBe("all");
    expect(updateNames(report(row({ harness: "pi", update: "minor" })))).toEqual(["pi"]);
  });

  it("offers the host button only for an online host with can_update_harnesses", () => {
    const h = (name: string, over: Partial<Host>): Host => ({ name, status: "online", online: true, ...over });
    const hosts = [
      h("dock", { capabilities: { can_update_harnesses: true } }),
      h("lab1", { capabilities: { docker: { present: true } } }),
      h("old", { status: "offline", online: false, capabilities: { can_update_harnesses: true } }),
    ];
    expect(updateHosts(hosts).map((x) => x.name)).toEqual(["dock"]);
    expect(updateHosts(undefined)).toEqual([]);
  });

  it("computes the update kind like scripts/harness.py", () => {
    expect(harnessUpdateKind("2.1.310", "2.1.310")).toBe("none");
    expect(harnessUpdateKind("2.1.310", "2.1.300")).toBe("none");
    expect(harnessUpdateKind("2.1.310", "2.1.311")).toBe("minor");
    expect(harnessUpdateKind("2.1.310", "2.2.0")).toBe("minor");
    expect(harnessUpdateKind("2.1.310", "3.0.0")).toBe("major");
    expect(harnessUpdateKind("0.164.0", "0.164.2")).toBe("minor");
    expect(harnessUpdateKind("0.164.0", "0.165.0")).toBe("major");
    expect(harnessUpdateKind("0.0.3", "0.0.4")).toBe("major");
    expect(harnessUpdateKind("1.4.0", "1.5.0-beta.1")).toBe("minor");
    expect(harnessUpdateKind("1.5.0-beta.1", "1.5.0")).toBe("minor");
    expect(harnessUpdateKind("nightly", "nightly")).toBe("none");
    expect(harnessUpdateKind("nightly", "1.0.0")).toBe("minor");
  });

  it("counts the rows out of range and spots a version off the pin", () => {
    const r = report(row({}), row({ harness: "pi", installed: "0.86.0", in_range: false }), row({ harness: "codex", installed: null, in_range: null }));
    expect(outOfRange(r).map((h) => h.harness)).toEqual(["pi"]);
    expect(outOfRange(undefined)).toEqual([]);
    expect(offPin(row({ installed: "2.1.311" }))).toBe(true);
    expect(offPin(row({}))).toBe(false);
    expect(offPin(row({ installed: null }))).toBe(false);
    expect(publishedDay("2026-10-25T09:30:00Z")).toBe("2026-10-25");
    expect(publishedDay(null)).toBeNull();
  });

  it("names the policy", () => {
    expect(policyNote({ policy: "warn", harnesses: [] })).toContain("warning event");
    expect(policyNote({ policy: "refuse", harnesses: [] })).toContain("refused");
  });

  it("the mock answers GET /api/harnesses in the daemon's shape", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const r = await api.harnesses();
    expect(r.policy).toBe("warn");
    expect(r.harnesses.map((h) => h.harness)).toEqual(["claude", "codex", "copilot", "pi"]);
    for (const h of r.harnesses) {
      expect(Object.keys(h).sort()).toEqual(["checked_at", "harness", "in_range", "installed", "package", "pinned", "tested"]);
    }
    expect(outOfRange(r).map((h) => h.harness)).toEqual(["pi"]);
  });

  it("the mock answers ?latest=true with latest, update, and latest_error", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const r = await api.harnesses(false, true);
    for (const h of r.harnesses) {
      const keys = Object.keys(h).sort();
      if (h.latest === null) expect(keys).toEqual(["checked_at", "harness", "in_range", "installed", "latest", "latest_error", "package", "pinned", "tested", "update"]);
      else expect(keys).toEqual(["checked_at", "harness", "in_range", "installed", "latest", "package", "pinned", "tested", "update"]);
      if (h.latest) {
        expect(Object.keys(h.latest).sort()).toEqual(["published_at", "version"]);
        expect(h.update).toBe(harnessUpdateKind(h.pinned, h.latest.version));
      } else expect(h.update).toBeNull();
    }
    expect(r.harnesses.map((h) => harnessStatus(h).status)).toEqual(["latest", "update available (major)", "update available", "unsupported"]);
    expect(updateCommand(r)).toBe("just harness::latest");
  });

  it("the mock runs harness.update on a capable host and ends with the rollout", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const hosts = await api.hosts.list();
    expect(updateHosts(hosts).map((h) => h.name)).toEqual(["lab1"]);
    await expect(api.hosts.harnessUpdate("lab1", ["nope"])).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.hosts.harnessUpdate("pi-cluster", "all")).rejects.toMatchObject({ status: 409, code: "conflict" });
    const s = await api.hosts.harnessUpdate("lab1", "all");
    expect(Object.keys(s).sort()).toEqual(["host", "names", "output", "rollout", "session_id"]);
    expect(s).toMatchObject({ host: "lab1", names: ["all"], rollout: HARNESS_ROLLOUT, output: `/api/hosts/lab1/sessions/${s.session_id}/output` });
    let since = 0;
    let text = "";
    for (let i = 0; i < 20; i++) {
      const o = await api.hosts.sessionOutput("lab1", s.session_id, since);
      text += o.chunks.map((c) => atob(c.data)).join("");
      since = o.next;
      if (o.done) {
        expect(o.end).toMatchObject({ type: "exit", code: 0 });
        expect(o.rollout).toBe(HARNESS_ROLLOUT);
        break;
      }
    }
    expect(text).toContain("scripts/harness.py latest --no-rollout");
    expect(since).toBe(text.length);
    const audit = await api.audit({ action: "harness.update" });
    expect(audit.items[0]).toMatchObject({ action: "harness.update", target: "lab1", detail: { names: ["all"], session: s.session_id } });
  });
});
