import { useInstance } from "../../instance";
import { ReadBoundary, useReadBoundary } from "../../components/ReadBoundary";
import { CommentPost } from "../../components/CommentPost";
import { PostEditor } from "../../components/PostEditor";
// The card side panel (GitHub #31, TER-571). A card key in text opens the card
// over the current view: the title, the column, the description, and the
// comments, with a link to the card on its board. The Layout mounts it once.

import { A } from "../../instance/navigation";
import { createResource, createSignal, For, Show } from "solid-js";
import { TextMarkdown } from "../../components/TextMarkdown";
import { Drawer, ErrorBanner } from "../../components/ui";
import { targetHref } from "../../lib/markdown";


export function CardPeek() {
  const { cardPeek, api } = useInstance();
  return (
    <Show when={cardPeek.current()} keyed>
      {(t) => {
        const [card, { mutate }] = createResource(() => t.id, (id) => api.cards.get(id));
        const [text, setText] = createSignal("");
        const [error, setError] = createSignal<unknown>(null);
        const [sending, setSending] = createSignal(false);
        let editor: HTMLTextAreaElement | undefined;
        // `card.latest` throws the load error again: read it only without an error (TER-598).
        const loaded = () => (card.error ? undefined : card.latest);
        const read = useReadBoundary("card", () => t.id, () => loaded()?.comments.map((c) => c.created_at) ?? []);
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
                    <ErrorBanner error={read.error()} />
                    <For each={c().comments} fallback={<p class="muted">No comments.</p>}>
                      {(m, index) => (
                        <>
                        <Show when={index() === read.first()}><ReadBoundary /></Show>
                        <CommentPost comment={m} index={index()} card={c().id} project={c().project} onQuote={(quote) => {
                          setText((old) => old + (old ? "\n\n" : "") + quote);
                          editor?.focus();
                        }} />
                        </>
                      )}
                    </For>
                    <ErrorBanner error={error()} />
                    <form class="stack" onSubmit={async (e) => {
                      e.preventDefault();
                      if (!text().trim() || sending()) return;
                      setSending(true);
                      try {
                        mutate(await api.cards.comment(t.id, text().trim()));
                        setText("");
                        setError(null);
                      } catch (err) { setError(err); }
                      finally { setSending(false); }
                    }}>
                      <PostEditor label="Comment" text={text()} project={c().project}>
                        <textarea ref={editor} aria-label="Comment" value={text()} onInput={(e) => setText(e.currentTarget.value)} placeholder="Comment as operator" />
                      </PostEditor>
                      <button type="submit" disabled={!text().trim() || sending()}>Comment</button>
                    </form>
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
