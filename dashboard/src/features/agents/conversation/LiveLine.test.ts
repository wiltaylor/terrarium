// @vitest-environment jsdom
// The bottom of the conversation (TER-54): the live line follows the
// agent's status, and the queued turns show oldest first with Edit (the
// text leaves the queue and goes into the composer) and Remove.
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../../../test/wait";
import type { Agent, AgentStatus, QueuedTurn } from "../../../api/types";
import { ApiError } from "../../../api/http";

const queued = vi.fn();
const removeQueued = vi.fn();
const sessions = vi.fn();
vi.mock("../../../stores", () => ({
  api: {
    agents: {
      queued,
      removeQueued,
      sessions,
      transcript: vi.fn(),
      transcriptStream: vi.fn(),
      send: vi.fn(),
      attach: vi.fn(),
      interrupt: vi.fn(),
    },
  },
}));

const { LiveLine, Conversation } = await import("./Conversation");
const { footerState } = await import("./model");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});

const agent = (status: AgentStatus, more: Partial<Agent> = {}) =>
  ({ id: "A1", name: "w1", project: "p", harness: "claude", status, ...more }) as unknown as Agent;

const turn = (id: number, body: string): QueuedTurn => ({ id, source: "store", text: `[User] ${body}`, body, created_at: "2026-11-06T01:00:00.000Z" });

describe("footerState", () => {
  it("names each status", () => {
    expect(footerState({ status: "working" })).toEqual({ text: "Thinking…", running: true, tone: "info" });
    expect(footerState({ status: "waiting", waitingOn: "process build on box" }).text).toBe("Waiting on process build on box");
    expect(footerState({ status: "waiting" }).text).toBe("Waiting");
    expect(footerState({ status: "blocked", reason: "a question" }).text).toBe("Blocked: a question");
    expect(footerState({ status: "idle" }).text).toBe("Idle");
    expect(footerState({ status: "stopped" }).text).toBe("Stopped");
    expect(footerState({ status: "stopped", reason: "idle" }).text).toBe("Stopped (idle)");
    expect(footerState({ status: "stopped", paused: true }).text).toBe("Paused");
    expect(footerState({ status: "starting" }).running).toBe(true);
    expect(footerState({ status: "error", reason: "spawn failed" })).toMatchObject({ text: "Error: spawn failed", tone: "err" });
  });
});

describe("the live line", () => {
  it("follows the status, with a moving indicator only while a turn runs", () => {
    const el = document.createElement("div");
    document.body.append(el);
    const [a, setA] = createSignal(agent("idle"));
    dispose = render(
      () =>
        createComponent(LiveLine, {
          get agent() {
            return a();
          },
        }),
      el,
    );
    const line = () => el.querySelector<HTMLElement>(".convo-live")!;
    expect(line().textContent).toBe("Idle");
    expect(line().dataset.running).toBe("false");
    setA(agent("working"));
    expect(line().textContent).toBe("Thinking…");
    expect(line().dataset.running).toBe("true");
    expect(el.querySelector(".convo-live-spinner")).not.toBeNull();
    setA(agent("waiting", { waiting_on: "time 2026-11-06T02:00:00.000Z" }));
    expect(line().textContent).toBe("Waiting on time 2026-11-06T02:00:00.000Z");
    expect(el.querySelector(".convo-live-spinner")).toBeNull();
    setA(agent("blocked", { status_reason: "permission prompt" }));
    expect(line().textContent).toBe("Blocked: permission prompt");
    setA(agent("stopped"));
    expect(line().textContent).toBe("Stopped");
  });
});

function mountConversation(initial: Agent) {
  const el = document.createElement("div");
  document.body.append(el);
  const [a, setA] = createSignal(initial);
  dispose = render(
    () =>
      createComponent(Conversation, {
        get agent() {
          return a();
        },
      }),
    el,
  );
  const items = () => [...el.querySelectorAll(".convo-queued-item .convo-queued-text")].map((n) => n.textContent);
  const button = (row: number, label: string) =>
    [...el.querySelectorAll(".convo-queued-item")[row]!.querySelectorAll("button")].find((b) => b.textContent?.trim() === label)!;
  const box = () => el.querySelector<HTMLTextAreaElement>("textarea")!;
  return { el, setA, items, button, box };
}

