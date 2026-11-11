import { describe, expect, it } from "vitest";
import { createMockApi } from "../../api/mock";
import type { ScheduleCaller } from "../../api/mock/schedules";
import { actionTypesFor, agentOptions, createScheduleDialog, emptyScheduleForm, fieldOf, formFromSchedule, formPreview, GLOBAL, presetExpr, presetOf, recipientOptions, refusal, scheduleBody, targetOptions, timeZones } from "./form";
import { ApiError } from "../../api/http";
import type { Schedule } from "../../api/types";

const mock = (scheduleCaller?: ScheduleCaller) => createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, scheduleCaller });

describe("the schedule form defaults", () => {
  it("defaults the scope to the selected project, never to global", () => {
    expect(emptyScheduleForm("terrarium").scope).toBe("terrarium");
    expect(emptyScheduleForm(null).scope).toBe("");
    expect(scheduleBody(emptyScheduleForm(null, { name: "x" }))).toEqual({ error: "Choose a scope: a project, or global.", field: "scope" });
  });

  it("starts on a cron of every 15 min in UTC, enabled, catch-up skip", () => {
    const f = emptyScheduleForm("p");
    expect(f).toMatchObject({ specKind: "cron", preset: "every15", expr: "*/15 * * * *", tz: "UTC", enabled: true, catchUp: "skip" });
  });
});

describe("cron presets", () => {
  const f = { time: "09:30", minute: 7, weekday: 3, expr: "1 2 3 4 5" };
  it("write the expression of each preset", () => {
    expect(presetExpr({ ...f, preset: "every5" })).toBe("*/5 * * * *");
    expect(presetExpr({ ...f, preset: "every15" })).toBe("*/15 * * * *");
    expect(presetExpr({ ...f, preset: "every60" })).toBe("0 * * * *");
    expect(presetExpr({ ...f, preset: "hourly" })).toBe("7 * * * *");
    expect(presetExpr({ ...f, preset: "daily" })).toBe("30 9 * * *");
    expect(presetExpr({ ...f, preset: "weekly" })).toBe("30 9 * * 3");
    expect(presetExpr({ ...f, preset: "custom" })).toBe("1 2 3 4 5");
  });

  it("read back from an expression, for Edit", () => {
    expect(presetOf("*/5 * * * *")).toEqual({ preset: "every5" });
    expect(presetOf("0 * * * *")).toEqual({ preset: "every60" });
    expect(presetOf("45 * * * *")).toEqual({ preset: "hourly", minute: 45 });
    expect(presetOf("0 9 * * *")).toEqual({ preset: "daily", time: "09:00" });
    expect(presetOf("5 17 * * 7")).toEqual({ preset: "weekly", time: "17:05", weekday: 0 });
    expect(presetOf("0 9 * * 1-5")).toEqual({ preset: "custom" });
  });

  it("a preset or its time writes the expression; typing one picks the matching preset", () => {
    const d = createScheduleDialog(mock(), { project: "terrarium" });
    d.set("preset", "daily");
    d.set("time", "07:45");
    expect(d.form().expr).toBe("45 7 * * *");
    d.set("preset", "weekly");
    d.set("weekday", 5);
    expect(d.form().expr).toBe("45 7 * * 5");
    d.set("expr", "0 9 * * 1-5");
    expect(d.form()).toMatchObject({ preset: "custom", expr: "0 9 * * 1-5" });
    d.set("expr", "*/5 * * * *");
    expect(d.form().preset).toBe("every5");
  });
});

describe("the live preview", () => {
  const now = Date.parse("2026-10-26T10:07:00Z");
  it("gives the next 3 runs in the zone", () => {
    expect(formPreview({ expr: "0 9 * * *", tz: "UTC" }, now)).toEqual({ runs: [Date.parse("2026-10-27T09:00:00Z"), Date.parse("2026-10-28T09:00:00Z"), Date.parse("2026-10-29T09:00:00Z")] });
    expect(formPreview({ expr: "0 9 * * *", tz: "Australia/Brisbane" }, now)).toEqual({ runs: [Date.parse("2026-10-26T23:00:00Z"), Date.parse("2026-10-27T23:00:00Z"), Date.parse("2026-10-28T23:00:00Z")] });
  });

  it("says why an expression or a zone is bad", () => {
    expect(formPreview({ expr: "0 9 * *", tz: "UTC" }, now)).toEqual({ error: expect.stringContaining("5 fields") });
    expect(formPreview({ expr: "0 9 * * *", tz: "Atlantis" }, now)).toEqual({ error: "`Atlantis` is not a time zone" });
  });
});

