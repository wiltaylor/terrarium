// The small "session" link of a log row (./sessionLink.ts). A click opens
// the session page and does not toggle the row.

import { A } from "../instance/navigation";
import { Show } from "solid-js";
import type { LogSessionLink } from "../api/types";
import { sessionLinkOf } from "./sessionLink";

export function SessionLink(props: { row: LogSessionLink }) {
  return (
    <Show when={sessionLinkOf(props.row)}>
      {(l) => (
        <A class="session-link small" href={l().href} title={l().title} aria-label={l().title} onClick={(e) => e.stopPropagation()}>
          <svg viewBox="0 0 16 16" width="12" height="12" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <rect x="1.5" y="2.5" width="13" height="11" rx="1.5" />
            <path d="M4.5 6.5l2 1.5-2 1.5M8.5 10h3" />
          </svg>
          <span>{l().text}</span>
        </A>
      )}
    </Show>
  );
}
