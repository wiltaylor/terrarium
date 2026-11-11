// @vitest-environment jsdom
// Project actions (TER-53): a button per action, a confirm step that says
// what runs (an exec names its target and links the run history), then the
// run and its result. A disabled action is a disabled button with its
// reason; a refusal shows the daemon's message.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import type { Schedule } from "../../api/types";

const run = vi.fn();
vi.mock("../../stores", () => ({ api: { schedules: { run } } }));
vi.mock("@solidjs/router", () => ({
  A: (p: { href: string; children?: unknown }) => {
    const a = document.createElement("a");
    a.setAttribute("href", p.href);
    a.textContent = String(p.children ?? "");
    return a;
  },
}));

const { ActionButtons } = await import("./ActionButtons");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});

const sched = (over: Partial<Schedule>): Schedule =>
  ({
    id: "01SCH",
    name: "notify",
    project: "terra",
    enabled: true,
    spec: { kind: "manual" },
    action: { type: "mail", to: "terra/lead", subject: "Go", body: "" },
    catch_up: "skip",
    created_by: "operator",
    run_as: "operator",
    next_run_at: null,
    last_run: null,
    runs_count: 0,
    source: "api",
    created_at: "",
    updated_at: "",
    ...over,
  }) as Schedule;

const tick = () => new Promise((r) => setTimeout(r, 0));

function mount(actions: Schedule[], onRan?: () => void) {
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(() => createComponent(ActionButtons, { actions, onRan }), el);
  return el;
}

const button = (root: ParentNode, text: string) => [...root.querySelectorAll("button")].find((b) => b.textContent?.includes(text))!;
const dialog = () => document.querySelector<HTMLElement>("[role=dialog]");

describe("ActionButtons", () => {
  it("renders nothing without actions", () => {
    const el = mount([]);
    expect(el.querySelector(".action-buttons")).toBeNull();
  });

  it("asks first, then runs and shows the result with the run history link", async () => {
    run.mockResolvedValue({ id: "r1", schedule: "01SCH", status: "ok", summary: "mail 01M to terra/lead", error: null, trigger: "manual", started_at: "", finished_at: "", output: {} });
    const onRan = vi.fn();
    const el = mount([sched({})], onRan);
    const b = el.querySelector<HTMLButtonElement>("[data-action=notify]")!;
    expect(b.disabled).toBe(false);
    b.click();
    await tick();
    // The confirm step: nothing ran yet.
    expect(run).not.toHaveBeenCalled();
    const d = dialog()!;
    expect(d.getAttribute("aria-label")).toBe("Run notify?");
    expect(d.textContent).toContain("mail → lead: Go");
    expect(d.textContent).toContain("Runs as you (the operator).");
    expect(d.querySelector("[data-exec-note]")).toBeNull();
    button(d, "Run").click();
    await waitFor(() => expect(d.querySelector(".action-result")).not.toBeNull());
    expect(run).toHaveBeenCalledWith("01SCH");
    expect(onRan).toHaveBeenCalled();
    const r = d.querySelector(".action-result")!;
    expect(r.getAttribute("data-status")).toBe("ok");
    expect(r.textContent).toContain("mail 01M to terra/lead");
    expect(r.querySelector("a")!.getAttribute("href")).toBe("/p/terra/schedules?tab=actions&open=01SCH");
    // The Run button is gone; Close ends the dialog.
    expect([...d.querySelectorAll("button")].some((x) => x.textContent === "Run")).toBe(false);
    button(d, "Close").click();
    await tick();
    expect(dialog()).toBeNull();
  });

  it("cancel runs nothing", async () => {
    const el = mount([sched({})]);
    el.querySelector<HTMLButtonElement>("[data-action=notify]")!.click();
    await tick();
    button(dialog()!, "Cancel").click();
    await tick();
    expect(dialog()).toBeNull();
    expect(run).not.toHaveBeenCalled();
  });

  it("an exec names its target, its limit, and where the result goes", async () => {
    run.mockResolvedValue({ id: "r2", schedule: "01EX", status: "failed", summary: "exec on box: exit code 2", error: "exit code 2", trigger: "manual", started_at: "", finished_at: "", output: {} });
    const el = mount([sched({ id: "01EX", name: "deploy", action: { type: "exec", target: "box", cmd: ["./deploy.sh", "staging"], timeout_secs: 900 } })]);
    el.querySelector<HTMLButtonElement>("[data-action=deploy]")!.click();
    await tick();
    const note = dialog()!.querySelector("[data-exec-note]")!;
    expect(note.textContent).toContain("./deploy.sh staging");
    expect(note.textContent).toContain("on the target box");
    expect(note.textContent).toContain("15 min");
    expect(note.querySelector("a")!.getAttribute("href")).toBe("/p/terra/schedules?tab=actions&open=01EX");
    button(dialog()!, "Run").click();
    await waitFor(() => expect(dialog()!.querySelector(".action-result")).not.toBeNull());
    const r = dialog()!.querySelector(".action-result")!;
    expect(r.getAttribute("data-status")).toBe("failed");
    expect(r.textContent).toContain("exit code 2");
  });

  it("a disabled action is a disabled button with its reason", () => {
    const el = mount([sched({ enabled: false, disabled_reason: "3 runs skipped" })]);
    const b = el.querySelector<HTMLButtonElement>("[data-action=notify]")!;
    expect(b.disabled).toBe(true);
    expect(b.title).toBe("disabled: 3 runs skipped");
  });

  it("shows the daemon's refusal", async () => {
    run.mockRejectedValue(Object.assign(new Error("action `terra/notify` is disabled"), { status: 409 }));
    const el = mount([sched({})]);
    el.querySelector<HTMLButtonElement>("[data-action=notify]")!.click();
    await tick();
    button(dialog()!, "Run").click();
    await waitFor(() => expect(dialog()!.querySelector(".action-result")).not.toBeNull());
    const r = dialog()!.querySelector(".action-result")!;
    expect(r.getAttribute("data-status")).toBe("refused");
    expect(r.textContent).toContain("is disabled");
  });
});
