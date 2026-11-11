// @vitest-environment jsdom
import { createComponent, type Component } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, expect, it, vi } from "vitest";
import { InstanceProvider, createInstance, type Instance } from ".";
import { createAuthStore } from "../stores/auth";
import { ConfirmButton, Modal, PageHead } from "../components/ui";
import { ArtifactViewer } from "../components/ArtifactViewer";
import { waitFor } from "../test/wait";

vi.mock("../stores", async () => {
  const { instanceTestStores } = await import("../test/instance");
  return instanceTestStores({});
});
let dispose: (() => void) | undefined;
let instance: Instance | undefined;
afterEach(() => { dispose?.(); instance?.dispose(); document.body.replaceChildren(); vi.unstubAllGlobals(); });
function mount(component: Component) {
  instance = createInstance({ name: "lab", remoteOrigin: "https://lab.example", auth: createAuthStore() });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => createComponent(InstanceProvider, { instance: instance!, get children() { return createComponent(component, {}); } }), root);
  return root;
}

it("names the remote in confirmation controls, portal dialogs and page identity", () => {
  const root = mount(() => [
    createComponent(PageHead, { title: "Worker" }),
    createComponent(ConfirmButton, { label: "Delete worker", onConfirm: vi.fn() }),
    createComponent(Modal, { title: "Stop worker", onClose: vi.fn(), children: "Confirm" }),
  ]);
  expect(root.textContent).toContain("Remote: lab");
  root.querySelector("button")!.click();
  expect(root.textContent).toContain("Are you sure on lab?");
  expect(root.textContent).toContain("Yes, delete worker on lab");
  expect(document.querySelector('[role="dialog"]')?.getAttribute("aria-label")).toBe("Stop worker on lab");
});

it("shows remote artifacts as external links without fetching or embedding their bytes", async () => {
  const fetch = vi.fn(async (_url: string) => new Response('{"url":"/a/A1/report.html?sig=signed","exp":123,"expires_at":""}'));
  vi.stubGlobal("fetch", fetch);
  const root = mount(() => createComponent(ArtifactViewer, { artifact: { id: "ART-1", uid: "A1", name: "report.html", kind: "file", content_type: "text/html", size: 10 } }));
  await waitFor(() => expect(root.querySelector("a")?.href).toBe("https://lab.example/a/A1/report.html?sig=signed"));
  expect(root.textContent).toContain("external");
  expect(root.querySelector("iframe, img")).toBeNull();
  expect(fetch).toHaveBeenCalledTimes(1);
  expect(fetch.mock.calls[0]?.[0]).toBe("/api/remotes/lab/proxy/api/artifacts/A1/url");
});
