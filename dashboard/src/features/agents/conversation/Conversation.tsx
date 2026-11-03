// The Conversation tab of the agent page: the transcript of the agent's
// current session as blocks (docs/api.md §6, transcripts), with the live
// tail and the composer. It follows a new session. The Sessions tab opens
// the other sessions; the raw terminal stays on its own tab.
//
// Agent-written text is untrusted: see ./TranscriptLog.

import { createEffect, createMemo, createSignal, For, on, onCleanup, Show } from "solid-js";
import type { Agent, Session } from "../../../api/types";
import { StatusBadge } from "../../../components/StatusBadge";
import { ErrorBanner } from "../../../components/ui";
import { api } from "../../../stores";
import { currentSession } from "../sessions";
import { bytes } from "../../../lib/format";
import { canSend, imageProblem, imagesFromFiles, imagesFromItems, submitTurn, thumbnailOf, type DraftImage } from "./attachments";
import { composerState, isSendKey, sessionGone } from "./model";
import { createTranscriptView, StreamChip, TranscriptLog } from "./TranscriptLog";

const LIVE = ["starting", "idle", "waiting", "working", "blocked", "restarting"];

/** The newest session rows: enough to find the current one. */
const NEWEST = 5;

export function Conversation(props: { agent: Agent; onOpenTerminal: () => void }) {
  // ----- the current session -----
  const [session, setSession] = createSignal<Session | undefined>(undefined);
  const [loaded, setLoaded] = createSignal(false);
  const [listError, setListError] = createSignal<unknown>(null);
  let gen = 0;
  const loadCurrent = async () => {
    const mine = ++gen;
    const id = props.agent.id;
    try {
      const page = await api.agents.sessions(id, { limit: NEWEST });
      if (mine !== gen) return;
      const next = currentSession(page.items);
      // Keep the same object when the session did not change: the transcript keys on its ID.
      setSession((cur) => (cur && next && cur.id === next.id ? cur : next));
      setListError(null);
    } catch (e) {
      if (mine === gen) setListError(e);
    } finally {
      if (mine === gen) setLoaded(true);
    }
  };
  const live = createMemo(() => LIVE.includes(props.agent.status));
  // A new agent, a new harness session, or a start or a stop: read the current session again.
  createEffect(on([() => props.agent.id, () => props.agent.session, live], () => void loadCurrent()));
  onCleanup(() => gen++);

  const view = createTranscriptView(() => props.agent, session);
  const t = view.t;
  // The session went (a 404 from its page or its stream): read the current session again.
  // The same ID again keeps the same object, so this does not loop.
  createEffect(
    on(
      () => sessionGone(t.error()) || sessionGone(t.streamError()),
      (gone) => {
        if (gone) void loadCurrent();
      },
    ),
  );

  // The transcript's own `active` (its page, then `status` frames). When it cannot load, the list's word.
  const isActive = () => {
    if (!t.error()) return t.active();
    const s = session();
    return s?.active ?? (!!s?.current && live());
  };

  return (
    <section class="convo-main" aria-label="Conversation">
      <div class="convo-strip">
        <span class="row" style={{ gap: "6px" }}>
          <StatusBadge agent={props.agent} />
          <span class="muted small">
            {props.agent.harness}
            {props.agent.model ? ` · ${props.agent.model}` : ""}
          </span>
          <Show when={session()}>
            <StreamChip state={t.streamState()} ended={!t.active()} />
          </Show>
        </span>
        <button type="button" class="link small" onClick={() => props.onOpenTerminal()}>
          Open terminal
        </button>
      </div>
      <ErrorBanner error={listError()} />
      <TranscriptLog
        view={view}
        label="Conversation"
        empty={<p class="muted convo-note">{loaded() ? "No session yet. Send a turn: the agent wakes to run it." : "Loading…"}</p>}
      />
      <Composer agent={props.agent} sessionActive={isActive()} />
    </section>
  );
}

