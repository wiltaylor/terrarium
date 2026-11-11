import { useInstance } from "../../instance";
// The card side panel (GitHub #31, TER-571). A card key in text opens the card
// over the current view: the title, the column, the description, and the
// comments, with a link to the card on its board. The Layout mounts it once.

import { A } from "../../instance/navigation";
import { createResource, For, Show } from "solid-js";
import { TextMarkdown } from "../../components/TextMarkdown";
import { Drawer, ErrorBanner } from "../../components/ui";
import { ago } from "../../lib/format";
import { targetHref } from "../../lib/markdown";


export function CardPeek() {
  const { cardPeek, api } = useInstance();
  return (
    <Show when={cardPeek.current()} keyed>
      {(t) => {
        const [card] = createResource(() => t.id, (id) => api.cards.get(id));
        // `card.latest` throws the load error again: read it only without an error (TER-598).
        const loaded = () => (card.error ? undefined : card.latest);
        return (
          <Drawer
            title={
              <span>
                <span class="mono muted">{t.id}</span>
                <Show when={loaded()}>{(c) => <> <span class="chip">{c().column}</span></>}</Show>
              </span>
            }
            onClose={() => cardPeek.close()}
          >
            <Show when={card.error}>
              <ErrorBanner error={card.error} />
            </Show>
            <Show when={card.loading && !loaded()}>
              <p class="muted">Loading the card.</p>
            </Show>
            <Show when={loaded()}>
              {(c) => (
                <div class="stack card-peek" style={{ gap: "10px" }}>
                  <h2 style={{ margin: 0 }}>{c().title}</h2>
                  <div class="small">
                    <A href={targetHref({ kind: "card", project: c().project, id: c().id })} onClick={() => cardPeek.close()}>
                      Open on the board
                    </A>
                  </div>
                  <div>
                    <Show when={c().body} fallback={<span class="muted">No description.</span>}>
                      <TextMarkdown text={c().body} project={c().project} />
                    </Show>
                  </div>
                  <section class="stack" style={{ gap: "8px" }}>
                    <h3 style={{ margin: 0 }}>Comments</h3>
                    <For each={c().comments} fallback={<p class="muted">No comments.</p>}>
                      {(m) => (
                        <div class="comment">
                          <div class="small muted">
                            <strong style={{ color: "var(--text)" }}>{m.author_name ?? m.author}</strong> · {ago(m.created_at)}
                          </div>
                          <div>
                            <TextMarkdown text={m.body} project={c().project} />
                          </div>
                        </div>
                      )}
                    </For>
                  </section>
                </div>
              )}
            </Show>
          </Drawer>
        );
      }}
    </Show>
  );
}
