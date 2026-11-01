// The Version modal of the board (docs/api.md §13): one version with its notes, cards, artifacts, review
// actions, archive, and review history; or a new version (create mode). The board opens it from a version
// row header, a version chip on a card, the toolbar version menu, and `?rows=version&version=<id>`. It
// replaces the old Versions tab of the board screen.

import { createMemo, createResource, createSignal, For, Show } from "solid-js";
import { ArtifactViewer } from "../../components/ArtifactViewer";
import { Markdown } from "../../components/Markdown";
import { ErrorBanner, Modal } from "../../components/ui";
import type { Card, Version } from "../../api/types";
import { ago, humanize } from "../../lib/format";
import { renderWiki } from "../../lib/markdown";
import { api } from "../../stores";
import { takesCards } from "./lanes";
import { VERSION_CHIP } from "./versions";
import { LinkedRuns } from "../tests/parts";

/** A list of the project's cards with check boxes and a search field. */
export function CardPicker(props: { cards: Card[]; selected: string[]; onChange: (ids: string[]) => void; label?: string }) {
  const [q, setQ] = createSignal("");
  const shown = () => {
    const t = q().trim().toLowerCase();
    return props.cards.filter((c) => !t || c.id.toLowerCase().includes(t) || c.title.toLowerCase().includes(t));
  };
  const toggle = (id: string, on: boolean) => props.onChange(on ? [...props.selected, id] : props.selected.filter((x) => x !== id));
  return (
    <fieldset class="card-picker">
      <legend class="small">{props.label ?? "Cards"}</legend>
      <input type="search" value={q()} onInput={(e) => setQ(e.currentTarget.value)} placeholder="Find a card by ID or title" aria-label="Find a card" />
      <ul>
        <For each={shown()} fallback={<li class="muted small">No cards.</li>}>
          {(c) => (
            <li>
              <label class="row small" style={{ gap: "6px" }}>
                <input type="checkbox" checked={props.selected.includes(c.id)} onChange={(e) => toggle(c.id, e.currentTarget.checked)} />
                <span class="mono muted">{c.id}</span> <span>{c.title}</span>
              </label>
            </li>
          )}
        </For>
      </ul>
    </fieldset>
  );
}

