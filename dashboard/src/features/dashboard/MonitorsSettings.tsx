import { useParams } from "@solidjs/router";
import { createResource, onCleanup } from "solid-js";
import { useInstance } from "../../instance";
import { ErrorBanner, useNow } from "../../components/ui";
import { MonitorsPanel } from "./MonitorsPanel";

export default function MonitorsSettings() {
  const params = useParams<{ project: string }>();
  const { api, events } = useInstance();
  const now = useNow(5000);
  const [monitors, { refetch }] = createResource(() => params.project, p => api.monitors.list(p));
  onCleanup(events.on(e => { if (e.project === params.project && e.kind === "monitor.result") void refetch(); }));
  return <><ErrorBanner error={monitors.error} /><MonitorsPanel project={params.project} monitors={monitors.latest ?? []} now={now()} onChanged={() => void refetch()} /></>;
}