describe("the queued turns", () => {
  it("keeps store items visible when the runner queue is unavailable", async () => {
    sessions.mockResolvedValue({ items: [] });
    queued.mockResolvedValue({ items: [turn(1, "stored")], queues: { runner: "unavailable" } });
    const { el, items } = mountConversation(agent("working"));
    await waitFor(() => expect(items()).toEqual(["stored"]));
    expect(el.textContent).toContain("Runner queue: unavailable");
  });

  it("does not poll an empty idle queue", async () => {
    vi.useFakeTimers();
    try {
      sessions.mockResolvedValue({ items: [] });
      queued.mockResolvedValue({ items: [] });
      mountConversation(agent("idle"));
      await Promise.resolve();
      await Promise.resolve();
      const calls = queued.mock.calls.length;
      expect(calls).toBe(1);
      await vi.advanceTimersByTimeAsync(6000);
      expect(queued).toHaveBeenCalledTimes(calls);
    } finally {
      dispose?.();
      dispose = undefined;
      vi.useRealTimers();
    }
  });

  it.each(["runner", "supervisor"] as const)("edits opaque %s IDs even without a store queue count", async (source) => {
    sessions.mockResolvedValue({ items: [] });
    queued.mockResolvedValue({ items: [{ ...turn(1, "live"), id: "opaque-id", source }] });
    removeQueued.mockResolvedValue(undefined);
    const { items, button, box } = mountConversation(agent("working"));
    await waitFor(() => expect(items()).toEqual(["live"]));
    button(0, "Edit").click();
    expect(removeQueued).toHaveBeenCalledWith("A1", "opaque-id", source);
    await waitFor(() => expect(items()).toEqual([]));
    expect(box().value).toBe("live");
  });

  it("show oldest first; Edit moves the text into the composer and out of the queue", async () => {
    sessions.mockResolvedValue({ items: [] });
    queued.mockResolvedValue({ items: [turn(1, "first"), turn(2, "second")] });
    removeQueued.mockResolvedValue(undefined);
    const { items, button, box } = mountConversation(agent("stopped", { queued: 2, paused: true }));
    await waitFor(() => expect(items()).toEqual(["first", "second"]));
    expect(queued).toHaveBeenCalledWith("A1");
    button(1, "Edit").click();
    expect(removeQueued).toHaveBeenCalledWith("A1", 2, "store");
    await waitFor(() => expect(items()).toEqual(["first"]));
    expect(box().value).toBe("second");
    // A second edit joins the draft on a new line.
    button(0, "Edit").click();
    await waitFor(() => expect(items()).toEqual([]));
    expect(box().value).toBe("second\nfirst");
  });

  it("Remove drops a turn and leaves the composer alone", async () => {
    sessions.mockResolvedValue({ items: [] });
    queued.mockResolvedValue({ items: [turn(1, "first"), turn(2, "second")] });
    removeQueued.mockResolvedValue(undefined);
    const { items, button, box } = mountConversation(agent("stopped", { queued: 2, paused: true }));
    await waitFor(() => expect(items()).toEqual(["first", "second"]));
    button(0, "Remove").click();
    expect(removeQueued).toHaveBeenCalledWith("A1", 1, "store");
    await waitFor(() => expect(items()).toEqual(["second"]));
    expect(box().value).toBe("");
  });

  it("a turn that went out already: says so, reads the queue again, and keeps the composer", async () => {
    sessions.mockResolvedValue({ items: [] });
    queued.mockResolvedValueOnce({ items: [turn(1, "first")] }).mockResolvedValue({ items: [] });
    removeQueued.mockRejectedValue(new ApiError(404, "turn 1 does not wait for the agent", undefined, "not_found"));
    const { el, items, button, box } = mountConversation(agent("stopped", { queued: 1 }));
    await waitFor(() => expect(items()).toEqual(["first"]));
    button(0, "Edit").click();
    await waitFor(() => expect(el.textContent).toContain("went to the agent already"));
    await waitFor(() => expect(items()).toEqual([]));
    expect(box().value).toBe("");
  });

  it("reads nothing while a stopped agent has no queued work", async () => {
    sessions.mockResolvedValue({ items: [] });
    const { el, setA } = mountConversation(agent("stopped"));
    await Promise.resolve();
    expect(queued).not.toHaveBeenCalled();
    expect(el.querySelector(".convo-queued")).toBeNull();
    queued.mockResolvedValue({ items: [turn(5, "later")] });
    setA(agent("stopped", { queued: 1 }));
    await waitFor(() => expect(el.querySelector(".convo-queued")?.textContent).toContain("later"));
  });
});
