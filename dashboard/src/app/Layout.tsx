// The app shell: sidebar with the project picker, global items, and the footer.

import { A, useLocation, useNavigate, type RouteSectionProps } from "@solidjs/router";
import { createEffect, createResource, createSignal, For, onCleanup, onMount, Show } from "solid-js";
import { isMock } from "../api";
import { duration } from "../lib/format";
import { agents, api, events, projects, subscriptions, theme } from "../stores";

const PROJECT_ITEMS = [
  { path: "board", label: "Board" },
  { path: "inbox", label: "Inbox" },
  { path: "wiki", label: "Wiki" },
  { path: "memory", label: "Memory" },
  { path: "versions", label: "Versions" },
  { path: "artifacts", label: "Artifacts" },
] as const;

const GLOBAL_ITEMS = [
  { href: "/", label: "Home", end: true },
  { href: "/agents", label: "Agents" },
  { href: "/subscriptions", label: "Subscriptions" },
  { href: "/mail", label: "Mail" },
  { href: "/targets", label: "Targets & hosts" },
  { href: "/events", label: "Event log" },
  { href: "/audit", label: "Audit" },
] as const;

export function Layout(props: RouteSectionProps) {
  const location = useLocation();
  const navigate = useNavigate();
  const [open, setOpen] = createSignal(false);
  const [health, { refetch }] = createResource(() => api.health().catch(() => null));

  onMount(() => {
    void projects.refresh().catch(() => undefined);
    void agents.refresh();
    subscriptions.poll(isMock ? 5000 : 30_000);
    events.start();
    const t = setInterval(refetch, 30_000);
    onCleanup(() => clearInterval(t));
  });

  // The URL wins: /p/<name>/... selects that project.
  createEffect(() => {
    const m = /^\/p\/([^/]+)/.exec(location.pathname);
    if (m) {
      const name = decodeURIComponent(m[1]!);
      if (projects.selected() !== name) projects.select(name);
    }
  });

  createEffect(() => {
    void location.pathname;
    setOpen(false);
  });

  const pickProject = (name: string) => {
    projects.select(name);
    const m = /^\/p\/[^/]+\/([^/]+)/.exec(location.pathname);
    if (m) navigate(`/p/${encodeURIComponent(name)}/${m[1]}`);
  };

  const blockedCount = () => agents.inProject(projects.selected()).filter((a) => a.status === "blocked" || a.status === "error").length;
  const projectHref = (path: string) => `/p/${encodeURIComponent(projects.selected() ?? "")}/${path}`;

  return (
    <div class="shell">
      <Show when={open()}>
        <div class="scrim" onClick={() => setOpen(false)} />
      </Show>
      <nav class={`sidebar${open() ? " open" : ""}`} aria-label="Main">
        <div class="brand">
          <img src="/favicon.svg" alt="" />
          Terrarium
          <Show when={isMock}>
            <span class="mock-pill" title="VITE_MOCK=1: fake data, no daemon">mock</span>
          </Show>
        </div>

        <div class="nav-section project-picker">
          <h4>Project</h4>
          <select aria-label="Project" value={projects.selected() ?? ""} onChange={(e) => pickProject(e.currentTarget.value)}>
            <For each={projects.projects()}>{(p) => <option value={p.name}>{p.name}</option>}</For>
          </select>
        </div>

        <Show when={projects.selected()}>
          <div class="nav-section">
            <For each={PROJECT_ITEMS}>
              {(item) => (
                <A href={projectHref(item.path)} activeClass="active">
                  {item.label}
                </A>
              )}
            </For>
          </div>
        </Show>

        <div class="nav-section">
          <h4>Terrarium</h4>
          <For each={GLOBAL_ITEMS}>
            {(item) => (
              <A href={item.href} activeClass="active" end={"end" in item}>
                <span>{item.label}</span>
                <Show when={item.href === "/agents" && blockedCount() > 0}>
                  <span class="nav-count" title="Blocked or in error in this project">
                    {blockedCount()}
                  </span>
                </Show>
              </A>
            )}
          </For>
        </div>

        <div class="sidebar-foot">
          <div>
            <Show when={health()} fallback={<span class="chip bad">daemon unreachable</span>}>
              {(h) => (
                <span title="GET /api/health">
                  v{h().version} · up {duration(h().uptime * 1000)}
                </span>
              )}
            </Show>
          </div>
          <div class="row">
            <span class={`chip ${events.state() === "open" ? "ok" : events.state() === "connecting" ? "info" : "bad"}`}>events {events.state()}</span>
            <button class="small ghost" onClick={() => theme.cycle()} title="Theme: system, light, dark">
              theme: {theme.pref()}
            </button>
          </div>
        </div>
      </nav>

      <div style={{ "min-width": 0 }}>
        <header class="topbar">
          <button class="ghost" onClick={() => setOpen(!open())} aria-label="Menu">
            ☰
          </button>
          <strong>Terrarium</strong>
          <span class="muted small">{projects.selected()}</span>
        </header>
        <main class="main">{props.children}</main>
      </div>
    </div>
  );
}
