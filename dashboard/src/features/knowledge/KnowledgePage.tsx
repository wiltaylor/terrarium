// The Knowledge screen of a project: the wiki and the memory as tabs
// (app/nav.ts KNOWLEDGE_TABS). Each tab is a route under
// /p/:project/knowledge. /p/:project/knowledge redirects to the Wiki tab.

import { useParams, type RouteSectionProps } from "@solidjs/router";
import { TabScreen } from "../../components/TabScreen";
import { KNOWLEDGE_TABS } from "../../app/nav";

export default function KnowledgePage(props: RouteSectionProps) {
  const params = useParams<{ project: string }>();
  return (
    <TabScreen title={`Knowledge · ${params.project}`} label="Knowledge" base={`/p/${encodeURIComponent(params.project)}/knowledge`} tabs={KNOWLEDGE_TABS}>
      {props.children}
    </TabScreen>
  );
}
