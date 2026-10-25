import { describe, expect, it } from "vitest";
import { ApiError } from "../api/http";
import type { AgentEvent, Lease } from "../api/types";
import { actorLabel, applyLeaseEvent, countdown, holderLabel, leaseActive, leaseEventData, leaseHeld, shownState, untilValue } from "./leases";

const NOW = Date.parse("2026-10-25T09:00:00.000Z");
const at = (ms: number) => new Date(NOW + ms).toISOString();

const lease = (id: string, extra: Partial<Lease> = {}): Lease => ({
  id,
  target: "vm",
  agent: "01AGENT",
  agent_name: "terra/w-ter-3",
  project: "terra",
  granted_by: "operator",
  granted_at: at(-60_000),
  expires_at: at(30 * 60_000),
  exclusive: true,
  revoked_at: null,
  revoked_by: null,
  reason: "TER-3",
  state: "active",
  ...extra,
});

let seq = 0;
const ev = (data: Record<string, unknown>, extra: Partial<AgentEvent> = {}): AgentEvent =>
  ({ seq: ++seq, id: `E${seq}`, at: at(0), kind: "lease", actor: "operator", agent: "w-ter-3", agent_id: "01AGENT", summary: "", data, ...extra }) as AgentEvent;

describe("countdown", () => {
  it("formats seconds, minutes with seconds, hours with minutes, and days with hours", () => {
    expect(countdown(at(42_000), NOW)).toBe("42s");
    expect(countdown(at(400), NOW)).toBe("1s");
    expect(countdown(at(12 * 60_000 + 5_000), NOW)).toBe("12m 05s");
    expect(countdown(at(60_000), NOW)).toBe("1m 00s");
    expect(countdown(at(2 * 3_600_000 + 5 * 60_000), NOW)).toBe("2h 05m");
    expect(countdown(at(3 * 86_400_000 + 4 * 3_600_000), NOW)).toBe("3d 4h");
  });

  it("says until revoked for no end, expired for a past end, and a dash for a bad time", () => {
    expect(countdown(null, NOW)).toBe("until revoked");
    expect(countdown(undefined, NOW)).toBe("until revoked");
    expect(countdown(at(0), NOW)).toBe("expired");
    expect(countdown(at(-5_000), NOW)).toBe("expired");
    expect(countdown("not a time", NOW)).toBe("—");
  });

  it("counts down as the clock moves", () => {
    const end = at(61_000);
    expect([0, 1_000, 2_000, 60_500].map((d) => countdown(end, NOW + d))).toEqual(["1m 01s", "1m 00s", "59s", "1s"]);
  });
});

describe("lease state helpers", () => {
  it("treats an active lease past its time as expired before the daemon's timer", () => {
    const l = lease("L1", { expires_at: at(1_000) });
    expect(leaseActive(l, NOW)).toBe(true);
    expect(leaseActive(l, NOW + 1_000)).toBe(false);
    expect(shownState(l, NOW + 2_000)).toBe("expired");
    expect(shownState({ ...l, state: "revoked" }, NOW)).toBe("revoked");
    expect(leaseActive({ ...l, expires_at: null }, NOW + 1e12)).toBe(true);
  });

  it("names the holder and the actors", () => {
    expect(holderLabel(lease("L1"))).toBe("terra/w-ter-3");
    expect(holderLabel(lease("L1", { agent_name: null }))).toBe("01AGENT");
    expect(actorLabel("operator")).toBe("operator");
    expect(actorLabel("system")).toBe("system");
    expect(actorLabel("agent:01LEAD", (id) => (id === "01LEAD" ? "terra/lead" : undefined))).toBe("terra/lead");
    expect(actorLabel("agent:01GONE", () => undefined)).toBe("01GONE");
    expect(actorLabel(null)).toBe("—");
  });
});

describe("untilValue", () => {
  it("sends a duration as it is, a local time as RFC 3339, and none as null", () => {
    expect(untilValue("duration", " 30m ", NOW)).toEqual({ until: "30m" });
    expect(untilValue("duration", "2 h", NOW)).toEqual({ until: "2h" });
    expect(untilValue("none", "whatever", NOW)).toEqual({ until: null });
    const local = "2026-10-26T11:30"; // a day later: in the future in every time zone
    expect(untilValue("time", local, NOW)).toEqual({ until: new Date(local).toISOString() });
  });

  it("refuses a bad duration, a zero duration, an empty time, and a time in the past", () => {
    expect(untilValue("duration", "30", NOW)).toHaveProperty("error");
    expect(untilValue("duration", "1w", NOW)).toHaveProperty("error");
    expect(untilValue("duration", "0m", NOW)).toHaveProperty("error");
    expect(untilValue("time", "", NOW)).toHaveProperty("error");
    expect(untilValue("time", "2020-01-01T00:00", NOW)).toEqual({ error: "The time must be in the future." });
  });
});

