// @vitest-environment jsdom
import { Router, Route } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { waitFor } from "../test/wait";
import { Remotes } from "./Remotes";

vi.mock("../stores", async () => {
  const { instanceTestStores } = await import("../test/instance");
  return instanceTestStores({ auth: { token: () => "home-token", csrf: () => "home-csrf", mode: () => "bearer" } });
});
vi.mock("./ProjectTree", () => ({ ProjectTree: () => "Project tree" }));

let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); document.body.replaceChildren(); localStorage.clear(); vi.unstubAllGlobals(); window.history.replaceState({}, "", "/"); });

const lab = { name: "lab", label: "Lab", url: "https://lab.example", status: "online", read_only: false };
function mount() {
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => createComponent(Router, { get children() { return createComponent(Route, { path: "*", component: Remotes }); } }), root);
  return root;
}
function button(label: string) {
  return document.querySelector<HTMLButtonElement>(`button[aria-label="${label}"]`)!;
}
function input(label: string, value: string) {
  const el = Array.from(document.querySelectorAll("label")).find((l) => l.textContent?.startsWith(label))!.querySelector("input")!;
  el.value = value; el.dispatchEvent(new Event("input", { bubbles: true }));
}

it("pairs through home with CSRF, clears the one-time code and stores only the badge colour", async () => {
  const fetch = vi.fn(async (_url: string, init?: RequestInit) => new Response(JSON.stringify(init?.method === "POST" ? lab : [])));
  vi.stubGlobal("fetch", fetch); mount();
  button("Add remote").click();
  input("name", "lab"); input("url", lab.url); input("instance_id", "remote-id"); input("Pairing code", "one-time-code");
  document.querySelector("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await waitFor(() => expect(document.querySelector('[role="dialog"]')).toBeNull());
  const write = fetch.mock.calls.find(([, opts]) => opts?.method === "POST")!;
  expect(write[0]).toBe("/api/remotes");
  expect(new Headers(write[1]?.headers).get("X-CSRF-Token")).toBe("home-csrf");
  expect(new Headers(write[1]?.headers).get("Authorization")).toBe("Bearer home-token");
  expect(JSON.parse(write[1]?.body as string)).toMatchObject({ name: "lab", code: "one-time-code", instance_id: "remote-id" });
  expect(JSON.stringify(localStorage)).not.toContain("one-time-code");
  button("Add remote").click();
  expect(document.querySelector<HTMLInputElement>('input[type="password"]')?.value).toBe("");
});

it("requires the remote name before removing a home link", async () => {
  const fetch = vi.fn(async (_url: string, opts?: RequestInit) => new Response(JSON.stringify(opts?.method === "DELETE" ? {} : [lab])));
  vi.stubGlobal("fetch", fetch); mount();
  await waitFor(() => expect(button("Remove remote lab")).not.toBeNull());
  button("Remove remote lab").click();
  const remove = Array.from(document.querySelectorAll("button")).find((b) => b.textContent === "Remove remote")!;
  expect(remove.disabled).toBe(true);
  const field = document.querySelector<HTMLInputElement>('input[aria-label="Remote name to remove"]')!;
  field.value = "lab"; field.dispatchEvent(new Event("input", { bubbles: true }));
  expect(remove.disabled).toBe(false); remove.click();
  await waitFor(() => expect(fetch.mock.calls.some(([url, opts]) => url === "/api/remotes/lab" && opts?.method === "DELETE")).toBe(true));
});

it("expands a remote's own menu without sending management calls to the proxy", async () => {
  const fetch = vi.fn(async (url: string) => new Response(JSON.stringify(url === "/api/remotes?limit=64" ? [lab] : [])));
  vi.stubGlobal("fetch", fetch); const root = mount();
  await waitFor(() => expect(button("Expand remote lab")).not.toBeNull());
  button("Expand remote lab").click();
  await waitFor(() => expect(root.textContent).toContain("Project tree"));
  expect(Array.from(root.querySelectorAll(".remote-tree a"), (a) => a.getAttribute("href")).every((h) => h?.startsWith("/remote/lab/"))).toBe(true);
  expect(fetch.mock.calls.some(([url]) => url.startsWith("/api/remotes/lab/proxy/api/projects"))).toBe(true);
});