describe("scheduleBody, per action type", () => {
  const base = emptyScheduleForm("terrarium", { name: " standup " });
  it("mail: to, subject, body", () => {
    expect(scheduleBody({ ...base, mailTo: "terrarium/lead", mailSubject: " Standup ", mailBody: "Post it." })).toEqual({
      body: { name: "standup", project: "terrarium", enabled: true, catch_up: "skip", spec: { kind: "cron", expr: "*/15 * * * *", tz: "UTC" }, action: { type: "mail", to: "terrarium/lead", subject: "Standup", body: "Post it." } },
    });
    // The daemon takes an empty subject.
    expect(scheduleBody({ ...base, mailTo: "operator" })).toMatchObject({ body: { action: { type: "mail", to: "operator", subject: "" } } });
    expect(scheduleBody(base)).toEqual({ error: "Choose the recipient.", field: "mailTo" });
  });

  it("turn: agent and text, start_if_stopped only when set", () => {
    const f = { ...base, actionType: "turn" as const, agent: "terrarium/w1", text: "Go." };
    expect(scheduleBody(f)).toMatchObject({ body: { action: { type: "turn", agent: "terrarium/w1", text: "Go." } } });
    expect(scheduleBody({ ...f, startIfStopped: true })).toMatchObject({ body: { action: { start_if_stopped: true } } });
    expect(scheduleBody({ ...f, text: " " })).toEqual({ error: "Write the text of the turn.", field: "text" });
  });

  it("exec: target, the command split into an argv, cwd, and timeout", () => {
    const f = { ...base, actionType: "exec" as const, target: "svc-dev", cmd: `sh -c 'make test'`, cwd: "/srv", timeout: "600" };
    expect(scheduleBody(f)).toMatchObject({ body: { action: { type: "exec", target: "svc-dev", cmd: ["sh", "-c", "make test"], cwd: "/srv", timeout_secs: 600 } } });
    expect(scheduleBody({ ...f, cwd: "", timeout: "" })).toMatchObject({ body: { action: { type: "exec", target: "svc-dev", cmd: ["sh", "-c", "make test"] } } });
    expect(scheduleBody({ ...f, cmd: "echo 'x" })).toEqual({ error: "The command: a single quote is not closed.", field: "cmd" });
    expect(scheduleBody({ ...f, cmd: "  " })).toEqual({ error: "Write the command.", field: "cmd" });
    expect(scheduleBody({ ...f, timeout: "1.5" })).toEqual({ error: "The timeout is a whole number of seconds, 1 to 3600.", field: "timeout" });
    expect(scheduleBody({ ...f, timeout: "3601" })).toMatchObject({ field: "timeout" });
  });

  it("agent: the agent and the verb", () => {
    expect(scheduleBody({ ...base, actionType: "agent", agent: "terrarium/w1", verb: "stop" })).toMatchObject({ body: { action: { type: "agent", agent: "terrarium/w1", verb: "stop" } } });
  });

  it("probe: the subscription", () => {
    expect(scheduleBody({ ...base, actionType: "probe", subscription: "anthropic-max" })).toMatchObject({ body: { action: { type: "probe", subscription: "anthropic-max" } } });
    expect(scheduleBody({ ...base, actionType: "probe" })).toEqual({ error: "Choose the subscription.", field: "subscription" });
  });

  it("the spec kinds and the global scope", () => {
    const mail = { ...base, mailTo: "operator", mailSubject: "s" };
    expect(scheduleBody({ ...mail, specKind: "interval", every: "1h30m" })).toMatchObject({ body: { spec: { kind: "interval", every: "1h30m" } } });
    expect(scheduleBody({ ...mail, specKind: "interval", every: "often" })).toMatchObject({ field: "every" });
    expect(scheduleBody({ ...mail, specKind: "at", at: "2026-11-01T09:00" })).toMatchObject({ body: { spec: { kind: "at", at: new Date("2026-11-01T09:00").toISOString() } } });
    expect(scheduleBody({ ...mail, specKind: "at", at: "" })).toMatchObject({ field: "at" });
    expect(scheduleBody({ ...mail, tz: " " })).toMatchObject({ body: { spec: { tz: "UTC" } } });
    expect(scheduleBody({ ...mail, scope: GLOBAL })).toMatchObject({ body: { project: null } });
    expect(scheduleBody({ ...mail, expr: "" })).toEqual({ error: "Give a cron expression.", field: "expr" });
  });
});

