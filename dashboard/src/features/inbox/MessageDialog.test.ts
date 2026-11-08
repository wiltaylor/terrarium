// @vitest-environment jsdom
// "Message an agent" in the Inbox (TER-46): the dialog sends one mail from
// the operator to the agent through the mock API (POST /api/mail), with the
// first line as the subject, and makes no watch: the Asked tab stays empty
// and the badge does not change.
import { createComponent } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import type { Message } from "../../api/types";

const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
const target = api.world.agents.find((a) => !a.ephemeral)!;
vi.mock("../../stores", () => ({
  api,
  agents: { state: { agents: [...api.world.agents, { id: "E1", project: "demo", name: "tmp", ephemeral: true }] } },
}));

const { MessageDialog } = await import("./MessageDialog");
const { messageBody } = await import("./model");

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  document.body.innerHTML = "";
});

const settle = () => new Promise((r) => setTimeout(r, 0));
const setValue = (sel: string, value: string, ev = "input") => {
  const e = document.querySelector(sel) as HTMLSelectElement | HTMLTextAreaElement;
  e.value = value;
  e.dispatchEvent(new Event(ev, { bubbles: true }));
};

describe("MessageDialog", () => {
  it("preselects the server default and allows clearing it", async () => {
    await api.account.setPreferences({ default_agent: `${target.project}/${target.name}` });
    const el = document.createElement("div");
    document.body.append(el);
    dispose = render(() => createComponent(MessageDialog, { onClose: () => {}, onSent: () => {} }), el);
    await settle();
    expect((document.querySelector("select") as HTMLSelectElement).value).toBe(`${target.project}/${target.name}`);
    setValue("select", "", "change");
    expect((document.querySelector("select") as HTMLSelectElement).value).toBe("");
    await api.account.setPreferences({ default_agent: null });
  });
  it("sends one mail to the agent and makes no watch", async () => {
    const who = `${target.project}/${target.name}`;
    const before = await api.inbox.counts();
    const asked = await api.inbox.list({ kind: "watch", status: "all" });
    const sent = vi.fn<(m: Message) => void>();
    const el = document.createElement("div");
    document.body.append(el);
    dispose = render(() => createComponent(MessageDialog, { onClose: () => {}, onSent: sent }), el);

    // The picker leaves the ephemeral agent out.
    const options = [...document.querySelectorAll("select[aria-label=Agent] option")].map((o) => o.textContent);
    expect(options).toContain(who);
    expect(options).not.toContain("demo/tmp");

    setValue("select[aria-label=Agent]", who, "change");
    setValue("textarea[aria-label='Message text']", "Please rebase on main.\nThe release is out.");
    (document.querySelector("form[aria-label=Message]") as HTMLFormElement).requestSubmit();
    await vi.waitFor(() => expect(sent).toHaveBeenCalledTimes(1));
    const m = sent.mock.calls[0]![0];
    expect(m.from_kind).toBe("operator");
    expect(m.to).toBe(target.id);
    expect(m.subject).toBe("Please rebase on main.");
    expect(m.body).toBe("Please rebase on main.\nThe release is out.");

    // One mail on the thread; no watch, so the Asked tab and the badge stay as they were.
    const thread = (await api.mail.list({ thread: m.thread_id })).items ?? [];
    expect(thread.length).toBe(1);
    expect(await api.inbox.list({ kind: "watch", status: "all" })).toEqual(asked);
    expect((await api.inbox.counts()).open).toBe(before.open);
    expect((await api.inbox.counts()).watch ?? 0).toBe(before.watch ?? 0);
    await settle();
  });

  it("asks for an agent and a text, and cuts a long subject", () => {
    expect(messageBody("", "x")).toEqual({ error: "Pick an agent." });
    expect(messageBody("p/w", "  ")).toEqual({ error: "Write the message." });
    const long = "a".repeat(100);
    const b = messageBody("p/w", `\n\n${long}\nmore`);
    expect("body" in b && b.body.subject.length).toBe(80);
    expect("body" in b && b.body.subject.endsWith("…")).toBe(true);
    expect("body" in b && b.body.body).toBe(`${long}\nmore`);
  });
});