/** Create mode: a new version, with the cards to include. */
export function NewVersionModal(props: { project: string; cards: Card[]; onClose: () => void; onCreated: (v: Version) => void }) {
  const [name, setName] = createSignal("");
  const [notes, setNotes] = createSignal("");
  const [repo, setRepo] = createSignal("");
  const [gitRef, setGitRef] = createSignal("");
  const [cards, setCards] = createSignal<string[]>([]);
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal(false);
  return (
    <Modal title="New version" class="modal-full version-modal" onClose={() => props.onClose()}>
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <form
        class="stack"
        onSubmit={async (e) => {
          e.preventDefault();
          if (busy()) return;
          setBusy(true);
          try {
            const v = await api.versions.create(props.project, {
              name: name().trim(),
              notes: notes(),
              ...(repo().trim() ? { repo: repo().trim() } : {}),
              ...(gitRef().trim() ? { git_ref: gitRef().trim() } : {}),
              cards: cards(),
            });
            props.onCreated(v);
          } catch (err) {
            setError(err);
          } finally {
            setBusy(false);
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
        <div class="row">
          <label class="drawer-field">
            Repo (optional)
            <input value={repo()} onInput={(e) => setRepo(e.currentTarget.value)} placeholder="terrarium" />
          </label>
          <label class="drawer-field">
            Git ref (optional)
            <input value={gitRef()} onInput={(e) => setGitRef(e.currentTarget.value)} placeholder="main@abc123" />
          </label>
        </div>
        <CardPicker cards={props.cards} selected={cards()} onChange={setCards} label="Cards to include" />
        <div class="row">
          <button class="primary" type="submit" disabled={busy() || !name().trim()}>
            Create version
          </button>
          <button type="button" onClick={() => props.onClose()}>
            Cancel
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** View mode: one version. `all` is every version of the project, for the comparison with the one before. */
export function VersionModal(props: { version: Version; all: Version[]; cards: Card[]; onChanged: () => void; onOpenCard: (id: string) => void; onClose: () => void }) {
  const v = () => props.version;
  const [comment, setComment] = createSignal("");
  const [error, setError] = createSignal<unknown>(null);
  const [adding, setAdding] = createSignal(false);
  const [toAdd, setToAdd] = createSignal<string[]>([]);
  const previous = createMemo(() => props.all.filter((x) => x.id !== v().id && x.created_at < v().created_at).sort((a, b) => b.created_at.localeCompare(a.created_at))[0]);
  const [compare] = createResource(() => (previous() ? { a: v().id, b: previous()!.id } : null), (k) => api.versions.compare(k.a, k.b).catch(() => null));
  const editable = () => takesCards(v());
  const notIn = () => props.cards.filter((c) => !v().cards.some((x) => x.id === c.id));

  const run = async (f: () => Promise<unknown>) => {
    try {
      await f();
      setError(null);
      props.onChanged();
      return true;
    } catch (e) {
      setError(e);
      return false;
    }
  };
  const act = (action: "submit" | "approve" | "reject" | "release") =>
    run(async () => {
      await api.versions.transition(v().id, action, comment().trim() || undefined);
      setComment("");
    });

  return (
    <Modal title={`Version ${v().name}`} class="modal-full version-modal" onClose={() => props.onClose()}>
      <div class="stack">
        <div class="spread">
          <div class="row" style={{ gap: "6px" }}>
            <span class={`chip ${VERSION_CHIP[v().status]}`}>{humanize(v().status)}</span>
            <Show when={v().archived}>
              <span class="chip" title={v().archived_at ? `Archived ${ago(v().archived_at!)}` : undefined}>
                archived
              </span>
            </Show>
            <span class="muted small">
              {v().created_by_name ?? v().created_by} · {ago(v().created_at)}
            </span>
          </div>
          <span class="muted small mono">{[v().repo, v().git_ref].filter(Boolean).join(" @ ")}</span>
        </div>
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <div class="card-box">
          <Show when={v().notes.trim()} fallback={<span class="muted">No notes.</span>}>
            <Markdown html={renderWiki(v().notes, { project: v().project })} />
          </Show>
        </div>

        <Show when={v().status === "in_review"}>
          <div class="review-bar">
            <input class="review-comment" placeholder="Comment (required to reject)" value={comment()} onInput={(e) => setComment(e.currentTarget.value)} aria-label="Review comment" />
            <button class="primary" onClick={() => void act("approve")}>
              Approve
            </button>
            <button class="danger" disabled={!comment().trim()} onClick={() => void act("reject")}>
              Reject
            </button>
          </div>
        </Show>
        <div class="row">
          <Show when={v().status === "draft" || v().status === "rejected"}>
            <button onClick={() => void act("submit")}>Submit for review</button>
          </Show>
          <Show when={v().status === "approved"}>
            <button class="primary" onClick={() => void act("release")}>
              Release
            </button>
          </Show>
          <Show
            when={v().archived}
            fallback={
              <button title="The version leaves the board rows; its status and reviews stay." onClick={() => void run(() => api.versions.archive(v().id))}>
                Archive
              </button>
            }
          >
            <button onClick={() => void run(() => api.versions.unarchive(v().id))}>Unarchive</button>
          </Show>
        </div>

        <section class="card-box stack" style={{ gap: "6px" }}>
          <div class="spread">
            <h3 style={{ margin: 0 }}>Cards</h3>
            <Show when={editable()}>
              <button class="small" onClick={() => setAdding((a) => !a)} aria-expanded={adding()}>
                Add cards
              </button>
            </Show>
          </div>
          <Show when={!editable()}>
            <p class="muted small" style={{ margin: 0 }}>
              The cards of a version change in draft or after a rejection.
            </p>
          </Show>
          <Show when={adding()}>
            <CardPicker cards={notIn()} selected={toAdd()} onChange={setToAdd} label="Cards to add" />
            <div class="row">
              <button
                class="primary small"
                disabled={!toAdd().length}
                onClick={async () => {
                  if (await run(() => api.versions.update(v().id, { add_cards: toAdd() }))) {
                    setToAdd([]);
                    setAdding(false);
                  }
                }}
              >
                Add {toAdd().length || ""} {toAdd().length === 1 ? "card" : "cards"}
              </button>
              <button class="small" onClick={() => setAdding(false)}>
                Cancel
              </button>
            </div>
          </Show>
          <For each={v().cards} fallback={<span class="muted">None.</span>}>
            {(c) => (
              <div class="row small version-card" style={{ gap: "6px" }}>
                <button type="button" class="link mono" onClick={() => props.onOpenCard(c.id)}>
                  {c.id}
                </button>
                <span>{c.title}</span>
                <span class="chip">{humanize(props.cards.find((x) => x.id === c.id)?.column ?? c.column)}</span>
                <Show when={editable()}>
                  <button type="button" class="ghost small" aria-label={`Remove ${c.id} from ${v().name}`} title={`Remove ${c.id} from ${v().name}`} onClick={() => void run(() => api.versions.update(v().id, { remove_cards: [c.id] }))}>
                    ×
                  </button>
                </Show>
              </div>
            )}
          </For>
        </section>

        <div class="card-box">
          <h3>Compare to {previous()?.name ?? "previous"}</h3>
          <Show when={compare()} fallback={<span class="muted">{previous() ? "Loading…" : "No previous version."}</span>}>
            {(c) => (
              <div class="small stack" style={{ gap: "4px" }}>
                <div>Cards added: {c().cards.added.join(", ") || "none"}</div>
                <div>Cards removed: {c().cards.removed.join(", ") || "none"}</div>
                <div>Cards landed (done): {c().cards.landed.join(", ") || "none"}</div>
                <div>Pages changed: {c().pages.map((pg) => pg.path).join(", ") || "none"}</div>
                <div>Artifacts added: {c().artifacts.added.join(", ") || "none"}</div>
              </div>
            )}
          </Show>
        </div>

        <div class="card-box">
          <LinkedRuns project={v().project} version={v().id} />
        </div>

        <Show when={v().artifacts.length}>
          <h3 style={{ margin: 0 }}>Artifacts</h3>
          <For each={v().artifacts}>
            {(a) => (
              <div class="card-box">
                <ArtifactViewer artifact={a} />
              </div>
            )}
          </For>
        </Show>

        <div class="card-box">
          <h3>Reviews</h3>
          <For each={v().reviews} fallback={<span class="muted">No reviews yet.</span>}>
            {(r) => (
              <div class="comment">
                <div class="small muted">
                  <strong style={{ color: "var(--text)" }}>{r.author_name ?? r.author}</strong> {r.decision} · {ago(r.created_at)}
                </div>
                <Show when={r.comment}>
                  <div>{r.comment}</div>
                </Show>
              </div>
            )}
          </For>
        </div>
      </div>
    </Modal>
  );
}
