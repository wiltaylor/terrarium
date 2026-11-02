import { describe, expect, it } from "vitest";
import type { Agent } from "../api/types";
import { compactButton, idleStopped, lastCompactedText, lifecycleActions, lifecycleBadge, newSessionButton, queuedLabel, resetBody, resetChip, resetNotice, resetThenProblem } from "./lifecycle";

type L = Pick<Agent, "status" | "paused" | "status_reason" | "waiting_on" | "queued">;
const a = (over: Partial<L> = {}): L => ({ status: "idle", paused: false, ...over });

describe("lifecycleBadge", () => {
  it("shows the status of an agent that runs", () => {
    expect(lifecycleBadge(a())).toMatchObject({ label: "idle", cls: "st-idle", title: "Status: idle" });
    expect(lifecycleBadge(a({ status: "working" }))).toMatchObject({ label: "working", cls: "st-working" });
    expect(lifecycleBadge(a({ status: "waiting", waiting_on: "time 10m" })).title).toBe("Status: waiting on time 10m");
  });

  it("shows `stopped (idle)` for an idle stop, and `stopped` for another stop", () => {
    expect(lifecycleBadge(a({ status: "stopped", status_reason: "idle" }))).toMatchObject({ label: "stopped (idle)", cls: "st-stopped" });
    expect(lifecycleBadge(a({ status: "stopped", status_reason: "stopped" }))).toMatchObject({ label: "stopped", cls: "st-stopped" });
    expect(lifecycleBadge(a({ status: "stopped" })).label).toBe("stopped");
    // An idle reason on a live status is old: the status wins.
    expect(lifecycleBadge(a({ status: "starting", status_reason: "idle" })).label).toBe("starting");
  });

  it("shows `paused` for a paused agent, whatever its status", () => {
    expect(lifecycleBadge(a({ status: "stopped", paused: true, status_reason: "paused" }))).toMatchObject({ label: "paused", cls: "st-paused" });
    expect(lifecycleBadge(a({ status: "stopped", paused: true, status_reason: "idle" })).label).toBe("paused");
    expect(lifecycleBadge(a({ status: "working", paused: true })).label).toBe("paused");
  });

  it("knows an idle stop", () => {
    expect(idleStopped({ status: "stopped", status_reason: "idle" })).toBe(true);
    expect(idleStopped({ status: "stopped", status_reason: "paused" })).toBe(false);
    expect(idleStopped({ status: "idle", status_reason: "idle" })).toBe(false);
  });
});

describe("queuedLabel", () => {
  it("counts the work that waits, and is null when there is none", () => {
    expect(queuedLabel({ queued: 3 })).toBe("3 queued");
    expect(queuedLabel({ queued: 0 })).toBeNull();
    expect(queuedLabel({})).toBeNull();
  });
});

describe("lifecycleActions", () => {
  it("has Pause and Restart, and no Start or Stop, for an agent that is not paused", () => {
    for (const status of ["idle", "working", "stopped", "error"] as const) {
      const b = lifecycleActions({ ...a({ status }) });
      expect(b.map((x) => [x.kind, x.label, x.disabled])).toEqual([
        ["pause", "Pause", false],
        ["restart", "Restart", false],
      ]);
    }
  });

  it("has Resume, and Restart off, for a paused agent", () => {
    const b = lifecycleActions({ paused: true });
    expect(b.map((x) => [x.kind, x.label, x.disabled])).toEqual([
      ["resume", "Resume", false],
      ["restart", "Restart", true],
    ]);
    expect(b[1]!.title).toMatch(/Resume it first/);
  });
});

describe("compactButton", () => {
  type C = Parameters<typeof compactButton>[0];
  const c = (over: Partial<C> = {}): C => ({ harness: "claude", status: "idle", paused: false, compact_supported: true, ...over });

  it("is on for an agent at rest, and for a busy agent (the request waits for the end of the turn)", () => {
    expect(compactButton(c()).disabled).toBe(false);
    expect(compactButton(c({ status: "waiting" })).disabled).toBe(false);
    expect(compactButton(c({ status: "working" }))).toMatchObject({ disabled: false, title: expect.stringMatching(/working.*until the current turn ends/) });
    expect(compactButton(c({ status: "blocked" })).disabled).toBe(false);
    expect(compactButton(c({ status: "starting" })).disabled).toBe(false);
    expect(compactButton(c({ status: "working", status_reason: "compacting" }))).toMatchObject({ disabled: false, title: expect.stringMatching(/compacts its session now/) });
  });

  it("is off, with the reason, when the daemon would refuse it", () => {
    expect(compactButton(c({ compact_supported: false, harness: "pi" }))).toEqual({ disabled: true, title: "auto compact not supported for pi" });
    expect(compactButton(c({ paused: true, status: "stopped" }))).toMatchObject({ disabled: true, title: expect.stringMatching(/paused/) });
    expect(compactButton(c(), true)).toMatchObject({ disabled: true, title: expect.stringMatching(/hold/) });
    expect(compactButton(c({ status: "stopped" }))).toMatchObject({ disabled: true, title: expect.stringMatching(/stopped.*only while it runs/) });
    expect(compactButton(c({ status: "error" })).disabled).toBe(true);
  });
});

