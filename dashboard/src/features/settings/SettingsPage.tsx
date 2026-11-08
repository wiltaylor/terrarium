// The settings screen: the title and a tab bar over the tab pages. Each tab
// is a route under /settings (app/nav.ts SETTINGS_TABS). Under a project
// (/p/:project/settings), the screen has the tabs of that project
// (PROJECT_SETTINGS_TABS). The shell is components/TabScreen.tsx.

import { useParams, type RouteSectionProps } from "@solidjs/router";
import { TabScreen } from "../../components/TabScreen";
import { PROJECT_SETTINGS_TABS, SETTINGS_TABS } from "../../app/nav";

export default function SettingsPage(props: RouteSectionProps) {
  const params = useParams<{ project?: string }>();
  return (
    <TabScreen
      title={params.project ? `Settings · ${params.project}` : "Settings"}
      label="Settings"
      base={params.project ? `/p/${encodeURIComponent(params.project)}/settings` : "/settings"}
      tabs={params.project ? PROJECT_SETTINGS_TABS : SETTINGS_TABS}
      level={params.project ? 2 : 1}
    >
      {props.children}
    </TabScreen>
  );
}
