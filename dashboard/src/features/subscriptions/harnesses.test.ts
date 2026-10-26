import { describe, expect, it } from "vitest";
import type { HarnessReport, HarnessVersion } from "../../api/types";
import { createMockApi } from "../../api/mock";
import { harnessUpdateKind } from "../../lib/harness-update";
import { harnessStatus, npmCommand, offPin, outOfRange, policyNote, publishedDay, resultLine, updateFailure, withUpdate } from "./harnesses";

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
    expect(out.title).toContain("Update it here, or run just harness::update claude");
    // npm did not answer: the range alone.
    const noNpm = harnessStatus(row({ latest: null, update: null, latest_error: "cannot reach npm: timeout" }));
    expect(noNpm).toMatchObject({ status: "in range", cls: "ok" });
    expect(noNpm.title).toContain("cannot reach npm");
    // Without `latest=true` (an older daemon): the range alone.
    expect(harnessStatus(row({})).status).toBe("in range");
    expect(harnessStatus(row({ installed: null, in_range: null, error: "cannot run claude" }))).toEqual({ status: "unknown", cls: "warn", title: "cannot run claude" });
  });

  it("gives the npm line for a row with a newer release", () => {
    const latest = { version: "0.165.0", published_at: null };
    expect(npmCommand(row({ update: "none", latest: { ...latest, version: "2.1.310" } }))).toBeNull();
    expect(npmCommand(row({ update: null, latest: null }))).toBeNull();
    expect(npmCommand(row({ harness: "codex", package: "@openai/codex", update: "major", latest }))).toBe("npm install -g @openai/codex@0.165.0");
    const two = report(row({ harness: "codex", update: "major" }), row({ harness: "pi", update: "minor" }), row({ update: null }));
    expect(withUpdate(two).map((h) => h.harness)).toEqual(["codex", "pi"]);
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
    expect(r.harnesses.map(npmCommand)).toEqual([null, "npm install -g @openai/codex@0.165.0", "npm install -g @github/copilot@1.0.110", null]);
  });

  it("the mock updates a harness in place: 422 for a bad version or an npm error, 409 while agents run, then force", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    await expect(api.harnessUpdate("nope")).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.harnessUpdate("codex", { version: "1.2" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    const npm = await api.harnessUpdate("codex", { version: "99.0.0", force: true }).catch((e: unknown) => e);
    expect(npm).toMatchObject({ status: 422, code: "invalid" });
    const f = updateFailure(npm);
    expect(f.output).toContain("notarget");
    expect(f.running).toBeNull();

    // The mock has live codex agents: 409 with their names.
    const busy = await api.harnessUpdate("codex").catch((e: unknown) => e);
    expect(busy).toMatchObject({ status: 409, code: "conflict" });
    const b = updateFailure(busy);
    expect(b.running).toContain("terrarium/adapter-codex");
    expect(b.message).toContain("risky");

    const res = await api.harnessUpdate("codex", { force: true });
    expect(Object.keys(res).sort()).toEqual(["from", "harness", "in_range", "output", "package", "tested", "to"]);
    expect(res).toMatchObject({ harness: "codex", from: "0.164.0", to: "0.165.0", in_range: false });
    expect(resultLine(res)).toBe("codex 0.164.0 → 0.165.0");
    const codex = (await api.harnesses(false, true)).harnesses.find((h) => h.harness === "codex")!;
    expect(codex).toMatchObject({ installed: "0.165.0", update: "none", in_range: false });
    const ev = await api.events.history({ kind: "harness" });
    expect(ev.items.at(-1)?.data).toMatchObject({ change: "updated", harness: "codex", from: "0.164.0", to: "0.165.0", forced: true });

    // All: 409 while agents run; with force, one result per harness and the extension.
    await expect(api.harnessUpdateAll()).rejects.toMatchObject({ status: 409 });
    const all = await api.harnessUpdateAll({ force: true }).catch((e: unknown) => e);
    // pi's npm does not answer in the mock, so "all" is a 422 with the results.
    expect(all).toMatchObject({ status: 422 });
    const af = updateFailure(all);
    expect(af.output).toContain("pi: npm install -g @earendil-works/pi-coding-agent@latest failed");
    expect(af.output).toContain("copilot-prompt 0.1.2 → 0.1.2");
  });
});
