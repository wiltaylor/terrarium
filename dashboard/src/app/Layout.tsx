// The app shell: sidebar with the project picker, global items, and the footer.

import { A, useLocation, useNavigate, type RouteSectionProps } from "@solidjs/router";
import { createEffect, createResource, createSignal, ErrorBoundary, For, on, onCleanup, onMount, Show } from "solid-js";
import { isMock } from "../api";
import { QuotaBanner } from "../components/QuotaBanner";
import { ErrorBanner, useNow } from "../components/ui";
import { duration } from "../lib/format";
import { agents, api, events, projects, subscriptions, theme, unread } from "../stores";

const PROJECT_ITEMS = [
  { path: "board", label: "Board" },
  { path: "inbox", label: "Inbox" },
  { path: "wiki", label: "Wiki" },
  { path: "memory", label: "Memory" },
  { path: "versions", label: "Versions" },
  { path: "artifacts", label: "Artifacts" },
  { path: "spend", label: "Spend" },
  { path: "settings", label: "Settings & repos" },
] as const;

const GLOBAL_ITEMS = [
  { href: "/", label: "Home", end: true },
  { href: "/projects", label: "Projects" },
  { href: "/agents", label: "Agents" },
  { href: "/subscriptions", label: "Subscriptions" },
  { href: "/mail", label: "Mail" },
  { href: "/targets", label: "Targets" },
  { href: "/hosts", label: "Hosts" },
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
    void unread.refresh();
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

  createEffect(() => {
    const p = projects.selected();
    if (p) unread.watch(p);
  });

  const inboxCount = () => unread.inbox(projects.selected());
  const mailCount = () => unread.operator();
  const countText = (c: { n: number; more: boolean } | undefined) => (c ? `${c.n}${c.more ? "+" : ""}` : "");

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
            <For each={projects.projects()}>
              {(p) => (
                // `selected` per option: the select's value alone is lost when the options arrive later.
                <option value={p.name} selected={p.name === projects.selected()}>
                  {p.name}
                </option>
              )}
            </For>
          </select>
        </div>

        <Show when={projects.selected()}>
          <div class="nav-section">
            <For each={PROJECT_ITEMS}>
              {(item) => (
                <A href={projectHref(item.path)} activeClass="active">
                  <span>{item.label}</span>
                  <Show when={item.path === "inbox" && (inboxCount()?.n ?? 0) > 0}>
                    <span class="nav-count info" title="Unread in the project inbox">
                      {countText(inboxCount())}
                    </span>
                  </Show>
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
                <Show when={item.href === "/mail" && mailCount().n > 0}>
                  <span class="nav-count info" title="Unread mail to the operator">
                    {countText(mailCount())}
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
        <ConnectionBanner />
        <main class="main">
          <QuotaBanner />
          <ErrorBoundary
            fallback={(err, reset) => {
              // A view whose resource failed lands here. A new route resets it.
              createEffect(on(() => location.pathname, () => reset(), { defer: true }));
              return (
                <div class="stack">
                  <ErrorBanner error={err} />
                  <div>
                    <button onClick={reset}>Try again</button>
                  </div>
                </div>
              );
            }}
          >
            {props.children}
          </ErrorBoundary>
        </main>
      </div>
    </div>
  );
}

/** A strip across the top while the events socket is down. */
function ConnectionBanner() {
  const now = useNow(1000);
  const retry = () => {
    const at = events.status().retryAt;
    return at ? Math.max(0, Math.ceil((at - now()) / 1000)) : null;
  };
  return (
    <Show when={events.disconnected()}>
      <div class="conn-banner" role="status">
        <span>
          <strong>Disconnected</strong> from the daemon's event stream. Live updates are paused.
          <Show when={retry() !== null}> Retrying in {retry()}s.</Show>
          <Show when={events.state() === "connecting"}> Connecting…</Show>
        </span>
        <button class="small" onClick={() => events.reconnect()}>
          Reconnect now
        </button>
      </div>
    </Show>
  );
}