describe("leaseHeld", () => {
  it("reads the holder and the expiry from a 409 lease_held", () => {
    const details = { target: "vm", holder: "terra/w-ter-1", holder_id: "01W1", lease_id: "01L", exclusive: true, expires_at: at(600_000) };
    const e = new ApiError(409, "target `vm` is held exclusively by terra/w-ter-1", { error: { code: "lease_held", details } }, "lease_held", details);
    expect(leaseHeld(e)).toEqual({ ...details, message: "target `vm` is held exclusively by terra/w-ter-1" });
  });

  it("is null for other errors, and fills in a holder when details are missing", () => {
    expect(leaseHeld(new ApiError(409, "conflict", undefined, "conflict"))).toBeNull();
    expect(leaseHeld(new Error("x"))).toBeNull();
    expect(leaseHeld(new ApiError(409, "held", undefined, "lease_held"))).toMatchObject({ holder: "another agent", expires_at: null, exclusive: false });
  });
});

describe("applyLeaseEvent", () => {
  const byTarget = { match: (d: { target: string }) => d.target === "vm" };

  it("reads the event data and ignores other kinds and bad payloads", () => {
    expect(leaseEventData(ev({ change: "granted", target: "vm", lease_id: "L1", expires_at: null, exclusive: false, state: "active" }))).toMatchObject({ change: "granted", lease_id: "L1" });
    expect(leaseEventData(ev({ change: "moved", target: "vm", lease_id: "L1" }))).toBeNull();
    expect(leaseEventData(ev({ change: "granted" }))).toBeNull();
    expect(leaseEventData({ ...ev({}), kind: "status" } as AgentEvent)).toBeNull();
    const list = [lease("L1")];
    expect(applyLeaseEvent(list, { ...ev({}), kind: "status" } as AgentEvent, byTarget)).toEqual({ list, refetch: false });
  });

  it("asks for a refetch on a grant it does not know: the event has no granted_by or reason", () => {
    const list = [lease("L1")];
    const r = applyLeaseEvent(list, ev({ change: "granted", target: "vm", lease_id: "L2", expires_at: null, exclusive: false, state: "active" }), byTarget);
    expect(r.refetch).toBe(true);
    expect(r.list).toBe(list);
  });

  it("moves the end on an extend", () => {
    const r = applyLeaseEvent([lease("L1")], ev({ change: "extended", target: "vm", lease_id: "L1", expires_at: at(7_200_000), exclusive: true, state: "active" }), byTarget);
    expect(r.refetch).toBe(false);
    expect(r.list[0]).toMatchObject({ id: "L1", expires_at: at(7_200_000), state: "active", reason: "TER-3" });
  });

  it("drops an ended lease from the active list, and keeps it with its state in the history", () => {
    const end = ev({ change: "ended", target: "vm", lease_id: "L1", expires_at: at(1_000), exclusive: true, state: "expired" }, { actor: "system" });
    expect(applyLeaseEvent([lease("L1"), lease("L2")], end, byTarget).list.map((l) => l.id)).toEqual(["L2"]);
    const kept = applyLeaseEvent([lease("L1")], end, { ...byTarget, keepEnded: true });
    expect(kept.list[0]).toMatchObject({ state: "expired", revoked_by: "system", revoked_at: at(0) });
    // An end the active list never had needs nothing.
    expect(applyLeaseEvent([], end, byTarget)).toEqual({ list: [], refetch: false });
  });

  it("leaves the list alone for an event about another target or agent", () => {
    const list = [lease("L1")];
    const other = ev({ change: "ended", target: "box", lease_id: "L1", expires_at: null, exclusive: false, state: "revoked" });
    expect(applyLeaseEvent(list, other, byTarget).list).toBe(list);
    const byAgent = { match: (_d: unknown, agent: string | undefined) => agent === "01OTHER" };
    expect(applyLeaseEvent(list, ev({ change: "ended", target: "vm", lease_id: "L1", expires_at: null, exclusive: false, state: "revoked" }), byAgent).list).toBe(list);
  });
});
