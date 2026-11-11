// Card descriptions, comments and memory bodies share the wiki renderer and sink.
import { renderMarkdown } from "../lib/markdown";
import { readerCardKeys } from "./LinkText";
import { Markdown } from "./Markdown";

export function TextMarkdown(props: { text: string; project: string }) {
  const options = () => {
    const keys = readerCardKeys();
    return { textBody: true, cardKeys: keys, cardPrefixes: new Set(Object.keys(keys)), prefixProject: keys };
  };
  return <div class="text-markdown"><Markdown html={renderMarkdown(props.text, props.project, options())} /></div>;
}
