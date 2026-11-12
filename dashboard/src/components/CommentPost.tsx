import { Show } from "solid-js";
import type { CardComment } from "../api/types";
import { A } from "../instance/navigation";
import { renderMarkdown, targetHref } from "../lib/markdown";
import { useInstance } from "../instance";
import { sessionHref } from "../features/board/sessions";
import { ForumPost, quotePost } from "./ForumPost";
import { SafeHtml } from "./SafeHtml";
import { openCardLink, readerCardKeys } from "./LinkText";

export function CommentPost(props: { comment: CardComment; index: number; card: string; project: string; onQuote?: (text: string) => void }) {
  const instance = useInstance();
  const html = () => {
    const keys = readerCardKeys();
    return renderMarkdown(props.comment.body, props.project, { textBody: true, cardKeys: keys, cardPrefixes: new Set(Object.keys(keys)), prefixProject: keys });
  };
  const id = () => `comment-${props.card}-${props.index + 1}`;
  return <ForumPost id={id()} number={props.index + 1} href={`${targetHref({ kind: "card", project: props.project, id: props.card })}#${id()}`} author={props.comment.author_name ?? props.comment.author} authorId={props.comment.author} label="Comment" at={props.comment.created_at}
    onQuote={props.onQuote ? (post) => props.onQuote?.(quotePost(props.comment.body, post)) : undefined}
    badges={<Show when={props.comment.session}>{(s) => <A href={sessionHref(s().id)} title={`${s().harness} session ${s().id}`}>View session</A>}</Show>}>
    <SafeHtml class="prose text-markdown" html={html()} onClick={(e) => openCardLink(e, instance.cardPeek)} />
  </ForumPost>;
}
