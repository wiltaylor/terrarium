import { describe, expect, it } from "vitest";
import type { AgentScreen } from "../../api/types";
import { createMockApi } from "../../api/mock";
import { KEY_BAR, rawText, screenText, screenTitle, shouldAutoRefresh } from "./screen";

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

  it("the key bar uses the daemon's key names", () => {
    expect(KEY_BAR.map((k) => k.key)).toEqual(["esc", "up", "down", "enter", "tab", "ctrl-c"]);
    expect(KEY_BAR.map((k) => k.label)).toEqual(["Esc", "↑", "↓", "Enter", "Tab", "Ctrl-C"]);
  });

  it("a raw text goes as typed; only an empty one is refused", () => {
    expect(rawText("/compact")).toBe("/compact");
    expect(rawText("  /plan ")).toBe("  /plan ");
    expect(rawText("   ")).toBeNull();
    expect(rawText("")).toBeNull();
  });

  it("the mock takes keys and raw text in the daemon's shape, and audits no text", async () => {
    const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
    const agents = await api.agents.list();
    const live = agents.find((a) => a.status === "idle" || a.status === "working")!;
    expect(await api.agents.keys(live.id, ["Esc", "down", "ctrl+c", "text:hi"])).toEqual({
      accepted: true,
      keys: ["escape", "down", "ctrl-c", "text(2)"],
    });
    expect(await api.agents.sendRaw(live.id, "/compact now")).toEqual({ accepted: true, raw: true });
    await expect(api.agents.keys(live.id, [])).rejects.toMatchObject({ code: "invalid", status: 422 });
    await expect(api.agents.keys(live.id, ["super-e"])).rejects.toMatchObject({ code: "invalid", status: 422 });
    await expect(api.agents.sendRaw(live.id, " ")).rejects.toMatchObject({ code: "invalid", status: 422 });
    const stopped = agents.find((a) => a.status === "stopped")!;
    await expect(api.agents.keys(stopped.id, ["esc"])).rejects.toMatchObject({ code: "not_running", status: 409 });
  });
});
