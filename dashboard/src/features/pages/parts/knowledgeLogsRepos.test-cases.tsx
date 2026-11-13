// @vitest-environment jsdom
import { MemoryRouter, Route, createMemoryHistory } from "@solidjs/router";
import { createSignal, For, type JSX } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { AgentEvent, EventQuery, Memory, Message, PageWidget } from "../../../api/types";
import { createBookmarksStore } from "../../../stores/bookmarks";
import { waitFor } from "../../../test/wait";

type Call = { path: string; args: unknown[] };
const calls: Call[] = [];
const at = "2026-11-13T00:00:00Z";
const memory = (project: string, id: string): Memory => ({
  id, key: `${project}/${id}`, project, title: `${project} ${id}`, body: "body", tags: [], author: "operator", sources: [], supersedes: [], links: [], created_at: at, updated_at: at,
});
const message = (project: string, id: string): Message => ({
  id, thread_id: `T${id}`, from: "agent", from_kind: "agent", from_name: `${project}/lead`, to: "operator", to_kind: "operator", to_name: "operator",
  subject: `${project} subject`, body: "body", state: "delivered", created_at: at,
} as Message);
const event = (project: string, seq: number): AgentEvent => ({ project, seq, id: `${project}-${seq}`, kind: "mail", at, summary: `${project} event ${seq}`, data: {} });
const dbType = (project: string, name = "items") => ({ project, name, fields: [], records: 1, description: "", updated_at: at });
const UNSAFE = 'Safe text <script>window.pwned = 1</script> <img src="x" onerror="window.pwned = 1"> [bad](javascript:window.pwned=1)';

/** Read calls the parts may make. Anything else, a write too, is a failure. */
const handlers: Record<string, (...args: never[]) => unknown> = {
  "projects.repos": (p: string) => [{ id: `${p}-infra`, project: p, name: "infra", url: "https://example.com/infra.git" }],
  "projects.targets": () => [],
  "repos.trees": (id: string) => ({ repo_id: id, trees: [{ id: "main", kind: "main", label: "main" }] }),
  "repos.files": (_id: string, _tree: string, path: string) => ({ path, entries: [{ path: "README.md", name: "README.md", kind: "file" }], truncated: false }),
  "wiki.tree": (p: string) => [{ path: "index", title: `${p} index` }],
  "wiki.get": (key: string) => ({ id: key, path: "index", title: key, body: UNSAFE, rev: 1, updated_at: at, author_name: "operator", links: [] }),
  "wiki.backlinks": () => [],
  "wiki.revisions": () => [],
  "artifacts.list": () => [],
  "memories.list": (p: string) => [memory(p, "MEM-1"), memory(p, "MEM-2")],
  "memories.graph": (key: string) => ({ root: key, depth: 1, nodes: [memory(key.split("/")[0]!, "MEM-1")], edges: [] }),
  "db.types": (p: string) => [dbType(p)],
  "db.type": (p: string, name: string) => dbType(p, name),
  "db.query": (p: string, q: string) => ({ type: "items", query: q, view: "table", fields: [], records: [{ project: p, id: "REC-1", type: "items", data: {}, revision: 1, updated_at: at }], count: 1, labels: {} }),
  "mail.list": () => ({ items: [message("alpha", "01M7A000000000000000000001"), message("beta", "01M7A000000000000000000002")], next: null }),
  "events.history": (q: EventQuery) => ({ items: [event(q.project!, 1)], next: null }),
};
const READS = new Set(Object.keys(handlers));
const api = new Proxy({}, {
  get: (_api, group: string) => new Proxy({}, {
    get: (_group, method: string) => (...args: unknown[]) => {
      const path = `${group}.${method}`;
      calls.push({ path, args });
      const handler = handlers[path] as ((...a: unknown[]) => unknown) | undefined;
      return handler ? Promise.resolve(handler(...args)) : Promise.reject(new Error(`unexpected ${path}`));
    },
  }),
});
const listeners = new Set<(event: AgentEvent) => void>();
vi.mock("../../../stores", async () => {
  const { instanceTestStores } = await import("../../../test/instance");
  return instanceTestStores({
    api,
    bookmarks: createBookmarksStore(null, window.location.origin),
    projects: { prefixes: () => ({}), projects: () => [], selected: () => "foreign" },
    agents: { inProject: () => [] },
    repos: { inProject: () => [] },
    events: {
      status: () => ({ lastSeq: 1 }), state: () => "live",
      on: (fn: (event: AgentEvent) => void) => { listeners.add(fn); return () => listeners.delete(fn); },
    },
  });
});
const { KNOWLEDGE_LOGS_REPOS_KINDS, KNOWLEDGE_LOGS_REPOS_PARTS, KnowledgeLogsReposWidget, knowledgeLogsReposPart } = await import("./knowledgeLogsRepos");
const { WIDGET_REGISTRY } = await import("../widgetRegistry");

