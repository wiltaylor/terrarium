// The tab row of a project: one route link per PROJECT_TABS entry. The project
// shell shows it on every /p/:project page, and the agent page shows it with
// Agents active (TER-153), since /agents/:id sits outside /p.

import { A, useLocation } from "@solidjs/router";
import { createEffect, For, on, Show } from "solid-js";
import { PROJECT_TABS } from "../../app/nav";
import type { TabBadge } from "./tabCounts";

/**
 * `base` is the encoded project URL, `/p/<name>`. `active` says which tab path is selected.
 * `badge` gives the count of a tab (TER-154): the words go into the accessible name and the tooltip, the number is aria-hidden.
 */
export function ProjectTabs(props: { base: string; active: (path: string) => boolean; badge?: (path: string) => TabBadge | null }) {
  const location = useLocation();
  let tabs!: HTMLElement;

  // A route change can select a tab outside the horizontal viewport. Keep the
  // active tab visible on a phone without scrolling the page on a desktop.
  createEffect(
    on(
      () => location.pathname,
      () => {
        if (typeof matchMedia === "function" && matchMedia("(max-width: 760px)").matches) {
          tabs.querySelector<HTMLElement>("a.active")?.scrollIntoView({ block: "nearest", inline: "nearest" });
        }
      },
    ),
  );

  return (
    // Route links, not an ARIA tablist: every tab is a page.
    <nav ref={tabs} class="tabs project-tabs" aria-label="Project">
      <For each={PROJECT_TABS}>
        {(tab) => (
          <A
            href={`${props.base}/${tab.path}`}
            class={props.active(tab.path) ? "active" : undefined}
            activeClass=""
            inactiveClass=""
            title={props.badge?.(tab.path)?.text}
          >
            {tab.label}
            <Show when={props.badge?.(tab.path)}>
              {(b) => (
                <>
                  <span class={b().cls} aria-hidden="true">
                    {b().n}
                  </span>
                  <span class="sr-only">, {b().text}</span>
                </>
              )}
            </Show>
          </A>
        )}
      </For>
    </nav>
  );
}
