// @vitest-environment jsdom
// The conversation of the pinned bar (TER-44): the composer is on for the
// agent's current session also when the agent is stopped (the daemon wakes
// it) and when the transcript says the session is not active, and a turn
// goes out.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Agent, AgentStatus } from "../../../api/types";

const send = vi.fn();
const sessions = vi.fn();
const transcript = vi.fn();
const transcriptStream = vi.fn();
vi.mock("../../../stores", async () => { const { instanceTestStores } = await import("../../../test/instance"); return instanceTestStores(({ api: { agents: { send, sessions, transcript, transcriptStream, attach: vi.fn(), interrupt: vi.fn() } } })); });

const { Conversation } = await import("./Conversation");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});

const agent = (status: AgentStatus) => ({ id: "A1", name: "lead", project: "p", harness: "claude", status, session: "S1" }) as unknown as Agent;

function mount(a: Agent) {
  const el = document.createElement("div");
  document.body.append(el);
  // As the pinned bar mounts it: with the link that opens the terminal.
  dispose = render(() => createComponent(Conversation, { agent: a, onOpenTerminal: () => {} }), el);
  return el;
}

const settle = async () => {
  for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0));
};

describe("the pinned conversation's composer", () => {
  for (const [status, ended] of [
    ["stopped", true],
    ["idle", false],
  ] as const) {
    it(`sends a turn for the current session of a ${status} agent whose transcript is not active`, async () => {
      sessions.mockResolvedValue({ items: [{ id: "S1", current: true, active: false, blocks: 0, started_at: "2026-11-05T10:00:00Z", ...(ended ? { ended_at: "2026-11-05T11:00:00Z" } : {}) }] });
      transcript.mockResolvedValue({ session: "S1", harness: "claude", active: false, blocks: [], next_after: 0, complete: true });
      transcriptStream.mockReturnValue({ subscribe: () => () => {}, onState: () => () => {}, close: () => {} });
      send.mockResolvedValue({ accepted: true, queued: status === "stopped" });
      const el = mount(agent(status));
      await settle();
      const box = el.querySelector("textarea[aria-label='Turn text']") as HTMLTextAreaElement;
      expect(box.disabled).toBe(false);
      expect(el.textContent).not.toContain("not the active session");
      if (status === "stopped") expect(el.textContent).toContain("wakes to run the turn");
      box.value = "hello";
      box.dispatchEvent(new Event("input", { bubbles: true }));
      const sendButton = [...el.querySelectorAll("button")].find((b) => b.textContent?.trim() === "Send")!;
      expect(sendButton.disabled).toBe(false);
      sendButton.click();
      await settle();
      expect(send).toHaveBeenCalledWith("A1", "hello");
    });
  }
});
