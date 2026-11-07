// One Inbox thread in a list (docs/api.md §6.13): the Threads tab, and an
// Inbox item of kind `thread`. The topic is the opener's words: a text node
// only. The link opens the chat page. A pending close proposal of the agent
// shows with Confirm and Decline (TER-95); `onChange` reads the list again
// after one, and after a 409. With `boards` (the Threads tab), the n/m chip
// of the linked cards (TER-147).

import { A } from "@solidjs/router";
import { Show } from "solid-js";
import type { Board, Thread } from "../../api/types";
import { CloseProposal, isConflict } from "./CloseProposal";
import { ProgressChip } from "./TrackedCards";
import { ago } from "../../lib/format";
import { lastActivity, openQuestionCount, scopeLabel, threadChip, threadHref, waitsOnYou } from "./threads";

export function ThreadRow(props: { thread: Thread; now: number; boards?: Record<string, Board | undefined>; onChange?: () => void }) {
  const t = () => props.thread;
  const chip = () => threadChip(t());
  const open = () => openQuestionCount(t());
  return (
    <article class={`card-box stack thread-row${waitsOnYou(t()) ? " waits" : ""}`} style={{ gap: "6px" }} aria-label={`Thread ${t().id}`}>
      <div class="row small" style={{ "flex-wrap": "wrap", gap: "6px" }}>
        <span class="chip">thread</span>
        <span class={`chip ${chip().chip}`} data-testid="thread-status">
          {chip().label}
        </span>
        <Show when={props.boards}>{(b) => <ProgressChip thread={t()} boards={b()} />}</Show>
        <span class="muted">{scopeLabel(t().scope)}</span>
        <Show when={t().agent_name}>
          {(n) => (
            <span>
              with <strong>{n()}</strong>
            </span>
          )}
        </Show>
        <span class="muted" title={lastActivity(t())}>
          {ago(lastActivity(t()), props.now)}
        </span>
      </div>
      <A href={threadHref(t().id)} style={{ "font-weight": 600 }}>
        {t().topic}
      </A>
      <div class="row small muted" style={{ "flex-wrap": "wrap", gap: "10px" }}>
        <Show when={t().message_count !== undefined}>
          <span>
            {t().message_count} {t().message_count === 1 ? "message" : "messages"}
          </span>
        </Show>
        <Show when={open() > 0}>
          <span class="warn-text">
            {open()} open {open() === 1 ? "question" : "questions"}
          </span>
        </Show>
        <Show when={t().report_to_name}>{(n) => <span>reports to {n()}</span>}</Show>
      </div>
      <CloseProposal thread={t()} now={props.now} onChanged={() => props.onChange?.()} onError={(e) => isConflict(e) && props.onChange?.()} />
    </article>
  );
}
