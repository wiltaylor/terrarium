// The pure parts of the Calendar page and the recurrence rules (docs/api.md §33).
import { describe, expect, it } from "vitest";
import type { CalendarEvent, CalendarOccurrence } from "../../api/types";
import { agenda, emptyForm, formError, formOf, inputOf, monthGrid, monthRange, onDay, timeLabel, weekDays } from "./model";
import { describeRule, expand, localInstant, parseRrule, toRrule, zoned } from "./recur";

const t = (s: string) => Date.parse(s);
const occ = (start: string, end: string, extra: Partial<CalendarOccurrence> = {}): CalendarOccurrence => ({
  event_id: "E",
  start,
  end,
  all_day: false,
  title: "x",
  kind: "maintenance",
  project: "terra",
  global: false,
  recurring: false,
  assignees: [],
  ...extra,
});

describe("calendar recurrence", () => {
  it("keeps the local time across a DST change, as the daemon does", () => {
    const s = { start: t("2027-03-14T09:00:00Z"), end: t("2027-03-14T10:00:00Z"), allDay: false, tz: "Europe/London", rule: { freq: "weekly" as const }, skipped: new Set<number>() };
    const list = expand(s, t("2027-03-14T00:00:00Z"), t("2027-04-05T00:00:00Z")).map((o) => new Date(o.start).toISOString());
    expect(list).toEqual(["2027-03-14T09:00:00.000Z", "2027-03-21T09:00:00.000Z", "2027-03-28T08:00:00.000Z", "2027-04-04T08:00:00.000Z"]);
  });

  it("moves a skipped local time on and takes the first of a doubled one", () => {
    expect(new Date(localInstant({ y: 2027, m: 3, d: 28, h: 1, mi: 30, s: 0 }, "Europe/London")).toISOString()).toBe("2027-03-28T01:00:00.000Z");
    expect(new Date(localInstant({ y: 2027, m: 10, d: 31, h: 1, mi: 30, s: 0 }, "Europe/London")).toISOString()).toBe("2027-10-31T00:30:00.000Z");
    expect(zoned(t("2027-07-01T12:00:00Z"), "Europe/London").h).toBe(13);
  });

  it("counts skips, ends on a count, and skips short months", () => {
    const skipped = new Set([t("2026-11-03T22:00:00Z")]);
    const daily = { start: t("2026-11-01T22:00:00Z"), end: t("2026-11-01T23:00:00Z"), allDay: false, tz: "UTC", rule: { freq: "daily" as const, interval: 2, count: 3 }, skipped };
    expect(expand(daily, t("2026-10-01T00:00:00Z"), t("2027-01-01T00:00:00Z")).length).toBe(2);
    const monthly = { start: t("2027-01-31T12:00:00Z"), end: t("2027-01-31T13:00:00Z"), allDay: false, tz: "UTC", rule: { freq: "monthly" as const, count: 3 }, skipped: new Set<number>() };
    expect(expand(monthly, t("2027-01-01T00:00:00Z"), t("2028-01-01T00:00:00Z")).map((o) => new Date(o.start).toISOString().slice(0, 10))).toEqual(["2027-01-31", "2027-03-31", "2027-05-31"]);
  });

  it("reads the RRULE subset and refuses the rest", () => {
    const r = parseRrule("RRULE:FREQ=WEEKLY;INTERVAL=2;BYDAY=WE,MO;COUNT=6");
    expect(r).toEqual({ freq: "weekly", interval: 2, weekdays: ["mo", "we"], count: 6 });
    expect(toRrule(r as never)).toBe("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=6");
    expect(describeRule(r as never)).toBe("every 2 weeks on mo, we, 6 times");
    expect(parseRrule("FREQ=YEARLY")).toContain("YEARLY");
    expect(parseRrule("FREQ=WEEKLY;BYDAY=1MO")).toContain("BYDAY");
    expect(parseRrule("FREQ=DAILY;BYHOUR=9")).toContain("BYHOUR");
    expect(parseRrule("FREQ=DAILY;COUNT=2;UNTIL=20261231")).toContain("not both");
  });
});