describe("formFromSchedule (Edit)", () => {
  it("fills the form from each spec and action, and the body comes back the same", async () => {
    const api = mock();
    for (const s of await api.schedules.list()) {
      const f = formFromSchedule(s);
      expect(f.scope).toBe(s.project ?? GLOBAL);
      const b = scheduleBody(f);
      expect(b).toMatchObject({ body: { name: s.name, project: s.project, spec: s.spec, catch_up: s.catch_up, enabled: s.enabled } });
      if ("body" in b) expect(b.body.action).toEqual(s.action.type === "turn" && !s.action.start_if_stopped ? { type: "turn", agent: s.action.agent, text: s.action.text } : s.action);
    }
  });
});

describe("the pickers", () => {
  const agents = [
    { project: "terrarium", name: "w1" },
    { project: "terrarium", name: "lead" },
    { project: "webshop", name: "shop-lead" },
  ];
  it("agents: the project's, or all for global", () => {
    expect(agentOptions(agents, "terrarium")).toEqual([
      { value: "terrarium/lead", label: "lead" },
      { value: "terrarium/w1", label: "w1" },
    ]);
    expect(agentOptions(agents, GLOBAL).map((o) => o.value)).toEqual(["terrarium/lead", "terrarium/w1", "webshop/shop-lead"]);
  });

  it("recipients: the operator, the inboxes, and the agents, with the scope's project first", () => {
    const g = recipientOptions([{ name: "terrarium" }, { name: "webshop" }], agents, "webshop");
    expect(g.map((x) => x.label)).toEqual(["Operator", "Project inboxes", "Agents"]);
    expect(g[0]!.options).toEqual([{ value: "operator", label: "operator" }]);
    expect(g[1]!.options.map((o) => o.value)).toEqual(["project:webshop", "project:terrarium"]);
    expect(g[2]!.options[0]!.value).toBe("webshop/shop-lead");
  });

  it("targets: the global ones and the project's", () => {
    const t = [{ name: "build-box" }, { name: "pg-test", project: "webshop" }, { name: "lab-win11", project: "terrarium" }];
    expect(targetOptions(t, "terrarium").map((o) => o.value)).toEqual(["build-box", "lab-win11"]);
    expect(targetOptions(t, GLOBAL)).toEqual([{ value: "build-box", label: "build-box (global)" }]);
  });

  it("time zones: UTC first, then the browser's", () => {
    const z = timeZones("Australia/Brisbane");
    expect(z.slice(0, 2)).toEqual(["UTC", "Australia/Brisbane"]);
    expect(new Set(z).size).toBe(z.length);
  });
});