const widget = (id: string, kind: PageWidget["kind"], options?: PageWidget["options"]): PageWidget => ({ id, kind, x: 0, y: 0, w: 12, h: 3, ...(options ? { options } : {}) });
let dispose: (() => void) | undefined;
afterEach(() => { dispose?.(); dispose = undefined; document.body.replaceChildren(); calls.length = 0; listeners.clear(); delete (window as { pwned?: number }).pwned; });

function mount(view: () => JSX.Element) {
  const history = createMemoryHistory(); history.set({ value: "/host?q=foreign&id=foreign&open=foreign" });
  const root = document.createElement("div"); document.body.append(root);
  dispose = render(() => <MemoryRouter history={history}><Route path="/host" component={view} /></MemoryRouter>, root);
  return { root, history };
}
const input = (el: HTMLInputElement, value: string) => { el.value = value; el.dispatchEvent(new Event("input", { bubbles: true })); };
const callsOf = (path: string) => calls.filter((c) => c.path === path);

describe("registry contract", () => {
  it("covers exactly the registry parts of its kinds", () => {
    const registry = WIDGET_REGISTRY.filter((d) => (KNOWLEDGE_LOGS_REPOS_KINDS as readonly string[]).includes(d.kind)).map((d) => d.part).sort();
    expect(Object.keys(KNOWLEDGE_LOGS_REPOS_PARTS).sort()).toEqual(registry);
    expect(registry).toEqual(["db:records", "db:types", "events:events", "mail:messages", "memory:graph", "memory:list", "repos:files", "repos:list", "wiki_index:browser"].sort());
  });

  it("resolves only valid typed options of its own kinds", () => {
    expect(knowledgeLogsReposPart(widget("a", "repos"))).toBe("repos:list");
    expect(knowledgeLogsReposPart(widget("a", "memory", { view: "graph", search: "x" }))).toBe("memory:graph");
    expect(knowledgeLogsReposPart(widget("a", "board", { view: "lanes" }))).toBeUndefined();
    expect(knowledgeLogsReposPart(widget("a", "wiki"))).toBeUndefined();
    expect(knowledgeLogsReposPart(widget("a", "repos", { view: "lanes" as never }))).toBeUndefined();
    expect(knowledgeLogsReposPart(widget("a", "db", { view: "records", project: "beta" } as never))).toBeUndefined();
    expect(knowledgeLogsReposPart(widget("a", "mail", { search: "x".repeat(257) }))).toBeUndefined();
  });

  it("shows Not Found and loads nothing for a foreign, unknown or invalid widget", async () => {
    const invalid = [widget("a", "board"), widget("b", "markdown"), widget("c", "repos", { view: "graph" as never }), widget("d", "events", { search: "x", range: "24h" } as never)];
    const { root } = mount(() => <For each={invalid}>{(w) => <section><KnowledgeLogsReposWidget project="alpha" widget={w} /></section>}</For>);
    await Promise.resolve();
    expect([...root.querySelectorAll("section")].map((s) => s.textContent)).toEqual(["Not Found", "Not Found", "Not Found", "Not Found"]);
    expect(calls).toEqual([]);
  });
});

