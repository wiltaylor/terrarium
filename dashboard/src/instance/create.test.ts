// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { createAuthStore } from "../stores/auth";
import { createInstance, instanceHref, type Instance } from "./create";

const instances: Instance[] = [];
function instance(name?: string) {
  const result = createInstance({ name, auth: createAuthStore() });
  instances.push(result);
  return result;
}

afterEach(() => {
  instances.splice(0).forEach((i) => i.dispose());
  localStorage.clear();
  sessionStorage.clear();
  vi.unstubAllGlobals();
});

describe("instance scope", () => {
  it("isolates stores and browser preferences for identical daemon identifiers", async () => {
    const fetch = vi.fn(async () => new Response("[]"));
    vi.stubGlobal("fetch", fetch);
    const local = instance(), lab = instance("lab"), other = instance("other");
    for (const field of ["agents", "projects", "events", "inbox", "pins", "bookmarks", "cardPeek", "expanded"] as const) {
      expect(lab[field]).not.toBe(local[field]);
      expect(lab[field]).not.toBe(other[field]);
    }
    local.projects.select("local-project");
    lab.projects.select("remote-project");
    lab.pins.setCollapsed(true);
    lab.expanded.open("folder:F1");
    lab.session?.setItem("draft:Q1", "remote answer");
    expect(local.projects.selected()).toBe("local-project");
    expect(other.projects.selected()).toBeNull();
    expect(local.pins.collapsed()).toBe(false);
    expect(local.expanded.isOpen("folder:F1")).toBe(false);
    expect(local.session?.getItem("draft:Q1")).toBeNull();
    expect(other.session?.getItem("draft:Q1")).toBeNull();
    const reopened = instance("lab");
    expect(reopened.projects.selected()).toBe("remote-project");
    expect(reopened.pins.collapsed()).toBe(true);
    expect(reopened.session?.getItem("draft:Q1")).toBe("remote answer");
    await lab.projects.refresh();
    expect(fetch).toHaveBeenCalledWith("/api/remotes/lab/proxy/api/projects?limit=1000", expect.anything());
  });

  it("sends home CSRF to the proxy and does not end the home session on remote 401", async () => {
    const auth = createAuthStore();
    const remote = createInstance({ name: "lab", auth });
    instances.push(remote);
    auth.startSession({ csrf: "home-csrf" });
    const fetch = vi.fn(async () => new Response('{"error":"revoked remote link"}', { status: 401 }));
    vi.stubGlobal("fetch", fetch);
    await expect(remote.api.agents.send("A1", "hello")).rejects.toMatchObject({ status: 401 });
    expect(fetch.mock.calls[0]).toEqual([
      "/api/remotes/lab/proxy/api/agents/A1/send",
      expect.objectContaining({ credentials: "same-origin", headers: expect.objectContaining({ "X-CSRF-Token": "home-csrf" }) }),
    ]);
    expect(auth.mode()).toBe("session");
    expect(auth.expired()).toBe(false);
  });

  it("validates remote names before building a proxy path", () => {
    for (const name of ["../local", "x/y", "", "X", "lab?x=1", "a".repeat(33)]) {
      expect(() => instance(name)).toThrow("Invalid remote name");
    }
  });

  it("rebases dashboard paths once and leaves external links intact", () => {
    expect(instanceHref(undefined, "/p/demo/board")).toBe("/p/demo/board");
    expect(instanceHref("lab", "/p/demo/board?card=C1#comment")).toBe("/remote/lab/p/demo/board?card=C1#comment");
    expect(instanceHref("lab", "/remote/lab/p/demo/board")).toBe("/remote/lab/p/demo/board");
    for (const href of ["https://elsewhere.test/x", "//elsewhere.test/x", "#part", "mailto:a@example.test"]) {
      expect(instanceHref("lab", href)).toBe(href);
    }
    const lab = instance("lab");
    expect(lab.path("/remote/lab/p/demo/board")).toBe("/p/demo/board");
    expect(lab.path("/remote/lab")).toBe("/");
  });
});
