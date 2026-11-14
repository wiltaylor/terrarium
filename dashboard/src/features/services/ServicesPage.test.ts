// @vitest-environment jsdom
// The Services page (TER-1269): a service card shows the definition, the
// count of each state, the requests, and cancel only on an open request.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Service, ServiceRequest } from "../../api/types";

const requests = vi.fn();
const cancel = vi.fn();
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({ api: { services: { requests, cancel } } });
});

const { ServiceCard } = await import("./ServicesPage");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
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

const settle = () => new Promise((r) => setTimeout(r, 0));

async function mount() {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(() => createComponent(ServiceCard, { service }), el);
  await settle();
  const buttons = (text: string) => [...el.querySelectorAll("button")].filter((b) => b.textContent?.trim().startsWith(text));
  return { el, buttons };
}

describe("a service card", () => {
  it("shows the definition, the state counts, and the requests", async () => {
    requests.mockResolvedValue([req("R2", "queued", { position: 1 }), req("R1", "failed", { exit_code: 2, error: "out of memory" })]);
    const { el } = await mount();
    expect(requests).toHaveBeenCalledWith("video-gen", { state: undefined, limit: 100 });
    expect(el.textContent).toContain("/opt/video/run");
    expect(el.textContent).toContain("1 GPU, exclusive");
    expect(el.textContent).toContain("its admins only");
    const chips = [...el.querySelectorAll("[aria-label=States] .chip")].map((c) => c.textContent);
    expect(chips).toEqual(["queued 1", "failed 1"]);
    expect(el.textContent).toContain("#1 in the queue");
    expect(el.textContent).toContain("exit 2: out of memory");
  });

  it("cancels an open request after the confirm, and offers no cancel on an ended one", async () => {
    requests.mockResolvedValue([req("R2", "running"), req("R1", "done")]);
    cancel.mockResolvedValue(req("R2", "cancelled"));
    const { buttons } = await mount();
    expect(buttons("Cancel request")).toHaveLength(1);
    buttons("Cancel request")[0]!.click();
    buttons("Yes, cancel")[0]!.click();
    await settle();
    expect(cancel).toHaveBeenCalledWith("R2");
    expect(requests).toHaveBeenCalledTimes(2);
  });

  it("shows the error of a refused cancel", async () => {
    requests.mockResolvedValue([req("R2", "queued")]);
    cancel.mockRejectedValue(new Error("service request `R2` has ended"));
    const { el, buttons } = await mount();
    buttons("Cancel request")[0]!.click();
    buttons("Yes, cancel")[0]!.click();
    await settle();
    expect(el.textContent).toContain("has ended");
  });

  it("asks the daemon for one state when the filter is set", async () => {
    requests.mockResolvedValue([]);
    const { el } = await mount();
    const select = el.querySelector<HTMLSelectElement>("select[aria-label='State of video-gen']")!;
    select.value = "timed_out";
    select.dispatchEvent(new Event("change"));
    await settle();
    expect(requests).toHaveBeenLastCalledWith("video-gen", { state: "timed_out", limit: 100 });
  });
});
