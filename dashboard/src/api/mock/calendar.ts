// The calendar of the mock (docs/api.md §33), after the daemon's
// `store/calendar.rs` and `api/calendar.rs`: the project and global
// calendars, the combined view with expanded occurrences, the checks that
// name the field, skips, assignees, watchers with their mail (not to the
// actor), the grants, the audit rows, and the `calendar.*` events. The mock
// has no notice tick. The seed gives project `terrarium` a weekly
// maintenance window, a change freeze, and a deploy, and the global
// calendar a host maintenance window.

import type { Agent, AgentEvent, CalendarAssignee, CalendarEvent, CalendarInput, CalendarKind, CalendarOccurrence, CalendarRange, CalendarRule, CalendarView, CalendarWatcher } from "../types";
import { checkRule, describeRule, expand, hasOccurrence, localInstant, parseDate, parseRrule, toRrule, validZone, zoned } from "../../features/calendar/recur";
import type { ScheduleCaller } from "./schedules";

type Fail = (status: number, code: string, message: string, details?: unknown) => never;

export interface CalendarMockContext {
  reply: <T>(fn: () => T) => Promise<T>;
  fail: Fail;
  emit: (d: { kind: AgentEvent["kind"]; actor: string; project?: string; summary: string; data: Record<string, unknown> }) => unknown;
  audit: (action: string, target: string, project: string | undefined, detail?: Record<string, unknown>) => unknown;
  /** Throws `404` for an unknown project. */
  project: (name: string) => unknown;
  agents: () => Agent[];
  /** Mail from `system`. */
  mail: (to: string, subject: string, body: string) => unknown;
  caller?: ScheduleCaller;
  ulid: (tag: string) => string;
  now: () => number;
}

const KINDS: CalendarKind[] = ["window", "maintenance", "freeze", "deploy", "task", "other"];
const MAX_RANGE_DAYS = 400;
const DAY = 86_400_000;
const iso = (ms: number) => new Date(ms).toISOString();

interface Row {
  e: CalendarEvent;
}

interface Watch {
  watcher: string;
  project: string | null;
  created_by: string;
  created_at: string;
}

