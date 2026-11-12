// @vitest-environment jsdom
import { MemoryRouter, Route } from "@solidjs/router";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import type { Message } from "../../api/types";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api, agents: { state: { agents: api.world.agents } } });
});
const { MessageView } = await import("./parts");
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); });

it("links mail authors and known sessions, leaving operator and removed agents plain", () => {
  const agent = api.world.agents[0]!;
  const base: Message = { id: "m", thread_id: "t", from: agent.id, from_name: agent.name, from_kind: "agent", to: "operator", to_name: "operator", to_kind: "operator", body: "Message", subject: "", state: "read", created_at: "2026-11-12T00:00:00Z", session_id: "known-session" };
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter><Route path="/" component={() => <>
    <MessageView message={base} />
    <MessageView message={{ ...base, id: "op", from: "operator", from_name: "operator", from_kind: "operator", session_id: undefined }} />
    <MessageView message={{ ...base, id: "gone", from: "removed-agent", from_name: "former worker", session_id: undefined }} />
  </>} /></MemoryRouter>, root);
  const posts = root.querySelectorAll(".thread-post");
  expect(posts[0]!.querySelector("strong a")?.getAttribute("href")).toBe(`/agents/${agent.id}`);
  expect(posts[0]!.querySelector('a[href="/sessions/known-session"]')).not.toBeNull();
  for (const post of [posts[1]!, posts[2]!]) {
    expect(post.querySelector("strong a")).toBeNull();
    expect(post.querySelector(".session-link")).toBeNull();
  }
});
