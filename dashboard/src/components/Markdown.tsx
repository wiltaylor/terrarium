// Renders wiki HTML from lib/markdown, routes internal links through the
// router, and mounts artifact embeds and DB query blocks into their
// placeholders. The HTML goes
// through DOMPurify (lib/sanitize) on its way into the DOM.

import { useNavigate } from "../instance/navigation";
import { createEffect, onCleanup } from "solid-js";
import { render } from "solid-js/web";
import { setSafeHtml } from "../lib/sanitize";
import { ArtifactViewer, type ArtifactLike } from "./ArtifactViewer";
import { DbEmbed } from "./DbEmbed";
import { useInstance } from "../instance";
import { scopeRemoteHtml } from "../instance/html";
import { openCardLink } from "./LinkText";
import { mountCodeCopy } from "../lib/codeCopy";

/** `project`: the project of the page. A `db` block queries the DB of this project only. */
export function Markdown(props: { html: string; artifacts?: Record<string, ArtifactLike>; project?: string }) {
  let el!: HTMLDivElement;
  const navigate = useNavigate();
  const instance = useInstance();
  let disposers: (() => void)[] = [];

  createEffect(() => {
    setSafeHtml(el, props.html, instance.key === "local" ? undefined : { blockImages: true, transform: (fragment) => scopeRemoteHtml(fragment, instance) });
    disposers.forEach((d) => d());
    disposers = [];
    mountCodeCopy(el);
    el.querySelectorAll<HTMLElement>(".artifact-embed").forEach((slot) => {
      const id = slot.dataset.artifact ?? "";
      const a = props.artifacts?.[id];
      if (a) disposers.push(render(() => <ArtifactViewer artifact={a} />, slot));
      else slot.textContent = `[missing artifact ${id}]`;
    });
    // The query is the text of the attribute (`dataset`), never HTML.
    el.querySelectorAll<HTMLElement>(".db-embed").forEach((slot) => {
      const query = slot.dataset.dbQuery ?? "";
      const project = props.project;
      if (project) disposers.push(render(() => <DbEmbed project={project} query={query} />, slot));
      else slot.textContent = "[a DB query shows on the wiki page only]";
    });
  });
  onCleanup(() => disposers.forEach((d) => d()));

  const onClick = (e: MouseEvent) => {
    openCardLink(e, instance.cardPeek);
    if (e.defaultPrevented) return;
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
