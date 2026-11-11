import { useInstance } from "../../../instance";
// The Conversation tab of the agent page: the transcript of the agent's
// current session as blocks (docs/api.md §6, transcripts), with the live
// tail and the composer. It follows a new session. The Sessions tab opens
// the other sessions; the raw terminal stays on its own tab.
//
// Agent-written text is untrusted: see ./TranscriptLog.

import { createEffect, createMemo, createSignal, For, on, onCleanup, Show, type JSX } from "solid-js";
import type { Agent, QueuedTurn, Session } from "../../../api/types";
import { ApiError } from "../../../api/http";
import { StatusBadge } from "../../../components/StatusBadge";
import { ErrorBanner } from "../../../components/ui";

import { currentSession } from "../sessions";
import { bytes } from "../../../lib/format";
import { canSend, imageProblem, imagesFromFiles, imagesFromItems, submitTurn, thumbnailOf, type DraftImage } from "./attachments";
import { canInterrupt, composerState, footerState, interruptNote, isSendKey, sessionGone } from "./model";
import { createTranscriptView, StreamChip, TranscriptLog } from "./TranscriptLog";

const LIVE = ["starting", "idle", "waiting", "working", "blocked", "restarting"];

/** The newest session rows: enough to find the current one. */
const NEWEST = 5;

/**
 * `onOpenTerminal`: a link in the header that opens the terminal. The pinned
 * bar passes it (its only way to the terminal); the agent page does not, as
 * its Terminal tab sits next to this one (issue #22). `footer`: shown under
 * the composer (the agent page's quota line).
 */
export function Conversation(props: { agent: Agent; onOpenTerminal?: () => void; footer?: JSX.Element }) {
  const { api } = useInstance();
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

  // The agent runs again in the same session (a wake resumes it): the
  // transcript that went inactive at the stop reads its session again, so
  // `active` and the stream come back (TER-44).
  createEffect(
    on(
      live,
      (on, was) => {
        if (on && was === false && session() && !t.active()) t.reopen();
      },
      { defer: true },
    ),
  );

  // The transcript's own `active` (its page, then `status` frames). When it cannot load, the list's word.
  // A queued turn that the operator takes back to edit: it goes into the composer (TER-54).
  const [draft, setDraft] = createSignal<{ text: string; n: number } | undefined>(undefined);

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
        <Show when={props.onOpenTerminal}>
          {(open) => (
            <button type="button" class="link small" onClick={() => open()()}>
              Open terminal
            </button>
          )}
        </Show>
      </div>
      <ErrorBanner error={listError()} />
      <TranscriptLog
        view={view}
        label="Conversation"
        empty={<p class="muted convo-note">{loaded() ? "No session yet. Send a turn: the agent wakes to run it." : "Loading…"}</p>}
      />
      <LiveLine agent={props.agent} />
      <QueuedTurns agent={props.agent} onEdit={(text) => setDraft({ text, n: (draft()?.n ?? 0) + 1 })} />
      {/* The view always shows the agent's current session (`currentSession`): a turn may go. */}
      <Composer agent={props.agent} sessionActive={isActive()} current draft={draft()} />
      {props.footer}
    </section>
  );
}

/** The live line under the transcript (TER-54): thinking, waiting, idle, stopped, blocked. */
export function LiveLine(props: { agent: Agent }) {
  const st = () => footerState({ status: props.agent.status, reason: props.agent.status_reason, waitingOn: props.agent.waiting_on, paused: props.agent.paused });
  return (
    <div class="convo-live" classList={{ [`tone-${st().tone}`]: !!st().tone }} role="status" aria-live="polite" data-running={st().running ? "true" : "false"}>
      <Show when={st().running} fallback={<span class="convo-live-dot" aria-hidden="true" />}>
        <span class="convo-live-spinner" aria-hidden="true">
          <span />
          <span />
          <span />
        </span>
      </Show>
      <span class="small">{st().text}</span>
    </div>
  );
}

/**
 * Untouched queued turns in delivery order, at the bottom of
 * the conversation. Edit takes a turn out of the queue and gives its text to the composer; Remove
 * drops it. The list reads again when the agent's `queued` count or status changes.
 */
