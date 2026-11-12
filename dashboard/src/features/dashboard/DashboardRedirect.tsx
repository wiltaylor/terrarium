import { Navigate, useParams, useSearchParams } from "@solidjs/router";

export default function DashboardRedirect() {
  const params = useParams<{ project: string }>();
  const [search] = useSearchParams();
  return <Navigate href={`/p/${params.project}/${search.tab === "monitors" ? "settings/monitors" : "pages/dashboard"}`} />;
}
