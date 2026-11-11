// @vitest-environment jsdom
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { waitFor } from "../../test/wait";
import type { Agent, AgentEvent, Question } from "../../api/types";

const ofAgent = vi.fn();
const refresh = vi.fn();
const off = vi.fn();
let event: (e: AgentEvent) => void;
vi.mock("../../stores", async () => { const { instanceTestStores } = await import("../../test/instance"); return instanceTestStores(({
  api: { questions: { ofAgent } },
  inbox: { refresh },
  events: { on: (fn: typeof event) => { event = fn; return off; } },
})); });

let change: (q: Question) => void;
vi.mock("./QuestionCard", () => ({ QuestionCard: (p: { question: Question; onChange: typeof change }) => {
  change = p.onChange;
  return p.question.text;
} }));
vi.mock("./BundleCard", () => ({ BundleCard: (p: { question: Question; onChange: typeof change }) => {
  change = p.onChange;
  return p.question.text;
} }));
const { QuestionsPanel } = await import("./QuestionsPanel");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});
const q = (id: string, status: Question["status"] = "open", kind: Question["kind"] = "question") => ({
  id, text: id, status, kind, priority: "normal", created_at: "2026-11-08T00:00:00Z",
} as Question);
const mount = async (roles = ["project_admin"]) => {
  dispose = render(() => createComponent(QuestionsPanel, { agent: { id: "A1", roles } as Agent }), document.body);
  await waitFor(() => expect(ofAgent).toHaveBeenCalled());
};

describe("agent open questions", () => {
  it("requests only open items and hides every completed status without a history toggle", async () => {
    ofAgent.mockResolvedValue([q("open-question"), q("open-notice", "open", "notice"), q("open-bundle", "open", "bundle"),
      q("answered", "answered"), q("acknowledged", "acknowledged", "notice"), q("retracted", "retracted")]);
    await mount();
    expect(ofAgent).toHaveBeenCalledWith("A1", { status: "open" });
    for (const id of ["open-question", "open-notice", "open-bundle"]) expect(document.body.textContent).toContain(id);
    for (const id of ["answered", "acknowledged", "retracted"]) expect(document.body.textContent).not.toContain(id);
    expect(document.querySelector("input[type=checkbox]")).toBeNull();
  });

  it.each(["answered", "acknowledged", "retracted"] as const)("removes an item immediately when it becomes %s", async (status) => {
    ofAgent.mockResolvedValue([q("pending")]);
    await mount();
    expect(document.body.textContent).toContain("pending");
    change(q("pending", status));
    expect(document.body.textContent).not.toContain("pending");
    expect(document.body.textContent).toContain("No open questions or notices.");
    expect(refresh).toHaveBeenCalledOnce();
  });

  it("refreshes on question events and hides an empty panel for a worker", async () => {
    ofAgent.mockResolvedValueOnce([q("pending")]).mockResolvedValue([]);
    await mount([]);
    expect(document.body.textContent).toContain("pending");
    event({ kind: "question", agent_id: "A1", data: { agent_id: "A1" } } as unknown as AgentEvent);
    await waitFor(() => expect(document.querySelector("section")).toBeNull());
    expect(ofAgent).toHaveBeenLastCalledWith("A1", { status: "open" });
  });
});
