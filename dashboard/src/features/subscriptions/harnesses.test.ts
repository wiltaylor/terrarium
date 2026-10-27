import { describe, expect, it } from "vitest";
import type { HarnessReport, HarnessVersion } from "../../api/types";
import { createMockApi } from "../../api/mock";
import { harnessUpdateKind } from "../../lib/harness-update";
import { harnessStatus, offPin, oldAgentLine, oldAgents, otherVersions, outOfRange, policyNote, publishedDay, resultLine, resultNotes, updateCommand, updateFailure, withUpdate } from "./harnesses";

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

  it("gives the CLI line for a row with a newer release", () => {
    const latest = { version: "0.165.0", published_at: null };
    expect(updateCommand(row({ update: "none", latest: { ...latest, version: "2.1.310" } }))).toBeNull();
    expect(updateCommand(row({ update: null, latest: null }))).toBeNull();
    expect(updateCommand(row({ harness: "codex", package: "@openai/codex", update: "major", latest }))).toBe("terrarium admin harness update codex --version 0.165.0");
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

  it("names the agents on an older version and the other version dirs", () => {
    const agents = [
      { agent: "terra/a", id: "01A", version: "1.0.103" },
      { agent: "terra/b", id: "01B", version: "1.0.110" },
      { agent: "terra/c", id: "01C", version: null },
    ];
    const h = row({ harness: "copilot", active: "1.0.110", versions: ["1.0.103", "1.0.110"], agents });
    expect(oldAgents(h).map((a) => a.agent)).toEqual(["terra/a", "terra/c"]);
    expect(oldAgents(h).map((a) => oldAgentLine(a, h.active!))).toEqual(["terra/a runs 1.0.103; 1.0.110 at its next restart", "terra/c runs an older install; 1.0.110 at its next restart"]);
    expect(otherVersions(h)).toEqual(["1.0.103"]);
    // An older daemon: no tools fields.
    expect(oldAgents(row({ agents }))).toEqual([]);
    expect(otherVersions(row({}))).toEqual([]);
    const res = { harness: "copilot", package: "@github/copilot", from: "1.0.103", to: "1.0.110", output: "", tested: null, in_range: null, reused: false, agents_on_old_version: [], note: "1 live agent(s) run another version", removed: ["1.0.90"] };
    expect(resultNotes(res)).toEqual(["1 live agent(s) run another version", "copilot: removed the unused version 1.0.90"]);
    expect(resultNotes({ ...res, note: undefined, removed: [] })).toEqual([]);
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
      for (const k of Object.keys(h)) expect(["active", "agents", "checked_at", "harness", "in_range", "installed", "package", "pinned", "tested", "versions"]).toContain(k);
      expect(h.active).toBe(h.installed);
      expect(h.versions).toEqual([h.installed]);
    }
    expect(outOfRange(r).map((h) => h.harness)).toEqual(["pi"]);
  });

  it("the mock answers ?latest=true with latest, update, and latest_error", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const r = await api.harnesses(false, true);
    for (const h of r.harnesses) {
      const keys = Object.keys(h)
        .filter((k) => !["active", "versions", "agents"].includes(k))
        .sort();
      if (h.latest === null) expect(keys).toEqual(["checked_at", "harness", "in_range", "installed", "latest", "latest_error", "package", "pinned", "tested", "update"]);
      else expect(keys).toEqual(["checked_at", "harness", "in_range", "installed", "latest", "package", "pinned", "tested", "update"]);
      if (h.latest) {
        expect(Object.keys(h.latest).sort()).toEqual(["published_at", "version"]);
        expect(h.update).toBe(harnessUpdateKind(h.pinned, h.latest.version));
      } else expect(h.update).toBeNull();
    }
    expect(r.harnesses.map((h) => harnessStatus(h).status)).toEqual(["latest", "update available (major)", "update available", "unsupported"]);
    expect(r.harnesses.map(updateCommand)).toEqual([null, "terrarium admin harness update codex --version 0.165.0", "terrarium admin harness update copilot --version 1.0.110", null]);
  });

  it("the mock updates a harness into a version dir: 422 for a bad version or an npm error; running agents keep their version", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    await expect(api.harnessUpdate("nope")).rejects.toMatchObject({ status: 422, code: "invalid" });
    await expect(api.harnessUpdate("codex", { version: "1.2" })).rejects.toMatchObject({ status: 422, code: "invalid" });
    const npm = await api.harnessUpdate("codex", { version: "99.0.0" }).catch((e: unknown) => e);
    expect(npm).toMatchObject({ status: 422, code: "invalid" });
    expect(updateFailure(npm).output).toContain("notarget");

    // The mock has live codex agents: no 409; the answer names them.
    const res = await api.harnessUpdate("codex");
    expect(Object.keys(res).sort()).toEqual(["agents_on_old_version", "from", "harness", "in_range", "note", "output", "package", "reused", "tested", "to"]);
    expect(res).toMatchObject({ harness: "codex", from: "0.164.0", to: "0.165.0", in_range: false, reused: false });
    expect(res.agents_on_old_version.map((a) => a.agent)).toContain("terrarium/adapter-codex");
    expect(res.agents_on_old_version.every((a) => a.version === "0.164.0")).toBe(true);
    expect(res.note).toContain("moves to 0.165.0 at its next restart");
    expect(resultLine(res)).toBe("codex 0.164.0 → 0.165.0");
    const codex = (await api.harnesses(false, true)).harnesses.find((h) => h.harness === "codex")!;
    expect(codex).toMatchObject({ installed: "0.165.0", active: "0.165.0", versions: ["0.164.0", "0.165.0"], update: "none", in_range: false });
    expect(oldAgents(codex).length).toBe(res.agents_on_old_version.length);
    const ev = await api.events.history({ kind: "harness" });
    expect(ev.items.at(-1)?.data).toMatchObject({ change: "updated", harness: "codex", from: "0.164.0", to: "0.165.0", forced: false });

    // Back to the old version: it is on disk, so no npm run.
    const back = await api.harnessUpdate("codex", { version: "0.164.0" });
    expect(back).toMatchObject({ reused: true, agents_on_old_version: [] });

    // All: no 409 while agents run. pi's npm does not answer in the mock, so "all" is a 422 with the results.
    const all = await api.harnessUpdateAll().catch((e: unknown) => e);
    expect(all).toMatchObject({ status: 422 });
    const af = updateFailure(all);
    expect(af.output).toContain("pi: npm install -g @earendil-works/pi-coding-agent@latest failed");
    expect(af.output).toContain("copilot-prompt 0.1.2 → 0.1.2");
  });
});
