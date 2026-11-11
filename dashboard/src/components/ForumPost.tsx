import { createEffect, Show, type JSX } from "solid-js";
import { A, useLocation } from "../instance/navigation";
import { ago } from "../lib/format";

/** The shared header strip and full-width body for every conversation. */
export function ForumPost(props: {
  id: string; number: number; href: string; author: JSX.Element;
  at: string; now?: number; label: string; icon?: JSX.Element;
  badges?: JSX.Element; children: JSX.Element;
  onQuote?: (post: HTMLElement) => void;
}) {
  let post!: HTMLElement;
  const location = useLocation();
  createEffect(() => {
    if (location.hash === `#${props.id}`) queueMicrotask(() => post.scrollIntoView?.({ block: "start" }));
  });
  return <article ref={post} id={props.id} class={`thread-post post-${props.label.toLowerCase().replaceAll(" ", "-")}`}>
    <header class="thread-post-header">
      {props.icon}<strong>{props.author}</strong><span class="chip">{props.label}</span>
      {props.badges}
      <time class="muted" dateTime={props.at} title={props.at}>{ago(props.at, props.now)}</time>
      <A href={props.href} aria-label={`Permalink to post ${props.number}`}>#{props.number}</A>
      <Show when={props.onQuote}><button type="button" onClick={() => props.onQuote?.(post)}>Quote</button></Show>
    </header>
    <div class="thread-post-body">{props.children}</div>
  </article>;
}

export function quotePost(text: string, post: HTMLElement) {
  const selection = window.getSelection();
  const selected = selection && post.contains(selection.anchorNode) && post.contains(selection.focusNode) ? selection.toString() : "";
  return (selected || text).split("\n").map((line) => `> ${line}`).join("\n") + "\n\n";
}