describe("calendar views", () => {
  it("builds whole weeks from Monday for a month", () => {
    const grid = monthGrid(new Date(2026, 10, 15).getTime(), [], new Date(2026, 10, 4).getTime());
    expect(grid.length).toBe(6);
    expect(new Date(grid[0]![0]!.day).getDay()).toBe(1);
    expect(grid.flat().filter((d) => d.inMonth).length).toBe(30);
    expect(grid.flat().filter((d) => d.today).length).toBe(1);
    const r = monthRange(new Date(2026, 10, 15).getTime());
    expect(r.from).toBe(grid[0]![0]!.day);
  });

  it("puts a multi-day occurrence on each of its days, and a point on its day", () => {
    const freeze = occ(new Date(2026, 10, 10).toISOString(), new Date(2026, 10, 13).toISOString(), { all_day: true });
    const days = weekDays(new Date(2026, 10, 11).getTime(), [freeze], 0);
    expect(days.filter((d) => d.items.length).map((d) => new Date(d.day).getDate())).toEqual([10, 11, 12]);
    const point = new Date(2026, 10, 10, 9).toISOString();
    expect(onDay(occ(point, point), new Date(2026, 10, 10).getTime())).toBe(true);
    expect(timeLabel(freeze)).toBe("all day, to 2026-11-12");
    expect(timeLabel(occ(new Date(2026, 10, 10, 9).toISOString(), new Date(2026, 10, 10, 10, 30).toISOString()))).toBe("09:00–10:30");
  });

  it("groups the agenda by day, an ongoing occurrence under the first day", () => {
    const from = new Date(2026, 10, 10).getTime();
    const g = agenda(
      [occ(new Date(2026, 10, 12, 9).toISOString(), new Date(2026, 10, 12, 10).toISOString(), { title: "b" }), occ(new Date(2026, 10, 8).toISOString(), new Date(2026, 10, 11).toISOString(), { title: "a", all_day: true })],
      from,
    );
    expect(g.map((d) => d.items.map((o) => o.title))).toEqual([["a"], ["b"]]);
    expect(g[0]!.day).toBe(from);
  });
});

describe("calendar form", () => {
  const event: CalendarEvent = {
    id: "E",
    project: "terra",
    global: false,
    title: "Patch window",
    description: "d",
    kind: "maintenance",
    start: "2026-11-10T12:00:00.000Z",
    end: "2026-11-10T13:30:00.000Z",
    all_day: false,
    tz: "Australia/Brisbane",
    recurrence: { freq: "weekly", weekdays: ["tu"], count: 4 },
    rrule: "FREQ=WEEKLY;BYDAY=TU;COUNT=4",
    repeat: "every week on tu, 4 times",
    links: { target: "box" },
    assignees: [{ id: "01W", name: "worker", project: "terra" }],
    exceptions: [],
    created_by: "operator",
    created_by_name: "operator",
    updated_by: "operator",
    created_at: "",
    updated_at: "",
  };

  it("edits the times in the event's zone and round-trips the body", () => {
    const f = formOf(event);
    expect([f.startDate, f.startTime, f.endTime]).toEqual(["2026-11-10", "22:00", "23:30"]);
    expect(f.endMode).toBe("count");
    expect(inputOf(f, true)).toEqual({
      title: "Patch window",
      kind: "maintenance",
      description: "d",
      all_day: false,
      tz: "Australia/Brisbane",
      start: "2026-11-10T22:00",
      end: "2026-11-10T23:30",
      assignees: ["01W"],
      recurrence: { freq: "weekly", weekdays: ["tu"], count: 4 },
      links: { target: "box" },
    });
    // An edit with no repeat and no links clears them; a new event leaves them out.
    const plain = { ...f, repeat: "none" as const, target: "" };
    expect(inputOf(plain, true)).toMatchObject({ recurrence: null, links: null });
    expect(inputOf(plain)).not.toHaveProperty("recurrence");
  });

  it("names the first problem of the form", () => {
    const f = { ...emptyForm(Date.now()), title: "t" };
    expect(formError(f)).toBeNull();
    expect(formError({ ...f, title: " " })).toContain("title");
    expect(formError({ ...f, tz: "Mars/Base" })).toContain("zone");
    expect(formError({ ...f, endDate: "2000-01-01" })).toContain("before");
    expect(formError({ ...f, repeat: "monthly", monthDay: "32" })).toContain("month");
    expect(formError({ ...f, repeat: "daily", endMode: "count", count: "0" })).toContain("times");
    expect(formError({ ...f, url: "ftp://x" })).toContain("URL");
    const day = { ...f, allDay: true, startDate: "2026-12-20", endDate: "2026-12-24" };
    expect(formError(day)).toBeNull();
    expect(inputOf(day)).toMatchObject({ all_day: true, start: "2026-12-20", end: "2026-12-24" });
  });
});
