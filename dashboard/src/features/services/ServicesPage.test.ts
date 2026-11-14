// @vitest-environment jsdom
// The Services page (TER-1269): a service card shows the definition, the
// count of each state, the requests, and cancel only on an open request.
// The 15 s reload keeps an open details row, the state filter, and an open
// cancel confirm (rev-1269 finding 1). A plain caller sees only its own
// requests (finding 3).
import { MemoryRouter, Route } from "@solidjs/router";
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import type { Service, ServiceRequest } from "../../api/types";

const list = vi.fn();
const get = vi.fn();
const requests = vi.fn();
const cancel = vi.fn();
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api: { services: { list, get, requests, cancel } } });
});

const { default: ServicesPage, ServiceCard, REFRESH_MS } = await import("./ServicesPage");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
  vi.useRealTimers();
});

const service: Service = {
  name: "video-gen",
  kind: "script",
  target: "gpu-box",
  command: ["/opt/video/run"],
  resources: { gpus: 1, gpu_exclusive: true },
  scope: "global",
  source: "file",
  created_at: "",
  updated_at: "",
};

const req = (id: string, state: ServiceRequest["state"], over: Partial<ServiceRequest> = {}): ServiceRequest => ({
  id,
  service: "video-gen",
  caller: "agent:p/w1",
  state,
  request: { prompt: id },
  payload_size: 0,
  wait_until: "2026-11-15T00:00:00Z",
  created_at: "2026-11-14T00:00:00Z",
  ...over,
});

const settle = async () => {
  for (let i = 0; i < 5; i++) await Promise.resolve();
  await new Promise((r) => setTimeout(r, 0));
};

const buttonsIn = (el: Element) => (text: string) => [...el.querySelectorAll("button")].filter((b) => b.textContent?.trim().startsWith(text));

async function mountCard() {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(() => createComponent(ServiceCard, { service }), el);
  await settle();
  return { el, buttons: buttonsIn(el) };
}

describe("a service card", () => {
  it("shows the definition, the open count of the queue, the state counts, and the requests", async () => {
    get.mockResolvedValue({ ...service, open_requests: 7, may_call: true });
    requests.mockResolvedValue([req("R2", "queued", { position: 1 }), req("R1", "failed", { exit_code: 2, error: "out of memory" })]);
    const { el } = await mountCard();
    expect(requests).toHaveBeenCalledWith("video-gen", { state: undefined, limit: 100 });
    expect(el.textContent).toContain("/opt/video/run");
    expect(el.textContent).toContain("1 GPU, exclusive");
    expect(el.textContent).toContain("its admins only");
    const chips = [...el.querySelectorAll("[aria-label=States] .chip")].map((c) => c.textContent);
    expect(chips).toEqual(["7 open in the queue", "queued 1", "failed 1"]);
    expect(el.textContent).not.toContain("of the newest");
    expect(el.textContent).toContain("#1 in the queue");
    expect(el.textContent).toContain("exit 2: out of memory");
  });

  it("says that the counts are of the newest 100 when the load is full", async () => {
    get.mockResolvedValue(undefined);
    requests.mockResolvedValue(Array.from({ length: 100 }, (_, i) => req(`R${String(i).padStart(3, "0")}`, "done")));
    const { el } = await mountCard();
    expect(el.querySelector("[aria-label=States]")!.textContent).toContain("of the newest 100:");
  });

  it("cancels an open request after the confirm, and offers no cancel on an ended one", async () => {
    get.mockResolvedValue(undefined);
    requests.mockResolvedValue([req("R2", "running"), req("R1", "done")]);
    cancel.mockResolvedValue(req("R2", "cancelled"));
    const { buttons } = await mountCard();
    expect(buttons("Cancel request")).toHaveLength(1);
    buttons("Cancel request")[0]!.click();
    buttons("Yes, cancel")[0]!.click();
    await settle();
    expect(cancel).toHaveBeenCalledWith("R2");
    expect(requests).toHaveBeenCalledTimes(2);
  });

  it("shows the error of a refused cancel", async () => {
    get.mockResolvedValue(undefined);
    requests.mockResolvedValue([req("R2", "queued")]);
    cancel.mockRejectedValue(new Error("service request `R2` has ended"));
    const { el, buttons } = await mountCard();
    buttons("Cancel request")[0]!.click();
    buttons("Yes, cancel")[0]!.click();
    await settle();
    expect(el.textContent).toContain("has ended");
  });

  it("asks the daemon for one state when the filter is set", async () => {
    get.mockResolvedValue(undefined);
    requests.mockResolvedValue([]);
    const { el } = await mountCard();
    const select = el.querySelector<HTMLSelectElement>("select[aria-label='State of video-gen']")!;
    select.value = "timed_out";
    select.dispatchEvent(new Event("change"));
    await settle();
    expect(requests).toHaveBeenLastCalledWith("video-gen", { state: "timed_out", limit: 100 });
  });

  it("shows a plain caller only its own requests, each with its cancel", async () => {
    const m = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0, serviceCaller: { role: "member", project: "terrarium", name: "lead" } });
    get.mockImplementation((n: string) => m.services.get(n));
    requests.mockImplementation((n: string, q: never) => m.services.requests(n, q));
    const { el, buttons } = await mountCard();
    const callers = [...el.querySelectorAll("tbody tr td:nth-child(2)")].map((td) => td.textContent);
    expect(callers.length).toBeGreaterThan(0);
    expect(new Set(callers)).toEqual(new Set(["agent:terrarium/lead"]));
    const open = (await m.services.requests("video-gen")).filter((r) => ["queued", "waiting", "running"].includes(r.state));
    expect(buttons("Cancel request")).toHaveLength(open.length);
  });
});

