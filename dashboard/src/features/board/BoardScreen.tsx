// The Board screen of a project: the cards, the artifacts, and the schedules
// as tabs (app/nav.ts BOARD_TABS). Each tab is a route under /p/:project/board.
// The Cards tab is the index route, so the board keeps its URL and a `?card=`
// link opens the card drawer. The Cards tab filters and groups the cards by
// their version label (`?version=<name>&group=version`).

import { useParams, type RouteSectionProps } from "@solidjs/router";
import { TabScreen } from "../../components/TabScreen";
import { BOARD_TABS } from "../../app/nav";
import { decodePathParam, projectBase } from "../../lib/url";

export default function BoardScreen(props: RouteSectionProps) {
  const params = useParams<{ project: string }>();
  return (
    <TabScreen title={`Board · ${decodePathParam(params.project)}`} label="Board" base={`${projectBase(params.project)}/board`} tabs={BOARD_TABS} level={2}>
      {props.children}
    </TabScreen>
  );
}
