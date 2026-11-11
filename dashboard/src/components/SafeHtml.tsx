// A block of untrusted HTML, sanitized by DOMPurify (lib/sanitize). Use this,
// or `Markdown`, for any HTML string; ESLint forbids `innerHTML`.

import { createEffect } from "solid-js";
import { setSafeHtml } from "../lib/sanitize";
import { useInstance } from "../instance";
import { scopeHtmlLinks } from "../instance/html";

/** `onClick`: a delegated click on the content, for example `openCardLink` (TER-571). */
export function SafeHtml(props: { html: string; class?: string; onClick?: (e: MouseEvent) => void }) {
  let el!: HTMLDivElement;
  const instance = useInstance();
  createEffect(() => { setSafeHtml(el, props.html); scopeHtmlLinks(el, instance.href); });
  return <div class={props.class} ref={el} onClick={(e) => props.onClick?.(e)} />;
}
