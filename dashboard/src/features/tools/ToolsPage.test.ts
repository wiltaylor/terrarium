// @vitest-environment jsdom
// Settings > Tools (design D7): a tool card shows the definition with
// secret names only, grants to a grantee the scope allows, revokes, and
// deletes only an API tool.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Tool } from "../../api/types";

const grant = vi.fn();
const revoke = vi.fn();
const remove = vi.fn();
vi.mock("../../stores", () => ({
  api: { tools: { grant, revoke, remove } },
  projects: { projects: () => [{ name: "p" }, { name: "q" }] },
  agents: { state: { agents: [{ id: "A1", project: "p", name: "w1" }, { id: "A2", project: "q", name: "x1" }] }, refresh: vi.fn() },
}));

const { ToolCard } = await import("./ToolsPage");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
  vi.clearAllMocks();
});

const tool = (over: Partial<Tool> = {}): Tool => ({
  name: "deploy",
  target: "p-box",
  command: ["/opt/deploy", "--quiet"],
  project: "p",
  env_secrets: { TOKEN: "pt" },
  scope: "project",
  source: "api",
  created_at: "",
  updated_at: "",
  grants: [{ tool: "deploy", grantee: "project:p", grantee_label: "project:p", created_by: "operator", created_at: "" }],
  ...over,
});

function mount(t: Tool) {
  const onChange = vi.fn();
  const onError = vi.fn();
  const el = document.createElement("div");
  document.body.append(el);
  dispose = render(() => createComponent(ToolCard, { tool: t, onChange, onError }), el);
  const button = (text: string) => [...el.querySelectorAll("button")].find((b) => b.textContent?.trim().startsWith(text));
  return { el, button, onChange, onError };
}

const settle = () => new Promise((r) => setTimeout(r, 0));

describe("a tool card", () => {
  it("shows the definition with secret names, and offers only the tool's project", () => {
    const { el } = mount(tool());
    expect(el.textContent).toContain("/opt/deploy --quiet");
    expect(el.textContent).toContain("TOKEN = pt");
    const options = [...el.querySelectorAll("select[aria-label=Grantee] option")].map((o) => o.getAttribute("value"));
    expect(options).toEqual(["agent:A1"]);
  });

  it("grants, revokes, and deletes an API tool", async () => {
    grant.mockResolvedValue({});
    revoke.mockResolvedValue(undefined);
    remove.mockResolvedValue(undefined);
    const { button, onChange } = mount(tool());
    button("Grant")!.click();
    await settle();
    expect(grant).toHaveBeenCalledWith("deploy", "agent:A1");
    button("Revoke")!.click();
    button("Yes, revoke")!.click();
    await settle();
    expect(revoke).toHaveBeenCalledWith("deploy", "project:p");
    button("Delete")!.click();
    button("Yes, delete")!.click();
    await settle();
    expect(remove).toHaveBeenCalledWith("deploy");
    expect(onChange).toHaveBeenCalledTimes(3);
  });

  it("has no Delete for a file tool", () => {
    const { button } = mount(tool({ source: "file" }));
    expect(button("Delete")).toBeUndefined();
  });
});
