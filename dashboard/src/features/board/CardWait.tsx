import { For, Show } from "solid-js";
import type { CardWait, CardWaitSummary } from "../../api/types";
import { TextMarkdown } from "../../components/TextMarkdown";

export function WaitChip(props: { wait?: CardWaitSummary | null }) {
  return <Show when={props.wait}>
    <span class={`chip${props.wait?.met ? "" : " warn"}`} title={props.wait?.met ? "Wait completed (latched)" : "Waiting for card conditions"}>
      {props.wait?.met ? "wait met" : `waits ${props.wait?.mode === "any" ? "any" : "all"}: ${props.wait?.deps_met}/${props.wait?.deps_total}`}
    </span>
  </Show>;
}

export function CardWaitDetails(props: { wait?: CardWait | null; project: string }) {
  return <Show when={props.wait}>{(wait) => <section class="stack card-wait" aria-label="Card wait">
    <h3>Wait conditions</h3>
    <p>{wait().mode === "any" ? "Any condition" : "All conditions"} must be met.
      {wait().met_at ? " Wait completed (latched)." : " Wait pending."}</p>
    <Show when={wait().met_at}>{(at) => <time datetime={at()}>Completed {new Date(at()).toLocaleString()}</time>}</Show>
    <ul><For each={wait().dependencies}>{(dep) => <li>
      <span class={`chip${dep.met ? "" : " warn"}`}>{dep.met ? "met" : "not met"}</span>{" "}
      <Show when={!dep.hidden} fallback={<span>hidden</span>}>
        <TextMarkdown text={dep.card ?? "Unknown card"} project={props.project} />
        <span>Target: {dep.target ?? "done"}</span>
      </Show>
    </li>}</For></ul>
    <dl class="kv">
      <dt>Move to</dt><dd>{wait().action.move_to || "No move"}</dd>
      <dt>Comment</dt><dd><Show when={wait().action.comment} fallback="No comment">{(comment) => <TextMarkdown text={comment()} project={props.project} />}</Show></dd>
      <dt>Who wakes</dt><dd>{wait().action.wake ? (wait().wake_recipient || "Assignee, otherwise setter, project lead or operator") : "No wake requested"}</dd>
    </dl>
  </section>}</Show>;
}
