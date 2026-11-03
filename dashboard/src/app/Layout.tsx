// The app shell: sidebar with Home, Inbox, Logs, and Schedules, the project
// tree, the Settings section, and the footer; on the right, the side bar of
// pinned conversations (./PinnedBar).

import { A, useLocation, type RouteSectionProps } from "@solidjs/router";
import { createEffect, createResource, createSignal, ErrorBoundary, For, on, onCleanup, onMount, Show } from "solid-js";
import { isMock } from "../api";
import { CriticalBanner } from "../components/CriticalBanner";
import { ErrorBanner, useNow } from "../components/ui";
import { duration } from "../lib/format";
import { agents, api, auth, events, inbox, pins, projects, signOutNow, subscriptions, theme } from "../stores";
import { badge } from "../features/inbox/model";
import { SETTINGS_ITEMS, TOP_ITEMS } from "./nav";
import { ProjectTree } from "./ProjectTree";
import { projectFromPath } from "./treeModel";
import { LoginScreen } from "./LoginScreen";
import { PinnedBar } from "./PinnedBar";

export function Layout(props: RouteSectionProps) {
  const location = useLocation();
  const [open, setOpen] = createSignal(false);
  const [health, { refetch }] = createResource(() => api.health().catch(() => null));

  onMount(() => {
    void projects.refresh().catch(() => undefined);
    void agents.refresh();
    void inbox.refresh();
    void pins.refresh();
    // The pins are on the daemon: read them again when the operator comes back (another browser may have changed them).
    const again = () => {
      if (document.visibilityState === "visible") void pins.refresh();
    };
    document.addEventListener("visibilitychange", again);
    window.addEventListener("focus", again);
    onCleanup(() => {
      document.removeEventListener("visibilitychange", again);
      window.removeEventListener("focus", again);
    });
    subscriptions.poll(isMock ? 5000 : 30_000);
    events.start();
    const t = setInterval(refetch, 30_000);
    onCleanup(() => clearInterval(t));
  });

  // The URL wins: /p/<name>/... selects that project.
  createEffect(() => {
    const name = projectFromPath(location.pathname);
    if (name && projects.selected() !== name) projects.select(name);
  });

  createEffect(() => {
    void location.pathname;
    setOpen(false);
    pins.setOverlay(false);
  });

  const inboxBadge = () => badge(inbox.counts());

  return (
    <div class="shell" classList={{ "has-pins": pins.pins().length > 0, "pins-collapsed": pins.collapsed() }}>
      <Show when={auth.needsToken()}>
        <LoginScreen overlay />
      </Show>
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

        <div class="nav-section">
          <For each={TOP_ITEMS}>
            {(item) => (
              <A href={item.href} activeClass="active" end={"end" in item}>
                <span>{item.label}</span>
                <Show when={item.href === "/inbox" && inboxBadge()}>
                  {(b) => (
                    <span class={b().cls} title={b().title}>
                      {b().n}
                    </span>
                  )}
                </Show>
              </A>
            )}
          </For>
        </div>

        <ProjectTree />

        {/* At the bottom, above the footer. The link is active on every /settings/... tab. */}
        <div class="nav-section nav-settings">
          <h4>Settings</h4>
          <For each={SETTINGS_ITEMS}>
            {(item) => (
              <A href={item.href} activeClass="active">
                <span>{item.label}</span>
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
            <button class="small ghost" onClick={() => void signOutNow()} title="End this session (DELETE /api/session)">
              sign out
            </button>
          </div>
          <Show when={auth.bearerFallback()}>
            <A href="/settings/account" class="small" title="This daemon has no cookie sessions, so the token stays in the browser">
              <span class="chip warn">bearer mode</span> no cookie sessions
            </A>
          </Show>
        </div>
      </nav>

      <div style={{ "min-width": 0 }}>
        <header class="topbar">
          <button class="ghost" onClick={() => setOpen(!open())} aria-label="Menu">
            ☰
          </button>
          <strong>Terrarium</strong>
          <span class="muted small">{projects.selected()}</span>
          <Show when={pins.pins().length}>
            {(n) => (
              <button class="ghost topbar-pins" onClick={() => pins.setOverlay(!pins.overlay())} aria-label={`Pinned conversations (${n()})`} aria-expanded={pins.overlay()}>
                Pinned {n()}
              </button>
            )}
          </Show>
        </header>
        <CriticalBanner />
        <ConnectionBanner />
        <main class="main">
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
      <PinnedBar />
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