describe("rendered parts", () => {
  it("renders every part for the host project only, sanitizes wiki content and makes no write", async () => {
    const all: PageWidget[] = [
      widget("r", "repos"), widget("f", "repos", { view: "files", search: "infra" }), widget("w", "wiki_index"),
      widget("ml", "memory"), widget("mg", "memory", { view: "graph" }), widget("dt", "db", { view: "types" }),
      widget("dr", "db", { view: "records", search: "type:items" }), widget("m", "mail"), widget("e", "events"),
    ];
    const { root, history } = mount(() => <For each={all}>{(w) => <section data-part={w.id}><KnowledgeLogsReposWidget project="alpha" widget={w} /></section>}</For>);
    const part = (id: string) => root.querySelector(`[data-part="${id}"]`)!;
    await waitFor(() => {
      expect(part("r").querySelector('a[href="/p/alpha/repos/infra"]')).toBeTruthy();
      expect(part("f").textContent).toContain("README.md");
      expect(part("w").querySelector("article")?.textContent).toContain("Safe text");
      expect(part("ml").textContent).toContain("alpha MEM-1");
      expect(part("mg").querySelector("marker")).toBeTruthy();
      expect(part("dt").textContent).toContain("items");
      expect(part("dr").querySelector("tbody tr")).toBeTruthy();
      expect(part("m").querySelectorAll("tr.log-row")).toHaveLength(1);
      expect(part("e").querySelector(".event-row")).toBeTruthy();
    });
    // Mail is filtered to the host project in the browser, as on the legacy project route.
    expect(part("m").textContent).toContain("alpha subject");
    expect(part("m").textContent).not.toContain("beta subject");
    expect(part("e").querySelector('[aria-label="Scope"]')).toBeNull();
    // The panels ignore the host URL query and keep it unchanged.
    expect(part("dr").querySelector<HTMLInputElement>('[aria-label="Query"]')!.value).toBe("type:items");
    expect(history.get()).toBe("/host?q=foreign&id=foreign&open=foreign");

    const article = part("w").querySelector("article")!;
    expect(article.querySelector("script, [onerror]")).toBeNull();
    expect(article.querySelector('a[href^="javascript:"]')).toBeNull();
    expect((window as { pwned?: number }).pwned).toBeUndefined();

    expect(calls.filter((c) => !READS.has(c.path))).toEqual([]);
    for (const c of calls) {
      const [first] = c.args as [unknown];
      if (["projects.repos", "projects.targets", "wiki.tree", "artifacts.list", "memories.list", "db.types", "db.type", "db.query"].includes(c.path)) expect(first, c.path).toBe("alpha");
      if (["wiki.get", "memories.graph"].includes(c.path)) expect(String(first).startsWith("alpha/"), c.path).toBe(true);
      if (["repos.trees", "repos.files"].includes(c.path)) expect(first, c.path).toBe("alpha-infra");
      if (c.path === "events.history") expect((first as EventQuery).project).toBe("alpha");
    }
  });

  it("shows Not Found for copied references the host project lacks, without asking another project", async () => {
    const copied = [widget("f", "repos", { view: "files", search: "missing" }), widget("d", "db", { view: "records", search: "type:missing status:open" })];
    const { root } = mount(() => <For each={copied}>{(w) => <section><KnowledgeLogsReposWidget project="alpha" widget={w} /></section>}</For>);
    await waitFor(() => expect([...root.querySelectorAll("section")].map((s) => s.textContent?.includes("Not Found"))).toEqual([true, true]));
    expect(callsOf("repos.trees")).toEqual([]);
    expect(callsOf("db.query")).toEqual([]);
    expect(calls.every((c) => c.args[0] === "alpha")).toBe(true);
  });
});

