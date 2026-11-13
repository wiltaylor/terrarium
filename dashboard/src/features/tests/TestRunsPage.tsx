import { useParams, useSearchParams } from "@solidjs/router";
import { TestRunsPanel, type TestRunsQuery } from "./TestRunsPanel";

export default function TestRunsPage() {
  const params = useParams<{ project: string }>();
  const [search, setSearch] = useSearchParams<TestRunsQuery>();
  return <TestRunsPanel project={params.project} query={search} onQueryChange={setSearch} />;
}
