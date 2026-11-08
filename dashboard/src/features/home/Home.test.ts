// @vitest-environment jsdom
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const navigate = vi.fn();
vi.mock("@solidjs/router", () => ({ A: (p: { children?: unknown }) => p.children, useNavigate: () => navigate }));
vi.mock("../../stores", () => ({
  api,
  agents: { state: { agents: api.world.agents } },
  projects: { projects: () => api.world.projects },
  subscriptions: { list: () => [], loaded: () => true, missing: () => false, byName: () => undefined },
  events: { on: () => () => {} },
}));
vi.mock("./ActiveCards", () => ({ ActiveCards: () => null }));
vi.mock("./BlockedCards", () => ({ BlockedCards: () => null }));
vi.mock("./HostCards", () => ({ HostCards: () => null }));
const { Home } = await import("./Home");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.innerHTML = ""; vi.clearAllMocks(); });
const settle = () => new Promise((r) => setTimeout(r, 0));
function button(label: string) {
  return [...document.querySelectorAll("button")].find((b) => b.textContent?.trim() === label)!;
}
function input(selector: string, value: string, event = "input") {
  const el = document.querySelector(selector) as HTMLInputElement;
  el.value = value;
  el.dispatchEvent(new Event(event, { bubbles: true }));
}
function mount() {
  const el = document.createElement("div"); document.body.append(el);
  dispose = render(() => createComponent(Home, {}), el);
}
it("asks from Home with the default agent", async () => {
  const a = api.world.agents.find((a) => !a.ephemeral)!;
  await api.account.setPreferences({ default_agent: `${a.project}/${a.name}` });
  const ask = vi.spyOn(api.inbox, "ask");
  mount(); button("Ask an agent").click(); await settle();
  input("textarea", "Why?"); button("Ask").click(); await settle();
  expect(ask).toHaveBeenCalledWith({ to: `${a.project}/${a.name}`, text: "Why?" });
  expect(navigate).toHaveBeenCalledWith("/inbox?status=asked");
});
it("messages from Home", async () => {
  const a = api.world.agents.find((a) => !a.ephemeral)!;
  await api.account.setPreferences({ default_agent: `${a.project}/${a.name}` });
  const send = vi.spyOn(api.mail, "send");
  mount(); button("Message an agent").click(); await settle();
  input("textarea", "Hello"); button("Send").click(); await settle();
  expect(send).toHaveBeenCalled();
  expect(navigate).toHaveBeenCalledWith("/inbox");
});
it("opens New thread from Home with the shared dialog", async () => {
  const open = vi.spyOn(api.threads, "open");
  mount(); button("New thread").click(); await settle();
  expect(document.querySelector("form[aria-label='New thread']")).not.toBeNull();
  input("select[aria-label=Scope]", "terrarium", "change");
  input("select[aria-label=Agent]", "terrarium/lead", "change");
  input("input[aria-label=Topic]", "A new topic");
  input("textarea", "First message");
  button("Open the thread").click(); await settle();
  expect(open).toHaveBeenCalledWith({ scope: "terrarium", agent: "terrarium/lead", topic: "A new topic", text: "First message" });
  expect(navigate).toHaveBeenCalledWith(expect.stringContaining("/inbox/threads/"));
  expect(document.querySelector("form[aria-label='New thread']")).toBeNull();
});