describe("the reload", () => {
  it("keeps an open details row, the state filter, and an open cancel confirm", async () => {
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
    // Each answer is a new object, as from the daemon.
    list.mockImplementation(async () => [{ ...service }]);
    get.mockImplementation(async () => ({ ...service, open_requests: 2, may_call: true }));
    requests.mockImplementation(async (_n: string, q: { state?: string }) =>
      [req("R3", "running"), req("R2", "running"), req("R1", "done")].filter((r) => !q.state || r.state === q.state),
    );
    const el = document.createElement("div");
    document.body.append(el);
    dispose = render(
      () =>
        createComponent(MemoryRouter, {
          get children() {
            return createComponent(Route, { path: "*", component: ServicesPage });
          },
        }),
      el,
    );
    await settle();
    const buttons = buttonsIn(el);
    const select = el.querySelector<HTMLSelectElement>("select[aria-label='State of video-gen']")!;
    select.value = "running";
    select.dispatchEvent(new Event("change"));
    await settle();
    expect(buttons("Cancel request")).toHaveLength(2);
    const details = el.querySelector<HTMLButtonElement>("button[title=R2]")!;
    details.click();
    buttons("Cancel request")[0]!.click();
    expect(details.getAttribute("aria-expanded")).toBe("true");
    expect(buttons("Yes, cancel")).toHaveLength(1);
    const card = el.querySelector("article");

    vi.advanceTimersByTime(REFRESH_MS);
    await settle();

    expect(list).toHaveBeenCalledTimes(2);
    expect(requests).toHaveBeenLastCalledWith("video-gen", { state: "running", limit: 100 });
    expect(el.querySelector("article")).toBe(card);
    expect(el.querySelector<HTMLSelectElement>("select[aria-label='State of video-gen']")!.value).toBe("running");
    expect(el.querySelector("button[title=R2]")).toBe(details);
    expect(details.getAttribute("aria-expanded")).toBe("true");
    expect(el.textContent).toContain('"prompt": "R2"');
    expect(buttons("Yes, cancel")).toHaveLength(1);
  });
});
