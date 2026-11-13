import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { render } from "solid-js/web";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { createMockApi } from "../../api/mock";
import { waitFor } from "../../test/wait";

// TER-1098: two mail panels on one page that show the same message must not
// share DOM ids, and the legacy `/logs/mail?open=<id>#mail-<id>` link keeps
// its id and its scroll.
const api = createMockApi({ latencyMs: 0, eventsIntervalMs: 0, stepMs: 0 });
vi.mock("../../stores", async () => {
  const { instanceTestStores } = await import("../../test/instance");
  return instanceTestStores({
    api,
    agents: { state: { agents: api.world.agents } },
    projects: { projects: () => [{ name: "terrarium", card_prefix: "TER" }], prefixes: () => ({}) },
    events: { on: () => () => {} },
  });
});
const { default: MailPage, MailPanel } = await import("./MailPage");
let dispose: (() => void) | undefined;
let scrolled: string[] = [];
const nativeScroll = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollIntoView");
beforeEach(() => {
  scrolled = [];
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", { configurable: true, value(this: HTMLElement) { scrolled.push(this.id); } });
});
afterEach(() => {
  dispose?.(); dispose = undefined; document.body.replaceChildren();
  if (nativeScroll) Object.defineProperty(HTMLElement.prototype, "scrollIntoView", nativeScroll);
  else Reflect.deleteProperty(HTMLElement.prototype, "scrollIntoView");
});

const tick = () => new Promise((r) => setTimeout(r, 20));
const button = (scope: ParentNode, text: string) => [...scope.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent?.trim() === text)!;

/** Each Compose dialog: its recipient input must resolve to the datalist inside that same dialog. */
function expectBoundRecipientLists(count: number) {
  const dialogs = [...document.querySelectorAll<HTMLElement>('[role="dialog"]')];
  expect(dialogs).toHaveLength(count);
  const ids = dialogs.map((dialog) => {
    const input = dialog.querySelector<HTMLInputElement>("input[list]")!;
    const list = dialog.querySelector("datalist")!;
    expect(input.getAttribute("list")).toBe(list.id);
    expect(document.getElementById(list.id)).toBe(list);
    expect(document.querySelectorAll(`[id="${list.id}"]`)).toHaveLength(1);
    expect([...list.querySelectorAll("option")].map((o) => o.value)).toContain("project:terrarium");
    return list.id;
  });
  expect(new Set(ids).size).toBe(count);
}

it("gives two panels that open the same message their own post ids, anchors and recipient lists", async () => {
  const message = await api.mail.send({ to: "terrarium/lead", subject: "Twin panels", body: "same message" });
  const history = createMemoryHistory(); history.set({ value: `/host#mail-${message.id}` });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={() => <>
    <section><MailPanel project="terrarium" /></section><section><MailPanel project="terrarium" /></section>
  </>} /></MemoryRouter>, root);
  const sections = [...root.querySelectorAll("section")];
  const row = (s: Element) => [...s.querySelectorAll<HTMLTableRowElement>("tr.log-row")].find((r) => r.querySelector(".subject")?.textContent === "Twin panels");
  await waitFor(() => expect(sections.every((s) => row(s))).toBe(true));
  for (const s of sections) row(s)!.click();
  await waitFor(() => expect(sections.every((s) => s.querySelector(".log-thread .thread-post"))).toBe(true));

  const posts = sections.map((s) => s.querySelector<HTMLElement>(".log-thread .thread-post")!);
  const ids = posts.map((p) => p.id);
  expect(new Set(ids).size).toBe(2);
  for (const [i, id] of ids.entries()) {
    expect(id).toMatch(new RegExp(`^mail-panel-.+-${message.id}$`));
    expect(document.getElementById(id)).toBe(posts[i]);
  }
  expect(document.getElementById(`mail-${message.id}`)).toBeNull();
  // The permalink is still the legacy deep link to the mail log.
  for (const p of posts) expect(p.querySelector('[aria-label="Permalink to post 1"]')?.getAttribute("href")).toBe(`/logs/mail?open=${message.id}#mail-${message.id}`);
  // The legacy hash belongs to the mail log route: neither panel scrolls on it.
  await tick();
  expect(scrolled).toEqual([]);
  // A hash naming one panel's post scrolls only that post.
  history.set({ value: `/host#${ids[1]}` });
  await waitFor(() => expect(scrolled).toContain(ids[1]));
  expect(scrolled).not.toContain(ids[0]);

  for (const s of sections) button(s, "Compose").click();
  await waitFor(() => expect(document.querySelectorAll('[role="dialog"]')).toHaveLength(2));
  expectBoundRecipientLists(2);
});

it("keeps the legacy /logs/mail?open=<id>#mail-<id> id, scroll and recipient list", async () => {
  const message = await api.mail.send({ to: "terrarium/lead", subject: "Legacy link", body: "deep link" });
  const history = createMemoryHistory(); history.set({ value: `/logs/mail?open=${message.id}#mail-${message.id}` });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/logs/mail" component={MailPage} /></MemoryRouter>, root);
  await waitFor(() => expect(document.getElementById(`mail-${message.id}`)?.classList.contains("thread-post")).toBe(true));
  await waitFor(() => expect(scrolled).toContain(`mail-${message.id}`));
  const post = document.getElementById(`mail-${message.id}`)!;
  expect(root.querySelectorAll(`[id="mail-${message.id}"]`)).toHaveLength(1);
  expect(post.querySelector('[aria-label="Permalink to post 1"]')?.getAttribute("href")).toBe(`/logs/mail?open=${message.id}#mail-${message.id}`);
  button(root, "Compose").click();
  await waitFor(() => expect(document.querySelectorAll('[role="dialog"]')).toHaveLength(1));
  expectBoundRecipientLists(1);
});
