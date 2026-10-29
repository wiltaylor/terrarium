// The settings screen: the title and a tab bar over the tab pages. Each tab
// is a route under /settings (app/nav.ts SETTINGS_TABS). The shell is
// components/TabScreen.tsx.

import type { RouteSectionProps } from "@solidjs/router";
import { TabScreen } from "../../components/TabScreen";
import { SETTINGS_TABS } from "../../app/nav";

export default function SettingsPage(props: RouteSectionProps) {
  return (
    <TabScreen title="Settings" label="Settings" base="/settings" tabs={SETTINGS_TABS}>
      {props.children}
    </TabScreen>
  );
}
