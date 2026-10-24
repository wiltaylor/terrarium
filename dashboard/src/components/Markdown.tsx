// Renders wiki HTML from lib/markdown, routes internal links through the
// router, and mounts artifact embeds into their placeholders. The HTML goes
// through DOMPurify (lib/sanitize) on its way into the DOM.

import { useNavigate } from "@solidjs/router";
import { createEffect, onCleanup } from "solid-js";
import { render } from "solid-js/web";
import { setSafeHtml } from "../lib/sanitize";
import { ArtifactViewer, type ArtifactLike } from "./ArtifactViewer";

export function Markdown(props: { html: string; artifacts?: Record<string, ArtifactLike> }) {
  let el!: HTMLDivElement;
  const navigate = useNavigate();
  let disposers: (() => void)[] = [];

  createEffect(() => {
    setSafeHtml(el, props.html);
    disposers.forEach((d) => d());
    disposers = [];
    el.querySelectorAll<HTMLElement>(".artifact-embed").forEach((slot) => {
      const id = slot.dataset.artifact ?? "";
      const a = props.artifacts?.[id];
      if (a) disposers.push(render(() => <ArtifactViewer artifact={a} />, slot));
      else slot.textContent = `[missing artifact ${id}]`;
    });
  });
  onCleanup(() => disposers.forEach((d) => d()));

  const onClick = (e: MouseEvent) => {
    const a = (e.target as HTMLElement).closest("a");
    if (!a || e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
    const href = a.getAttribute("href") ?? "";
    if (href.startsWith("/") && !href.startsWith("//") && !a.hasAttribute("download")) {
      e.preventDefault();
      navigate(href);
    }
  };

  return <div class="prose" ref={el} onClick={onClick} />;
}
