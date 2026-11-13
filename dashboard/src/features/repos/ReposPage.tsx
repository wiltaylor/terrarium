import { useParams } from "@solidjs/router";
import { decodePathParam } from "../../lib/url";
import { ReposPanel } from "./ReposPanel";

export { repoHref } from "./ReposPanel";

export default function ReposPage() {
  const params = useParams<{ project: string }>();
  return <ReposPanel project={decodePathParam(params.project)} />;
}
