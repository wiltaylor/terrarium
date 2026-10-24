import { A, useParams } from "@solidjs/router";
import { createResource, For, Show } from "solid-js";
import { ArtifactViewer } from "../../components/ArtifactViewer";
import { Empty, PageHead } from "../../components/ui";
import { ago, bytes } from "../../lib/format";
import { api } from "../../stores";

export default function ArtifactsPage() {
  const params = useParams<{ project: string; id?: string }>();
  const [list] = createResource(() => params.project, (p) => api.artifacts.list(p));
  const selected = () => list()?.find((a) => a.id === params.id) ?? list()?.[0];
  const total = () => (list() ?? []).reduce((n, a) => n + a.size, 0);

  return (
    <>
      <PageHead title={`Artifacts · ${params.project}`}>
        <span class="muted small">
          {list()?.length ?? 0} files · {bytes(total())} used
        </span>
      </PageHead>
      <div class="split">
        <div class="card-box thread-list" style={{ padding: "4px" }}>
          <For each={list()} fallback={<Empty>No artifacts.</Empty>}>
            {(a) => (
              <A href={`/p/${params.project}/artifacts/${a.id}`} style={{ display: "block", padding: "8px 6px", "border-bottom": "1px solid var(--border)", background: selected()?.id === a.id ? "var(--accent-soft)" : undefined }}>
                <div>
                  <span class="mono muted small">{a.id}</span> {a.name}
                </div>
                <div class="muted small">
                  {a.kind === "bundle" ? "bundle" : a.content_type} · {bytes(a.size)} · {a.created_by}, {ago(a.created_at)}
                </div>
              </A>
            )}
          </For>
        </div>
        <Show when={selected()}>
          {(a) => (
            <div class="stack">
              <ArtifactViewer artifact={a()} />
              <Show when={a().links.length}>
                <div class="small">
                  Attached to:{" "}
                  <For each={a().links}>
                    {(l) => (
                      <span class="chip" style={{ "margin-right": "4px" }}>
                        {l.kind}:{l.ref}
                      </span>
                    )}
                  </For>
                </div>
              </Show>
              <div class="muted small mono ellipsis">sha256 {a().sha256}</div>
            </div>
          )}
        </Show>
      </div>
    </>
  );
}
