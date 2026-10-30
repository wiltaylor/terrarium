// A screen with a title and a tab bar over the tab pages. Each tab is a
// route under `base`, so a tab has a URL and the browser back button moves
// between tabs. The tab page gives its own heading at level 2. The Settings
// and Logs screens use it, and their project variants too.

import { A } from "@solidjs/router";
import { For, type JSX } from "solid-js";
import { PageHead } from "./ui";

export function TabScreen(props: { title: JSX.Element; label: string; base: string; tabs: readonly { path: string; label: string }[]; children?: JSX.Element }) {
  return (
    <>
      <PageHead title={props.title} />
      {/* Links, not ARIA tabs: each tab is a page, and the current one has aria-current="page". */}
      <nav class="tabs" aria-label={props.label}>
        <For each={props.tabs}>
          {(t) => (
            <A href={`${props.base}/${t.path}`} activeClass="active">
              {t.label}
            </A>
          )}
        </For>
      </nav>
      {props.children}
    </>
  );
}
