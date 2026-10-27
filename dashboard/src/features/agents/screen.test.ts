import { describe, expect, it } from "vitest";
import type { AgentScreen } from "../../api/types";
import { createMockApi } from "../../api/mock";
import { screenText, screenTitle, shouldAutoRefresh } from "./screen";

const snap = (p: Partial<AgentScreen> = {}): AgentScreen => ({
  agent: "p/w",
  agent_id: "01W",
  status: "blocked",
  rows: 24,
  cols: 80,
  text: "Allow?\n> Yes",
  at: "2026-10-27T10:02:11.000Z",
  ...p,
});

describe("screen panel model", () => {
  it("keeps plain text as it is", () => {
    expect(screenText(snap())).toBe("Allow?\n> Yes");
  });

  it("removes escape codes and control characters", () => {
    const text = "\x1b[1;31mError\x1b[0m   \n\x1b]0;title\x07\x1b[?25l> <b>x</b>\x07\x00";
    expect(screenText(snap({ text }))).toBe("Error\n> <b>x</b>");
  });

  it("names the agent, status, and size", () => {
    expect(screenTitle(snap())).toBe("p/w · blocked · 24×80");
  });

  it("refreshes by itself only while visible, on, and running", () => {
    expect(shouldAutoRefresh({ auto: true, pageVisible: true, running: true })).toBe(true);
    expect(shouldAutoRefresh({ auto: false, pageVisible: true, running: true })).toBe(false);
    expect(shouldAutoRefresh({ auto: true, pageVisible: false, running: true })).toBe(false);
    expect(shouldAutoRefresh({ auto: true, pageVisible: true, running: false })).toBe(false);
  });

  it("the mock answers in the daemon's shape, and 409 when stopped", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const agents = await api.agents.list();
    const live = agents.find((a) => a.status === "idle" || a.status === "working")!;
    const s = await api.agents.screen(live.id);
    expect(s).toMatchObject({ agent: `${live.project}/${live.name}`, agent_id: live.id, rows: 24, cols: 80 });
    expect(screenText(s)).toBe(s.text);
    const stopped = agents.find((a) => a.status === "stopped")!;
    await expect(api.agents.screen(stopped.id)).rejects.toMatchObject({ code: "not_running", status: 409 });
  });
});
