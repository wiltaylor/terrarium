// A screen with a title and a tab bar over the tab pages. Each tab is a
// route under `base`, so a tab has a URL and the browser back button moves
// between tabs. The tab page gives its own heading at level 2. The Settings
// and Logs screens use it, and their project variants too, and the Board and
// Knowledge screens of a project. A tab with the empty path is the index
// tab: its URL is `base`, and it is active on `base` only.

import { A, useLocation } from "@solidjs/router";
import { For, type JSX } from "solid-js";
import { PageHead } from "./ui";
import { decodePath } from "../lib/url";

export function TabScreen(props: { title: JSX.Element; label: string; base: string; tabs: readonly { path: string; label: string }[]; level?: 1 | 2; children?: JSX.Element }) {
  const location = useLocation();
  const active = (tab: { path: string }) => {
    const current = decodePath(location.pathname).replace(/\/$/, "");
    const target = decodePath(tab.path ? `${props.base}/${tab.path}` : props.base).replace(/\/$/, "");
    return tab.path ? current === target || current.startsWith(`${target}/`) : current === target;
  };
  return (
    <>
      <PageHead title={props.title} level={props.level} />
      {/* Links, not ARIA tabs: each tab is a page, and the current one has aria-current="page". */}
      <nav class="tabs" aria-label={props.label}>
        <For each={props.tabs}>
          {(t) => (
            <A href={t.path ? `${props.base}/${t.path}` : props.base} class={active(t) ? "active" : undefined} activeClass="" inactiveClass="">
              {t.label}
            </A>
          )}
        </For>
      </nav>
      {props.children}
    </>
  );
}
