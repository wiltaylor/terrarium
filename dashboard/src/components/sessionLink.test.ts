// @vitest-environment jsdom
// The session link of a log row (docs/api.md §4, §7, §9): the model, and the
// link in the event list and in a mail message.
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentEvent, Message } from "../api/types";
import { sessionLinkOf, sessionPage } from "./sessionLink";

vi.mock("../stores", async () => { const { instanceTestStores } = await import("../test/instance"); return instanceTestStores(({
  agents: { state: { agents: [] }, byId: () => undefined },
  projects: { projects: () => [] },
  api: {},
})); });

const { EventList } = await import("./EventList");
const { MessageView } = await import("../features/mail/parts");

describe("sessionLinkOf", () => {
  it("links a row with a session to the session page", () => {
    expect(sessionLinkOf({ session_id: "01SES", session_label: "dev · 2026-10-26 09:12 UTC" })).toEqual({
      href: "/sessions/01SES",
      text: "session",
      title: "Open the session: dev · 2026-10-26 09:12 UTC",
    });
  });

  it("gives no link without a session, and an ID title without a label", () => {
    expect(sessionLinkOf({})).toBeNull();
    expect(sessionLinkOf({ session_label: "dev · x" })).toBeNull();
    expect(sessionLinkOf({ session_id: "01SES" })?.title).toBe("Open the session: session 01SES");
  });

  it("escapes the ID in the path", () => {
    expect(sessionPage("a/b c")).toBe("/sessions/a%2Fb%20c");
  });
});

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
});

const mount = (view: () => JSX.Element) => {
  const el = document.createElement("div");
  document.body.append(el);
  const history = createMemoryHistory();
  history.set({ value: "/logs/events" });
  dispose = render(
    () =>
      createComponent(MemoryRouter, {
        history,
        get children() {
          return createComponent(Route, { path: "*", component: view });
        },
      }),
    el,
  );
  return el;
};

const event = (seq: number, extra: Partial<AgentEvent> = {}): AgentEvent => ({
  seq,
  id: `ev${seq}`,
  at: "2026-10-26T09:30:00.000Z",
  kind: "turn_start",
  agent: "dev",
  agent_id: "01AGENT",
  project: "p",
  summary: `dev: turn ${seq}`,
  data: {},
  ...extra,
});

const links = (el: HTMLElement) => Array.from(el.querySelectorAll<HTMLAnchorElement>("a.session-link"));

describe("the session link in the log rows", () => {
  it("shows a link on each event with a session, and none on the others", () => {
    const el = mount(() => EventList({ events: [event(2, { session_id: "01SES", session_label: "dev · 2026-10-26 09:12 UTC" }), event(1)], showAgent: true }));
    const rows = Array.from(el.querySelectorAll(".event-row"));
    expect(rows).toHaveLength(2);
    const [a] = links(el);
    expect(links(el)).toHaveLength(1);
    expect(a!.getAttribute("href")).toBe("/sessions/01SES");
    expect(a!.title).toBe("Open the session: dev · 2026-10-26 09:12 UTC");
    expect(a!.textContent).toBe("session");
    expect(a!.closest(".event-row")).toBe(rows[0]);
    expect(rows[1]!.querySelector(".event-session")?.childElementCount).toBe(0);
  });

  it("shows the sender's session on a message of the thread view", () => {
    const m: Message = {
      id: "m1",
      thread_id: "t1",
      from: "01AGENT",
      from_kind: "agent",
      from_name: "p/dev",
      to: "operator",
      to_kind: "operator",
      to_name: "operator",
      subject: "hi",
      body: "hello",
      state: "delivered",
      created_at: "2026-10-26T09:30:00.000Z",
      session_id: "01SES",
      session_label: "dev · 2026-10-26 09:12 UTC",
    };
    const el = mount(() => MessageView({ message: m }));
    expect(links(el).map((a) => a.getAttribute("href"))).toEqual(["/sessions/01SES"]);
    const none = mount(() => MessageView({ message: { ...m, session_id: undefined, session_label: undefined } }));
    expect(links(none)).toHaveLength(0);
  });
});
