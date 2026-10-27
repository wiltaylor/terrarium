import { describe, expect, it } from "vitest";
import type { AgentEvent, Wait } from "../../api/types";
import { statusTitle } from "../../components/StatusBadge";
import { isProcessResult, isWaitEventFor, putWait, splitWaits, toggleOn, waitCommand, waitResultText, waitStateChip, watchBody, watchRole, watchText } from "./waits";

const wait = (over: Partial<Wait> = {}): Wait => ({
  id: "W1",
  agent: "A1",
  agent_name: "demo/lead",
  project: "demo",
  kind: "agent",
  spec: { agent: "A2", agent_name: "demo/w1", until: "done" },
  summary: "agent demo/w1 (done)",
  state: "open",
  created_by: "operator",
  created_at: "2026-10-27T10:00:00Z",
  ...over,
});

describe("wait results", () => {
  it("names a process exit, a signal, and a reason", () => {
    const exit = { exit_code: 3, signal: null, reason: "exited", duration_secs: 12.3 };
    expect(isProcessResult(exit)).toBe(true);
    expect(waitResultText(wait({ kind: "process", state: "resolved", result: exit }))).toBe("exit 3 · 12s");
    expect(waitResultText(wait({ kind: "process", state: "cancelled", result: { exit_code: null, signal: "SIGKILL", reason: "killed", duration_secs: 1.25 } }))).toBe("killed (SIGKILL) · 1.3s");
    expect(waitResultText(wait({ kind: "process", state: "failed", result: { exit_code: null, signal: null, reason: "lost" } }))).toBe("lost");
  });

  it("uses the outcome of the other kinds, else a dash", () => {
    expect(isProcessResult({ outcome: "due" })).toBe(false);
    expect(waitResultText(wait({ state: "resolved", result: { outcome: "idle" } }))).toBe("idle");
    expect(waitResultText(wait({ state: "timed_out" }))).toBe("—");
    expect(waitResultText(wait({ result: null }))).toBe("—");
  });

  it("gives each state a chip, and the command of a process wait", () => {
    expect(waitStateChip("open")).toBe("info");
    expect(waitStateChip("failed")).toBe("bad");
    expect(waitCommand(wait({ kind: "process", spec: { target: "box", session: "S", command: "cargo test" } }))).toBe("cargo test");
    expect(waitCommand(wait())).toBeNull();
  });
});

describe("wait lists", () => {
  it("splits open and closed, newest first", () => {
    const list = [
      wait({ id: "o1", created_at: "2026-10-27T09:00:00Z" }),
      wait({ id: "c1", state: "resolved", created_at: "2026-10-27T08:00:00Z", resolved_at: "2026-10-27T11:00:00Z" }),
      wait({ id: "o2", created_at: "2026-10-27T10:00:00Z" }),
      wait({ id: "c2", state: "cancelled", created_at: "2026-10-27T10:30:00Z", resolved_at: "2026-10-27T10:40:00Z" }),
    ];
    const { open, closed } = splitWaits(list);
    expect(open.map((w) => w.id)).toEqual(["o2", "o1"]);
    expect(closed.map((w) => w.id)).toEqual(["c1", "c2"]);
  });

  it("puts a changed wait in place, and a new one first", () => {
    const list = [wait({ id: "a" }), wait({ id: "b" })];
    expect(putWait(list, wait({ id: "b", state: "cancelled" })).map((w) => [w.id, w.state])).toEqual([["a", "open"], ["b", "cancelled"]]);
    expect(putWait(list, wait({ id: "c" })).map((w) => w.id)).toEqual(["c", "a", "b"]);
  });

  it("matches wait events by the agent ULID", () => {
    const ev = (over: Partial<AgentEvent>): AgentEvent => ({ seq: 1, id: "E", at: "", kind: "wait", summary: "", data: {}, ...over });
    expect(isWaitEventFor(ev({ agent_id: "A1" }), "A1")).toBe(true);
    expect(isWaitEventFor(ev({ data: { agent_id: "A1" } }), "A1")).toBe(true);
    expect(isWaitEventFor(ev({ agent_id: "A2" }), "A1")).toBe(false);
    expect(isWaitEventFor(ev({ kind: "lease", agent_id: "A1" }), "A1")).toBe(false);
  });
});

describe("watches", () => {
  it("keeps the form's statuses in a fixed order", () => {
    expect(toggleOn(["error"], "idle", true)).toEqual(["idle", "error"]);
    expect(toggleOn(["idle", "error"], "idle", false)).toEqual(["error"]);
    expect(toggleOn(["idle"], "idle", true)).toEqual(["idle"]);
  });

  it("refuses an empty watch and sends debounce", () => {
    expect(watchBody([], false)).toEqual({ error: "Pick one status or more." });
    expect(watchBody(["idle", "blocked"], true)).toEqual({ body: { on: ["idle", "blocked"], once: true, debounce: true } });
  });

  it("says whether a watch is on the agent or by it, and its short text", () => {
    expect(watchRole({ agent: "A1", watcher: "operator" }, "A1")).toBe("on");
    expect(watchRole({ agent: "A2", watcher: "A1" }, "A1")).toBe("by");
    expect(watchText({ on: ["idle", "blocked"], once: true, fired: 2 })).toBe("idle, blocked · once · fired 2×");
    expect(watchText({ on: ["error"], once: false, fired: 0 })).toBe("error");
  });
});

describe("the status badge title", () => {
  it("lists what a waiting agent waits on", () => {
    expect(statusTitle("waiting", "agent demo/w1 (done)")).toBe("Status: waiting on agent demo/w1 (done)");
    expect(statusTitle("waiting")).toBe("Status: waiting");
    expect(statusTitle("idle", "ignored")).toBe("Status: idle");
  });
});