describe("instance state", () => {
  it("keeps duplicate instances independent and local state, DOM and focus through a layout refresh", async () => {
    const [first, setFirst] = createSignal(widget("one", "memory", { view: "list", search: "first" }));
    const second = widget("two", "memory", { view: "list", search: "second" });
    const { root } = mount(() => <><section><KnowledgeLogsReposWidget project="alpha" widget={first()} /></section><section><KnowledgeLogsReposWidget project="alpha" widget={second} /></section></>);
    const searches = () => [...root.querySelectorAll<HTMLInputElement>('[aria-label="Search"]')];
    await waitFor(() => expect(searches().map((s) => s.value)).toEqual(["first", "second"]));
    await waitFor(() => expect(callsOf("memories.list").map((c) => (c.args[1] as { q?: string }).q)).toEqual(expect.arrayContaining(["first", "second"])));

    const box = searches()[0]!;
    box.focus(); input(box, "typed");
    await waitFor(() => expect(callsOf("memories.list").some((c) => (c.args[1] as { q?: string }).q === "typed")).toBe(true));
    expect(searches()[1]!.value).toBe("second");
    const before = calls.length;

    // A layout refresh: a new object of the same instance, moved and retitled.
    setFirst({ ...first(), y: 4, title: "Moved", options: { view: "list", search: "first" } });
    await Promise.resolve();
    expect(searches()[0]).toBe(box);
    expect(box.value).toBe("typed");
    expect(document.activeElement).toBe(box);
    expect(calls.length).toBe(before);

    // A changed configured search replaces the local query. A changed view remounts the part.
    setFirst({ ...first(), options: { view: "list", search: "edited" } });
    await waitFor(() => expect(searches()[0]!.value).toBe("edited"));
    setFirst({ ...first(), options: { view: "graph", search: "edited" } });
    await waitFor(() => expect(root.querySelector("section")!.querySelector("marker")).toBeTruthy());
    expect(searches()[1]!.value).toBe("second");
  });

  it("retains a DB query draft and live event rows through a layout refresh", async () => {
    const [records, setRecords] = createSignal(widget("dr", "db", { view: "records", search: "type:items" }));
    const [log, setLog] = createSignal(widget("ev", "events"));
    const { root } = mount(() => <><section><KnowledgeLogsReposWidget project="alpha" widget={records()} /></section><section><KnowledgeLogsReposWidget project="alpha" widget={log()} /></section></>);
    await waitFor(() => { expect(root.querySelector("tbody tr")).toBeTruthy(); expect(root.querySelector(".event-row")).toBeTruthy(); });
    const query = root.querySelector<HTMLInputElement>('[aria-label="Query"]')!;
    query.focus(); input(query, "type:items draft");
    const row = root.querySelector(".event-row")!;
    const before = { query: callsOf("db.query").length, history: callsOf("events.history").length };

    setRecords({ ...records(), y: 9 });
    setLog({ ...log(), title: "Events" });
    await Promise.resolve();
    expect(root.querySelector('[aria-label="Query"]')).toBe(query);
    expect(query.value).toBe("type:items draft");
    expect(document.activeElement).toBe(query);
    expect(root.querySelector(".event-row")).toBe(row);
    expect({ query: callsOf("db.query").length, history: callsOf("events.history").length }).toEqual(before);

    for (const fn of listeners) fn(event("beta", 2));
    for (const fn of listeners) fn(event("alpha", 2));
    await waitFor(() => expect(root.querySelectorAll(".event-row")).toHaveLength(2));
    expect(root.textContent).not.toContain("beta event");
    expect([...root.querySelectorAll(".event-row")]).toContain(row);
    expect(query.value).toBe("type:items draft");
  });

  it("scopes requests to a changed host project and cleans up its subscriptions", async () => {
    const [project, setProject] = createSignal("alpha");
    mount(() => <><KnowledgeLogsReposWidget project={project()} widget={widget("e", "events")} /><KnowledgeLogsReposWidget project={project()} widget={widget("m", "memory")} /></>);
    await waitFor(() => expect(callsOf("events.history")).toHaveLength(1));
    const subscribed = listeners.size;
    calls.length = 0;
    setProject("beta");
    await waitFor(() => { expect(callsOf("events.history")).toHaveLength(1); expect(callsOf("memories.list").length).toBeGreaterThan(0); });
    expect((callsOf("events.history")[0]!.args[0] as EventQuery).project).toBe("beta");
    expect(callsOf("memories.list").every((c) => c.args[0] === "beta")).toBe(true);
    expect(listeners.size).toBe(subscribed);
    dispose!(); dispose = undefined;
    expect(listeners.size).toBe(0);
  });
});
