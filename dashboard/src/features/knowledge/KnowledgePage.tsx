// The Knowledge screen of a project: the wiki, the memory, and the DB as tabs
// (app/nav.ts KNOWLEDGE_TABS). Each tab is a route under
// /p/:project/knowledge. /p/:project/knowledge redirects to the Wiki tab.

import { useParams, type RouteSectionProps } from "@solidjs/router";
import { TabScreen } from "../../components/TabScreen";
import { KNOWLEDGE_TABS } from "../../app/nav";
import { decodePathParam, projectBase } from "../../lib/url";

export default function KnowledgePage(props: RouteSectionProps) {
  const params = useParams<{ project: string }>();
  return (
    <TabScreen title={`Knowledge · ${decodePathParam(params.project)}`} label="Knowledge" base={`${projectBase(params.project)}/knowledge`} tabs={KNOWLEDGE_TABS} level={2}>
      {props.children}
    </TabScreen>
  );
}
