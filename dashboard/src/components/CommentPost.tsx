import { Show } from "solid-js";
import type { CardComment } from "../api/types";
import { A } from "../instance/navigation";
import { targetHref } from "../lib/markdown";
import { sessionHref } from "../features/board/sessions";
import { ForumPost, quotePost } from "./ForumPost";
import { TextMarkdown } from "./TextMarkdown";

export function CommentPost(props: { comment: CardComment; index: number; card: string; project: string; onQuote?: (text: string) => void }) {
  const id = () => `comment-${props.card}-${props.index + 1}`;
  return <ForumPost id={id()} number={props.index + 1} href={`${targetHref({ kind: "card", project: props.project, id: props.card })}#${id()}`} author={props.comment.author_name ?? props.comment.author} label="Comment" at={props.comment.created_at}
    onQuote={props.onQuote ? (post) => props.onQuote?.(quotePost(props.comment.body, post)) : undefined}
    badges={<Show when={props.comment.session}>{(s) => <A href={sessionHref(s().id)} title={`${s().harness} session ${s().id}`}>View session</A>}</Show>}>
    <TextMarkdown text={props.comment.body} project={props.project} />
  </ForumPost>;
}
