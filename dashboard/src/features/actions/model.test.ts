// Project actions (TER-53): the pure part of the buttons.
import { describe, expect, it } from "vitest";
import type { Schedule } from "../../api/types";
import { actionsByProject, confirmText, failureOf, historyHref, isAction, limitWords, offReason, projectActions, resultOf, runsAs } from "./model";

const sched = (over: Partial<Schedule>): Schedule =>
  ({
    id: "01SCH",
    name: "deploy",
    project: "terra",
    enabled: true,
    spec: { kind: "manual" },
    action: { type: "mail", to: "terra/lead", subject: "Go", body: "" },
    catch_up: "skip",
    created_by: "operator",
    run_as: "operator",
    next_run_at: null,
    last_run: null,
    runs_count: 0,
    source: "api",
    created_at: "",
    updated_at: "",
    ...over,
  }) as Schedule;

describe("project actions", () => {
  it("keeps only the manual specs of the project, by name; Home keeps the enabled ones", () => {
    const list = [
      sched({ id: "b", name: "zeta" }),
      sched({ id: "a", name: "alpha", enabled: false }),
      sched({ id: "t", name: "timed", spec: { kind: "interval", every: "1h" } }),
      sched({ id: "o", name: "other", project: "ops" }),
      sched({ id: "g", name: "global", project: null }),
    ];
    expect(isAction(list[0]!)).toBe(true);
    expect(isAction(list[2]!)).toBe(false);
    expect(projectActions(list, "terra").map((s) => s.name)).toEqual(["alpha", "zeta"]);
    expect(projectActions(list, "terra", true).map((s) => s.name)).toEqual(["zeta"]);
    const by = actionsByProject(list);
    expect([...by.keys()].sort()).toEqual(["ops", "terra"]);
    expect(by.get("terra")!.map((s) => s.name)).toEqual(["zeta"]);
  });

  it("says why a button is off", () => {
    expect(offReason(sched({}))).toBeNull();
    expect(offReason(sched({ enabled: false }))).toBe("disabled");
    expect(offReason(sched({ enabled: false, disabled_reason: "3 runs skipped" }))).toBe("disabled: 3 runs skipped");
  });

  it("names who the run acts as", () => {
    expect(runsAs(sched({}))).toBe("you (the operator)");
    expect(runsAs(sched({ owner: "project" }))).toBe("project terra (its rules)");
    expect(runsAs(sched({ run_as: "agent:01LEAD" }))).toBe("agent:01LEAD");
  });

  it("tells for an exec the target, the limit, and the run history", () => {
    const mail = confirmText(sched({}));
    expect(mail.what).toBe("mail → lead: Go");
    expect(mail.exec).toBeUndefined();
    const ex = confirmText(sched({ action: { type: "exec", target: "box", cmd: ["./deploy.sh", "staging"], timeout_secs: 900 } }));
    expect(ex.exec).toEqual({ target: "box", cmd: "./deploy.sh staging", limitSecs: 900 });
    expect(confirmText(sched({ action: { type: "exec", target: "box", cmd: ["ls"] } })).exec!.limitSecs).toBe(300);
    expect(limitWords(30)).toBe("30 s");
    expect(limitWords(900)).toBe("15 min");
    expect(limitWords(3600)).toBe("1 h");
    expect(historyHref(sched({ id: "01X", project: "my proj" }))).toBe("/p/my%20proj/schedules?tab=actions&open=01X");
  });

  it("shows the end of a run, a refusal, and a request that ended first", () => {
    expect(resultOf({ status: "ok", summary: "mail 01M to lead", error: null })).toEqual({ tone: "ok", status: "ok", summary: "mail 01M to lead" });
    expect(resultOf({ status: "failed", summary: "exec on box: exit code 2", error: "exit code 2" })).toMatchObject({ tone: "bad", error: "exit code 2" });
    expect(resultOf({ status: "skipped", summary: "skipped: agent not running", error: "agent not running" }).tone).toBe("warn");
    expect(failureOf({ status: 409, message: "action `terra/deploy` is disabled" })).toEqual({ tone: "bad", status: "refused", summary: "action `terra/deploy` is disabled" });
    const lost = failureOf(new TypeError("Failed to fetch"));
    expect(lost.status).toBe("unknown");
    expect(lost.summary).toMatch(/run history/);
  });
});
