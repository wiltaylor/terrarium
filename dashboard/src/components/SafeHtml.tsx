// A block of untrusted HTML, sanitized by DOMPurify (lib/sanitize). Use this,
// or `Markdown`, for any HTML string; ESLint forbids `innerHTML`.

import { createEffect } from "solid-js";
import { setSafeHtml } from "../lib/sanitize";

export function SafeHtml(props: { html: string; class?: string }) {
  let el!: HTMLDivElement;
  createEffect(() => setSafeHtml(el, props.html));
  return <div class={props.class} ref={el} />;
}
