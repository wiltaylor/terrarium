import { describe, expect, it } from "vitest";
import { actionWords, commandLine, cronWords, everyWords, guardWords, matchesWords, nextRunWords, parseEvery, shortAddress, specWords, splitCommand } from "./schedule";

describe("specWords: interval", () => {
  it("reads the duration forms", () => {
    expect(parseEvery("15m")).toBe(900);
    expect(parseEvery("1h30m")).toBe(5400);
    expect(parseEvery("15 min")).toBe(900);
    expect(parseEvery("2 days")).toBe(172800);
    expect(parseEvery("90")).toBe(90);
    expect(parseEvery(300)).toBe(300);
    expect(parseEvery("soon")).toBeNull();
    expect(parseEvery("15x")).toBeNull();
    expect(parseEvery("")).toBeNull();
    expect(parseEvery("0m")).toBeNull();
  });

  it("says the largest whole unit", () => {
    expect(everyWords(60)).toBe("minute");
    expect(everyWords(900)).toBe("15 min");
    expect(everyWords(3600)).toBe("hour");
    expect(everyWords(7200)).toBe("2 h");
    expect(everyWords(5400)).toBe("1 h 30 min");
    expect(everyWords(86400)).toBe("day");
    expect(everyWords(3 * 86400)).toBe("3 days");
    expect(everyWords(30)).toBe("30 s");
    expect(everyWords(90)).toBe("1 min 30 s");
  });

  it("puts it together, and keeps a duration it cannot read", () => {
    expect(specWords({ kind: "interval", every: "15m" })).toBe("every 15 min");
    expect(specWords({ kind: "interval", every: "1h" })).toBe("every hour");
    expect(specWords({ kind: "interval", every: "fortnightly" })).toBe("every fortnightly");
  });
});

describe("specWords: at", () => {
  it("says once at the UTC time", () => {
    expect(specWords({ kind: "at", at: "2026-11-01T09:00:00Z" })).toBe("once at 2026-11-01 09:00 UTC");
    expect(specWords({ kind: "at", at: "2026-11-01T09:00:00+10:00" })).toBe("once at 2026-10-31 23:00 UTC");
  });
});

describe("specWords: cron", () => {
  const cron = (expr: string, tz = "UTC") => specWords({ kind: "cron", expr, tz });
  it("names the minute steps and the hourly forms", () => {
    expect(cron("* * * * *")).toBe("every minute");
    expect(cron("*/5 * * * *")).toBe("every 5 min");
    expect(cron("*/15 * * * *")).toBe("every 15 min");
    expect(cron("0 * * * *")).toBe("hourly");
    expect(cron("@hourly")).toBe("hourly");
    expect(cron("7 * * * *")).toBe("hourly at :07");
    expect(cron("0 */2 * * *")).toBe("every 2 h");
    expect(cron("30 */6 * * *")).toBe("every 6 h at :30");
  });

  it("says daily, weekdays, weekly, monthly, and yearly with the time and the zone", () => {
    expect(cron("0 9 * * *")).toBe("daily 09:00 UTC");
    expect(cron("@daily")).toBe("daily 00:00 UTC");
    expect(cron("0 9,17 * * *", "Europe/Berlin")).toBe("daily 09:00, 17:00 Europe/Berlin");
    expect(cron("0 9 * * 1-5")).toBe("weekdays 09:00 UTC");
    expect(cron("0 9 * * MON-FRI")).toBe("weekdays 09:00 UTC");
    expect(cron("0 10 * * 0,6")).toBe("weekends 10:00 UTC");
    expect(cron("0 9 * * 1")).toBe("weekly Mon 09:00 UTC");
    expect(cron("0 9 * * 7")).toBe("weekly Sun 09:00 UTC");
    expect(cron("0 9 * * 1,3,5")).toBe("Mon, Wed, Fri 09:00 UTC");
    expect(cron("0 9 * * 0,1")).toBe("Mon, Sun 09:00 UTC");
    expect(cron("30 6 1 * *")).toBe("monthly on day 1 06:30 UTC");
    expect(cron("0 0 25 12 *")).toBe("yearly on Dec 25 00:00 UTC");
  });

  it("falls back to the expression and the zone, and marks a bad one", () => {
    expect(cron("0 9-17 * * *")).toBe("cron 0 9-17 * * * (UTC)");
    expect(cron("0 0 13 * 5")).toBe("cron 0 0 13 * 5 (UTC)");
    expect(cron("0 0 * 1 *")).toBe("cron 0 0 * 1 * (UTC)");
    expect(cron("61 * * * *")).toBe("cron 61 * * * * (invalid)");
    expect(cronWords("0 9 * * *", "")).toBe("daily 09:00 UTC");
  });
});

