// The Versions tab of the board screen (app/nav.ts BOARD_TABS): the version
// timeline of the project and the version of the URL (/p/:project/board/versions/:id).

import { A, useParams } from "@solidjs/router";
import { createMemo, createResource, createSignal, For, onCleanup, Show } from "solid-js";
import { ArtifactViewer } from "../../components/ArtifactViewer";
import { Markdown } from "../../components/Markdown";
import { Empty, ErrorBanner, Modal, PageHead } from "../../components/ui";
import type { Version } from "../../api/types";
import { parseList } from "../agents/form";
import { ago, humanize } from "../../lib/format";
import { renderWiki } from "../../lib/markdown";
import { api, events } from "../../stores";
import { VERSION_CHIP as CHIP } from "../board/versions";

export default function VersionsPage() {
  const params = useParams<{ project: string; id?: string }>();
  const [list, { refetch }] = createResource(() => params.project, (p) => api.versions.list(p));
  const current = () => list()?.find((v) => v.id === params.id || v.name === params.id) ?? (params.id ? undefined : list()?.[0]);
  const [creating, setCreating] = createSignal(false);
  // Live: version events of this project reload the list.
  const off = events.on((e) => {
    if (e.kind.startsWith("version.") && e.project === params.project) void refetch();
  });
  onCleanup(off);

  return (
    <>
      <PageHead title="Versions" level={2}>
        <button class="primary" onClick={() => setCreating(true)}>
          New version
        </button>
      </PageHead>
      <Show when={creating()}>
        <NewVersionModal project={params.project} onClose={() => setCreating(false)} onCreated={() => void refetch()} />
      </Show>
      <div class="split">
        <div class="card-box">
          <Show when={list()?.length} fallback={<span class="muted">No versions.</span>}>
            <ul class="timeline">
              <For each={list()}>
                {(v) => (
                  <li>
                    <A href={`/p/${params.project}/board/versions/${v.id}`} style={{ "font-weight": current()?.id === v.id ? 700 : 500 }}>
                      {v.name}
                    </A>{" "}
                    <span class={`chip ${CHIP[v.status]}`}>{humanize(v.status)}</span>
                    <div class="muted small">
                      {v.created_by_name ?? v.created_by} · {ago(v.created_at)}
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
  const [error, setError] = createSignal<unknown>(null);
  const previous = createMemo(() => props.all.filter((v) => v.id !== props.version.id && v.created_at < props.version.created_at)[0]);
  const [compare] = createResource(() => (previous() ? { a: props.version.id, b: previous()!.id } : null), (k) => api.versions.compare(k.a, k.b));
  const attached = () => props.version.artifacts;

  const act = async (action: "submit" | "approve" | "reject" | "release") => {
    try {
      await api.versions.transition(props.version.id, action, comment() || undefined);
      setComment("");
      setError(null);
      props.onChange();
    } catch (e) {
      setError(e);
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
          <input class="review-comment" placeholder="Comment (required to reject)" value={comment()} onInput={(e) => setComment(e.currentTarget.value)} aria-label="Review comment" />
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
                <A href={`/p/${props.version.project}/board?card=${c.id}`}>{c.id}</A> {c.title} <span class="chip">{humanize(c.column)}</span>
              </div>
            )}
          </For>
        </div>
        <div class="card-box">
          <h3>Compare to {previous()?.name ?? "previous"}</h3>
          <Show when={compare()} fallback={<span class="muted">{previous() ? "Loading…" : "No previous version."}</span>}>
            {(c) => (
              <div class="small stack" style={{ gap: "4px" }}>
                <div>Cards added: {c().cards.added.join(", ") || "none"}</div>
                <div>Cards removed: {c().cards.removed.join(", ") || "none"}</div>
                <div>Cards landed (done): {c().cards.landed.join(", ") || "none"}</div>
                <div>
                  Pages changed:{" "}
                  <For each={c().pages} fallback={<span>none</span>}>
                    {(pg) => (
                      <>
                        <A href={`/p/${props.version.project}/knowledge/wiki/${pg.path}`}>{pg.path}</A>
                        <span class="muted"> ({pg.from_rev ? `rev ${pg.from_rev} → ${pg.to_rev}` : `new, rev ${pg.to_rev}`}) </span>
                      </>
                    )}
                  </For>
                </div>
                <div>Artifacts added: {c().artifacts.added.join(", ") || "none"}</div>
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
                <strong style={{ color: "var(--text)" }}>{r.author_name ?? r.author}</strong> {r.decision} · {ago(r.created_at)}
              </div>
              <div>{r.comment}</div>
            </div>
          )}
        </For>
      </div>
    </div>
  );
}

function NewVersionModal(props: { project: string; onClose: () => void; onCreated: (v: Version) => void }) {
  const [name, setName] = createSignal("");
  const [notes, setNotes] = createSignal("");
  const [gitRef, setGitRef] = createSignal("");
  const [cards, setCards] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  return (
    <Modal title="New version" onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <form
        class="stack"
        onSubmit={async (e) => {
          e.preventDefault();
          try {
            const v = await api.versions.create(props.project, { name: name().trim(), notes: notes(), git_ref: gitRef().trim() || undefined, cards: parseList(cards()) });
            props.onCreated(v);
            props.onClose();
          } catch (err) {
            setError(err);
          }
        }}
      >
        <label>
          Name
          <input value={name()} onInput={(e) => setName(e.currentTarget.value)} required placeholder="v0.2.0" />
        </label>
        <label>
          Notes (Markdown, wikilinks work)
          <textarea value={notes()} onInput={(e) => setNotes(e.currentTarget.value)} />
        </label>
        <label>
          Git ref
          <input value={gitRef()} onInput={(e) => setGitRef(e.currentTarget.value)} placeholder="main@abc123" />
        </label>
        <label>
          Cards (comma separated)
          <input value={cards()} onInput={(e) => setCards(e.currentTarget.value)} placeholder="TER-1, TER-2" />
        </label>
        <div>
          <button class="primary" type="submit">
            Create version
          </button>
        </div>
      </form>
    </Modal>
  );
}
