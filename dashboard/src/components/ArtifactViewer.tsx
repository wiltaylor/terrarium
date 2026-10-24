// Preview for one artifact (§15.2). HTML and bundles run only in a sandboxed
// iframe without allow-same-origin, so they cannot reach the dashboard.

import { createResource, Match, Show, Switch } from "solid-js";
import type { Artifact } from "../api/types";
import { renderMarkdown } from "../lib/markdown";
import { bytes } from "../lib/format";
import { api } from "../stores";

export type PreviewKind = "image" | "pdf" | "markdown" | "html" | "text" | "download";

export function previewKind(a: Pick<Artifact, "content_type" | "kind" | "name">): PreviewKind {
  const t = a.content_type.toLowerCase();
  if (a.kind === "bundle" || t.startsWith("text/html")) return "html";
  if (t.startsWith("image/")) return "image";
  if (t === "application/pdf") return "pdf";
  if (t === "text/markdown" || /\.md$/i.test(a.name)) return "markdown";
  if (t.startsWith("text/") || /json|xml|yaml|toml|javascript|typescript/.test(t)) return "text";
  return "download";
}

export function ArtifactViewer(props: { artifact: Artifact; compact?: boolean }) {
  const kind = () => previewKind(props.artifact);
  const [text] = createResource(
    () => (kind() === "markdown" || kind() === "text" ? props.artifact : null),
    (a) => api.artifacts.text(a),
  );
  return (
    <div class="stack" style={{ gap: "8px" }}>
      <Show when={!props.compact}>
        <div class="spread small">
          <span>
            <strong>{props.artifact.name}</strong> <span class="muted">{props.artifact.id}</span>
          </span>
          <span class="muted">
            {props.artifact.content_type} · {bytes(props.artifact.size)} ·{" "}
            <a href={props.artifact.url} download={props.artifact.name} target="_blank" rel="noopener noreferrer">
              Download
            </a>
          </span>
        </div>
      </Show>
      <Switch>
        <Match when={kind() === "image"}>
          <img src={props.artifact.url} alt={props.artifact.name} style={{ "max-width": "100%", "border-radius": "var(--radius-sm)" }} />
        </Match>
        <Match when={kind() === "pdf"}>
          <object data={props.artifact.url} type="application/pdf" class="artifact-frame" style={{ "min-height": "420px" }}>
            <a href={props.artifact.url} download={props.artifact.name}>
              Download {props.artifact.name}
            </a>
          </object>
        </Match>
        <Match when={kind() === "html"}>
          <iframe class="artifact-frame" src={props.artifact.url} sandbox="allow-scripts" referrerPolicy="no-referrer" title={props.artifact.name} />
        </Match>
        <Match when={kind() === "markdown"}>
          { }
          <div class="prose card-box artifact-text" innerHTML={renderMarkdown(text() ?? "")} />
        </Match>
        <Match when={kind() === "text"}>
          <pre class="artifact-text">{text() ?? "Loading…"}</pre>
        </Match>
        <Match when={kind() === "download"}>
          <div class="empty">
            No preview for {props.artifact.content_type}.{" "}
            <a href={props.artifact.url} download={props.artifact.name}>
              Download
            </a>
          </div>
        </Match>
      </Switch>
    </div>
  );
}