describe("createScheduleDialog against the mock", () => {
  const fill = (d: ReturnType<typeof createScheduleDialog>) => {
    d.set("name", "nightly");
    d.set("mailTo", "terrarium/lead");
    d.set("mailSubject", "Report");
  };

  it("creates a schedule of the selected project", async () => {
    const api = mock();
    const d = createScheduleDialog(api, { project: "terrarium" });
    fill(d);
    const s = await d.submit();
    expect(s).toMatchObject({ name: "nightly", project: "terrarium", action: { type: "mail" } });
    expect(d.outcome().kind).toBe("done");
  });

  it("creates each action type", async () => {
    const api = mock();
    const types = [
      { actionType: "mail", mailTo: "project:terrarium", mailSubject: "Weekly" },
      { actionType: "turn", agent: "terrarium/lead", text: "Review the board." },
      { actionType: "exec", target: "build-box", cmd: "make test" },
      { actionType: "agent", agent: "terrarium/runner-dev", verb: "restart" },
      { actionType: "probe", subscription: "chatgpt-pro" },
    ] as const;
    for (const [i, init] of types.entries()) {
      const d = createScheduleDialog(api, { project: "terrarium", init: { name: `a${i}`, ...init } });
      const s = await d.submit();
      expect(s?.action.type).toBe(init.actionType);
    }
  });

  it("shows the daemon's 422 under the field it names: a cron the client preview refused is still sent", async () => {
    const api = mock();
    const d = createScheduleDialog(api, { project: "terrarium" });
    fill(d);
    d.set("expr", "0 25 * * *");
    expect(d.preview(Date.now())).toHaveProperty("error");
    expect(await d.submit()).toBeNull();
    expect(d.outcome()).toMatchObject({ kind: "rejected", field: "expr", daemonField: "spec.expr" });
    expect(d.fieldError("expr")).toContain("`0 25 * * *`");
    expect(d.fieldError("name")).toBeNull();
    d.set("expr", "0 23 * * *");
    expect(d.outcome().kind).toBe("idle");
    expect(await d.submit()).not.toBeNull();
  });

  it("keeps the form's own check on the field, without a request", async () => {
    const d = createScheduleDialog(mock(), { project: "terrarium" });
    expect(await d.submit()).toBeNull();
    expect(d.outcome()).toEqual({ kind: "invalid", message: "Give the schedule a name.", field: "name" });
    expect(d.fieldError("name")).toBe("Give the schedule a name.");
  });

  it("answers 403 for a global schedule from a project_admin, and for a member", async () => {
    const d = createScheduleDialog(mock({ role: "project_admin", project: "terrarium" }), { project: "terrarium" });
    fill(d);
    d.set("scope", GLOBAL);
    expect(await d.submit()).toBeNull();
    expect(d.outcome()).toMatchObject({ kind: "forbidden", scope: GLOBAL });
    const m = createScheduleDialog(mock({ role: "member", project: "terrarium" }), { project: "terrarium" });
    fill(m);
    await m.submit();
    expect(m.outcome().kind).toBe("forbidden");
  });

  it("refuses an edit of a file schedule with 409 file_managed, as a banner", async () => {
    const api = mock();
    const s = (await api.schedules.list()).find((x) => x.source === "file")!;
    const d = createScheduleDialog(api, { schedule: s });
    d.set("mailSubject", "New");
    expect(await d.submit()).toBeNull();
    expect(d.outcome()).toMatchObject({ kind: "error", error: { status: 409, code: "file_managed" } });
  });

  it("edits with PATCH: the same ID, the new spec", async () => {
    const api = mock();
    const s = (await api.schedules.list()).find((x) => x.name === "tests")!;
    const d = createScheduleDialog(api, { schedule: s });
    expect(d.form()).toMatchObject({ specKind: "interval", every: "15m", actionType: "exec", cmd: "make test", target: "build-box" });
    d.set("every", "30m");
    const out = (await d.submit()) as Schedule;
    expect(out.id).toBe(s.id);
    expect(out.spec).toEqual({ kind: "interval", every: "30m" });
    expect((await api.schedules.list()).filter((x) => x.name === "tests")).toHaveLength(1);
  });

  it("sorts other errors into a banner", () => {
    expect(refusal(new ApiError(500, "boom"), "p")).toMatchObject({ kind: "error" });
    expect(refusal(new ApiError(422, "bad", undefined, "invalid", { field: "spec.weird" }), "p")).toEqual({ kind: "rejected", message: "bad", field: undefined, daemonField: "spec.weird" });
  });
});

describe("the card action", () => {
  const base = emptyScheduleForm("terrarium", { name: "review", actionType: "card" });
  it("is a choice of a time schedule too", () => {
    expect(actionTypesFor("cron").map((a) => a.id)).toEqual(["mail", "turn", "exec", "agent", "probe", "card", "webhook"]);
  });

  it("maps op and key; title, body, and column only when given", () => {
    expect(scheduleBody({ ...base, cardKey: " weekly " })).toMatchObject({ body: { action: { type: "card", op: "open", key: "weekly" } } });
    const b = scheduleBody({ ...base, cardKey: "weekly" });
    expect("body" in b && Object.keys(b.body.action).sort()).toEqual(["key", "op", "type"]);
    expect(scheduleBody({ ...base, cardOp: "close", cardKey: "k", cardBody: "Green again.", cardColumn: "review" })).toMatchObject({ body: { action: { type: "card", op: "close", key: "k", body: "Green again.", column: "review" } } });
    expect(scheduleBody({ ...base, cardKey: "k", cardTitle: " Weekly review " })).toMatchObject({ body: { action: { title: "Weekly review" } } });
  });

  it("needs a key, a project, and does not open in done", () => {
    expect(scheduleBody(base)).toMatchObject({ field: "cardKey" });
    expect(scheduleBody({ ...base, cardKey: "k", scope: GLOBAL })).toMatchObject({ field: "actionType", error: expect.stringContaining("global") });
    expect(scheduleBody({ ...base, cardKey: "k", cardColumn: "done" })).toEqual({ error: "A card does not open in done.", field: "cardColumn" });
    expect(scheduleBody({ ...base, cardKey: "k", cardOp: "close", cardColumn: "done" })).toHaveProperty("body");
  });

  it("shows the daemon's action.body under the body of the action type", () => {
    expect(fieldOf("action.body", "card")).toBe("cardBody");
    expect(fieldOf("action.body", "mail")).toBe("mailBody");
    expect(fieldOf("action.key")).toBe("cardKey");
  });
});

