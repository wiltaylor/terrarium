// Small shared UI pieces.

import { createSignal, For, onCleanup, onMount, Show, type JSX } from "solid-js";
import { Portal } from "solid-js/web";
import { describeError } from "../lib/errors";

/** Shows any thrown value. An ApiError shows its code and `error.details`. */
export function ErrorBanner(props: { error: unknown; onDismiss?: () => void }) {
  const view = () => describeError(props.error);
  return (
    <Show when={view()}>
      {(v) => (
        <div class="error-banner" role="alert">
          <div class="stack" style={{ gap: "4px", "min-width": 0, flex: 1 }}>
            <span>
              {v().message}
              <Show when={v().code}>
                {" "}
                <span class="mono small muted">({v().code})</span>
              </Show>
            </span>
            <For each={v().details}>
              {(d) => (
                <div class="small">
                  <strong>{d.label}:</strong>{" "}
                  <Show when={d.pre} fallback={<span class="mono">{d.value}</span>}>
                    <pre class="error-detail">{d.value}</pre>
                  </Show>
                </div>
              )}
            </For>
          </div>
          <Show when={props.onDismiss}>
            <button class="ghost small" onClick={() => props.onDismiss?.()} aria-label="Dismiss">
              ✕
            </button>
          </Show>
        </div>
      )}
    </Show>
  );
}

/** A "Load more" button for a cursor list. */
export function LoadMore(props: { hasMore: boolean; loading?: boolean; onMore: () => void; count?: number }) {
  return (
    <Show when={props.hasMore || props.count !== undefined}>
      <div class="row load-more">
        <Show when={props.count !== undefined}>
          <span class="muted small">{props.count} shown</span>
        </Show>
        <Show when={props.hasMore}>
          <button class="small" disabled={props.loading} onClick={() => props.onMore()}>
            {props.loading ? "Loading…" : "Load more"}
          </button>
        </Show>
      </div>
    </Show>
  );
}

/** A delete button that asks once, inline. Browsers in the viewer may block confirm(). */
export function ConfirmButton(props: { label: string; confirmLabel?: string; onConfirm: () => void | Promise<void>; class?: string; disabled?: boolean }) {
  const [asking, setAsking] = createSignal(false);
  return (
    <Show
      when={asking()}
      fallback={
        <button class={props.class ?? "danger"} disabled={props.disabled} onClick={() => setAsking(true)}>
          {props.label}
        </button>
      }
    >
      <span class="row confirm">
        <span class="small">Are you sure?</span>
        <button
          class="danger"
          onClick={async () => {
            setAsking(false);
            await props.onConfirm();
          }}
        >
          {props.confirmLabel ?? `Yes, ${props.label.toLowerCase()}`}
        </button>
        <button onClick={() => setAsking(false)}>Cancel</button>
      </span>
    </Show>
  );
}

export function CopyButton(props: { text: string; label?: string }) {
  const [done, setDone] = createSignal(false);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(props.text);
      setDone(true);
      setTimeout(() => setDone(false), 1500);
    } catch {
      // Clipboard blocked: select the text instead is left to the user.
    }
  };
  return (
    <button class="small" type="button" onClick={copy}>
      {done() ? "Copied" : (props.label ?? "Copy")}
    </button>
  );
}

function useEscape(fn: () => void) {
  onMount(() => {
    const h = (e: KeyboardEvent) => e.key === "Escape" && fn();
    window.addEventListener("keydown", h);
    onCleanup(() => window.removeEventListener("keydown", h));
  });
}

export function Modal(props: { title: string; onClose: () => void; children: JSX.Element }) {
  useEscape(() => props.onClose());
  return (
    <Portal>
      <div class="modal-scrim" onClick={() => props.onClose()} />
      <div class="modal" role="dialog" aria-modal="true" aria-label={props.title}>
        <div class="spread">
          <h2 style={{ margin: 0 }}>{props.title}</h2>
          <button class="ghost" onClick={() => props.onClose()} aria-label="Close">
            ✕
          </button>
        </div>
        {props.children}
      </div>
    </Portal>
  );
}

export function Drawer(props: { title: JSX.Element; onClose: () => void; children: JSX.Element }) {
  useEscape(() => props.onClose());
  return (
    <Portal>
      <div class="drawer-scrim" onClick={() => props.onClose()} />
      <aside class="drawer" role="dialog" aria-modal="true">
        <div class="spread">
          <div>{props.title}</div>
          <button class="ghost" onClick={() => props.onClose()} aria-label="Close">
            ✕
          </button>
        </div>
        {props.children}
      </aside>
    </Portal>
  );
}

export function Tabs<T extends string>(props: { tabs: readonly { id: T; label: string }[]; active: T; onChange: (t: T) => void }) {
  return (
    <div class="tabs" role="tablist">
      {props.tabs.map((t) => (
        <button role="tab" aria-selected={props.active === t.id} class={props.active === t.id ? "active" : ""} onClick={() => props.onChange(t.id)}>
          {t.label}
        </button>
      ))}
    </div>
  );
}

/** The page title and its actions. A tab of a page with its own title (Settings) gives `level={2}`. */
export function PageHead(props: { title: JSX.Element; level?: 1 | 2; children?: JSX.Element }) {
  return (
    <div class="page-head">
      {props.level === 2 ? <h2>{props.title}</h2> : <h1>{props.title}</h1>}
      <div class="actions">{props.children}</div>
    </div>
  );
}

export function Empty(props: { children: JSX.Element }) {
  return <div class="empty">{props.children}</div>;
}

/** A ticking clock signal for relative times. */
export function useNow(ms = 1000) {
  const [now, setNow] = createSignal(Date.now());
  const t = setInterval(() => setNow(Date.now()), ms);
  onCleanup(() => clearInterval(t));
  return now;
}

const PROVIDER_COLORS: Record<string, string> = {
  anthropic: "#c96442",
  openai: "#10a37f",
  github: "#6e40c9",
  google: "#4285f4",
  openrouter: "#6467f2",
  xai: "#222",
  groq: "#f55036",
};

export function ProviderLogo(props: { provider: string }) {
  return (
    <span class="provider-logo" style={{ background: PROVIDER_COLORS[props.provider] ?? "#718096" }} aria-hidden="true">
      {props.provider.slice(0, 1).toUpperCase()}
    </span>
  );
}
