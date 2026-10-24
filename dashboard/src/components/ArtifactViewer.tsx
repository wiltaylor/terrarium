// Preview for one artifact (§15.2, docs/api.md §12). The viewer mints a
// short-lived signed URL (`POST /api/artifacts/{id}/url`) and loads the
// bytes from the serving routes. HTML and bundles run only in a sandboxed
// iframe without allow-same-origin, so they cannot reach the dashboard.

import { createResource, Match, Show, Switch } from "solid-js";
import type { Artifact } from "../api/types";
import { renderMarkdown } from "../lib/markdown";
import { bytes } from "../lib/format";
import { api } from "../stores";
import { ErrorBanner } from "./ui";

/** What the viewer needs. A full artifact fits, and so does a version's artifact entry. */
export type ArtifactLike = Pick<Artifact, "id" | "uid" | "name" | "kind" | "content_type" | "size"> & { renders?: boolean; project?: string };

export type PreviewKind = "image" | "pdf" | "markdown" | "html" | "text" | "download";

export function previewKind(a: Pick<Artifact, "content_type" | "kind" | "name"> & { renders?: boolean }): PreviewKind {
  const t = a.content_type.toLowerCase();
  // A bundle always shows in the sandboxed frame (PLAN §15.2). The daemon
  // answers `renders: false` for bundles today, so `renders` is not read here.
  if (a.kind === "bundle") return "html";
  // For files, the daemon decides what may render (executables never do).
  if (a.renders === false) return "download";
  if (t.startsWith("text/html")) return "html";
  if (t.startsWith("image/")) return "image";
  if (t.startsWith("application/pdf")) return "pdf";
  if (t.startsWith("text/markdown") || /\.md$/i.test(a.name)) return "markdown";
  if (t.startsWith("text/") || /json|xml|yaml|toml|javascript|typescript/.test(t)) return "text";
  return "download";
}

export function ArtifactViewer(props: { artifact: ArtifactLike; compact?: boolean }) {
  const kind = () => previewKind(props.artifact);
  // The uid is unique across projects, so it is the safest key.
  const [signed] = createResource(() => props.artifact.uid || props.artifact.id, (key) => api.artifacts.url(key));
  const url = () => signed()?.url;
  const [text] = createResource(
    () => (kind() === "markdown" || kind() === "text" ? url() : null),
    (u) => api.artifacts.text(u),
  );
  return (
    <div class="stack" style={{ gap: "8px" }}>
      <Show when={!props.compact}>
        <div class="spread small">
          <span>
            <strong>{props.artifact.name}</strong> <span class="muted">{props.artifact.id}</span>
          </span>
          <span class="muted">
            {props.artifact.kind === "bundle" ? "bundle" : props.artifact.content_type} · {bytes(props.artifact.size)}
            <Show when={url()}>
              {(u) => (
                <>
                  {" · "}
                  <a href={u()} download={props.artifact.name} target="_blank" rel="noopener noreferrer">
                    {props.artifact.kind === "bundle" ? "Open" : "Download"}
                  </a>
                </>
              )}
            </Show>
          </span>
        </div>
      </Show>
      <ErrorBanner error={signed.error} />
      <Show when={url()} fallback={<Show when={!signed.error}><p class="muted small">Loading…</p></Show>}>
        {(u) => (
          <Switch>
            <Match when={kind() === "image"}>
              <img src={u()} alt={props.artifact.name} style={{ "max-width": "100%", "border-radius": "var(--radius-sm)" }} />
            </Match>
            <Match when={kind() === "pdf"}>
              <object data={u()} type="application/pdf" class="artifact-frame" style={{ "min-height": "420px" }}>
                <a href={u()} download={props.artifact.name}>
                  Download {props.artifact.name}
                </a>
              </object>
            </Match>
            <Match when={kind() === "html"}>
              <iframe class="artifact-frame" src={u()} sandbox="allow-scripts" referrerPolicy="no-referrer" title={props.artifact.name} />
            </Match>
            <Match when={kind() === "markdown"}>
              <div class="prose card-box artifact-text" innerHTML={renderMarkdown(text() ?? "")} />
            </Match>
            <Match when={kind() === "text"}>
              <pre class="artifact-text">{text() ?? "Loading…"}</pre>
            </Match>
            <Match when={kind() === "download"}>
              <div class="empty">
                No preview for {props.artifact.content_type}.{" "}
                <a href={u()} download={props.artifact.name}>
                  Download
                </a>
              </div>
            </Match>
          </Switch>
        )}
      </Show>
    </div>
  );
}
