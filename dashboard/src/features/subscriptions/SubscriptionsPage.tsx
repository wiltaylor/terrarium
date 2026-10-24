import { createResource, createSignal, For, Show } from "solid-js";
import { ErrorBanner, PageHead, ProviderLogo, useNow } from "../../components/ui";
import type { LoginKind, Subscription, SubscriptionStatus } from "../../api/types";
import { ago, humanize, timeUntil } from "../../lib/format";
import { api, subscriptions } from "../../stores";
import { AddSubscriptionWizard } from "./AddSubscriptionWizard";

const CHIP: Record<SubscriptionStatus, string> = { logged_in: "ok", expiring: "warn", expired: "bad", missing: "bad" };

export function kindFor(s: Pick<Subscription, "provider" | "harnesses" | "probe">): LoginKind {
  if (s.harnesses.includes("claude")) return "claude";
  if (s.harnesses.includes("codex")) return "codex";
  if (s.harnesses.includes("copilot")) return "copilot";
  return s.probe ? "pi_oauth" : "pi_api_key";
}

export default function SubscriptionsPage() {
  const now = useNow(30_000);
  const [wizard, setWizard] = createSignal<{ kind?: LoginKind } | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [busy, setBusy] = createSignal<string | null>(null);
  const [detected] = createResource(() => api.subscriptions.detect());

  const act = async (name: string, fn: () => Promise<unknown>) => {
    setBusy(name);
    try {
      await fn();
      await subscriptions.refresh();
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(null);
    }
  };

  return (
    <>
      <PageHead title="Subscriptions">
        <button onClick={() => subscriptions.refresh()}>Refresh</button>
        <button class="primary" onClick={() => setWizard({})}>
          Add subscription
        </button>
      </PageHead>
      <ErrorBanner error={error() ?? subscriptions.error()} onDismiss={() => setError(null)} />

      <Show when={detected()?.length}>
        <div class="notice stack" style={{ gap: "6px", "margin-bottom": "14px" }}>
          <For each={detected()}>
            {(d) => (
              <div class="spread">
                <span>
                  Found a <strong>{d.harness}</strong> login for <strong>{d.provider}</strong>
                  {d.account ? ` (${d.account})` : ""} with no subscription definition.
                </span>
                <button class="small" onClick={() => setWizard({ kind: d.harness === "pi" ? "pi_oauth" : (d.harness as LoginKind) })}>
                  Create definition
                </button>
              </div>
            )}
          </For>
        </div>
      </Show>

      <div class="stack">
        <For each={subscriptions.list()}>
          {(s) => (
            <div class="card-box spread" style={{ "align-items": "flex-start" }}>
              <div class="row" style={{ "align-items": "flex-start", gap: "12px", "min-width": 0 }}>
                <ProviderLogo provider={s.provider} />
                <div style={{ "min-width": 0 }}>
                  <div class="row">
                    <strong>{s.name}</strong>
                    <span class={`chip ${CHIP[s.status]}`}>{humanize(s.status)}</span>
                  </div>
                  <div class="muted small">
                    {s.plan} · {s.provider} · harnesses: {s.harnesses.join(", ")}
                  </div>
                  <div class="muted small">
                    Verified {ago(s.verified_at, now())} · probe {s.probe ?? "none (local estimate only)"}
                    <Show when={s.windows.length}>
                      {" · "}
                      {s.windows.map((w) => `${w.window} ${w.used_pct}% (resets in ${timeUntil(w.resets_at, now())})`).join(", ")}
                    </Show>
                  </div>
                </div>
              </div>
              <div class="row">
                <button class="small" disabled={busy() === s.name} onClick={() => act(s.name, () => api.subscriptions.verify(s.name))}>
                  Verify now
                </button>
                <Show when={s.probe}>
                  <button class="small" disabled={busy() === s.name} onClick={() => act(s.name, () => api.subscriptions.probe(s.name))}>
                    Probe
                  </button>
                </Show>
                <button class="small" onClick={() => setWizard({ kind: kindFor(s) })}>
                  Re-login
                </button>
                <button class="small danger" disabled={busy() === s.name || s.status === "missing"} onClick={() => act(s.name, () => api.subscriptions.logout(s.name))}>
                  Logout
                </button>
              </div>
            </div>
          )}
        </For>
      </div>

      <Show when={wizard()}>{(w) => <AddSubscriptionWizard initialKind={w().kind} onClose={() => setWizard(null)} />}</Show>
    </>
  );
}
