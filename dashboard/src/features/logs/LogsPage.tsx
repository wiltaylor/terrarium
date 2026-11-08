// The Logs screen: the mail log, the event log, and the audit log as tabs
// (app/nav.ts LOGS_TABS). Each tab is a route under /logs. Under a project
// (/p/:project/logs), the screen has the Mail and Events tabs only
// (PROJECT_LOGS_TABS), and each tab shows that project.

import { useParams, type RouteSectionProps } from "@solidjs/router";
import { TabScreen } from "../../components/TabScreen";
import { LOGS_TABS, PROJECT_LOGS_TABS } from "../../app/nav";

export default function LogsPage(props: RouteSectionProps) {
  const params = useParams<{ project?: string }>();
  return (
    <TabScreen
      title={params.project ? `Logs · ${params.project}` : "Logs"}
      label="Logs"
      base={params.project ? `/p/${encodeURIComponent(params.project)}/logs` : "/logs"}
      tabs={params.project ? PROJECT_LOGS_TABS : LOGS_TABS}
      level={params.project ? 2 : 1}
    >
      {props.children}
    </TabScreen>
  );
}
