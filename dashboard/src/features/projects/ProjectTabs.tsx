// The tab row of a project: one route link per PROJECT_TABS entry. The project
// shell shows it on every /p/:project page, and the agent page shows it with
// Agents active (TER-153), since /agents/:id sits outside /p.

import { A, useLocation } from "@solidjs/router";
import { createEffect, For, on } from "solid-js";
import { PROJECT_TABS } from "../../app/nav";

/** `base` is the encoded project URL, `/p/<name>`. `active` says which tab path is selected. */
export function ProjectTabs(props: { base: string; active: (path: string) => boolean }) {
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
          <A href={`${props.base}/${tab.path}`} class={props.active(tab.path) ? "active" : undefined} activeClass="" inactiveClass="">
            {tab.label}
          </A>
        )}
      </For>
    </nav>
  );
}
