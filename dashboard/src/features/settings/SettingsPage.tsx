// The settings screen: the title and a tab bar over the tab pages. Each tab
// is a route under /settings (app/nav.ts SETTINGS_TABS), so a tab has a URL
// and the browser back button moves between tabs. The tab page gives its own
// heading at level 2.

import { A, type RouteSectionProps } from "@solidjs/router";
import { For } from "solid-js";
import { PageHead } from "../../components/ui";
import { SETTINGS_TABS } from "../../app/nav";

export default function SettingsPage(props: RouteSectionProps) {
  return (
    <>
      <PageHead title="Settings" />
      {/* Links, not ARIA tabs: each tab is a page, and the current one has aria-current="page". */}
      <nav class="tabs" aria-label="Settings">
        <For each={SETTINGS_TABS}>
          {(t) => (
            <A href={`/settings/${t.path}`} activeClass="active">
              {t.label}
            </A>
          )}
        </For>
      </nav>
      {props.children}
    </>
  );
}
