// The shell of every project page. The first path segment selects a project
// tab, so deep links and browser history keep working without a route change.

import { A, useLocation, useParams, type RouteSectionProps } from "@solidjs/router";
import { createEffect, For, on } from "solid-js";
import { PROJECT_TABS } from "../../app/nav";
import { PageHead } from "../../components/ui";

export default function ProjectScreen(props: RouteSectionProps) {
  const params = useParams<{ project: string }>();
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

  // Router params retain their URL encoding; encoding again would turn `%20`
  // into `%2520` and leave the project on tab navigation.
  const base = () => `/p/${params.project}`;
  return (
    <>
      <PageHead title={params.project} />
      {/* Route links, not an ARIA tablist: every tab is a page. */}
      <nav ref={tabs} class="tabs project-tabs" aria-label="Project">
        <For each={PROJECT_TABS}>
          {(tab) => (
            <A href={`${base()}/${tab.path}`} activeClass="active">
              {tab.label}
            </A>
          )}
        </For>
      </nav>
      {props.children}
    </>
  );
}