describe("a trigger in the schedule form", () => {
  const trig = emptyScheduleForm("terrarium", { name: "ci-failed", specKind: "on", kinds: "ci.run.failed", mailTo: "terrarium/lead", mailSubject: "CI failed: {{event.data.repo}}", mailBody: "Fix it." });

  it("offers only mail, exec, card, and webhook", () => {
    expect(actionTypesFor("on").map((a) => a.id)).toEqual(["mail", "exec", "card", "webhook"]);
  });

  it("refuses turn, agent, and probe on the action field, before a request", () => {
    for (const actionType of ["turn", "agent", "probe"] as const)
      expect(scheduleBody({ ...trig, actionType, agent: "terrarium/lead", text: "x", subscription: "s" })).toEqual({ error: "A trigger runs only mail, exec, card, or webhook. To notify an agent, mail it.", field: "actionType" });
  });

  it("sends the spec on with the action", () => {
    expect(scheduleBody(trig)).toEqual({
      body: { name: "ci-failed", project: "terrarium", enabled: true, catch_up: "skip", spec: { kind: "on", kinds: ["ci.run.failed"] }, action: { type: "mail", to: "terrarium/lead", subject: "CI failed: {{event.data.repo}}", body: "Fix it." } },
    });
    expect(scheduleBody({ ...trig, actionType: "exec", target: "build-box", cmd: "notify {{event.data.repo}}" })).toMatchObject({ body: { action: { type: "exec", cmd: ["notify", "{{event.data.repo}}"] } } });
  });

  it("names the form field of each trigger field of the daemon", () => {
    expect(fieldOf("spec.kinds")).toBe("kinds");
    expect(fieldOf("spec.match")).toBe("rules");
    expect(fieldOf("spec.match.1.value")).toBe("rules");
    expect(fieldOf("spec.dedupe.window")).toBe("dedupeWindow");
    expect(fieldOf("spec.escalate.reset_on")).toBe("escalateResetOn");
    expect(fieldOf("spec.escalate.to")).toBe("escalateTo");
    expect(fieldOf("spec.max_per_hour")).toBe("maxPerHour");
  });

  it("creates with the preset against the mock, and edits the rules", async () => {
    const api = mock();
    const d = createScheduleDialog(api, { project: "terrarium", init: { specKind: "on" } });
    expect(d.isTrigger()).toBe(true);
    d.set("name", "ci-main");
    d.ciPreset("terrarium/lead");
    d.addRule();
    d.setRule(1, { path: "data.repo", op: "in", value: "wil/terrarium, wil/site" });
    d.setRule(0, { op: "glob", value: "release/*" });
    const s = await d.submit();
    expect(s?.spec).toMatchObject({
      kind: "on",
      kinds: ["ci.run.failed"],
      match: [
        { path: "data.branch", op: "glob", value: "release/*" },
        { path: "data.repo", op: "in", value: ["wil/terrarium", "wil/site"] },
      ],
      dedupe: { key: "{{event.data.repo}}", window: "10m" },
      escalate: { after: 3, reset_on: ["ci.run.succeeded"] },
    });
    expect(s?.action).toMatchObject({ type: "mail", to: "terrarium/lead" });
    d.removeRule(0);
    expect(d.form().rules).toEqual([{ path: "data.repo", op: "in", value: "wil/terrarium, wil/site" }]);
  });

  it("shows a 422 of the daemon under the trigger field, and a bad placeholder under the subject", async () => {
    const api = mock();
    const d = createScheduleDialog(api, { project: "terrarium", init: { name: "t", specKind: "on", kinds: "ci.run.failed", mailTo: "operator", mailSubject: "{{event.nope}}" } });
    expect(await d.submit()).toBeNull();
    expect(d.outcome()).toMatchObject({ kind: "rejected", field: "mailSubject", daemonField: "action.subject" });
    d.set("mailSubject", "ok");
    d.set("escalate", true);
    d.set("escalateTo", "nobody");
    expect(await d.submit()).toBeNull();
    expect(d.fieldError("escalateTo")).toContain("nobody");
  });

  it("refuses a trigger card on a global scope at the daemon too", async () => {
    const e = refusal(new ApiError(422, "action: a card action needs a project schedule", undefined, "invalid", { field: "action" }), GLOBAL, "card");
    expect(e).toMatchObject({ kind: "rejected", field: "actionType" });
  });
});

