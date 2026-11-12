import { useParams, useSearchParams } from "@solidjs/router";
import { Navigate } from "../../instance/navigation";
import { createResource, Show } from "solid-js";
import { useInstance } from "../../instance";
import { ErrorBanner } from "../../components/ui";

export default function DashboardRedirect() {
  const params = useParams<{ project: string }>();
  const [search] = useSearchParams();
  const { api } = useInstance();
  const [pages] = createResource(() => search.tab !== "monitors" && params.project, p => api.pages.list(p));
  return <Show when={search.tab === "monitors"} fallback={<><ErrorBanner error={pages.error} /><Show when={pages.latest}><Navigate href={`/p/${params.project}/pages/${pages.latest?.dashboard_slug ?? "dashboard"}`} /></Show></>}><Navigate href={`/p/${params.project}/settings/monitors`} /></Show>;
}