describe("actionWords", () => {
  it("summarises each action type, with addresses short in the schedule's project", () => {
    expect(actionWords({ type: "mail", to: "terrarium/lead", subject: "Standup", body: "" }, "terrarium")).toBe("mail → lead: Standup");
    expect(actionWords({ type: "mail", to: "webshop/lead", subject: "", body: "" }, "terrarium")).toBe("mail → webshop/lead: (no subject)");
    expect(actionWords({ type: "mail", to: "project:terrarium", subject: "Weekly", body: "" }, "terrarium")).toBe("mail → terrarium inbox: Weekly");
    expect(actionWords({ type: "mail", to: "operator", subject: "Digest", body: "" }, null)).toBe("mail → operator: Digest");
    expect(actionWords({ type: "exec", target: "svc-dev", cmd: ["make", "test"] }, "terrarium")).toBe("exec on svc-dev: make test");
    expect(actionWords({ type: "turn", agent: "terrarium/w1", text: "go" }, "terrarium")).toBe("turn → w1");
    expect(actionWords({ type: "agent", agent: "terrarium/w1", verb: "restart" }, "terrarium")).toBe("restart → w1");
    expect(actionWords({ type: "probe", subscription: "anthropic-max" }, null)).toBe("probe anthropic-max");
  });

  it("keeps an address of another project, and a ULID, as they are", () => {
    expect(shortAddress("01JB0WRK1", "terrarium")).toBe("01JB0WRK1");
    expect(shortAddress("terrarium/lead", null)).toBe("terrarium/lead");
  });
});

describe("command lines", () => {
  it("splits on spaces and keeps quoted groups", () => {
    expect(splitCommand("make test")).toEqual({ argv: ["make", "test"] });
    expect(splitCommand(`  sh -c 'echo "hi there"' `)).toEqual({ argv: ["sh", "-c", 'echo "hi there"'] });
    expect(splitCommand(`git commit -m "a b" x\\ y ''`)).toEqual({ argv: ["git", "commit", "-m", "a b", "x y", ""] });
    expect(splitCommand("")).toEqual({ argv: [] });
    expect(splitCommand("echo 'open")).toEqual({ error: "a single quote is not closed" });
    expect(splitCommand("echo x\\")).toEqual({ error: "the line ends with a backslash" });
  });

  it("joins an argv back, quoting what needs it", () => {
    expect(commandLine(["make", "test"])).toBe("make test");
    expect(commandLine(["sh", "-c", "echo hi"])).toBe("sh -c 'echo hi'");
    expect(commandLine(["echo", "it's"])).toBe(`echo 'it'\\''s'`);
    const line = commandLine(["sh", "-c", "echo 'x y'", ""]);
    expect(splitCommand(line)).toEqual({ argv: ["sh", "-c", "echo 'x y'", ""] });
  });
});

describe("nextRunWords", () => {
  const now = Date.parse("2026-10-26T10:00:00Z");
  const base = { enabled: true, next_run_at: "2026-10-26T10:14:00Z", spec: { kind: "interval", every: "15m" } as const, last_run: null };
  it("says in how long, due, disabled, or done", () => {
    expect(nextRunWords(base, now)).toBe("in 14m");
    expect(nextRunWords({ ...base, next_run_at: "2026-10-26T09:59:00Z" }, now)).toBe("due now");
    expect(nextRunWords({ ...base, enabled: false }, now)).toBe("disabled");
    expect(nextRunWords({ ...base, next_run_at: null, spec: { kind: "at", at: "2026-10-26T09:00:00Z" }, last_run: { at: "2026-10-26T09:00:00Z", status: "ok", summary: "", run_id: "r" } }, now)).toBe("done (ran once)");
    expect(nextRunWords({ ...base, next_run_at: null }, now)).toBe("not planned");
  });
});

describe("a trigger in words", () => {
  const spec = { kind: "on" as const, kinds: ["ci.run.failed", "ci.job.*"], match: [{ path: "data.branch", value: "main" }, { path: "data.repo", op: "in" as const, value: ["a", "b"] }] };
  it("the kinds, the rules, and the guards", () => {
    expect(specWords(spec)).toBe("on ci.run.failed, ci.job.*");
    expect(matchesWords(spec)).toBe("data.branch = main and data.repo in a, b");
    expect(guardWords(spec)).toBe("max 30/h");
    expect(guardWords({ ...spec, dedupe: { key: "k", window: "10m" }, escalate: { key: "k", after: 3 }, max_per_hour: 5 })).toBe("dedupe 10m · escalate after 3 → operator · max 5/h");
  });

  it("has no next run: it runs on each match", () => {
    expect(nextRunWords({ enabled: true, next_run_at: null, spec, last_run: null })).toBe("on each match");
    expect(nextRunWords({ enabled: false, next_run_at: null, spec, last_run: null })).toBe("disabled");
  });

  it("the card action", () => {
    expect(actionWords({ type: "card", op: "open", key: "ci:{{event.data.repo}}", title: "CI failed" }, "p")).toBe("card open ci:{{event.data.repo}}: CI failed");
    expect(actionWords({ type: "card", op: "close", key: "ci" }, "p")).toBe("card close ci");
  });
});
