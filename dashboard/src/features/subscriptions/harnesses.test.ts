import { describe, expect, it } from "vitest";
import type { HarnessReport, HarnessVersion } from "../../api/types";
import { createMockApi } from "../../api/mock";
import { harnessUpdateKind } from "../../lib/harness-update";
import { canPinCurrent, canUnpin, checkLine, harnessStatus, lastCheckLine, offPin, oldAgentLine, oldAgents, otherVersions, pinLine, policyChip, publishedDay, resultLine, resultNotes, rollbackTarget, scheduleNote, updateCommand, updateFailure, withUpdate } from "./harnesses";
import type { HarnessAutoUpdate } from "../../api/types";

const row = (over: Partial<HarnessVersion>): HarnessVersion => ({
  harness: "claude",
  package: "@anthropic-ai/claude-code",
  pinned: "2.1.310",
  installed: "2.1.310",
  checked_at: "2026-10-25T10:00:00Z",
  ...over,
});

const report = (...harnesses: HarnessVersion[]): HarnessReport => ({ harnesses });

describe("harness versions table", () => {
  it("names the status from the npm update", () => {
    const latest = { version: "2.1.310", published_at: "2026-10-20T10:00:00Z" };
    expect(harnessStatus(row({ latest, update: "none" }))).toMatchObject({ status: "latest", cls: "ok" });
    expect(harnessStatus(row({ latest: { ...latest, version: "2.1.320" }, update: "minor" }))).toMatchObject({ status: "update available", cls: "info" });
    const major = harnessStatus(row({ latest: { ...latest, version: "3.0.0" }, update: "major" }));
    expect(major).toMatchObject({ status: "update available (major)", cls: "warn" });
    expect(major.title).toContain("major bump");
    // A version far past the pin is not a warning: no tested range.
    expect(harnessStatus(row({ installed: "9.0.0", latest: { ...latest, version: "9.0.0" }, update: "none" }))).toMatchObject({ status: "latest", cls: "ok" });
    // npm did not answer: the installed version alone.
    const noNpm = harnessStatus(row({ latest: null, update: null, latest_error: "cannot reach npm: timeout" }));
    expect(noNpm).toEqual({ status: "installed", cls: "ok", title: "2.1.310 is installed. The latest release is not known: cannot reach npm: timeout" });
    // Without `latest=true`: the installed version alone.
    expect(harnessStatus(row({})).status).toBe("installed");
    expect(harnessStatus(row({ installed: null, error: "cannot run claude" }))).toEqual({ status: "unknown", cls: "warn", title: "cannot run claude" });
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

  it("spots a version off the pin", () => {
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
    const res = { harness: "copilot", package: "@github/copilot", from: "1.0.103", to: "1.0.110", output: "", reused: false, agents_on_old_version: [], note: "1 live agent(s) run another version", removed: ["1.0.90"] };
    expect(resultNotes(res)).toEqual(["1 live agent(s) run another version", "copilot: removed the unused version 1.0.90"]);
    expect(resultNotes({ ...res, note: undefined, removed: [] })).toEqual([]);
  });

  it("the mock answers GET /api/harnesses in the daemon's shape", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const r = await api.harnesses();
    expect(r).not.toHaveProperty("policy");
    expect(r.harnesses.map((h) => h.harness)).toEqual(["claude", "codex", "copilot", "pi"]);
    for (const h of r.harnesses) {
      for (const k of Object.keys(h)) expect(["active", "agents", "auto_update", "checked_at", "harness", "installed", "package", "pinned", "versions"]).toContain(k);
      expect(h.active).toBe(h.installed);
      expect(h.versions).toContain(h.installed);
      expect(Object.keys(h.auto_update!).sort()).toEqual(["changelog", "checked_at", "detail", "latest", "pinned_version", "policy", "previous", "result"]);
    }
  });

  it("the mock answers ?latest=true with latest, update, and latest_error", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const r = await api.harnesses(false, true);
    for (const h of r.harnesses) {
      const keys = Object.keys(h)
        .filter((k) => !["active", "versions", "agents", "auto_update"].includes(k))
        .sort();
      if (h.latest === null) expect(keys).toEqual(["checked_at", "harness", "installed", "latest", "latest_error", "package", "pinned", "update"]);
      else expect(keys).toEqual(["checked_at", "harness", "installed", "latest", "package", "pinned", "update"]);
      if (h.latest) {
        expect(Object.keys(h.latest).sort()).toEqual(["published_at", "version"]);
        expect(h.update).toBe(harnessUpdateKind(h.pinned, h.latest.version));
      } else expect(h.update).toBeNull();
    }
    expect(r.harnesses.map((h) => harnessStatus(h).status)).toEqual(["latest", "update available (major)", "update available", "installed"]);
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
    expect(Object.keys(res).sort()).toEqual(["agents_on_old_version", "from", "harness", "note", "output", "package", "reused", "to"]);
    expect(res).toMatchObject({ harness: "codex", from: "0.164.0", to: "0.165.0", reused: false });
    expect(res.agents_on_old_version.map((a) => a.agent)).toContain("terrarium/adapter-codex");
    expect(res.agents_on_old_version.every((a) => a.version === "0.164.0")).toBe(true);
    expect(res.note).toContain("moves to 0.165.0 at its next restart");
    expect(resultLine(res)).toBe("codex 0.164.0 → 0.165.0");
    const codex = (await api.harnesses(false, true)).harnesses.find((h) => h.harness === "codex")!;
    expect(codex).toMatchObject({ installed: "0.165.0", active: "0.165.0", versions: ["0.164.0", "0.165.0"], update: "none" });
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

  it("names the update policy, the rollback target, and the last check", () => {
    const au = (over: Partial<HarnessAutoUpdate>): HarnessAutoUpdate => ({ policy: "auto", pinned_version: null, previous: null, latest: null, checked_at: null, result: null, detail: null, changelog: null, ...over });
    expect(policyChip(row({})).label).toBe("—");
    expect(policyChip(row({ auto_update: au({}) }))).toMatchObject({ label: "auto", cls: "info" });
    const pinned = row({ auto_update: au({ policy: "pinned", pinned_version: "2.1.310" }) });
    expect(policyChip(pinned)).toMatchObject({ label: "pinned 2.1.310", cls: "warn" });
    expect(canUnpin(pinned)).toBe(true);
    expect(canPinCurrent(pinned)).toBe(false);
    expect(canPinCurrent(row({ active: "2.1.310", auto_update: au({}) }))).toBe(true);
    // The rollback target is the previous version, never the active one.
    expect(rollbackTarget(row({ active: "2.1.320", auto_update: au({ previous: "2.1.310" }) }))).toBe("2.1.310");
    expect(rollbackTarget(row({ active: "2.1.310", auto_update: au({ previous: "2.1.310" }) }))).toBeNull();
    // After a rollback the previous version is newer: no "Roll back" to it.
    expect(rollbackTarget(row({ active: "2.1.310", auto_update: au({ previous: "2.1.320" }) }))).toBeNull();
    expect(rollbackTarget(row({}))).toBeNull();
    expect(lastCheckLine(row({ auto_update: au({}) }))).toBeNull();
    expect(lastCheckLine(row({ auto_update: au({ checked_at: "2026-10-31T10:05:12.000Z", result: "updated", detail: "1.4.0 -> 1.5.1" }) }))).toBe("2026-10-31 10:05 · updated: 1.4.0 -> 1.5.1");
  });

  it("names the schedule, a check, and a pin", () => {
    expect(scheduleNote(report())).toBe("");
    expect(scheduleNote({ ...report(), auto_update: { schedule: "off", next_check_at: null } })).toContain("TERRARIUM_HARNESS_AUTO_UPDATE=off");
    expect(scheduleNote({ ...report(), auto_update: { schedule: "6h", next_check_at: "2026-10-31T10:05:00Z" } })).toBe(
      "The auto-update checks npm every 6h and installs each newer release of a harness that is not pinned. The next check is at 2026-10-31 10:05 UTC.",
    );
    expect(checkLine({ harness: "codex", result: "updated", detail: "0.164.0 -> 0.165.0", latest: "0.165.0", mailed: true })).toBe("codex: updated: 0.164.0 -> 0.165.0 (mailed)");
    expect(pinLine({ harness: "pi", policy: "pinned", pinned_version: "1.4.0", from: "1.5.1", to: "1.4.0", agents_on_old_version: [] })).toBe("pi: 1.5.1 → 1.4.0, pinned. New agent starts use 1.4.0");
    expect(pinLine({ harness: "pi", policy: "pinned", pinned_version: "1.5.1", from: "1.5.1", to: "1.5.1", agents_on_old_version: [] })).toBe("pi: pinned to 1.5.1");
    expect(pinLine({ harness: "pi", policy: "auto", pinned_version: null, from: "1.5.1", to: "1.5.1", agents_on_old_version: [] })).toContain("unpinned");
  });

  it("the mock checks, rolls back and pins, and unpins like the daemon", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const get = async (h: string) => (await api.harnesses(false, true)).harnesses.find((x) => x.harness === h)!;
    const r = await api.harnesses();
    expect(r.auto_update?.schedule).toBe("daily");
    expect(policyChip((await get("claude"))).label).toBe("pinned 2.1.310");
    const pi = await get("pi");
    expect(rollbackTarget(pi)).toBe("1.4.0");
    expect(pi.auto_update?.detail).toBe("1.4.0 -> 1.5.1");

    // The check: codex has a newer release: an install and no mail (TERRARIUM_HARNESS_UPDATE_MAIL is off by default); claude is pinned; pi's npm is down.
    const c = await api.harnessCheck();
    const by = Object.fromEntries(c.results.map((i) => [i.harness, i]));
    expect(by.codex).toMatchObject({ result: "updated", from: "0.164.0", to: "0.165.0", detail: "0.164.0 -> 0.165.0", mailed: false });
    expect(by.claude).toMatchObject({ result: "pinned", mailed: false });
    expect(by.copilot).toMatchObject({ result: "updated" });
    expect(by.pi).toMatchObject({ result: "skipped" });
    const mail = await api.mail.list({ to: "operator" });
    const subjects = mail.items.map((m) => m.subject);
    expect(subjects.filter((s) => s.startsWith("Harness update:"))).toEqual([]);
    expect(c.results.every((i) => !i.mailed)).toBe(true);
    // Nothing new: no new mail.
    const again = await api.harnessCheck();
    expect(again.results.find((i) => i.harness === "codex")).toMatchObject({ result: "current", mailed: false });
    expect((await api.mail.list({ to: "operator" })).items.length).toBe(mail.items.length);

    // Roll back and pin: to the previous version, at once; the newer version stays on disk.
    const pin = await api.harnessPin("codex");
    expect(pin).toMatchObject({ policy: "pinned", pinned_version: "0.164.0", from: "0.165.0", to: "0.164.0" });
    const codex = await get("codex");
    expect(codex.active).toBe("0.164.0");
    expect(codex.versions).toEqual(["0.164.0", "0.165.0"]);
    expect(codex.auto_update).toMatchObject({ policy: "pinned", pinned_version: "0.164.0", previous: "0.165.0" });
    expect((await api.harnessCheck()).results.find((i) => i.harness === "codex")).toMatchObject({ result: "pinned" });
    await expect(api.harnessPin("copilot-prompt")).rejects.toMatchObject({ status: 422 });
    await expect(api.harnessPin("pi", "latest")).rejects.toMatchObject({ status: 422 });

    // Unpin: back to auto; the next check updates again.
    const un = await api.harnessUnpin("codex");
    expect(un).toMatchObject({ policy: "auto", pinned_version: null, to: "0.164.0" });
    expect((await api.harnessCheck()).results.find((i) => i.harness === "codex")).toMatchObject({ result: "updated", to: "0.165.0" });
    const ev = await api.events.history({ kind: "harness" });
    expect(ev.items.map((e) => (e.data as { change: string }).change)).toEqual(expect.arrayContaining(["auto_updated", "pinned", "unpinned"]));
  });
});
