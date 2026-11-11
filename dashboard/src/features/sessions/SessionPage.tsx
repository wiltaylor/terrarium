import { useInstance } from "../../instance";
// One agent session, read-only (docs/api.md §6.6): the page that a card's
// "View session" link opens. It works for a deleted agent too: the daemon
// keeps the session and reads its transcript from the session archive.
// The blocks render with the same TranscriptLog as the agent's Sessions tab;
// there is no composer.

import { useParams } from "@solidjs/router";
import { A } from "../../instance/navigation";
import { createMemo, createResource, Show } from "solid-js";
import { ErrorBanner, PageHead } from "../../components/ui";
import { ago } from "../../lib/format";

import { createTranscriptView, StreamChip, TranscriptLog } from "../agents/conversation/TranscriptLog";
import { asSession, sessionChips, sessionRange } from "../board/sessions";

export default function SessionPage() {
  const { api } = useInstance();
  const params = useParams();
  const [detail] = createResource(
    () => params.id,
    (id) => api.sessions.get(id),
  );
  const d = () => detail.latest;
  const session = createMemo(() => (d() ? asSession(d()!) : undefined), undefined, { equals: (a, b) => a?.id === b?.id });
  const agent = createMemo(() => (d() ? { id: d()!.agent_id, project: d()!.project } : { id: "", project: "" }));
  const view = createTranscriptView(agent, session, (_aid, sid, after, limit) => api.sessions.transcript(sid, { after, limit }));

  return (
    <div class="stack">
      <PageHead title={<span>Session {d() ? `of ${d()!.project}/${d()!.agent}` : ""}</span>} />
      <ErrorBanner error={detail.error} />
      <Show when={d()}>
        {(s) => (
          <div class="card-box session-head">
            <dl class="kv small">
              <dt>Agent</dt>
              <dd>
                <Show when={!s().removed} fallback={<span>{`${s().project}/${s().agent}`}</span>}>
                  <A href={`/agents/${encodeURIComponent(s().agent_id)}?tab=sessions&session=${encodeURIComponent(s().id)}`}>{`${s().project}/${s().agent}`}</A>
                </Show>{" "}
                <span class="muted">({s().harness})</span>
              </dd>
              <dt>Session</dt>
              <dd class="mono">{s().id}</dd>
              <Show when={s().harness_session_id}>
                {(h) => (
                  <>
                    <dt>Harness session</dt>
                    <dd class="mono">{h()}</dd>
                  </>
                )}
              </Show>
              <dt>Time</dt>
              <dd title={s().started_at}>
                {sessionRange(s())} ({ago(s().started_at)})
              </dd>
              <dt>State</dt>
              <dd class="row" style={{ gap: "6px" }}>
                {sessionChips(s()).map((c) => (
                  <span class={`chip ${c.tone}`}>{c.label}</span>
                ))}
                <StreamChip state={view.t.streamState()} ended={!view.t.active()} />
              </dd>
            </dl>
            <Show when={s().removed}>
              <p class="muted small" style={{ margin: 0 }}>
                The agent is deleted. This transcript is the archived copy.
              </p>
            </Show>
          </div>
        )}
      </Show>
      <section class="convo-main" aria-label="Session transcript">
        <TranscriptLog view={view} label="Session transcript" empty={<p class="muted convo-note">{detail.loading ? "Loading…" : "No session."}</p>} />
      </section>
    </div>
  );
}
