import { InstanceAnchor } from "../instance/navigation";
import { useInstance } from "../instance";
// Preview for one artifact (§15.2, docs/api.md §12). The viewer mints a
// short-lived signed URL (`POST /api/artifacts/{id}/url`) and loads the
// bytes from the artifact origin (docs/security-review.md F5). HTML, bundles,
// and PDFs run only in a sandboxed iframe without allow-same-origin, so they
// cannot reach the dashboard. Markdown goes through DOMPurify (SafeHtml).
// When the signed URL is on the dashboard origin, a badge says so.

import { createResource, Match, Show, Switch } from "solid-js";
import { artifactOrigin } from "../api";
import type { Artifact } from "../api/types";
import { artifactOriginKind, SAME_ORIGIN_WARNING } from "../lib/artifact-origin";
import { renderMarkdown } from "../lib/markdown";
import { SafeHtml } from "./SafeHtml";
import { bytes } from "../lib/format";

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
  const { api } = useInstance();
  const kind = () => previewKind(props.artifact);
  // The uid is unique across projects, so it is the safest key.
  const [signed] = createResource(() => props.artifact.uid || props.artifact.id, (key) => api.artifacts.url(key));
  const url = () => signed()?.url;
  const origin = () => {
    const u = url();
    return u ? artifactOriginKind(u, location.origin) : null;
  };
  // A separate origin that this build's CSP does not name: the browser blocks the preview.
  const notInCsp = () => {
    const u = url();
    if (!u || origin() !== "separate") return null;
    const o = new URL(u, location.origin).origin;
    return o === artifactOrigin ? null : o;
  };
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
                  <InstanceAnchor href={u()} download={props.artifact.name} target="_blank" rel="noopener noreferrer">
                    {props.artifact.kind === "bundle" ? "Open" : "Download"}
                  </InstanceAnchor
                </>
              )}
            </Show>
          </span>
        </div>
      </Show>
      <Show when={origin() === "same"}>
        <p class="small" role="status" style={{ margin: 0 }}>
          <span class="chip warn" title="Artifacts are untrusted. Serve them from their own origin (PLAN §15.2).">
            {SAME_ORIGIN_WARNING}
          </span>
        </p>
      </Show>
      <Show when={notInCsp()}>
        {(o) => (
          <p class="small" role="status" style={{ margin: 0 }}>
            <span class="chip warn">the dashboard CSP does not allow {o()}; build the dashboard with VITE_ARTIFACT_ORIGIN={o()}</span>
          </p>
        )}
      </Show>
      <ErrorBanner error={signed.error} />
      <ErrorBanner error={text.error} />
      <Show when={url()} fallback={<Show when={!signed.error}><p class="muted small">Loading…</p></Show>}>
        {(u) => (
          <Switch>
            <Match when={kind() === "image"}>
              <img src={u()} alt={props.artifact.name} referrerPolicy="no-referrer" style={{ "max-width": "100%", "border-radius": "var(--radius-sm)" }} />
            </Match>
            <Match when={kind() === "pdf"}>
              {/* No <object>: the CSP has object-src 'none'. Some browsers do not draw a PDF in a sandboxed frame; the link opens it. */}
              <iframe class="artifact-frame" src={u()} sandbox="allow-scripts" referrerPolicy="no-referrer" title={props.artifact.name} style={{ "min-height": "420px" }} />
              <InstanceAnchor class="small" href={u()} target="_blank" rel="noopener noreferrer">
                Open {props.artifact.name} in a new tab
              </InstanceAnchor
            </Match>
            <Match when={kind() === "html"}>
              <iframe class="artifact-frame" src={u()} sandbox="allow-scripts" referrerPolicy="no-referrer" title={props.artifact.name} />
            </Match>
            <Match when={kind() === "markdown"}>
              <SafeHtml class="prose card-box artifact-text" html={renderMarkdown(text() ?? "")} />
            </Match>
            <Match when={kind() === "text"}>
              <pre class="artifact-text">{text() ?? "Loading…"}</pre>
            </Match>
            <Match when={kind() === "download"}>
              <div class="empty">
                No preview for {props.artifact.content_type}.{" "}
                <InstanceAnchor href={u()} download={props.artifact.name}>
                  Download
                </InstanceAnchor
              </div>
            </Match>
          </Switch>
        )}
      </Show>
    </div>
  );
}
