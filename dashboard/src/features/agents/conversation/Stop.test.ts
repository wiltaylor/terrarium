// @vitest-environment jsdom
// The Stop button of the composer (issue #12): it shows only while a turn
// runs, calls the interrupt route, and says what happened.
import { createComponent, createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../../../test/wait";
import type { Agent, AgentStatus } from "../../../api/types";
import { ApiError } from "../../../api/http";

const interrupt = vi.fn();
vi.mock("../../../stores", () => ({ api: { agents: { interrupt, send: vi.fn(), attach: vi.fn() } } }));

const { Composer } = await import("./Conversation");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});

const agent = (status: AgentStatus, status_reason?: string) => ({ id: "A1", name: "lead", project: "p", harness: "claude", status, status_reason }) as unknown as Agent;

function mount(initial: Agent) {
  const el = document.createElement("div");
  document.body.append(el);
  const [a, setA] = createSignal(initial);
  dispose = render(
    () =>
      createComponent(Composer, {
        get agent() {
          return a();
        },
        sessionActive: true,
        current: true,
      }),
    el,
  );
  const stop = () => [...el.querySelectorAll("button")].find((b) => b.textContent?.startsWith("Stop"));
  return { el, setA, stop };
}

describe("the Stop button", () => {
  it("shows only while a turn runs", () => {
    const { setA, stop } = mount(agent("idle"));
    expect(stop()).toBeUndefined();
    setA(agent("working"));
    expect(stop()).toBeDefined();
    setA(agent("blocked"));
    expect(stop()).toBeDefined();
    setA(agent("stopped"));
    expect(stop()).toBeUndefined();
  });

  it("interrupts the turn and says so", async () => {
    let done!: (v: unknown) => void;
    interrupt.mockReturnValue(new Promise((r) => (done = r)));
    const { el, setA, stop } = mount(agent("working"));
    stop()!.click();
    expect(interrupt).toHaveBeenCalledWith("A1");
    await waitFor(() => expect(stop()!.textContent).toBe("Stopping…"));
    expect(stop()!.disabled).toBe(true);
    done({ accepted: true, interrupted: true });
    await waitFor(() => expect(el.textContent).toContain("Stop sent"));
    // The daemon reports the end of the turn: the button goes, the note stays.
    setA(agent("idle", "interrupted"));
    expect(stop()).toBeUndefined();
    expect(el.querySelector('[role="status"]')?.textContent).toMatch(/^Interrupted/);
  });

  it("shows a refusal", async () => {
    interrupt.mockRejectedValue(new ApiError(409, "the agent runs no turn: there is nothing to interrupt", undefined, "conflict"));
    const { el, stop } = mount(agent("working"));
    stop()!.click();
    await waitFor(() => expect(el.textContent).toContain("nothing to interrupt"));
  });
});
