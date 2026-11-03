// The right-hand side bar of pinned conversations (docs/api.md §6.11): one
// tab for each pinned agent, and the open tab's Conversation (the same view
// and composer as the agent page). It stays while the operator browses other
// pages. On a wide screen it collapses to a rail; on a phone it is an
// overlay that the top bar opens.

import { A, useNavigate } from "@solidjs/router";
import { createResource, For, Show } from "solid-js";
import type { Agent, Pin } from "../api/types";
import { ErrorBanner } from "../components/ui";
import { Conversation } from "../features/agents/conversation/Conversation";
import { tabKeyTarget } from "../features/pins/model";
import { lifecycleBadge } from "../lib/lifecycle";
import { agents, api, pins } from "../stores";

/** The agent of a pin: from the live list, else one read (an agent the list does not have yet). */
function usePinAgent(id: () => string | null) {
  const [fetched] = createResource(
    () => {
      const v = id();
      return v && !agents.byId(v) ? v : null;
    },
    (key) => api.agents.get(key).catch(() => null),
  );
  return (): Agent | undefined => {
    const v = id();
    if (!v) return undefined;
    return agents.byId(v) ?? (fetched()?.id === v ? (fetched() ?? undefined) : undefined);
  };
}

/** The status class of a pin's dot, from the live agent list. */
const dotClass = (p: Pin) => {
  const a = agents.byId(p.agent);
  return `pin-dot-host st-${a?.status ?? "stopped"}${a?.paused ? " paused" : ""}`;
};
const pinTitle = (p: Pin) => {
  const a = agents.byId(p.agent);
  return `${p.project}/${p.name}${a ? `\n${lifecycleBadge(a).title}` : ""}`;
};

export function PinnedBar() {
  const navigate = useNavigate();
  const active = () => pins.active();
  const agent = usePinAgent(active);
  const activePin = () => pins.pins().find((p) => p.agent === active());
  let strip: HTMLDivElement | undefined;

  const unpin = (id: string) => void pins.unpin(id).catch(() => undefined);
  const close = () => pins.setOverlay(false);

  const onTabKey = (e: KeyboardEvent) => {
    const to = tabKeyTarget(pins.pins(), active(), e.key);
    if (!to) return;
    e.preventDefault();
    pins.select(to);
    strip?.querySelector<HTMLElement>(`[data-pin="${CSS.escape(to)}"]`)?.focus();
  };

  return (
    <Show when={pins.pins().length}>
      <Show when={pins.overlay()}>
        <div class="scrim pinbar-scrim" onClick={close} />
      </Show>
      <aside
        class="pinbar"
        classList={{ collapsed: pins.collapsed(), open: pins.overlay() }}
        aria-label="Pinned conversations"
        onKeyDown={(e) => {
          if (e.key === "Escape" && pins.overlay()) close();
        }}
      >
        <Show
          when={!pins.collapsed() || pins.overlay()}
          fallback={
            // The rail: one dot for each pin. A click opens the side bar on that tab.
            <div class="pinbar-rail">
              <button type="button" class="ghost small" title="Show the pinned conversations" aria-label="Show the pinned conversations" aria-expanded="false" onClick={() => pins.setCollapsed(false)}>
                ‹
              </button>
              <For each={pins.pins()}>
                {(p) => (
                  <button
                    type="button"
                    class={`ghost pinbar-rail-item ${dotClass(p)}`}
                    classList={{ active: p.agent === active() }}
                    title={pinTitle(p)}
                    aria-label={`Open ${p.project}/${p.name}`}
                    onClick={() => {
                      pins.select(p.agent);
                      pins.setCollapsed(false);
                    }}
                  >
                    <span class="status-dot" aria-hidden="true" />
                    <span class="pinbar-rail-name" aria-hidden="true">
                      {p.name.slice(0, 2)}
                    </span>
                  </button>
                )}
              </For>
            </div>
          }
        >
          <div class="pinbar-head">
            <div class="pinbar-tabs" role="tablist" aria-label="Pinned agents" ref={strip} onKeyDown={onTabKey}>
              <For each={pins.pins()}>
                {(p) => (
                  <span class={`pin-tab ${dotClass(p)}`} classList={{ active: p.agent === active() }}>
                    <button
                      type="button"
                      role="tab"
                      id={`pin-tab-${p.agent}`}
                      data-pin={p.agent}
                      aria-selected={p.agent === active()}
                      aria-controls="pinbar-panel"
                      tabIndex={p.agent === active() ? 0 : -1}
                      title={pinTitle(p)}
                      onClick={() => pins.select(p.agent)}
                    >
                      <span class="status-dot" aria-hidden="true" />
                      <span class="pin-tab-name">{p.name}</span>
                    </button>
                    <button type="button" class="pin-tab-close" title={`Unpin ${p.project}/${p.name}`} aria-label={`Unpin ${p.project}/${p.name}`} onClick={() => unpin(p.agent)}>
                      ×
                    </button>
                  </span>
                )}
              </For>
            </div>
            <Show
              when={pins.overlay()}
              fallback={
                <button type="button" class="ghost small" title="Collapse the side bar" aria-label="Collapse the side bar" aria-expanded="true" onClick={() => pins.setCollapsed(true)}>
                  ›
                </button>
              }
            >
              <button type="button" class="ghost small" aria-label="Close the pinned conversations" onClick={close}>
                ✕
              </button>
            </Show>
          </div>
          <ErrorBanner error={pins.error()} onDismiss={() => pins.clearError()} />
          <div class="pinbar-panel" id="pinbar-panel" role="tabpanel" aria-labelledby={active() ? `pin-tab-${active()}` : undefined}>
            <Show when={activePin()}>
              {(p) => (
                <div class="pinbar-title small">
                  <A href={`/agents/${encodeURIComponent(p().agent)}`} onClick={close}>
                    {p().project}/{p().name}
                  </A>
                </div>
              )}
            </Show>
            {/* One conversation at a time: each open one holds a transcript stream. A new tab mounts a new view. */}
            <Show when={agent()} keyed fallback={<p class="muted small">Loading…</p>}>
              {(a) => (
                <Conversation
                  agent={agents.byId(a.id) ?? a}
                  onOpenTerminal={() => {
                    close();
                    navigate(`/agents/${encodeURIComponent(a.id)}?tab=terminal`);
                  }}
                />
              )}
            </Show>
          </div>
        </Show>
      </aside>
    </Show>
  );
}

/** The pin toggle of an agent: on the agent page. */
export function PinButton(props: { agent: Pick<Agent, "id" | "project" | "name">; onError?: (e: unknown) => void; class?: string }) {
  const pinned = () => pins.isPinned(props.agent.id);
  return (
    <button
      type="button"
      class={props.class}
      aria-pressed={pinned()}
      title={pinned() ? "Remove the conversation from the pinned side bar" : "Keep the conversation in the side bar on the right, on every page"}
      onClick={() => void pins.toggle(props.agent.id).catch((e: unknown) => props.onError?.(e))}
    >
      {pinned() ? "Unpin" : "Pin"}
    </button>
  );
}
