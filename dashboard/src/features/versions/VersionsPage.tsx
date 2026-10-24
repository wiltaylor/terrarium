import { A, useParams } from "@solidjs/router";
import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import { ArtifactViewer } from "../../components/ArtifactViewer";
import { Markdown } from "../../components/Markdown";
import { Empty, ErrorBanner, PageHead } from "../../components/ui";
import type { Version, VersionStatus } from "../../api/types";
import { ago, humanize } from "../../lib/format";
import { renderWiki } from "../../lib/markdown";
import { api } from "../../stores";

const CHIP: Record<VersionStatus, string> = { draft: "", in_review: "warn", approved: "ok", rejected: "bad", released: "info" };

export default function VersionsPage() {
  const params = useParams<{ project: string; id?: string }>();
  const [list, { refetch }] = createResource(() => params.project, (p) => api.versions.list(p));
  const current = () => list()?.find((v) => v.id === params.id || v.name === params.id);

  return (
    <>
      <PageHead title={`Versions · ${params.project}`} />
      <div class="split">
        <div class="card-box">
          <Show when={list()?.length} fallback={<span class="muted">No versions.</span>}>
            <ul class="timeline">
              <For each={list()}>
                {(v) => (
                  <li>
                    <A href={`/p/${params.project}/versions/${v.id}`} style={{ "font-weight": current()?.id === v.id ? 700 : 500 }}>
                      {v.name}
                    </A>{" "}
                    <span class={`chip ${CHIP[v.status]}`}>{humanize(v.status)}</span>
                    <div class="muted small">
                      {v.created_by} · {ago(v.created_at)}
                    </div>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </div>
        <Show when={current()} fallback={<Empty>Pick a version from the timeline.</Empty>}>
          {(v) => <VersionView version={v()} all={list() ?? []} onChange={refetch} />}
        </Show>
      </div>
    </>
  );
}

function VersionView(props: { version: Version; all: Version[]; onChange: () => void }) {
  const [comment, setComment] = createSignal("");
  const [error, setError] = createSignal<string | null>(null);
  const [artifacts] = createResource(() => props.version.project, (p) => api.artifacts.list(p));
  const previous = createMemo(() => props.all.filter((v) => v.id !== props.version.id && v.created_at < props.version.created_at)[0]);
  const [compare] = createResource(() => (previous() ? { a: props.version.id, b: previous()!.id } : null), (k) => api.versions.compare(k.a, k.b));
  const attached = () => (artifacts() ?? []).filter((a) => props.version.artifacts.includes(a.id));

  const act = async (action: "submit" | "approve" | "reject" | "release") => {
    try {
      await api.versions.transition(props.version.id, action, comment() || undefined);
      setComment("");
      setError(null);
      props.onChange();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  return (
    <div class="stack">
      <div class="spread">
        <h2 style={{ margin: 0 }}>
          {props.version.name} <span class={`chip ${CHIP[props.version.status]}`}>{humanize(props.version.status)}</span>
        </h2>
        <span class="muted small mono">{props.version.git_ref ?? ""}</span>
      </div>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="card-box">
        <Markdown html={renderWiki(props.version.notes, { project: props.version.project })} />
      </div>

      <Show when={props.version.status === "in_review"}>
        <div class="review-bar">
          <input style={{ flex: 1, "min-width": "160px" }} placeholder="Comment (required to reject)" value={comment()} onInput={(e) => setComment(e.currentTarget.value)} aria-label="Review comment" />
          <button class="primary" onClick={() => act("approve")}>
            Approve
          </button>
          <button class="danger" disabled={!comment().trim()} onClick={() => act("reject")}>
            Reject
          </button>
        </div>
      </Show>
      <Show when={props.version.status === "draft" || props.version.status === "rejected"}>
        <div>
          <button onClick={() => act("submit")}>Submit for review</button>
        </div>
      </Show>
      <Show when={props.version.status === "approved"}>
        <div>
          <button class="primary" onClick={() => act("release")}>
            Release
          </button>
        </div>
      </Show>

      <div class="grid">
        <div class="card-box">
          <h3>Cards</h3>
          <For each={props.version.cards} fallback={<span class="muted">None.</span>}>
            {(c) => (
              <div>
                <A href={`/p/${props.version.project}/board?card=${c}`}>{c}</A>
              </div>
            )}
          </For>
        </div>
        <div class="card-box">
          <h3>Compare to {previous()?.name ?? "previous"}</h3>
          <Show when={compare()} fallback={<span class="muted">{previous() ? "Loading…" : "No previous version."}</span>}>
            {(c) => (
              <div class="small">
                <div>Cards: {c().cards.join(", ") || "none"}</div>
                <div>Pages: {c().pages.join(", ") || "none"}</div>
                <div>Artifacts: {c().artifacts.join(", ") || "none"}</div>
              </div>
            )}
          </Show>
        </div>
      </div>

      <Show when={attached().length}>
        <h3 style={{ margin: 0 }}>Artifacts</h3>
        <For each={attached()}>
          {(a) => (
            <div class="card-box">
              <ArtifactViewer artifact={a} />
            </div>
          )}
        </For>
      </Show>

      <div class="card-box">
        <h3>Reviews</h3>
        <For each={props.version.reviews} fallback={<span class="muted">No reviews yet.</span>}>
          {(r) => (
            <div class="comment">
              <div class="small muted">
                <strong style={{ color: "var(--text)" }}>{r.author}</strong> {r.decision} · {ago(r.created_at)}
              </div>
              <div>{r.comment}</div>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}
