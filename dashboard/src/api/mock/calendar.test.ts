// The calendar of the mock answers in the daemon's shapes (docs/api.md §33).
import { describe, expect, it } from "vitest";
import { createMockApi } from ".";
import type { AgentEvent } from "../types";

const mock = (opts = {}) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, ...opts });
const wait = (ms = 5) => new Promise((r) => setTimeout(r, ms));

describe("mock calendar", () => {
  it("seeds project events and a global one, and expands the repeats", async () => {
    const api = mock();
    const v = await api.calendar.project("terrarium", { days: 31 });
    expect(v.can_write).toBe(true);
    expect(v.occurrences.some((o) => o.global && o.kind === "window")).toBe(true);
    expect(v.occurrences.filter((o) => o.title === "Weekly patch window").length).toBeGreaterThanOrEqual(4);
    expect(v.events.every((e) => typeof e.can_edit === "boolean")).toBe(true);
    const web = await api.calendar.project("webshop", { days: 31 });
    expect(web.occurrences.every((o) => o.global)).toBe(true);
    const all = await api.calendar.combined({ days: 31 });
    expect(all.occurrences.length).toBe(v.occurrences.length);
    const g = await api.calendar.global({ days: 31 });
    expect(g.occurrences.every((o) => o.global)).toBe(true);
  });

  it("creates, checks the fields, skips, assigns, and emits events", async () => {
    const api = mock();
    const frames: AgentEvent[] = [];
    api.events.stream({ since: api.world.seq }).subscribe((e) => frames.push(e));
    await wait();
    const e = await api.calendar.create("terrarium", { title: "DB upgrade", kind: "maintenance", start: "2026-11-10T09:00", tz: "Europe/London", recurrence: "FREQ=DAILY;COUNT=3", assignees: ["lead"] });
    expect(e.rrule).toBe("FREQ=DAILY;COUNT=3");
    expect(e.end).toBe("2026-11-10T10:00:00.000Z");
    expect(e.assignees.map((a) => a.name)).toEqual(["lead"]);
    await expect(api.calendar.create("terrarium", { title: "x", start: "2026-11-10T09:00", recurrence: "FREQ=YEARLY" })).rejects.toMatchObject({ status: 422, details: { field: "recurrence" } });
    await expect(api.calendar.create("terrarium", { title: "x", start: "2026-11-10T09:00", assignees: ["shop-lead"] })).rejects.toMatchObject({ status: 422 });
    const s = await api.calendar.skip(e.id, "2026-11-11");
    expect(s.exceptions).toEqual(["2026-11-11T09:00:00.000Z"]);
    await expect(api.calendar.skip(e.id, "2026-11-20")).rejects.toMatchObject({ status: 422 });
    const u = await api.calendar.update(e.id, { title: "DB upgrade v2" });
    expect(u.title).toBe("DB upgrade v2");
    expect(u.exceptions).toHaveLength(1);
    await api.calendar.assign(e.id, "runner-dev");
    await api.calendar.remove(e.id);
    await expect(api.calendar.get(e.id)).rejects.toMatchObject({ status: 404 });
    await wait();
    const kinds = frames.map((f) => f.kind);
    expect(kinds).toContain("calendar.event_created");
    expect(kinds).toContain("calendar.event_updated");
    expect(kinds).toContain("calendar.event_deleted");
  });

  it("reads only for a member, and keeps global events to a sysadmin", async () => {
    const api = mock({ scheduleCaller: { role: "member", project: "terrarium" } });
    const v = await api.calendar.project("terrarium", { days: 31 });
    expect(v.can_write).toBe(false);
    expect(v.events.every((e) => e.can_edit === false)).toBe(true);
    await expect(api.calendar.create("terrarium", { title: "x", start: "2026-11-10T09:00" })).rejects.toMatchObject({ status: 403 });
    await expect(api.calendar.project("webshop")).rejects.toMatchObject({ status: 403 });
    const lead = mock({ scheduleCaller: { role: "project_admin", project: "terrarium" } });
    await expect(lead.calendar.create(null, { title: "x", start: "2026-11-10T09:00" })).rejects.toMatchObject({ status: 403 });
    const lv = await lead.calendar.project("terrarium", { days: 31 });
    expect(lv.events.filter((e) => e.global).every((e) => e.can_edit === false)).toBe(true);
    expect(lv.events.filter((e) => !e.global).every((e) => e.can_edit === true)).toBe(true);
  });

  it("mails the watchers from system, and keeps the grants", async () => {
    const api = mock();
    await api.calendar.watch("terrarium", "lead");
    expect((await api.calendar.watchers("terrarium")).map((w) => w.name)).toEqual(["terrarium/lead"]);
    await api.calendar.create("terrarium", { title: "Deploy", kind: "deploy", start: "2026-11-12T09:00:00Z" });
    const mail = api.world.mail.filter((m) => m.from === "system" && m.subject.startsWith("Calendar terrarium: added"));
    expect(mail).toHaveLength(1);
    expect(mail[0]!.to_name).toBe("terrarium/lead");
    const g = await api.calendar.grant("terrarium", "runner-dev");
    expect(g).toMatchObject({ calendar_write: true, changed: true });
    expect((await api.calendar.grants("terrarium")).map((x) => x.name)).toEqual(["runner-dev"]);
    await api.calendar.revoke("terrarium", "runner-dev");
    expect(await api.calendar.grants("terrarium")).toEqual([]);
  });
});
