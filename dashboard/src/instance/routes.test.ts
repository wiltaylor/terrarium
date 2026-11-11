// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createComponent, type Component } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { createAuthStore } from "../stores/auth";
import { createInstance, type Instance } from "./create";
import { waitFor } from "../test/wait";

vi.mock("../stores", async () => {
  const { instanceTestStores } = await import("../test/instance");
  return instanceTestStores({});
});

import { InstanceProvider, useInstance } from ".";
import { A, InstanceAnchor, useLocation, useNavigate } from "./navigation";
import ToolsPage from "../features/tools/ToolsPage";
import ReposPage from "../features/repos/ReposPage";
import TestRunsPage from "../features/tests/TestRunsPage";

const instances: Instance[] = [];
let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  instances.splice(0).forEach((i) => i.dispose());
  document.body.replaceChildren();
  vi.unstubAllGlobals();
});

function mount(instance: Instance, component: Component, route: string, url: string) {
  const root = document.createElement("div");
  document.body.append(root);
  const history = createMemoryHistory();
  history.set({ value: url });
  dispose = render(() => createComponent(InstanceProvider, {
    instance,
    get children() {
      return createComponent(MemoryRouter, {
        history,
        get children() { return createComponent(Route, { path: route, component }); },
      });
    },
  }), root);
  return { root, history };
}

it.each([
  ["/settings/tools", ToolsPage],
  ["/p/:project/repos", ReposPage],
  ["/p/:project/tests", TestRunsPage],
] as const)("keeps all requests of %s under the selected remote proxy", async (route, component) => {
  const fetch = vi.fn(async () => new Response("[]"));
  vi.stubGlobal("fetch", fetch);
  const instance = createInstance({ name: "lab", auth: createAuthStore() });
  instances.push(instance);
  mount(instance, component, `/remote/lab${route}`, `/remote/lab${route.replace(":project", "demo")}`);
  await waitFor(() => expect(fetch.mock.calls.length).toBeGreaterThan(0));
  await new Promise((resolve) => setTimeout(resolve, 10));
  for (const [url] of fetch.mock.calls as unknown as [string][]) expect(url).toMatch(/^\/api\/remotes\/lab\/proxy\/api\//);
});

it("captures the instance for delayed actions, and scopes links and navigation", async () => {
  const fetch = vi.fn(async () => new Response('{"accepted":true}'));
  vi.stubGlobal("fetch", fetch);
  const instance = createInstance({ name: "lab", auth: createAuthStore() });
  instances.push(instance);
  let send!: () => Promise<unknown>;
  let navigate!: ReturnType<typeof useNavigate>;
  let location!: ReturnType<typeof useLocation>;
  function Consumer() {
    const { api } = useInstance();
    send = () => api.agents.send("same-agent-id", "remote only");
    navigate = useNavigate();
    location = useLocation();
    return [
      createComponent(A, { href: "/p/demo/board?card=C1", children: "Board" }),
      createComponent(InstanceAnchor, { href: "/p/demo/knowledge/wiki/start", children: "Wiki" }),
    ];
  }
  const { root, history } = mount(instance, Consumer, "*", "/remote/lab/p/demo/board");
  expect(location.pathname).toBe("/p/demo/board");
  expect([...root.querySelectorAll("a")].map((a) => a.getAttribute("href"))).toEqual([
    "/remote/lab/p/demo/board?card=C1", "/remote/lab/p/demo/knowledge/wiki/start",
  ]);
  await send();
  expect(fetch).toHaveBeenCalledWith("/api/remotes/lab/proxy/api/agents/same-agent-id/send", expect.anything());
  navigate("/p/demo/tests");
  await waitFor(() => expect(history.get()).toBe("/remote/lab/p/demo/tests"));
});