export function createCalendarMock(ctx: CalendarMockContext) {
  const rows: Row[] = [];
  const watches: Watch[] = [];
  const grants = new Set<string>();
  const field = (f: string, message: string): never => ctx.fail(422, "invalid", `${f}: ${message}`, { field: f });
  const actor = () => "operator";

  const canRead = (project: string | null) => {
    const c = ctx.caller;
    if (!project || !c || c.role === "operator" || c.role === "sysadmin" || c.project === project) return;
    ctx.fail(403, "forbidden", `project \`${project}\` is not your project; reading it needs sysadmin`);
  };
  let seeding = false;
  const mayWrite = (project: string | null) => {
    const c = ctx.caller;
    if (seeding || !c || c.role === "operator" || c.role === "sysadmin") return true;
    return project !== null && c.role === "project_admin" && c.project === project;
  };
  const canWrite = (project: string | null) => {
    if (mayWrite(project)) return;
    if (project === null) ctx.fail(403, "forbidden", "writing the global calendar needs sysadmin or the operator");
    ctx.fail(403, "forbidden", `writing the calendar of \`${project}\` needs the calendar grant from a lead (project_admin), or project_admin; ask your lead to add the event for you`);
  };
  const find = (id: string): Row => rows.find((r) => r.e.id === id) ?? ctx.fail(404, "not_found", `no calendar event \`${id}\``);

  const agentOf = (project: string | null, key: string): CalendarAssignee => {
    const all = ctx.agents();
    const [p, n] = key.includes("/") ? key.split("/", 2) : [project, key];
    const a = all.find((x) => x.id === key) ?? all.find((x) => x.name === n && (p ? x.project === p : true)) ?? field("assignees", `no agent \`${key}\``);
    if (project && a.project !== project) field("assignees", `agent \`${a.project}/${a.name}\` is not in project \`${project}\`; an event of a project takes its own agents`);
    return { id: a.id, name: a.name, project: a.project };
  };

  /** A time value: an instant, a local date-time in `tz`, or a date. */
  const timeOf = (f: string, v: string, tz: string): { ms: number; date?: string } => {
    const d = parseDate(v);
    if (d) return { ms: localInstant({ ...d, h: 0, mi: 0, s: 0 }, tz), date: v.trim() };
    const local = /^(\d{4})-(\d{2})-(\d{2})[T ](\d{2}):(\d{2})(?::(\d{2}))?$/.exec(v.trim());
    if (local) return { ms: localInstant({ y: +local[1]!, m: +local[2]!, d: +local[3]!, h: +local[4]!, mi: +local[5]!, s: +(local[6] ?? 0) }, tz) };
    const t = Date.parse(v);
    if (/[zZ]|[+-]\d{2}:\d{2}$/.test(v) && !Number.isNaN(t)) return { ms: t };
    return field(f, `bad time \`${v}\`: use RFC 3339, a local time in \`tz\`, or a date for an all-day event`);
  };

  const ruleOf = (v: CalendarInput["recurrence"]): CalendarRule | null => {
    if (v === undefined || v === null || v === "") return null;
    if (typeof v === "string") {
      const r = parseRrule(v);
      return typeof r === "string" ? field("recurrence", r) : r;
    }
    const e = checkRule(v);
    if (e) field("recurrence", e);
    return { ...v, ...(v.weekdays ? { weekdays: [...new Set(v.weekdays)] } : {}) };
  };

  /** Check an input and build the event fields, as the daemon's `check_input`. */
  const checked = (scope: string | null, i: CalendarInput) => {
    const title = (i.title ?? "").trim();
    if (!title) field("title", "give a title");
    if (title.length > 200) field("title", "at most 200 characters");
    const kind = i.kind ?? "other";
    if (!KINDS.includes(kind)) field("kind", `unknown kind \`${kind}\`: use ${KINDS.join(", ")}`);
    const tz = i.tz ?? "UTC";
    if (!validZone(tz)) field("tz", `unknown time zone \`${tz}\``);
    if (!i.start) field("start", "give a start");
    const s = timeOf("start", i.start!, tz);
    const allDay = i.all_day ?? s.date !== undefined;
    let start: number, end: number;
    if (allDay) {
      const first = s.date ?? isoDate(s.ms, tz);
      const last = i.end ? (parseDate(i.end) ? i.end : isoDate(timeOf("end", i.end, tz).ms - 1, tz)) : first;
      if (last < first) field("end", "the last day is before the first day");
      start = localInstant({ ...parseDate(first)!, h: 0, mi: 0, s: 0 }, tz);
      const l = parseDate(last)!;
      end = localInstant({ ...parseDate(isoDate(Date.UTC(l.y, l.m - 1, l.d) + DAY, "UTC"))!, h: 0, mi: 0, s: 0 }, tz);
    } else {
      if (s.date) field("start", "a date alone needs `all_day: true`; give a time too");
      start = s.ms;
      end = i.end ? timeOf("end", i.end, tz).ms : start + 3_600_000;
      if (end < start) field("end", "the end is before the start");
    }
    if (end - start > 366 * DAY) field("end", "an occurrence lasts at most 366 days");
    const recurrence = ruleOf(i.recurrence);
    const links = i.links ?? {};
    if (links.url && !/^https?:\/\//.test(links.url)) field("links.url", "use an http:// or https:// URL");
    const assignees = i.assignees ? [...new Map(i.assignees.map((k) => agentOf(scope, k)).map((a) => [a.id, a])).values()] : undefined;
    return { title, kind, description: i.description ?? "", tz, all_day: allDay, start: iso(start), end: iso(end), recurrence, links, assignees };
  };

  const isoDate = (ms: number, tz: string) => {
    const p = zoned(ms, tz);
    return `${String(p.y).padStart(4, "0")}-${String(p.m).padStart(2, "0")}-${String(p.d).padStart(2, "0")}`;
  };

  const finish = (e: CalendarEvent): CalendarEvent => {
    e.global = e.project === null;
    e.rrule = e.recurrence ? toRrule(e.recurrence) : null;
    e.repeat = e.recurrence ? describeRule(e.recurrence) : null;
    if (e.all_day) {
      e.start_date = isoDate(Date.parse(e.start), e.tz);
      e.end_date = isoDate(Date.parse(e.end) - 1, e.tz);
    } else {
      delete e.start_date;
      delete e.end_date;
    }
    return e;
  };

  const shown = (e: CalendarEvent): CalendarEvent => ({ ...e, can_edit: mayWrite(e.project) });

  const series = (e: CalendarEvent, skipped = true) => ({
    start: Date.parse(e.start),
    end: Date.parse(e.end),
    allDay: e.all_day,
    tz: e.tz,
    rule: e.recurrence,
    skipped: new Set(skipped ? e.exceptions.map((x) => Date.parse(x)) : []),
  });

  const summaryData = (e: CalendarEvent) => ({ project: e.project, global: e.global, event_id: e.id, title: e.title, kind: e.kind, start: e.start, end: e.end, all_day: e.all_day, rrule: e.rrule, assignees: e.assignees.map((a) => `${a.project}/${a.name}`) });
  const what = (e: CalendarEvent) => `${e.kind} "${e.title}"`;
  const label = (project: string | null) => project ?? "global";

  const notify = (e: CalendarEvent, verb: string, body: string) => {
    for (const w of watches.filter((x) => x.project === e.project)) {
      if (w.watcher === actor()) continue;
      ctx.mail(w.watcher === "operator" ? "operator" : w.watcher, `Calendar ${label(e.project)}: ${verb} ${what(e)}`, body);
    }
  };

  const diff = (a: CalendarEvent, b: CalendarEvent): [string, string][] => {
    const out: [string, string][] = [];
    const put = (k: string, x: string, y: string) => x !== y && out.push([k, `${x} → ${y}`]);
    put("title", a.title, b.title);
    put("kind", a.kind, b.kind);
    put("when", `${a.start} – ${a.end}`, `${b.start} – ${b.end}`);
    put("tz", a.tz, b.tz);
    put("repeats", a.repeat ?? "no", b.repeat ?? "no");
    put("links", JSON.stringify(a.links), JSON.stringify(b.links));
    put("assignees", a.assignees.map((x) => x.name).join(", ") || "none", b.assignees.map((x) => x.name).join(", ") || "none");
    put("skipped", a.exceptions.join(", ") || "none", b.exceptions.join(", ") || "none");
    if (a.description !== b.description) out.push(["description", "changed"]);
    return out;
  };

  const updated = (before: CalendarEvent, after: CalendarEvent, action: string) => {
    const changes = diff(before, after);
    ctx.audit(action, after.id, after.project ?? undefined, { id: after.id, changed: changes.map(([k]) => k) });
    if (!changes.length) return;
    ctx.emit({ kind: "calendar.event_updated", actor: actor(), ...(after.project ? { project: after.project } : {}), summary: `calendar ${label(after.project)}: updated ${what(after)} (${changes.map(([k]) => k).join(", ")})`, data: { ...summaryData(after), changed: changes.map(([k]) => k), diff: Object.fromEntries(changes) } });
    notify(after, "updated", `operator changed ${what(after)}.\n\n${changes.map(([k, v]) => `- ${k}: ${v}`).join("\n")}`);
  };

  const create = (scope: string | null, body: CalendarInput): CalendarEvent => {
    if (scope !== null) ctx.project(scope);
    canWrite(scope);
    const c = checked(scope, body);
    const at = iso(ctx.now());
    const e = finish({
      id: ctx.ulid("cal"),
      project: scope,
      global: scope === null,
      title: c.title,
      description: c.description,
      kind: c.kind,
      start: c.start,
      end: c.end,
      all_day: c.all_day,
      tz: c.tz,
      recurrence: c.recurrence,
      rrule: null,
      repeat: null,
      links: c.links,
      assignees: c.assignees ?? [],
      exceptions: [],
      created_by: actor(),
      created_by_name: actor(),
      updated_by: actor(),
      created_at: at,
      updated_at: at,
    });
    rows.push({ e });
    ctx.audit("calendar.create", e.id, e.project ?? undefined, { id: e.id, title: e.title, kind: e.kind });
    ctx.emit({ kind: "calendar.event_created", actor: actor(), ...(e.project ? { project: e.project } : {}), summary: `calendar ${label(e.project)}: added ${what(e)}`, data: summaryData(e) });
    notify(e, "added", `operator added ${what(e)}.`);
    return e;
  };

  const view = (filter: (e: CalendarEvent) => boolean, r: CalendarRange | undefined, writeScope: string | null | undefined): CalendarView => {
    const range = typeof r === "object" && r ? r : {};
    const dayOf = (v: string, name: string) => {
      const d = parseDate(v);
      if (d) return Date.UTC(d.y, d.m - 1, d.d);
      const t = Date.parse(v);
      return Number.isNaN(t) ? ctx.fail(400, "bad_request", `bad \`${name}\` \`${v}\`: use an RFC 3339 time or a date (2026-11-10)`) : t;
    };
    const n = new Date(ctx.now());
    const from = range.from ? dayOf(range.from, "from") : Date.UTC(n.getUTCFullYear(), n.getUTCMonth(), n.getUTCDate());
    const to = range.to ? dayOf(range.to, "to") : from + (range.days ?? 31) * DAY;
    if (to <= from) ctx.fail(400, "bad_request", "`to` is not after `from`");
    if (to - from > MAX_RANGE_DAYS * DAY) ctx.fail(400, "bad_request", `the window is longer than ${MAX_RANGE_DAYS} days`);
    const events = rows.map((x) => x.e).filter(filter);
    const occurrences: CalendarOccurrence[] = [];
    for (const e of events)
      for (const o of expand(series(e), from, to))
        occurrences.push({ event_id: e.id, start: iso(o.start), end: iso(o.end), all_day: e.all_day, title: e.title, kind: e.kind, project: e.project, global: e.global, recurring: !!e.recurrence, assignees: e.assignees.map((a) => `${a.project}/${a.name}`) });
    occurrences.sort((a, b) => a.start.localeCompare(b.start) || a.event_id.localeCompare(b.event_id));
    const ids = new Set(occurrences.map((o) => o.event_id));
    return {
      from: iso(from),
      to: iso(to),
      occurrences: occurrences.slice(0, 5000),
      events: events.filter((e) => ids.has(e.id)).map(shown),
      truncated: occurrences.length > 5000,
      can_write: writeScope !== undefined && mayWrite(writeScope),
    };
  };

  const merged = (e: CalendarEvent, patch: CalendarInput): CalendarInput => {
    const base: CalendarInput = {
      title: e.title,
      description: e.description,
      kind: e.kind,
      start: e.all_day ? e.start_date : e.start,
      end: e.all_day ? e.end_date : e.end,
      all_day: e.all_day,
      tz: e.tz,
      recurrence: e.recurrence,
      links: e.links,
    };
    if (patch.start !== undefined && patch.end === undefined && !e.all_day) {
      const t = Date.parse(patch.start);
      base.end = Number.isNaN(t) ? undefined : iso(t + (Date.parse(e.end) - Date.parse(e.start)));
    }
    const out: Record<string, unknown> = { ...base };
    for (const [k, v] of Object.entries(patch)) {
      if (v === null) delete out[k];
      else out[k] = v;
    }
    return out as CalendarInput;
  };

  const occurrenceOf = (e: CalendarEvent, v: string): number => {
    const t = Date.parse(v);
    if (!parseDate(v) && !Number.isNaN(t)) {
      if (!hasOccurrence(series(e, false), t)) field("occurrence", `event \`${e.id}\` has no occurrence that starts at ${v}`);
      return t;
    }
    const d = parseDate(v) ?? field("occurrence", `bad occurrence \`${v}\`: use its start (RFC 3339) or its date (2026-11-10)`);
    const from = localInstant({ ...d, h: 0, mi: 0, s: 0 }, e.tz);
    const list = expand(series(e, false), from, from + DAY).filter((o) => o.start >= from);
    if (list.length !== 1) field("occurrence", list.length ? `event \`${e.id}\` has more than one occurrence on ${v}; give its start` : `event \`${e.id}\` has no occurrence that starts on ${v} (${e.tz})`);
    return list[0]!.start;
  };

  const change = (id: string) => {
    const r = find(id);
    canRead(r.e.project);
    canWrite(r.e.project);
    return r;
  };

  const watcherKey = (scope: string | null, who?: string) => {
    canRead(scope);
    if (!who || who === "me") return "operator";
    if (who === "operator") return who;
    return agentOf(scope, who).id;
  };

  const api = {
    project: (project: string, range?: CalendarRange) =>
      ctx.reply(() => {
        ctx.project(project);
        canRead(project);
        return view((e) => e.project === project || e.project === null, range, project);
      }),
    combined: (range?: CalendarRange) =>
      ctx.reply(() => {
        const p = typeof range === "object" ? range?.project : undefined;
        if (p) canRead(p);
        const c = ctx.caller;
        const own = c && (c.role === "member" || c.role === "project_admin") ? c.project : undefined;
        const only = p ?? own;
        return view((e) => (only ? e.project === only || e.project === null : true), range, p);
      }),
    global: (range?: CalendarRange) => ctx.reply(() => view((e) => e.project === null, range, null)),
    get: (id: string) =>
      ctx.reply(() => {
        const r = find(id);
        canRead(r.e.project);
        return shown(r.e);
      }),
    create: (scope: string | null, body: CalendarInput) => ctx.reply(() => shown(create(scope, body))),
    update: (id: string, patch: CalendarInput) =>
      ctx.reply(() => {
        const r = change(id);
        const before = structuredClone(r.e);
        const c = checked(r.e.project, merged(r.e, patch));
        Object.assign(r.e, { title: c.title, description: c.description, kind: c.kind, start: c.start, end: c.end, all_day: c.all_day, tz: c.tz, recurrence: c.recurrence, links: c.links, updated_by: actor(), updated_at: iso(ctx.now()) });
        if (c.assignees) r.e.assignees = c.assignees;
        finish(r.e);
        r.e.exceptions = r.e.exceptions.filter((x) => hasOccurrence(series(r.e, false), Date.parse(x)));
        updated(before, r.e, "calendar.update");
        return shown(r.e);
      }),
    remove: (id: string) =>
      ctx.reply(() => {
        const r = change(id);
        rows.splice(rows.indexOf(r), 1);
        ctx.audit("calendar.delete", r.e.id, r.e.project ?? undefined, { id: r.e.id, title: r.e.title });
        ctx.emit({ kind: "calendar.event_deleted", actor: actor(), ...(r.e.project ? { project: r.e.project } : {}), summary: `calendar ${label(r.e.project)}: removed ${what(r.e)}`, data: summaryData(r.e) });
        notify(r.e, "removed", `operator removed ${what(r.e)}.`);
      }),
    skip: (id: string, occurrence: string) =>
      ctx.reply(() => {
        const r = change(id);
        if (!r.e.recurrence) field("occurrence", "the event does not repeat; delete it instead");
        const at = iso(occurrenceOf(r.e, occurrence));
        const before = structuredClone(r.e);
        if (!r.e.exceptions.includes(at)) r.e.exceptions = [...r.e.exceptions, at].sort();
        updated(before, r.e, "calendar.skip");
        return shown(r.e);
      }),
    unskip: (id: string, occurrence: string) =>
      ctx.reply(() => {
        const r = change(id);
        if (!r.e.recurrence) field("occurrence", "the event does not repeat; delete it instead");
        const at = iso(occurrenceOf(r.e, occurrence));
        const before = structuredClone(r.e);
        r.e.exceptions = r.e.exceptions.filter((x) => x !== at);
        updated(before, r.e, "calendar.unskip");
        return shown(r.e);
      }),
    assign: (id: string, agent: string) =>
      ctx.reply(() => {
        const r = change(id);
        const a = agentOf(r.e.project, agent);
        const before = structuredClone(r.e);
        if (!r.e.assignees.some((x) => x.id === a.id)) r.e.assignees = [...r.e.assignees, a];
        updated(before, r.e, "calendar.assign");
        return shown(r.e);
      }),
    unassign: (id: string, agent: string) =>
      ctx.reply(() => {
        const r = change(id);
        const a = agentOf(r.e.project, agent);
        const before = structuredClone(r.e);
        r.e.assignees = r.e.assignees.filter((x) => x.id !== a.id);
        updated(before, r.e, "calendar.unassign");
        return shown(r.e);
      }),
    watchers: (scope: string | null) =>
      ctx.reply((): CalendarWatcher[] => {
        if (scope !== null) ctx.project(scope);
        canRead(scope);
        return watches
          .filter((w) => w.project === scope)
          .map((w) => {
            const a = ctx.agents().find((x) => x.id === w.watcher);
            return { ...w, name: a ? `${a.project}/${a.name}` : w.watcher };
          });
      }),
    watch: (scope: string | null, watcher?: string) =>
      ctx.reply(() => {
        if (scope !== null) ctx.project(scope);
        const key = watcherKey(scope, watcher);
        const has = watches.some((w) => w.watcher === key && w.project === scope);
        if (!has) {
          watches.push({ watcher: key, project: scope, created_by: actor(), created_at: iso(ctx.now()) });
          ctx.audit("calendar.watch", label(scope), scope ?? undefined, { watcher: key });
        }
        return { calendar: label(scope), watcher: key, watching: true, changed: !has };
      }),
    unwatch: (scope: string | null, watcher?: string) =>
      ctx.reply(() => {
        const key = watcherKey(scope, watcher);
        const i = watches.findIndex((w) => w.watcher === key && w.project === scope);
        if (i >= 0) {
          watches.splice(i, 1);
          ctx.audit("calendar.unwatch", label(scope), scope ?? undefined, { watcher: key });
        }
        return { calendar: label(scope), watcher: key, watching: false, changed: i >= 0 };
      }),
    grants: (project: string) =>
      ctx.reply(() => {
        ctx.project(project);
        canRead(project);
        return ctx
          .agents()
          .filter((a) => a.project === project && (grants.has(a.id) || a.policy?.calendar_write))
          .map((a) => ({ id: a.id, name: a.name, project: a.project }));
      }),
    grant: (project: string, agent: string) => ctx.reply(() => setGrant(project, agent, true)),
    revoke: (project: string, agent: string) => ctx.reply(() => setGrant(project, agent, false)),
  };

  const setGrant = (project: string, agent: string, on: boolean) => {
    ctx.project(project);
    if (!mayWrite(project) || project === null) ctx.fail(403, "forbidden", `granting the calendar write right needs project_admin of \`${project}\`, sysadmin, or the operator`);
    const a = agentOf(project, agent);
    const had = grants.has(a.id);
    if (on) grants.add(a.id);
    else grants.delete(a.id);
    if (had !== on) ctx.audit(on ? "calendar.grant" : "calendar.revoke", a.id, project, { agent: a.id, calendar_write: on });
    return { agent: a.id, name: a.name, project, calendar_write: on, changed: had !== on };
  };

  /** The demo calendar. */
  const seed = () => {
    seeding = true;
    const today = new Date(ctx.now());
    const day = (n: number) => {
      const d = new Date(Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate() + n));
      return d.toISOString().slice(0, 10);
    };
    const dow = (today.getUTCDay() + 6) % 7;
    create("terrarium", {
      title: "Weekly patch window",
      kind: "maintenance",
      description: "OS and harness updates on the build hosts. Agents hold deploys while it runs.",
      start: `${day((1 - dow + 7) % 7)}T22:00`,
      end: `${day((1 - dow + 7) % 7)}T23:30`,
      tz: "Australia/Brisbane",
      recurrence: { freq: "weekly", weekdays: ["tu"] },
      assignees: ["lead", "runner-dev"],
      links: { target: "pi-runner" },
    });
    create("terrarium", {
      title: "Release freeze",
      kind: "freeze",
      description: "No merges to `main` while v0.2.0 is in review.",
      start: day(9),
      end: day(12),
      tz: "Australia/Brisbane",
    });
    create("terrarium", {
      title: "Deploy v0.2.0 to buildhost",
      kind: "deploy",
      start: `${day(13)}T09:00`,
      end: `${day(13)}T10:00`,
      tz: "Australia/Brisbane",
      assignees: ["lead"],
      links: { card: "TER-16", repo: "terrarium" },
    });
    create("terrarium", {
      title: "Dependency review",
      kind: "task",
      start: `${day(2)}T10:00`,
      end: `${day(2)}T10:30`,
      tz: "UTC",
      recurrence: { freq: "monthly", count: 6 },
      assignees: ["reviewer"],
    });
    create(null, {
      title: "Host maintenance: buildhost",
      kind: "window",
      description: "The host reboots for a kernel update. Every agent stops for about 20 minutes.",
      start: `${day(5)}T02:00:00Z`,
      end: `${day(5)}T03:00:00Z`,
    });
    seeding = false;
  };

  return { api, seed };
}
