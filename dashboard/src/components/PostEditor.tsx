import { createEffect, createSignal, on, Show, type JSX } from "solid-js";
import { SafeHtml } from "./SafeHtml";
import { renderMarkdown } from "../lib/markdown";

export function PostEditor(props: { label: string; text: string; project: string; children: JSX.Element }) {
  const [preview, setPreview] = createSignal(false);
  createEffect(on(() => props.text, () => setPreview(false), { defer: true }));
  return <div class="stack post-editor" style={{ width: "100%" }}>
    <div role="tablist" aria-label={`${props.label} editor`} class="row">
      <button type="button" role="tab" aria-selected={!preview()} onClick={() => setPreview(false)}>Write</button>
      <button type="button" role="tab" aria-selected={preview()} onClick={() => setPreview(true)}>Preview</button>
    </div>
    <div role="tabpanel" hidden={preview()}>{props.children}</div>
    <Show when={preview()}><div role="tabpanel" aria-label={`${props.label} preview`}><SafeHtml class="prose" html={renderMarkdown(props.text, props.project)} /></div></Show>
  </div>;
}
