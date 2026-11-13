import { useParams, useSearchParams } from "@solidjs/router";
import { useNavigate } from "../../instance/navigation";
import { projectBase } from "../../lib/url";
import { BoardPanel, type BoardQuery } from "./BoardPanel";
import { doneToggleHref } from "./done";

export function DoneCardsPage() { return <BoardPage done />; }
export function BoardPage(props: { done?: boolean }) {
  const params = useParams<{ project: string }>();
  const [search, setSearch] = useSearchParams<BoardQuery>();
  const navigate = useNavigate();
  return <BoardPanel project={params.project} done={props.done} query={search} onQueryChange={setSearch}
    onToggleDone={() => navigate(doneToggleHref(projectBase(params.project), !!props.done, search))} />;
}
