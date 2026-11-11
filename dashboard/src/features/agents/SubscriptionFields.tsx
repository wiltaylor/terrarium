import { useInstance } from "../../instance";
// The subscription select and the model field of the agent form, also on
// the agent page. The logic is in ./subscription.ts.

import { A } from "@solidjs/router";
import { createMemo, createResource, createSignal, For, onMount, Show } from "solid-js";
import type { Harness } from "../../api/types";

import { ModelPicker } from "../../components/ModelPicker";
import { compatibleSubscriptions, defaultModelFor, loginText, modelSourceText, normalizePool, poolCandidates, subscriptionOptions } from "./subscription";

/** The Subscriptions page with the API-key step of the wizard open. */
export const ADD_API_KEY_HREF = "/settings/subscriptions?add=api_key";

/** A select of the subscriptions that `harness` can spend, and "none". Loads the list itself. */
export function SubscriptionSelect(props: { harness: Harness; value: string; onChange: (name: string) => void; disabled?: boolean; label?: string }) {
  const { subscriptions } = useInstance();
  onMount(() => void subscriptions.ensureLoaded());
  const options = () => subscriptionOptions(subscriptions.list(), props.harness, props.value);
  const none = () => subscriptions.loaded() && compatibleSubscriptions(subscriptions.list(), props.harness).length === 0;
  return (
    <>
      <select aria-label={props.label ?? "Subscription"} value={props.value} disabled={props.disabled} onChange={(e) => props.onChange(e.currentTarget.value)}>
        <For each={options()}>
          {(o) => (
            <option value={o.value} selected={o.value === props.value}>
              {o.label}
            </option>
          )}
        </For>
      </select>
      <Show when={!subscriptions.loaded()}>
        <span class="muted small">Loading subscriptions…</span>
      </Show>
      <Show when={subscriptions.error()}>
        <span class="small" style={{ color: "var(--danger)" }}>
          Cannot list the subscriptions.
        </span>
      </Show>
      <Show when={none()}>
        <span class="muted small">
          No subscription is for {props.harness}. <A href={ADD_API_KEY_HREF}>Add an API key</A>
        </span>
      </Show>
    </>
  );
}

/**
 * The subscription pool (PLAN §16.3): a checkbox for each subscription of
 * the same provider as `current` that `harness` can spend. The current
 * subscription is always in a pool, so its box stays on.
 */
export function PoolPicker(props: { harness: Harness; current: string; value: string[]; onChange: (pool: string[]) => void; disabled?: boolean }) {
  const { subscriptions } = useInstance();
  onMount(() => void subscriptions.ensureLoaded());
  const candidates = () => poolCandidates(subscriptions.list(), props.harness, props.current);
  const cur = () => props.current.trim();
  const on = (name: string) => name === cur() || props.value.includes(name);
  const toggle = (name: string, checked: boolean) => {
    const next = checked ? [...props.value, name] : props.value.filter((n) => n !== name);
    props.onChange(normalizePool(next, cur()));
  };
  return (
    <Show when={cur()} fallback={<span class="muted small">Pick a subscription first.</span>}>
      <Show when={candidates().length > 1} fallback={<span class="muted small">No other account of this provider is for {props.harness}.</span>}>
        <div class="row" style={{ "flex-wrap": "wrap" }}>
          <For each={candidates()}>
            {(s) => (
              <label class="row small">
                <input type="checkbox" checked={on(s.name)} disabled={props.disabled || s.name === cur()} onChange={(e) => toggle(s.name, e.currentTarget.checked)} /> {s.name}
                <span class="muted">({s.account?.login ?? loginText(s.login_status, s.has_api_key)})</span>
              </label>
            )}
          </For>
        </div>
      </Show>
    </Show>
  );
}

/**
 * The model: a searchable combobox of the models the subscription can use
 * (`GET /api/subscriptions/{name}/models`), the daemon's default first. The
 * text is the value, so an id that is not in the list stays as typed.
 * Without a subscription, or when the list does not load, a text field.
 */
export function ModelField(props: { harness: Harness; subscription: string; value: string; onChange: (model: string) => void }) {
  const { subscriptions, api } = useInstance();
  const sub = () => subscriptions.byName(props.subscription.trim());
  const def = () => defaultModelFor(sub(), props.harness);
  // Each click on Refresh asks the daemon to read the list again.
  const [refreshes, setRefreshes] = createSignal(0);
  // A string key, so a form change that keeps the subscription does not
  // load the list again (a new tuple each time would).
  const key = createMemo(() => (sub() ? JSON.stringify([sub()!.name, refreshes()]) : null));
  const [list] = createResource(key, (k) => {
    const [name, n] = JSON.parse(k) as [string, number];
    return api.subscriptions.models(name, n > 0 ? { refresh: true } : {}).catch(() => undefined);
  });
  return (
    <>
      <ModelPicker
        value={props.value}
        onChange={(m) => props.onChange(m)}
        models={(sub() && list()?.models) || []}
        defaultModel={def()}
        placeholder="anthropic/claude-opus-5-5 (pi, codex)"
      />
      <Show when={sub() && list()}>
        {(l) => (
          <span class="muted small">
            {l().models.length} models {modelSourceText(l())}.{" "}
            <button type="button" class="link small" onClick={() => setRefreshes((n) => n + 1)} disabled={list.loading}>
              Refresh
            </button>
          </span>
        )}
      </Show>
    </>
  );
}

/** What the daemon does with no model: the hint under the model field. */
export function defaultModelHint(harness: Harness, subscription: string): string | null {
  const { subscriptions } = useInstance();
  const s = subscriptions.byName(subscription.trim());
  if (!s) return null;
  const def = defaultModelFor(s, harness);
  if (def) return `With no model, the daemon starts ${harness} on ${def}. Pick another model to override it.`;
  if (def === null) return null;
  if (harness === "pi") return `The daemon has no default model for pi on ${s.name}: pick a model, or the start can be refused.`;
  return null;
}
