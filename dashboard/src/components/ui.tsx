// Small shared UI pieces.

import { createSignal, For, onCleanup, onMount, Show, type JSX } from "solid-js";
import { Portal } from "solid-js/web";
import { useInstance } from "../instance";
import { describeError } from "../lib/errors";
import { isSelfHostedKind, PROVIDER_COLORS, providerLabel, providerMark } from "./providerLogos";

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
  const { key } = useInstance();
  const scope = () => key === "local" ? "" : ` on ${key.slice(7)}`;
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
        <span class="small">Are you sure{scope()}?</span>
        <button
          class="danger"
          onClick={async () => {
            setAsking(false);
            await props.onConfirm();
          }}
        >
          {props.confirmLabel ?? `Yes, ${props.label.toLowerCase()}`}{scope()}
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

/**
 * The open dialogs and drawers, the newest last (TER-598). Escape closes only
 * the top one: the card side panel over the board drawer closes, and the
 * drawer stays.
 */
const escapeStack: (() => void)[] = [];

const onEscapeKey = (e: KeyboardEvent) => {
  if (e.key === "Escape") escapeStack.at(-1)?.();
};

function useEscape(fn: () => void) {
  onMount(() => {
    const entry = () => fn();
    if (escapeStack.length === 0) window.addEventListener("keydown", onEscapeKey);
    escapeStack.push(entry);
    onCleanup(() => {
      const i = escapeStack.lastIndexOf(entry);
      if (i >= 0) escapeStack.splice(i, 1);
      if (escapeStack.length === 0) window.removeEventListener("keydown", onEscapeKey);
    });
  });
}

/** A dialog over the page. `class` adds classes to the dialog box, such as `modal-full` (full screen on a phone). */
export function Modal(props: { title: string; onClose: () => void; children: JSX.Element; class?: string }) {
  const { key } = useInstance();
  const title = () => key === "local" ? props.title : `${props.title} on ${key.slice(7)}`;
  useEscape(() => props.onClose());
  return (
    <Portal>
      <div class="modal-scrim" onClick={() => props.onClose()} />
      <div class={`modal${props.class ? ` ${props.class}` : ""}`} role="dialog" aria-modal="true" aria-label={title()}>
        <div class="spread">
          <h2 style={{ margin: 0 }}>{title()}</h2>
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
        {/* On a phone the drawer fills the screen, and this head with the close button stays at the top. */}
        <div class="spread drawer-head">
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
  const { key } = useInstance();
  return (
    <div class="page-head">
      {props.level === 2 ? <h2>{props.title}</h2> : <h1>{props.title}</h1>}
      <Show when={key !== "local"}><strong class="chip warn">Remote: {key.slice(7)}</strong></Show>
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

/**
 * The provider's own mark (components/providerLogos.ts), with the provider
 * name as the tooltip and the accessible name. A self-hosted server gets a
 * server glyph, and a provider with no bundled mark gets the letter tile.
 * `small` is the inline size, next to text.
 */
export function ProviderLogo(props: { provider: string; small?: boolean }) {
  const mark = () => providerMark(props.provider);
  const label = () => providerLabel(props.provider);
  const cls = (extra: string) => `provider-logo${props.small ? " small" : ""} ${extra}`;
  return (
    <Show
      when={mark()}
      fallback={
        <Show
          when={isSelfHostedKind(props.provider)}
          fallback={
            <span class={cls("letter")} style={{ background: PROVIDER_COLORS[props.provider.trim().toLowerCase()] ?? "#718096" }} role="img" aria-label={label()} title={label()}>
              <span aria-hidden="true">{props.provider.trim().slice(0, 1).toUpperCase() || "?"}</span>
            </span>
          }
        >
          <span class={cls("glyph")} role="img" aria-label={label()} title={label()}>
            <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" aria-hidden="true">
              <rect x="2.5" y="2.5" width="11" height="4.5" rx="1" />
              <rect x="2.5" y="9" width="11" height="4.5" rx="1" />
              <path d="M5 4.75h.01M5 11.25h.01" stroke-width="2" stroke-linecap="round" />
            </svg>
          </span>
        </Show>
      }
    >
      {(m) => (
        <span class={cls(m().plate ? "mark plate" : "mark")} role="img" aria-label={label()} title={label()}>
          <img class="provider-mark-light" src={m().light} alt="" draggable={false} />
          <Show when={m().dark !== m().light}>
            <img class="provider-mark-dark" src={m().dark} alt="" draggable={false} />
          </Show>
        </span>
      )}
    </Show>
  );
}