export function QueuedTurns(props: { agent: Agent; onEdit: (text: string) => void }) {
  const { api } = useInstance();
  const [items, setItems] = createSignal<QueuedTurn[]>([]);
  const [runnerStatus, setRunnerStatus] = createSignal<string | null>(null);
  const [error, setError] = createSignal<unknown>(null);
  const [busy, setBusy] = createSignal<number | string | null>(null);
  let gen = 0;
  const load = async () => {
    const mine = ++gen;
    try {
      const r = await api.agents.queued(props.agent.id);
      if (mine === gen) {
        setItems(r.items);
        setRunnerStatus(r.queues?.runner && r.queues.runner !== "available" ? r.queues.runner : null);
      }
    } catch (e) {
      if (mine === gen) setError(e);
    }
  };
  // The store count excludes the in-memory queues of live agents.
  createEffect(
    on([() => props.agent.id, () => props.agent.queued ?? 0, () => props.agent.status], ([, n]) => {
      if (n > 0 || !["stopped", "error"].includes(props.agent.status)) void load();
      else {
        gen++;
        setItems([]);
        setRunnerStatus(null);
      }
    }),
  );
  const timer = setInterval(() => {
    if (["working", "blocked"].includes(props.agent.status) || items().length > 0) void load();
  }, 2000);
  onCleanup(() => { clearInterval(timer); gen++; });
  const take = async (turn: QueuedTurn, edit: boolean) => {
    if (busy() !== null) return;
    setBusy(turn.id);
    try {
      await api.agents.removeQueued(props.agent.id, turn.id, turn.source);
      setItems((cur) => cur.filter((t) => t.id !== turn.id));
      setError(null);
      if (edit) props.onEdit(turn.body);
    } catch (e) {
      setError(e instanceof ApiError && e.status === 404 ? new Error("That turn went to the agent already; it is no longer queued.") : e);
      void load();
    } finally {
      setBusy(null);
    }
  };
  return (
    <Show when={items().length || error() || runnerStatus()}>
      <section class="convo-queued" aria-label="Queued turns">
        <ErrorBanner error={error()} onDismiss={() => setError(null)} />
        <Show when={runnerStatus()}><p class="small muted">Runner queue: {runnerStatus()}. Other queued turns are shown below.</p></Show>
        <Show when={items().length}>
          <p class="small muted convo-queued-head">Queued: these turns go to the agent in this order when it wakes.</p>
          <ol class="convo-queued-list">
            <For each={items()}>
              {(turn) => (
                <li class="convo-queued-item" data-source={turn.source}>
                  <span class="convo-queued-text">{turn.body}</span>
                  <span class="row convo-queued-actions">
                    <button type="button" class="small" disabled={busy() !== null} onClick={() => void take(turn, true)}>
                      Edit
                    </button>
                    <button type="button" class="small danger" disabled={busy() !== null} onClick={() => void take(turn, false)}>
                      Remove
                    </button>
                  </span>
                </li>
              )}
            </For>
          </ol>
        </Show>
      </section>
    </Show>
  );
}

/** The composer under the transcript: the turn text, images, Send, and Stop while a turn runs. */
/**
 * `sessionActive`: a turn runs in the shown session now (the Stop button).
 * `current`: the view shows the agent's current session; only then may a
 * turn go (TER-44: a turn wakes a stopped agent, so liveness does not matter).
 */
export function Composer(props: { agent: Agent; sessionActive: boolean; current: boolean; draft?: { text: string; n: number } }) {
  const { api } = useInstance();
  const [text, setText] = createSignal("");
  const [images, setImages] = createSignal<DraftImage[]>([]);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [sent, setSent] = createSignal(false);
  const [dragging, setDragging] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  let fileInput: HTMLInputElement | undefined;
  let textArea: HTMLTextAreaElement | undefined;
  let nextId = 0;
  onCleanup(() => clearTimeout(timer));
  // A queued turn taken back to edit (TER-54): its text joins the draft.
  createEffect(
    on(
      () => props.draft,
      (d) => {
        if (!d) return;
        setText((cur) => (cur.trim() ? `${cur}\n${d.text}` : d.text));
        textArea?.focus();
      },
      { defer: true },
    ),
  );
  const state = () => composerState({ agentStatus: props.agent.status, current: props.current, paused: props.agent.paused });

  // ----- Stop: interrupt the running turn (issue #12) -----
  const [stopping, setStopping] = createSignal(false);
  const [stopSent, setStopSent] = createSignal(false);
  const showStop = () => canInterrupt({ agentStatus: props.agent.status, sessionActive: props.sessionActive });
  // A new turn clears the note of the last Stop.
  createEffect(on(() => props.agent.status, (st) => st === "working" && !stopping() && setStopSent(false), { defer: true }));
  const stop = async () => {
    if (stopping() || !showStop()) return;
    setStopping(true);
    try {
      await api.agents.interrupt(props.agent.id);
      setError(null);
      setStopSent(true);
    } catch (e) {
      setError(e);
    } finally {
      setStopping(false);
    }
  };
  const stopNote = () =>
    interruptNote({ agentStatus: props.agent.status, reason: props.agent.status_reason }) ?? (stopSent() && showStop() ? "Stop sent. The turn stops…" : null);

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
          ref={textArea}
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
        <Show when={showStop()}>
          <button
            type="button"
            class="danger convo-stop"
            disabled={stopping()}
            title="Stop the running turn (the harness's interrupt key). The agent keeps running."
            onClick={() => void stop()}
          >
            {stopping() ? "Stopping…" : "Stop"}
          </button>
        </Show>
        <button class="primary" type="submit" disabled={!state().enabled || busy() || !canSend(text(), images())}>
          {busy() ? "Sending…" : "Send"}
        </button>
      </div>
      <Show when={stopNote()}>
        <p class="small convo-compose-note convo-stop-note" role="status">
          {stopNote()}
        </p>
      </Show>
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
