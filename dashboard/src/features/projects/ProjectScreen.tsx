// The shell of every project page. The first path segment selects a project
// tab, so deep links and browser history keep working without a route change.

import { A, useLocation, useParams, type RouteSectionProps } from "@solidjs/router";
import { createEffect, For, on } from "solid-js";
import { PROJECT_TABS } from "../../app/nav";
import { PageHead } from "../../components/ui";
import { decodePath, decodePathParam, projectBase } from "../../lib/url";

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

  // Router params retain their URL encoding. Decode once for the title, then
  // encode the name once for every tab URL (including names with spaces).
  const project = () => decodePathParam(params.project);
  const base = () => projectBase(params.project);
  const active = (path: string) => {
    const current = decodePath(location.pathname).replace(/\/$/, "");
    const target = decodePath(`${base()}/${path}`);
    return current === target || current.startsWith(`${target}/`);
  };
  return (
    <>
      <PageHead title={project()} />
      {/* Route links, not an ARIA tablist: every tab is a page. */}
      <nav ref={tabs} class="tabs project-tabs" aria-label="Project">
        <For each={PROJECT_TABS}>
          {(tab) => (
            <A href={`${base()}/${tab.path}`} class={active(tab.path) ? "active" : undefined} activeClass="" inactiveClass="">
              {tab.label}
            </A>
          )}
        </For>
      </nav>
      {props.children}
    </>
  );
}
