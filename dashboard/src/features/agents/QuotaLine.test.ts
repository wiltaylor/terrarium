// @vitest-environment jsdom
// The quota line under the composers of the agent page (issue #22): the
// muted line from `usage/me`, the plan name only without numbers, and the
// quota hold as a banner.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Agent, QuotaHold, UsageMe } from "../../api/types";

const me = vi.fn();
let holds: QuotaHold[] = [];
vi.mock("../../stores", () => ({
  api: { usage: { me } },
  events: { on: () => () => {} },
  quota: { seedHolds: vi.fn(), holdsFor: () => holds },
}));
vi.mock("../../stores/quota", () => ({ describeCause: (c: string) => `cause ${c}` }));

const { QuotaLine } = await import("./QuotaLine");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  holds = [];
  vi.clearAllMocks();
});

const agent = (over: Partial<Agent> = {}) => ({ id: "A1", name: "w", project: "p", subscription: "claude-ultra", ...over }) as Agent;

function mount(a: Agent) {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(() => createComponent(QuotaLine, { agent: a }), el);
  return el;
}

const settle = () => new Promise((r) => setTimeout(r, 0));

describe("the quota line", () => {
  it("shows the plan and today's use from usage/me", async () => {
    me.mockResolvedValue({
      agent: { id: "A1", name: "w", project: "p" },
      subscription: { name: "claude-ultra", display_name: "Claude Ultra", windows: [] },
      today: { since: "x", mine: { input_tokens: 900, output_tokens: 100, cache_read: 0, cache_write: 0, messages: 2, turns: 2 }, share_pct: 5 },
      holds: [],
    } as unknown as UsageMe);
    const el = mount(agent());
    await settle();
    await settle();
    expect(me).toHaveBeenCalledWith("A1");
    expect(el.querySelector("[data-testid=quota-line]")?.textContent).toBe("Claude Ultra · today 1.0k tokens, 2 turns (5% of the subscription)");
    expect(el.querySelector(".error-banner")).toBeNull();
  });

  it("shows the plan name only when the daemon has no numbers", async () => {
    me.mockRejectedValue(new Error("no usage"));
    const el = mount(agent());
    await settle();
    await settle();
    expect(el.querySelector("[data-testid=quota-line]")?.textContent).toBe("claude-ultra");
  });

  it("shows a quota hold as a banner, also without a subscription, and no line then", async () => {
    holds = [{ agent: "A1", cause: "budget:project:p", reason: "the budget is spent", enforced: true }];
    const el = mount(agent({ subscription: undefined }));
    await settle();
    expect(me).not.toHaveBeenCalled();
    expect(el.querySelector("[data-testid=quota-line]")).toBeNull();
    const banner = el.querySelector(".error-banner")?.textContent ?? "";
    expect(banner).toContain("Blocked by quota until further notice.");
    expect(banner).toContain("cause budget:project:p: the budget is spent");
  });
});
