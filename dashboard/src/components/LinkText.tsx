import { useInstance } from "../instance";
// Plain text with links (GitHub #31, TER-571). `lib/linkText` splits the text;
// each segment goes into the page as a text node or an `<a>` that Solid
// builds. No string ever reaches the DOM as HTML.

import { A } from "@solidjs/router";
import { For, Match, Switch } from "solid-js";
import { linkSegments } from "../lib/linkText";
import { cardOfHref } from "../lib/markdown";


/** A plain click opens the card in the side panel. A click with a modifier key or another button keeps the browser's own action (a new tab). */
const plainClick = (e: MouseEvent) => e.button === 0 && !e.metaKey && !e.ctrlKey && !e.shiftKey && !e.altKey;

/**
 * A delegated click on rendered Markdown (a question context): a plain click
 * on a `wikilink-card` link opens the card in the side panel. Other links keep
 * their action.
 */
export function openCardLink(e: MouseEvent) {
  const { cardPeek } = useInstance();
  if (!plainClick(e) || !(e.target instanceof Element)) return;
  const a = e.target.closest("a.wikilink-card");
  const card = a ? cardOfHref(a.getAttribute("href") ?? "") : null;
  if (!card) return;
  e.preventDefault();
  cardPeek.open(card);
}

/** Card prefix to project, for the projects of the reader: only their keys become card links. */
export const readerCardKeys = (): Record<string, string> =>
  {
  const { projects } = useInstance();
  return Object.fromEntries(projects.projects().filter((p) => p.card_prefix).map((p) => [p.card_prefix, p.name]));
};

export function LinkText(props: { text: string; project: string }) {
  const { cardPeek } = useInstance();
  const segments = () => linkSegments(props.text, { project: props.project, prefixProject: readerCardKeys() });
  return (
    <For each={segments()}>
      {(s) => (
        <Switch fallback={s.text}>
          <Match when={s.kind === "external" && s}>
            {(e) => (
              <a href={e().href} target="_blank" rel="noopener noreferrer" class="text-link external">
                {e().text}
              </a>
            )}
          </Match>
          <Match when={s.kind === "wiki" && s}>
            {(w) => (
              <A href={w().href} class="text-link wikilink">
                {w().text}
              </A>
            )}
          </Match>
          <Match when={s.kind === "card" && s}>
            {(c) => (
              <a
                href={c().href}
                class="text-link card-key"
                title={`Open card ${c().id}`}
                onClick={(e) => {
                  if (!plainClick(e)) return;
                  e.preventDefault();
                  cardPeek.open({ project: c().project, id: c().id });
                }}
              >
                {c().text}
              </a>
            )}
          </Match>
        </Switch>
      )}
    </For>
  );
}