function Composer(props: { agent: Agent; sessionActive: boolean }) {
  const [text, setText] = createSignal("");
  const [images, setImages] = createSignal<DraftImage[]>([]);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [sent, setSent] = createSignal(false);
  const [dragging, setDragging] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let fileInput: HTMLInputElement | undefined;
  let nextId = 0;
  onCleanup(() => clearTimeout(timer));
  const state = () => composerState({ agentStatus: props.agent.status, sessionActive: props.sessionActive, paused: props.agent.paused });

  const patchImage = (id: string, patch: Partial<DraftImage>) =>
    setImages((cur) => cur.map((i) => (i.id === id ? { ...i, ...patch } : i)));
  const addImages = (files: File[]) => {
    if (!files.length || !state().enabled) return;
    const added = files.map((file): DraftImage => {
      const problem = imageProblem(file);
      return { id: `img-${++nextId}`, file, state: problem ? "failed" : "ready", error: problem ?? undefined };
    });
    setImages((cur) => [...cur, ...added]);
    for (const img of added) void thumbnailOf(img.file).then((thumb) => thumb && patchImage(img.id, { thumb }));
  };
  const removeImage = (id: string) => {
    setImages((cur) => cur.filter((i) => i.id !== id));
  };

  const send = async () => {
    if (busy() || !state().enabled || !canSend(text(), images())) return;
    setBusy(true);
    try {
      const id = props.agent.id;
      const r = await submitTurn(text(), images(), {
        attach: (file) => api.agents.attach(id, file),
        send: (t) => api.agents.send(id, t),
        onImage: patchImage,
      });
      if (!r.ok) {
        setError(r.error);
        return;
      }
      setImages([]);
      setText("");
      setError(null);
      setSent(true);
      clearTimeout(timer);
      timer = setTimeout(() => setSent(false), 6000);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      class="convo-composer"
      classList={{ "convo-drop": dragging() }}
      onSubmit={(e) => {
        e.preventDefault();
        void send();
      }}
      onDragOver={(e) => {
        if (!state().enabled || !e.dataTransfer?.types.includes("Files")) return;
        e.preventDefault();
        setDragging(true);
      }}
      onDragLeave={() => setDragging(false)}
      onDrop={(e) => {
        setDragging(false);
        const files = imagesFromFiles(e.dataTransfer?.files);
        if (!files.length) return;
        e.preventDefault();
        addImages(files);
      }}
    >
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <Show when={images().length}>
        <ul class="convo-images" aria-label="Attached images">
          <For each={images()}>
            {(img) => (
              <li class="convo-image" classList={{ failed: img.state === "failed" }} title={img.error ?? img.file.name}>
                <Show when={img.thumb} fallback={<span class="convo-image-blank" aria-hidden="true" />}>
                  <img src={img.thumb} alt={img.file.name || "pasted image"} />
                </Show>
                <span class="small muted">
                  {img.state === "uploading" ? "Uploading…" : img.state === "failed" ? "Failed" : bytes(img.file.size)}
                </span>
                <button type="button" class="convo-image-remove" aria-label={`Remove ${img.file.name || "image"}`} disabled={busy()} onClick={() => removeImage(img.id)}>
                  ×
                </button>
              </li>
            )}
          </For>
        </ul>
      </Show>
      <div class="convo-compose-row">
        <textarea
          rows={2}
          value={text()}
          disabled={!state().enabled}
          placeholder={state().enabled ? "Type a turn. Enter sends, Shift+Enter adds a line. Paste or drop images." : (state().reason ?? "")}
          aria-label="Turn text"
          onInput={(e) => setText(e.currentTarget.value)}
          onPaste={(e) => {
            const files = imagesFromItems(e.clipboardData?.items);
            if (!files.length) return;
            e.preventDefault();
            addImages(files);
          }}
          onKeyDown={(e) => {
            if (isSendKey(e)) {
              e.preventDefault();
              void send();
            }
          }}
        />
        <input
          ref={fileInput}
          type="file"
          accept="image/*"
          multiple
          hidden
          onChange={(e) => {
            addImages(imagesFromFiles(e.currentTarget.files));
            e.currentTarget.value = "";
          }}
        />
        <button type="button" disabled={!state().enabled || busy()} title="Attach image" onClick={() => fileInput?.click()}>
          Attach image
        </button>
        <button class="primary" type="submit" disabled={!state().enabled || busy() || !canSend(text(), images())}>
          {busy() ? "Sending…" : "Send"}
        </button>
      </div>
      <Show when={!state().enabled} fallback={
        <Show when={state().note || sent()}>
          <p class="small muted convo-compose-note">
            {state().note ?? ""}
            {state().note && sent() ? " · " : ""}
            {sent() ? "Sent. The turn shows here when the harness writes it." : ""}
          </p>
        </Show>
      }>
        <p class="small muted convo-compose-note">{state().reason}</p>
      </Show>
    </form>
  );
}
