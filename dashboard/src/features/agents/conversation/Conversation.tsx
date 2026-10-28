// The Conversation tab of the agent page: the transcript of the agent's
// current session as blocks (docs/api.md §6, transcripts), with the live
// tail and the composer. It follows a new session. The Sessions tab opens
// the other sessions; the raw terminal stays on its own tab.
//
// Agent-written text is untrusted: see ./TranscriptLog.

import { createEffect, createMemo, createSignal, on, onCleanup, Show } from "solid-js";
import type { Agent, Session } from "../../../api/types";
import { StatusBadge } from "../../../components/StatusBadge";
import { ErrorBanner } from "../../../components/ui";
import { api } from "../../../stores";
import { currentSession } from "../sessions";
import { composerState, isSendKey } from "./model";
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
          <StatusBadge status={props.agent.status} waitingOn={props.agent.waiting_on} />
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
        empty={<p class="muted convo-note">{loaded() ? "No session yet. Start the agent." : "Loading…"}</p>}
      />
      <Composer agent={props.agent} sessionActive={isActive()} />
    </section>
  );
}

function Composer(props: { agent: Agent; sessionActive: boolean }) {
  const [text, setText] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  const [sent, setSent] = createSignal(false);
  let timer: ReturnType<typeof setTimeout> | undefined;
  onCleanup(() => clearTimeout(timer));
  const state = () => composerState({ agentStatus: props.agent.status, sessionActive: props.sessionActive });

  const send = async () => {
    const body = text().trim();
    if (!body || busy() || !state().enabled) return;
    setBusy(true);
    try {
      await api.agents.send(props.agent.id, body);
      setText("");
      setError(null);
      setSent(true);
      clearTimeout(timer);
      timer = setTimeout(() => setSent(false), 6000);
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form
      class="convo-composer"
      onSubmit={(e) => {
        e.preventDefault();
        void send();
      }}
    >
      <ErrorBanner error={error()} onDismiss={() => setError(null)} />
      <div class="convo-compose-row">
        <textarea
          rows={2}
          value={text()}
          disabled={!state().enabled}
          placeholder={state().enabled ? "Type a turn. Enter sends, Shift+Enter adds a line." : (state().reason ?? "")}
          aria-label="Turn text"
          onInput={(e) => setText(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (isSendKey(e)) {
              e.preventDefault();
              void send();
            }
          }}
        />
        <button class="primary" type="submit" disabled={!state().enabled || busy() || !text().trim()}>
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
