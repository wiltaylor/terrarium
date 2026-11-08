// A screen with a title and a tab bar over the tab pages. Each tab is a
// route under `base`, so a tab has a URL and the browser back button moves
// between tabs. The tab page gives its own heading at level 2. The Settings
// and Logs screens use it, and their project variants too, and the Board and
// Knowledge screens of a project. A tab with the empty path is the index
// tab: its URL is `base`, and it is active on `base` only.

import { A } from "@solidjs/router";
import { For, type JSX } from "solid-js";
import { PageHead } from "./ui";

export function TabScreen(props: { title: JSX.Element; label: string; base: string; tabs: readonly { path: string; label: string }[]; level?: 1 | 2; children?: JSX.Element }) {
  return (
    <>
      <PageHead title={props.title} level={props.level} />
      {/* Links, not ARIA tabs: each tab is a page, and the current one has aria-current="page". */}
      <nav class="tabs" aria-label={props.label}>
        <For each={props.tabs}>
          {(t) => (
            <A href={t.path ? `${props.base}/${t.path}` : props.base} activeClass="active" end={!t.path}>
              {t.label}
            </A>
          )}
        </For>
      </nav>
      {props.children}
    </>
  );
}