describe("a day_start schedule (TER-458)", () => {
  const base = emptyScheduleForm("terrarium", { name: "morning", specKind: "day_start", mailTo: "terrarium/lead", mailSubject: "Morning" });
  it("sends only the fields that are set, Monday first", () => {
    expect(scheduleBody(base)).toMatchObject({ body: { spec: { kind: "day_start", tz: "UTC" } } });
    const b = scheduleBody({ ...base, tz: "Australia/Brisbane", notBefore: "07:30", notAfter: "18:00", delay: "5m", weekdays: ["fri", "mon"] });
    expect("body" in b && b.body.spec).toEqual({ kind: "day_start", tz: "Australia/Brisbane", not_before: "07:30", not_after: "18:00", delay: "5m", weekdays: ["mon", "fri"] });
    const all = scheduleBody({ ...base, weekdays: ["mon", "tue", "wed", "thu", "fri", "sat", "sun"] });
    expect("body" in all && all.body.spec).toEqual({ kind: "day_start", tz: "UTC" });
  });
  it("checks the window and the delay as the daemon does", () => {
    expect(scheduleBody({ ...base, notBefore: "7:30" })).toMatchObject({ field: "notBefore" });
    expect(scheduleBody({ ...base, notBefore: "09:00", notAfter: "08:00" })).toMatchObject({ field: "notAfter" });
    expect(scheduleBody({ ...base, delay: "soon" })).toMatchObject({ field: "delay" });
    expect(scheduleBody({ ...base, delay: "13h" })).toMatchObject({ field: "delay", error: "The delay is at most 12h." });
    expect(scheduleBody({ ...base, delay: "12h" })).toHaveProperty("body");
  });
  it("reads a day_start schedule back for Edit", () => {
    const f = formFromSchedule({
      id: "s", name: "morning", project: "terrarium", enabled: true, spec: { kind: "day_start", tz: "UTC", not_before: "07:30", delay: "5m", weekdays: ["Monday", "fri"] },
      action: { type: "mail", to: "terrarium/lead", subject: "Morning", body: "" }, catch_up: "skip", created_by: "operator", run_as: "operator",
      next_run_at: null, last_run: null, runs_count: 0, source: "api", created_at: "t", updated_at: "t",
    } as Schedule);
    expect(f).toMatchObject({ specKind: "day_start", tz: "UTC", notBefore: "07:30", notAfter: "", delay: "5m", weekdays: ["mon", "fri"] });
  });
  it("creates it on the mock, and the mock refuses what the daemon refuses", async () => {
    const api = mock();
    const d = createScheduleDialog(api, { project: "terrarium", init: { ...base, notBefore: "07:30", weekdays: ["mon"] } });
    const s = await d.submit();
    expect(s).toMatchObject({ spec: { kind: "day_start", tz: "UTC", not_before: "07:30", weekdays: ["mon"] }, next_run_at: null });
    await expect(api.schedules.create({ name: "bad", project: "terrarium", spec: { kind: "day_start", weekdays: ["someday"] }, action: { type: "mail", to: "operator", subject: "x", body: "" } })).rejects.toMatchObject({ status: 422 });
    await expect(api.schedules.create({ name: "bad2", project: "terrarium", spec: { kind: "day_start", delay: "13h" }, action: { type: "mail", to: "operator", subject: "x", body: "" } })).rejects.toMatchObject({ status: 422 });
  });
});