describe("newSessionButton", () => {
  it("is always on, and the title tells when the new session starts", () => {
    expect(newSessionButton({ status: "idle", paused: false })).toMatchObject({ disabled: false, title: expect.stringMatching(/now/) });
    expect(newSessionButton({ status: "working", paused: false })).toMatchObject({ disabled: false, title: expect.stringMatching(/until the current turn ends/) });
    expect(newSessionButton({ status: "stopped", paused: false })).toMatchObject({ disabled: false, title: expect.stringMatching(/Without one, its next wake starts a new session/) });
    expect(newSessionButton({ status: "stopped", paused: true })).toMatchObject({ disabled: false, title: expect.stringMatching(/paused.*until you resume/) });
  });
});

describe("resetChip", () => {
  const pending = { kind: "compact" as const, by: "operator", then_set: true, recap: false, deferred: false, requested_at: "T" };

  it("is null when no reset waits", () => {
    expect(resetChip({ status: "working" })).toBeNull();
  });

  it("names the kind, who asked, and the then prompt", () => {
    const chip = resetChip({ status: "working", reset_pending: pending }, (iso) => `at ${iso}`)!;
    expect(chip.label).toBe("compaction queued");
    expect(chip.title).toMatch(/Requested by operator at T\./);
    expect(chip.title).toMatch(/A "then" prompt follows it/);
    expect(chip.title).not.toMatch(/recap/);
    const fresh = resetChip({ status: "working", reset_pending: { ...pending, kind: "new_session", by: "demo/lead", then_set: false, recap: true } })!;
    expect(fresh.label).toBe("new session queued");
    expect(fresh.title).toMatch(/Requested by demo\/lead\..*No "then" prompt\..*A recap turn goes first/);
  });

  it("says that a stopped agent starts the new session at its next wake", () => {
    const chip = resetChip({ status: "stopped", reset_pending: { ...pending, kind: "new_session" } })!;
    expect(chip.label).toBe("new session at next wake");
    expect(chip.title).toMatch(/next wake/);
  });
});

describe("resetNotice", () => {
  const r = { kind: "compact" as const, state: "started" as const, replaced: false, deferred: false, then_set: false };

  it("tells when the reset runs", () => {
    expect(resetNotice(r)).toBe("Compaction started.");
    expect(resetNotice({ ...r, state: "queued", then_set: true })).toBe('Compaction queued: it runs when the current turn ends. The "then" prompt goes as the next turn.');
    expect(resetNotice({ ...r, kind: "new_session", state: "queued" }, true)).toBe("New session queued: it runs after you resume the agent.");
    expect(resetNotice({ ...r, kind: "new_session", state: "next_wake" })).toMatch(/^New session at the next wake/);
  });

  it("says when the request replaced a queued request", () => {
    expect(resetNotice({ ...r, state: "queued", replaced: true })).toMatch(/It replaced the queued request\.$/);
  });
});

describe("lastCompactedText", () => {
  it("says when the last compaction was, and when it failed", () => {
    const at = (iso: string) => `at ${iso}`;
    expect(lastCompactedText({}, at)).toBeNull();
    expect(lastCompactedText({ last_compact_at: "T", last_compact_ok: true }, at)).toBe("last compacted at T");
    expect(lastCompactedText({ last_compact_at: "T", last_compact_ok: false }, at)).toBe("last compacted at T (failed)");
  });
});

describe("resetThenProblem and resetBody", () => {
  it("allows at most 32 KiB", () => {
    expect(resetThenProblem("x".repeat(32 * 1024))).toBeNull();
    expect(resetThenProblem("x".repeat(32 * 1024 + 1))).toMatch(/at most 32768/);
    // Bytes, not characters.
    expect(resetThenProblem("é".repeat(16 * 1024 + 1))).not.toBeNull();
  });

  it("leaves out a blank then prompt, and sends recap only for a new session", () => {
    expect(resetBody("compact", "  ", true)).toEqual({});
    expect(resetBody("compact", "go on", true)).toEqual({ then: "go on" });
    expect(resetBody("new_session", "", false)).toEqual({ recap: false });
    expect(resetBody("new_session", "go on", true)).toEqual({ then: "go on", recap: true });
  });
});
